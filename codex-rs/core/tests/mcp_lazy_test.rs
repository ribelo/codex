use codex_core::config::types::McpServerConfig;
use codex_core::config::types::McpServerTransportConfig;
use codex_core::mcp_connection_manager::McpConnectionManager;
use codex_core::protocol::EventMsg;
use codex_core::protocol::McpStartupStatus;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_mcp_lazy_init_background() {
    let (tx, rx) = async_channel::unbounded();
    let mut manager = McpConnectionManager::default();
    let cancel_token = CancellationToken::new();

    // Configure a dummy server that sleeps
    let mut configs = HashMap::new();
    configs.insert(
        "sleepy".to_string(),
        McpServerConfig {
            enabled: true,
            transport: McpServerTransportConfig::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 0.1".to_string()],
                env: None,
                env_vars: vec![],
                cwd: None,
            },
            startup_timeout_sec: Some(Duration::from_secs(1)),
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
        },
    );

    manager.configure(
        configs,
        Default::default(),
        HashMap::new(),
        tx,
        cancel_token,
    );

    // Call ensure_started
    manager.ensure_started("sleepy").await;

    // Check events
    let event = rx.recv().await.expect("should receive starting event");
    if let EventMsg::McpStartupUpdate(ev) = event.msg {
        assert_eq!(ev.server, "sleepy");
        assert!(matches!(ev.status, McpStartupStatus::Starting));
    } else {
        panic!("unexpected event: {event:?}");
    }

    // Wait for failure (handshake timeout or process exit)
    let event = rx.recv().await.expect("should receive result event");
    if let EventMsg::McpStartupUpdate(ev) = event.msg {
        assert_eq!(ev.server, "sleepy");
        if let McpStartupStatus::Failed { .. } = ev.status {
            // expected
        } else {
            panic!("expected failure, got {:?}", ev.status);
        }
    } else {
        panic!("unexpected event: {event:?}");
    }
}
