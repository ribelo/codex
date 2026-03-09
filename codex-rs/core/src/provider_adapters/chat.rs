use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::tools::ToolSpec;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::provider_adapters::AdapterContext;
use crate::provider_adapters::conversation_headers;
use crate::util::backoff;
use codex_otel::SessionTelemetry;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use reqwest::StatusCode;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

pub(crate) async fn stream_chat_completions(
    ctx: &AdapterContext<'_>,
    prompt: &Prompt,
    model_info: &ModelInfo,
    session_telemetry: &SessionTelemetry,
) -> Result<ResponseStream> {
    let payload = build_chat_payload(prompt, model_info)?;
    let mut attempt = 0_u64;
    let max_retries = ctx.provider.request_max_retries();
    loop {
        attempt += 1;
        let started_at = Instant::now();
        let mut request = ctx.request_builder_for_path(
            "/chat/completions",
            conversation_headers(ctx.conversation_id, ctx.session_source),
        );
        if let Some(token) = ctx.bearer_token() {
            request = request.bearer_auth(token);
        }
        let response = request.json(&payload).send().await;
        let duration = started_at.elapsed();
        session_telemetry.record_api_request(
            attempt,
            response.as_ref().ok().map(|resp| resp.status().as_u16()),
            response
                .as_ref()
                .err()
                .map(std::string::ToString::to_string)
                .as_deref(),
            duration,
        );

        match response {
            Ok(response) if response.status().is_success() => {
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
                tokio::spawn(process_chat_sse(
                    response.bytes_stream(),
                    tx_event,
                    ctx.provider.stream_idle_timeout(),
                    session_telemetry.clone(),
                ));
                return Ok(ResponseStream { rx_event });
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let is_retryable = status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::CONFLICT
                    || status.is_server_error();
                if !is_retryable || attempt > max_retries {
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        url: None,
                        cf_ray: None,
                        request_id: None,
                    }));
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(source) => {
                if attempt > max_retries {
                    return Err(CodexErr::ConnectionFailed(ConnectionFailedError { source }));
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

fn build_chat_payload(prompt: &Prompt, model_info: &ModelInfo) -> Result<Value> {
    let mut messages = Vec::<Value>::new();
    messages.push(json!({
        "role": "system",
        "content": prompt.base_instructions.text,
    }));

    let input = prompt.get_formatted_input();
    let mut reasoning_by_anchor_index: HashMap<usize, String> = HashMap::new();
    let mut last_emitted_role: Option<&str> = None;
    for item in &input {
        match item {
            ResponseItem::Message { role, .. } => last_emitted_role = Some(role.as_str()),
            ResponseItem::FunctionCall { .. } => last_emitted_role = Some("assistant"),
            ResponseItem::FunctionCallOutput { .. } => last_emitted_role = Some("tool"),
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => {}
        }
    }

    let mut last_user_index: Option<usize> = None;
    for (idx, item) in input.iter().enumerate() {
        if let ResponseItem::Message { role, .. } = item
            && role == "user"
        {
            last_user_index = Some(idx);
        }
    }

    if !matches!(last_emitted_role, Some("user")) {
        for (idx, item) in input.iter().enumerate() {
            if let Some(last_user_index) = last_user_index
                && idx <= last_user_index
            {
                continue;
            }

            if let ResponseItem::Reasoning {
                content: Some(items),
                ..
            } = item
            {
                let mut text = String::new();
                for entry in items {
                    match entry {
                        ReasoningItemContent::ReasoningText { text: segment }
                        | ReasoningItemContent::Text { text: segment } => text.push_str(segment),
                    }
                }
                if text.trim().is_empty() {
                    continue;
                }

                let mut attached = false;
                if idx > 0
                    && let ResponseItem::Message { role, .. } = &input[idx - 1]
                    && role == "assistant"
                {
                    reasoning_by_anchor_index
                        .entry(idx - 1)
                        .and_modify(|value| value.push_str(&text))
                        .or_insert(text.clone());
                    attached = true;
                }

                if !attached && idx + 1 < input.len() {
                    match &input[idx + 1] {
                        ResponseItem::FunctionCall { .. } => {
                            reasoning_by_anchor_index
                                .entry(idx + 1)
                                .and_modify(|value| value.push_str(&text))
                                .or_insert(text.clone());
                        }
                        ResponseItem::Message { role, .. } if role == "assistant" => {
                            reasoning_by_anchor_index
                                .entry(idx + 1)
                                .and_modify(|value| value.push_str(&text))
                                .or_insert(text.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let mut last_assistant_text: Option<String> = None;
    for (idx, item) in input.iter().enumerate() {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if role != "user" && role != "assistant" {
                    continue;
                }

                let mut text = String::new();
                let mut parts = Vec::<Value>::new();
                let mut saw_image = false;
                for item in content {
                    match item {
                        ContentItem::InputText { text: segment }
                        | ContentItem::OutputText { text: segment } => {
                            text.push_str(segment);
                            parts.push(json!({ "type": "text", "text": segment }));
                        }
                        ContentItem::InputImage { image_url } => {
                            saw_image = true;
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": image_url },
                            }));
                        }
                    }
                }

                if role == "assistant" {
                    if last_assistant_text.as_ref() == Some(&text) {
                        continue;
                    }
                    last_assistant_text = Some(text.clone());
                }

                let content_value = if role == "assistant" {
                    json!(text)
                } else if saw_image {
                    json!(parts)
                } else {
                    json!(text)
                };

                let mut message = json!({
                    "role": role,
                    "content": content_value,
                });
                if role == "assistant"
                    && let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                    && let Some(object) = message.as_object_mut()
                {
                    object.insert("reasoning".to_string(), json!(reasoning));
                }
                messages.push(message);
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let mut message = json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }],
                });
                if let Some(reasoning) = reasoning_by_anchor_index.get(&idx)
                    && let Some(object) = message.as_object_mut()
                {
                    object.insert("reasoning".to_string(), json!(reasoning));
                }
                messages.push(message);
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_output_content(output),
                }));
            }
            ResponseItem::CustomToolCall {
                id, name, input, ..
            } => {
                messages.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "custom",
                        "custom": {
                            "name": name,
                            "input": input,
                        }
                    }],
                }));
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_output_content(output),
                }));
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => {}
        }
    }

    let tools = build_chat_tools(&prompt.tools)?;
    let mut payload = json!({
        "model": model_info.slug,
        "messages": messages,
        "stream": true,
    });
    if let Some(object) = payload.as_object_mut() {
        if !tools.is_empty() {
            object.insert("tools".to_string(), Value::Array(tools));
            object.insert(
                "parallel_tool_calls".to_string(),
                Value::Bool(prompt.parallel_tool_calls),
            );
        }
        if let Some(schema) = &prompt.output_schema {
            object.insert(
                "response_format".to_string(),
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "codex_output",
                        "strict": true,
                        "schema": schema,
                    }
                }),
            );
        }
    }
    Ok(payload)
}

fn build_chat_tools(tools: &[ToolSpec]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for tool in tools {
        match tool {
            ToolSpec::Function(spec) => {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "strict": spec.strict,
                        "parameters": serde_json::to_value(&spec.parameters)?,
                    }
                }));
            }
            ToolSpec::Freeform(spec) => {
                out.push(json!({
                    "type": "custom",
                    "custom": {
                        "name": spec.name,
                        "description": spec.description,
                        "format": serde_json::to_value(&spec.format)?,
                    }
                }));
            }
            ToolSpec::LocalShell {}
            | ToolSpec::ImageGeneration { .. }
            | ToolSpec::WebSearch { .. } => {}
        }
    }
    Ok(out)
}

fn tool_output_content(output: &FunctionCallOutputPayload) -> Value {
    if let Some(items) = output.content_items() {
        Value::Array(
            items
                .iter()
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        json!({ "type": "text", "text": text })
                    }
                    FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                        json!({
                            "type": "image_url",
                            "image_url": { "url": image_url },
                        })
                    }
                })
                .collect(),
        )
    } else {
        json!(output.text_content().unwrap_or_default())
    }
}

async fn process_chat_sse<S, E>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    session_telemetry: SessionTelemetry,
) where
    S: Stream<Item = std::result::Result<tokio_util::bytes::Bytes, E>> + Unpin + Eventsource,
    E: std::fmt::Display,
{
    let mut stream = stream.eventsource();

    #[derive(Default, Debug)]
    struct ToolCallState {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    }

    let mut tool_calls: HashMap<usize, ToolCallState> = HashMap::new();
    let mut tool_call_order: Vec<usize> = Vec::new();
    let mut tool_call_order_seen: HashSet<usize> = HashSet::new();
    let mut tool_call_index_by_id: HashMap<String, usize> = HashMap::new();
    let mut next_tool_call_index = 0usize;
    let mut last_tool_call_index: Option<usize> = None;
    let mut assistant_item: Option<ResponseItem> = None;
    let mut reasoning_item: Option<ResponseItem> = None;
    let mut completed_sent = false;

    loop {
        let started_at = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        session_telemetry.log_sse_event(&response, started_at.elapsed());
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(source))) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(source.to_string(), None)))
                    .await;
                return;
            }
            Ok(None) => {
                if let Some(reasoning) = reasoning_item {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(reasoning)))
                        .await;
                }
                if let Some(assistant) = assistant_item {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(assistant)))
                        .await;
                }
                if !completed_sent {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id: String::new(),
                            token_usage: None,
                        }))
                        .await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(
                        "idle timeout waiting for SSE".to_string(),
                        None,
                    )))
                    .await;
                return;
            }
        };

        if sse.data.trim().is_empty() {
            continue;
        }
        if sse.data.trim() == "[DONE]" {
            if let Some(reasoning) = reasoning_item.take() {
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(reasoning)))
                    .await;
            }
            if let Some(assistant) = assistant_item.take() {
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(assistant)))
                    .await;
            }
            if !completed_sent {
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: None,
                    }))
                    .await;
            }
            return;
        }

        trace!("chat SSE event: {}", sse.data);
        let value: Value = match serde_json::from_str(&sse.data) {
            Ok(value) => value,
            Err(err) => {
                debug!("Failed to parse chat SSE event: {err}");
                continue;
            }
        };

        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            continue;
        };

        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(reasoning) = delta.get("reasoning") {
                    if let Some(text) = reasoning.as_str() {
                        append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string())
                            .await;
                    } else if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                        append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string())
                            .await;
                    } else if let Some(text) = reasoning.get("content").and_then(Value::as_str) {
                        append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string())
                            .await;
                    }
                }

                if let Some(content) = delta.get("content") {
                    if let Some(items) = content.as_array() {
                        for item in items {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                append_assistant_text(
                                    &tx_event,
                                    &mut assistant_item,
                                    text.to_string(),
                                )
                                .await;
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        append_assistant_text(&tx_event, &mut assistant_item, text.to_string())
                            .await;
                    }
                }

                if let Some(tool_call_values) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tool_call in tool_call_values {
                        let mut index = tool_call
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|index| index as usize);

                        let mut call_id_for_lookup = None;
                        if let Some(call_id) = tool_call.get("id").and_then(Value::as_str) {
                            call_id_for_lookup = Some(call_id.to_string());
                            if let Some(existing) = tool_call_index_by_id.get(call_id) {
                                index = Some(*existing);
                            }
                        }

                        if index.is_none() && call_id_for_lookup.is_none() {
                            index = last_tool_call_index;
                        }

                        let index = index.unwrap_or_else(|| {
                            while tool_calls.contains_key(&next_tool_call_index) {
                                next_tool_call_index += 1;
                            }
                            let index = next_tool_call_index;
                            next_tool_call_index += 1;
                            index
                        });

                        let call_state = tool_calls.entry(index).or_default();
                        if tool_call_order_seen.insert(index) {
                            tool_call_order.push(index);
                        }

                        if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                            call_state.id.get_or_insert_with(|| id.to_string());
                            tool_call_index_by_id.entry(id.to_string()).or_insert(index);
                        }

                        if let Some(function) = tool_call.get("function") {
                            if let Some(name) = function.get("name").and_then(Value::as_str)
                                && !name.is_empty()
                            {
                                call_state.name.get_or_insert_with(|| name.to_string());
                            }
                            if let Some(arguments) =
                                function.get("arguments").and_then(Value::as_str)
                            {
                                call_state.arguments.push_str(arguments);
                            }
                        }

                        last_tool_call_index = Some(index);
                    }
                }
            }

            if let Some(message) = choice.get("message")
                && let Some(reasoning) = message.get("reasoning")
            {
                if let Some(text) = reasoning.as_str() {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string()).await;
                } else if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string()).await;
                } else if let Some(text) = reasoning.get("content").and_then(Value::as_str) {
                    append_reasoning_text(&tx_event, &mut reasoning_item, text.to_string()).await;
                }
            }

            match choice.get("finish_reason").and_then(Value::as_str) {
                Some("stop") => {
                    if let Some(reasoning) = reasoning_item.take() {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemDone(reasoning)))
                            .await;
                    }
                    if let Some(assistant) = assistant_item.take() {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemDone(assistant)))
                            .await;
                    }
                    if !completed_sent {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::Completed {
                                response_id: String::new(),
                                token_usage: None,
                            }))
                            .await;
                        completed_sent = true;
                    }
                }
                Some("length") => {
                    let _ = tx_event.send(Err(CodexErr::ContextWindowExceeded)).await;
                    return;
                }
                Some("tool_calls") => {
                    if let Some(reasoning) = reasoning_item.take() {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemDone(reasoning)))
                            .await;
                    }
                    for index in tool_call_order.drain(..) {
                        let Some(state) = tool_calls.remove(&index) else {
                            continue;
                        };
                        tool_call_order_seen.remove(&index);
                        let Some(name) = state.name else {
                            debug!("Skipping tool call {index} without a name");
                            continue;
                        };
                        let item = ResponseItem::FunctionCall {
                            id: None,
                            name,
                            arguments: state.arguments,
                            call_id: state.id.unwrap_or_else(|| format!("tool-call-{index}")),
                        };
                        let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                    }
                    if !completed_sent {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::Completed {
                                response_id: String::new(),
                                token_usage: None,
                            }))
                            .await;
                        completed_sent = true;
                    }
                }
                Some(_) | None => {}
            }
        }
    }
}

async fn append_assistant_text(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_item: &mut Option<ResponseItem>,
    text: String,
) {
    if assistant_item.is_none() {
        let item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
            end_turn: None,
            phase: None,
        };
        *assistant_item = Some(item.clone());
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item)))
            .await;
    }

    if let Some(ResponseItem::Message { content, .. }) = assistant_item {
        content.push(ContentItem::OutputText { text: text.clone() });
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(text)))
            .await;
    }
}

async fn append_reasoning_text(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    reasoning_item: &mut Option<ResponseItem>,
    text: String,
) {
    if reasoning_item.is_none() {
        let item = ResponseItem::Reasoning {
            id: String::new(),
            summary: Vec::new(),
            content: Some(Vec::new()),
            encrypted_content: None,
        };
        *reasoning_item = Some(item.clone());
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item)))
            .await;
    }

    if let Some(ResponseItem::Reasoning {
        content: Some(content),
        ..
    }) = reasoning_item
    {
        let content_index = content.len() as i64;
        content.push(ReasoningItemContent::ReasoningText { text: text.clone() });
        let _ = tx_event
            .send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: text,
                content_index,
            }))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use pretty_assertions::assert_eq;
    use tokio_util::io::ReaderStream;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn assistant_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn reasoning_item(text: &str) -> ResponseItem {
        ResponseItem::Reasoning {
            id: String::new(),
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText {
                text: text.to_string(),
            }]),
            encrypted_content: None,
        }
    }

    fn function_call() -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: "f".to_string(),
            arguments: "{}".to_string(),
            call_id: "c1".to_string(),
        }
    }

    fn messages_from(body: &Value) -> Vec<Value> {
        body["messages"].as_array().expect("messages").to_vec()
    }

    fn first_assistant(messages: &[Value]) -> &Value {
        messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("assistant message")
    }

    fn build_body(events: &[Value]) -> String {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    async fn collect_events(body: &str) -> Vec<ResponseEvent> {
        let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(16);
        let telemetry = SessionTelemetry::new(
            codex_protocol::ThreadId::new(),
            "model",
            "model",
            None,
            None,
            None,
            "originator".to_string(),
            false,
            "terminal".to_string(),
            codex_protocol::protocol::SessionSource::Cli,
        );
        tokio::spawn(process_chat_sse(
            reader,
            tx,
            Duration::from_millis(1_000),
            telemetry,
        ));

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.expect("stream event"));
        }
        events
    }

    #[test]
    fn attaches_reasoning_to_previous_assistant() {
        let prompt = Prompt {
            input: vec![
                user_message("u1"),
                assistant_message("a1"),
                reasoning_item("r1"),
            ],
            ..Prompt::default()
        };
        let model_info = crate::models_manager::model_info::model_info_from_slug("mock-model");
        let payload = build_chat_payload(&prompt, &model_info).expect("payload");
        let messages = messages_from(&payload);
        let assistant = first_assistant(&messages);

        assert_eq!(assistant["content"], Value::String("a1".into()));
        assert_eq!(assistant["reasoning"], Value::String("r1".into()));
    }

    #[test]
    fn attaches_reasoning_to_function_call_anchor() {
        let prompt = Prompt {
            input: vec![user_message("u1"), reasoning_item("r1"), function_call()],
            ..Prompt::default()
        };
        let model_info = crate::models_manager::model_info::model_info_from_slug("mock-model");
        let payload = build_chat_payload(&prompt, &model_info).expect("payload");
        let messages = messages_from(&payload);
        let assistant = first_assistant(&messages);

        assert_eq!(assistant["reasoning"], Value::String("r1".into()));
    }

    #[tokio::test]
    async fn emits_tool_call_events() {
        let body = build_body(&[
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "id": "call_a",
                            "index": 0,
                            "function": {
                                "name": "do_a",
                                "arguments": "{\"foo\":"
                            }
                        }]
                    }
                }]
            }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "1}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ]);

        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                }),
                ResponseEvent::Completed { .. }
            ] if call_id == "call_a" && name == "do_a" && arguments == "{\"foo\":1}"
        );
    }
}
