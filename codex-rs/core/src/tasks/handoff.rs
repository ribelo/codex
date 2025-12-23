use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::client::ModelClient;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::state::TaskKind;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HandoffDraftEvent;
use codex_protocol::protocol::SessionSource;
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

fn extraction_result_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "Plain text with bullets. No markdown headers, no bold/italic, no code fences. Use workspace-relative paths for files."
            },
            "files": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "description": "An array of file or directory paths (workspace-relative) that are relevant to accomplishing the goal. Prioritize by importance, up to 12 items."
            }
        },
        "required": ["summary", "files"],
        "additionalProperties": false
    })
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
            model_context_window: Some(ctx.client.get_model_context_window()),
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
    use crate::truncate::TruncationBias;
    use crate::truncate::approx_token_count;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use futures::StreamExt;

    // Build prompt with conversation history
    let mut history = sess.clone_history().await;
    let mut input = history.get_history_for_prompt();

    // Early check for empty history - nothing to extract from
    if input.is_empty() {
        warn!("Handoff extraction: history is EMPTY - nothing to extract from");
        return Err("No conversation history to extract from. Please have a conversation first before using /handoff.".to_string());
    }

    tracing::info!(
        "Handoff extraction: starting with {} history messages",
        input.len()
    );

    let config = sess.config().await;
    let handoff_config = &config.handoff;

    // Resolve client to use: configured handoff model or current session's model
    let client: ModelClient =
        if handoff_config.model.is_some() || handoff_config.model_provider.is_some() {
            // Use configured handoff model
            let provider_id = handoff_config
                .model_provider
                .as_deref()
                .unwrap_or(&config.model_provider_id);

            let provider = config
                .model_providers
                .get(provider_id)
                .ok_or_else(|| format!("Handoff provider '{provider_id}' not found in config"))?
                .clone();

            let model_name = handoff_config
                .model
                .as_deref()
                .or(config.model.as_deref())
                .ok_or_else(|| "No model configured for handoff".to_string())?;

            let model_family = sess
                .services
                .models_manager
                .construct_model_family(model_name, &config)
                .await;

            info!(
                "Handoff extraction: using configured model {} (provider: {})",
                model_name, provider_id
            );

            ModelClient::new(
                Arc::new((*config).clone()),
                Some(Arc::clone(&sess.services.auth_manager)),
                model_family,
                sess.services.otel_event_manager.clone(),
                provider,
                None, // no reasoning effort for extraction
                ReasoningSummary::None,
                sess.conversation_id(),
                SessionSource::Exec,
            )
        } else {
            // Use current session's client
            info!(
                "Handoff extraction: using session model {}",
                ctx.client.get_model()
            );
            ctx.client.clone()
        };

    // Get context window from configured model or client
    let model_context = handoff_config
        .model_context_window
        .unwrap_or_else(|| client.get_model_context_window());
    // Reserve ~30% for extraction prompt, response, and token estimation error
    let budget = (model_context as usize).saturating_sub(model_context as usize * 3 / 10);

    // Log model and context info
    let model_name = client.get_model();
    let instruction_tokens = approx_token_count(prompt);
    debug!(
        "Handoff extraction: model={}, context_window={}, budget={}, instruction_tokens={}",
        model_name, model_context, budget, instruction_tokens
    );

    // Truncate history if too long (Head + Tail strategy)
    // Conservative token budget for extraction to avoid context overflow while allowing enough context.

    let total_estimate: usize = input
        .iter()
        .map(|item| approx_token_count(&serde_json::to_string(item).unwrap_or_default()))
        .sum();

    // Apply correction factor: our 4-bytes-per-token estimate is often ~25% low
    // Real tokenizers typically yield ~3 bytes per token for mixed content
    let total_estimate = total_estimate + total_estimate / 4;

    debug!(
        "Handoff extraction: history_messages={}, history_tokens_estimate={}, total_with_instructions={}",
        input.len(),
        total_estimate,
        total_estimate + instruction_tokens
    );

    if total_estimate > budget {
        debug!("Handoff extraction: history exceeds budget, truncating...");

        // Use TailHeavy truncation: 20% head, 80% tail
        // This preserves initial context/goal and recent work
        let bias = TruncationBias::TailHeavy;
        let head_budget = (budget as f64 * bias.head_ratio()) as usize;
        let tail_budget = budget.saturating_sub(head_budget);

        // Collect head items (first messages up to head_budget)
        let mut head = Vec::new();
        let mut head_tokens = 0;
        for item in input.iter() {
            let cost = approx_token_count(&serde_json::to_string(item).unwrap_or_default());
            // Apply correction factor
            let cost = cost + cost / 4;
            if head_tokens + cost > head_budget {
                break;
            }
            head.push(item.clone());
            head_tokens += cost;
        }

        // Collect tail items (recent messages up to tail_budget, in reverse)
        let mut tail = Vec::new();
        let mut tail_tokens = 0;
        let skip_count = head.len();
        for item in input.iter().skip(skip_count).rev() {
            let cost = approx_token_count(&serde_json::to_string(item).unwrap_or_default());
            // Apply correction factor
            let cost = cost + cost / 4;
            if tail_tokens + cost > tail_budget {
                break;
            }
            tail.push(item.clone());
            tail_tokens += cost;
        }
        tail.reverse();

        let omitted_count = input.len().saturating_sub(head.len() + tail.len());
        debug!(
            "Handoff truncation: head={} msgs ({}t), tail={} msgs ({}t), omitted={}",
            head.len(),
            head_tokens,
            tail.len(),
            tail_tokens,
            omitted_count
        );

        // Build truncated input: head + marker + tail
        let mut truncated = head;
        if omitted_count > 0 {
            truncated.push(ResponseItem::Message {
                id: None,
                role: "system".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("[... {omitted_count} messages omitted for context limit ...]"),
                }],
            });
        }
        truncated.extend(tail);
        input = truncated;
    }

    let extraction_prompt = Prompt {
        input,
        tools: vec![],
        base_instructions_override: Some(prompt.to_string()),
        output_schema: Some(extraction_result_schema()),
    };

    // Stream the response
    let mut stream = client
        .stream(&extraction_prompt)
        .await
        .map_err(|e| format!("Failed to start extraction: {e}"))?;

    let mut response_text = String::new();
    let mut received_deltas = false;
    let mut event_count = 0;
    while let Some(event) = stream.next().await {
        event_count += 1;
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                debug!("Handoff extraction: OutputItemDone event #{event_count}");
                // Only use OutputItemDone if we haven't received streaming deltas
                // (to avoid double accumulation)
                if let codex_protocol::models::ResponseItem::Message { content, .. } = &item {
                    for c in content {
                        if let ContentItem::OutputText { text, .. } = c {
                            debug!(
                                "Handoff extraction: OutputItemDone text len={}, content={:?}",
                                text.len(),
                                &text.chars().take(100).collect::<String>()
                            );
                            // Use OutputItemDone content if we haven't accumulated from deltas
                            // or if deltas were empty
                            if !received_deltas || response_text.is_empty() {
                                response_text.push_str(text);
                            }
                        }
                    }
                }
            }
            Ok(ResponseEvent::OutputTextDelta(delta)) => {
                if !received_deltas {
                    debug!(
                        "Handoff extraction: first OutputTextDelta received, len={}",
                        delta.len()
                    );
                }
                received_deltas = true;
                debug!(
                    "Handoff extraction: OutputTextDelta len={}, content={:?}",
                    delta.len(),
                    &delta.chars().take(50).collect::<String>()
                );
                response_text.push_str(&delta);
            }
            Ok(ResponseEvent::Completed { .. }) => {
                debug!(
                    "Handoff extraction: Completed event, total events={event_count}, response_text len={}",
                    response_text.len()
                );
                break;
            }
            Err(e) => return Err(format!("Extraction stream error: {e}")),
            other => {
                debug!(
                    "Handoff extraction: other event #{event_count}: {:?}",
                    std::mem::discriminant(&other)
                );
                continue;
            }
        }
    }

    if response_text.is_empty() {
        warn!(
            "Handoff extraction: response_text is EMPTY after {event_count} events (received_deltas={received_deltas})"
        );
        return Err("Extraction returned empty response. Please try again.".to_string());
    } else {
        tracing::info!(
            "Handoff extraction: response_text len={}, first 200 chars: {:?}",
            response_text.len(),
            &response_text.chars().take(200).collect::<String>()
        );
    }

    // Parse JSON from response
    let result = parse_extraction_result(&response_text)?;
    tracing::info!(
        "Handoff extraction: parsed summary len={}, files={}",
        result.summary.len(),
        result.files.len()
    );
    Ok(result)
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
