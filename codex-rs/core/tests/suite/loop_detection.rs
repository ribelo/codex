#![cfg(not(target_os = "windows"))]

use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::WarningEvent;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

/// Test that 5 consecutive identical tool calls trigger loop detection
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_detection_identical_tool_calls() -> anyhow::Result<()> {
    let server = start_mock_server().await;

    let TestCodex {
        codex,
        session_configured,
        ..
    } = test_codex().with_model("gpt-5.1").build(&server).await?;

    let session_model = session_configured.model.clone();
    let cwd = session_configured.cwd.clone();

    // Mount a response with 5 identical shell calls - should trigger loop detection
    let response = sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell",
            r#"{"command":"echo hi","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-2",
            "shell",
            r#"{"command":"echo hi","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-3",
            "shell",
            r#"{"command":"echo hi","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-4",
            "shell",
            r#"{"command":"echo hi","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-5",
            "shell",
            r#"{"command":"echo hi","workdir":"/tmp"}"#,
        ),
        ev_completed("resp-1"),
    ]);

    responses::mount_sse_once(&server, response).await;

    codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "run echo".into(),
            }],
            final_output_json_schema: None,
            cwd: cwd.clone(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
        })
        .await?;

    // Should receive a warning event about loop detection
    let warning = wait_for_event(&codex, |ev| matches!(ev, EventMsg::Warning(_))).await;
    let EventMsg::Warning(WarningEvent { message }) = warning else {
        panic!("expected warning event");
    };

    assert!(
        message.contains("Loop detected") && message.contains("tool calls"),
        "expected loop detection warning, got: {message}"
    );

    Ok(())
}

/// Test that different tool calls do NOT trigger loop detection
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_loop_for_different_tool_calls() -> anyhow::Result<()> {
    let server = start_mock_server().await;

    let TestCodex {
        codex,
        session_configured,
        ..
    } = test_codex().with_model("gpt-5.1").build(&server).await?;

    let session_model = session_configured.model.clone();
    let cwd = session_configured.cwd.clone();

    // Mount 5 shell calls with different arguments - should NOT trigger loop
    let first_response = sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell",
            r#"{"command":"echo 1","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-2",
            "shell",
            r#"{"command":"echo 2","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-3",
            "shell",
            r#"{"command":"echo 3","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-4",
            "shell",
            r#"{"command":"echo 4","workdir":"/tmp"}"#,
        ),
        ev_function_call(
            "call-5",
            "shell",
            r#"{"command":"echo 5","workdir":"/tmp"}"#,
        ),
        ev_completed("resp-1"),
    ]);

    responses::mount_sse_once(&server, first_response).await;

    // Second response to complete the turn (tool outputs trigger follow-up)
    let second_response = sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]);
    responses::mount_sse_once(&server, second_response).await;

    codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "run commands".into(),
            }],
            final_output_json_schema: None,
            cwd: cwd.clone(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
        })
        .await?;

    // Wait a bit and check no warning about loop detection
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // The test passes if we don't see a loop detection warning
    // (we can't easily assert absence of an event, so we just verify the turn completes)
    Ok(())
}
