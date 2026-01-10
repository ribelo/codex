use std::sync::Arc;

use crate::Prompt;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::compact::SUMMARY_PREFIX;
use crate::compact::content_items_to_text;
use crate::error::Result as CodexResult;
use crate::protocol::CompactedItem;
use crate::protocol::ContextCompactedEvent;
use crate::protocol::EventMsg;
use crate::protocol::RolloutItem;
use crate::protocol::TaskStartedEvent;
use codex_protocol::models::ResponseItem;

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) {
    run_remote_compact_task_inner(&sess, &turn_context).await;
}

pub(crate) async fn run_remote_compact_task(sess: Arc<Session>, turn_context: Arc<TurnContext>) {
    let start_event = EventMsg::TaskStarted(TaskStartedEvent {
        model_context_window: Some(turn_context.client.get_model_context_window()),
    });
    sess.send_event(&turn_context, start_event).await;

    run_remote_compact_task_inner(&sess, &turn_context).await;
}

async fn run_remote_compact_task_inner(sess: &Arc<Session>, turn_context: &Arc<TurnContext>) {
    if let Err(err) = run_remote_compact_task_inner_impl(sess, turn_context).await {
        let event = EventMsg::Error(
            err.to_error_event(Some("Error running remote compact task".to_string())),
        );
        sess.send_event(turn_context, event).await;
    }
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> CodexResult<()> {
    let tokens_before = sess
        .clone_history()
        .await
        .estimate_token_count(turn_context.as_ref())
        .and_then(|tokens| i32::try_from(tokens).ok())
        .unwrap_or(i32::MAX);

    let mut history = sess.clone_history().await;
    let prompt = Prompt {
        input: history.get_history_for_prompt(),
        tools: vec![],
        base_instructions_override: turn_context.base_instructions.clone(),
        output_schema: None,
    };

    let mut new_history = turn_context
        .client
        .compact_conversation_history(&prompt)
        .await?;
    // Required to keep `/undo` available after compaction
    let ghost_snapshots: Vec<ResponseItem> = history
        .get_history()
        .iter()
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .cloned()
        .collect();

    if !ghost_snapshots.is_empty() {
        new_history.extend(ghost_snapshots);
    }
    sess.replace_history(new_history.clone()).await;
    sess.recompute_token_usage(turn_context).await;

    let summary_suffix = extract_summary_suffix(&new_history).unwrap_or_default();
    let compacted_item = CompactedItem {
        message: String::new(),
        replacement_history: Some(new_history),
    };
    sess.persist_rollout_items(&[RolloutItem::Compacted(compacted_item)])
        .await;

    let event = EventMsg::ContextCompacted(ContextCompactedEvent {
        tokens_before,
        summary: summary_suffix,
    });
    sess.send_event(turn_context, event).await;

    Ok(())
}

fn extract_summary_suffix(history: &[ResponseItem]) -> Option<String> {
    for item in history {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
            && let Some(text) = content_items_to_text(content)
        {
            let prefix = format!("{SUMMARY_PREFIX}\n");
            if let Some(summary_suffix) = text.strip_prefix(&prefix) {
                return Some(summary_suffix.to_string());
            }
        }
    }
    None
}
