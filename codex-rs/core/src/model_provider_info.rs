//! Registry of model providers supported by Codex.
//!
//! Providers can be defined in two places:
//!   1. Built-in defaults compiled into the binary so Codex works out-of-the-box.
//!   2. User-defined entries inside `~/.codex/config.toml` under the `model_providers`
//!      key. These override or extend the defaults at runtime.

use codex_api::Provider as ApiProvider;
use codex_api::WireApi as ApiWireApi;
use codex_api::provider::RetryConfig as ApiRetryConfig;
use codex_app_server_protocol::AuthMode;
use http::HeaderMap;
use http::header::HeaderName;
use http::header::HeaderValue;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use std::collections::HashMap;
use std::env::VarError;
use std::time::Duration;

use crate::default_client::CodexRequestBuilder;
use crate::error::CodexErr;
use crate::error::EnvVarError;
use codex_client::CodexHttpClient;
const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_STREAM_MAX_RETRIES: u64 = 5;
const DEFAULT_REQUEST_MAX_RETRIES: u64 = 4;
/// Hard cap for user-configured `stream_max_retries`.
const MAX_STREAM_MAX_RETRIES: u64 = 100;
/// Hard cap for user-configured `request_max_retries`.
const MAX_REQUEST_MAX_RETRIES: u64 = 100;

/// Wire protocol that the provider speaks. Most third-party services only
/// implement the classic OpenAI Chat Completions JSON schema, whereas OpenAI
/// itself (and a handful of others) additionally expose the more modern
/// *Responses* API. The two protocols use different request/response shapes
/// and *cannot* be auto-detected at runtime, therefore each provider entry
/// must declare which one it expects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    /// The Responses API exposed by OpenAI at `/v1/responses`.
    Responses,

    /// Regular Chat Completions compatible with `/v1/chat/completions`.
    #[default]
    Chat,

    /// Anthropic Messages API at `/v1/messages`.
    Anthropic,

    /// Google Gemini API.
    Gemini,

    /// Google Antigravity API.
    Antigravity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
    Antigravity,
    OpenRouter,
}

/// Serializable representation of a provider definition.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelProviderInfo {
    /// Friendly display name.
    pub name: String,
    /// Base URL for the provider's OpenAI-compatible API.
    pub base_url: Option<String>,
    /// Environment variable that stores the user's API key for this provider.
    pub env_key: Option<String>,

    /// Optional instructions to help the user get a valid value for the
    /// variable and set it.
    pub env_key_instructions: Option<String>,

    /// Value to use with `Authorization: Bearer <token>` header. Use of this
    /// config is discouraged in favor of `env_key` for security reasons, but
    /// this may be necessary when using this programmatically.
    pub experimental_bearer_token: Option<String>,

    /// Which wire protocol this provider expects.
    #[serde(default)]
    pub wire_api: WireApi,
    /// Kind of provider this entry represents.
    #[serde(default, alias = "type")]
    pub provider_kind: ProviderKind,

    /// Optional query parameters to append to the base URL.
    pub query_params: Option<HashMap<String, String>>,

    /// Additional HTTP headers to include in requests to this provider where
    /// the (key, value) pairs are the header name and value.
    pub http_headers: Option<HashMap<String, String>>,

    /// Optional HTTP headers to include in requests to this provider where the
    /// (key, value) pairs are the header name and _environment variable_ whose
    /// value should be used. If the environment variable is not set, or the
    /// value is empty, the header will not be included in the request.
    pub env_http_headers: Option<HashMap<String, String>>,

    /// Maximum number of times to retry a failed HTTP request to this provider.
    pub request_max_retries: Option<u64>,

    /// Number of times to retry reconnecting a dropped streaming response before failing.
    pub stream_max_retries: Option<u64>,

    /// Idle timeout (in milliseconds) to wait for activity on a streaming response before treating
    /// the connection as lost.
    pub stream_idle_timeout_ms: Option<u64>,

    /// Does this provider require an OpenAI API Key or ChatGPT login token? If true,
    /// user is presented with login screen on first run, and login preference and token/key
    /// are stored in auth.json. If false (which is the default), login screen is skipped,
    /// and API key (if needed) comes from the "env_key" environment variable.
    #[serde(default)]
    pub requires_openai_auth: bool,

    /// Optional API key env var override for non-OpenAI providers.
    pub api_key_env_var: Option<String>,
    /// Optional API version override (used by Anthropic and Gemini).
    pub version: Option<String>,
    /// Optional beta flags (Anthropic).
    pub beta: Option<Vec<String>>,
    /// Optional project id (Gemini).
    pub project_id: Option<String>,
    /// When true, send the API key as `Authorization: Bearer <key>` instead of provider-specific header.
    /// This is needed for some Anthropic-compatible APIs like Kimi.
    #[serde(default)]
    pub use_bearer_auth: bool,
}

#[derive(Deserialize)]
struct ModelProviderInfoSerde {
    name: String,
    base_url: Option<String>,
    env_key: Option<String>,
    env_key_instructions: Option<String>,
    experimental_bearer_token: Option<String>,
    #[serde(default)]
    wire_api: Option<WireApi>,
    #[serde(default, alias = "type")]
    provider_kind: Option<ProviderKind>,
    query_params: Option<HashMap<String, String>>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    request_max_retries: Option<u64>,
    stream_max_retries: Option<u64>,
    stream_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    requires_openai_auth: Option<bool>,
    api_key_env_var: Option<String>,
    version: Option<String>,
    beta: Option<Vec<String>>,
    project_id: Option<String>,
    #[serde(default)]
    use_bearer_auth: Option<bool>,
}

impl<'de> Deserialize<'de> for ModelProviderInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ModelProviderInfoSerde::deserialize(deserializer)?;
        let provider_kind = raw.provider_kind.unwrap_or_default();
        let wire_api = raw.wire_api.unwrap_or(match provider_kind {
            ProviderKind::Anthropic => WireApi::Anthropic,
            ProviderKind::Gemini => WireApi::Gemini,
            ProviderKind::Antigravity => WireApi::Antigravity,
            ProviderKind::OpenAi => WireApi::Chat,
            ProviderKind::OpenRouter => WireApi::Responses,
        });

        Ok(Self {
            name: raw.name,
            base_url: raw.base_url,
            env_key: raw.env_key,
            env_key_instructions: raw.env_key_instructions,
            experimental_bearer_token: raw.experimental_bearer_token,
            wire_api,
            provider_kind,
            query_params: raw.query_params,
            http_headers: raw.http_headers,
            env_http_headers: raw.env_http_headers,
            request_max_retries: raw.request_max_retries,
            stream_max_retries: raw.stream_max_retries,
            stream_idle_timeout_ms: raw.stream_idle_timeout_ms,
            requires_openai_auth: raw.requires_openai_auth.unwrap_or(false),
            api_key_env_var: raw.api_key_env_var,
            version: raw.version,
            beta: raw.beta,
            project_id: raw.project_id,
            use_bearer_auth: raw.use_bearer_auth.unwrap_or(false),
        })
    }
}

impl Default for ModelProviderInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Chat,
            provider_kind: ProviderKind::OpenAi,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            api_key_env_var: None,
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct GeminiProvider {
    pub name: String,
    pub base_url: Option<String>,
    pub api_key_env_var: Option<String>,
    pub experimental_bearer_token: Option<String>,
    #[serde(default = "default_version")]
    pub version: String,
    pub query_params: Option<HashMap<String, String>>,
    pub http_headers: Option<HashMap<String, String>>,
    pub env_http_headers: Option<HashMap<String, String>>,
    pub project_id: Option<String>,
    pub request_max_retries: Option<u64>,
    pub stream_max_retries: Option<u64>,
    pub stream_idle_timeout_ms: Option<u64>,
}

fn default_version() -> String {
    "v1beta".to_string()
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self {
            name: "Gemini".to_string(),
            base_url: None,
            api_key_env_var: None,
            experimental_bearer_token: None,
            version: default_version(),
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            project_id: None,
            request_max_retries: Some(DEFAULT_REQUEST_MAX_RETRIES),
            stream_max_retries: Some(DEFAULT_STREAM_MAX_RETRIES),
            stream_idle_timeout_ms: Some(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AnthropicProvider {
    pub name: String,
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
    pub api_key_env_var: Option<String>,
    pub experimental_bearer_token: Option<String>,
    /// When true, send the API key as `Authorization: Bearer <key>` instead of `x-api-key` header.
    /// This is needed for some Anthropic-compatible APIs like Kimi.
    #[serde(default)]
    pub use_bearer_auth: bool,
    pub http_headers: Option<HashMap<String, String>>,
    pub env_http_headers: Option<HashMap<String, String>>,
    #[serde(default = "default_anthropic_version")]
    pub version: String,
    pub beta: Option<Vec<String>>,
    pub request_max_retries: Option<u64>,
    pub stream_max_retries: Option<u64>,
    pub stream_idle_timeout_ms: Option<u64>,
}

fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com/v1".to_string()
}

fn default_anthropic_version() -> String {
    "2023-06-01".to_string()
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self {
            name: "Anthropic".to_string(),
            base_url: default_anthropic_base_url(),
            api_key_env_var: None,
            experimental_bearer_token: None,
            use_bearer_auth: false,
            http_headers: None,
            env_http_headers: None,
            version: default_anthropic_version(),
            beta: None,
            request_max_retries: Some(DEFAULT_REQUEST_MAX_RETRIES),
            stream_max_retries: Some(DEFAULT_STREAM_MAX_RETRIES),
            stream_idle_timeout_ms: Some(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
        }
    }
}

impl AnthropicProvider {
    pub async fn create_request_builder<'a>(
        &'a self,
        client: &'a CodexHttpClient,
    ) -> crate::error::Result<CodexRequestBuilder> {
        let url = self.get_full_url();
        let mut builder = client.post(&url);

        if let Some(token) = &self.experimental_bearer_token {
            builder = builder.bearer_auth(token);
        } else if let Some(key) = self.api_key()? {
            if self.use_bearer_auth {
                builder = builder.bearer_auth(&key);
            } else {
                builder = builder.header("x-api-key", key);
            }
        }

        builder = builder.header("anthropic-version", &self.version);
        if let Some(betas) = &self.beta {
            builder = builder.header("anthropic-beta", betas.join(","));
        }
        if let Some(extra) = &self.http_headers {
            for (header_name, value) in extra {
                builder = builder.header(header_name.as_str(), value.as_str());
            }
        }
        if let Some(env_headers) = &self.env_http_headers {
            for (header_name, env_var) in env_headers {
                if let Ok(val) = std::env::var(env_var) {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() {
                        builder = builder.header(header_name.as_str(), trimmed);
                    }
                }
            }
        }
        Ok(builder)
    }

    pub fn api_key(&self) -> crate::error::Result<Option<String>> {
        if let Some(env_var) = &self.api_key_env_var {
            std::env::var(env_var)
                .map(|v| {
                    let trimmed = v.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .map_err(|_| {
                    crate::error::CodexErr::EnvVar(EnvVarError {
                        env_var: env_var.clone(),
                        instructions: None,
                    })
                })?
                .ok_or_else(|| {
                    crate::error::CodexErr::EnvVar(EnvVarError {
                        env_var: env_var.clone(),
                        instructions: None,
                    })
                })
                .map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn get_full_url(&self) -> String {
        format!("{}/messages", self.base_url.trim_end_matches('/'))
    }

    pub fn request_max_retries(&self) -> u64 {
        self.request_max_retries
            .unwrap_or(DEFAULT_REQUEST_MAX_RETRIES)
            .min(MAX_REQUEST_MAX_RETRIES)
    }

    pub fn stream_max_retries(&self) -> u64 {
        self.stream_max_retries
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES)
            .min(MAX_STREAM_MAX_RETRIES)
    }

    pub fn stream_idle_timeout(&self) -> Duration {
        Duration::from_millis(
            self.stream_idle_timeout_ms
                .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
        )
    }
}

#[allow(dead_code)]
impl GeminiProvider {
    pub fn api_key(&self) -> crate::error::Result<Option<String>> {
        if let Some(env_var) = &self.api_key_env_var {
            std::env::var(env_var)
                .map(|v| {
                    let trimmed = v.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .map_err(|_| {
                    crate::error::CodexErr::EnvVar(EnvVarError {
                        env_var: env_var.clone(),
                        instructions: None,
                    })
                })?
                .ok_or_else(|| {
                    crate::error::CodexErr::EnvVar(EnvVarError {
                        env_var: env_var.clone(),
                        instructions: None,
                    })
                })
                .map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn request_max_retries(&self) -> u64 {
        self.request_max_retries
            .unwrap_or(DEFAULT_REQUEST_MAX_RETRIES)
            .min(MAX_REQUEST_MAX_RETRIES)
    }

    pub fn stream_max_retries(&self) -> u64 {
        self.stream_max_retries
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES)
            .min(MAX_STREAM_MAX_RETRIES)
    }

    pub fn stream_idle_timeout(&self) -> Duration {
        Duration::from_millis(
            self.stream_idle_timeout_ms
                .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
        )
    }
}

impl ModelProviderInfo {
    fn build_header_map(&self) -> crate::error::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        if let Some(extra) = &self.http_headers {
            for (k, v) in extra {
                if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
                    headers.insert(name, value);
                }
            }
        }

        if let Some(env_headers) = &self.env_http_headers {
            for (header, env_var) in env_headers {
                if let Ok(val) = std::env::var(env_var)
                    && !val.trim().is_empty()
                    && let (Ok(name), Ok(value)) =
                        (HeaderName::try_from(header), HeaderValue::try_from(val))
                {
                    headers.insert(name, value);
                }
            }
        }

        Ok(headers)
    }

    fn provider_api_key_env_var(&self) -> Option<String> {
        self.api_key_env_var
            .clone()
            .or_else(|| self.env_key.clone())
    }

    pub fn to_gemini_provider(&self) -> crate::error::Result<GeminiProvider> {
        if !matches!(
            self.provider_kind,
            ProviderKind::Gemini | ProviderKind::Antigravity
        ) && !matches!(self.wire_api, WireApi::Gemini | WireApi::Antigravity)
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "Model provider {} is not Gemini-compatible",
                self.name
            )));
        }

        Ok(GeminiProvider {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            api_key_env_var: self.provider_api_key_env_var(),
            experimental_bearer_token: self.experimental_bearer_token.clone(),
            version: self.version.clone().unwrap_or_else(default_version),
            query_params: self.query_params.clone(),
            http_headers: self.http_headers.clone(),
            env_http_headers: self.env_http_headers.clone(),
            project_id: self.project_id.clone(),
            request_max_retries: self.request_max_retries,
            stream_max_retries: self.stream_max_retries,
            stream_idle_timeout_ms: self.stream_idle_timeout_ms,
        })
    }

    pub fn to_anthropic_provider(&self) -> crate::error::Result<AnthropicProvider> {
        if !matches!(self.provider_kind, ProviderKind::Anthropic)
            && !matches!(self.wire_api, WireApi::Anthropic)
        {
            return Err(CodexErr::UnsupportedOperation(format!(
                "Model provider {} is not Anthropic-compatible",
                self.name
            )));
        }

        Ok(AnthropicProvider {
            name: self.name.clone(),
            base_url: self
                .base_url
                .clone()
                .unwrap_or_else(default_anthropic_base_url),
            api_key_env_var: self.provider_api_key_env_var(),
            experimental_bearer_token: self.experimental_bearer_token.clone(),
            use_bearer_auth: self.use_bearer_auth,
            http_headers: self.http_headers.clone(),
            env_http_headers: self.env_http_headers.clone(),
            version: self
                .version
                .clone()
                .unwrap_or_else(default_anthropic_version),
            beta: self.beta.clone(),
            request_max_retries: self.request_max_retries,
            stream_max_retries: self.stream_max_retries,
            stream_idle_timeout_ms: self.stream_idle_timeout_ms,
        })
    }

    pub(crate) fn to_api_provider(
        &self,
        auth_mode: Option<AuthMode>,
    ) -> crate::error::Result<ApiProvider> {
        let default_base_url = if matches!(auth_mode, Some(AuthMode::ChatGPT)) {
            "https://chatgpt.com/backend-api/codex"
        } else {
            "https://api.openai.com/v1"
        };
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url.to_string());

        let headers = self.build_header_map()?;
        let retry = ApiRetryConfig {
            max_attempts: self.request_max_retries(),
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        };

        Ok(ApiProvider {
            name: self.name.clone(),
            base_url,
            query_params: self.query_params.clone(),
            wire: match self.wire_api {
                WireApi::Responses => ApiWireApi::Responses,
                WireApi::Chat => ApiWireApi::Chat,
                WireApi::Anthropic | WireApi::Gemini | WireApi::Antigravity => {
                    return Err(crate::error::CodexErr::UnsupportedOperation(
                        "codex-api Provider adaptation is not supported for this provider"
                            .to_string(),
                    ));
                }
            },
            headers,
            retry,
            stream_idle_timeout: self.stream_idle_timeout(),
        })
    }

    /// If `env_key` is Some, returns the API key for this provider if present
    /// (and non-empty) in the environment. If `env_key` is required but
    /// cannot be found, returns an error.
    pub fn api_key(&self) -> crate::error::Result<Option<String>> {
        if let Some(env_key) = self.provider_api_key_env_var() {
            let env_value = std::env::var(&env_key);
            env_value
                .and_then(|v| {
                    if v.trim().is_empty() {
                        Err(VarError::NotPresent)
                    } else {
                        Ok(Some(v))
                    }
                })
                .map_err(|_| {
                    crate::error::CodexErr::EnvVar(EnvVarError {
                        env_var: env_key.clone(),
                        instructions: self.env_key_instructions.clone(),
                    })
                })
        } else {
            Ok(None)
        }
    }

    /// Effective maximum number of request retries for this provider.
    pub fn request_max_retries(&self) -> u64 {
        self.request_max_retries
            .unwrap_or(DEFAULT_REQUEST_MAX_RETRIES)
            .min(MAX_REQUEST_MAX_RETRIES)
    }

    /// Effective maximum number of stream reconnection attempts for this provider.
    pub fn stream_max_retries(&self) -> u64 {
        self.stream_max_retries
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES)
            .min(MAX_STREAM_MAX_RETRIES)
    }

    /// Effective idle timeout for streaming responses.
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(DEFAULT_STREAM_IDLE_TIMEOUT_MS))
    }
    pub fn create_openai_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: "OpenAI".into(),
            // Allow users to override the default OpenAI endpoint by
            // exporting `OPENAI_BASE_URL`. This is useful when pointing
            // Codex at a proxy, mock server, or Azure-style deployment
            // without requiring a full TOML override for the built-in
            // OpenAI provider.
            base_url: std::env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: Some(
                [("version".to_string(), env!("CARGO_PKG_VERSION").to_string())]
                    .into_iter()
                    .collect(),
            ),
            env_http_headers: Some(
                [
                    (
                        "OpenAI-Organization".to_string(),
                        "OPENAI_ORGANIZATION".to_string(),
                    ),
                    ("OpenAI-Project".to_string(), "OPENAI_PROJECT".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            // Use global defaults for retry/timeout unless overridden in config.toml.
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: true,
            provider_kind: ProviderKind::OpenAi,
            api_key_env_var: None,
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: false,
        }
    }
}

/// Built-in default provider list.
pub fn built_in_model_providers() -> HashMap<String, ModelProviderInfo> {
    use ModelProviderInfo as P;

    // Built-in providers: OpenAI plus major third-party APIs. Users can add
    // additional providers via `model_providers` in config.toml.
    [
        ("openai", P::create_openai_provider()),
        (
            "anthropic",
            P {
                name: "Anthropic".into(),
                base_url: Some(default_anthropic_base_url()),
                env_key: Some("ANTHROPIC_API_KEY".to_string()),
                env_key_instructions: None,
                experimental_bearer_token: None,
                wire_api: WireApi::Anthropic,
                provider_kind: ProviderKind::Anthropic,
                query_params: None,
                http_headers: None,
                env_http_headers: None,
                request_max_retries: Some(DEFAULT_REQUEST_MAX_RETRIES),
                stream_max_retries: Some(DEFAULT_STREAM_MAX_RETRIES),
                stream_idle_timeout_ms: Some(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
                requires_openai_auth: false,
                api_key_env_var: None,
                version: Some(default_anthropic_version()),
                beta: None,
                project_id: None,
                use_bearer_auth: false,
            },
        ),
        (
            "gemini",
            P {
                name: "Gemini".into(),
                base_url: None,
                env_key: Some("GEMINI_API_KEY".to_string()),
                env_key_instructions: None,
                experimental_bearer_token: None,
                wire_api: WireApi::Gemini,
                provider_kind: ProviderKind::Gemini,
                query_params: None,
                http_headers: None,
                env_http_headers: None,
                request_max_retries: Some(DEFAULT_REQUEST_MAX_RETRIES),
                stream_max_retries: Some(DEFAULT_STREAM_MAX_RETRIES),
                stream_idle_timeout_ms: Some(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
                requires_openai_auth: false,
                api_key_env_var: None,
                version: Some(default_version()),
                beta: None,
                project_id: None,
                use_bearer_auth: false,
            },
        ),
        (
            "openrouter",
            P {
                name: "OpenRouter".into(),
                base_url: Some("https://openrouter.ai/api/v1".into()),
                env_key: Some("OPENROUTER_API_KEY".to_string()),
                env_key_instructions: Some("Get your API key at https://openrouter.ai/keys".into()),
                experimental_bearer_token: None,
                wire_api: WireApi::Responses,
                provider_kind: ProviderKind::OpenRouter,
                query_params: None,
                http_headers: None,
                env_http_headers: None,
                request_max_retries: Some(DEFAULT_REQUEST_MAX_RETRIES),
                stream_max_retries: Some(DEFAULT_STREAM_MAX_RETRIES),
                stream_idle_timeout_ms: Some(DEFAULT_STREAM_IDLE_TIMEOUT_MS),
                requires_openai_auth: false,
                api_key_env_var: None,
                version: None,
                beta: None,
                project_id: None,
                use_bearer_auth: true,
            },
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn built_in_providers_includes_openrouter() {
        let providers = built_in_model_providers();

        let openrouter = providers
            .get("openrouter")
            .expect("openrouter should be a built-in provider");

        assert_eq!(openrouter.name, "OpenRouter");
        assert_eq!(openrouter.provider_kind, ProviderKind::OpenRouter);
        assert_eq!(openrouter.wire_api, WireApi::Responses);
        assert_eq!(
            openrouter.base_url,
            Some("https://openrouter.ai/api/v1".to_string())
        );
        assert_eq!(openrouter.env_key, Some("OPENROUTER_API_KEY".to_string()));
        assert!(openrouter.use_bearer_auth);
    }

    #[test]
    fn deserializes_openrouter_provider() {
        let toml = r#"
    name = "OpenRouter"
    provider_kind = "openrouter"
    base_url = "https://openrouter.ai/api/v1"
    env_key = "OPENROUTER_API_KEY"
        "#;

        let provider: ModelProviderInfo = toml::from_str(toml).unwrap();
        assert_eq!(provider.provider_kind, ProviderKind::OpenRouter);
        assert_eq!(provider.wire_api, WireApi::Responses);
    }

    #[test]
    fn openrouter_can_use_chat_wire_api() {
        let toml = r#"
    name = "OpenRouter Chat"
    provider_kind = "openrouter"
    wire_api = "chat"
    base_url = "https://openrouter.ai/api/v1"
        "#;

        let provider: ModelProviderInfo = toml::from_str(toml).unwrap();
        assert_eq!(provider.provider_kind, ProviderKind::OpenRouter);
        assert_eq!(provider.wire_api, WireApi::Chat);
    }

    #[test]
    fn test_deserialize_custom_model_provider_toml() {
        let custom_provider_toml = r#"
name = "CustomProvider"
base_url = "http://localhost:8080/v1"
        "#;
        let expected_provider = ModelProviderInfo {
            name: "CustomProvider".into(),
            base_url: Some("http://localhost:8080/v1".into()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Chat,
            provider_kind: ProviderKind::OpenAi,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            api_key_env_var: None,
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: false,
        };

        let provider: ModelProviderInfo = toml::from_str(custom_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn deserializes_anthropic_provider_with_type_alias() {
        let provider_toml = r#"
type = "anthropic"
name = "Kimi"
base_url = "https://api.kimi.com/coding/v1"
api_key_env_var = "KIMI_FOR_CODING_API_KEY"
use_bearer_auth = true
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Kimi".into(),
            base_url: Some("https://api.kimi.com/coding/v1".into()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Anthropic,
            provider_kind: ProviderKind::Anthropic,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            api_key_env_var: Some("KIMI_FOR_CODING_API_KEY".into()),
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: true,
        };

        let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn api_key_prefers_api_key_env_var() {
        let provider = ModelProviderInfo {
            name: "Test".into(),
            base_url: None,
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Anthropic,
            provider_kind: ProviderKind::Anthropic,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            api_key_env_var: Some("TEST_API_KEY".into()),
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: false,
        };
        let _guard = EnvVarGuard::set("TEST_API_KEY", "secret-key");

        let key = provider.api_key().unwrap();

        assert_eq!(Some("secret-key".to_string()), key);
    }

    #[test]
    fn test_deserialize_azure_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Azure"
base_url = "https://xxxxx.openai.azure.com/openai"
env_key = "AZURE_OPENAI_API_KEY"
query_params = { api-version = "2025-04-01-preview" }
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Azure".into(),
            base_url: Some("https://xxxxx.openai.azure.com/openai".into()),
            env_key: Some("AZURE_OPENAI_API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Chat,
            provider_kind: ProviderKind::OpenAi,
            query_params: Some(maplit::hashmap! {
                "api-version".to_string() => "2025-04-01-preview".to_string(),
            }),
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            api_key_env_var: None,
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: false,
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_deserialize_example_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Example"
base_url = "https://example.com"
env_key = "API_KEY"
http_headers = { "X-Example-Header" = "example-value" }
env_http_headers = { "X-Example-Env-Header" = "EXAMPLE_ENV_VAR" }
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Example".into(),
            base_url: Some("https://example.com".into()),
            env_key: Some("API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Chat,
            provider_kind: ProviderKind::OpenAi,
            query_params: None,
            http_headers: Some(maplit::hashmap! {
                "X-Example-Header".to_string() => "example-value".to_string(),
            }),
            env_http_headers: Some(maplit::hashmap! {
                "X-Example-Env-Header".to_string() => "EXAMPLE_ENV_VAR".to_string(),
            }),
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            api_key_env_var: None,
            version: None,
            beta: None,
            project_id: None,
            use_bearer_auth: false,
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn detects_azure_responses_base_urls() {
        let positive_cases = [
            "https://foo.openai.azure.com/openai",
            "https://foo.openai.azure.us/openai/deployments/bar",
            "https://foo.cognitiveservices.azure.cn/openai",
            "https://foo.aoai.azure.com/openai",
            "https://foo.openai.azure-api.net/openai",
            "https://foo.z01.azurefd.net/",
        ];
        for base_url in positive_cases {
            let provider = ModelProviderInfo {
                name: "test".into(),
                base_url: Some(base_url.into()),
                env_key: None,
                env_key_instructions: None,
                experimental_bearer_token: None,
                wire_api: WireApi::Responses,
                query_params: None,
                http_headers: None,
                env_http_headers: None,
                request_max_retries: None,
                stream_max_retries: None,
                stream_idle_timeout_ms: None,
                requires_openai_auth: false,
                ..Default::default()
            };
            let api = provider.to_api_provider(None).expect("api provider");
            assert!(
                api.is_azure_responses_endpoint(),
                "expected {base_url} to be detected as Azure"
            );
        }

        let named_provider = ModelProviderInfo {
            name: "Azure".into(),
            base_url: Some("https://example.com".into()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            ..Default::default()
        };
        let named_api = named_provider.to_api_provider(None).expect("api provider");
        assert!(named_api.is_azure_responses_endpoint());

        let negative_cases = [
            "https://api.openai.com/v1",
            "https://example.com/openai",
            "https://myproxy.azurewebsites.net/openai",
        ];
        for base_url in negative_cases {
            let provider = ModelProviderInfo {
                name: "test".into(),
                base_url: Some(base_url.into()),
                env_key: None,
                env_key_instructions: None,
                experimental_bearer_token: None,
                wire_api: WireApi::Responses,
                query_params: None,
                http_headers: None,
                env_http_headers: None,
                request_max_retries: None,
                stream_max_retries: None,
                stream_idle_timeout_ms: None,
                requires_openai_auth: false,
                ..Default::default()
            };
            let api = provider.to_api_provider(None).expect("api provider");
            assert!(
                !api.is_azure_responses_endpoint(),
                "expected {base_url} not to be detected as Azure"
            );
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // set_var is unsafe on newer toolchains due to process-global mutation.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.original {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }
}
