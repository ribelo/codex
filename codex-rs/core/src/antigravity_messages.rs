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
use crate::default_client::CodexHttpClient;
use crate::error::CodexErr;
use crate::error::ConnectionFailedError;
use crate::error::Result;
use crate::error::UnexpectedResponseError;
use crate::gemini_messages::GeminiRequest;
use crate::gemini_messages::build_gemini_payload;
use crate::gemini_messages::process_gemini_sse;
use crate::model_provider_info::GeminiProvider;
use crate::openai_models::model_family::ModelFamily;
use crate::util::backoff;
use crate::util::try_parse_error_message;

const ANTIGRAVITY_AUTH_HINT: &str =
    "Antigravity requires a valid OAuth login. Run `codex login --antigravity`.";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityRequest {
    project: String,
    user_agent: String,
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
    let (payload, normalized_model) = build_gemini_payload(prompt, config, model_family)?;
    let auth =
        auth.ok_or_else(|| CodexErr::UnsupportedOperation(ANTIGRAVITY_AUTH_HINT.to_string()))?;
    let mut request_body = AntigravityRequest {
        project: String::new(),
        user_agent: "antigravity".to_string(),
        request_id: format!("agent-{}", Uuid::new_v4()),
        model: normalized_model.clone(),
        request: payload,
    };

    let payload_json = to_value(&request_body.request)?;
    let mut attempt = 0_u64;
    let max_retries = provider.request_max_retries();
    let base_url = provider
        .base_url
        .as_deref()
        .unwrap_or(ANTIGRAVITY_ENDPOINT)
        .trim_end_matches('/');
    let url = format!("{base_url}/v1internal:streamGenerateContent?alt=sse");

    loop {
        attempt += 1;
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

        let req_builder = client
            .post(&url)
            .bearer_auth(tokens.access_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::USER_AGENT, &request_body.user_agent)
            .json(&request_body);

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
                let is_retryable = status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::CONFLICT
                    || status.is_server_error();

                if !is_retryable {
                    let body = resp.text().await.unwrap_or_default();
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
