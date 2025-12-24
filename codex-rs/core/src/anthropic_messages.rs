#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::bytes::Bytes;
use tracing::debug;
use tracing::info;
use tracing::trace;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::tools::ToolSpec;
use crate::config::Config;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::Result;
use crate::error::RetryLimitReachedError;
use crate::error::UnexpectedResponseError;
use crate::model_provider_info::AnthropicProvider;
use crate::openai_models::model_family::ModelFamily;
use crate::protocol::TokenUsage;
use crate::truncate::approx_token_count;
use crate::util::backoff;
use codex_client::CodexHttpClient;

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
    output_format: Option<AnthropicOutputFormat>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<Value>,
}

#[derive(Serialize)]
struct AnthropicOutputFormat {
    #[serde(rename = "type")]
    format_type: String,
    schema: Value,
}

/// Sanitize tool ID to match Anthropic's strict requirements (alphanumeric, hyphen, underscore).
/// Matches Opencode's logic: `part.toolCallId.replace(/[^a-zA-Z0-9_-]/g, "_")`
fn sanitize_tool_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
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
    if let Some(msg) = pending.take()
        && !msg.content.is_empty()
    {
        messages.push(msg);
    }
}

fn ensure_pending_assistant(pending: &mut Option<AnthropicMessage>) -> &mut AnthropicMessage {
    pending.get_or_insert_with(|| AnthropicMessage {
        role: "assistant".to_string(),
        content: Vec::new(),
    })
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    #[serde(rename = "input_schema")]
    input_schema: Value,
}

#[derive(Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<i64>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Auto,
}

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64_000;

pub(crate) async fn stream_anthropic_messages(
    prompt: &Prompt,
    config: &Config,
    model_family: &ModelFamily,
    client: &CodexHttpClient,
    provider: &AnthropicProvider,
    otel_event_manager: &OtelEventManager,
) -> Result<ResponseStream> {
    let full_instructions = prompt.get_full_instructions(model_family).to_string();
    let (messages, extra_system) = build_anthropic_messages(prompt)?;
    let mut system_prompt = full_instructions;
    if let Some(extra) = extra_system
        && !extra.trim().is_empty()
    {
        system_prompt = format!("{system_prompt}\n\n{extra}");
    }

    let tools = build_anthropic_tools(&prompt.tools)?;
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(AnthropicToolChoice::Auto)
    };

    let thinking = model_family
        .supports_reasoning_summaries
        .then_some(AnthropicThinking {
            r#type: "enabled",
            budget_tokens: None,
        });
    let prompt_tokens =
        anthropic_prompt_token_estimate(&system_prompt, &messages, &tools, &tool_choice, &thinking);
    let max_tokens = resolve_max_tokens(
        config,
        model_family,
        prompt_tokens,
        DEFAULT_MAX_OUTPUT_TOKENS,
    );
    let thinking = thinking.map(|cfg| AnthropicThinking {
        budget_tokens: Some(resolve_thinking_budget(
            config
                .model_reasoning_effort
                .or(model_family.default_reasoning_effort),
            max_tokens,
        )),
        ..cfg
    });

    let output_format = prompt
        .output_schema
        .as_ref()
        .map(|schema| AnthropicOutputFormat {
            format_type: "json_schema".to_string(),
            schema: schema.clone(),
        });

    let payload = AnthropicRequest {
        model: model_family.get_model_slug().to_string(),
        system: Some(system_prompt),
        messages,
        tools: if tools.is_empty() { None } else { Some(tools) },
        tool_choice,
        max_tokens,
        stream: true,
        thinking,
        output_format,
    };

    let payload_json = serde_json::to_value(&payload)?;
    let request_url = provider.get_full_url();
    info!(
        provider = %provider.name,
        url = %request_url,
        payload = %payload_json,
        "Dispatching anthropic Messages request"
    );
    trace!("POST to {}: {}", request_url, payload_json);

    let mut attempt = 0_u64;
    let max_retries = provider.request_max_retries();
    loop {
        attempt += 1;

        let mut req_builder = provider.create_request_builder(client).await?;
        if payload_json.get("output_format").is_some() {
            req_builder = req_builder.header("anthropic-beta", "structured-outputs-2025-11-13");
        }

        let res = otel_event_manager
            .log_request(attempt, || {
                req_builder
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&payload_json)
                    .send()
            })
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => {
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
                let stream = resp.bytes_stream();
                tokio::spawn(process_anthropic_sse(
                    stream,
                    tx_event,
                    provider.stream_idle_timeout(),
                    otel_event_manager.clone(),
                ));
                return Ok(ResponseStream { rx_event });
            }
            Ok(resp) => {
                let status = resp.status();
                // Retry on rate limits, transient timeouts (408), conflicts (409), and server errors.
                let is_retryable = status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::CONFLICT
                    || status.is_server_error();

                if !is_retryable {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        request_id: None,
                    }));
                }

                if attempt > max_retries {
                    return Err(CodexErr::RetryLimit(RetryLimitReachedError {
                        status,
                        request_id: None,
                    }));
                }

                // Prefer millisecond-precision retry-after-ms if present, else fall back to seconds.
                let retry_after_ms = resp
                    .headers()
                    .get("retry-after-ms")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let retry_after_secs = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                let delay = retry_after_ms
                    .map(Duration::from_millis)
                    .or_else(|| retry_after_secs.map(Duration::from_secs))
                    .unwrap_or_else(|| backoff(attempt));
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                if attempt > max_retries {
                    return Err(CodexErr::ConnectionFailed(ConnectionFailedError {
                        source: e,
                    }));
                }
                let delay = backoff(attempt);
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn build_anthropic_messages(prompt: &Prompt) -> Result<(Vec<AnthropicMessage>, Option<String>)> {
    let mut messages = Vec::new();
    let mut pending_assistant: Option<AnthropicMessage> = None;
    let mut system_segments = Vec::new();
    let input = prompt.get_formatted_input();

    for item in input {
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
                    let pending = ensure_pending_assistant(&mut pending_assistant);
                    pending.content.extend(blocks);
                } else {
                    flush_pending_assistant(&mut messages, &mut pending_assistant);
                    messages.push(AnthropicMessage {
                        role: role.clone(),
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
                let input_json = parse_tool_arguments(&arguments);
                let pending = ensure_pending_assistant(&mut pending_assistant);
                pending.content.push(json!({
                    "type": "tool_use",
                    "id": sanitize_tool_id(&call_id),
                    "name": name,
                    "input": input_json,
                }));
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                flush_pending_assistant(&mut messages, &mut pending_assistant);
                if let Some(block) = build_tool_result_block(&sanitize_tool_id(&call_id), &output) {
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
                let input_json = parse_tool_arguments(&input);
                let pending = ensure_pending_assistant(&mut pending_assistant);
                pending.content.push(json!({
                    "type": "tool_use",
                    "id": sanitize_tool_id(&call_id),
                    "name": name,
                    "input": input_json,
                }));
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                flush_pending_assistant(&mut messages, &mut pending_assistant);
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: vec![json!({
                        "type": "tool_result",
                        "tool_use_id": sanitize_tool_id(&call_id),
                        "content": output,
                    })],
                });
            }
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                // Build the thinking text from content or summary
                let thinking_text = if let Some(content_items) = content {
                    content_items
                        .iter()
                        .map(|c| match c {
                            codex_protocol::models::ReasoningItemContent::ReasoningText {
                                text,
                            } => text.as_str(),
                            codex_protocol::models::ReasoningItemContent::Text { text } => {
                                text.as_str()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    summary
                        .iter()
                        .map(|s| {
                            match s {
                            codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                                text,
                            } => text.as_str(),
                        }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };

                if !thinking_text.is_empty() || encrypted_content.is_some() {
                    let pending = ensure_pending_assistant(&mut pending_assistant);
                    // If we have encrypted content but no thinking text, use redacted_thinking
                    if thinking_text.is_empty() {
                        if let Some(data) = encrypted_content {
                            pending.content.push(json!({
                                "type": "redacted_thinking",
                                "data": data,
                            }));
                        }
                    } else {
                        // Use thinking block with optional signature
                        let mut block = json!({
                            "type": "thinking",
                            "thinking": thinking_text,
                        });
                        if let Some(sig) = encrypted_content {
                            block["signature"] = json!(sig);
                        }
                        pending.content.push(block);
                    }
                }
            }
            ResponseItem::WebSearchCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::CompactionSummary { .. }
            | ResponseItem::Other => {}
        }
    }

    flush_pending_assistant(&mut messages, &mut pending_assistant);

    let extra_system = if system_segments.is_empty() {
        None
    } else {
        Some(system_segments.join("\n\n"))
    };

    Ok((messages, extra_system))
}

fn build_anthropic_tools(tools: &[ToolSpec]) -> Result<Vec<AnthropicTool>> {
    let mut out = Vec::new();
    for tool in tools {
        if let ToolSpec::Function(spec) = tool {
            let schema = serde_json::to_value(&spec.parameters)?;
            out.push(AnthropicTool {
                name: spec.name.clone(),
                description: spec.description.clone(),
                input_schema: schema,
            });
        }
    }
    Ok(out)
}

fn map_message_content(content: &[ContentItem]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
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
    let mut acc = String::new();
    for entry in content {
        match entry {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !acc.is_empty() {
                    acc.push_str("\n\n");
                }
                acc.push_str(text);
            }
            ContentItem::InputImage { image_url } => {
                if !acc.is_empty() {
                    acc.push_str("\n\n");
                }
                acc.push_str(&format!("[image: {image_url}]"));
            }
        }
    }
    acc
}

fn parse_tool_arguments(raw: &str) -> Value {
    match serde_json::from_str(raw) {
        Ok(Value::Object(obj)) => Value::Object(obj),
        Ok(other) => json!({ "value": other }),
        Err(_) => json!({ "raw": raw }),
    }
}

fn build_tool_result_block(call_id: &str, output: &FunctionCallOutputPayload) -> Option<Value> {
    let is_error = matches!(output.success, Some(false));
    if let Some(items) = &output.content_items {
        let mapped: Vec<Value> = items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => json!({
                    "type": "text",
                    "text": text,
                }),
                FunctionCallOutputContentItem::InputImage { image_url } => json!({
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": image_url,
                    }
                }),
            })
            .collect();
        return Some(json!({
            "type": "tool_result",
            "tool_use_id": sanitize_tool_id(call_id),
            "content": mapped,
            "is_error": is_error,
        }));
    }

    if output.content.is_empty() {
        return None;
    }

    Some(json!({
        "type": "tool_result",
        "tool_use_id": sanitize_tool_id(call_id),
        "content": output.content,
        "is_error": is_error,
    }))
}

fn anthropic_prompt_token_estimate(
    system_prompt: &str,
    messages: &[AnthropicMessage],
    tools: &[AnthropicTool],
    tool_choice: &Option<AnthropicToolChoice>,
    thinking: &Option<AnthropicThinking>,
) -> i64 {
    let prompt_json = json!({
        "system": system_prompt,
        "messages": messages,
        "tools": if tools.is_empty() {
            Value::Null
        } else {
            serde_json::to_value(tools).unwrap_or(Value::Null)
        },
        "tool_choice": tool_choice,
        "thinking": thinking,
    });
    i64::try_from(approx_token_count(&prompt_json.to_string())).unwrap_or(i64::MAX)
}

fn resolve_max_tokens(
    config: &Config,
    model_family: &ModelFamily,
    prompt_tokens: i64,
    hard_cap: i64,
) -> i64 {
    let window = effective_context_window(config, model_family);
    let remaining = window.saturating_sub(prompt_tokens).max(1);
    remaining.min(hard_cap)
}

fn effective_context_window(config: &Config, model_family: &ModelFamily) -> i64 {
    config
        .model_context_window
        .saturating_mul(model_family.effective_context_window_percent)
        / 100
}

async fn process_anthropic_sse<S, E>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    otel_event_manager: OtelEventManager,
) where
    S: futures::Stream<Item = std::result::Result<Bytes, E>> + Unpin + Eventsource,
    E: std::fmt::Display,
{
    let mut stream = stream.eventsource();
    let mut response_id: Option<String> = None;
    let mut usage: Option<TokenUsage> = None;
    let mut blocks: HashMap<i64, BlockState> = HashMap::new();
    let mut seen_message_start = false;
    let mut emitted_anything = false;
    let mut assistant_state: Option<AssistantState> = None;

    loop {
        let start = std::time::Instant::now();
        let next_event = timeout(idle_timeout, stream.next()).await;
        let duration = start.elapsed();
        otel_event_manager.log_sse_event(&next_event, duration);

        let sse = match next_event {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(e.to_string(), None)))
                    .await;
                return;
            }
            Ok(None) => {
                if seen_message_start || emitted_anything {
                    if let Some(state) = assistant_state.take() {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemDone(state.item.clone())))
                            .await;
                    }
                    let _ = tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id: response_id.clone().unwrap_or_default(),
                            token_usage: usage.clone(),
                        }))
                        .await;
                } else {
                    let _ = tx_event
                        .send(Err(CodexErr::Stream(
                            "stream closed before message_stop".into(),
                            None,
                        )))
                        .await;
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(
                        "idle timeout waiting for SSE".into(),
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
            Ok(val) => val,
            Err(err) => {
                debug!("Failed to parse SSE event: {err}");
                continue;
            }
        };
        info!("anthropic SSE event: {event}");
        let Some(kind) = event.get("type").and_then(|v| v.as_str()) else {
            continue;
        };

        match kind {
            "message_start" => {
                seen_message_start = true;
                response_id = event
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
                    return;
                }
            }
            "message_delta" => {
                if let Some(usage_val) = event.get("usage")
                    && let Some(parsed) = parse_usage(usage_val)
                {
                    usage = Some(parsed);
                }
            }
            "message_stop" => {
                if let Some(token_usage) = &usage {
                    otel_event_manager.sse_event_completed(
                        token_usage.input_tokens,
                        token_usage.output_tokens,
                        Some(token_usage.cached_input_tokens),
                        Some(token_usage.reasoning_output_tokens),
                        token_usage.total_tokens,
                    );
                }
                if let Some(state) = assistant_state.take() {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(state.item.clone())))
                        .await;
                }
                let event = ResponseEvent::Completed {
                    response_id: response_id.clone().unwrap_or_default(),
                    token_usage: usage.clone(),
                };
                let _ = tx_event.send(Ok(event)).await;
                return;
            }
            "content_block_start" => {
                handle_block_start(&event, &tx_event, &mut blocks, &mut assistant_state).await;
            }
            "content_block_delta" => {
                if handle_text_delta(&event, &tx_event, &mut assistant_state).await {
                    emitted_anything = true;
                } else {
                    handle_block_delta(&event, &tx_event, &mut blocks, &mut assistant_state).await;
                    emitted_anything = true;
                }
            }
            "content_block_stop" => {
                handle_block_stop(&event, &tx_event, &mut blocks).await;
            }
            "error" => {
                let message = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| "anthropic stream error".to_string());
                let _ = tx_event.send(Err(CodexErr::Stream(message, None))).await;
                return;
            }
            "ping" => {}
            _ => {}
        }
    }
}

async fn handle_block_start(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i64, BlockState>,
    assistant_state: &mut Option<AssistantState>,
) {
    let Some(index) = event.get("index").and_then(serde_json::Value::as_i64) else {
        return;
    };
    let Some(block) = event.get("content_block") else {
        return;
    };
    let Some(block_type) = block.get("type").and_then(|v| v.as_str()) else {
        return;
    };

    match block_type {
        "text" => {
            trace!("handle_block_start: text block at index {}", index);
            ensure_assistant_item(assistant_state);
        }
        "thinking" | "redacted_thinking" => {
            trace!("handle_block_start: THINKING block at index {}", index);
            let state = ThinkingState {
                item: ResponseItem::Reasoning {
                    id: String::new(),
                    summary: Vec::new(),
                    content: Some(Vec::new()),
                    encrypted_content: None,
                },
                added: false,
                next_index: 0,
                buffer: String::new(),
            };
            blocks.insert(index, BlockState::Thinking(state));
        }
        "tool_use" => {
            let name = block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let call_id = block
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            let initial =
                if input.is_null() || matches!(input, Value::Object(ref o) if o.is_empty()) {
                    String::new()
                } else {
                    serde_json::to_string(&input).unwrap_or_default()
                };
            let mut state = ToolUseState {
                item: ResponseItem::FunctionCall {
                    id: None,
                    name,
                    arguments: initial.clone(),
                    call_id,
                },
                buffer: initial,
                added: false,
            };
            if tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                .await
                .is_err()
            {
                return;
            }
            state.added = true;
            blocks.insert(index, BlockState::ToolUse(state));
        }
        _ => {
            trace!(
                "handle_block_start: unknown block type '{}' at index {}",
                block_type, index
            );
        }
    }
}

async fn handle_block_delta(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i64, BlockState>,
    assistant_state: &mut Option<AssistantState>,
) {
    let Some(index) = event.get("index").and_then(serde_json::Value::as_i64) else {
        return;
    };
    let Some(delta) = event.get("delta") else {
        return;
    };
    if let Some(block_state) = blocks.get_mut(&index) {
        match block_state {
            BlockState::Thinking(state) => {
                append_thinking_delta(tx_event, state, delta).await;
            }
            BlockState::ToolUse(state) => {
                if let Some(fragment) = delta.get("partial_json").and_then(|v| v.as_str()) {
                    state.buffer.push_str(fragment);
                }
            }
        }
    } else if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
        append_text_delta(tx_event, assistant_state, text).await;
    }
}

async fn append_thinking_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    state: &mut ThinkingState,
    delta: &Value,
) {
    let Some(text) = delta
        .get("thinking")
        .or_else(|| delta.get("text"))
        .and_then(|v| v.as_str())
    else {
        trace!(
            "append_thinking_delta: no thinking/text found in delta: {:?}",
            delta
        );
        return;
    };

    trace!(
        "append_thinking_delta: received thinking text of {} chars",
        text.len()
    );

    // Accumulate thinking text into buffer for final item
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

    let content_index = state.next_index;
    state.next_index += 1;

    let _ = tx_event
        .send(Ok(ResponseEvent::ReasoningSummaryDelta {
            delta: text.to_string(),
            summary_index: content_index,
        }))
        .await;
}

async fn handle_block_stop(
    event: &Value,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i64, BlockState>,
) {
    let Some(index) = event.get("index").and_then(serde_json::Value::as_i64) else {
        return;
    };
    if let Some(state) = blocks.remove(&index) {
        match state {
            BlockState::Thinking(mut state) => {
                // Populate the item's summary with accumulated thinking text so it
                // is always surfaced to UIs, regardless of the raw‑reasoning flag.
                if let ResponseItem::Reasoning {
                    ref mut summary, ..
                } = state.item
                    && !state.buffer.is_empty()
                {
                    *summary =
                        vec![ReasoningItemReasoningSummary::SummaryText { text: state.buffer }];
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                    .await;
            }
            BlockState::ToolUse(mut state) => {
                if let ResponseItem::FunctionCall { arguments, .. } = &mut state.item
                    && !state.buffer.is_empty()
                {
                    *arguments = state.buffer.clone();
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                    .await;
            }
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
        .and_then(|v| v.as_str())
        .map(|t| t == "text_delta")
        .unwrap_or(false)
        && let Some(text) = delta.get("text").and_then(|v| v.as_str())
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
    if let Some(state) = assistant_state.as_mut() {
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
            if let Some(ContentItem::OutputText { text, .. }) = content.last_mut() {
                text.push_str(&addition);
            } else {
                content.push(ContentItem::OutputText {
                    text: addition.clone(),
                    signature: None,
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
}

fn ensure_assistant_item(assistant_state: &mut Option<AssistantState>) {
    if assistant_state.is_some() {
        return;
    }
    let item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: Vec::new(),
    };
    let state = AssistantState {
        item,
        added: false,
        buffer: String::new(),
    };
    *assistant_state = Some(state);
    // We'll emit OutputItemAdded when the first text delta arrives.
}

fn parse_usage(value: &Value) -> Option<TokenUsage> {
    let input_tokens = value
        .get("input_tokens")
        .and_then(serde_json::Value::as_i64)?;
    let output_tokens = value
        .get("output_tokens")
        .and_then(serde_json::Value::as_i64)?;
    let cached = value
        .get("cache_read_input_tokens")
        .or_else(|| value.get("cache_creation_input_tokens"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let reasoning_tokens = value
        .get("output_tokens_details")
        .and_then(|d| {
            d.get("thinking_tokens")
                .or_else(|| d.get("reasoning_tokens"))
        })
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let total_tokens = input_tokens + output_tokens;
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens: cached,
        output_tokens,
        reasoning_output_tokens: reasoning_tokens,
        total_tokens,
    })
}

enum BlockState {
    Thinking(ThinkingState),
    ToolUse(ToolUseState),
}

struct AssistantState {
    item: ResponseItem,
    added: bool,
    buffer: String,
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
    added: bool,
}

fn resolve_thinking_budget(effort: Option<ReasoningEffort>, max_tokens: i64) -> i64 {
    let target_budget = match effort {
        Some(ReasoningEffort::Low)
        | Some(ReasoningEffort::Minimal)
        | Some(ReasoningEffort::None) => 4_096,
        Some(ReasoningEffort::Medium) => 16_384,
        Some(ReasoningEffort::High) | Some(ReasoningEffort::XHigh) => 32_768,
        None => max_tokens * 4 / 5, // Default heuristic if no effort set
    };

    // Ensure we always leave room for the response.
    // Anthropic requires budget_tokens < max_tokens.
    // We reserve at least 20% or 4k tokens (whichever is smaller) for response,
    // but ensuring strictly < max_tokens.
    let reserved = (max_tokens / 5).clamp(1024, 4096);
    let hard_cap = max_tokens.saturating_sub(reserved).max(1);

    // If target budget fits, use it. Otherwise, clamp to hard cap.
    if target_budget < max_tokens {
        target_budget.min(hard_cap)
    } else {
        hard_cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_protocol::models::ResponseItem;
    use tokio::sync::mpsc;
    use tokio_util::io::ReaderStream;

    #[test]
    fn parse_usage_does_not_double_count() {
        let json = json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 10,
            "output_tokens_details": {
                "thinking_tokens": 5
            }
        });
        let usage = parse_usage(&json).unwrap();
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cached_input_tokens, 10);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.reasoning_output_tokens, 5);
    }

    fn otel() -> OtelEventManager {
        OtelEventManager::new(
            codex_protocol::ConversationId::new(),
            "claude-test",
            "claude-test",
            None,
            None,
            None,
            false,
            "test-agent".to_string(),
        )
    }

    #[tokio::test]
    async fn sse_streams_text_and_completion() {
        let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\"}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_delta
data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(16);
        let stream = ReaderStream::new(std::io::Cursor::new(body));
        tokio::spawn(process_anthropic_sse(
            stream,
            tx,
            Duration::from_millis(10_000),
            otel(),
        ));

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }

        assert!(matches!(events[0], ResponseEvent::Created));
        assert!(matches!(
            events[1],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        ));
        assert!(matches!(
            events[2],
            ResponseEvent::OutputTextDelta(ref text) if text == "Hello"
        ));
        assert!(matches!(
            events[3],
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
        ));
        assert!(matches!(events[4], ResponseEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn thinking_blocks_stream_deltas_without_duplication() {
        let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_456\"}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Chunk 1\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" Chunk 2\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_stop
data: {\"type\":\"message_stop\"}
";

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(16);
        let stream = ReaderStream::new(std::io::Cursor::new(body));
        tokio::spawn(process_anthropic_sse(
            stream,
            tx,
            Duration::from_millis(10_000),
            otel(),
        ));

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }

        assert!(matches!(events[0], ResponseEvent::Created));
        match &events[1] {
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { summary, .. }) => {
                assert!(
                    summary.is_empty(),
                    "OutputItemAdded should have empty summary"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(
            events[2],
            ResponseEvent::ReasoningSummaryDelta { ref delta, summary_index: 0 } if delta == "Chunk 1"
        ));
        assert!(matches!(
            events[3],
            ResponseEvent::ReasoningSummaryDelta { ref delta, summary_index: 1 } if delta == " Chunk 2"
        ));

        match &events[4] {
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { summary, .. }) => {
                assert_eq!(summary.len(), 1, "Expected exactly one summary entry");
                match &summary[0] {
                    ReasoningItemReasoningSummary::SummaryText { text } => {
                        assert_eq!(
                            text, "Chunk 1 Chunk 2",
                            "OutputItemDone should have accumulated thinking summary"
                        );
                    }
                }
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(events[5], ResponseEvent::Completed { .. }));
    }

    #[test]
    fn build_messages_handles_tool_use() {
        let prompt = Prompt {
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "hello".to_string(),
                    }],
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    arguments: "{\"command\":[\"ls\"],\"workdir\":null}".to_string(),
                    call_id: "toolu_1".to_string(),
                },
            ],
            ..Default::default()
        };

        let (messages, extra) = build_anthropic_messages(&prompt).unwrap();
        assert!(extra.is_none());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        let tool_content = messages[1].content.last().unwrap();
        let tool_type = tool_content.get("type").and_then(|v| v.as_str()).unwrap();
        assert_eq!(tool_type, "tool_use");
    }

    #[test]
    fn assistant_text_and_tool_use_merge_into_single_message() {
        let prompt = Prompt {
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "Hi".to_string(),
                        signature: None,
                    }],
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    arguments: "{}".to_string(),
                    call_id: "toolu_1".to_string(),
                },
            ],
            ..Default::default()
        };

        let (messages, _) = build_anthropic_messages(&prompt).unwrap();
        assert_eq!(messages.len(), 1);
        let assistant = &messages[0];
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.content.len(), 2);
        assert_eq!(
            assistant.content[0].get("type").and_then(|v| v.as_str()),
            Some("text")
        );
        assert_eq!(
            assistant.content[1].get("type").and_then(|v| v.as_str()),
            Some("tool_use")
        );
    }

    #[test]
    fn resolve_thinking_budget_works() {
        // Test heuristic (80%)
        let max = 10_000;
        let budget = resolve_thinking_budget(None, max);
        assert_eq!(budget, 8_000);
        assert!(budget < max);

        // Test Low (4k)
        let budget = resolve_thinking_budget(Some(ReasoningEffort::Low), 20_000);
        assert_eq!(budget, 4_096);

        // Test High (32k)
        let budget = resolve_thinking_budget(Some(ReasoningEffort::High), 64_000);
        assert_eq!(budget, 32_768);

        // Test Clamping (High requested but max is small)
        let max_small = 8_000;
        let budget = resolve_thinking_budget(Some(ReasoningEffort::High), max_small);
        // Should be clamped to max - reserved.
        // Reserved = 8000/5 = 1600. 8000-1600 = 6400.
        assert_eq!(budget, 6_400);
        assert!(budget < max_small);
    }
}
