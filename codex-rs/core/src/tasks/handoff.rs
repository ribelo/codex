use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::warn;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::state::TaskKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HandoffDraftEvent;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::user_input::UserInput;

use super::SessionTask;
use super::SessionTaskContext;

pub const HANDOFF_EXTRACTION_PROMPT: &str =
    include_str!("../../templates/handoff/extraction_prompt.md");

/// Task for extracting context for handoff to a new thread.
pub struct HandoffTask {
    pub goal: String,
}

#[derive(Debug, Deserialize)]
struct ExtractionResult {
    summary: String,
    #[serde(default)]
    files: Vec<String>,
}

#[async_trait]
impl SessionTask for HandoffTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        _cancellation_token: CancellationToken,
    ) -> Option<String> {
        let sess = session.clone_session();

        // Emit TaskStarted so TUI shows "Working"
        let event = EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: ctx.client.get_model_context_window(),
        });
        sess.send_event(&ctx, event).await;

        // Build extraction prompt
        let prompt = format!("{HANDOFF_EXTRACTION_PROMPT}\n{}", self.goal);

        // Run extraction using current model
        match run_extraction(&sess, &ctx, &prompt).await {
            Ok(result) => {
                let relevant_files: Vec<PathBuf> =
                    result.files.into_iter().map(PathBuf::from).collect();

                let draft_event = EventMsg::HandoffDraft(HandoffDraftEvent {
                    summary: result.summary,
                    goal: self.goal.clone(),
                    relevant_files,
                    parent_id: sess.conversation_id(),
                });
                sess.send_event(&ctx, draft_event).await;
            }
            Err(e) => {
                warn!("Handoff extraction failed: {e}");
                let error_event = EventMsg::Error(
                    crate::error::CodexErr::Fatal(format!("Handoff extraction failed: {e}"))
                        .to_error_event(None),
                );
                sess.send_event(&ctx, error_event).await;
            }
        }

        None // Don't continue turn - wait for user to confirm/cancel
    }

    async fn abort(&self, _session: Arc<SessionTaskContext>, _ctx: Arc<TurnContext>) {
        // Nothing to clean up
    }
}

async fn run_extraction(
    sess: &Session,
    ctx: &TurnContext,
    prompt: &str,
) -> Result<ExtractionResult, String> {
    use crate::client_common::Prompt;
    use crate::client_common::ResponseEvent;
    use crate::truncate::approx_token_count;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use futures::StreamExt;

    // Build prompt with conversation history
    let mut history = sess.clone_history().await;
    let mut input = history.get_history_for_prompt();

    // Get model context window, default to conservative 32k if unknown
    let model_context = ctx.client.get_model_context_window().unwrap_or(32_000);
    // Reserve ~20% for extraction prompt and response
    let budget = (model_context as usize).saturating_sub(model_context as usize / 5);

    // Truncate history if too long (Head + Tail strategy)
    // Conservative token budget for extraction to avoid context overflow while allowing enough context.

    let total_estimate: usize = input
        .iter()
        .map(|item| approx_token_count(&serde_json::to_string(item).unwrap_or_default()))
        .sum();

    if total_estimate > budget {
        let mut truncated = Vec::new();
        let mut used_tokens = 0;

        // Keep first message (original user intent)
        if let Some(first) = input.first() {
            let cost = approx_token_count(&serde_json::to_string(first).unwrap_or_default());
            truncated.push(first.clone());
            used_tokens += cost;
        }

        // Collect tail items that fit
        let mut tail = Vec::new();
        for item in input.iter().skip(1).rev() {
            let cost = approx_token_count(&serde_json::to_string(item).unwrap_or_default());
            if used_tokens + cost >= budget {
                break;
            }
            tail.push(item.clone());
            used_tokens += cost;
        }

        let omitted_count = input.len().saturating_sub(1 + tail.len());
        if omitted_count > 0 {
            debug!("Truncated {omitted_count} messages for handoff extraction");
            // Add a marker message indicating omission
            truncated.push(ResponseItem::Message {
                id: None,
                role: "system".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("[... omitted {omitted_count} messages for brevity ...]"),
                }],
            });
        }

        // Append tail in chronological order
        tail.reverse();
        truncated.extend(tail);
        input = truncated;
    }

    let extraction_prompt = Prompt {
        input,
        tools: vec![],
        base_instructions_override: Some(prompt.to_string()),
        output_schema: None,
    };

    // Stream the response
    let mut stream = ctx
        .client
        .clone()
        .stream(&extraction_prompt)
        .await
        .map_err(|e| format!("Failed to start extraction: {e}"))?;

    let mut response_text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                if let codex_protocol::models::ResponseItem::Message { content, .. } = &item {
                    for c in content {
                        if let ContentItem::OutputText { text, .. } = c {
                            response_text.push_str(text);
                        }
                    }
                }
            }
            Ok(ResponseEvent::Completed { .. }) => break,
            Err(e) => return Err(format!("Extraction stream error: {e}")),
            _ => continue,
        }
    }

    // Parse JSON from response
    parse_extraction_result(&response_text)
}

fn parse_extraction_result(text: &str) -> Result<ExtractionResult, String> {
    // Try to find JSON in the response
    let json_start = text.find('{');
    let json_end = text.rfind('}');

    match (json_start, json_end) {
        (Some(start), Some(end)) if end > start => {
            let json_str = &text[start..=end];
            serde_json::from_str(json_str).or_else(|_| {
                Ok(ExtractionResult {
                    summary: text.trim().to_string(),
                    files: vec![],
                })
            })
        }
        _ => {
            // Fallback: use entire text as summary, no files
            Ok(ExtractionResult {
                summary: text.trim().to_string(),
                files: vec![],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_valid_json() {
        let input = r#"{"summary": "I implemented auth", "files": ["src/auth.rs"]}"#;
        let result = parse_extraction_result(input).unwrap();
        assert_eq!(result.summary, "I implemented auth");
        assert_eq!(result.files, vec!["src/auth.rs"]);
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let input = r#"Here is the extraction:
{"summary": "Working on feature", "files": ["a.rs", "b.rs"]}
Done."#;
        let result = parse_extraction_result(input).unwrap();
        assert_eq!(result.summary, "Working on feature");
        assert_eq!(result.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn fallback_on_no_json() {
        let input = "Just some plain text summary without JSON";
        let result = parse_extraction_result(input).unwrap();
        assert_eq!(result.summary, input);
        assert!(result.files.is_empty());
    }

    #[test]
    fn fallback_on_invalid_json() {
        let input = r#"Malformed JSON: {"summary": "missing quote, "files": []}"#;
        let result = parse_extraction_result(input).unwrap();
        assert_eq!(result.summary, input);
        assert!(result.files.is_empty());
    }
}
