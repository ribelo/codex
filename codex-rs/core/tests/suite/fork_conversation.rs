use codex_core::CodexAuth;
use codex_core::ConversationManager;
use codex_core::ModelProviderInfo;
use codex_core::NewConversation;
use codex_core::built_in_model_providers;
use codex_core::parse_turn_item;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EntryKind;
use codex_protocol::protocol::LogEntry;
use codex_protocol::user_input::UserInput;
use core_test_support::load_default_config_for_test;
use core_test_support::skip_if_no_network;
use core_test_support::wait_for_event;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// Build minimal SSE stream with completed marker using the JSON fixture.
fn sse_completed(id: &str) -> String {
    core_test_support::load_sse_fixture_with_id("tests/fixtures/completed_template.json", id)
}

fn read_user_messages(path: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).expect("read session file");
    let mut messages = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let entry = serde_json::from_str::<LogEntry>(line).expect("log entry line");
        if let EntryKind::ResponseItem { item } = entry.kind
            && let Some(TurnItem::UserMessage(msg)) = parse_turn_item(&item)
        {
            messages.push(msg.message().to_string());
        }
    }
    messages
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_conversation_twice_drops_to_first_message() {
    skip_if_no_network!();

    // Start a mock server that completes three turns.
    let server = MockServer::start().await;
    let sse = sse_completed("resp");
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse.clone(), "text/event-stream");

    // Expect three calls to /v1/responses – one per user input.
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(3)
        .mount(&server)
        .await;

    // Configure Codex to use the mock server.
    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    let home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&home);
    config.model_provider = model_provider.clone();
    let config_for_fork = config.clone();

    let conversation_manager = ConversationManager::with_models_provider(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        home.path().to_path_buf(),
    );
    let NewConversation {
        conversation: codex,
        ..
    } = conversation_manager
        .new_conversation(config)
        .await
        .expect("create conversation");

    // Send three user messages; wait for three completed turns.
    for text in ["first", "second", "third"] {
        codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: text.to_string(),
                }],
            })
            .await
            .unwrap();
        let _ = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
    }

    // Request history from the base conversation to obtain rollout path.
    let base_path = codex.rollout_path();

    let base_user_messages = read_user_messages(&base_path);
    pretty_assertions::assert_eq!(
        base_user_messages,
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );

    // After dropping again (n=1 on fork1), compute expected relative to fork1's rollout.

    // Fork once with n=1 → drops the last user input and everything after.
    let NewConversation {
        conversation: codex_fork1,
        ..
    } = conversation_manager
        .fork_conversation(1, config_for_fork.clone(), base_path.clone())
        .await
        .expect("fork 1");

    let fork1_path = codex_fork1.rollout_path();

    let fork1_user_messages = read_user_messages(&fork1_path);
    pretty_assertions::assert_eq!(fork1_user_messages, vec!["first".to_string()]);

    // Fork again with n=0 → drops the first user message, leaving none.
    let NewConversation {
        conversation: codex_fork2,
        ..
    } = conversation_manager
        .fork_conversation(0, config_for_fork.clone(), fork1_path.clone())
        .await
        .expect("fork 2");

    let fork2_path = codex_fork2.rollout_path();
    let fork2_user_messages = read_user_messages(&fork2_path);
    assert!(
        fork2_user_messages.is_empty(),
        "expected fork2 to have no user messages, got {fork2_user_messages:?}"
    );
}
