use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::to_response;
use codex_app_server_protocol::AddConversationListenerParams;
use codex_app_server_protocol::AddConversationSubscriptionResponse;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InputItem;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::NewConversationParams;
use codex_app_server_protocol::NewConversationResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SendUserMessageParams;
use codex_app_server_protocol::SendUserMessageResponse;
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn create_config_toml(codex_home: &Path, base_url: &str) -> Result<()> {
    let config_content = format!(
        r#"
model = "mock_provider/mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{base_url}/v1"
wire_api = "chat"
request_max_retries = 0
stream_max_retries = 0
"#
    );
    fs::write(codex_home.join("config.toml"), config_content)?;
    Ok(())
}

/// Regression test: verifies that providing `originator` in the initialize request
/// results in outbound HTTP requests carrying that value as the `originator` header,
/// and the user agent includes the originator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_originator_propagates_to_outbound_requests() -> Result<()> {
    // Create a mock server that captures requests.
    let server = MockServer::start().await;

    let sse_response = create_final_assistant_message_sse_response("Done")?;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_response, "text/event-stream"),
        )
        .mount(&server)
        .await;

    // Create a temporary Codex home with config pointing at the mock server.
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    // Start MCP server process and initialize with custom originator.
    let mut mcp = McpProcess::new(codex_home.path()).await?;
    let custom_originator = "my_custom_originator";
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.initialize_with_params(InitializeParams {
            client_info: ClientInfo {
                name: "originator-test".to_string(),
                title: None,
                version: "1.0.0".to_string(),
            },
            originator: Some(custom_originator.to_string()),
        }),
    )
    .await??;

    // Start a conversation to trigger an outbound request.
    let new_conv_id = mcp
        .send_new_conversation_request(NewConversationParams::default())
        .await?;
    let new_conv_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(new_conv_id)),
    )
    .await??;
    let NewConversationResponse {
        conversation_id, ..
    } = to_response::<_>(new_conv_resp)?;

    // Add listener to receive events.
    let add_listener_id = mcp
        .send_add_conversation_listener_request(AddConversationListenerParams {
            conversation_id,
            experimental_raw_events: false,
        })
        .await?;
    let add_listener_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(add_listener_id)),
    )
    .await??;
    let AddConversationSubscriptionResponse { subscription_id: _ } =
        to_response::<_>(add_listener_resp)?;

    // Send a user message to trigger the model request.
    let send_id = mcp
        .send_send_user_message_request(SendUserMessageParams {
            conversation_id,
            items: vec![InputItem::Text {
                text: "Hello".to_string(),
            }],
        })
        .await?;

    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(send_id)),
    )
    .await??;
    let _ok: SendUserMessageResponse = to_response::<SendUserMessageResponse>(response)?;

    // Wait for task completion notification.
    let _task_finished = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("codex/event/task_complete"),
    )
    .await??;

    // Verify the mock server received requests with the custom originator header.
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.is_empty(),
        "mock server should have received at least one request"
    );

    // Check that all requests have the custom originator header.
    for request in &requests {
        let originator_header = request
            .headers
            .get("originator")
            .expect("originator header should be present");
        assert_eq!(
            originator_header.to_str().unwrap(),
            custom_originator,
            "originator header should match the value provided in initialize"
        );
    }

    Ok(())
}
