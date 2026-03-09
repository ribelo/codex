use std::time::Duration;

use crate::antigravity::ANTIGRAVITY_ENDPOINT;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::provider_adapters::AdapterContext;
use crate::provider_adapters::gemini::GeminiRequest;
use crate::provider_adapters::gemini::SafetySetting;
use crate::provider_adapters::gemini::build_payload;
use crate::provider_adapters::gemini::normalize_model_name;
use crate::provider_adapters::gemini::prepend_system_instruction_part;
use crate::provider_adapters::gemini::process_gemini_sse;
use crate::provider_adapters::gemini::set_tool_config_validated_mode;
use crate::util::backoff;
use codex_otel::SessionTelemetry;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use rand::Rng;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

const ANTIGRAVITY_AUTH_HINT: &str =
    "Antigravity requires OAuth login. Run `codex login antigravity`.";
const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.**Absolute paths only****Proactiveness**";
const ANTHROPIC_INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityRequest<'a> {
    project: &'a str,
    user_agent: &'a str,
    request_type: &'a str,
    request_id: String,
    model: &'a str,
    request: &'a GeminiRequest,
}

pub(crate) async fn stream_antigravity_generate_content(
    ctx: &AdapterContext<'_>,
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    session_telemetry: &SessionTelemetry,
) -> Result<ResponseStream> {
    let Some(auth_manager) = ctx.auth_manager else {
        return Err(CodexErr::UnsupportedOperation(
            ANTIGRAVITY_AUTH_HINT.to_string(),
        ));
    };

    let (mut payload, normalized_model) = build_payload(prompt, model_info, effort)?;
    let normalized_model = normalize_model_name(&normalized_model);
    prepend_system_instruction_part(&mut payload, ANTIGRAVITY_SYSTEM_INSTRUCTION);
    payload.session_id = Some(generate_session_id());

    let antigravity_model = map_antigravity_model(&normalized_model);
    let is_claude = antigravity_model.to_ascii_lowercase().contains("claude");
    if is_claude {
        payload.safety_settings = None;
        set_tool_config_validated_mode(&mut payload);
    } else {
        payload.safety_settings = Some(default_safety_settings());
    }

    let mut base_urls = if let Some(custom) = &ctx.provider.base_url {
        vec![custom.trim_end_matches('/').to_string()]
    } else {
        vec![
            ANTIGRAVITY_ENDPOINT.trim_end_matches('/').to_string(),
            "https://autopush-cloudcode-pa.sandbox.googleapis.com".to_string(),
            "https://cloudcode-pa.googleapis.com".to_string(),
        ]
    };
    base_urls.dedup();

    let mut attempt = 0_u64;
    let mut base_idx = 0_usize;
    let max_retries = ctx.provider.request_max_retries();
    loop {
        attempt += 1;
        let base_url = &base_urls[base_idx];
        let (tokens, project_id) = auth_manager
            .antigravity_oauth_context_for_account(0)
            .await
            .map_err(|err| CodexErr::UnsupportedOperation(err.to_string()))?;
        let request = AntigravityRequest {
            project: &project_id,
            user_agent: "antigravity/1.11.9",
            request_type: "agent",
            request_id: format!("agent-{}", Uuid::new_v4()),
            model: &antigravity_model,
            request: &payload,
        };
        let mut request_body = serde_json::to_value(&request)?;
        if is_claude {
            rewrite_thinking_config_for_claude(&mut request_body);
        }

        let url = format!("{base_url}/v1internal:streamGenerateContent?alt=sse");
        let host = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(base_url);

        let mut request_builder = ctx
            .http_client
            .post(&url)
            .bearer_auth(tokens.access_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::HOST, host)
            .header(reqwest::header::USER_AGENT, request.user_agent)
            .json(&request_body);
        if is_claude_thinking_model(&antigravity_model) {
            request_builder =
                request_builder.header("anthropic-beta", ANTHROPIC_INTERLEAVED_THINKING_BETA);
        }

        let started_at = std::time::Instant::now();
        let response = request_builder.send().await;
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
                if matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN)
                    && base_idx + 1 < base_urls.len()
                {
                    base_idx += 1;
                    continue;
                }

                let is_retryable = status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::CONFLICT
                    || status.is_server_error();
                let retry_after_ms = response
                    .headers()
                    .get("retry-after-ms")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                let retry_after_secs = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                let body = response.text().await.unwrap_or_default();
                if !is_retryable || attempt > max_retries {
                    return Err(CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body,
                        url: None,
                        cf_ray: None,
                        request_id: None,
                    }));
                }
                tokio::time::sleep(
                    retry_after_ms
                        .map(Duration::from_millis)
                        .or_else(|| retry_after_secs.map(Duration::from_secs))
                        .unwrap_or_else(|| backoff(attempt)),
                )
                .await;
            }
            Err(source) => {
                if base_idx + 1 < base_urls.len() {
                    base_idx += 1;
                    continue;
                }
                if attempt > max_retries {
                    return Err(CodexErr::ConnectionFailed(ConnectionFailedError { source }));
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

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

fn map_antigravity_model(model: &str) -> String {
    if model == "gemini-3-pro-preview" || model.starts_with("gemini-3-pro") {
        return "gemini-3-pro-high".to_string();
    }
    model.to_string()
}

fn generate_session_id() -> String {
    let n: u64 = rand::rng().random_range(1_000_000_000_000_000_000..=9_999_999_999_999_999_999);
    format!("-{n}")
}

fn is_claude_thinking_model(model: &str) -> bool {
    let model_lower = model.to_ascii_lowercase();
    model_lower.contains("claude") && model_lower.contains("thinking")
}

fn rewrite_thinking_config_for_claude(body: &mut Value) {
    let Some(obj) = body
        .pointer_mut("/request/generationConfig/thinkingConfig")
        .and_then(Value::as_object_mut)
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
