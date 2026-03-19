use super::*;

use serde_json::Value;
use tempfile::TempDir;
use tokio::process::Command;

async fn run_git(repo_path: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .await
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={:?} stderr={:?}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "spawns real git commands and can trigger local credential/signing prompts on this fork"]
async fn build_turn_metadata_header_includes_has_changes_for_clean_repo() {
    let temp_dir = TempDir::new().expect("temp dir");
    let repo_path = temp_dir.path().join("repo");
    std::fs::create_dir_all(&repo_path).expect("create repo");

    run_git(&repo_path, &["init"]).await;
    run_git(&repo_path, &["config", "user.name", "Test User"]).await;
    run_git(&repo_path, &["config", "user.email", "test@example.com"]).await;
    run_git(&repo_path, &["config", "commit.gpgsign", "false"]).await;

    std::fs::write(repo_path.join("README.md"), "hello").expect("write file");
    run_git(&repo_path, &["add", "."]).await;
    run_git(&repo_path, &["commit", "-m", "initial"]).await;

    let header = build_turn_metadata_header(&repo_path, Some("none"))
        .await
        .expect("header");
    let parsed: Value = serde_json::from_str(&header).expect("valid json");
    let workspace = parsed
        .get("workspaces")
        .and_then(Value::as_object)
        .and_then(|workspaces| workspaces.values().next())
        .cloned()
        .expect("workspace");

    assert_eq!(
        workspace.get("has_changes").and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn turn_metadata_state_uses_platform_sandbox_tag() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().to_path_buf();
    let sandbox_policy = SandboxPolicy::new_read_only_policy();

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "turn-a".to_string(),
        cwd,
        &sandbox_policy,
        WindowsSandboxLevel::Disabled,
    );

    let header = state.current_header_value().expect("header");
    let json: Value = serde_json::from_str(&header).expect("json");
    let sandbox_name = json.get("sandbox").and_then(Value::as_str);
    let session_id = json.get("session_id").and_then(Value::as_str);

    let expected_sandbox = sandbox_tag(&sandbox_policy, WindowsSandboxLevel::Disabled);
    assert_eq!(sandbox_name, Some(expected_sandbox));
    assert_eq!(session_id, Some("session-a"));
}
