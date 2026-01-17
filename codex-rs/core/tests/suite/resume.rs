use anyhow::Result;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_includes_initial_messages_from_rollout_events() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial.session_configured.rollout_path.clone();

    let initial_sse = sse(vec![
        ev_response_created("resp-initial"),
        ev_assistant_message("msg-1", "Completed first turn"),
        ev_completed("resp-initial"),
    ]);
    mount_sse_once(&server, initial_sse).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record some messages".into(),
            }],
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TaskComplete(_))).await;

    let resumed = builder.resume(&server, home, rollout_path).await?;
    let initial_messages = resumed
        .session_configured
        .initial_messages
        .expect("expected initial messages to be present for resumed session");
    let user_idx = initial_messages
        .iter()
        .position(
            |m| matches!(m, EventMsg::UserMessage(msg) if msg.message == "Record some messages"),
        )
        .expect("missing initial UserMessage");
    let assistant_idx = initial_messages
        .iter()
        .position(
            |m| matches!(m, EventMsg::AgentMessage(msg) if msg.message == "Completed first turn"),
        )
        .expect("missing initial AgentMessage");
    assert!(
        user_idx < assistant_idx,
        "expected user message before assistant message"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_includes_initial_messages_from_reasoning_events() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|_| {});
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial.session_configured.rollout_path.clone();

    let initial_sse = sse(vec![
        ev_response_created("resp-initial"),
        ev_reasoning_item("reason-1", &["Summarized step"], &["raw detail"]),
        ev_assistant_message("msg-1", "Completed reasoning turn"),
        ev_completed("resp-initial"),
    ]);
    mount_sse_once(&server, initial_sse).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record reasoning messages".into(),
            }],
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TaskComplete(_))).await;

    let resumed = builder.resume(&server, home, rollout_path).await?;
    let initial_messages = resumed
        .session_configured
        .initial_messages
        .expect("expected initial messages to be present for resumed session");

    let first_user = initial_messages
        .iter()
        .find_map(|m| match m {
            EventMsg::UserMessage(msg) => Some(msg),
            _ => None,
        })
        .expect("missing UserMessage");
    assert_eq!(first_user.message, "Record reasoning messages");

    let raw = initial_messages
        .iter()
        .find_map(|m| match m {
            EventMsg::AgentReasoningRawContent(msg) => Some(msg),
            _ => None,
        })
        .expect("missing AgentReasoningRawContent");
    assert_eq!(raw.text, "raw detail");

    let assistant_message = initial_messages
        .iter()
        .find_map(|m| match m {
            EventMsg::AgentMessage(msg) => Some(msg),
            _ => None,
        })
        .expect("missing AgentMessage");
    assert_eq!(assistant_message.message, "Completed reasoning turn");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_includes_tool_outputs_in_initial_messages() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial.session_configured.rollout_path.clone();

    let file_path = initial.cwd_path().join("replay_tool.txt");
    std::fs::write(&file_path, "replay-tool-output\n")?;
    let args = serde_json::json!({ "file_path": file_path.display().to_string() });
    let function_arguments = serde_json::to_string(&args)?;
    let call_id = "call-read-file";

    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-initial"),
                ev_function_call(call_id, "read_file", &function_arguments),
                ev_completed("resp-initial"),
            ]),
            sse(vec![ev_completed("resp-followup")]),
        ],
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record tool output".into(),
            }],
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TaskComplete(_))).await;

    let resumed = builder.resume(&server, home, rollout_path).await?;
    let initial_messages = resumed
        .session_configured
        .initial_messages
        .expect("expected initial messages to be present for resumed session");

    let mut saw_call = false;
    let mut saw_output = false;
    for msg in &initial_messages {
        let EventMsg::RawResponseItem(ev) = msg else {
            continue;
        };
        match &ev.item {
            ResponseItem::FunctionCall {
                call_id: cid,
                name,
                arguments: actual_arguments,
                ..
            } if cid == call_id => {
                saw_call = true;
                assert_eq!(name, "read_file");
                assert_eq!(actual_arguments, &function_arguments);
            }
            ResponseItem::FunctionCallOutput {
                call_id: cid,
                output,
            } if cid == call_id => {
                saw_output = true;
                assert!(
                    output.content.contains("replay-tool-output"),
                    "expected tool output to be present"
                );
            }
            _ => {}
        }
    }

    assert!(saw_call, "missing RawResponseItem FunctionCall");
    assert!(saw_output, "missing RawResponseItem FunctionCallOutput");

    Ok(())
}
