use std::collections::HashMap;
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
use crate::provider_adapters::max_output_tokens;
use crate::util::backoff;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64_000;

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    max_tokens: i64,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<Value>,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    #[serde(rename = "input_schema")]
    input_schema: Value,
    strict: bool,
}

#[derive(Serialize)]
struct AnthropicOutputConfig {
    format: AnthropicOutputFormat,
}

#[derive(Serialize)]
struct AnthropicOutputFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
    name: &'static str,
    schema: Value,
}

#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    thinking_type: &'static str,
    budget_tokens: i64,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto,
}

enum BlockState {
    Thinking(ThinkingState),
    ToolUse(ToolUseState),
}

struct ThinkingState {
    item: ResponseItem,
    added: bool,
    next_index: i64,
    buffer: String,
}

struct ToolUseState {
    item: ResponseItem,
    buffer: String,
}

struct AssistantState {
    item: ResponseItem,
    added: bool,
    buffer: String,
}

pub(crate) async fn stream_anthropic_messages(
    ctx: &AdapterContext<'_>,
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    session_telemetry: &SessionTelemetry,
) -> Result<ResponseStream> {
    let payload = build_request(prompt, model_info, effort, summary)?;
    let mut attempt = 0_u64;
    let max_retries = ctx.provider.request_max_retries();
    loop {
        attempt += 1;
        let started_at = Instant::now();
        let mut request = ctx.request_builder_for_path("/messages", http::HeaderMap::new());
        if let Some(token) = ctx.bearer_token() {
            request = if ctx.provider.use_bearer_auth {
                request.bearer_auth(token)
            } else {
                request.header("x-api-key", token)
            };
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
                tokio::spawn(process_anthropic_sse(
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

fn build_request(
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
) -> Result<AnthropicRequest> {
    let (messages, extra_system) = build_messages(prompt)?;
    let tools = build_tools(&prompt.tools)?;
    let system = if let Some(extra_system) = extra_system {
        if prompt.base_instructions.text.trim().is_empty() {
            Some(extra_system)
        } else if extra_system.trim().is_empty() {
            Some(prompt.base_instructions.text.clone())
        } else {
            Some(format!(
                "{}\n\n{extra_system}",
                prompt.base_instructions.text
            ))
        }
    } else if prompt.base_instructions.text.trim().is_empty() {
        None
    } else {
        Some(prompt.base_instructions.text.clone())
    };

    let tool_choice = (!tools.is_empty()).then_some(AnthropicToolChoice::Auto);
    let thinking = model_info
        .supports_reasoning_summaries
        .then_some(AnthropicThinking {
            thinking_type: "enabled",
            budget_tokens: thinking_budget(effort.or(model_info.default_reasoning_level), summary),
        });
    let output_config = prompt
        .output_schema
        .as_ref()
        .map(|schema| AnthropicOutputConfig {
            format: AnthropicOutputFormat {
                format_type: "json_schema",
                name: "codex_output",
                schema: schema.clone(),
            },
        });

    Ok(AnthropicRequest {
        model: model_info.slug.clone(),
        system,
        messages,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        max_tokens: max_output_tokens(model_info, DEFAULT_MAX_OUTPUT_TOKENS),
        stream: true,
        thinking,
        output_config,
    })
}

fn build_messages(prompt: &Prompt) -> Result<(Vec<AnthropicMessage>, Option<String>)> {
    let mut messages = Vec::new();
    let mut pending_assistant: Option<AnthropicMessage> = None;
    let mut system_segments = Vec::new();

    for item in prompt.get_formatted_input() {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if role == "system" {
                    let text = flatten_content(&content);
                    if !text.is_empty() {
                        system_segments.push(text);
                    }
                    continue;
                }
                if role != "user" && role != "assistant" {
                    continue;
                }
                let blocks = map_message_content(&content);
                if blocks.is_empty() {
                    continue;
                }
                if role == "assistant" {
                    ensure_pending_assistant(&mut pending_assistant)
                        .content
                        .extend(blocks);
                } else {
                    flush_pending_assistant(&mut messages, &mut pending_assistant);
                    messages.push(AnthropicMessage {
                        role,
                        content: blocks,
                    });
                }
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                ensure_pending_assistant(&mut pending_assistant)
                    .content
                    .push(json!({
                        "type": "tool_use",
                        "id": sanitize_tool_id(&call_id),
                        "name": name,
                        "input": parse_tool_arguments(&arguments),
                    }));
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                flush_pending_assistant(&mut messages, &mut pending_assistant);
                if let Some(block) = tool_result_block(&call_id, &output) {
                    messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: vec![block],
                    });
                }
            }
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                ensure_pending_assistant(&mut pending_assistant)
                    .content
                    .push(json!({
                        "type": "tool_use",
                        "id": sanitize_tool_id(&call_id),
                        "name": name,
                        "input": parse_tool_arguments(&input),
                    }));
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                flush_pending_assistant(&mut messages, &mut pending_assistant);
                if let Some(block) = tool_result_block(&call_id, &output) {
                    messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: vec![block],
                    });
                }
            }
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                let text = if let Some(content) = content {
                    content
                        .into_iter()
                        .map(|item| match item {
                            codex_protocol::models::ReasoningItemContent::ReasoningText {
                                text,
                            }
                            | codex_protocol::models::ReasoningItemContent::Text { text } => text,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    summary
                        .into_iter()
                        .map(|item| match item {
                            ReasoningItemReasoningSummary::SummaryText { text } => text,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                if text.is_empty() && encrypted_content.is_none() {
                    continue;
                }
                let mut block = json!({
                    "type": "thinking",
                    "thinking": text,
                });
                if let Some(signature) = encrypted_content {
                    block["signature"] = json!(signature);
                }
                ensure_pending_assistant(&mut pending_assistant)
                    .content
                    .push(block);
            }
            ResponseItem::LocalShellCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => {}
        }
    }

    flush_pending_assistant(&mut messages, &mut pending_assistant);
    let extra_system = (!system_segments.is_empty()).then(|| system_segments.join("\n\n"));
    Ok((messages, extra_system))
}

fn build_tools(tools: &[ToolSpec]) -> Result<Vec<AnthropicTool>> {
    let mut out = Vec::new();
    for tool in tools {
        if let ToolSpec::Function(spec) = tool {
            out.push(AnthropicTool {
                name: spec.name.clone(),
                description: spec.description.clone(),
                input_schema: serde_json::to_value(&spec.parameters)?,
                strict: spec.strict,
            });
        }
    }
    Ok(out)
}

fn thinking_budget(effort: Option<ReasoningEffortConfig>, summary: ReasoningSummaryConfig) -> i64 {
    let base = match effort.unwrap_or(ReasoningEffortConfig::Medium) {
        ReasoningEffortConfig::None | ReasoningEffortConfig::Minimal => 0,
        ReasoningEffortConfig::Low => 2_048,
        ReasoningEffortConfig::Medium => 8_192,
        ReasoningEffortConfig::High => 16_384,
        ReasoningEffortConfig::XHigh => 32_768,
    };
    if summary == ReasoningSummaryConfig::None {
        base.max(1_024)
    } else {
        base
    }
}

fn sanitize_tool_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn flush_pending_assistant(
    messages: &mut Vec<AnthropicMessage>,
    pending: &mut Option<AnthropicMessage>,
) {
    if let Some(message) = pending.take()
        && !message.content.is_empty()
    {
        messages.push(message);
    }
}

fn ensure_pending_assistant(pending: &mut Option<AnthropicMessage>) -> &mut AnthropicMessage {
    pending.get_or_insert_with(|| AnthropicMessage {
        role: "assistant".to_string(),
        content: Vec::new(),
    })
}

fn map_message_content(content: &[ContentItem]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if !text.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
            }
            ContentItem::InputImage { image_url } => {
                blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": image_url,
                    }
                }));
            }
        }
    }
    blocks
}

fn flatten_content(content: &[ContentItem]) -> String {
    let mut out = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                out.push(text.clone());
            }
            ContentItem::InputImage { image_url } => {
                out.push(format!("[image: {image_url}]"));
            }
        }
    }
    out.join("\n\n")
}

fn parse_tool_arguments(raw: &str) -> Value {
    match serde_json::from_str(raw) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(other) => json!({ "value": other }),
        Err(_) => json!({ "raw": raw }),
    }
}

fn tool_result_block(call_id: &str, output: &FunctionCallOutputPayload) -> Option<Value> {
    let is_error = matches!(output.success, Some(false));
    if let Some(items) = output.content_items() {
        let content = items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => {
                    json!({ "type": "text", "text": text })
                }
                FunctionCallOutputContentItem::InputImage { image_url, .. } => json!({
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": image_url,
                    }
                }),
            })
            .collect::<Vec<_>>();
        return Some(json!({
            "type": "tool_result",
            "tool_use_id": sanitize_tool_id(call_id),
            "content": content,
            "is_error": is_error,
        }));
    }
    let content = output.text_content().unwrap_or_default();
    if content.is_empty() {
        return None;
    }
    Some(json!({
        "type": "tool_result",
        "tool_use_id": sanitize_tool_id(call_id),
        "content": content,
        "is_error": is_error,
    }))
}

async fn process_anthropic_sse(
    stream: impl futures::Stream<Item = std::result::Result<tokio_util::bytes::Bytes, reqwest::Error>>
    + Unpin
    + Eventsource,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    session_telemetry: SessionTelemetry,
) {
    let mut stream = stream.eventsource();
    let mut response_id: Option<String> = None;
    let mut usage = None;
    let mut blocks: HashMap<i64, BlockState> = HashMap::new();
    let mut seen_message_start = false;
    let mut emitted_anything = false;
    let mut assistant_state: Option<AssistantState> = None;

    loop {
        let started_at = Instant::now();
        let next_event = timeout(idle_timeout, stream.next()).await;
        session_telemetry.log_sse_event(&next_event, started_at.elapsed());
        let sse = match next_event {
            Ok(Some(Ok(event))) => event,
            Ok(Some(Err(source))) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(source.to_string(), None)))
                    .await;
                return;
            }
            Ok(None) => {
                if seen_message_start || emitted_anything {
                    if let Some(state) = assistant_state.take() {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                            .await;
                    }
                    let _ = tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id: response_id.unwrap_or_default(),
                            token_usage: usage,
                        }))
                        .await;
                } else {
                    let _ = tx_event
                        .send(Err(CodexErr::Stream(
                            "stream closed before message_stop".to_string(),
                            None,
                        )))
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
        let event: Value = match serde_json::from_str(&sse.data) {
            Ok(value) => value,
            Err(err) => {
                debug!("Failed to parse Anthropic SSE event: {err}");
                continue;
            }
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };

        match kind {
            "message_start" => {
                seen_message_start = true;
                response_id = event
                    .get("message")
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
                    return;
                }
            }
            "message_delta" => {
                if let Some(value) = event.get("usage") {
                    usage = parse_usage(value);
                }
            }
            "message_stop" => {
                if let Some(state) = assistant_state.take() {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                        .await;
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: response_id.unwrap_or_default(),
                        token_usage: usage,
                    }))
                    .await;
                return;
            }
            "content_block_start" => {
                handle_block_start(&event, &tx_event, &mut blocks, &mut assistant_state).await;
            }
            "content_block_delta" => {
                if handle_text_delta(&event, &tx_event, &mut assistant_state).await {
                    emitted_anything = true;
                } else {
                    handle_block_delta(&event, &tx_event, &mut blocks).await;
                    emitted_anything = true;
                }
            }
            "content_block_stop" => {
                handle_block_stop(&event, &tx_event, &mut blocks).await;
            }
            "error" => {
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("anthropic stream error")
                    .to_string();
                let _ = tx_event.send(Err(CodexErr::Stream(message, None))).await;
                return;
            }
            "ping" => {}
            _ => {
                trace!("Ignoring Anthropic SSE event type `{kind}`");
            }
        }
    }
}

async fn handle_block_start(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i64, BlockState>,
    assistant_state: &mut Option<AssistantState>,
) {
    let Some(index) = event.get("index").and_then(Value::as_i64) else {
        return;
    };
    let Some(block) = event.get("content_block") else {
        return;
    };
    match block.get("type").and_then(Value::as_str) {
        Some("text") => ensure_assistant_item(assistant_state),
        Some("thinking") | Some("redacted_thinking") => {
            blocks.insert(
                index,
                BlockState::Thinking(ThinkingState {
                    item: ResponseItem::Reasoning {
                        id: String::new(),
                        summary: Vec::new(),
                        content: None,
                        encrypted_content: None,
                    },
                    added: false,
                    next_index: 0,
                    buffer: String::new(),
                }),
            );
        }
        Some("tool_use") => {
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                return;
            };
            let Some(call_id) = block.get("id").and_then(Value::as_str) else {
                return;
            };
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            let initial = if input.is_null()
                || matches!(input, Value::Object(ref object) if object.is_empty())
            {
                String::new()
            } else {
                serde_json::to_string(&input).unwrap_or_default()
            };
            let state = ToolUseState {
                item: ResponseItem::FunctionCall {
                    id: None,
                    name: name.to_string(),
                    arguments: initial.clone(),
                    call_id: call_id.to_string(),
                },
                buffer: initial,
            };
            if tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                .await
                .is_err()
            {
                return;
            }
            blocks.insert(index, BlockState::ToolUse(state));
        }
        _ => {}
    }
}

async fn handle_block_delta(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i64, BlockState>,
) {
    let Some(index) = event.get("index").and_then(Value::as_i64) else {
        return;
    };
    let Some(delta) = event.get("delta") else {
        return;
    };
    let Some(block_state) = blocks.get_mut(&index) else {
        return;
    };
    match block_state {
        BlockState::Thinking(state) => {
            let Some(text) = delta
                .get("thinking")
                .or_else(|| delta.get("text"))
                .and_then(Value::as_str)
            else {
                return;
            };
            state.buffer.push_str(text);
            if !state.added {
                state.added = true;
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let summary_index = state.next_index;
            state.next_index += 1;
            let _ = tx_event
                .send(Ok(ResponseEvent::ReasoningSummaryDelta {
                    delta: text.to_string(),
                    summary_index,
                }))
                .await;
        }
        BlockState::ToolUse(state) => {
            if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                state.buffer.push_str(fragment);
            }
        }
    }
}

async fn handle_block_stop(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i64, BlockState>,
) {
    let Some(index) = event.get("index").and_then(Value::as_i64) else {
        return;
    };
    let Some(state) = blocks.remove(&index) else {
        return;
    };
    match state {
        BlockState::Thinking(mut state) => {
            if let ResponseItem::Reasoning { summary, .. } = &mut state.item
                && !state.buffer.is_empty()
            {
                summary.push(ReasoningItemReasoningSummary::SummaryText { text: state.buffer });
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                .await;
        }
        BlockState::ToolUse(mut state) => {
            if let ResponseItem::FunctionCall { arguments, .. } = &mut state.item
                && !state.buffer.is_empty()
            {
                *arguments = state.buffer;
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                .await;
        }
    }
}

async fn handle_text_delta(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
) -> bool {
    let Some(delta) = event.get("delta") else {
        return false;
    };
    if delta
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "text_delta")
        && let Some(text) = delta.get("text").and_then(Value::as_str)
    {
        append_text_delta(tx_event, assistant_state, text).await;
        return true;
    }
    false
}

async fn append_text_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
    text: &str,
) {
    ensure_assistant_item(assistant_state);
    let Some(state) = assistant_state.as_mut() else {
        return;
    };

    let addition = if state.buffer.is_empty() {
        text.to_string()
    } else if text.starts_with(&state.buffer) {
        text[state.buffer.len()..].to_string()
    } else if state.buffer.starts_with(text) {
        String::new()
    } else {
        text.to_string()
    };
    if addition.is_empty() {
        return;
    }

    state.buffer.push_str(&addition);
    if let ResponseItem::Message { content, .. } = &mut state.item {
        if let Some(ContentItem::OutputText { text }) = content.last_mut() {
            text.push_str(&addition);
        } else {
            content.push(ContentItem::OutputText {
                text: addition.clone(),
            });
        }
    }
    if !state.added {
        state.added = true;
        if tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = tx_event
        .send(Ok(ResponseEvent::OutputTextDelta(addition)))
        .await;
}

fn ensure_assistant_item(assistant_state: &mut Option<AssistantState>) {
    if assistant_state.is_some() {
        return;
    }
    *assistant_state = Some(AssistantState {
        item: ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
            end_turn: None,
            phase: None,
        },
        added: false,
        buffer: String::new(),
    });
}

fn parse_usage(value: &Value) -> Option<codex_protocol::protocol::TokenUsage> {
    let input_tokens = value.get("input_tokens").and_then(Value::as_i64)?;
    let output_tokens = value.get("output_tokens").and_then(Value::as_i64)?;
    let cached_input_tokens = value
        .get("cache_read_input_tokens")
        .or_else(|| value.get("cache_creation_input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let reasoning_output_tokens = value
        .get("output_tokens_details")
        .and_then(|details| {
            details
                .get("thinking_tokens")
                .or_else(|| details.get("reasoning_tokens"))
        })
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(codex_protocol::protocol::TokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens,
        total_tokens: input_tokens + output_tokens,
    })
}
