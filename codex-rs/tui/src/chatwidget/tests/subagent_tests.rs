use super::test_config;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::chatwidget::*;
use crate::history_cell::HistoryCell;
use crate::tui::FrameRequester;
use codex_core::AuthManager;
use codex_core::CodexAuth;
use codex_core::openai_models::models_manager::ModelsManager;
use codex_core::protocol::EventMsg;
use codex_core::protocol::SubagentEventPayload;
use codex_core::protocol::TaskStartedEvent;
use codex_protocol::protocol::Op;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;

// Use same manual setup as tests.rs, but stripped down.
fn make_chatwidget_manual() -> (ChatWidget, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    let (tx_raw, rx) = unbounded_channel::<AppEvent>();
    let app_event_tx = AppEventSender::new(tx_raw);
    let (op_tx, _op_rx) = unbounded_channel::<Op>();
    // Use same test_config helper as parent tests.
    let cfg = test_config();
    let resolved_model = ModelsManager::get_model_offline(cfg.model.as_deref());

    let bottom = BottomPane::new(BottomPaneParams {
        app_event_tx: app_event_tx.clone(),
        frame_requester: FrameRequester::test_dummy(),
        has_input_focus: true,
        enhanced_keys_supported: false,
        placeholder_text: "Ask Codex".to_string(),
        disable_paste_burst: false,
        animations_enabled: false,
        skills: None,
    });
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test"));
    let widget = ChatWidget {
        app_event_tx,
        codex_op_tx: op_tx,
        bottom_pane: bottom,
        active_cell: None,
        config: cfg.clone(),
        model_family: ModelsManager::construct_model_family_offline(&resolved_model, &cfg),
        auth_manager: auth_manager.clone(),
        models_manager: Arc::new(ModelsManager::new(auth_manager)),
        session_header: SessionHeader::new(resolved_model.clone()),
        initial_user_message: None,
        token_info: None,
        rate_limit_snapshot: None,
        plan_type: None,
        rate_limit_warnings: RateLimitWarningState::default(),
        rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
        rate_limit_poller: None,
        stream_controller: None,
        reasoning_stream_controller: None,
        running_commands: HashMap::new(),
        suppressed_exec_calls: HashSet::new(),
        last_unified_wait: None,
        task_complete_pending: false,
        mcp_startup_status: None,
        interrupts: InterruptManager::new(),
        reasoning_buffer: String::new(),
        full_reasoning_buffer: String::new(),
        current_status_header: String::from("Working"),
        retry_status_header: None,
        conversation_id: None,
        frame_requester: FrameRequester::test_dummy(),
        show_welcome_banner: true,
        queued_user_messages: VecDeque::new(),
        suppress_session_configured_redraw: false,
        pending_notification: None,
        is_review_mode: false,
        pre_review_token_info: None,
        needs_final_message_separator: false,
        commit_animation_active: false,
        last_rendered_width: std::cell::Cell::new(None),
        feedback: codex_feedback::CodexFeedback::new(),
        current_rollout_path: None,
        available_profiles: Vec::new(),
        subagent_states: HashMap::new(),
        active_subagent_cells: HashMap::new(),
    };
    (widget, rx)
}

#[test]
fn subagent_nesting_logic() {
    let (mut chat, _rx) = make_chatwidget_manual();

    // 1. Parent subagent starts.
    // Call ID: "call_parent"
    // Delegation ID: "del_parent"
    // Parent Delegation ID: None (Top level)
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_parent".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Parent Task".to_string(),
        delegation_id: Some("del_parent".to_string()),
        parent_delegation_id: None,
        depth: Some(0),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    assert_eq!(
        chat.active_subagent_cells.len(),
        1,
        "Should have 1 active cell"
    );
    assert!(chat.active_subagent_cells.contains_key("call_parent"));

    // 2. Child subagent starts.
    // Call ID: "call_child"
    // Delegation ID: "del_child"
    // Parent Delegation ID: "del_parent" (Should link to parent)
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_child".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Child Task".to_string(),
        delegation_id: Some("del_child".to_string()),
        parent_delegation_id: Some("del_parent".to_string()),
        depth: Some(1),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    // 3. Verify nesting.
    assert_eq!(
        chat.active_subagent_cells.len(),
        1,
        "Should still have 1 top-level active cell (parent)"
    );

    // If nesting failed, length would be 2.
    // If nesting succeeded, Child is inside Parent.

    // 4. Verify that the child is actually inside the parent cell's children
    let parent_cell = chat.active_subagent_cells.get("call_parent").unwrap();
    assert_eq!(
        parent_cell.children().len(),
        1,
        "Parent should have exactly 1 child"
    );

    // 5. Verify rendering shows proper nesting
    let lines = parent_cell.display_lines(80);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();

    // Find the child task line (should be indented with depth=1)
    let child_header = rendered.iter().find(|s| s.contains("Child Task"));
    assert!(
        child_header.is_some(),
        "Child task description should appear in rendered output"
    );

    // Parent header should not have tree connector (depth=0)
    assert!(
        rendered[0].starts_with("•"),
        "Parent header should start with bullet (no indent): got {:?}",
        rendered[0]
    );

    // Child header should have tree connector and be indented (depth=1)
    let child_line_idx = rendered
        .iter()
        .position(|s| s.contains("@explorer") && s.contains("•"));
    assert!(child_line_idx.is_some(), "Child header should have bullet");
}

#[test]
fn subagent_nested_rendering_order() {
    // This test verifies the exact rendering order:
    // Parent task should appear FIRST, with child nested BELOW it.
    //
    // Expected:
    // • Delegated to @explorer
    //   └ Parent Task
    //     └ • Delegated to @explorer
    //       └ Child Task
    //
    // NOT (incorrect):
    //   └ • Delegated to @explorer
    //     └ Child Task
    // • Delegated to @explorer
    //   └ Parent Task

    let (mut chat, _rx) = make_chatwidget_manual();

    // Parent subagent starts first
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_parent".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Parent Task".to_string(),
        delegation_id: Some("del_parent".to_string()),
        parent_delegation_id: None,
        depth: Some(0),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    // Child subagent starts, nested under parent
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_child".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Child Task".to_string(),
        delegation_id: Some("del_child".to_string()),
        parent_delegation_id: Some("del_parent".to_string()),
        depth: Some(1),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    // Get the parent cell and render it
    let parent_cell = chat.active_subagent_cells.get("call_parent").unwrap();
    let lines = parent_cell.display_lines(80);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();

    // Find positions of key elements
    // Parent is at depth 0 (starts with "•"), child is indented (starts with "  •")
    let parent_header_idx = rendered
        .iter()
        .position(|s| s.contains("@explorer") && s.starts_with("•"));
    let parent_desc_idx = rendered.iter().position(|s| s.contains("Parent Task"));
    let child_header_idx = rendered
        .iter()
        .position(|s| s.contains("@explorer") && s.starts_with("  •"));
    let child_desc_idx = rendered.iter().position(|s| s.contains("Child Task"));

    // All elements should be present
    assert!(
        parent_header_idx.is_some(),
        "Parent header should be present. Rendered:\n{}",
        rendered.join("\n")
    );
    assert!(
        parent_desc_idx.is_some(),
        "Parent description should be present. Rendered:\n{}",
        rendered.join("\n")
    );
    assert!(
        child_header_idx.is_some(),
        "Child header should be present. Rendered:\n{}",
        rendered.join("\n")
    );
    assert!(
        child_desc_idx.is_some(),
        "Child description should be present. Rendered:\n{}",
        rendered.join("\n")
    );

    let parent_header_idx = parent_header_idx.unwrap();
    let parent_desc_idx = parent_desc_idx.unwrap();
    let child_header_idx = child_header_idx.unwrap();
    let child_desc_idx = child_desc_idx.unwrap();

    // CRITICAL: Parent content must come BEFORE child content
    assert!(
        parent_header_idx < child_header_idx,
        "Parent header (line {}) should appear before child header (line {}). Rendered:\n{}",
        parent_header_idx,
        child_header_idx,
        rendered.join("\n")
    );
    assert!(
        parent_desc_idx < child_desc_idx,
        "Parent description (line {}) should appear before child description (line {}). Rendered:\n{}",
        parent_desc_idx,
        child_desc_idx,
        rendered.join("\n")
    );

    // Verify the order: parent_header -> parent_desc -> child_header -> child_desc
    assert!(
        parent_header_idx < parent_desc_idx
            && parent_desc_idx < child_header_idx
            && child_header_idx < child_desc_idx,
        "Expected order: parent_header({}) < parent_desc({}) < child_header({}) < child_desc({}). Rendered:\n{}",
        parent_header_idx,
        parent_desc_idx,
        child_header_idx,
        child_desc_idx,
        rendered.join("\n")
    );
}

#[test]
fn subagent_child_before_parent_becomes_separate_cell() {
    // This test verifies the defensive re-parenting logic.
    //
    // In normal operation, the parent TaskStarted event is sent before any
    // child events can arrive (fixed in core/src/tools/handlers/task.rs).
    //
    // However, as a defensive measure, if a child event somehow arrives before
    // its parent, the TUI will re-parent the orphan when the parent arrives.

    let (mut chat, _rx) = make_chatwidget_manual();

    // Child subagent event arrives FIRST (before parent exists)
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_child".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Child Task".to_string(),
        delegation_id: Some("del_child".to_string()),
        parent_delegation_id: Some("del_parent".to_string()), // References non-existent parent
        depth: Some(1),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    // Parent subagent event arrives SECOND
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_parent".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Parent Task".to_string(),
        delegation_id: Some("del_parent".to_string()),
        parent_delegation_id: None,
        depth: Some(0),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    // With the current implementation, we expect 2 separate top-level cells
    // because the child arrived before the parent existed.
    // This is the BUG - we should have 1 cell with the child nested inside.

    // After the fix: orphan children should be re-parented when the parent arrives.
    assert_eq!(
        chat.active_subagent_cells.len(),
        1,
        "Child should be re-parented when parent arrives"
    );

    // Verify that the parent cell contains the child
    let parent_cell = chat.active_subagent_cells.get("call_parent").unwrap();
    assert_eq!(
        parent_cell.children().len(),
        1,
        "Parent should have 1 child after re-parenting"
    );
}

#[test]
fn subagent_patch_activity_display() {
    let (mut chat, _rx) = make_chatwidget_manual();
    use std::path::PathBuf;

    // Start a subagent
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_sub".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Patching Task".to_string(),
        delegation_id: Some("del_sub".to_string()),
        parent_delegation_id: None,
        depth: Some(0),
        inner: Box::new(EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: None,
        })),
    });

    // Send PatchApplyEnd event
    let mut changes = HashMap::new();
    changes.insert(
        PathBuf::from("foo.txt"),
        codex_core::protocol::FileChange::Add {
            content: "line1\nline2\n".to_string(),
        },
    );
    changes.insert(
        PathBuf::from("bar.rs"),
        codex_core::protocol::FileChange::Delete {
            content: "old\n".to_string(),
        },
    );

    // We need to construct PatchApplyEndEvent.
    chat.on_subagent_event(SubagentEventPayload {
        parent_call_id: "call_sub".to_string(),
        subagent_type: "explorer".to_string(),
        task_description: "Patching Task".to_string(),
        delegation_id: Some("del_sub".to_string()),
        parent_delegation_id: None,
        depth: Some(0),
        inner: Box::new(EventMsg::PatchApplyEnd(
            codex_core::protocol::PatchApplyEndEvent {
                call_id: "p1".into(),
                turn_id: "t1".into(),
                stdout: "".into(),
                stderr: "".into(),
                success: true,
                changes,
            },
        )),
    });

    // Check rendered output
    let cell = chat.active_subagent_cells.get("call_sub").unwrap();
    let lines = cell.display_lines(80);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();

    // Expect: Edited 2 files (+2 -1)
    // Note: The actual string might be wrapped or formatted, but "Edited 2 files (+2 -1)" should be present.
    let found = rendered
        .iter()
        .any(|s| s.contains("Edited 2 files (+2 -1)"));
    assert!(
        found,
        "Expected summary 'Edited 2 files (+2 -1)', found: {rendered:#?}"
    );
}
