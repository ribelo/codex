use std::time::Duration;

use codex_otel::otel_event_manager::OtelEventManager;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::to_value;
use tokio::sync::mpsc;
use tracing::info;
use tracing::trace;
use uuid::Uuid;

use crate::antigravity::ANTIGRAVITY_ENDPOINT;
use crate::auth::CodexAuth;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::config::Config;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::gemini_messages::GeminiRequest;
use crate::gemini_messages::SafetySetting;
use crate::gemini_messages::build_gemini_payload;
use crate::gemini_messages::process_gemini_sse;
use crate::model_provider_info::GeminiProvider;
use crate::openai_models::model_family::ModelFamily;
use crate::util::backoff;
use crate::util::try_parse_error_message;
use codex_client::CodexHttpClient;
use rand::Rng;

const ANTIGRAVITY_AUTH_HINT: &str =
    "Antigravity requires a valid OAuth login. Run `codex login --antigravity`.";

const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.**Absolute paths only****Proactiveness**";

/// Anthropic beta header for interleaved thinking (real-time thinking token streaming)
const ANTHROPIC_INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Check if a model is a Claude thinking model (requires interleaved thinking header)
fn is_claude_thinking_model(model: &str) -> bool {
    let model_lower = model.to_lowercase();
    model_lower.contains("claude") && model_lower.contains("thinking")
}

/// Rewrite thinking config keys from camelCase to snake_case for Claude on Antigravity.
/// Claude expects `thinking_budget` and `include_thoughts` instead of `thinkingBudget` and `includeThoughts`.
fn rewrite_thinking_config_for_claude(body: &mut serde_json::Value) {
    let Some(obj) = body
        .pointer_mut("/request/generationConfig/thinkingConfig")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };

    if let Some(value) = obj.remove("thinkingBudget") {
        obj.insert("thinking_budget".to_string(), value);
    }
    if let Some(value) = obj.remove("includeThoughts") {
        obj.insert("include_thoughts".to_string(), value);
    }
}

/// Maps model names to their Antigravity-internal variants.
/// For Gemini models: gemini-3-pro-preview -> gemini-3-pro-low or gemini-3-pro-high
/// Claude models should be specified directly in config (e.g., claude-opus-4-5-thinking)
fn map_antigravity_model(model: &str, request: &GeminiRequest) -> String {
    // Map Gemini 3 Pro to -low/-high variant based on thinking level
    if model == "gemini-3-pro-preview" || model.starts_with("gemini-3-pro") {
        let thinking_level = request
            .generation_config
            .as_ref()
            .and_then(|gc| gc.thinking_config.as_ref())
            .and_then(|tc| tc.thinking_level.as_ref())
            .map(std::string::String::as_str)
            .unwrap_or("high");

        if thinking_level == "low" {
            return "gemini-3-pro-low".to_string();
        }
        return "gemini-3-pro-high".to_string();
    }

    model.to_string()
}

/// Generate Antigravity session ID: -{random_number}
fn generate_session_id() -> String {
    let mut rng = rand::rng();
    let n: u64 = rng.random_range(1_000_000_000_000_000_000..=9_999_999_999_999_999_999);
    format!("-{n}")
}

/// Default safety settings to prevent content filtering (per reference implementation)
fn default_safety_settings() -> Vec<SafetySetting> {
    vec![
        SafetySetting {
            category: "HARM_CATEGORY_HARASSMENT".to_string(),
            threshold: "OFF".to_string(),
        },
        SafetySetting {
            category: "HARM_CATEGORY_HATE_SPEECH".to_string(),
            threshold: "OFF".to_string(),
        },
        SafetySetting {
            category: "HARM_CATEGORY_SEXUALLY_EXPLICIT".to_string(),
            threshold: "OFF".to_string(),
        },
        SafetySetting {
            category: "HARM_CATEGORY_DANGEROUS_CONTENT".to_string(),
            threshold: "OFF".to_string(),
        },
        SafetySetting {
            category: "HARM_CATEGORY_CIVIC_INTEGRITY".to_string(),
            threshold: "BLOCK_NONE".to_string(),
        },
    ]
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityRequest {
    project: String,
    user_agent: String,
    request_type: String,
    request_id: String,
    model: String,
    request: GeminiRequest,
}

pub(crate) async fn stream_antigravity_messages(
    prompt: &Prompt,
    config: &Config,
    model_family: &ModelFamily,
    client: &CodexHttpClient,
    provider: &GeminiProvider,
    otel_event_manager: &OtelEventManager,
    auth: Option<CodexAuth>,
) -> Result<ResponseStream> {
    let (mut payload, normalized_model) = build_gemini_payload(prompt, config, model_family)?;
    let auth =
        auth.ok_or_else(|| CodexErr::UnsupportedOperation(ANTIGRAVITY_AUTH_HINT.to_string()))?;

    crate::gemini_messages::prepend_system_instruction_part(
        &mut payload,
        ANTIGRAVITY_SYSTEM_INSTRUCTION,
    );

    // Add Antigravity-specific fields to the request (per reference implementation)
    payload.session_id = Some(generate_session_id());

    // Map model to Antigravity-internal variant (e.g., gemini-3-pro-preview -> gemini-3-pro-high)
    let antigravity_model = map_antigravity_model(&normalized_model, &payload);

    // Claude models require different handling than Gemini models
    let is_claude = antigravity_model.to_lowercase().contains("claude");
    let is_claude_thinking = is_claude_thinking_model(&antigravity_model);
    if is_claude {
        // Claude doesn't use safety settings - they cause 429 errors
        payload.safety_settings = None;
        // Claude requires VALIDATED mode for function calling
        crate::gemini_messages::set_tool_config_validated_mode(&mut payload);
    } else {
        // Gemini models use safety settings to disable content filtering
        payload.safety_settings = Some(default_safety_settings());
    }
    tracing::debug!(
        original_model = %normalized_model,
        antigravity_model = %antigravity_model,
        session_id = ?payload.session_id,
        "Mapped model for Antigravity"
    );

    let mut request_body = AntigravityRequest {
        project: String::new(),
        user_agent: "antigravity/1.11.9".to_string(),
        request_type: "agent".to_string(),
        request_id: format!("agent-{}", Uuid::new_v4()),
        model: antigravity_model,
        request: payload,
    };

    let payload_json = to_value(&request_body.request)?;
    let mut attempt = 0_u64;
    let max_retries = provider.request_max_retries();
    // Order matches Python proxy: sandbox first, then autopush, then production
    let mut base_urls: Vec<String> = if let Some(custom) = &provider.base_url {
        vec![custom.trim_end_matches('/').to_string()]
    } else {
        vec![
            ANTIGRAVITY_ENDPOINT.trim_end_matches('/').to_string(), // daily-cloudcode-pa.sandbox
            "https://autopush-cloudcode-pa.sandbox.googleapis.com".to_string(),
            "https://cloudcode-pa.googleapis.com".to_string(), // production fallback
        ]
    };
    base_urls.dedup();
    let mut base_idx = 0_usize;

    loop {
        attempt += 1;
        let base_url = &base_urls[base_idx];
        let url = format!("{base_url}/v1internal:streamGenerateContent?alt=sse");
        let (tokens, project_id) = auth
            .antigravity_oauth_context_for_account(0)
            .await
            .map_err(|err| CodexErr::UnsupportedOperation(format!("{err}")))?;
        request_body.project = project_id;

        if attempt == 1 {
            info!(
                provider = %provider.name,
                url = %url,
                "Dispatching Antigravity request"
            );
            trace!("POST to {}: {}", url, payload_json);
        }

        // Extract host from base_url for Host header (per Python reference)
        let host = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(base_url);

        // Serialize request body to JSON, then apply Claude-specific transformations
        let mut request_body_json = to_value(&request_body)?;
        if is_claude {
            rewrite_thinking_config_for_claude(&mut request_body_json);
        }

        let req_builder = client
            .post(&url)
            .bearer_auth(tokens.access_token.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::HOST, host)
            .header(reqwest::header::USER_AGENT, &request_body.user_agent)
            .json(&request_body_json);

        // Add interleaved thinking header for Claude thinking models (per reference implementations)
        let req_builder = if is_claude_thinking {
            tracing::debug!(
                model = %request_body.model,
                "Adding anthropic-beta interleaved-thinking header for Claude thinking model"
            );
            req_builder.header("anthropic-beta", ANTHROPIC_INTERLEAVED_THINKING_BETA)
        } else {
            req_builder
        };

        tracing::debug!(
            url = %url,
            host = %host,
            project = %request_body.project,
            "Sending Antigravity request"
        );

        let res = otel_event_manager
            .log_request(attempt, || req_builder.send())
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(
                    status = %resp.status(),
                    "Antigravity request succeeded, starting SSE stream"
                );
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
                let stream = resp.bytes_stream();
                tokio::spawn(process_gemini_sse(
                    stream,
                    tx_event,
                    provider.stream_idle_timeout(),
                    otel_event_manager.clone(),
                ));
                return Ok(ResponseStream { rx_event });
            }
            Ok(resp) => {
                let status = resp.status();
                tracing::debug!(
                    status = %status,
                    "Antigravity request returned non-success status"
                );
                if status == StatusCode::NOT_FOUND && base_idx + 1 < base_urls.len() {
                    base_idx += 1;
                    continue;
                }
                if status == StatusCode::FORBIDDEN && base_idx + 1 < base_urls.len() {
                    base_idx += 1;
                    continue;
                }
                let is_retryable = status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::CONFLICT
                    || status.is_server_error();

                if !is_retryable {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::error!(
                        status = %status,
                        body = %body,
                        "Antigravity request failed with non-retryable error"
                    );
                    let message = try_parse_error_message(&body);
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body: message,
                        request_id: None,
                    }));
                }

                if attempt > max_retries {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        request_id: None,
                    }));
                }

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
                tracing::error!(
                    error = %e,
                    base_idx = base_idx,
                    "Antigravity request connection failed"
                );
                if base_idx + 1 < base_urls.len() {
                    base_idx += 1;
                    continue;
                }
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
