use codex_core::AuthManager;
use codex_core::CodexAuth;
use codex_core::ContentItem;
use codex_core::ConversationManager;
use codex_core::ModelClient;
use codex_core::ModelProviderInfo;
use codex_core::NewConversation;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_core::ResponseItem;
use codex_core::WireApi;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::built_in_model_providers;
use codex_core::error::CodexErr;
use codex_core::openai_models::models_manager::ModelsManager;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SessionSource;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::ConversationId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Verbosity;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::WebSearchAction;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::user_input::UserInput;
use core_test_support::load_default_config_for_test;
use core_test_support::load_sse_fixture_with_id;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::get_responses_requests;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use dunce::canonicalize as normalize_path;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::header_regex;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

/// Build minimal SSE stream with completed marker using the JSON fixture.
fn sse_completed(id: &str) -> String {
    load_sse_fixture_with_id("tests/fixtures/completed_template.json", id)
}

#[expect(clippy::unwrap_used)]
fn assert_message_role(request_body: &serde_json::Value, role: &str) {
    assert_eq!(request_body["role"].as_str().unwrap(), role);
}

#[expect(clippy::expect_used)]
fn assert_message_equals(request_body: &serde_json::Value, text: &str) {
    let content = request_body["content"][0]["text"]
        .as_str()
        .expect("invalid message content");

    assert_eq!(
        content, text,
        "expected message content '{content}' to equal '{text}'"
    );
}

#[expect(clippy::expect_used)]
fn assert_message_starts_with(request_body: &serde_json::Value, text: &str) {
    let content = request_body["content"][0]["text"]
        .as_str()
        .expect("invalid message content");

    assert!(
        content.starts_with(text),
        "expected message content '{content}' to start with '{text}'"
    );
}

#[expect(clippy::expect_used)]
fn assert_message_ends_with(request_body: &serde_json::Value, text: &str) {
    let content = request_body["content"][0]["text"]
        .as_str()
        .expect("invalid message content");

    assert!(
        content.ends_with(text),
        "expected message content '{content}' to end with '{text}'"
    );
}

/// Writes an `auth.json` into the provided `codex_home` with the specified parameters.
/// Returns the fake JWT string written to `tokens.id_token`.
#[expect(clippy::unwrap_used)]
fn write_auth_json(
    codex_home: &TempDir,
    openai_api_key: Option<&str>,
    chatgpt_plan_type: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> String {
    use base64::Engine as _;

    let header = json!({ "alg": "none", "typ": "JWT" });
    let payload = json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": chatgpt_plan_type,
            "chatgpt_account_id": account_id.unwrap_or("acc-123")
        }
    });

    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header_b64 = b64(&serde_json::to_vec(&header).unwrap());
    let payload_b64 = b64(&serde_json::to_vec(&payload).unwrap());
    let signature_b64 = b64(b"sig");
    let fake_jwt = format!("{header_b64}.{payload_b64}.{signature_b64}");

    let mut tokens = json!({
        "id_token": fake_jwt,
        "access_token": access_token,
        "refresh_token": "refresh-test",
    });
    if let Some(acc) = account_id {
        tokens["account_id"] = json!(acc);
    }

    let auth_json = json!({
        "OPENAI_API_KEY": openai_api_key,
        "tokens": tokens,
        // RFC3339 datetime; value doesn't matter for these tests
        "last_refresh": chrono::Utc::now(),
    });

    std::fs::write(
        codex_home.path().join("auth.json"),
        serde_json::to_string_pretty(&auth_json).unwrap(),
    )
    .unwrap();

    fake_jwt
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_includes_initial_messages_and_sends_prior_items() {
    skip_if_no_network!();

    // Create a fake rollout session file with prior user + system + assistant messages.
    let tmpdir = TempDir::new().unwrap();
    let session_path = tmpdir.path().join("resume-session.jsonl");
    let mut f = std::fs::File::create(&session_path).unwrap();
    let convo_id = Uuid::new_v4();
    writeln!(
        f,
        "{}",
        json!({
            "timestamp": "2024-01-01T00:00:00.000Z",
            "type": "session_meta",
            "payload": {
                "id": convo_id,
                "timestamp": "2024-01-01T00:00:00Z",
                "instructions": "be nice",
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "model_provider": "test-provider"
            }
        })
    )
    .unwrap();

    // Prior item: user message (should be delivered)
    let prior_user = codex_protocol::models::ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: "resumed user message".to_string(),
        }],
    };
    let prior_user_json = serde_json::to_value(&prior_user).unwrap();
    writeln!(
        f,
        "{}",
        json!({
            "timestamp": "2024-01-01T00:00:01.000Z",
            "type": "response_item",
            "payload": prior_user_json
        })
    )
    .unwrap();

    // Prior item: system message (excluded from API history)
    let prior_system = codex_protocol::models::ResponseItem::Message {
        id: None,
        role: "system".to_string(),
        content: vec![codex_protocol::models::ContentItem::OutputText {
            signature: None,
            text: "resumed system instruction".to_string(),
        }],
    };
    let prior_system_json = serde_json::to_value(&prior_system).unwrap();
    writeln!(
        f,
        "{}",
        json!({
            "timestamp": "2024-01-01T00:00:02.000Z",
            "type": "response_item",
            "payload": prior_system_json
        })
    )
    .unwrap();

    // Prior item: assistant message
    let prior_item = codex_protocol::models::ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![codex_protocol::models::ContentItem::OutputText {
            signature: None,
            text: "resumed assistant message".to_string(),
        }],
    };
    let prior_item_json = serde_json::to_value(&prior_item).unwrap();
    writeln!(
        f,
        "{}",
        json!({
            "timestamp": "2024-01-01T00:00:03.000Z",
            "type": "response_item",
            "payload": prior_item_json
        })
    )
    .unwrap();
    drop(f);

    // Mock server that will receive the resumed request
    let server = MockServer::start().await;
    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;

    // Configure Codex to resume from our file
    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    // Also configure user instructions to ensure they are NOT delivered on resume.
    config.user_instructions = Some("be nice".to_string());

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let auth_manager =
        codex_core::AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let NewConversation {
        conversation: codex,
        session_configured,
        ..
    } = conversation_manager
        .resume_conversation_from_rollout(config, session_path.clone(), auth_manager)
        .await
        .expect("resume conversation");

    // 1) Assert initial_messages only includes existing EventMsg entries; response items are not converted
    let initial_msgs = session_configured
        .initial_messages
        .clone()
        .expect("expected initial messages option for resumed session");
    let initial_json = serde_json::to_value(&initial_msgs).unwrap();
    let expected_initial_json = json!([]);
    assert_eq!(initial_json, expected_initial_json);

    // 2) Submit new input; the request body must include the prior item followed by the new user input.
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();
    let expected_input = json!([
        {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "resumed user message" }]
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "resumed assistant message" }]
        },
        {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "hello" }]
        }
    ]);
    assert_eq!(request_body["input"], expected_input);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_conversation_id_and_model_headers_in_request() {
    skip_if_no_network!();

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let NewConversation {
        conversation: codex,
        conversation_id,
        session_configured: _,
    } = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation");

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // get request from the server
    let requests = get_responses_requests(&server).await;
    let request = requests
        .first()
        .expect("expected POST request to /responses");
    let request_conversation_id = request.headers.get("conversation_id").unwrap();
    let request_authorization = request.headers.get("authorization").unwrap();
    let request_originator = request.headers.get("originator").unwrap();

    assert_eq!(
        request_conversation_id.to_str().unwrap(),
        conversation_id.to_string()
    );
    assert_eq!(request_originator.to_str().unwrap(), "codex_cli_rs");
    assert_eq!(
        request_authorization.to_str().unwrap(),
        "Bearer Test API Key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_base_instructions_override_in_request() {
    skip_if_no_network!();
    // Mock server
    let server = MockServer::start().await;
    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;

    // Use OpenAI provider with a Flexible mode model (gpt-4o) so that
    // base_instructions_override is respected. Strict mode models (GPT-5.x)
    // ignore the override entirely per InstructionMode::Strict.
    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);

    config.model = Some("gpt-4o".to_string());
    config.base_instructions = Some("test instructions".to_string());
    config.model_provider = model_provider;

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert!(
        request_body["instructions"]
            .as_str()
            .unwrap()
            .contains("test instructions")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_auth_sends_correct_request() {
    skip_if_no_network!();

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/api/codex/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/api/codex", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    let conversation_manager = ConversationManager::with_models_provider(
        create_dummy_codex_auth(),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let NewConversation {
        conversation: codex,
        conversation_id,
        session_configured: _,
    } = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation");

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // get request from the server
    let requests = get_responses_requests(&server).await;
    let request = requests
        .first()
        .expect("expected POST request to /responses");
    let request_conversation_id = request.headers.get("conversation_id").unwrap();
    let request_authorization = request.headers.get("authorization").unwrap();
    let request_originator = request.headers.get("originator").unwrap();
    let request_chatgpt_account_id = request.headers.get("chatgpt-account-id").unwrap();
    let request_body = request.body_json::<serde_json::Value>().unwrap();

    assert_eq!(
        request_conversation_id.to_str().unwrap(),
        conversation_id.to_string()
    );
    assert_eq!(request_originator.to_str().unwrap(), "codex_cli_rs");
    assert_eq!(
        request_authorization.to_str().unwrap(),
        "Bearer Access Token"
    );
    assert_eq!(request_chatgpt_account_id.to_str().unwrap(), "account_id");
    assert!(request_body["stream"].as_bool().unwrap());
    assert_eq!(
        request_body["include"][0].as_str().unwrap(),
        "reasoning.encrypted_content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefers_apikey_when_config_prefers_apikey_even_with_chatgpt_tokens() {
    skip_if_no_network!();

    // Mock server
    let server = MockServer::start().await;

    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    // Expect API key header, no ChatGPT account header required.
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header_regex("Authorization", r"Bearer sk-test-key"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    // Write auth.json that contains both API key and ChatGPT tokens for a plan that should prefer ChatGPT,
    // but config will force API key preference.
    let _jwt = write_auth_json(
        &codex_home,
        Some("sk-test-key"),
        "pro",
        "Access-123",
        Some("acc-123"),
    );

    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;

    let auth_manager =
        match CodexAuth::from_auth_storage(codex_home.path(), AuthCredentialsStoreMode::File) {
            Ok(Some(auth)) => codex_core::AuthManager::from_auth_for_testing(auth),
            Ok(None) => panic!("No CodexAuth found in codex_home"),
            Err(e) => panic!("Failed to load CodexAuth: {e}"),
        };
    let conversation_manager = ConversationManager::new(
        auth_manager,
        SessionSource::Exec,
        codex_home.path().to_path_buf(),
    );
    let NewConversation {
        conversation: codex,
        ..
    } = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation");

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_user_instructions_message_in_request() {
    skip_if_no_network!();
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    config.user_instructions = Some("be nice".to_string());

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert!(
        !request_body["instructions"]
            .as_str()
            .unwrap()
            .contains("be nice")
    );
    assert_message_role(&request_body["input"][0], "user");
    assert_message_starts_with(&request_body["input"][0], "# AGENTS.md instructions for ");
    assert_message_ends_with(&request_body["input"][0], "</INSTRUCTIONS>");
    let ui_text = request_body["input"][0]["content"][0]["text"]
        .as_str()
        .expect("invalid message content");
    assert!(ui_text.contains("<INSTRUCTIONS>"));
    assert!(ui_text.contains("be nice"));
    assert_message_role(&request_body["input"][1], "user");
    assert_message_starts_with(&request_body["input"][1], "<environment_context>");
    assert_message_ends_with(&request_body["input"][1], "</environment_context>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skills_append_to_instructions_when_feature_enabled() {
    skip_if_no_network!();
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    let codex_home = TempDir::new().unwrap();
    let skill_dir = codex_home.path().join("skills/demo");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: build charts\n---\n\n# body\n",
    )
    .expect("write skill");

    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    config.cwd = codex_home.path().to_path_buf();

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert_message_role(&request_body["input"][0], "user");
    let instructions_text = request_body["input"][0]["content"][0]["text"]
        .as_str()
        .expect("instructions text");
    assert!(
        instructions_text.contains("## Skills"),
        "expected skills section present"
    );
    assert!(
        instructions_text.contains("demo: build charts"),
        "expected skill summary"
    );
    let expected_path = normalize_path(skill_dir.join("SKILL.md")).unwrap();
    let expected_path_str = expected_path.to_string_lossy().replace('\\', "/");
    assert!(
        instructions_text.contains(&expected_path_str),
        "expected path {expected_path_str} in instructions"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_configured_effort_in_request() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex()
        .with_model("gpt-5.1-codex")
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::Medium);
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert_eq!(
        request_body
            .get("reasoning")
            .and_then(|t| t.get("effort"))
            .and_then(|v| v.as_str()),
        Some("medium")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_no_effort_in_request() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex()
        .with_model("gpt-5.1-codex")
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert_eq!(
        request_body
            .get("reasoning")
            .and_then(|t| t.get("effort"))
            .and_then(|v| v.as_str()),
        None
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_default_reasoning_effort_in_request_when_defined_by_model_family()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex().with_model("gpt-5.1").build(&server).await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert_eq!(
        request_body
            .get("reasoning")
            .and_then(|t| t.get("effort"))
            .and_then(|v| v.as_str()),
        Some("medium")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_reasoning_summary_is_sent() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex()
        .with_config(|config| {
            config.model_reasoning_summary = ReasoningSummary::Concise;
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    pretty_assertions::assert_eq!(
        request_body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("summary"))
            .and_then(|value| value.as_str()),
        Some("concise")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_summary_is_omitted_when_disabled() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex()
        .with_config(|config| {
            config.model_reasoning_summary = ReasoningSummary::None;
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    pretty_assertions::assert_eq!(
        request_body
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("summary")),
        None
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_default_verbosity_in_request() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex().with_model("gpt-5.1").build(&server).await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert_eq!(
        request_body
            .get("text")
            .and_then(|t| t.get("verbosity"))
            .and_then(|v| v.as_str()),
        Some("low")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_verbosity_not_sent_for_models_without_support() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex()
        .with_model("gpt-5.1-codex")
        .with_config(|config| {
            config.model_verbosity = Some(Verbosity::High);
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert!(
        request_body
            .get("text")
            .and_then(|t| t.get("verbosity"))
            .is_none()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_verbosity_is_sent() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;
    let TestCodex { codex, .. } = test_codex()
        .with_model("gpt-5.1")
        .with_config(|config| {
            config.model_verbosity = Some(Verbosity::High);
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert_eq!(
        request_body
            .get("text")
            .and_then(|t| t.get("verbosity"))
            .and_then(|v| v.as_str()),
        Some("high")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_developer_instructions_message_in_request() {
    skip_if_no_network!();
    let server = MockServer::start().await;

    let resp_mock = mount_sse_once(&server, sse_completed("resp1")).await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    config.user_instructions = Some("be nice".to_string());
    config.developer_instructions = Some("be useful".to_string());

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = resp_mock.single_request();
    let request_body = request.body_json();

    assert!(
        !request_body["instructions"]
            .as_str()
            .unwrap()
            .contains("be nice")
    );
    assert_message_role(&request_body["input"][0], "developer");
    assert_message_equals(&request_body["input"][0], "be useful");
    assert_message_role(&request_body["input"][1], "user");
    assert_message_starts_with(&request_body["input"][1], "# AGENTS.md instructions for ");
    assert_message_ends_with(&request_body["input"][1], "</INSTRUCTIONS>");
    let ui_text = request_body["input"][1]["content"][0]["text"]
        .as_str()
        .expect("invalid message content");
    assert!(ui_text.contains("<INSTRUCTIONS>"));
    assert!(ui_text.contains("be nice"));
    assert_message_role(&request_body["input"][2], "user");
    assert_message_starts_with(&request_body["input"][2], "<environment_context>");
    assert_message_ends_with(&request_body["input"][2], "</environment_context>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_responses_request_includes_store_and_reasoning_ids() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    let sse_body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );

    let template = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_body, "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/openai/responses"))
        .respond_with(template)
        .expect(1)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "azure".into(),
        base_url: Some(format!("{}/openai", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
        ..Default::default()
    };

    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort;
    let summary = config.model_reasoning_summary;
    let model = ModelsManager::get_model_offline(config.model.as_deref());
    config.model = Some(model.clone());
    let config = Arc::new(config);
    let model_family = ModelsManager::construct_model_family_offline(model.as_str(), &config);
    let conversation_id = ConversationId::new();
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
    let otel_event_manager = OtelEventManager::new(
        conversation_id,
        model.as_str(),
        model_family.slug.as_str(),
        None,
        Some("test@test.com".to_string()),
        auth_manager.get_auth_mode(),
        false,
        "test".to_string(),
    );

    let client = ModelClient::new(
        Arc::clone(&config),
        None,
        model_family,
        otel_event_manager,
        provider,
        effort,
        summary,
        conversation_id,
        codex_protocol::protocol::SessionSource::Exec,
    );

    let mut prompt = Prompt::default();
    prompt.input.push(ResponseItem::Reasoning {
        id: "reasoning-id".into(),
        summary: vec![ReasoningItemReasoningSummary::SummaryText {
            text: "summary".into(),
        }],
        content: Some(vec![ReasoningItemContent::ReasoningText {
            text: "content".into(),
        }]),
        encrypted_content: None,
    });
    prompt.input.push(ResponseItem::Message {
        id: Some("message-id".into()),
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            signature: None,
            text: "message".into(),
        }],
    });
    prompt.input.push(ResponseItem::WebSearchCall {
        id: Some("web-search-id".into()),
        status: Some("completed".into()),
        action: WebSearchAction::Search {
            query: Some("weather".into()),
        },
    });
    prompt.input.push(ResponseItem::FunctionCall {
        id: Some("function-id".into()),
        name: "do_thing".into(),
        arguments: "{}".into(),
        call_id: "function-call-id".into(),
    });
    prompt.input.push(ResponseItem::CustomToolCall {
        id: Some("custom-tool-id".into()),
        status: Some("completed".into()),
        call_id: "custom-tool-call-id".into(),
        name: "custom_tool".into(),
        input: "{}".into(),
    });

    let mut stream = client
        .stream(&prompt)
        .await
        .expect("responses stream to start");

    while let Some(event) = stream.next().await {
        if let Ok(ResponseEvent::Completed { .. }) = event {
            break;
        }
    }

    let requests = get_responses_requests(&server).await;
    assert_eq!(requests.len(), 1, "expected a single POST request");
    let body: serde_json::Value = requests[0]
        .body_json()
        .expect("request body to be valid JSON");

    assert_eq!(body["store"], serde_json::Value::Bool(true));
    assert_eq!(body["stream"], serde_json::Value::Bool(true));
    assert_eq!(body["input"].as_array().map(Vec::len), Some(5));
    assert_eq!(body["input"][0]["id"].as_str(), Some("reasoning-id"));
    assert_eq!(body["input"][1]["id"].as_str(), Some("message-id"));
    assert_eq!(body["input"][2]["id"].as_str(), Some("web-search-id"));
    assert_eq!(body["input"][3]["id"].as_str(), Some("function-id"));
    assert_eq!(body["input"][4]["id"].as_str(), Some("custom-tool-id"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_count_includes_rate_limits_snapshot() {
    skip_if_no_network!();
    let server = MockServer::start().await;

    let sse_body = sse(vec![ev_completed_with_tokens("resp_rate", 123)]);

    let response = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .insert_header("x-codex-primary-used-percent", "12.5")
        .insert_header("x-codex-secondary-used-percent", "40.0")
        .insert_header("x-codex-primary-window-minutes", "10")
        .insert_header("x-codex-secondary-window-minutes", "60")
        .insert_header("x-codex-primary-reset-at", "1704069000")
        .insert_header("x-codex-secondary-reset-at", "1704074400")
        .set_body_raw(sse_body, "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;

    let mut provider = built_in_model_providers()["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));

    let home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&home);
    config.model_provider = provider;

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("test"),
        config.model_provider.clone(),
        home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    let first_token_event =
        wait_for_event(&codex, |msg| matches!(msg, EventMsg::TokenCount(_))).await;
    let rate_limit_only = match first_token_event {
        EventMsg::TokenCount(ev) => ev,
        _ => unreachable!(),
    };

    let rate_limit_json = serde_json::to_value(&rate_limit_only).unwrap();
    pretty_assertions::assert_eq!(
        rate_limit_json,
        json!({
            "info": null,
            "rate_limits": {
                "primary": {
                    "used_percent": 12.5,
                    "window_minutes": 10,
                    "resets_at": 1704069000
                },
                "secondary": {
                    "used_percent": 40.0,
                    "window_minutes": 60,
                    "resets_at": 1704074400
                },
                "credits": null,
                "plan_type": null
            }
        })
    );

    let token_event = wait_for_event(
        &codex,
        |msg| matches!(msg, EventMsg::TokenCount(ev) if ev.info.is_some()),
    )
    .await;
    let final_payload = match token_event {
        EventMsg::TokenCount(ev) => ev,
        _ => unreachable!(),
    };
    // Assert full JSON for the final token count event (usage + rate limits)
    let final_json = serde_json::to_value(&final_payload).unwrap();
    pretty_assertions::assert_eq!(
        final_json,
        json!({
            "info": {
                "total_token_usage": {
                    "input_tokens": 123,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 123
                },
                "last_token_usage": {
                    "input_tokens": 123,
                    "cached_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 123
                },
                // Test config sets model_context_window = 128000, effective = 128000 * 95% = 121600
                "model_context_window": 121600
            },
            "rate_limits": {
                "primary": {
                    "used_percent": 12.5,
                    "window_minutes": 10,
                    "resets_at": 1704069000
                },
                "secondary": {
                    "used_percent": 40.0,
                    "window_minutes": 60,
                    "resets_at": 1704074400
                },
                "credits": null,
                "plan_type": null
            }
        })
    );
    let usage = final_payload
        .info
        .expect("token usage info should be recorded after completion");
    assert_eq!(usage.total_token_usage.total_tokens, 123);
    let final_snapshot = final_payload
        .rate_limits
        .expect("latest rate limit snapshot should be retained");
    assert_eq!(
        final_snapshot
            .primary
            .as_ref()
            .map(|window| window.used_percent),
        Some(12.5)
    );
    assert_eq!(
        final_snapshot
            .primary
            .as_ref()
            .and_then(|window| window.resets_at),
        Some(1704069000)
    );

    wait_for_event(&codex, |msg| matches!(msg, EventMsg::TaskComplete(_))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_error_emits_rate_limit_event() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    let response = ResponseTemplate::new(429)
        .insert_header("x-codex-primary-used-percent", "100.0")
        .insert_header("x-codex-secondary-used-percent", "87.5")
        .insert_header("x-codex-primary-over-secondary-limit-percent", "95.0")
        .insert_header("x-codex-primary-window-minutes", "15")
        .insert_header("x-codex-secondary-window-minutes", "60")
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": 1704067242,
                "plan_type": "pro"
            }
        }));

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;

    let mut builder = test_codex();
    let codex_fixture = builder.build(&server).await?;
    let codex = codex_fixture.codex.clone();

    let expected_limits = json!({
        "primary": {
            "used_percent": 100.0,
            "window_minutes": 15,
            "resets_at": null
        },
        "secondary": {
            "used_percent": 87.5,
            "window_minutes": 60,
            "resets_at": null
        },
        "credits": null,
        "plan_type": null
    });

    let submission_id = codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .expect("submission should succeed while emitting usage limit error events");

    let token_event = wait_for_event(&codex, |msg| matches!(msg, EventMsg::TokenCount(_))).await;
    let EventMsg::TokenCount(event) = token_event else {
        unreachable!();
    };

    let event_json = serde_json::to_value(&event).expect("serialize token count event");
    pretty_assertions::assert_eq!(
        event_json,
        json!({
            "info": null,
            "rate_limits": expected_limits
        })
    );

    let error_event = wait_for_event(&codex, |msg| matches!(msg, EventMsg::Error(_))).await;
    let EventMsg::Error(error_event) = error_event else {
        unreachable!();
    };
    assert!(
        error_event.message.to_lowercase().contains("usage limit"),
        "unexpected error message for submission {submission_id}: {}",
        error_event.message
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_window_error_sets_total_tokens_to_model_window() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;

    const MODEL_CONTEXT_WINDOW: i64 = 272_000;
    const EFFECTIVE_CONTEXT_WINDOW: i64 = (MODEL_CONTEXT_WINDOW * 95) / 100;

    mount_sse_once_match(
        &server,
        body_string_contains("trigger context window"),
        sse_failed(
            "resp_context_window",
            "context_length_exceeded",
            "Your input exceeds the context window of this model. Please adjust your input and try again.",
        ),
    )
    .await;

    mount_sse_once_match(
        &server,
        body_string_contains("seed turn"),
        sse_completed("resp_seed"),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|config| {
            config.model = Some("gpt-5.1".to_string());
            config.model_context_window = MODEL_CONTEXT_WINDOW;
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "seed turn".into(),
            }],
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "trigger context window".into(),
            }],
        })
        .await?;

    let token_event = wait_for_event(&codex, |event| {
        matches!(
            event,
            EventMsg::TokenCount(payload)
                if payload.info.as_ref().is_some_and(|info| {
                    info.model_context_window == Some(info.total_token_usage.total_tokens)
                        && info.total_token_usage.total_tokens > 0
                })
        )
    })
    .await;

    let EventMsg::TokenCount(token_payload) = token_event else {
        unreachable!("wait_for_event returned unexpected event");
    };

    let info = token_payload
        .info
        .expect("token usage info present when context window is exceeded");

    assert_eq!(info.model_context_window, Some(EFFECTIVE_CONTEXT_WINDOW));
    assert_eq!(
        info.total_token_usage.total_tokens,
        EFFECTIVE_CONTEXT_WINDOW
    );

    let error_event = wait_for_event(&codex, |ev| matches!(ev, EventMsg::Error(_))).await;
    let expected_context_window_message = CodexErr::ContextWindowExceeded.to_string();
    assert!(
        matches!(
            error_event,
            EventMsg::Error(ref err) if err.message == expected_context_window_message
        ),
        "expected context window error; got {error_event:?}"
    );

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_overrides_assign_properties_used_for_responses_url() {
    skip_if_no_network!();
    let existing_env_var_with_random_value = if cfg!(windows) { "USERNAME" } else { "USER" };

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    // Expect POST to /openai/responses with api-version query param
    Mock::given(method("POST"))
        .and(path("/openai/responses"))
        .and(query_param("api-version", "2025-04-01-preview"))
        .and(header_regex("Custom-Header", "Value"))
        .and(header_regex(
            "Authorization",
            format!(
                "Bearer {}",
                std::env::var(existing_env_var_with_random_value).unwrap()
            )
            .as_str(),
        ))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "custom".to_string(),
        base_url: Some(format!("{}/openai", server.uri())),
        // Reuse the existing environment variable to avoid using unsafe code
        env_key: Some(existing_env_var_with_random_value.to_string()),
        experimental_bearer_token: None,
        query_params: Some(std::collections::HashMap::from([(
            "api-version".to_string(),
            "2025-04-01-preview".to_string(),
        )])),
        env_key_instructions: None,
        wire_api: WireApi::Responses,
        http_headers: Some(std::collections::HashMap::from([(
            "Custom-Header".to_string(),
            "Value".to_string(),
        )])),
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
        ..Default::default()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = provider;

    let conversation_manager = ConversationManager::with_models_provider(
        create_dummy_codex_auth(),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_var_overrides_loaded_auth() {
    skip_if_no_network!();
    let existing_env_var_with_random_value = if cfg!(windows) { "USERNAME" } else { "USER" };

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    // Expect POST to /openai/responses with api-version query param
    Mock::given(method("POST"))
        .and(path("/openai/responses"))
        .and(query_param("api-version", "2025-04-01-preview"))
        .and(header_regex("Custom-Header", "Value"))
        .and(header_regex(
            "Authorization",
            format!(
                "Bearer {}",
                std::env::var(existing_env_var_with_random_value).unwrap()
            )
            .as_str(),
        ))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "custom".to_string(),
        base_url: Some(format!("{}/openai", server.uri())),
        // Reuse the existing environment variable to avoid using unsafe code
        env_key: Some(existing_env_var_with_random_value.to_string()),
        query_params: Some(std::collections::HashMap::from([(
            "api-version".to_string(),
            "2025-04-01-preview".to_string(),
        )])),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        http_headers: Some(std::collections::HashMap::from([(
            "Custom-Header".to_string(),
            "Value".to_string(),
        )])),
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
        ..Default::default()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = provider;

    let conversation_manager = ConversationManager::with_models_provider(
        create_dummy_codex_auth(),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let codex = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation")
        .conversation;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
}

fn create_dummy_codex_auth() -> CodexAuth {
    CodexAuth::create_dummy_chatgpt_auth_for_testing()
}

/// Scenario:
/// - Turn 1: user sends U1; model streams deltas then a final assistant message A.
/// - Turn 2: user sends U2; model streams a delta then the same final assistant message A.
/// - Turn 3: user sends U3; model responds (same SSE again, not important).
///
/// We assert that the `input` sent on each turn contains the expected conversation history
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_dedupes_streamed_and_final_messages_across_turns() {
    // Skip under Codex sandbox network restrictions (mirrors other tests).
    skip_if_no_network!();

    // Mock server that will receive three sequential requests and return the same SSE stream
    // each time: a few deltas, then a final assistant message, then completed.
    let server = MockServer::start().await;

    // Build a small SSE stream with deltas and a final assistant message.
    // We emit the same body for all 3 turns; ids vary but are unused by assertions.
    let sse_raw = r##"[
        {"type":"response.output_item.added", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":""}]
        }},
        {"type":"response.output_text.delta", "delta":"Hey "},
        {"type":"response.output_text.delta", "delta":"there"},
        {"type":"response.output_text.delta", "delta":"!\n"},
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"Hey there!\n"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"##;
    let sse1 = core_test_support::load_sse_fixture_with_id_from_str(sse_raw, "resp1");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse1.clone(), "text/event-stream"),
        )
        .expect(3) // respond identically to the three sequential turns
        .mount(&server)
        .await;

    // Configure provider to point to mock server (Responses API) and use API key auth.
    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session with isolated codex home.
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("Test API Key"),
        config.model_provider.clone(),
        codex_home.path().to_path_buf(),
    );
    let NewConversation {
        conversation: codex,
        ..
    } = conversation_manager
        .new_conversation(config)
        .await
        .expect("create new conversation");

    // Turn 1: user sends U1; wait for completion.
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text { text: "U1".into() }],
        })
        .await
        .unwrap();
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Turn 2: user sends U2; wait for completion.
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text { text: "U2".into() }],
        })
        .await
        .unwrap();
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Turn 3: user sends U3; wait for completion.
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text { text: "U3".into() }],
        })
        .await
        .unwrap();
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Inspect the three captured requests.
    let requests = get_responses_requests(&server).await;
    assert_eq!(requests.len(), 3, "expected 3 requests (one per turn)");

    // Replace full-array compare with tail-only raw JSON compare using a single hard-coded value.
    let r3_tail_expected = json!([
        {
            "type": "message",
            "role": "user",
            "content": [{"type":"input_text","text":"U1"}]
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type":"output_text","text":"Hey there!\n"}]
        },
        {
            "type": "message",
            "role": "user",
            "content": [{"type":"input_text","text":"U2"}]
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type":"output_text","text":"Hey there!\n"}]
        },
        {
            "type": "message",
            "role": "user",
            "content": [{"type":"input_text","text":"U3"}]
        }
    ]);

    let r3_input_array = requests[2]
        .body_json::<serde_json::Value>()
        .unwrap()
        .get("input")
        .and_then(|v| v.as_array())
        .cloned()
        .expect("r3 missing input array");
    // skipping earlier context and developer messages
    let tail_len = r3_tail_expected.as_array().unwrap().len();
    let actual_tail = &r3_input_array[r3_input_array.len() - tail_len..];
    assert_eq!(
        serde_json::Value::Array(actual_tail.to_vec()),
        r3_tail_expected,
        "request 3 tail mismatch",
    );
}

// =============================================================================
// Subagent request integration tests
// =============================================================================

use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::start_mock_server;
use wiremock::matchers::header;

/// Test that task-based delegation includes task context + identity prompt.
/// This verifies that the `search` task description and worker identity prompt are present in the
/// subagent request for Strict instruction mode models.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_request_includes_task_description_and_identity_prompt() {
    skip_if_no_network!();

    let server = start_mock_server().await;

    // Parent request: model issues a task function call to invoke finder subagent
    let task_args = json!({
        "description": "Find code",
        "prompt": "Search for the main function",
        "task_type": "search"
    });
    let parent_response = sse(vec![
        ev_response_created("resp-parent-1"),
        ev_function_call("call-task-1", "task", &task_args.to_string()),
        ev_completed("resp-parent-1"),
    ]);
    let parent_mock = mount_sse_once(&server, parent_response).await;

    // Subagent request: matched by x-openai-subagent header
    let subagent_response = sse(vec![
        ev_response_created("resp-subagent-1"),
        ev_assistant_message("msg-subagent-1", "Found the files."),
        ev_completed("resp-subagent-1"),
    ]);
    let subagent_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "search"),
        subagent_response,
    )
    .await;

    // Parent completion after subagent finishes
    let parent_completion = sse(vec![
        ev_response_created("resp-parent-2"),
        ev_assistant_message("msg-parent-2", "Done."),
        ev_completed("resp-parent-2"),
    ]);
    let parent_completion_mock = mount_sse_once(&server, parent_completion).await;

    // Build and run codex
    let mut builder = test_codex();
    let test = builder
        .build(&server)
        .await
        .expect("create test Codex conversation");

    test.submit_turn("invoke the finder agent")
        .await
        .expect("submit turn");

    // Ensure we observed both the parent and subagent requests.
    assert_eq!(parent_mock.requests().len(), 1);
    assert_eq!(subagent_mock.requests().len(), 1);
    assert_eq!(parent_completion_mock.requests().len(), 1);

    // Verify the subagent request contains the task description + identity prompt.
    let subagent_request = &subagent_mock.requests()[0];
    let combined = format!(
        "{}\n{}",
        subagent_request.instructions(),
        subagent_request.message_input_texts("user").join("\n")
    );
    assert!(
        combined.contains("Find code, definitions, and references"),
        "subagent request should include task description. Got: {combined}"
    );
    assert!(
        combined.contains("# Worker Context"),
        "subagent request should include identity prompt header. Got: {combined}"
    );
}

/// Test that subagent request includes the identity prompt with a valid parent session UUID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_request_includes_identity_prompt_with_parent_uuid() {
    skip_if_no_network!();

    let server = start_mock_server().await;

    // Parent request: model issues a task function call
    let task_args = json!({
        "description": "Quick fix",
        "prompt": "Fix the typo",
        "task_type": "code"
    });
    let parent_response = sse(vec![
        ev_response_created("resp-parent-1"),
        ev_function_call("call-task-1", "task", &task_args.to_string()),
        ev_completed("resp-parent-1"),
    ]);
    let parent_mock = mount_sse_once(&server, parent_response).await;

    // Subagent request
    let subagent_response = sse(vec![
        ev_response_created("resp-subagent-1"),
        ev_assistant_message("msg-subagent-1", "Fixed."),
        ev_completed("resp-subagent-1"),
    ]);
    let subagent_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "code"),
        subagent_response,
    )
    .await;

    // Parent completion
    let parent_completion = sse(vec![
        ev_response_created("resp-parent-2"),
        ev_assistant_message("msg-parent-2", "Done."),
        ev_completed("resp-parent-2"),
    ]);
    let parent_completion_mock = mount_sse_once(&server, parent_completion).await;

    // Build and run codex
    let mut builder = test_codex();
    let test = builder
        .build(&server)
        .await
        .expect("create test Codex conversation");

    test.submit_turn("invoke the rush agent")
        .await
        .expect("submit turn");

    // Ensure we observed both the parent and subagent requests.
    assert_eq!(parent_mock.requests().len(), 1);
    assert_eq!(subagent_mock.requests().len(), 1);
    assert_eq!(parent_completion_mock.requests().len(), 1);

    // Verify the subagent request contains identity prompt
    let subagent_request = &subagent_mock.requests()[0];
    let combined = format!(
        "{}\n{}",
        subagent_request.instructions(),
        subagent_request.message_input_texts("user").join("\n")
    );

    // Check for identity prompt header and content
    assert!(
        combined.contains("# Worker Context"),
        "subagent request should include identity prompt header. Got: {combined}"
    );
    assert!(
        combined.contains("You are an internal worker session delegated by a parent agent"),
        "subagent request should include identity prompt text. Got: {combined}"
    );

    // Verify parent session ID is present and is a valid UUID format
    assert!(
        combined.contains("Parent session ID:"),
        "identity prompt should include parent session ID. Got: {combined}"
    );

    // Extract and validate the UUID
    let uuid_regex = regex_lite::Regex::new(
        r"Parent session ID: ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
    )
    .unwrap();
    assert!(
        uuid_regex.is_match(&combined),
        "parent session ID should be a valid UUID. Got: {combined}"
    );
}

/// Test that even when a subagent template has empty system_prompt (frontmatter-only),
/// the identity prompt is still injected into the first user message (wrapped in <agent_context>).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_with_empty_system_prompt_still_gets_identity_prompt() {
    skip_if_no_network!();

    let server = start_mock_server().await;

    // Create a custom frontmatter-only subagent with an empty system_prompt.
    let task_args = json!({
        "description": "Review code",
        "prompt": "Check the changes",
        "task_type": "minimal"
    });
    let parent_response = sse(vec![
        ev_response_created("resp-parent-1"),
        ev_function_call("call-task-1", "task", &task_args.to_string()),
        ev_completed("resp-parent-1"),
    ]);
    let parent_mock = mount_sse_once(&server, parent_response).await;

    // Subagent request
    let subagent_response = sse(vec![
        ev_response_created("resp-subagent-1"),
        ev_assistant_message("msg-subagent-1", "Reviewed."),
        ev_completed("resp-subagent-1"),
    ]);
    let subagent_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "minimal"),
        subagent_response,
    )
    .await;

    // Parent completion
    let parent_completion = sse(vec![
        ev_response_created("resp-parent-2"),
        ev_assistant_message("msg-parent-2", "Done."),
        ev_completed("resp-parent-2"),
    ]);
    let parent_completion_mock = mount_sse_once(&server, parent_completion).await;

    // Build and run codex
    let mut builder = test_codex().with_config(|config| {
        config.tasks = vec![codex_core::config::types::TaskConfigToml {
            task_type: "minimal".to_string(),
            description: None,
            skills: None,
            model: None,
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: None,
        }];
    });
    let test = builder
        .build(&server)
        .await
        .expect("create test Codex conversation");

    test.submit_turn("invoke the minimal agent")
        .await
        .expect("submit turn");

    assert_eq!(parent_mock.requests().len(), 1);
    assert_eq!(subagent_mock.requests().len(), 1);
    assert_eq!(parent_completion_mock.requests().len(), 1);

    let subagent_request = &subagent_mock.requests()[0];
    let combined = format!(
        "{}\n{}",
        subagent_request.instructions(),
        subagent_request.message_input_texts("user").join("\n")
    );

    assert!(
        combined.contains("# Worker Context"),
        "identity prompt should be injected. Got: {combined}"
    );
    assert!(
        combined.contains("You are an internal worker session delegated by a parent agent"),
        "identity prompt content should be present. Got: {combined}"
    );
}

/// Test nested subagent delegation (parent → child → grandchild) includes an identity prompt
/// in each subagent's first user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_delegation_includes_identity_prompt_in_user_input() {
    skip_if_no_network!();

    fn parent_session_ids(text: &str) -> Vec<Uuid> {
        text.lines()
            .filter_map(|line| line.split("Parent session ID: ").nth(1))
            .filter_map(|id| Uuid::parse_str(id.trim()).ok())
            .collect()
    }

    let server = start_mock_server().await;

    let mut builder = test_codex().with_config(|config| {
        config.tasks = vec![
            codex_core::config::types::TaskConfigToml {
                task_type: "child".to_string(),
                description: Some("CHILD_SYSTEM_PROMPT_MARKER".to_string()),
                skills: None,
                model: None,
                reasoning_effort: None,
                sandbox_policy: None,
                approval_policy: None,
                difficulty: None,
            },
            codex_core::config::types::TaskConfigToml {
                task_type: "grandchild".to_string(),
                description: Some("GRANDCHILD_SYSTEM_PROMPT_MARKER".to_string()),
                skills: None,
                model: None,
                reasoning_effort: None,
                sandbox_policy: None,
                approval_policy: None,
                difficulty: None,
            },
        ];
    });

    // Root -> child task call.
    let parent_task_args = json!({
        "description": "Spawn child",
        "prompt": "CHILD_PROMPT",
        "task_type": "child"
    });
    let parent_response = sse(vec![
        ev_response_created("resp-parent-1"),
        ev_function_call("call-parent-task-1", "task", &parent_task_args.to_string()),
        ev_completed("resp-parent-1"),
    ]);
    mount_sse_once(&server, parent_response).await;

    // Child -> grandchild task call.
    let child_task_args = json!({
        "description": "Spawn grandchild",
        "prompt": "GRANDCHILD_PROMPT",
        "task_type": "grandchild"
    });
    let child_response = sse(vec![
        ev_response_created("resp-child-1"),
        ev_function_call("call-child-task-1", "task", &child_task_args.to_string()),
        ev_completed("resp-child-1"),
    ]);
    let child_first_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "child"),
        child_response,
    )
    .await;

    // Grandchild completion.
    let grandchild_response = sse(vec![
        ev_response_created("resp-grandchild-1"),
        ev_assistant_message("msg-grandchild-1", "grandchild done"),
        ev_completed("resp-grandchild-1"),
    ]);
    let grandchild_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "grandchild"),
        grandchild_response,
    )
    .await;

    // Child completion after receiving grandchild output.
    let child_completion = sse(vec![
        ev_response_created("resp-child-2"),
        ev_assistant_message("msg-child-2", "child done"),
        ev_completed("resp-child-2"),
    ]);
    let child_completion_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "child"),
        child_completion,
    )
    .await;

    // Parent completion after receiving child output.
    let parent_completion = sse(vec![
        ev_response_created("resp-parent-2"),
        ev_assistant_message("msg-parent-2", "parent done"),
        ev_completed("resp-parent-2"),
    ]);
    let parent_completion_mock = mount_sse_once(&server, parent_completion).await;

    let test = builder
        .build(&server)
        .await
        .expect("create test Codex conversation");

    test.submit_turn("invoke nested delegation chain")
        .await
        .expect("submit turn");

    assert_eq!(child_first_mock.requests().len(), 1);
    assert_eq!(grandchild_mock.requests().len(), 1);
    assert_eq!(child_completion_mock.requests().len(), 1);
    assert_eq!(parent_completion_mock.requests().len(), 1);

    let child_req = &child_first_mock.requests()[0];
    let grandchild_req = &grandchild_mock.requests()[0];

    let child_combined = format!(
        "{}\n{}",
        child_req.instructions(),
        child_req.message_input_texts("user").join("\n")
    );
    let grandchild_combined = format!(
        "{}\n{}",
        grandchild_req.instructions(),
        grandchild_req.message_input_texts("user").join("\n")
    );

    assert_eq!(
        i64::try_from(child_combined.matches("# Worker Context").count()).unwrap(),
        1,
        "expected exactly 1 identity prompt in child request. Got: {child_combined}"
    );
    assert_eq!(
        i64::try_from(grandchild_combined.matches("# Worker Context").count()).unwrap(),
        1,
        "expected exactly 1 identity prompt in grandchild request. Got: {grandchild_combined}"
    );

    let child_ids = parent_session_ids(&child_combined);
    assert_eq!(
        i64::try_from(child_ids.len()).unwrap(),
        1,
        "expected exactly 1 parent session id in child user input. Got: {child_combined}"
    );
    let _root_parent_id = child_ids[0];

    let grandchild_ids = parent_session_ids(&grandchild_combined);
    assert_eq!(
        i64::try_from(grandchild_ids.len()).unwrap(),
        1,
        "expected exactly 1 parent session id in grandchild user input. Got: {grandchild_combined}"
    );
    assert!(
        child_combined.contains("CHILD_SYSTEM_PROMPT_MARKER"),
        "child request should include child system prompt content. Got: {child_combined}"
    );
    assert!(
        grandchild_combined.contains("GRANDCHILD_SYSTEM_PROMPT_MARKER"),
        "grandchild request should include grandchild system prompt content. Got: {grandchild_combined}"
    );
}
