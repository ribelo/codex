#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use codex_core::features::Feature;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use codex_core::sandboxing::SandboxPermissions;
use core_test_support::echo_path;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::sh_path;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use regex_lite::Regex;
use serde_json::Value;
use serde_json::json;

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_tool_unknown_returns_custom_output_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    let call_id = "custom-unsupported";
    let tool_name = "unsupported_tool";

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call(call_id, tool_name, "\"payload\""),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "invoke custom tool",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let item = mock.single_request().custom_tool_call_output(call_id);
    let output = item
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = format!("unsupported custom tool call: {tool_name}");
    assert_eq!(output, expected);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_escalated_permissions_rejected_then_ok() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.1");
    let test = builder.build(&server).await?;

    let echo = echo_path();
    let command = format!("{} 'shell ok'", echo.as_str());
    let call_id_blocked = "shell-blocked";
    let call_id_success = "shell-success";

    let first_args = json!({
        "command": command.clone(),
        "timeout_ms": 1_000,
        "sandbox_permissions": SandboxPermissions::RequireEscalated,
    });
    let second_args = json!({
        "command": command.clone(),
        "timeout_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(
                call_id_blocked,
                "shell_command",
                &serde_json::to_string(&first_args)?,
            ),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let second_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-2"),
            ev_function_call(
                call_id_success,
                "shell_command",
                &serde_json::to_string(&second_args)?,
            ),
            ev_completed("resp-2"),
        ]),
    )
    .await;
    let third_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-3"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "run the shell command",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let policy = AskForApproval::Never;
    let expected_message = format!(
        "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}"
    );

    let blocked_output = second_mock
        .single_request()
        .function_call_output_content_and_success(call_id_blocked)
        .and_then(|(content, _)| content)
        .expect("blocked output string");
    assert_eq!(
        blocked_output, expected_message,
        "unexpected rejection message"
    );

    let success_output = third_mock
        .single_request()
        .function_call_output_content_and_success(call_id_success)
        .and_then(|(content, _)| content)
        .expect("success output string");

    // The shell_command tool now returns freeform output format.
    // Verify exit code 0 and "shell ok" in the output.
    assert!(
        success_output.starts_with("Exit code: 0\n"),
        "expected exit code 0 after rerunning without escalation, got: {success_output}",
    );
    assert!(
        success_output.contains("shell ok"),
        "expected 'shell ok' in output: {success_output}",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sandbox_denied_shell_returns_original_output() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.1-codex");
    let fixture = builder.build(&server).await?;

    let call_id = "sandbox-denied-shell";
    let target_path = fixture.workspace_path("sandbox-denied.txt");
    let sentinel = "sandbox-denied sentinel output";
    let sentinel_with_newline = format!("{sentinel}\n");
    let inner_script = format!(
        "printf {sentinel_with_newline:?}; printf {content:?} > {path:?}",
        content = "sandbox denied",
        path = &target_path
    );
    let command = format!("{} -c '{}'", sh_path(), inner_script);
    let args = json!({
        "command": command.clone(),
        "timeout_ms": 1_000,
    });

    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    ];
    let mock = mount_sse_sequence(&server, responses).await;

    fixture
        .submit_turn_with_policy(
            "run a command that should be denied by the read-only sandbox",
            SandboxPolicy::ReadOnly,
        )
        .await?;

    let output_text = mock
        .function_call_output_text(call_id)
        .context("shell output present")?;
    let exit_code_line = output_text
        .lines()
        .next()
        .context("exit code line present")?;
    let exit_code = exit_code_line
        .strip_prefix("Exit code: ")
        .context("exit code prefix present")?
        .trim()
        .parse::<i32>()
        .context("exit code is integer")?;
    let body = output_text;

    let body_lower = body.to_lowercase();
    // Required for multi-OS.
    let has_denial = body_lower.contains("permission denied")
        || body_lower.contains("operation not permitted")
        || body_lower.contains("read-only file system");
    assert!(
        has_denial,
        "expected sandbox denial details in tool output: {body}"
    );
    assert!(
        body.contains(sentinel),
        "expected sentinel output from command to reach the model: {body}"
    );
    let target_path_str = target_path
        .to_str()
        .context("target path string representation")?;
    assert!(
        body.contains(target_path_str),
        "expected sandbox error to mention denied path: {body}"
    );
    assert!(
        !body_lower.contains("failed in sandbox"),
        "expected original tool output, found fallback message: {body}"
    );
    assert_ne!(
        exit_code, 0,
        "sandbox denial should surface a non-zero exit code"
    );

    Ok(())
}

async fn collect_tools(use_unified_exec: bool) -> Result<Vec<String>> {
    let server = start_mock_server().await;

    let responses = vec![sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ])];
    let mock = mount_sse_sequence(&server, responses).await;

    let mut builder = test_codex().with_config(move |config| {
        if use_unified_exec {
            config.features.enable(Feature::UnifiedExec);
        } else {
            config.features.disable(Feature::UnifiedExec);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_policies(
        "list tools",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let first_body = mock.single_request().body_json();
    Ok(tool_names(&first_body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_spec_toggle_end_to_end() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let tools_disabled = collect_tools(false).await?;
    assert!(
        !tools_disabled.iter().any(|name| name == "exec_command"),
        "tools list should not include exec_command when disabled: {tools_disabled:?}"
    );
    assert!(
        !tools_disabled.iter().any(|name| name == "write_stdin"),
        "tools list should not include write_stdin when disabled: {tools_disabled:?}"
    );

    let tools_enabled = collect_tools(true).await?;
    assert!(
        tools_enabled.iter().any(|name| name == "exec_command"),
        "tools list should include exec_command when enabled: {tools_enabled:?}"
    );
    assert!(
        tools_enabled.iter().any(|name| name == "write_stdin"),
        "tools list should include write_stdin when enabled: {tools_enabled:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_timeout_includes_timeout_prefix_and_metadata() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.1");
    let test = builder.build(&server).await?;

    let call_id = "shell-timeout";
    let timeout_ms = 50u64;
    let sh = sh_path();
    let args = json!({
        "command": format!("{} -c 'yes line | head -n 400; sleep 1'", sh),
        "timeout_ms": timeout_ms,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let second_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "run a long command",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let timeout_item = second_mock.single_request().function_call_output(call_id);

    let output_str = timeout_item
        .get("output")
        .and_then(Value::as_str)
        .expect("timeout output string");

    // The shell_command tool returns freeform output. For a timeout we expect:
    // 1. Exit code 124 (standard timeout exit code), OR
    // 2. "command timed out" message in the output
    // Note: The "command timed out" prefix may be omitted if the process produced output
    // before being killed; in that case we just verify exit code 124.
    let has_exit_124 = output_str.starts_with("Exit code: 124\n");
    let has_timeout_message = output_str.contains("command timed out");
    let has_signal_error = output_str.contains("execution error:") && output_str.contains("signal");

    assert!(
        has_exit_124 || has_timeout_message || has_signal_error,
        "expected timeout indication in output: {output_str}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_timeout_handles_background_grandchild_stdout() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.1").with_config(|config| {
        config.sandbox_policy = SandboxPolicy::DangerFullAccess;
    });
    let test = builder.build(&server).await?;

    let call_id = "shell-grandchild-timeout";
    let pid_path = test.cwd.path().join("grandchild_pid.txt");
    let script_path = test.cwd.path().join("spawn_detached.py");
    let sh = sh_path();
    let script = format!(
        r#"import subprocess
import time
from pathlib import Path

# Spawn a detached grandchild that inherits stdout/stderr so the pipe stays open.
proc = subprocess.Popen([{sh:?}, "-c", "sleep 60"], start_new_session=True)
Path({pid_path:?}).write_text(str(proc.pid))
time.sleep(60)
"#
    );
    fs::write(&script_path, script)?;

    let args = json!({
        "command": format!("python3 {}", script_path.to_string_lossy()),
        "timeout_ms": 200,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let second_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    let start = Instant::now();
    let output_str = tokio::time::timeout(Duration::from_secs(10), async {
        test.submit_turn_with_policies(
            "run a command with a detached grandchild",
            AskForApproval::Never,
            SandboxPolicy::DangerFullAccess,
        )
        .await?;
        let timeout_item = second_mock.single_request().function_call_output(call_id);
        timeout_item
            .get("output")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("timeout output string")
    })
    .await
    .context("exec call should not hang waiting for grandchild pipes to close")??;
    let elapsed = start.elapsed();

    // The shell_command tool returns freeform output. Check for:
    // 1. Exit code 124 in freeform format, OR
    // 2. "command timed out" or "timeout" in the output
    let exit_124_pattern = r"(?m)^Exit code: 124$";
    let exit_124_regex = Regex::new(exit_124_pattern)?;
    let timeout_pattern = r"(?is)command timed out|timeout";
    let timeout_regex = Regex::new(timeout_pattern)?;

    assert!(
        exit_124_regex.is_match(&output_str) || timeout_regex.is_match(&output_str),
        "expected timeout indication in output: {output_str}"
    );

    assert!(
        elapsed < Duration::from_secs(9),
        "command should return shortly after timeout even with live grandchildren: {elapsed:?}"
    );

    if let Ok(pid_str) = fs::read_to_string(&pid_path)
        && let Ok(pid) = pid_str.trim().parse::<libc::pid_t>()
    {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_spawn_failure_truncates_exec_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|cfg| {
        cfg.sandbox_policy = SandboxPolicy::DangerFullAccess;
    });
    let test = builder.build(&server).await?;

    let call_id = "shell-spawn-failure";
    let bogus_component = "missing-bin-".repeat(700);
    let bogus_exe = test
        .cwd
        .path()
        .join(bogus_component)
        .to_string_lossy()
        .into_owned();

    let args = json!({
        "command": bogus_exe,
        "timeout_ms": 1_000,
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let second_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "spawn a missing binary",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let failure_item = second_mock.single_request().function_call_output(call_id);

    let output = failure_item
        .get("output")
        .and_then(Value::as_str)
        .expect("spawn failure output string");

    // The shell_command tool returns freeform output. For spawn failures we expect either:
    // 1. An "execution error:" message from the spawn failure
    // 2. A shell error message like "File name too long" or "No such file"
    // Both cases should have non-zero exit code and size under 10 KiB.
    let spawn_error_pattern = r#"(?s)^Exit code: -?\d+
Wall time: [0-9]+(?:\.[0-9]+)? seconds
Output:
execution error: .*$"#;
    let spawn_truncated_pattern = r#"(?s)^Exit code: -?\d+
Wall time: [0-9]+(?:\.[0-9]+)? seconds
Total output lines: \d+
Output:

execution error: .*$"#;
    // Shell may report the error directly (e.g., "File name too long", "No such file or directory")
    let shell_error_pattern = r#"(?s)^Exit code: -?\d+
Wall time: [0-9]+(?:\.[0-9]+)? seconds
(?:Total output lines: \d+
)?Output:
.*(?:File name too long|No such file|not found|cannot find).*$"#;

    let spawn_error_regex = Regex::new(spawn_error_pattern)?;
    let spawn_truncated_regex = Regex::new(spawn_truncated_pattern)?;
    let shell_error_regex = Regex::new(shell_error_pattern)?;

    // Verify the output matches one of the expected patterns
    assert!(
        spawn_error_regex.is_match(output)
            || spawn_truncated_regex.is_match(output)
            || shell_error_regex.is_match(output),
        "expected spawn failure output pattern, got: {output}"
    );

    // Verify non-zero exit code
    assert!(
        output.contains("Exit code:") && !output.starts_with("Exit code: 0\n"),
        "expected non-zero exit code for spawn failure"
    );

    assert!(output.len() <= 10 * 1024);

    Ok(())
}
