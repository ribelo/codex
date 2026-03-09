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
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;
use uuid::Uuid;

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64_000;
const SYNTHETIC_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";
const GEMINI_MODEL_FALLBACKS: &[(&str, &str)] = &[
    ("gemini-2.5-flash-image", "gemini-2.5-flash"),
    ("gemini-3-pro", "gemini-3-pro-preview"),
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCall {
    name: String,
    args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionResponse {
    name: String,
    response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallingConfig {
    mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "responseJsonSchema")]
    response_json_schema: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiThinkingConfig {
    thinking_budget: i64,
    include_thoughts: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GeminiResponseContent {
    parts: Option<Vec<GeminiPartWrapper>>,
}

#[derive(Deserialize, Debug, Clone)]
struct GeminiPartWrapper {
    text: Option<String>,
    #[serde(rename = "functionCall")]
    function_call: Option<GeminiFunctionCallResponse>,
    #[serde(rename = "thoughtSignature")]
    thought_signature: Option<String>,
    thought: Option<Value>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallResponse {
    name: String,
    args: Value,
    id: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    prompt_token_count: Option<i64>,
    candidates_token_count: Option<i64>,
    total_token_count: Option<i64>,
    thoughts_token_count: Option<i64>,
}

struct AssistantState {
    item: ResponseItem,
    added: bool,
}

struct ReasoningState {
    item: ResponseItem,
    added: bool,
    streamed_delta_count: i64,
    accumulated_text: String,
    signature: Option<String>,
}

pub(crate) async fn stream_gemini_generate_content(
    ctx: &AdapterContext<'_>,
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    session_telemetry: &SessionTelemetry,
) -> Result<ResponseStream> {
    let (payload, model_name) = build_payload(prompt, model_info, effort)?;
    let request_url = ctx.request_url_with_query(
        &format!("/models/{model_name}:streamGenerateContent"),
        &[("alt", "sse")],
    )?;
    let mut attempt = 0_u64;
    let max_retries = ctx.provider.request_max_retries();
    loop {
        attempt += 1;
        let started_at = Instant::now();
        let mut request = ctx.request_builder(request_url.clone(), http::HeaderMap::new());
        if let Some(token) = ctx.bearer_token() {
            request = if ctx.provider.use_bearer_auth {
                request.bearer_auth(token)
            } else {
                request.header("x-goog-api-key", token)
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
                tokio::spawn(process_gemini_sse(
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

fn build_payload(
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
) -> Result<(GeminiRequest, String)> {
    let model_name = normalize_model_name(&model_info.slug);
    let (mut contents, system_instruction) =
        build_messages(prompt, &prompt.base_instructions.text, &model_name)?;
    curate_gemini_history(&mut contents);
    apply_synthetic_thought_signatures(&mut contents, &model_name);

    let tools = build_tools(&prompt.tools)?;
    let tool_config = tools.as_ref().map(|_| GeminiToolConfig {
        function_calling_config: GeminiFunctionCallingConfig {
            mode: "AUTO".to_string(),
        },
    });
    let generation_config = GeminiGenerationConfig {
        max_output_tokens: Some(max_output_tokens(model_info, DEFAULT_MAX_OUTPUT_TOKENS)),
        thinking_config: model_info
            .supports_reasoning_summaries
            .then_some(GeminiThinkingConfig {
                thinking_budget: thinking_budget(effort.or(model_info.default_reasoning_level)),
                include_thoughts: true,
            }),
        response_mime_type: prompt
            .output_schema
            .as_ref()
            .map(|_| "application/json".to_string()),
        response_json_schema: prompt.output_schema.clone(),
    };

    Ok((
        GeminiRequest {
            contents,
            tools,
            tool_config,
            system_instruction,
            generation_config: Some(generation_config),
        },
        model_name,
    ))
}

fn normalize_model_name(model: &str) -> String {
    GEMINI_MODEL_FALLBACKS
        .iter()
        .find_map(|(pattern, fallback)| (*pattern == model).then(|| (*fallback).to_string()))
        .unwrap_or_else(|| model.to_string())
}

fn build_messages(
    prompt: &Prompt,
    base_instructions: &str,
    model_name: &str,
) -> Result<(Vec<GeminiContent>, Option<GeminiContent>)> {
    let mut messages = Vec::<GeminiContent>::new();
    let mut system_segments = Vec::new();
    if !base_instructions.trim().is_empty() {
        system_segments.push(base_instructions.trim_end().to_string());
    }

    let input = prompt.get_formatted_input();
    for item in &input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if role == "system" {
                    let text = flatten_content(content);
                    if !text.is_empty() {
                        system_segments.push(text);
                    }
                    continue;
                }
                if role != "user" && role != "assistant" {
                    continue;
                }
                let parts = map_message_content(content);
                if parts.is_empty() {
                    continue;
                }
                let gemini_role = if role == "user" { "user" } else { "model" };
                if let Some(last) = messages.last_mut()
                    && last.role == gemini_role
                {
                    last.parts.extend(parts);
                } else {
                    messages.push(GeminiContent {
                        role: gemini_role.to_string(),
                        parts,
                    });
                }
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let (thought_signature, id) = if call_id.starts_with("gemini_sig:") {
                    (
                        Some(call_id.trim_start_matches("gemini_sig:").to_string()),
                        None,
                    )
                } else {
                    (None, Some(call_id.clone()))
                };
                let part = GeminiPart {
                    function_call: Some(GeminiFunctionCall {
                        name: name.clone(),
                        args: parse_tool_arguments(arguments),
                        id,
                    }),
                    thought_signature,
                    ..Default::default()
                };
                if let Some(last) = messages.last_mut()
                    && last.role == "model"
                {
                    last.parts.push(part);
                } else {
                    messages.push(GeminiContent {
                        role: "model".to_string(),
                        parts: vec![part],
                    });
                }
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                let function_name = input.iter().find_map(|previous| {
                    if let ResponseItem::FunctionCall {
                        name, call_id: id, ..
                    } = previous
                        && id == call_id
                    {
                        return Some(name.clone());
                    }
                    None
                });
                let part = GeminiPart {
                    function_response: Some(GeminiFunctionResponse {
                        name: function_name.unwrap_or_else(|| "tool".to_string()),
                        response: tool_output_value(output),
                        id: Some(call_id.clone()),
                    }),
                    ..Default::default()
                };
                if let Some(last) = messages.last_mut()
                    && last.role == "user"
                    && last
                        .parts
                        .iter()
                        .all(|part| part.function_response.is_some())
                {
                    last.parts.push(part);
                } else {
                    messages.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![part],
                    });
                }
            }
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                let mut text = String::new();
                if let Some(content) = content {
                    for item in content {
                        match item {
                            ReasoningItemContent::ReasoningText { text: segment }
                            | ReasoningItemContent::Text { text: segment } => {
                                text.push_str(segment);
                            }
                        }
                    }
                } else {
                    for item in summary {
                        match item {
                            ReasoningItemReasoningSummary::SummaryText { text: segment } => {
                                text.push_str(segment);
                            }
                        }
                    }
                }
                if text.is_empty() && encrypted_content.is_none() {
                    continue;
                }
                let part = GeminiPart {
                    text: (!text.is_empty()).then_some(text),
                    thought: Some(true),
                    thought_signature: encrypted_content.clone(),
                    ..Default::default()
                };
                if let Some(last) = messages.last_mut()
                    && last.role == "model"
                {
                    last.parts.insert(0, part);
                } else {
                    messages.push(GeminiContent {
                        role: "model".to_string(),
                        parts: vec![part],
                    });
                }
            }
            ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => {}
        }
    }

    let system_instruction = (!system_segments.is_empty()).then(|| GeminiContent {
        role: "user".to_string(),
        parts: vec![GeminiPart {
            text: Some(system_segments.join("\n\n")),
            ..Default::default()
        }],
    });
    let _ = model_name;
    Ok((messages, system_instruction))
}

fn build_tools(tools: &[ToolSpec]) -> Result<Option<Vec<GeminiTool>>> {
    let mut declarations = Vec::new();
    for tool in tools {
        if let ToolSpec::Function(spec) = tool {
            let mut parameters = serde_json::to_value(&spec.parameters)?;
            strip_additional_properties(&mut parameters);
            declarations.push(GeminiFunctionDeclaration {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters: Some(parameters),
            });
        }
    }
    if declarations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(vec![GeminiTool {
            function_declarations: declarations,
        }]))
    }
}

fn strip_additional_properties(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("additionalProperties");
            for value in map.values_mut() {
                strip_additional_properties(value);
            }
        }
        Value::Array(array) => {
            for value in array {
                strip_additional_properties(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn thinking_budget(effort: Option<ReasoningEffortConfig>) -> i64 {
    match effort.unwrap_or(ReasoningEffortConfig::Medium) {
        ReasoningEffortConfig::None | ReasoningEffortConfig::Minimal => 0,
        ReasoningEffortConfig::Low => 4_096,
        ReasoningEffortConfig::Medium => 8_192,
        ReasoningEffortConfig::High => 16_384,
        ReasoningEffortConfig::XHigh => 32_768,
    }
}

fn map_message_content(content: &[ContentItem]) -> Vec<GeminiPart> {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if !text.is_empty() {
                    parts.push(GeminiPart {
                        text: Some(text.clone()),
                        ..Default::default()
                    });
                }
            }
            ContentItem::InputImage { .. } => {}
        }
    }
    parts
}

fn flatten_content(content: &[ContentItem]) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                parts.push(text.clone());
            }
            ContentItem::InputImage { image_url } => {
                parts.push(format!("[image: {image_url}]"));
            }
        }
    }
    parts.join("\n\n")
}

fn parse_tool_arguments(raw: &str) -> Value {
    match serde_json::from_str(raw) {
        Ok(Value::Object(object)) => Value::Object(object),
        Ok(other) => json!({ "value": other }),
        Err(_) => json!({ "raw": raw }),
    }
}

fn tool_output_value(output: &codex_protocol::models::FunctionCallOutputPayload) -> Value {
    if let Some(items) = output.content_items() {
        json!({ "content": items })
    } else if let Some(text) = output.text_content() {
        serde_json::from_str(text).unwrap_or_else(|_| json!({ "result": text }))
    } else {
        Value::Null
    }
}

fn curate_gemini_history(contents: &mut Vec<GeminiContent>) {
    if contents.is_empty() {
        return;
    }
    let mut merged: Vec<GeminiContent> = Vec::with_capacity(contents.len());
    for content in contents.drain(..) {
        if let Some(last) = merged.last_mut()
            && last.role == content.role
        {
            last.parts.extend(content.parts);
        } else {
            merged.push(content);
        }
    }

    let mut result = Vec::with_capacity(merged.len());
    let mut expected_role = "user";
    let mut skip_orphaned_responses = false;
    for mut content in merged {
        if skip_orphaned_responses && content.role == "user" {
            let cleaned_parts = content
                .parts
                .into_iter()
                .filter(|part| part.function_response.is_none())
                .collect::<Vec<_>>();
            if cleaned_parts.is_empty() {
                continue;
            }
            content = GeminiContent {
                role: content.role,
                parts: cleaned_parts,
            };
            skip_orphaned_responses = false;
        }

        if content.role == expected_role {
            result.push(content);
            expected_role = if expected_role == "user" {
                "model"
            } else {
                "user"
            };
            skip_orphaned_responses = false;
        } else if content.role == "model"
            && content
                .parts
                .iter()
                .any(|part| part.function_call.is_some())
        {
            skip_orphaned_responses = true;
        }
    }
    *contents = result;
}

fn apply_synthetic_thought_signatures(contents: &mut [GeminiContent], model_name: &str) {
    if !model_name.starts_with("gemini-3") {
        return;
    }
    let Some(start_index) = contents.iter().rposition(|content| {
        content.role == "user"
            && !content
                .parts
                .iter()
                .any(|part| part.function_response.is_some())
    }) else {
        return;
    };

    for content in contents.iter_mut() {
        if content.role != "model" {
            continue;
        }
        for part in &mut content.parts {
            if part.function_call.is_some() {
                part.thought_signature = None;
            }
        }
    }

    for content in &mut contents[start_index..] {
        if content.role != "model" {
            continue;
        }
        if let Some(part) = content
            .parts
            .iter_mut()
            .find(|part| part.function_call.is_some())
        {
            part.thought_signature = Some(SYNTHETIC_THOUGHT_SIGNATURE.to_string());
        }
    }
}

async fn process_gemini_sse(
    stream: impl futures::Stream<Item = std::result::Result<tokio_util::bytes::Bytes, reqwest::Error>>
    + Unpin
    + Eventsource,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    session_telemetry: SessionTelemetry,
) {
    let mut stream = stream.eventsource();
    let mut usage: Option<TokenUsage> = None;
    let mut assistant_state: Option<AssistantState> = None;
    let mut reasoning_state: Option<ReasoningState> = None;
    let mut emitted_content = false;
    let mut saw_candidates = false;

    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

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
                if let Some(mut state) = reasoning_state.take() {
                    if let ResponseItem::Reasoning {
                        summary,
                        encrypted_content,
                        ..
                    } = &mut state.item
                    {
                        if !state.accumulated_text.is_empty() {
                            summary.push(ReasoningItemReasoningSummary::SummaryText {
                                text: std::mem::take(&mut state.accumulated_text),
                            });
                        }
                        *encrypted_content = state.signature.take();
                    }
                    if !state.added {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                            .await;
                    }
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                        .await;
                }
                if let Some(state) = assistant_state.take() {
                    if !state.added {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                            .await;
                    }
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                        .await;
                    emitted_content = true;
                }
                if !emitted_content && !saw_candidates {
                    let _ = tx_event
                        .send(Err(CodexErr::Stream(
                            "Gemini returned no content".to_string(),
                            None,
                        )))
                        .await;
                    return;
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: usage,
                    }))
                    .await;
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
        let value: Value = match serde_json::from_str(&sse.data) {
            Ok(value) => value,
            Err(err) => {
                debug!("Failed to parse Gemini SSE event: {err}");
                continue;
            }
        };
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Gemini stream error")
                .to_string();
            let _ = tx_event.send(Err(CodexErr::Stream(message, None))).await;
            return;
        }
        let value = value.get("response").cloned().unwrap_or(value);
        let response: GeminiResponse = match serde_json::from_value(value) {
            Ok(response) => response,
            Err(err) => {
                debug!("Failed to decode Gemini SSE payload: {err}");
                continue;
            }
        };

        if let Some(metadata) = response.usage_metadata {
            usage = Some(TokenUsage {
                input_tokens: metadata.prompt_token_count.unwrap_or(0),
                cached_input_tokens: 0,
                output_tokens: metadata.candidates_token_count.unwrap_or(0),
                reasoning_output_tokens: metadata.thoughts_token_count.unwrap_or(0),
                total_tokens: metadata.total_token_count.unwrap_or(0),
            });
        }

        if let Some(candidates) = response.candidates {
            saw_candidates = true;
            for candidate in candidates {
                if let Some(content) = candidate.content
                    && let Some(parts) = content.parts
                {
                    for part in parts {
                        if let Some(text) = part.text.as_deref() {
                            if is_thought_value(&part.thought) {
                                append_reasoning_delta(
                                    &tx_event,
                                    &mut reasoning_state,
                                    &mut emitted_content,
                                    text,
                                    part.thought_signature.clone(),
                                )
                                .await;
                            } else {
                                append_text_delta(
                                    &tx_event,
                                    &mut assistant_state,
                                    &mut emitted_content,
                                    text,
                                )
                                .await;
                            }
                        }
                        if part.text.is_none() && part.thought_signature.is_some() {
                            append_reasoning_delta(
                                &tx_event,
                                &mut reasoning_state,
                                &mut emitted_content,
                                "",
                                part.thought_signature.clone(),
                            )
                            .await;
                        }
                        if let Some(function_call) = part.function_call {
                            handle_function_call(
                                &tx_event,
                                &mut assistant_state,
                                &mut emitted_content,
                                function_call,
                                part.thought_signature,
                            )
                            .await;
                        }
                    }
                }

                if matches!(candidate.finish_reason.as_deref(), Some("MAX_TOKENS")) {
                    let _ = tx_event.send(Err(CodexErr::ContextWindowExceeded)).await;
                    return;
                }
            }
        }
    }
}

fn is_thought_value(value: &Option<Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

async fn append_text_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
    emitted_content: &mut bool,
    text: &str,
) {
    ensure_assistant_item(assistant_state);
    let Some(state) = assistant_state.as_mut() else {
        return;
    };
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
    if let ResponseItem::Message { content, .. } = &mut state.item {
        if let Some(ContentItem::OutputText { text: existing }) = content.last_mut() {
            existing.push_str(text);
        } else {
            content.push(ContentItem::OutputText {
                text: text.to_string(),
            });
        }
    }
    let _ = tx_event
        .send(Ok(ResponseEvent::OutputTextDelta(text.to_string())))
        .await;
    *emitted_content = true;
}

async fn append_reasoning_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    reasoning_state: &mut Option<ReasoningState>,
    emitted_content: &mut bool,
    text: &str,
    signature: Option<String>,
) {
    ensure_reasoning_item(reasoning_state);
    let Some(state) = reasoning_state.as_mut() else {
        return;
    };
    if signature.is_some() {
        state.signature = signature;
    }
    if text.is_empty() {
        return;
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
    state.accumulated_text.push_str(text);
    let content_index = state.streamed_delta_count;
    state.streamed_delta_count += 1;
    let _ = tx_event
        .send(Ok(ResponseEvent::ReasoningContentDelta {
            delta: text.to_string(),
            content_index,
        }))
        .await;
    *emitted_content = true;
}

async fn handle_function_call(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
    emitted_content: &mut bool,
    function_call: GeminiFunctionCallResponse,
    thought_signature: Option<String>,
) {
    if let Some(state) = assistant_state.take() {
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(state.item)))
            .await;
    }

    let call_id = if let Some(signature) = thought_signature {
        format!("gemini_sig:{signature}")
    } else {
        function_call
            .id
            .unwrap_or_else(|| Uuid::new_v4().to_string())
    };
    let arguments = serde_json::to_string(&function_call.args).unwrap_or_default();
    let item = ResponseItem::FunctionCall {
        id: None,
        name: function_call.name,
        arguments,
        call_id,
    };
    let _ = tx_event
        .send(Ok(ResponseEvent::OutputItemAdded(item.clone())))
        .await;
    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
    *emitted_content = true;
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
    });
}

fn ensure_reasoning_item(reasoning_state: &mut Option<ReasoningState>) {
    if reasoning_state.is_some() {
        return;
    }
    *reasoning_state = Some(ReasoningState {
        item: ResponseItem::Reasoning {
            id: String::new(),
            summary: Vec::new(),
            content: Some(Vec::new()),
            encrypted_content: None,
        },
        added: false,
        streamed_delta_count: 0,
        accumulated_text: String::new(),
        signature: None,
    });
}
