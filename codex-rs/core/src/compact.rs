use std::borrow::Cow;
use std::collections::BTreeSet;
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
use crate::openai_models::model_family::InstructionMode;
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
use serde::Deserialize;
use tracing::error;

pub const SUMMARIZATION_PROMPT: &str = include_str!("../templates/compact/prompt.md");
pub const UPDATE_PROMPT: &str = include_str!("../templates/compact/update_prompt.md");
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

const MAX_FILE_OPS_PER_KIND: usize = 200;

const FILE_OPS_XML_TAG: &str = "file_ops";
const FILE_OPS_XML_READ_FILE_TAG: &str = "read_file";
const FILE_OPS_XML_WRITE_FILE_TAG: &str = "write_file";
const FILE_OPS_XML_APPLY_PATCH_TAG: &str = "apply_patch";

#[derive(Debug, Default, Clone)]
struct FileOps {
    read_file_paths: BTreeSet<String>,
    write_file_paths: BTreeSet<String>,
    apply_patch_paths: BTreeSet<String>,
}

impl FileOps {
    fn is_empty(&self) -> bool {
        self.read_file_paths.is_empty()
            && self.write_file_paths.is_empty()
            && self.apply_patch_paths.is_empty()
    }

    fn merge(&mut self, other: FileOps) {
        self.read_file_paths.extend(other.read_file_paths);
        self.write_file_paths.extend(other.write_file_paths);
        self.apply_patch_paths.extend(other.apply_patch_paths);
    }

    fn merge_with_limit(older: FileOps, newer: FileOps, limit: usize) -> FileOps {
        FileOps {
            read_file_paths: merge_paths_with_limit(
                &older.read_file_paths,
                &newer.read_file_paths,
                limit,
            ),
            write_file_paths: merge_paths_with_limit(
                &older.write_file_paths,
                &newer.write_file_paths,
                limit,
            ),
            apply_patch_paths: merge_paths_with_limit(
                &older.apply_patch_paths,
                &newer.apply_patch_paths,
                limit,
            ),
        }
    }

    fn truncate_to_limit(&mut self, limit: usize) {
        truncate_paths_to_limit(&mut self.read_file_paths, limit);
        truncate_paths_to_limit(&mut self.write_file_paths, limit);
        truncate_paths_to_limit(&mut self.apply_patch_paths, limit);
    }

    fn render_xml_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut out = String::new();
        out.push_str(&format!("<{FILE_OPS_XML_TAG}>\n"));

        if !self.read_file_paths.is_empty() {
            out.push_str(&format!("<{FILE_OPS_XML_READ_FILE_TAG}>\n"));
            for path in &self.read_file_paths {
                out.push_str(escape_xml_text(path).as_ref());
                out.push('\n');
            }
            out.push_str(&format!("</{FILE_OPS_XML_READ_FILE_TAG}>\n"));
        }

        if !self.write_file_paths.is_empty() {
            out.push_str(&format!("<{FILE_OPS_XML_WRITE_FILE_TAG}>\n"));
            for path in &self.write_file_paths {
                out.push_str(escape_xml_text(path).as_ref());
                out.push('\n');
            }
            out.push_str(&format!("</{FILE_OPS_XML_WRITE_FILE_TAG}>\n"));
        }

        if !self.apply_patch_paths.is_empty() {
            out.push_str(&format!("<{FILE_OPS_XML_APPLY_PATCH_TAG}>\n"));
            for path in &self.apply_patch_paths {
                out.push_str(escape_xml_text(path).as_ref());
                out.push('\n');
            }
            out.push_str(&format!("</{FILE_OPS_XML_APPLY_PATCH_TAG}>\n"));
        }

        out.push_str(&format!("</{FILE_OPS_XML_TAG}>"));
        Some(out)
    }
}

fn truncate_paths_to_limit(paths: &mut BTreeSet<String>, limit: usize) {
    if paths.len() <= limit {
        return;
    }
    *paths = paths.iter().take(limit).cloned().collect();
}

fn merge_paths_with_limit(
    older: &BTreeSet<String>,
    newer: &BTreeSet<String>,
    limit: usize,
) -> BTreeSet<String> {
    let mut merged: BTreeSet<String> = newer.iter().take(limit).cloned().collect();
    for path in older {
        if merged.len() >= limit {
            break;
        }
        merged.insert(path.clone());
    }
    merged
}

fn escape_xml_text(text: &str) -> Cow<'_, str> {
    if !text.contains('&')
        && !text.contains('<')
        && !text.contains('>')
        && !text.contains('"')
        && !text.contains('\'')
    {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

fn unescape_xml_text(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }

    Cow::Owned(
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[derive(Debug, Clone)]
struct PreviousSummary {
    summary_for_prompt: String,
    goal_section: Option<String>,
    file_ops: FileOps,
}

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
    let tokens_before = sess
        .clone_history()
        .await
        .estimate_token_count(turn_context.as_ref());

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

        let current_file_ops = extract_file_ops_from_items(&items_to_serialize);
        let previous_file_ops = previous_summary
            .as_ref()
            .map(|summary| summary.file_ops.clone())
            .unwrap_or_default();
        let merged_file_ops =
            FileOps::merge_with_limit(previous_file_ops, current_file_ops, MAX_FILE_OPS_PER_KIND);

        let previous_summary_for_prompt = previous_summary
            .as_ref()
            .map(|summary| summary.summary_for_prompt.as_str());

        // Build the XML-wrapped prompt
        let prompt_text = build_compaction_prompt_text(
            &previous_summary_for_prompt,
            &serialized_conversation,
            turn_context.compact_prompt(),
        );

        // Create history with just our compaction request
        // For Strict mode, the SUMMARIZATION_SYSTEM_PROMPT needs to be in the input
        // as a developer message since base_instructions_override is ignored.
        let model_family = turn_context.client.get_model_family();
        let (base_instructions_override, compaction_input) = match model_family.instruction_mode {
            InstructionMode::Strict => {
                // Inject as developer message for Strict mode
                let input = vec![
                    ResponseItem::Message {
                        id: None,
                        role: "developer".to_string(),
                        content: vec![ContentItem::InputText {
                            text: SUMMARIZATION_SYSTEM_PROMPT.to_string(),
                        }],
                    },
                    ResponseItem::Message {
                        id: None,
                        role: "user".to_string(),
                        content: vec![ContentItem::InputText { text: prompt_text }],
                    },
                ];
                (None, input)
            }
            InstructionMode::Prefix | InstructionMode::Flexible => {
                // Use base_instructions_override for non-Strict modes
                let input = vec![ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText { text: prompt_text }],
                }];
                (Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()), input)
            }
        };

        let prompt = Prompt {
            base_instructions_override,
            input: compaction_input,
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
                let previous_goal = previous_summary
                    .as_ref()
                    .and_then(|summary| summary.goal_section.as_deref());
                let summary_suffix =
                    finalize_summary_suffix(&summary_suffix, previous_goal, &merged_file_ops);

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

                let tokens_before = tokens_before
                    .and_then(|tokens| i32::try_from(tokens).ok())
                    .unwrap_or(i32::MAX);
                let event = EventMsg::ContextCompacted(ContextCompactedEvent {
                    tokens_before,
                    summary: summary_suffix,
                });
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
fn extract_previous_summary(history: &[ResponseItem]) -> Option<PreviousSummary> {
    for item in history {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
            && let Some(text) = content_items_to_text(content)
            && is_summary_message(&text)
        {
            // Strip the SUMMARY_PREFIX to get just the summary content
            let prefix = format!("{SUMMARY_PREFIX}\n");
            if let Some(summary) = text.strip_prefix(&prefix) {
                let (summary_for_prompt, file_ops) = strip_file_ops_from_summary(summary);
                let goal_section = extract_markdown_section(&summary_for_prompt, "## Goal");
                return Some(PreviousSummary {
                    summary_for_prompt,
                    goal_section,
                    file_ops,
                });
            }
        }
    }
    None
}

/// Builds the compaction prompt text with XML-wrapped sections.
/// Uses UPDATE_PROMPT when a previous summary exists for proper iterative updates.
fn build_compaction_prompt_text(
    previous_summary: &Option<&str>,
    serialized_conversation: &str,
    initial_instructions: &str,
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

    // Use UPDATE_PROMPT when there's a previous summary, otherwise use initial instructions
    if previous_summary.is_some() {
        prompt.push_str(UPDATE_PROMPT);
    } else {
        prompt.push_str(initial_instructions);
    }

    prompt
}

#[derive(Deserialize)]
struct ReadFileArgs {
    file_path: String,
}

#[derive(Deserialize)]
struct ApplyPatchArgs {
    input: String,
}

#[derive(Deserialize)]
struct WriteFileArgs {
    file_path: Option<String>,
    path: Option<String>,
}

fn extract_file_ops_from_items(items: &[ResponseItem]) -> FileOps {
    let mut ops = FileOps::default();

    for item in items {
        match item {
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => {
                if name == "read_file" {
                    if let Ok(args) = serde_json::from_str::<ReadFileArgs>(arguments) {
                        ops.read_file_paths.insert(args.file_path);
                    }
                } else if name == "write_file" {
                    if let Ok(args) = serde_json::from_str::<WriteFileArgs>(arguments)
                        && let Some(path) = args.file_path.or(args.path)
                    {
                        ops.write_file_paths.insert(path);
                    }
                } else if name == "apply_patch"
                    && let Ok(args) = serde_json::from_str::<ApplyPatchArgs>(arguments)
                {
                    ops.apply_patch_paths
                        .extend(extract_paths_from_apply_patch(&args.input));
                }
            }
            ResponseItem::CustomToolCall { name, input, .. } => {
                if name == "apply_patch" {
                    ops.apply_patch_paths
                        .extend(extract_paths_from_apply_patch(input));
                } else if name == "write_file" {
                    if let Ok(args) = serde_json::from_str::<WriteFileArgs>(input)
                        && let Some(path) = args.file_path.or(args.path)
                    {
                        ops.write_file_paths.insert(path);
                    }
                } else if name == "read_file"
                    && let Ok(args) = serde_json::from_str::<ReadFileArgs>(input)
                {
                    ops.read_file_paths.insert(args.file_path);
                }
            }
            _ => {}
        }
    }

    ops
}

fn extract_paths_from_apply_patch(patch: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();

    for line in patch.lines() {
        let line = line.trim();
        for prefix in [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty() {
                    paths.insert(path.to_string());
                }
                break;
            }
        }
    }

    paths
}

fn extract_xml_block<'a>(input: &'a str, tag: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let (before_open, rest) = input.split_once(&open)?;
    let (inside, after_close) = rest.split_once(&close)?;
    Some((before_open, inside, after_close))
}

fn extract_all_xml_blocks<'a>(mut input: &'a str, tag: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    while let Some((_, inside, after)) = extract_xml_block(input, tag) {
        blocks.push(inside);
        input = after;
    }
    blocks
}

fn strip_file_ops_from_summary(summary: &str) -> (String, FileOps) {
    let mut combined_ops = FileOps::default();
    let mut remaining = summary.to_string();

    loop {
        let Some((before, inside, after)) = extract_xml_block(&remaining, FILE_OPS_XML_TAG) else {
            break;
        };

        combined_ops.merge(parse_file_ops_xml(inside));
        remaining = format!("{before}{after}");
    }

    while remaining.contains("\n\n\n") {
        remaining = remaining.replace("\n\n\n", "\n\n");
    }

    combined_ops.truncate_to_limit(MAX_FILE_OPS_PER_KIND);
    (remaining.trim().to_string(), combined_ops)
}

fn parse_file_ops_xml(xml: &str) -> FileOps {
    let mut ops = FileOps::default();

    for read in extract_all_xml_blocks(xml, FILE_OPS_XML_READ_FILE_TAG) {
        for line in read.lines().map(str::trim).filter(|line| !line.is_empty()) {
            ops.read_file_paths
                .insert(unescape_xml_text(line).into_owned());
        }
    }

    for write in extract_all_xml_blocks(xml, FILE_OPS_XML_WRITE_FILE_TAG) {
        for line in write.lines().map(str::trim).filter(|line| !line.is_empty()) {
            ops.write_file_paths
                .insert(unescape_xml_text(line).into_owned());
        }
    }

    for apply in extract_all_xml_blocks(xml, FILE_OPS_XML_APPLY_PATCH_TAG) {
        for line in apply.lines().map(str::trim).filter(|line| !line.is_empty()) {
            ops.apply_patch_paths
                .insert(unescape_xml_text(line).into_owned());
        }
    }

    ops.truncate_to_limit(MAX_FILE_OPS_PER_KIND);
    ops
}

fn extract_markdown_section(summary: &str, heading: &str) -> Option<String> {
    let heading_line = format!("{heading}\n");
    let start = summary.find(&heading_line)?;
    let after_heading = start + heading_line.len();
    let rest = &summary[after_heading..];

    let mut end = summary.len();
    for (i, line) in rest.lines().enumerate() {
        if line.starts_with("## ") {
            let byte_index = rest.lines().take(i).fold(0, |acc, l| acc + l.len() + 1);
            end = after_heading + byte_index;
            break;
        }
    }

    Some(summary[start..end].trim_end().to_string())
}

fn ensure_goal_section(summary: &str, previous_goal_section: Option<&str>) -> String {
    if summary.contains("## Goal") {
        return summary.to_string();
    }
    let Some(previous) = previous_goal_section else {
        return summary.to_string();
    };

    if summary.trim().is_empty() {
        previous.to_string()
    } else {
        format!("{previous}\n\n{summary}")
    }
}

fn finalize_summary_suffix(
    summary_suffix_raw: &str,
    previous_goal: Option<&str>,
    file_ops: &FileOps,
) -> String {
    let (without_ops, _) = strip_file_ops_from_summary(summary_suffix_raw);
    let goal_preserved = ensure_goal_section(&without_ops, previous_goal);
    let goal_preserved = goal_preserved.trim().to_string();

    let Some(file_ops_xml) = file_ops.render_xml_block() else {
        return goal_preserved;
    };

    if goal_preserved.is_empty() {
        file_ops_xml
    } else {
        format!("{goal_preserved}\n\n{file_ops_xml}")
    }
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
pub(crate) fn is_session_prefix_message(content: &[ContentItem]) -> bool {
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
    use serde_json::json;

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

    #[test]
    fn extract_file_ops_from_items_collects_read_and_apply_patch_paths() {
        let patch = "*** Begin Patch\n*** Update File: b.txt\n@@\n-old\n+new\n*** End Patch";
        let apply_patch_args = json!({ "input": patch }).to_string();
        let read_file_args = json!({ "file_path": "/repo/a.txt" }).to_string();

        let items = vec![
            function_call_item("read_file", &read_file_args, "call-1"),
            function_call_item("apply_patch", &apply_patch_args, "call-2"),
        ];

        let ops = extract_file_ops_from_items(&items);

        assert_eq!(
            ops.read_file_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["/repo/a.txt".to_string()]
        );
        assert_eq!(
            ops.apply_patch_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["b.txt".to_string()]
        );
    }

    #[test]
    fn strip_file_ops_from_summary_parses_and_removes_xml_block() {
        let summary = "## Goal\nShip it\n\n<file_ops>\n<apply_patch>\nb.txt\na.txt\n</apply_patch>\n</file_ops>\n\n## Next Steps\n1. Done";
        let (without_ops, ops) = strip_file_ops_from_summary(summary);

        assert_eq!(
            ops.apply_patch_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert_eq!(without_ops, "## Goal\nShip it\n\n## Next Steps\n1. Done");
    }

    #[test]
    fn finalize_summary_suffix_preserves_goal_and_renders_sorted_file_ops() {
        let raw = "## Next Steps\n1. Continue";
        let previous_goal = Some("## Goal\nShip it");
        let mut file_ops = FileOps::default();
        file_ops.apply_patch_paths.insert("b.txt".to_string());
        file_ops.apply_patch_paths.insert("a.txt".to_string());

        let out = finalize_summary_suffix(raw, previous_goal, &file_ops);

        assert!(out.contains("## Goal\nShip it"));
        assert!(out.contains("<file_ops>"));
        assert!(out.contains("<apply_patch>\na.txt\nb.txt\n</apply_patch>"));
    }

    #[test]
    fn parse_file_ops_xml_handles_multiple_blocks_per_tag() {
        let summary = "<file_ops>\n<read_file>\na.txt\n</read_file>\n<read_file>\nb.txt\n</read_file>\n</file_ops>";
        let (_, ops) = strip_file_ops_from_summary(summary);

        assert_eq!(
            ops.read_file_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
    }

    #[test]
    fn file_ops_xml_round_trips_escaped_paths() {
        let mut ops = FileOps::default();
        ops.read_file_paths.insert("a&b.txt".to_string());
        ops.write_file_paths.insert("c<d>.txt".to_string());

        let xml = ops.render_xml_block().expect("xml block");
        assert!(xml.contains("a&amp;b.txt"));
        assert!(xml.contains("c&lt;d&gt;.txt"));

        let (_, parsed) = strip_file_ops_from_summary(&xml);
        assert_eq!(
            parsed.read_file_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["a&b.txt".to_string()]
        );
        assert_eq!(
            parsed.write_file_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["c<d>.txt".to_string()]
        );
    }

    #[test]
    fn file_ops_merge_with_limit_prefers_newer_paths() {
        let mut older = FileOps::default();
        older.read_file_paths.insert("a.txt".to_string());
        older.read_file_paths.insert("b.txt".to_string());

        let mut newer = FileOps::default();
        newer.read_file_paths.insert("z.txt".to_string());

        let merged = FileOps::merge_with_limit(older, newer, 1);
        assert_eq!(
            merged.read_file_paths.iter().cloned().collect::<Vec<_>>(),
            vec!["z.txt".to_string()]
        );
    }

    #[test]
    fn extract_previous_summary_returns_none_when_no_summary() {
        let history = vec![user_msg("hello"), assistant_msg("hi there")];

        let result = extract_previous_summary(&history);

        assert!(result.is_none());
    }

    #[test]
    fn extract_previous_summary_extracts_summary_from_history() {
        let summary_content = format!(
            "{}\n## Goal\nTest goal\n\n## Progress\n- Done",
            SUMMARY_PREFIX
        );
        let history = vec![
            user_msg(&summary_content),
            user_msg("new question"),
            assistant_msg("new answer"),
        ];

        let result = extract_previous_summary(&history);

        assert!(result.is_some());
        let summary = result.unwrap();
        assert!(summary.summary_for_prompt.contains("## Goal"));
        assert!(summary.goal_section.is_some());
        assert!(summary.goal_section.unwrap().contains("Test goal"));
    }

    #[test]
    fn extract_previous_summary_parses_file_ops_from_summary() {
        let summary_content = format!(
            "{}\n## Goal\nTest\n\n<file_ops>\n<read_file>\na.txt\n</read_file>\n</file_ops>",
            SUMMARY_PREFIX
        );
        let history = vec![user_msg(&summary_content)];

        let result = extract_previous_summary(&history);

        assert!(result.is_some());
        let summary = result.unwrap();
        assert!(summary.file_ops.read_file_paths.contains("a.txt"));
        // File ops should be stripped from summary_for_prompt
        assert!(!summary.summary_for_prompt.contains("<file_ops>"));
    }

    #[test]
    fn build_compaction_prompt_uses_initial_instructions_without_previous_summary() {
        let conversation = "[User]: hello";
        let initial = "Initial instructions here";

        let prompt = build_compaction_prompt_text(&None, conversation, initial);

        assert!(prompt.contains("<conversation>"));
        assert!(prompt.contains(conversation));
        assert!(prompt.contains(initial));
        assert!(!prompt.contains("<previous_summary>"));
        // Should NOT contain UPDATE_PROMPT content
        assert!(!prompt.contains("UPDATING an existing summary"));
    }

    #[test]
    fn build_compaction_prompt_uses_update_prompt_with_previous_summary() {
        let prev = Some("## Goal\nOld goal");
        let conversation = "[User]: hello";
        let initial = "Initial instructions here";

        let prompt = build_compaction_prompt_text(&prev, conversation, initial);

        assert!(prompt.contains("<previous_summary>"));
        assert!(prompt.contains("Old goal"));
        assert!(prompt.contains("<conversation>"));
        assert!(prompt.contains(conversation));
        // Should NOT contain initial instructions when previous summary exists
        assert!(!prompt.contains(initial));
        // Should contain UPDATE_PROMPT content
        assert!(prompt.contains("UPDATING an existing summary"));
    }
}
