use crate::api_bridge::CoreAuthProvider;
use crate::auth::AuthManager;
use crate::error::CodexErr;
use crate::error::UnexpectedResponseError;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use codex_api::AuthProvider as _;
use codex_api::Provider as ApiProvider;
use codex_api::build_conversation_headers;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use http::HeaderMap;
use http::HeaderValue;
use reqwest::RequestBuilder;
use reqwest::StatusCode;
use std::time::Duration;
use url::Url;

pub(crate) mod anthropic;
pub(crate) mod antigravity;
pub(crate) mod bedrock;
pub(crate) mod chat;
pub(crate) mod gemini;

pub(crate) struct AdapterContext<'a> {
    pub(crate) http_client: &'a reqwest::Client,
    pub(crate) provider: &'a ModelProviderInfo,
    pub(crate) api_provider: &'a ApiProvider,
    pub(crate) api_auth: &'a CoreAuthProvider,
    pub(crate) auth_manager: Option<&'a AuthManager>,
    pub(crate) conversation_id: &'a ThreadId,
    pub(crate) session_source: &'a SessionSource,
}

impl AdapterContext<'_> {
    pub(crate) fn request_builder(&self, url: String, extra_headers: HeaderMap) -> RequestBuilder {
        let mut headers = self.api_provider.headers.clone();
        headers.extend(extra_headers);
        self.http_client.post(url).headers(headers)
    }

    pub(crate) fn request_builder_for_path(
        &self,
        path: &str,
        extra_headers: HeaderMap,
    ) -> RequestBuilder {
        self.request_builder(self.api_provider.url_for_path(path), extra_headers)
    }

    pub(crate) fn request_url_with_query(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> crate::error::Result<String> {
        let mut url = Url::parse(&self.api_provider.url_for_path(path))
            .map_err(|err| CodexErr::Fatal(format!("invalid provider URL: {err}")))?;
        for (key, value) in query {
            url.query_pairs_mut().append_pair(key, value);
        }
        Ok(url.to_string())
    }

    pub(crate) fn bearer_token(&self) -> Option<String> {
        self.api_auth.bearer_token()
    }
}

pub(crate) fn conversation_headers(
    conversation_id: &ThreadId,
    session_source: &SessionSource,
) -> HeaderMap {
    let mut headers = build_conversation_headers(Some(conversation_id.to_string()));
    if let Some(subagent) = subagent_label(session_source)
        && let Ok(value) = HeaderValue::from_str(&subagent)
    {
        headers.insert("x-openai-subagent", value);
    }
    headers
}

pub(crate) fn subagent_label(session_source: &SessionSource) -> Option<String> {
    let SessionSource::SubAgent(subagent) = session_source else {
        return None;
    };
    let label = match subagent {
        SubAgentSource::Review => "review".to_string(),
        SubAgentSource::Compact => "compact".to_string(),
        SubAgentSource::MemoryConsolidation => "memory_consolidation".to_string(),
        SubAgentSource::ThreadSpawn { .. } => "collab_spawn".to_string(),
        SubAgentSource::Other(label) => label.clone(),
    };
    Some(label)
}

pub(crate) fn responses_only_operation_error(operation: &str, wire_api: WireApi) -> CodexErr {
    CodexErr::UnsupportedOperation(format!(
        "{operation} is only supported with Responses API providers (current wire_api: {})",
        wire_api.as_str()
    ))
}

pub(crate) fn record_native_api_request(
    session_telemetry: &SessionTelemetry,
    attempt: u64,
    status: Option<u16>,
    error: Option<&str>,
    duration: Duration,
    endpoint: &'static str,
) {
    session_telemetry.record_api_request(
        attempt, status, error, duration, /*auth_header_attached*/ false,
        /*auth_header_name*/ None, /*retry_after_unauthorized*/ false,
        /*recovery_mode*/ None, /*recovery_phase*/ None, endpoint,
        /*request_id*/ None, /*cf_ray*/ None, /*auth_error*/ None,
        /*auth_error_code*/ None,
    );
}

pub(crate) fn unexpected_response_error(
    status: StatusCode,
    body: String,
) -> UnexpectedResponseError {
    UnexpectedResponseError {
        status,
        body,
        url: None,
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    }
}

pub(crate) fn max_output_tokens(model_info: &ModelInfo, hard_cap: i64) -> i64 {
    let context_window = model_info.context_window.unwrap_or(272_000);
    let effective_context_window =
        context_window.saturating_mul(model_info.effective_context_window_percent) / 100;
    effective_context_window.max(1).min(hard_cap)
}
