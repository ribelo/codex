use std::sync::Arc;

use crate::Prompt;
use crate::client_common::ResponseEvent;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::features::Feature;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use crate::protocol::CompactedItem;
use crate::protocol::ContextCompactedEvent;
use crate::protocol::EventMsg;
use crate::protocol::TaskStartedEvent;
use crate::protocol::TurnContextItem;
use crate::protocol::WarningEvent;
use crate::truncate::approx_token_count;
use crate::util::backoff;
use codex_app_server_protocol::AuthMode;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::user_input::UserInput;
use futures::prelude::*;
use tracing::error;

pub const SUMMARIZATION_PROMPT: &str = include_str!("../templates/compact/prompt.md");
pub const SUMMARY_PREFIX: &str = include_str!("../templates/compact/summary_prefix.md");

/// System prompt for the summarization model.
/// This establishes the "observer" role and prevents the model from continuing the conversation.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to update the summary \
     of a software development session. Do not answer questions or continue the \
     conversation. ONLY output the structured summary.";

/// Maximum number of lines to keep from tool outputs before truncating.
const MAX_TOOL_OUTPUT_LINES: usize = 100;
/// Maximum characters to keep from any single text segment before truncating.
const MAX_TEXT_CHARS: usize = 50_000;

/// How many tokens of recent history to keep after compaction.
/// Based on pi-mono's default of 20000 tokens.
/// This is much more aggressive than the previous `context_window / 2` approach
/// which kept 50% of the context window (e.g., 64K tokens for a 128K model).
const KEEP_RECENT_TOKENS: usize = 20_000;

pub(crate) fn should_use_remote_compact_task(
    session: &Session,
    provider: &ModelProviderInfo,
) -> bool {
    session
        .services
        .auth_manager
        .auth()
        .is_some_and(|auth| auth.mode == AuthMode::ChatGPT)
        && session.enabled(Feature::RemoteCompaction)
        && matches!(provider.wire_api, WireApi::Chat | WireApi::Responses)
}

pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) {
    let prompt = turn_context.compact_prompt().to_string();
    let input = vec![UserInput::Text { text: prompt }];

    run_compact_task_inner(sess, turn_context, input).await;
}

pub(crate) async fn run_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
) {
    let start_event = EventMsg::TaskStarted(TaskStartedEvent {
        model_context_window: Some(turn_context.client.get_model_context_window()),
    });
    sess.send_event(&turn_context, start_event).await;
    run_compact_task_inner(sess.clone(), turn_context, input).await;
}

async fn run_compact_task_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    _input: Vec<UserInput>, // Now ignored - we build our own prompt
) {
    // Get current history and identify what to summarize vs keep
    let history_snapshot = sess.clone_history().await.get_history();
    let turns = identify_turns(&history_snapshot);
    // Use the smaller of: hardcoded budget OR half the model's context window
    let model_context_window = turn_context.client.get_model_context_window() as usize;
    let budget = KEEP_RECENT_TOKENS.min(model_context_window / 2);
    let (mut to_summarize, mut to_keep) = select_turns_within_budget(turns, budget);

    // Extract previous summary if it exists (from a prior compaction)
    let previous_summary = extract_previous_summary(&history_snapshot);

    let mut truncated_count = 0i32;
    let max_retries = turn_context.client.get_provider().stream_max_retries();
    let mut retries = 0;

    let rollout_item = RolloutItem::TurnContext(TurnContextItem {
        cwd: turn_context.cwd.clone(),
        approval_policy: turn_context.approval_policy,
        sandbox_policy: turn_context.sandbox_policy.clone(),
        model: turn_context.client.get_model(),
        effort: turn_context.client.get_reasoning_effort(),
        summary: turn_context.client.get_reasoning_summary(),
    });
    sess.persist_rollout_items(&[rollout_item]).await;

    loop {
        // Serialize the turns to be summarized into text
        let items_to_serialize: Vec<ResponseItem> = to_summarize
            .iter()
            .flat_map(|turn| turn.items.iter().cloned())
            .collect();
        let serialized_conversation = serialize_history_to_text(&items_to_serialize);

        // Build the XML-wrapped prompt
        let prompt_text = build_compaction_prompt_text(
            &previous_summary,
            &serialized_conversation,
            turn_context.compact_prompt(),
        );

        // Create history with just our compaction request
        let compaction_input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: prompt_text }],
        }];

        let prompt = Prompt {
            base_instructions_override: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
            input: compaction_input.clone(),
            ..Default::default()
        };
        let attempt_result = drain_to_completed(&sess, turn_context.as_ref(), &prompt).await;

        match attempt_result {
            Ok(summary_suffix) => {
                if truncated_count > 0 {
                    sess.notify_background_event(
                        turn_context.as_ref(),
                        format!(
                            "Trimmed {truncated_count} older conversation item(s) before compacting so the prompt fits the model context window."
                        ),
                    )
                    .await;
                }
                // Build the summary text with prefix
                let summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}");

                // Build the new compacted history
                let initial_context = sess.build_initial_context(turn_context.as_ref());
                let mut new_history =
                    build_compacted_history(initial_context, &to_keep, &summary_text);

                // Preserve ghost snapshots
                let ghost_snapshots: Vec<ResponseItem> = history_snapshot
                    .iter()
                    .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
                    .cloned()
                    .collect();
                new_history.extend(ghost_snapshots);
                sess.replace_history(new_history).await;
                sess.recompute_token_usage(&turn_context).await;

                let rollout_item = RolloutItem::Compacted(CompactedItem {
                    message: summary_text.clone(),
                    replacement_history: None,
                });
                sess.persist_rollout_items(&[rollout_item]).await;

                let event = EventMsg::ContextCompacted(ContextCompactedEvent {});
                sess.send_event(&turn_context, event).await;

                let warning = EventMsg::Warning(WarningEvent {
                    message: "Heads up: Long conversations and multiple compactions can cause the model to be less accurate. Start a new conversation when possible to keep conversations small and targeted.".to_string(),
                });
                sess.send_event(&turn_context, warning).await;
                return;
            }
            Err(CodexErr::Interrupted) => {
                return;
            }
            Err(CodexErr::ContextWindowExceeded) => {
                // The serialized prompt is too large - drop the newest turn from
                // to_summarize and prepend it to to_keep to preserve chronological order
                if !to_summarize.is_empty() {
                    let dropped = to_summarize.pop().unwrap();
                    truncated_count += dropped.items.len() as i32;
                    to_keep.insert(0, dropped);
                    continue;
                } else if !to_keep.is_empty() {
                    // Nothing to summarize but we can drop the oldest kept turn
                    // to reduce prompt size (it will be lost entirely)
                    if let Some(first_turn) = to_keep.first()
                        && !first_turn.items.is_empty()
                    {
                        let dropped = to_keep.remove(0);
                        truncated_count += dropped.items.len() as i32;
                        continue;
                    }
                    // Nothing left to drop
                    let e = CodexErr::ContextWindowExceeded;
                    error!("Context window exceeded with empty to_keep turn. Error: {e}");
                    sess.set_total_tokens_full(turn_context.as_ref()).await;
                    let event = EventMsg::Error(e.to_error_event(None));
                    sess.send_event(&turn_context, event).await;
                    return;
                } else {
                    // Nothing left to trim - this is fatal
                    let e = CodexErr::ContextWindowExceeded;
                    error!(
                        "Context window exceeded while compacting with no turns left to trim. Error: {e}"
                    );
                    sess.set_total_tokens_full(turn_context.as_ref()).await;
                    let event = EventMsg::Error(e.to_error_event(None));
                    sess.send_event(&turn_context, event).await;
                    return;
                }
            }
            Err(e) => {
                if retries < max_retries {
                    retries += 1;
                    let delay = backoff(retries);
                    sess.notify_stream_error(
                        turn_context.as_ref(),
                        format!("Reconnecting... {retries}/{max_retries}"),
                        e,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    continue;
                } else {
                    let event = EventMsg::Error(e.to_error_event(None));
                    sess.send_event(&turn_context, event).await;
                    return;
                }
            }
        }
    }
}

/// Extracts the previous summary from history if one exists from a prior compaction.
fn extract_previous_summary(history: &[ResponseItem]) -> Option<String> {
    for item in history {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
            && let Some(text) = content_items_to_text(content)
            && is_summary_message(&text)
        {
            // Strip the SUMMARY_PREFIX to get just the summary content
            let prefix = format!("{SUMMARY_PREFIX}\n");
            if let Some(summary) = text.strip_prefix(&prefix) {
                return Some(summary.to_string());
            }
        }
    }
    None
}

/// Builds the compaction prompt text with XML-wrapped sections.
fn build_compaction_prompt_text(
    previous_summary: &Option<String>,
    serialized_conversation: &str,
    compaction_instructions: &str,
) -> String {
    let mut prompt = String::new();

    // Add previous summary if it exists
    if let Some(summary) = previous_summary {
        prompt.push_str("<previous_summary>\n");
        prompt.push_str(summary);
        prompt.push_str("\n</previous_summary>\n\n");
    }

    // Add serialized conversation
    prompt.push_str("<conversation>\n");
    prompt.push_str(serialized_conversation);
    prompt.push_str("\n</conversation>\n\n");

    // Add compaction instructions
    prompt.push_str(compaction_instructions);

    prompt
}
pub fn content_items_to_text(content: &[ContentItem]) -> Option<String> {
    let mut pieces = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !text.is_empty() {
                    pieces.push(text.as_str());
                }
            }
            ContentItem::InputImage { .. } => {}
        }
    }
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join("\n"))
    }
}

pub(crate) fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

/// A complete conversation turn starting with a user message.
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    /// All ResponseItems in this turn (user message + assistant response + tool calls/results).
    pub items: Vec<ResponseItem>,
    /// Approximate token count for budget calculations.
    pub token_count: usize,
}

/// Estimates the token count for a single ResponseItem.
fn approx_token_count_for_item(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::Message { content, .. } => content_items_to_text(content)
            .map(|t| approx_token_count(&t))
            .unwrap_or(0),
        ResponseItem::FunctionCall {
            arguments, name, ..
        } => approx_token_count(name) + approx_token_count(arguments),
        ResponseItem::FunctionCallOutput { output, .. } => approx_token_count(&output.content),
        ResponseItem::CustomToolCall { input, name, .. } => {
            approx_token_count(name) + approx_token_count(input)
        }
        ResponseItem::CustomToolCallOutput { output, .. } => approx_token_count(output),
        ResponseItem::Reasoning {
            summary,
            encrypted_content,
            ..
        } => {
            let summary_tokens: usize = summary
                .iter()
                .map(|s| match s {
                    ReasoningItemReasoningSummary::SummaryText { text } => approx_token_count(text),
                })
                .sum();
            // Also count encrypted_content (signature) which can be large
            let encrypted_tokens = encrypted_content
                .as_ref()
                .map(|s| approx_token_count(s))
                .unwrap_or(0);
            summary_tokens + encrypted_tokens
        }
        ResponseItem::GhostSnapshot { .. } => 50, // Approximate fixed cost
        ResponseItem::WebSearchCall { .. } => 20,
        ResponseItem::CompactionSummary { encrypted_content } => {
            approx_token_count(encrypted_content)
        }
        ResponseItem::Other => 0,
    }
}

/// Truncates text to a maximum number of lines, adding a marker if truncated.
/// Also limits total character count to prevent OOM with long single-line outputs.
fn truncate_text(text: &str, max_lines: usize, max_chars: usize) -> String {
    // First limit by character count
    let text = if text.len() > max_chars {
        let truncated = &text[..max_chars];
        format!(
            "{truncated}\n[... {} more chars truncated ...]",
            text.len() - max_chars
        )
    } else {
        text.to_string()
    };

    // Then limit by line count
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    if total_lines <= max_lines {
        text
    } else {
        let kept: Vec<&str> = lines.into_iter().take(max_lines).collect();
        format!(
            "{}\n[... {} more lines truncated ...]",
            kept.join("\n"),
            total_lines - max_lines
        )
    }
}

/// Serializes a list of ResponseItems to a labeled text format for summarization.
///
/// This creates a text representation like:
/// ```text
/// [User]: What files are in this directory?
///
/// [Tool call: shell]: {"cmd": "ls -la"}
///
/// [Tool result]: file1.rs
/// file2.rs
///
/// [Assistant]: Here are the files...
/// ```
///
/// Large tool outputs are truncated to prevent token blowout.
pub(crate) fn serialize_history_to_text(items: &[ResponseItem]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let Some(text) = content_items_to_text(content) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let label = if role == "user" { "User" } else { "Assistant" };
                parts.push(format!("[{label}]: {text}"));
            }
            ResponseItem::Reasoning { summary, .. } => {
                let text: String = summary
                    .iter()
                    .filter_map(|s| match s {
                        ReasoningItemReasoningSummary::SummaryText { text } => Some(text.as_str()),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    parts.push(format!("[Assistant thinking]: {text}"));
                }
            }
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => {
                parts.push(format!("[Tool call: {name}]: {arguments}"));
            }
            ResponseItem::FunctionCallOutput { output, .. } => {
                let truncated =
                    truncate_text(&output.content, MAX_TOOL_OUTPUT_LINES, MAX_TEXT_CHARS);
                parts.push(format!("[Tool result]: {truncated}"));
            }
            ResponseItem::CustomToolCall { name, input, .. } => {
                parts.push(format!("[Tool call: {name}]: {input}"));
            }
            ResponseItem::CustomToolCallOutput { output, .. } => {
                let truncated = truncate_text(output, MAX_TOOL_OUTPUT_LINES, MAX_TEXT_CHARS);
                parts.push(format!("[Tool result]: {truncated}"));
            }
            ResponseItem::WebSearchCall { action, .. } => {
                let description = match action {
                    WebSearchAction::Search { query } => {
                        query.as_deref().unwrap_or("(no query)").to_string()
                    }
                    WebSearchAction::OpenPage { url } => {
                        format!("open {}", url.as_deref().unwrap_or("(no url)"))
                    }
                    WebSearchAction::FindInPage { url, pattern } => {
                        format!(
                            "find '{}' in {}",
                            pattern.as_deref().unwrap_or("(no pattern)"),
                            url.as_deref().unwrap_or("(no url)")
                        )
                    }
                    WebSearchAction::Other => "(unknown action)".to_string(),
                };
                parts.push(format!("[Web search]: {description}"));
            }
            ResponseItem::GhostSnapshot { .. } => {
                // Skip ghost snapshots - they're internal bookkeeping
            }
            ResponseItem::CompactionSummary { .. } => {
                // Skip compaction summaries - they're opaque encrypted content
            }
            ResponseItem::Other => {}
        }
    }

    parts.join("\n\n")
}

/// Checks if a user message is a session prefix entry (AGENTS.md, environment context, etc.)
fn is_session_prefix_message(content: &[ContentItem]) -> bool {
    let Some(text) = content_items_to_text(content) else {
        return false;
    };
    text.starts_with("# AGENTS.md instructions")
        || text.starts_with("<INSTRUCTIONS>")
        || text.starts_with("<ENVIRONMENT_CONTEXT>")
        || text.starts_with("<environment_context>")
        || text == SUMMARIZATION_PROMPT
}

/// Groups history items into complete turns.
/// A turn starts with a user message and includes all items until the next user message.
/// GhostSnapshots attach to their preceding turn.
/// Session prefix messages (AGENTS.md, environment context) are filtered out.
pub(crate) fn identify_turns(history: &[ResponseItem]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current_items: Vec<ResponseItem> = Vec::new();
    let mut current_tokens: usize = 0;

    for item in history {
        match item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                // Skip session prefix messages
                if is_session_prefix_message(content) {
                    continue;
                }
                // Skip summary messages from previous compaction
                if let Some(text) = content_items_to_text(content)
                    && is_summary_message(&text)
                {
                    continue;
                }
                // Start a new turn if we have items
                if !current_items.is_empty() {
                    turns.push(Turn {
                        items: std::mem::take(&mut current_items),
                        token_count: current_tokens,
                    });
                    current_tokens = 0;
                }
                let tokens = approx_token_count_for_item(item);
                current_items.push(item.clone());
                current_tokens += tokens;
            }
            ResponseItem::GhostSnapshot { .. } => {
                // GhostSnapshots attach to the current turn
                let tokens = approx_token_count_for_item(item);
                current_items.push(item.clone());
                current_tokens += tokens;
            }
            _ => {
                // Add to current turn if we have one started
                if !current_items.is_empty() {
                    let tokens = approx_token_count_for_item(item);
                    current_items.push(item.clone());
                    current_tokens += tokens;
                }
            }
        }
    }

    // Don't forget the last turn
    if !current_items.is_empty() {
        turns.push(Turn {
            items: current_items,
            token_count: current_tokens,
        });
    }

    turns
}

/// Selects turns from the end that fit within the token budget.
/// Returns (turns_to_summarize, turns_to_keep).
/// Always keeps the last turn even if it exceeds budget.
pub(crate) fn select_turns_within_budget(
    turns: Vec<Turn>,
    budget: usize,
) -> (Vec<Turn>, Vec<Turn>) {
    if turns.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Always keep at least the last turn
    let mut kept_indices: Vec<usize> = vec![turns.len() - 1];
    let mut used_tokens = turns.last().map(|t| t.token_count).unwrap_or(0);

    // Walk backwards from second-to-last, prepending complete turns
    for i in (0..turns.len().saturating_sub(1)).rev() {
        let turn_tokens = turns[i].token_count;
        if used_tokens + turn_tokens <= budget {
            kept_indices.push(i);
            used_tokens += turn_tokens;
        } else {
            // Budget exceeded, stop here
            break;
        }
    }

    // Reverse to get chronological order
    kept_indices.reverse();

    // Split turns
    let cut_point = *kept_indices.first().unwrap_or(&0);
    let mut to_summarize = Vec::new();
    let mut to_keep = Vec::new();

    for (i, turn) in turns.into_iter().enumerate() {
        if i < cut_point {
            to_summarize.push(turn);
        } else {
            to_keep.push(turn);
        }
    }

    (to_summarize, to_keep)
}

/// Builds the compacted history from initial context, kept turns, and summary.
/// Output structure: [InitialContext] + [Summary message] + [Kept turns flattened]
pub(crate) fn build_compacted_history(
    mut history: Vec<ResponseItem>,
    kept_turns: &[Turn],
    summary_text: &str,
) -> Vec<ResponseItem> {
    // Add summary message (summarizes what came before the kept turns)
    let summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };

    history.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text: summary_text }],
    });

    // Flatten kept turns into history, preserving all items
    for turn in kept_turns {
        history.extend(turn.items.iter().cloned());
    }

    history
}

async fn drain_to_completed(
    sess: &Session,
    turn_context: &TurnContext,
    prompt: &Prompt,
) -> CodexResult<String> {
    let mut stream = turn_context.client.clone().stream(prompt).await?;
    let mut summary_text = String::new();
    loop {
        let maybe_event = stream.next().await;
        let Some(event) = maybe_event else {
            return Err(CodexErr::Stream(
                "stream closed before response.completed".into(),
                None,
            ));
        };
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                // Extract text from the response item without recording to history
                if let ResponseItem::Message { content, .. } = &item
                    && let Some(text) = content_items_to_text(content)
                {
                    summary_text.push_str(&text);
                }
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                sess.update_token_usage_info(turn_context, token_usage.as_ref())
                    .await;
                return Ok(summary_text);
            }
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    fn user_msg(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
        }
    }

    fn assistant_msg(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
                signature: None,
            }],
        }
    }

    fn function_call_item(name: &str, args: &str, call_id: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: name.to_string(),
            arguments: args.to_string(),
            call_id: call_id.to_string(),
        }
    }

    fn function_result_item(call_id: &str, output: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload {
                content: output.to_string(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn content_items_to_text_joins_non_empty_segments() {
        let items = vec![
            ContentItem::InputText {
                text: "hello".to_string(),
            },
            ContentItem::OutputText {
                text: String::new(),
                signature: None,
            },
            ContentItem::OutputText {
                text: "world".to_string(),
                signature: None,
            },
        ];

        let joined = content_items_to_text(&items);

        assert_eq!(Some("hello\nworld".to_string()), joined);
    }

    #[test]
    fn content_items_to_text_ignores_image_only_content() {
        let items = vec![ContentItem::InputImage {
            image_url: "file://image.png".to_string(),
        }];

        let joined = content_items_to_text(&items);

        assert_eq!(None, joined);
    }

    #[test]
    fn test_build_compacted_history() {
        let initial_context: Vec<ResponseItem> = Vec::new();
        let kept_turns = vec![Turn {
            items: vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "kept".to_string(),
                }],
            }],
            token_count: 10,
        }];
        let summary_text = "summary text";

        let history = build_compacted_history(initial_context, &kept_turns, summary_text);

        assert_eq!(history.len(), 2);
        assert_eq!(
            content_items_to_text(match &history[0] {
                ResponseItem::Message { content, .. } => content,
                _ => panic!(),
            }),
            Some("summary text".to_string())
        );
        assert_eq!(
            content_items_to_text(match &history[1] {
                ResponseItem::Message { content, .. } => content,
                _ => panic!(),
            }),
            Some("kept".to_string())
        );
    }

    #[test]
    fn test_identify_turns_groups_by_user_message() {
        let history = vec![
            user_msg("first question"),
            assistant_msg("first answer"),
            user_msg("second question"),
            assistant_msg("second answer"),
        ];

        let turns = identify_turns(&history);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].items.len(), 2);
        assert_eq!(turns[1].items.len(), 2);
    }

    #[test]
    fn test_identify_turns_includes_tool_calls() {
        let history = vec![
            user_msg("run a command"),
            function_call_item("shell", "{\"cmd\": \"ls\"}", "call-1"),
            function_result_item("call-1", "file1\nfile2"),
            assistant_msg("here are the files"),
        ];

        let turns = identify_turns(&history);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].items.len(), 4);
    }

    #[test]
    fn test_identify_turns_filters_session_prefix() {
        let history = vec![
            user_msg(
                "# AGENTS.md instructions for project\n\n<INSTRUCTIONS>do things</INSTRUCTIONS>",
            ),
            user_msg("<environment_context>cwd=/tmp</environment_context>"),
            user_msg("real user message"),
            assistant_msg("real answer"),
        ];

        let turns = identify_turns(&history);

        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn test_identify_turns_empty_history() {
        let turns = identify_turns(&[]);
        assert!(turns.is_empty());
    }

    #[test]
    fn test_select_turns_keeps_last_turn_always() {
        let turns = vec![
            Turn {
                items: vec![user_msg("a")],
                token_count: 1000,
            },
            Turn {
                items: vec![user_msg("b")],
                token_count: 1000,
            },
            Turn {
                items: vec![user_msg("c")],
                token_count: 5000,
            },
        ];

        let (to_summarize, to_keep) = select_turns_within_budget(turns, 100);

        assert_eq!(to_keep.len(), 1);
        assert_eq!(to_summarize.len(), 2);
    }

    #[test]
    fn test_select_turns_respects_budget() {
        let turns = vec![
            Turn {
                items: vec![user_msg("a")],
                token_count: 100,
            },
            Turn {
                items: vec![user_msg("b")],
                token_count: 100,
            },
            Turn {
                items: vec![user_msg("c")],
                token_count: 100,
            },
        ];

        let (to_summarize, to_keep) = select_turns_within_budget(turns, 250);

        assert_eq!(to_keep.len(), 2);
        assert_eq!(to_summarize.len(), 1);
    }

    #[test]
    fn test_select_turns_all_fit() {
        let turns = vec![
            Turn {
                items: vec![user_msg("a")],
                token_count: 100,
            },
            Turn {
                items: vec![user_msg("b")],
                token_count: 100,
            },
        ];

        let (to_summarize, to_keep) = select_turns_within_budget(turns, 1000);

        assert_eq!(to_keep.len(), 2);
        assert!(to_summarize.is_empty());
    }

    #[test]
    fn test_select_turns_empty_input() {
        let (to_summarize, to_keep) = select_turns_within_budget(vec![], 1000);

        assert!(to_summarize.is_empty());
        assert!(to_keep.is_empty());
    }

    #[test]
    fn test_build_compacted_history_with_turns() {
        let initial = vec![user_msg("system context")];
        let turns = vec![Turn {
            items: vec![user_msg("question"), assistant_msg("answer")],
            token_count: 100,
        }];

        let history = build_compacted_history(initial, &turns, "summary of past");

        // Should be: initial + summary + turn items
        assert_eq!(history.len(), 4);
    }

    #[test]
    fn test_build_compacted_history_summary_before_turns() {
        let turns = vec![Turn {
            items: vec![user_msg("kept question")],
            token_count: 100,
        }];

        let history = build_compacted_history(vec![], &turns, "the summary");

        // First item should be the summary
        match &history[0] {
            ResponseItem::Message { content, .. } => {
                let text = content_items_to_text(content).unwrap();
                assert_eq!(text, "the summary");
            }
            _ => panic!("Expected message"),
        }

        // Second item should be the kept turn's user message
        match &history[1] {
            ResponseItem::Message { content, role, .. } => {
                assert_eq!(role, "user");
                let text = content_items_to_text(content).unwrap();
                assert_eq!(text, "kept question");
            }
            _ => panic!("Expected user message"),
        }
    }

    #[test]
    fn test_build_compacted_history_empty_turns() {
        let initial = vec![user_msg("context")];
        let history = build_compacted_history(initial, &[], "summary");

        // Should be: initial + summary
        assert_eq!(history.len(), 2);
    }
}
