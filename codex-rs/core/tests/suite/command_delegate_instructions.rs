use codex_core::protocol::Op;
use serde_json::Value;

use core_test_support::test_codex::TestCodexHarness;

#[tokio::test]
async fn delegate_command_preserves_base_instructions() -> anyhow::Result<()> {
    // 1. Setup harness with custom base instructions
    let base_instructions = "CUSTOM BASE INSTRUCTIONS";
    let harness = TestCodexHarness::with_config(move |config| {
        config.base_instructions = Some(base_instructions.to_string());
    })
    .await?;

    // 2. Submit DelegateCommand Op
    let op = Op::DelegateCommand {
        description: "test command".to_string(),
        prompt: "echo hello".to_string(),
        agent: None,
        profile: None,
    };

    // We need to submit this op. TestCodexHarness.submit() does UserTurn.
    // We can access harness.test().codex.submit(op)
    harness.test().codex.submit(op).await?;

    // 3. Wait for the task to complete.
    core_test_support::wait_for_event(&harness.test().codex, |event| {
        matches!(event, codex_core::protocol::EventMsg::TaskComplete(_))
    })
    .await;

    // 4. Inspect the request bodies sent to the mock server
    let bodies = harness.request_bodies().await;

    // Find the request that corresponds to the command execution
    // It should be a chat completion request.
    if bodies.is_empty() {
        panic!("No requests recorded");
    }

    let request = bodies.last().unwrap();
    // println!("Request body: {:?}", request);

    // Check if instructions contains the base instructions.
    // The field might be 'instructions' or 'messages' depending on the API.
    let content = if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        instructions
    } else if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        let system_message = messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            .expect("Should have a system message");
        system_message
            .get("content")
            .and_then(Value::as_str)
            .expect("content")
    } else {
        panic!("Neither instructions nor messages found in request");
    };

    assert!(
        content.contains(base_instructions),
        "System prompt should contain base instructions. Got: {content}"
    );

    Ok(())
}

#[tokio::test]
async fn delegate_command_uses_model_instructions_when_no_override() -> anyhow::Result<()> {
    // 1. Setup harness WITHOUT custom base instructions (None)
    // This should cause the model-specific instructions to be used
    let harness = TestCodexHarness::with_config(move |config| {
        config.base_instructions = None;
    })
    .await?;

    // 2. Submit DelegateCommand Op
    let op = Op::DelegateCommand {
        description: "test command".to_string(),
        prompt: "echo hello".to_string(),
        agent: None,
        profile: None,
    };

    harness.test().codex.submit(op).await?;

    // 3. Wait for the task to complete.
    core_test_support::wait_for_event(&harness.test().codex, |event| {
        matches!(event, codex_core::protocol::EventMsg::TaskComplete(_))
    })
    .await;

    // 4. Inspect the request bodies sent to the mock server
    let bodies = harness.request_bodies().await;

    if bodies.is_empty() {
        panic!("No requests recorded");
    }

    let request = bodies.last().unwrap();

    // Check that instructions field exists and is non-empty
    // (model-specific instructions should be used)
    let content = if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
        instructions
    } else if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        let system_message = messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            .expect("Should have a system message");
        system_message
            .get("content")
            .and_then(Value::as_str)
            .expect("content")
    } else {
        panic!("Neither instructions nor messages found in request");
    };

    assert!(
        !content.is_empty(),
        "Instructions should not be empty when using model defaults"
    );

    Ok(())
}
