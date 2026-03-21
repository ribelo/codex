use anyhow::Result;
use chrono::Duration;
use chrono::Utc;
use codex_core::AuthManager;
use codex_core::ModelClient;
use codex_core::ModelProviderInfo;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_core::ResponseStream;
use codex_core::WireApi;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::AuthDotJson;
use codex_core::auth::save_auth;
use codex_core::built_in_model_providers;
use codex_core::gemini::GEMINI_CODE_ASSIST_CLIENT_METADATA;
use codex_core::gemini::GEMINI_CODE_ASSIST_USER_AGENT;
use codex_core::gemini::GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT;
use codex_core::token_data::GeminiTokenData;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SessionSource;
use core_test_support::load_default_config_for_test;
use core_test_support::skip_if_no_network;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::path_regex;
use wiremock::matchers::query_param;

fn prompt_with_schema() -> Prompt {
    let mut prompt = Prompt::default();
    prompt.base_instructions = BaseInstructions {
        text: "Be terse and accurate.".to_string(),
    };
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "say hello".to_string(),
        }],
        end_turn: None,
        phase: None,
    });
    prompt.output_schema = Some(json!({
        "type": "object",
        "properties": {
            "answer": { "type": "string" }
        },
        "required": ["answer"],
        "additionalProperties": false
    }));
    prompt
}

fn provider_with_test_token(
    mut provider: ModelProviderInfo,
    base_url: String,
    wire_api: WireApi,
    token: &str,
) -> ModelProviderInfo {
    provider.name = format!("test-{}", wire_api.as_str());
    provider.base_url = Some(base_url);
    provider.env_key = None;
    provider.env_key_instructions = None;
    provider.experimental_bearer_token = Some(token.to_string());
    provider.wire_api = wire_api;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    provider.stream_idle_timeout_ms = Some(5_000);
    provider.supports_websockets = false;
    provider
}

async fn build_client_context(
    provider: ModelProviderInfo,
    model: &str,
) -> Result<(TempDir, ModelClient, ModelInfo, SessionTelemetry)> {
    build_client_context_with_auth(provider, model, None).await
}

async fn build_client_context_with_auth(
    provider: ModelProviderInfo,
    model: &str,
    auth_manager: Option<Arc<AuthManager>>,
) -> Result<(TempDir, ModelClient, ModelInfo, SessionTelemetry)> {
    let codex_home = TempDir::new()?;
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    config.model = Some(model.to_string());

    let model_info = codex_core::test_support::construct_model_info_offline(model, &config);
    let conversation_id = ThreadId::new();
    let telemetry = SessionTelemetry::new(
        conversation_id,
        model,
        model_info.slug.as_str(),
        None,
        None,
        None,
        "test_originator".to_string(),
        false,
        "test".to_string(),
        SessionSource::Exec,
    );
    let client = ModelClient::new(
        auth_manager,
        conversation_id,
        provider,
        SessionSource::Exec,
        config.model_verbosity,
        false,
        false,
        false,
        None,
    );
    Ok((codex_home, client, model_info, telemetry))
}

fn sample_google_tokens(label: &str, project_id: &str) -> GeminiTokenData {
    GeminiTokenData {
        access_token: format!("access-{label}"),
        refresh_token: format!("refresh-{label}"),
        id_token: None,
        project_id: Some(project_id.to_string()),
        managed_project_id: None,
        email: Some(format!("{label}@example.com")),
        expires_at: Some(Utc::now() + Duration::hours(1)),
    }
}

fn write_provider_auth(
    codex_home: &TempDir,
    gemini_accounts: Vec<GeminiTokenData>,
    antigravity_accounts: Vec<GeminiTokenData>,
) -> Result<Arc<AuthManager>> {
    save_auth(
        codex_home.path(),
        &AuthDotJson {
            auth_mode: None,
            openai_api_key: None,
            tokens: None,
            gemini_accounts,
            antigravity_accounts,
            last_refresh: None,
        },
        AuthCredentialsStoreMode::File,
    )?;
    Ok(Arc::new(AuthManager::new(
        codex_home.path().to_path_buf(),
        false,
        AuthCredentialsStoreMode::File,
    )))
}

async fn collect_events(mut stream: ResponseStream) -> Result<Vec<ResponseEvent>> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        let done = matches!(event, ResponseEvent::Completed { .. });
        events.push(event);
        if done {
            break;
        }
    }
    Ok(events)
}

async fn single_request(server: &MockServer) -> wiremock::Request {
    let requests = server
        .received_requests()
        .await
        .unwrap_or_else(|| panic!("request log should be available"));
    assert_eq!(requests.len(), 1);
    requests
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("expected a single captured request"))
}

fn sse_body(events: &[Value], include_done: bool) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str(&format!("data: {event}\n\n"));
    }
    if include_done {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_streams_tool_calls_and_json_schema() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "id": "call_a",
                            "index": 0,
                            "function": {
                                "name": "do_a",
                                "arguments": "{\"foo\":"
                            }
                        }]
                    }
                }]
            }),
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "1}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }),
        ],
        true,
    );

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_with_test_token(
        built_in_model_providers(/*openai_base_url*/ None)["openai"].clone(),
        format!("{}/v1", server.uri()),
        WireApi::Chat,
        "chat-key",
    );
    let (_codex_home, client, model_info, telemetry) =
        build_client_context(provider, "gpt-4.1-mini").await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::Medium),
            ReasoningSummary::Auto,
            None,
            None,
        )
        .await?;
    let events = collect_events(stream).await?;
    let request = single_request(&server).await;
    let body = request
        .body_json::<Value>()
        .unwrap_or_else(|_| panic!("chat request body should be json"));

    assert_eq!(request.url.path(), "/v1/chat/completions");
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer chat-key")
    );
    assert_eq!(body["model"], Value::String("gpt-4.1-mini".to_string()));
    assert_eq!(body["stream"], Value::Bool(true));
    assert_eq!(
        body["messages"][0]["role"],
        Value::String("system".to_string())
    );
    assert_eq!(
        body["response_format"]["type"],
        Value::String("json_schema".to_string())
    );

    assert!(events.iter().any(|event| {
        matches!(
            event,
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            }) if call_id == "call_a" && name == "do_a" && arguments == "{\"foo\":1}"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. }))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_wire_api_uses_native_headers_and_events() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[
            json!({
                "type": "message_start",
                "message": { "id": "msg_1" }
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "thinking" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "thinking": "step 1"
                }
            }),
            json!({
                "type": "content_block_stop",
                "index": 0
            }),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "tool_1",
                    "name": "shell",
                    "input": {}
                }
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {
                    "partial_json": "{\"cmd\":\"pwd\"}"
                }
            }),
            json!({
                "type": "content_block_stop",
                "index": 1
            }),
            json!({
                "type": "message_delta",
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 8,
                    "output_tokens_details": {
                        "thinking_tokens": 3
                    }
                }
            }),
            json!({
                "type": "message_stop"
            }),
        ],
        false,
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_with_test_token(
        built_in_model_providers(/*openai_base_url*/ None)["anthropic"].clone(),
        format!("{}/v1", server.uri()),
        WireApi::Anthropic,
        "anthropic-key",
    );
    let (_codex_home, client, model_info, telemetry) =
        build_client_context(provider, "claude-3-7-sonnet-latest").await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::Medium),
            ReasoningSummary::Detailed,
            None,
            None,
        )
        .await?;
    let events = collect_events(stream).await?;
    let request = single_request(&server).await;
    let body = request
        .body_json::<Value>()
        .unwrap_or_else(|_| panic!("anthropic request body should be json"));

    assert_eq!(request.url.path(), "/v1/messages");
    assert_eq!(
        request
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("anthropic-key")
    );
    assert_eq!(
        request
            .headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01")
    );
    assert_eq!(
        body["output_config"]["format"]["type"],
        Value::String("json_schema".to_string())
    );
    assert_eq!(
        body["system"],
        Value::String("Be terse and accurate.".to_string())
    );
    assert_eq!(
        body["thinking"]["type"],
        Value::String("enabled".to_string())
    );

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Created))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            ResponseEvent::ReasoningSummaryDelta { delta, .. } if delta == "step 1"
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            }) if call_id == "tool_1" && name == "shell" && arguments == "{\"cmd\":\"pwd\"}"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. }))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_compatible_provider_can_use_bearer_auth() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[
            json!({
                "type": "message_start",
                "message": { "id": "msg_bearer" }
            }),
            json!({
                "type": "message_stop"
            }),
        ],
        false,
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut provider = provider_with_test_token(
        built_in_model_providers(/*openai_base_url*/ None)["anthropic"].clone(),
        format!("{}/v1", server.uri()),
        WireApi::Anthropic,
        "anthropic-bearer",
    );
    provider.use_bearer_auth = true;

    let (_codex_home, client, model_info, telemetry) =
        build_client_context(provider, "claude-3-7-sonnet-latest").await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::Low),
            ReasoningSummary::Auto,
            None,
            None,
        )
        .await?;
    let _events = collect_events(stream).await?;
    let request = single_request(&server).await;

    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer anthropic-bearer")
    );
    assert_eq!(request.headers.get("x-api-key"), None);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_wire_api_uses_native_schema_and_thought_streaming() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[
            json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            {
                                "text": "considering",
                                "thought": true,
                                "thoughtSignature": "sig-1"
                            },
                            {
                                "functionCall": {
                                    "name": "shell",
                                    "args": { "cmd": "pwd" }
                                },
                                "thoughtSignature": "sig-1"
                            }
                        ]
                    },
                    "finishReason": "STOP"
                }]
            }),
            json!({
                "usageMetadata": {
                    "promptTokenCount": 11,
                    "candidatesTokenCount": 7,
                    "totalTokenCount": 18,
                    "thoughtsTokenCount": 3
                }
            }),
        ],
        false,
    );

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1beta/models/[^/]+:streamGenerateContent$"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_with_test_token(
        built_in_model_providers(/*openai_base_url*/ None)["gemini"].clone(),
        format!("{}/v1beta", server.uri()),
        WireApi::Gemini,
        "gemini-key",
    );
    let codex_home = TempDir::new()?;
    let auth_manager = write_provider_auth(
        &codex_home,
        vec![sample_google_tokens("oauth", "oauth-project")],
        Vec::new(),
    )?;
    let (_hold_home, client, model_info, telemetry) =
        build_client_context_with_auth(provider, "gemini-2.5-flash", Some(auth_manager)).await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::Medium),
            ReasoningSummary::Auto,
            None,
            None,
        )
        .await?;
    let events = collect_events(stream).await?;
    let request = single_request(&server).await;
    let body = request
        .body_json::<Value>()
        .unwrap_or_else(|_| panic!("gemini request body should be json"));

    assert_eq!(
        request
            .headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok()),
        Some("gemini-key")
    );
    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        Value::String("application/json".to_string())
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"],
        prompt_with_schema()
            .output_schema
            .unwrap_or_else(|| panic!("test prompt should include a schema"))
    );
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        Value::String("Be terse and accurate.".to_string())
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        Value::Number(8_192.into())
    );

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Created))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "considering"
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            }) if call_id == "gemini_sig:sig-1" && name == "shell" && arguments == "{\"cmd\":\"pwd\"}"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. }))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_wire_api_uses_code_assist_when_oauth_is_available() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "hello from oauth" }]
                },
                "finishReason": "STOP"
            }]
        })],
        false,
    );

    Mock::given(method("POST"))
        .and(path("/v1internal:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["gemini"].clone();
    provider.name = "test-gemini-oauth".to_string();
    provider.base_url = Some(server.uri());
    provider.env_key = None;
    provider.env_key_instructions = None;
    provider.experimental_bearer_token = None;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    provider.stream_idle_timeout_ms = Some(5_000);
    provider.supports_websockets = false;

    let codex_home = TempDir::new()?;
    let auth_manager = write_provider_auth(
        &codex_home,
        vec![sample_google_tokens("gemini-oauth", "oauth-project")],
        Vec::new(),
    )?;
    let (_hold_home, client, model_info, telemetry) =
        build_client_context_with_auth(provider, "gemini-2.5-flash", Some(auth_manager)).await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::Medium),
            ReasoningSummary::Auto,
            None,
            None,
        )
        .await?;
    let events = collect_events(stream).await?;
    let request = single_request(&server).await;
    let body = request
        .body_json::<Value>()
        .unwrap_or_else(|_| panic!("gemini oauth request body should be json"));

    assert_eq!(request.url.path(), "/v1internal:streamGenerateContent");
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer access-gemini-oauth")
    );
    assert_eq!(
        request
            .headers
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok()),
        None
    );
    assert_eq!(
        request
            .headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok()),
        Some(GEMINI_CODE_ASSIST_USER_AGENT)
    );
    assert_eq!(
        request
            .headers
            .get("x-goog-api-client")
            .and_then(|value| value.to_str().ok()),
        Some(GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT)
    );
    assert_eq!(
        request
            .headers
            .get("client-metadata")
            .and_then(|value| value.to_str().ok()),
        Some(GEMINI_CODE_ASSIST_CLIENT_METADATA)
    );
    assert_eq!(body["project"], Value::String("oauth-project".to_string()));
    assert_eq!(body["model"], Value::String("gemini-2.5-flash".to_string()));
    assert_eq!(
        body["userAgent"],
        Value::String("pi-coding-agent".to_string())
    );
    assert_eq!(
        body["requestId"]
            .as_str()
            .map(|request_id| request_id.starts_with("pi-")),
        Some(true)
    );
    assert_eq!(
        body["request"]["systemInstruction"]["parts"][0]["text"],
        Value::String("Be terse and accurate.".to_string())
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. }))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_3_oauth_uses_thinking_level() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "thinking level ok" }]
                },
                "finishReason": "STOP"
            }]
        })],
        false,
    );

    Mock::given(method("POST"))
        .and(path("/v1internal:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["gemini"].clone();
    provider.name = "test-gemini-3-oauth".to_string();
    provider.base_url = Some(server.uri());
    provider.env_key = None;
    provider.env_key_instructions = None;
    provider.experimental_bearer_token = None;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    provider.stream_idle_timeout_ms = Some(5_000);
    provider.supports_websockets = false;

    let codex_home = TempDir::new()?;
    let auth_manager = write_provider_auth(
        &codex_home,
        vec![sample_google_tokens("gemini-3-oauth", "oauth-project")],
        Vec::new(),
    )?;
    let (_hold_home, client, model_info, telemetry) =
        build_client_context_with_auth(provider, "gemini-3-pro-preview", Some(auth_manager))
            .await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::Medium),
            ReasoningSummary::Auto,
            None,
            None,
        )
        .await?;
    let events = collect_events(stream).await?;
    let request = single_request(&server).await;
    let body = request
        .body_json::<Value>()
        .unwrap_or_else(|_| panic!("gemini-3 oauth request body should be json"));

    assert_eq!(
        body["model"],
        Value::String("gemini-3-pro-preview".to_string())
    );
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        Value::String("HIGH".to_string())
    );
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"].get("thinkingBudget"),
        None
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. }))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn antigravity_wire_api_uses_oauth_and_claude_request_shape() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let response = sse_body(
        &[json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "antigravity ok" }]
                },
                "finishReason": "STOP"
            }]
        })],
        false,
    );

    Mock::given(method("POST"))
        .and(path("/v1internal:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(response, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["antigravity"].clone();
    provider.name = "test-antigravity".to_string();
    provider.base_url = Some(server.uri());
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    provider.stream_idle_timeout_ms = Some(5_000);
    provider.supports_websockets = false;

    let codex_home = TempDir::new()?;
    let auth_manager = write_provider_auth(
        &codex_home,
        Vec::new(),
        vec![sample_google_tokens("antigravity", "antigravity-project")],
    )?;
    let (_hold_home, client, model_info, telemetry) =
        build_client_context_with_auth(provider, "claude-opus-4-5-thinking", Some(auth_manager))
            .await?;
    let mut session = client.new_session();

    let stream = session
        .stream(
            &prompt_with_schema(),
            &model_info,
            &telemetry,
            Some(ReasoningEffort::High),
            ReasoningSummary::Auto,
            None,
            None,
        )
        .await?;
    let events = collect_events(stream).await?;
    let request = single_request(&server).await;
    let body = request
        .body_json::<Value>()
        .unwrap_or_else(|_| panic!("antigravity request body should be json"));

    assert_eq!(request.url.path(), "/v1internal:streamGenerateContent");
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer access-antigravity")
    );
    assert_eq!(
        request
            .headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok()),
        Some("antigravity/1.18.4 darwin/arm64")
    );
    assert_eq!(
        request
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok()),
        Some("interleaved-thinking-2025-05-14")
    );
    assert_eq!(
        body["project"],
        Value::String("antigravity-project".to_string())
    );
    assert_eq!(body["requestType"], Value::String("agent".to_string()));
    assert_eq!(body["userAgent"], Value::String("antigravity".to_string()));
    assert_eq!(
        body["requestId"]
            .as_str()
            .map(|request_id| request_id.starts_with("agent-")),
        Some(true)
    );
    assert_eq!(
        body["model"],
        Value::String("claude-opus-4-5-thinking".to_string())
    );
    assert!(
        body["request"]["sessionId"].as_str().is_some(),
        "antigravity requests should carry a session id"
    );
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        Value::Number(16_384.into())
    );
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
        Value::Bool(true)
    );
    assert_eq!(
        body["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        Value::String("VALIDATED".to_string())
    );
    assert_eq!(
        body["request"]["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .map(|text| text.contains("You are Antigravity")),
        Some(true)
    );
    assert_eq!(
        body["request"]["systemInstruction"]["parts"][1]["text"]
            .as_str()
            .map(|text| text.contains("Please ignore following [ignore]")),
        Some(true)
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ResponseEvent::Completed { .. }))
    );

    Ok(())
}
