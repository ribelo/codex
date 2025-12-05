#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

use std::env;
use std::time::Duration;

use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::bytes::Bytes;
use tracing::Level;
use tracing::debug;
use tracing::info;
use tracing::trace;

use crate::auth::CodexAuth;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::tools::ToolSpec;
use crate::config::Config;
use crate::default_client::CodexHttpClient;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::gemini::GEMINI_CODE_ASSIST_CLIENT_METADATA;
use crate::gemini::GEMINI_CODE_ASSIST_ENDPOINT;
use crate::gemini::GEMINI_CODE_ASSIST_USER_AGENT;
use crate::gemini::GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT;
use crate::gemini::GEMINI_MODEL_FALLBACKS;
use crate::model_provider_info::GeminiProvider;
use crate::openai_models::model_family::ModelFamily;
use crate::protocol::TokenUsage;
use crate::truncate::approx_token_count;
use crate::util::backoff;
use crate::util::try_parse_error_message;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Gemini API Structures
// ─────────────────────────────────────────────────────────────────────────────

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

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Default)]
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionResponse {
    name: String,
    response: Value,
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

const SYNTHETIC_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallingConfig {
    mode: String, // "AUTO", "ANY", "NONE"
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
    // response_mime_type: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<String>,
    /// Whether to include reasoning thoughts in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    include_thoughts: Option<bool>,
}

const GEMINI_MAX_PROVIDER_OUTPUT_TOKENS: i64 = 8_192;
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 4_096;

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
    #[serde(rename = "index")]
    _index: Option<i32>,
    // safety_ratings, etc.
}

#[derive(Deserialize, Debug)]
struct GeminiResponseContent {
    parts: Option<Vec<GeminiPartWrapper>>,
    #[serde(rename = "role")]
    _role: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GeminiPartWrapper {
    text: Option<String>,
    #[serde(rename = "functionCall")]
    function_call: Option<GeminiFunctionCallResponse>,
    #[serde(rename = "thoughtSignature")]
    thought_signature: Option<String>,
    thought: Option<bool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallResponse {
    name: String,
    args: Value,
    id: Option<String>,
    #[serde(rename = "thoughtSignature")]
    _thought_signature: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    prompt_token_count: Option<i64>,
    candidates_token_count: Option<i64>,
    total_token_count: Option<i64>,
    thoughts_token_count: Option<i64>,
}

const GEMINI_AUTH_HINT: &str =
    "Gemini requests require logging in with `codex login --gemini` or configuring GEMINI_API_KEY";

#[derive(Clone)]
enum GeminiCredential {
    ApiKey(String),
    ProviderBearer(String),
    OAuth {
        access_token: String,
        project_id: String,
    },
}

enum GeminiRequestConfig {
    GenerativeLanguage {
        url: String,
        bearer: Option<String>,
    },
    CodeAssist {
        url: String,
        access_token: String,
        project_id: String,
    },
}

impl GeminiRequestConfig {
    fn url(&self) -> &str {
        match self {
            Self::GenerativeLanguage { url, .. } => url,
            Self::CodeAssist { url, .. } => url,
        }
    }
}

fn gemini_thinking_config(
    config: &Config,
    model_family: &ModelFamily,
    model: &str,
) -> Option<GeminiThinkingConfig> {
    if !model_family.supports_reasoning_summaries {
        return None;
    }

    let effort = config
        .model_reasoning_effort
        .or(model_family.default_reasoning_effort)?;

    if is_gemini3(model) {
        let level = reasoning_effort_to_thinking_level(effort)?;
        return Some(GeminiThinkingConfig {
            thinking_budget: None,
            thinking_level: Some(level),
            include_thoughts: Some(true),
        });
    }

    reasoning_effort_to_thinking_budget(effort).map(|budget| GeminiThinkingConfig {
        thinking_budget: Some(budget),
        thinking_level: None,
        include_thoughts: Some(true),
    })
}

fn reasoning_effort_to_thinking_budget(effort: ReasoningEffort) -> Option<i64> {
    match effort {
        ReasoningEffort::None => Some(0),
        ReasoningEffort::Minimal => Some(0),
        ReasoningEffort::Low => Some(512),
        ReasoningEffort::Medium => Some(2048),
        ReasoningEffort::High => Some(8192),
        ReasoningEffort::XHigh => Some(16384),
    }
}

fn reasoning_effort_to_thinking_level(effort: ReasoningEffort) -> Option<String> {
    match effort {
        ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => {
            Some("low".to_string())
        }
        ReasoningEffort::Medium | ReasoningEffort::High | ReasoningEffort::XHigh => {
            Some("high".to_string())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Implementation
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn stream_gemini_messages(
    prompt: &Prompt,
    config: &Config,
    model_family: &ModelFamily,
    client: &CodexHttpClient,
    provider: &GeminiProvider,
    otel_event_manager: &OtelEventManager,
    auth: Option<CodexAuth>,
) -> Result<ResponseStream> {
    let full_instructions = prompt.get_full_instructions(model_family);
    let (contents, system_instruction) = build_gemini_messages(prompt, full_instructions.as_ref())?;
    let tools = build_gemini_tools(&prompt.tools)?;
    let normalized_model = normalize_model_name(&config.model);

    let tool_config = if tools.is_some() {
        // Force AUTO if tools are present, effectively same as None but explicit.
        Some(GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "AUTO".to_string(),
            },
        })
    } else {
        None
    };

    let prompt_tokens = gemini_prompt_token_estimate(
        &contents,
        tools.as_ref(),
        tool_config.as_ref(),
        &system_instruction,
    );
    let max_output_tokens = resolve_max_output_tokens(config, model_family, prompt_tokens);

    let generation_config = GeminiGenerationConfig {
        candidate_count: Some(1),
        max_output_tokens,
        temperature: None,
        top_p: None, // Config doesn't expose top_p yet
        top_k: None,
        stop_sequences: None,
        thinking_config: gemini_thinking_config(config, model_family, &normalized_model),
    };

    let payload = GeminiRequest {
        contents,
        tools,
        tool_config,
        system_instruction,
        generation_config: Some(generation_config),
    };

    let payload_json = serde_json::to_value(&payload)?;
    let mut attempt = 0_u64;
    let max_retries = provider.request_max_retries();
    let mut cached_credential: Option<GeminiCredential> = None;

    loop {
        attempt += 1;
        let credential = if let Some(existing) = cached_credential.clone() {
            existing
        } else {
            let resolved = resolve_gemini_credential(auth.as_ref(), provider).await?;
            cached_credential = Some(resolved.clone());
            resolved
        };

        let request_config = build_gemini_request_config(provider, &normalized_model, &credential)?;
        if attempt == 1 {
            let request_url = request_config.url().to_string();
            info!(
                provider = %provider.name,
                url = %request_url,
                "Dispatching Gemini Messages request"
            );
            trace!("POST to {}: {}", request_url, payload_json);
        }

        let req_builder = match &request_config {
            GeminiRequestConfig::GenerativeLanguage { url, bearer } => {
                let mut builder = client.post(url);
                builder = apply_provider_headers(builder, provider);
                if let Some(token) = bearer {
                    builder = builder.bearer_auth(token);
                }
                builder
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&payload_json)
            }
            GeminiRequestConfig::CodeAssist {
                url,
                access_token,
                project_id,
            } => {
                let wrapped = json!({
                    "project": project_id,
                    "model": normalized_model,
                    "request": payload_json.clone(),
                });
                client
                    .post(url)
                    .bearer_auth(access_token)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .header(reqwest::header::USER_AGENT, GEMINI_CODE_ASSIST_USER_AGENT)
                    .header("X-Goog-Api-Client", GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT)
                    .header("Client-Metadata", GEMINI_CODE_ASSIST_CLIENT_METADATA)
                    .json(&wrapped)
            }
        };

        let res = otel_event_manager
            .log_request(attempt, || req_builder.send())
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => {
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
                let stream = resp.bytes_stream();
                tokio::spawn(process_gemini_sse(
                    stream,
                    tx_event,
                    provider.stream_idle_timeout(),
                    config.show_raw_agent_reasoning,
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

                let make_provider_error = |body: String| {
                    if let Ok(value) = serde_json::from_str::<Value>(&body)
                        && let Some(provider_error) = gemini_error_from_value(&value)
                    {
                        let message = format_provider_error_message(&provider_error);
                        return CodexErr::ProviderError {
                            provider: "Gemini".to_string(),
                            message,
                            code: provider_error.code,
                        };
                    }
                    let message = try_parse_error_message(&body);
                    CodexErr::ProviderError {
                        provider: "Gemini".to_string(),
                        message,
                        code: Some(status.as_u16().to_string()),
                    }
                };

                if attempt > max_retries {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(make_provider_error(body));
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

fn gemini_prompt_token_estimate(
    contents: &[GeminiContent],
    tools: Option<&Vec<GeminiTool>>,
    tool_config: Option<&GeminiToolConfig>,
    system_instruction: &Option<GeminiContent>,
) -> i64 {
    let prompt_json = json!({
        "contents": contents,
        "tools": tools,
        "tool_config": tool_config,
        "system_instruction": system_instruction,
    });
    i64::try_from(approx_token_count(&prompt_json.to_string())).unwrap_or(i64::MAX)
}

fn resolve_max_output_tokens(
    config: &Config,
    model_family: &ModelFamily,
    prompt_tokens: i64,
) -> Option<i64> {
    let hard_cap = DEFAULT_MAX_OUTPUT_TOKENS.min(GEMINI_MAX_PROVIDER_OUTPUT_TOKENS);
    if let Some(window) = effective_context_window(config, model_family) {
        let remaining = window.saturating_sub(prompt_tokens).max(1);
        return Some(remaining.min(hard_cap));
    }
    Some(hard_cap)
}

fn effective_context_window(config: &Config, model_family: &ModelFamily) -> Option<i64> {
    config
        .model_context_window
        .map(|window| window.saturating_mul(model_family.effective_context_window_percent) / 100)
}

async fn resolve_gemini_credential(
    _auth: Option<&CodexAuth>,
    provider: &GeminiProvider,
) -> Result<GeminiCredential> {
    if let Some(token) = &provider.experimental_bearer_token {
        return Ok(GeminiCredential::ProviderBearer(token.clone()));
    }

    match provider.api_key() {
        Ok(Some(key)) => return Ok(GeminiCredential::ApiKey(key)),
        Ok(None) => {}
        Err(err) => return Err(err),
    }

    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Ok(GeminiCredential::ApiKey(trimmed.to_string()));
        }
    }

    Err(CodexErr::UnsupportedOperation(GEMINI_AUTH_HINT.to_string()))
}

fn build_gemini_request_config(
    provider: &GeminiProvider,
    model: &str,
    credential: &GeminiCredential,
) -> Result<GeminiRequestConfig> {
    let config = match credential {
        GeminiCredential::OAuth {
            access_token,
            project_id,
        } => {
            let base = provider
                .base_url
                .as_deref()
                .unwrap_or(GEMINI_CODE_ASSIST_ENDPOINT)
                .trim_end_matches('/');
            GeminiRequestConfig::CodeAssist {
                url: format!("{base}/v1internal:streamGenerateContent?alt=sse"),
                access_token: access_token.clone(),
                project_id: project_id.clone(),
            }
        }
        GeminiCredential::ApiKey(key) => GeminiRequestConfig::GenerativeLanguage {
            url: build_generative_url(provider, model, Some(key)),
            bearer: None,
        },
        GeminiCredential::ProviderBearer(token) => GeminiRequestConfig::GenerativeLanguage {
            url: build_generative_url(provider, model, None),
            bearer: Some(token.clone()),
        },
    };
    Ok(config)
}

fn build_generative_url(provider: &GeminiProvider, model: &str, api_key: Option<&str>) -> String {
    let base_url = provider
        .base_url
        .clone()
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
    let base_url = base_url.trim_end_matches('/');
    let mut query_parts: Vec<String> = provider
        .query_params
        .as_ref()
        .map(|params| {
            params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if let Some(key) = api_key {
        let has_key = provider
            .query_params
            .as_ref()
            .is_some_and(|params| params.contains_key("key"));
        if !has_key {
            query_parts.push(format!("key={key}"));
        }
    }

    query_parts.push("alt=sse".to_string());
    let query_string = if query_parts.is_empty() {
        String::new()
    } else {
        format!("?{}", query_parts.join("&"))
    };
    format!("{base_url}/models/{model}:streamGenerateContent{query_string}")
}

fn apply_provider_headers(
    mut builder: crate::default_client::CodexRequestBuilder,
    provider: &GeminiProvider,
) -> crate::default_client::CodexRequestBuilder {
    if let Some(extra) = &provider.http_headers {
        for (k, v) in extra {
            builder = builder.header(k, v);
        }
    }

    if let Some(env_headers) = &provider.env_http_headers {
        for (header, env_var) in env_headers {
            if let Ok(val) = std::env::var(env_var)
                && !val.trim().is_empty()
            {
                builder = builder.header(header, val);
            }
        }
    }
    builder
}

fn normalize_model_name(model: &str) -> String {
    for (pattern, fallback) in GEMINI_MODEL_FALLBACKS {
        if pattern == &model {
            return fallback.to_string();
        }
    }
    model.to_string()
}

fn is_gemini3(model: &str) -> bool {
    model.starts_with("gemini-3")
}

fn build_gemini_messages(
    prompt: &Prompt,
    full_instructions: &str,
) -> Result<(Vec<GeminiContent>, Option<GeminiContent>)> {
    let mut messages: Vec<GeminiContent> = Vec::new();
    let mut system_segments = Vec::new();
    let input = prompt.get_formatted_input();

    if !full_instructions.trim().is_empty() {
        system_segments.push(full_instructions.trim_end_matches('\n').to_string());
    }

    // Gemini requires alternating user/model turns.
    // System instructions are separate.

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

                let gemini_role = if role == "user" { "user" } else { "model" };
                let parts = map_message_content(content);

                if parts.is_empty() {
                    continue;
                }

                // Merge with previous if same role (Gemini requires strictly alternating roles?)
                // Actually, Gemini might complain if two user messages are sequential.
                // We should merge them.
                if let Some(last) = messages.last_mut()
                    && last.role == gemini_role
                {
                    last.parts.extend(parts);
                    continue;
                }

                messages.push(GeminiContent {
                    role: gemini_role.to_string(),
                    parts,
                });
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                // Function calls come from the "model".
                let args = parse_tool_arguments(arguments);

                let (thought_signature, _effective_id) = if call_id.starts_with("gemini_sig:") {
                    (
                        Some(call_id.trim_start_matches("gemini_sig:").to_string()),
                        call_id.clone(),
                    )
                } else {
                    (None, call_id.clone())
                };

                let part = GeminiPart {
                    function_call: Some(GeminiFunctionCall {
                        name: name.clone(),
                        args,
                    }),
                    thought_signature,
                    ..Default::default()
                };

                if let Some(last) = messages.last_mut()
                    && last.role == "model"
                {
                    last.parts.push(part);
                    continue;
                }

                messages.push(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![part],
                });
            }
            ResponseItem::FunctionCallOutput {
                call_id: output_call_id,
                output,
            } => {
                let func_name = input.iter().find_map(|prev| {
                    if let ResponseItem::FunctionCall {
                        name, call_id: cid, ..
                    } = prev
                        && cid == output_call_id
                    {
                        return Some(name.clone());
                    }
                    None
                });
                let func_name = func_name.unwrap_or_else(|| "unknown".to_string());

                // If we can't find it (e.g. truncated history), we might be in trouble.
                // But usually history is preserved.

                let response_content = if let Some(content) = &output.content_items {
                    // Convert content items to JSON object?
                    // Gemini expects a JSON object for `response`.
                    // If we have text/image, we should wrap it.
                    json!({ "content": content }) // Simplified
                } else {
                    // output.content is a String (JSON).
                    match serde_json::from_str::<Value>(&output.content) {
                        Ok(v) => normalize_response_value(v),
                        Err(_) => json!({ "result": output.content }),
                    }
                };

                let part = GeminiPart {
                    function_response: Some(GeminiFunctionResponse {
                        name: func_name,
                        response: response_content,
                    }),
                    ..Default::default()
                };

                if let Some(last) = messages.last_mut()
                    && last.role == "function"
                {
                    last.parts.push(part);
                    continue;
                }

                messages.push(GeminiContent {
                    role: "function".to_string(),
                    parts: vec![part],
                });
            }
            ResponseItem::LocalShellCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. } => {
                // Handle similarly if needed, or ignore.
                // Local shell is often treated as a tool.
            }
            _ => {} // Ignore other ResponseItem types for now
        }
    }

    apply_synthetic_thought_signatures(&mut messages);

    // Additional system instruction processing
    let system_instruction = if system_segments.is_empty() {
        None
    } else {
        let system_text = system_segments.join("\n\n");
        Some(GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart {
                text: Some(system_text),
                ..Default::default()
            }],
        })
    };

    Ok((messages, system_instruction))
}

fn apply_synthetic_thought_signatures(contents: &mut [GeminiContent]) {
    let Some(start_index) = contents.iter().rposition(|content| {
        content.role == "user"
            && content
                .parts
                .iter()
                .any(|part| part.text.as_ref().is_some_and(|t| !t.is_empty()))
    }) else {
        return;
    };

    for content in &mut contents[start_index..] {
        if content.role != "model" {
            continue;
        }

        if let Some(part) = content
            .parts
            .iter_mut()
            .find(|part| part.function_call.is_some())
            && part.thought_signature.is_none()
        {
            part.thought_signature = Some(SYNTHETIC_THOUGHT_SIGNATURE.to_string());
        }
    }
}

fn strip_additional_properties(v: &mut Value) {
    if let Value::Object(map) = v {
        map.remove("additionalProperties");
        for (_, value) in map {
            strip_additional_properties(value);
        }
    } else if let Value::Array(arr) = v {
        for value in arr {
            strip_additional_properties(value);
        }
    }
}

fn build_gemini_tools(tools: &[ToolSpec]) -> Result<Option<Vec<GeminiTool>>> {
    if tools.is_empty() {
        return Ok(None);
    }

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
        return Ok(None);
    }

    Ok(Some(vec![GeminiTool {
        function_declarations: declarations,
    }]))
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
            ContentItem::InputImage { image_url: _ } => {
                // Gemini handles images differently (inlineData or fileData).
                // Need to fetch or pass URL if supported.
                // Codex's `image_url` might be a data URI or a remote URL.
                // For now, we might have to skip or just send text "[Image]".
                // Implementing full image support requires parsing data URIs.
                // parts.push(GeminiPart::Text("[Image not supported yet]".to_string()));
            }
        }
    }
    parts
}

fn flatten_content(content: &[ContentItem]) -> String {
    let mut acc = String::new();
    for entry in content {
        match entry {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
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

fn normalize_response_value(v: Value) -> Value {
    match v {
        Value::Object(_) => v,
        other => json!({ "value": other }),
    }
}

fn parse_tool_arguments(raw: &str) -> Value {
    match serde_json::from_str(raw) {
        Ok(Value::Object(obj)) => Value::Object(obj),
        Ok(other) => json!({ "value": other }),
        Err(_) => json!({ "raw": raw }),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProviderError {
    message: String,
    code: Option<String>,
}

fn format_provider_error_message(provider_error: &ProviderError) -> String {
    match provider_error.code.as_deref() {
        Some(code) => format!("{} (code: {code})", provider_error.message),
        None => provider_error.message.clone(),
    }
}

fn build_empty_gemini_response_message(
    usage: Option<&TokenUsage>,
    saw_candidates: bool,
    finish_reason: Option<&String>,
) -> String {
    let mut message = if usage.is_some() && !saw_candidates {
        "Gemini returned token usage but no candidates.".to_string()
    } else {
        "Gemini returned no content.".to_string()
    };

    if let Some(reason) = finish_reason {
        message = format!("{message} finishReason={reason}.");
    }

    message
}

fn gemini_error_from_value(value: &Value) -> Option<ProviderError> {
    let error_value = value
        .get("error")
        .or_else(|| value.get("response").and_then(|resp| resp.get("error")))?;

    let (mut message, code) = match error_value {
        Value::String(text) => (text.to_string(), None),
        Value::Object(map) => {
            let message = map
                .get("message")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    map.get("status")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .unwrap_or_else(|| error_value.to_string());
            let code = map
                .get("status")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    map.get("code").map(|val| match val {
                        Value::String(text) => text.to_string(),
                        Value::Number(num) => num.to_string(),
                        other => other.to_string(),
                    })
                });

            (message, code)
        }
        other => (other.to_string(), None),
    };

    if message.is_empty() {
        message = "Gemini returned an error response".to_string();
    }

    Some(ProviderError { message, code })
}

async fn process_gemini_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    show_raw_reasoning: bool,
    otel_event_manager: OtelEventManager,
) where
    S: futures::Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Unpin + Eventsource,
{
    let mut stream = stream.eventsource();
    let response_id = String::new(); // Gemini doesn't always send ID in stream?
    let mut chunk_index: i64 = 0;
    let log_raw_sse = tracing::enabled!(target: module_path!(), Level::DEBUG);
    let mut usage: Option<TokenUsage> = None;
    let mut assistant_state: Option<AssistantState> = None;
    let mut reasoning_state: Option<ReasoningState> = None;
    let mut emitted_content = false;
    let mut saw_candidates = false;
    let mut last_finish_reason: Option<String> = None;
    let gemini_debug = env::var("GEMINI_DEBUG")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    // Send Created event
    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

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
                // Stream finished
                if let Some(state) = assistant_state.take() {
                    if !state.added {
                        // If we never added it (empty?), add it now?
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                            .await;
                    }
                    emitted_content = true;
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                        .await;
                }
                if let Some(state) = reasoning_state.take() {
                    if !state.added {
                        let _ = tx_event
                            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                            .await;
                    }
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                        .await;
                }

                if !emitted_content {
                    let message = build_empty_gemini_response_message(
                        usage.as_ref(),
                        saw_candidates,
                        last_finish_reason.as_ref(),
                    );
                    let _ = tx_event
                        .send(Err(CodexErr::ProviderError {
                            provider: "Gemini".to_string(),
                            message,
                            code: None,
                        }))
                        .await;
                    return;
                }

                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: response_id.clone(),
                        token_usage: usage.clone(),
                    }))
                    .await;
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

        let is_first_chunk = chunk_index == 0;
        chunk_index += 1;

        if sse.data.trim().is_empty() {
            continue;
        }

        if gemini_debug {
            info!("Gemini raw SSE chunk {chunk_index}: {}", sse.data);
        } else if log_raw_sse {
            debug!("Gemini raw SSE chunk {chunk_index}: {}", sse.data);
        }

        let parsed_value: Value = match serde_json::from_str(&sse.data) {
            Ok(val) => val,
            Err(err) => {
                if is_first_chunk || sse.data.contains("\"error\"") {
                    let _ = tx_event
                        .send(Err(CodexErr::ProviderError {
                            provider: "Gemini".to_string(),
                            message: format!("Gemini returned invalid SSE JSON: {err}"),
                            code: None,
                        }))
                        .await;
                    return;
                }
                debug!("Failed to parse Gemini SSE data: {err}");
                continue;
            }
        };
        if let Some(provider_error) = gemini_error_from_value(&parsed_value) {
            let message = format_provider_error_message(&provider_error);
            let _ = tx_event
                .send(Err(CodexErr::ProviderError {
                    provider: "Gemini".to_string(),
                    message,
                    code: provider_error.code,
                }))
                .await;
            return;
        }
        let effective = parsed_value
            .get("response")
            .cloned()
            .unwrap_or(parsed_value);
        let effective_reasoning = effective.clone();
        let response: GeminiResponse = match serde_json::from_value(effective) {
            Ok(val) => val,
            Err(err) => {
                if is_first_chunk {
                    let _ = tx_event
                        .send(Err(CodexErr::ProviderError {
                            provider: "Gemini".to_string(),
                            message: format!("Gemini SSE payload could not be parsed: {err}"),
                            code: None,
                        }))
                        .await;
                    return;
                }
                debug!("Failed to parse Gemini SSE data: {err}");
                continue;
            }
        };
        if log_raw_sse {
            debug!("Gemini SSE event: {:?}", response);
        }

        // Gemini stream usually sends `GenerateContentResponse` objects directly as data.

        // Extract usage if present (usually in the last chunk)
        if let Some(meta) = response.usage_metadata {
            usage = Some(TokenUsage {
                input_tokens: meta.prompt_token_count.unwrap_or(0),
                cached_input_tokens: 0,
                output_tokens: meta.candidates_token_count.unwrap_or(0),
                reasoning_output_tokens: meta.thoughts_token_count.unwrap_or(0),
                total_tokens: meta.total_token_count.unwrap_or(0),
            });
        }

        // If structured thoughts are present in this chunk, prefer them over the
        // heuristic extractor to avoid duplicate or reordered reasoning.
        let has_structured_thoughts = response.candidates.as_ref().is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate.content.as_ref().is_some_and(|content| {
                    content
                        .parts
                        .as_ref()
                        .is_some_and(|parts| parts.iter().any(|part| part.thought.unwrap_or(false)))
                })
            })
        });

        if show_raw_reasoning && !has_structured_thoughts {
            let reasoning_chunks = extract_gemini_reasoning_text(&effective_reasoning);
            for text in reasoning_chunks {
                append_reasoning_delta(
                    &tx_event,
                    &mut reasoning_state,
                    &mut emitted_content,
                    &text,
                )
                .await;
            }
        }

        // Extract candidates
        if let Some(candidates) = response.candidates {
            saw_candidates = true;
            for candidate in candidates {
                // handle finishReason
                if let Some(finish_reason) = candidate.finish_reason.clone() {
                    last_finish_reason = Some(finish_reason.clone());
                    if finish_reason != "STOP"
                        && finish_reason != "MAX_TOKENS"
                        && finish_reason != "function_call"
                    {
                        // warn or handle error
                    }
                }

                if let Some(content) = candidate.content
                    && let Some(parts) = content.parts
                {
                    for part in parts {
                        if let Some(ref text) = part.text {
                            if part.thought.unwrap_or(false) {
                                if show_raw_reasoning {
                                    append_reasoning_delta(
                                        &tx_event,
                                        &mut reasoning_state,
                                        &mut emitted_content,
                                        text,
                                    )
                                    .await;
                                } else {
                                    // Align with opencode: surface thought text when reasoning view is off.
                                    append_text_delta(
                                        &tx_event,
                                        &mut assistant_state,
                                        &mut emitted_content,
                                        text,
                                        part.thought_signature.clone(),
                                    )
                                    .await;
                                }
                            } else {
                                append_text_delta(
                                    &tx_event,
                                    &mut assistant_state,
                                    &mut emitted_content,
                                    text,
                                    part.thought_signature.clone(),
                                )
                                .await;
                            }
                        }
                        if part.text.is_none() && part.thought_signature.is_some() {
                            append_text_delta(
                                &tx_event,
                                &mut assistant_state,
                                &mut emitted_content,
                                "",
                                part.thought_signature.clone(),
                            )
                            .await;
                        }
                        if let Some(func_call) = part.function_call {
                            // Tool call
                            handle_function_call(
                                &tx_event,
                                &mut assistant_state,
                                &mut emitted_content,
                                func_call,
                                part.thought_signature,
                            )
                            .await;
                        }
                    }
                } else {
                    debug!("Gemini candidate missing content/parts");
                }
            }
        }
    }
}

async fn append_text_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
    emitted_content: &mut bool,
    text: &str,
    _signature: Option<String>,
) {
    ensure_assistant_item(assistant_state);
    if let Some(state) = assistant_state.as_mut() {
        if !state.added {
            state.added = true;
            if tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                .await
                .is_err()
            {
                return;
            }
            *emitted_content = true;
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
}

async fn append_reasoning_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    reasoning_state: &mut Option<ReasoningState>,
    emitted_content: &mut bool,
    text: &str,
) {
    if text.is_empty() {
        return;
    }

    ensure_reasoning_item(reasoning_state);
    if let Some(state) = reasoning_state.as_mut() {
        if !state.added {
            state.added = true;
            if tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                .await
                .is_err()
            {
                return;
            }
            *emitted_content = true;
        }

        // Stream only deltas; do not accumulate content in state.item to avoid duplication.
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
}

async fn handle_function_call(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
    emitted_content: &mut bool,
    func_call: GeminiFunctionCallResponse,
    thought_signature: Option<String>,
) {
    // If we were processing a text message, finish it.
    if let Some(state) = assistant_state.take() {
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(state.item)))
            .await;
    }

    let name = func_call.name;
    let args = func_call.args;

    let call_id = if let Some(sig) = thought_signature {
        format!("gemini_sig:{sig}")
    } else {
        func_call.id.unwrap_or_else(|| Uuid::new_v4().to_string())
    };

    // Gemini sends args as object, we need string for ResponseItem
    let args_str = serde_json::to_string(&args).unwrap_or_default();

    let item = ResponseItem::FunctionCall {
        id: None,
        name: name.clone(),
        arguments: args_str,
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
    let item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: Vec::new(),
    };
    let state = AssistantState { item, added: false };
    *assistant_state = Some(state);
}

fn ensure_reasoning_item(reasoning_state: &mut Option<ReasoningState>) {
    if reasoning_state.is_some() {
        return;
    }

    // Keep content empty; reasoning is delivered via streamed deltas to avoid duplication.
    let item = ResponseItem::Reasoning {
        id: String::new(),
        summary: Vec::new(),
        content: Some(Vec::new()),
        encrypted_content: None,
    };
    *reasoning_state = Some(ReasoningState {
        item,
        added: false,
        streamed_delta_count: 0,
    });
}

struct AssistantState {
    item: ResponseItem,
    added: bool,
}

struct ReasoningState {
    item: ResponseItem,
    added: bool,
    /// Tracks the number of streamed reasoning deltas to assign stable indices.
    streamed_delta_count: i64,
}

fn extract_gemini_reasoning_text(value: &Value) -> Vec<String> {
    let mut reasoning = Vec::new();
    if let Some(thoughts) = value.get("thoughts") {
        collect_reasoning_text(thoughts, &mut reasoning);
    }

    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        for candidate in candidates {
            if let Some(thoughts) = candidate.get("thoughts") {
                collect_reasoning_text(thoughts, &mut reasoning);
            }
        }
    }

    reasoning
}

fn collect_reasoning_text(node: &Value, acc: &mut Vec<String>) {
    match node {
        Value::String(text) => {
            if !text.is_empty() {
                acc.push(text.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_reasoning_text(item, acc);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                acc.push(text.to_string());
            }
            if let Some(text) = map.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                acc.push(text.to_string());
            }
            if let Some(parts) = map.get("parts").and_then(Value::as_array) {
                for part in parts {
                    collect_reasoning_text(part, acc);
                }
            }
        }
        _ => {}
    }
}
