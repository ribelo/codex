#![allow(clippy::too_many_arguments)]
#![allow(dead_code)]

use std::env;
use std::time::Duration;

use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::config_types::ReasoningDisplay;
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
use codex_client::CodexHttpClient;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Gemini API Structures
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_config: Option<GeminiGenerationConfig>,
    /// Session ID for Antigravity requests
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    /// Safety settings (mainly for Antigravity to disable content filtering)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) safety_settings: Option<Vec<SafetySetting>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SafetySetting {
    pub category: String,
    pub threshold: String,
}

#[derive(Serialize)]
pub(crate) struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
    /// Indicates this is a thinking/reasoning part (for Claude on Antigravity)
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiFunctionCall {
    name: String,
    args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiFunctionResponse {
    name: String,
    response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

const SYNTHETIC_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiFunctionCallingConfig {
    mode: String, // "AUTO", "ANY", "NONE"
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiGenerationConfig {
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
    pub(crate) thinking_config: Option<GeminiThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_schema: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_level: Option<String>,
    /// Whether to include reasoning thoughts in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    include_thoughts: Option<bool>,
}

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64_000;

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

#[derive(Clone, Debug)]
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
        .or(model_family.default_reasoning_effort);

    if is_gemini3(model) {
        // Gemini 3: default to "high" if no effort is specified
        let level = effort
            .and_then(reasoning_effort_to_thinking_level)
            .unwrap_or_else(|| "high".to_string());
        return Some(GeminiThinkingConfig {
            thinking_budget: None,
            thinking_level: Some(level),
            include_thoughts: Some(true),
        });
    }

    // For Gemini 2.5 and Claude models, use thinkingBudget
    let budget = reasoning_effort_to_thinking_budget(effort, model);
    Some(GeminiThinkingConfig {
        thinking_budget: Some(budget),
        thinking_level: None,
        include_thoughts: Some(true),
    })
}

/// Map reasoning effort to thinking budget tokens.
/// Values are based on LLM-API-Key-Proxy's antigravity_provider.py.
/// Returns -1 for "auto" when no effort is specified.
fn reasoning_effort_to_thinking_budget(effort: Option<ReasoningEffort>, model: &str) -> i64 {
    let Some(effort) = effort else {
        // No effort specified: use -1 for auto (model decides)
        return -1;
    };

    // Model-specific budgets (matching LLM-API-Key-Proxy)
    let is_claude = model.starts_with("claude");
    let is_gemini_25_pro = model.contains("gemini-2.5-pro");
    let is_gemini_25_flash = model.contains("gemini-2.5-flash");

    let (low, medium, high) = if is_gemini_25_pro || is_claude {
        // Gemini 2.5 Pro and Claude: higher budgets
        (8192, 16384, 32768)
    } else if is_gemini_25_flash {
        // Gemini 2.5 Flash: medium budgets
        (6144, 12288, 24576)
    } else {
        // Default/fallback
        (1024, 2048, 4096)
    };

    match effort {
        ReasoningEffort::None | ReasoningEffort::Minimal => 0,
        ReasoningEffort::Low => low,
        ReasoningEffort::Medium => medium,
        ReasoningEffort::High | ReasoningEffort::XHigh => high,
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

pub(crate) fn build_gemini_payload(
    prompt: &Prompt,
    config: &Config,
    model_family: &ModelFamily,
) -> Result<(GeminiRequest, String)> {
    let normalized_model = normalize_model_name(model_family.get_model_slug());
    let full_instructions = prompt.get_full_instructions(model_family);
    let (contents, system_instruction) = build_gemini_messages(prompt, full_instructions.as_ref())?;
    let tools = build_gemini_tools(&prompt.tools)?;

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

    let (response_mime_type, response_schema) = if let Some(schema) = &prompt.output_schema {
        (Some("application/json".to_string()), Some(schema.clone()))
    } else {
        (None, None)
    };

    let generation_config = GeminiGenerationConfig {
        candidate_count: Some(1),
        max_output_tokens,
        temperature: None,
        top_p: None, // Config doesn't expose top_p yet
        top_k: None,
        stop_sequences: None,
        thinking_config: gemini_thinking_config(config, model_family, &normalized_model),
        response_mime_type,
        response_schema,
    };

    let payload = GeminiRequest {
        contents,
        tools,
        tool_config,
        system_instruction,
        generation_config: Some(generation_config),
        session_id: None,
        safety_settings: None,
    };

    Ok((payload, normalized_model))
}

pub(crate) async fn stream_gemini_messages(
    prompt: &Prompt,
    config: &Config,
    model_family: &ModelFamily,
    client: &CodexHttpClient,
    provider: &GeminiProvider,
    otel_event_manager: &OtelEventManager,
    auth: Option<CodexAuth>,
) -> Result<ResponseStream> {
    let (payload, normalized_model) = build_gemini_payload(prompt, config, model_family)?;
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
                    config.reasoning_display,
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
    let hard_cap = DEFAULT_MAX_OUTPUT_TOKENS;
    let window = effective_context_window(config, model_family);
    let remaining = window.saturating_sub(prompt_tokens).max(1);
    Some(remaining.min(hard_cap))
}

fn effective_context_window(config: &Config, model_family: &ModelFamily) -> i64 {
    config
        .model_context_window
        .unwrap_or(model_family.context_window)
        .saturating_mul(model_family.effective_context_window_percent)
        / 100
}

async fn resolve_gemini_credential_for_account(
    auth: &CodexAuth,
    _provider: &GeminiProvider,
    account_index: usize,
) -> Result<GeminiCredential> {
    let (tokens, project_id) = auth
        .gemini_oauth_context_for_account(account_index)
        .await
        .map_err(|err| CodexErr::UnsupportedOperation(format!("{err}")))?;
    Ok(GeminiCredential::OAuth {
        access_token: tokens.access_token,
        project_id,
    })
}

async fn resolve_gemini_credential(
    auth: Option<&CodexAuth>,
    provider: &GeminiProvider,
) -> Result<GeminiCredential> {
    if let Some(token) = &provider.experimental_bearer_token {
        return Ok(GeminiCredential::ProviderBearer(token.clone()));
    }

    let auth_with_gemini =
        auth.and_then(|auth_ref| (auth_ref.gemini_account_count() > 0).then_some(auth_ref));

    // Prefer OAuth over API key when available
    if let Some(auth_ref) = auth_with_gemini {
        return resolve_gemini_credential_for_account(auth_ref, provider, 0).await;
    }

    // Fall back to API key if no OAuth
    match provider.api_key() {
        Ok(Some(key)) => Ok(GeminiCredential::ApiKey(key)),
        Ok(None) => Err(CodexErr::UnsupportedOperation(GEMINI_AUTH_HINT.to_string())),
        Err(err) => Err(err),
    }
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

pub(crate) fn normalize_model_name(model: &str) -> String {
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
                        id: Some(call_id.clone()),
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
                        id: Some(output_call_id.clone()),
                    }),
                    ..Default::default()
                };

                // Gemini API expects function responses in a "user" role content
                if let Some(last) = messages.last_mut()
                    && last.role == "user"
                    && last.parts.iter().all(|p| p.function_response.is_some())
                {
                    last.parts.push(part);
                    continue;
                }

                messages.push(GeminiContent {
                    role: "user".to_string(),
                    parts: vec![part],
                });
            }

            ResponseItem::CustomToolCall { .. } | ResponseItem::CustomToolCallOutput { .. } => {
                // Handle similarly if needed, or ignore.
                // Local shell is often treated as a tool.
            }
            ResponseItem::Reasoning {
                summary, content, ..
            } => {
                // Build thinking text from content or summary
                // Check content first, but fall back to summary if content is empty
                let thinking_text = if let Some(content_items) = content
                    && !content_items.is_empty()
                {
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

                if !thinking_text.is_empty() {
                    let part = GeminiPart {
                        text: Some(thinking_text),
                        thought: Some(true),
                        ..Default::default()
                    };

                    // Thinking parts belong to the model role
                    if let Some(last) = messages.last_mut()
                        && last.role == "model"
                    {
                        // Insert at the beginning of the model's parts
                        last.parts.insert(0, part);
                    } else {
                        messages.push(GeminiContent {
                            role: "model".to_string(),
                            parts: vec![part],
                        });
                    }
                }
            }
            _ => {} // Ignore other ResponseItem types for now
        }
    }

    curate_gemini_history(&mut messages);
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

/// Curates message history to enforce strict User-Model-User alternation.
/// Gemini requires strictly alternating roles - this function merges
/// consecutive same-role turns and drops orphaned turns that would violate alternation.
fn curate_gemini_history(contents: &mut Vec<GeminiContent>) {
    if contents.is_empty() {
        return;
    }

    // Step 1: Merge consecutive same-role turns
    let mut merged: Vec<GeminiContent> = Vec::with_capacity(contents.len());
    for content in contents.drain(..) {
        if let Some(last) = merged.last_mut()
            && last.role == content.role
        {
            last.parts.extend(content.parts);
            continue;
        }
        merged.push(content);
    }

    // Step 2: Enforce alternation - drop turns that violate User-Model-User pattern
    // Gemini expects: user, model, user, model, ... (starting with user)
    // Also handle orphaned function responses when we drop a model turn with function calls
    let mut result: Vec<GeminiContent> = Vec::with_capacity(merged.len());
    let mut expected_role = "user"; // History should start with user
    let mut skip_orphaned_responses = false; // Track if we need to skip function responses

    for mut content in merged {
        // If we're skipping orphaned responses and this is a user turn, filter out function_response parts
        if skip_orphaned_responses && content.role == "user" {
            // Filter out orphaned function_response parts
            let cleaned_parts: Vec<_> = content
                .parts
                .into_iter()
                .filter(|p| p.function_response.is_none())
                .collect();

            if cleaned_parts.is_empty() {
                // Pure function response turn - drop entirely
                tracing::warn!(
                    "Dropping orphaned function response turn (no matching function call)"
                );
                continue;
            }

            // Has non-function-response content, keep the cleaned version
            tracing::warn!("Stripped orphaned function response parts from user turn");
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
        } else {
            // Role mismatch - dropping this turn
            tracing::warn!(
                "Dropping turn with role '{}' - expected '{}' for Gemini alternation",
                content.role,
                expected_role
            );

            // If we're dropping a model turn with function calls, we need to skip orphaned responses
            if content.role == "model" && content.parts.iter().any(|p| p.function_call.is_some()) {
                skip_orphaned_responses = true;
            }
        }
    }

    *contents = result;
}

fn apply_synthetic_thought_signatures(contents: &mut [GeminiContent]) {
    // Find the last user turn that is NOT a tool response
    // (a tool response contains function_response parts)
    let Some(start_index) = contents.iter().rposition(|content| {
        content.role == "user"
            && !content
                .parts
                .iter()
                .any(|part| part.function_response.is_some())
    }) else {
        return;
    };

    // First: Strip ALL thought_signatures from ALL function calls in history
    // (historical signatures have no semantic value and waste tokens)
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

    // Then: Apply synthetic signature ONLY to active turn function calls
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
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
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

pub(crate) async fn process_gemini_sse<S, E>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
    _reasoning_display: ReasoningDisplay,
    otel_event_manager: OtelEventManager,
) where
    S: futures::Stream<Item = std::result::Result<Bytes, E>> + Unpin + Eventsource,
    E: std::fmt::Display,
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
                // Finalize reasoning FIRST so it appears before the message in history
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
                // Then finalize assistant message
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
                    content.parts.as_ref().is_some_and(|parts| {
                        parts.iter().any(|part| is_thought_value(&part.thought))
                    })
                })
            })
        });

        if !has_structured_thoughts {
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
                    // Process parts in arrival order - Gemini sends thinking chunks first.
                    for part in parts {
                        trace!(
                            "Gemini SSE part: text={:?}, thought={:?}, thought_sig={}, func_call={}",
                            part.text
                                .as_ref()
                                .map(|t| t.chars().take(50).collect::<String>()),
                            part.thought,
                            part.thought_signature.is_some(),
                            part.function_call.is_some()
                        );
                        if let Some(ref text) = part.text {
                            if is_thought_value(&part.thought) {
                                append_reasoning_delta(
                                    &tx_event,
                                    &mut reasoning_state,
                                    &mut emitted_content,
                                    text,
                                )
                                .await;
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
    thought_signature: Option<String>,
) {
    trace!(
        "append_text_delta: text len={}, has_signature={}",
        text.len(),
        thought_signature.is_some()
    );
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
            if let Some(ContentItem::OutputText { text: existing, .. }) = content.last_mut() {
                existing.push_str(text);
            } else {
                content.push(ContentItem::OutputText {
                    text: text.to_string(),
                    signature: thought_signature,
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

/// Check if the thought field indicates this is a thinking part.
/// Gemini may send thought as boolean true, or other truthy values.
fn is_thought_value(thought: &Option<Value>) -> bool {
    match thought {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) => false,
        Some(_) => true, // treat any other non-null value as true
        None => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthDotJson;
    use crate::token_data::GeminiTokenData;
    use codex_app_server_protocol::AuthMode;
    use futures::TryStreamExt;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;
    use tokio_util::io::ReaderStream;

    fn otel() -> OtelEventManager {
        OtelEventManager::new(
            codex_protocol::ConversationId::new(),
            "gemini-test",
            "gemini-test",
            None,
            None,
            None,
            false,
            "test-agent".to_string(),
        )
    }
    #[test]
    fn test_gemini_part_wrapper_deserialization() {
        // Case 1: thought is boolean true
        let json = json!({
            "text": "thinking...",
            "thought": true
        });
        let part: GeminiPartWrapper = serde_json::from_value(json).unwrap();
        assert!(is_thought_value(&part.thought));
        assert_eq!(part.text, Some("thinking...".to_string()));

        // Case 2: thought is boolean false
        let json = json!({
            "text": "answer",
            "thought": false
        });
        let part: GeminiPartWrapper = serde_json::from_value(json).unwrap();
        assert!(!is_thought_value(&part.thought));

        // Case 3: thought is null
        let json = json!({
            "text": "answer",
            "thought": null
        });
        let part: GeminiPartWrapper = serde_json::from_value(json).unwrap();
        assert!(!is_thought_value(&part.thought));

        // Case 4: thought field missing
        let json = json!({
            "text": "answer"
        });
        let part: GeminiPartWrapper = serde_json::from_value(json).unwrap();
        assert!(!is_thought_value(&part.thought));

        // Case 5: thought is some other value (treated as true)
        let json = json!({
            "text": "thinking...",
            "thought": "some thought"
        });
        let part: GeminiPartWrapper = serde_json::from_value(json).unwrap();
        assert!(is_thought_value(&part.thought));
    }

    fn auth_with_gemini_tokens() -> CodexAuth {
        let mut auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
        auth.mode = AuthMode::Gemini;
        let auth_data = AuthDotJson {
            openai_api_key: None,
            tokens: None,
            gemini_accounts: vec![GeminiTokenData {
                access_token: "access-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                id_token: None,
                project_id: Some("project-1".to_string()),
                managed_project_id: None,
                email: None,
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            }],
            antigravity_accounts: Vec::new(),
            last_refresh: None,
        };
        auth.persist_auth(&auth_data).unwrap();
        auth
    }

    fn with_env_var(name: &str, value: &str) -> Option<String> {
        let previous = std::env::var(name).ok();
        unsafe {
            std::env::set_var(name, value);
        }
        previous
    }

    #[tokio::test]
    async fn prefers_oauth_over_api_key_when_gemini_tokens_exist() {
        // Set API key to verify OAuth takes priority
        let previous = with_env_var("GEMINI_API_KEY", "test-api-key");

        let auth = auth_with_gemini_tokens();
        let provider = GeminiProvider {
            api_key_env_var: Some("GEMINI_API_KEY".to_string()),
            ..Default::default()
        };

        let credential = resolve_gemini_credential(Some(&auth), &provider)
            .await
            .expect("resolve credential");

        assert!(
            matches!(credential, GeminiCredential::OAuth { .. }),
            "expected OAuth credential when Gemini tokens exist, got {credential:?}"
        );

        // Cleanup
        if let Some(prev) = previous {
            unsafe {
                std::env::set_var("GEMINI_API_KEY", prev);
            }
        } else {
            unsafe {
                std::env::remove_var("GEMINI_API_KEY");
            }
        }
    }

    #[tokio::test]
    async fn thinking_block_emits_reasoning_delta() {
        let body = "data: ".to_owned()
            + &serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            { "text": "I am thinking...", "thought": true }
                        ]
                    }
                }]
            })
            .to_string()
            + "\n\n"
            + "data: "
            + &serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            { "text": "Here is the answer." }
                        ]
                    }
                }]
            })
            .to_string()
            + "\n\n";

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(16);
        let stream = ReaderStream::new(std::io::Cursor::new(body)).map_err(CodexErr::Io);

        tokio::spawn(process_gemini_sse(
            stream,
            tx,
            Duration::from_millis(10_000),
            ReasoningDisplay::Raw, // show_raw_reasoning
            otel(),
        ));

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let evt = event.unwrap();
            // Filter out created/completed/itemAdded/itemDone events to simplify assertion on deltas
            if matches!(
                evt,
                ResponseEvent::ReasoningContentDelta { .. } | ResponseEvent::OutputTextDelta(_)
            ) {
                events.push(evt);
            }
        }

        // Verify we got a reasoning event first
        assert!(
            events.iter().any(|ev| {
                matches!(ev, ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "I am thinking...")
            }),
            "Expected reasoning delta 'I am thinking...', got {events:?}"
        );

        // Verify we got text event second
        assert!(
            events.iter().any(|ev| {
                matches!(ev, ResponseEvent::OutputTextDelta(text) if text == "Here is the answer.")
            }),
            "Expected text delta 'Here is the answer.', got {events:?}"
        );
    }

    #[tokio::test]
    async fn reasoning_deltas_are_streamed_and_lifecycle_item_is_empty() {
        // Stream with two reasoning chunks and a final answer; reasoning must only appear via deltas.
        let chunk1 = "data: ".to_owned()
            + &serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            { "text": "Step 1: Analyze.", "thought": true }
                        ]
                    }
                }]
            })
            .to_string()
            + "\n\n";

        let chunk2 = "data: ".to_owned()
            + &serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            { "text": "Step 2: Synthesize.", "thought": true },
                            { "text": "The sky is blue due to Rayleigh scattering." }
                        ]
                    }
                }]
            })
            .to_string()
            + "\n\n";

        let body = chunk1 + &chunk2;

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(16);
        let stream = ReaderStream::new(std::io::Cursor::new(body)).map_err(CodexErr::Io);

        tokio::spawn(process_gemini_sse(
            stream,
            tx,
            Duration::from_millis(10_000),
            ReasoningDisplay::Raw,
            otel(),
        ));

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }

        // Collect deltas only to verify order and indexing.
        let deltas: Vec<_> = events
            .iter()
            .filter(|ev| {
                matches!(
                    ev,
                    ResponseEvent::ReasoningContentDelta { .. } | ResponseEvent::OutputTextDelta(_)
                )
            })
            .collect();

        assert_eq!(
            deltas.len(),
            3,
            "expected two reasoning deltas and one text delta: {deltas:?}"
        );
        assert!(matches!(
            deltas[0],
            ResponseEvent::ReasoningContentDelta { delta, content_index }
            if delta == "Step 1: Analyze." && *content_index == 0
        ));
        assert!(matches!(
            deltas[1],
            ResponseEvent::ReasoningContentDelta { delta, content_index }
            if delta == "Step 2: Synthesize." && *content_index == 1
        ));
        assert!(matches!(
            deltas[2],
            ResponseEvent::OutputTextDelta(text)
            if text == "The sky is blue due to Rayleigh scattering."
        ));

        // Ensure the lifecycle Reasoning item remained empty (content not duplicated).
        let final_reasoning_content = events.iter().find_map(|ev| {
            if let ResponseEvent::OutputItemDone(ResponseItem::Reasoning { content, .. }) = ev {
                Some(content.clone())
            } else {
                None
            }
        });

        match final_reasoning_content {
            Some(Some(content)) => assert!(
                content.is_empty(),
                "expected empty reasoning content in lifecycle item to avoid duplication"
            ),
            Some(None) => {} // also acceptable
            None => panic!("expected OutputItemDone(Reasoning)"),
        }
    }

    #[tokio::test]
    async fn single_chunk_mixed_thought_and_text_preserves_order_no_duplication() {
        // Single chunk with a thought part followed by a text part. A legacy "thoughts"
        // field is also present; when structured thoughts exist we should ignore it to
        // avoid duplication and preserve ordering.
        let body = "data: ".to_owned()
            + &serde_json::json!({
                "thoughts": [{ "text": "Step 1: Analyze the request." }],
                "candidates": [{
                    "content": {
                        "parts": [
                            { "text": "Step 1: Analyze the request.", "thought": true },
                            { "text": "The answer is 42." }
                        ]
                    }
                }]
            })
            .to_string()
            + "\n\n";

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(16);
        let stream = ReaderStream::new(std::io::Cursor::new(body)).map_err(CodexErr::Io);

        tokio::spawn(process_gemini_sse(
            stream,
            tx,
            Duration::from_millis(10_000),
            ReasoningDisplay::Raw,
            otel(),
        ));

        let mut deltas = Vec::new();
        while let Some(event) = rx.recv().await {
            let evt = event.unwrap();
            if matches!(
                evt,
                ResponseEvent::ReasoningContentDelta { .. } | ResponseEvent::OutputTextDelta(_)
            ) {
                deltas.push(evt);
            }
        }

        assert_eq!(
            deltas.len(),
            2,
            "expected exactly two deltas without duplication: {deltas:?}"
        );

        if let [first, second] = &deltas[..] {
            assert!(
                matches!(
                    first,
                    ResponseEvent::ReasoningContentDelta { delta, .. }
                    if delta == "Step 1: Analyze the request."
                ),
                "expected first delta to be reasoning: {first:?}"
            );
            assert!(
                matches!(
                    second,
                    ResponseEvent::OutputTextDelta(text) if text == "The answer is 42."
                ),
                "expected second delta to be text: {second:?}"
            );
        }
    }
}
