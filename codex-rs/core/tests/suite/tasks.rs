use std::collections::HashMap;

use codex_core::config::types::TaskConfigToml;
use codex_core::config::types::TaskDifficulty;
use codex_core::config::types::TaskDifficultyStrategy;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::get_responses_requests;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;
use wiremock::matchers::header;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_difficulty_selects_override_model() {
    skip_if_no_network!();

    let server = start_mock_server().await;

    let task_args = json!({
        "description": "Rust task",
        "prompt": "Do something",
        "task_type": "rust",
        "difficulty": "large",
    });
    let parent_response = sse(vec![
        ev_response_created("resp-parent-1"),
        ev_function_call("call-task-1", "task", &task_args.to_string()),
        ev_completed("resp-parent-1"),
    ]);

    let subagent_response = sse(vec![
        ev_response_created("resp-subagent-1"),
        ev_assistant_message("msg-subagent-1", "Done."),
        ev_completed("resp-subagent-1"),
    ]);

    let parent_completion = sse(vec![
        ev_response_created("resp-parent-2"),
        ev_assistant_message("msg-parent-2", "Parent done."),
        ev_completed("resp-parent-2"),
    ]);
    mount_sse_sequence(
        &server,
        vec![parent_response, subagent_response, parent_completion],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        let mut difficulty = HashMap::new();
        difficulty.insert(
            TaskDifficulty::Large,
            TaskDifficultyStrategy {
                model: Some("openai/test-gpt-5.1-codex-large".to_string()),
                reasoning_effort: None,
            },
        );

        config.tasks = vec![TaskConfigToml {
            task_type: "rust".to_string(),
            description: Some("Rust code changes".to_string()),
            skills: None,
            model: Some("openai/test-gpt-5.1-codex-medium".to_string()),
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: Some(difficulty),
        }];
    });
    let TestCodex {
        codex,
        cwd,
        session_configured,
        ..
    } = builder
        .build(&server)
        .await
        .expect("create test Codex conversation");

    codex
        .submit(codex_core::protocol::Op::UserTurn {
            items: vec![codex_protocol::user_input::UserInput::Text {
                text: "invoke the rust task".into(),
            }],
            final_output_json_schema: None,
            cwd: cwd.path().to_path_buf(),
            approval_policy: codex_core::protocol::AskForApproval::Never,
            sandbox_policy: codex_core::protocol::SandboxPolicy::DangerFullAccess,
            model: session_configured.model.clone(),
            effort: None,
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
        })
        .await
        .expect("submit turn");

    let completed = timeout(Duration::from_secs(10), async {
        loop {
            let event = codex.next_event().await.expect("event");
            if matches!(event.msg, codex_core::protocol::EventMsg::TaskComplete(_)) {
                break;
            }
        }
    })
    .await;
    if completed.is_err() {
        let requests = get_responses_requests(&server).await;
        panic!(
            "timeout waiting for TaskComplete; saw {} requests",
            requests.len()
        );
    }

    let requests = get_responses_requests(&server).await;
    let subagent_request = requests
        .iter()
        .find_map(|req| {
            let wrapped = ResponsesRequest::from_wiremock(req);
            let is_subagent = wrapped.header("x-openai-subagent").as_deref() == Some("rust");
            is_subagent.then_some(wrapped)
        })
        .expect("subagent request");

    let body = subagent_request.body_json();
    assert_eq!(
        body.get("model").and_then(serde_json::Value::as_str),
        Some("test-gpt-5.1-codex-large")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_injects_skills_into_prompt() {
    skip_if_no_network!();

    let server = start_mock_server().await;

    let task_args = json!({
        "description": "Skill task",
        "prompt": "Use the skill",
        "task_type": "skilltest",
    });
    let parent_response = sse(vec![
        ev_response_created("resp-parent-1"),
        ev_function_call("call-task-1", "task", &task_args.to_string()),
        ev_completed("resp-parent-1"),
    ]);
    let parent_mock = mount_sse_once(&server, parent_response).await;

    let subagent_response = sse(vec![
        ev_response_created("resp-subagent-1"),
        ev_assistant_message("msg-subagent-1", "Ok."),
        ev_completed("resp-subagent-1"),
    ]);
    let subagent_mock = mount_sse_once_match(
        &server,
        header("x-openai-subagent", "skilltest"),
        subagent_response,
    )
    .await;

    let parent_completion = sse(vec![
        ev_response_created("resp-parent-2"),
        ev_assistant_message("msg-parent-2", "Parent done."),
        ev_completed("resp-parent-2"),
    ]);
    let parent_completion_mock = mount_sse_once(&server, parent_completion).await;

    let mut builder = test_codex()
        .with_pre_build_hook(|codex_home| {
            let path = codex_home.join("skills/demo-skill/SKILL.md");
            std::fs::create_dir_all(path.parent().expect("skill dir")).expect("create skill dir");
            std::fs::write(&path, "Demo skill contents.").expect("write skill");
        })
        .with_config(|config| {
            config.tasks = vec![TaskConfigToml {
                task_type: "skilltest".to_string(),
                description: Some("Skill test task".to_string()),
                skills: Some(vec!["demo-skill".to_string()]),
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

    test.submit_turn("invoke the skill task")
        .await
        .expect("submit turn");

    assert_eq!(parent_mock.requests().len(), 1);
    assert_eq!(subagent_mock.requests().len(), 1);
    assert_eq!(parent_completion_mock.requests().len(), 1);

    let subagent_user_input = subagent_mock.requests()[0]
        .message_input_texts("user")
        .join("\n");
    assert!(
        subagent_user_input.contains("<name>demo-skill</name>"),
        "expected skill to be injected. Got: {subagent_user_input}"
    );
    assert!(
        subagent_user_input.contains("Demo skill contents."),
        "expected skill contents to be injected. Got: {subagent_user_input}"
    );
}
