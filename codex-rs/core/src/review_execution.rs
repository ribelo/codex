use std::sync::Arc;

use async_channel::Receiver;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentMessageDeltaEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::codex_delegate::run_codex_thread_one_shot;
use crate::config::Config;
use crate::config::Constrained;
use crate::error::CodexErr;
use crate::features::Feature;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewEventForwarding {
    ForwardToParent,
    Suppress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewApprovalMode {
    ForceNever,
    InheritParent,
}

pub(crate) async fn run_review_delegate(
    session: Arc<Session>,
    parent_turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
    event_forwarding: ReviewEventForwarding,
    approval_mode: ReviewApprovalMode,
) -> Result<Option<ReviewOutputEvent>, CodexErr> {
    let receiver = start_review_conversation(
        Arc::clone(&session),
        Arc::clone(&parent_turn_context),
        input,
        cancellation_token,
        approval_mode,
    )
    .await?;

    Ok(process_review_events(session, parent_turn_context, receiver, event_forwarding).await)
}

async fn start_review_conversation(
    session: Arc<Session>,
    parent_turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
    approval_mode: ReviewApprovalMode,
) -> Result<Receiver<Event>, CodexErr> {
    let sub_agent_config = build_review_delegate_config(
        parent_turn_context.config.as_ref(),
        &parent_turn_context.model_info.slug,
        approval_mode,
    );

    let io = run_codex_thread_one_shot(
        sub_agent_config,
        Arc::clone(&session.services.auth_manager),
        Arc::clone(&session.services.models_manager),
        input,
        session,
        parent_turn_context,
        cancellation_token,
        SubAgentSource::Review,
        None,
        None,
    )
    .await?;

    Ok(io.rx_event)
}

fn build_review_delegate_config(
    parent_config: &Config,
    parent_model_slug: &str,
    approval_mode: ReviewApprovalMode,
) -> Config {
    let mut sub_agent_config = parent_config.clone();
    // Carry over review-only feature restrictions so the delegate cannot
    // re-enable blocked tools (web search, collab tools).
    if let Err(err) = sub_agent_config
        .web_search_mode
        .set(WebSearchMode::Disabled)
    {
        panic!("by construction Constrained<WebSearchMode> must always support Disabled: {err}");
    }
    let _ = sub_agent_config.features.disable(Feature::Collab);

    // Set explicit review rubric for the sub-agent.
    sub_agent_config.base_instructions = Some(crate::REVIEW_PROMPT.to_string());
    if approval_mode == ReviewApprovalMode::ForceNever {
        sub_agent_config.permissions.approval_policy =
            Constrained::allow_only(AskForApproval::Never);
    }

    let model = parent_config
        .review_model
        .clone()
        .unwrap_or_else(|| parent_model_slug.to_string());
    sub_agent_config.model = Some(model);
    sub_agent_config.model_reasoning_effort = sub_agent_config.resolved_review_reasoning_effort();
    sub_agent_config
}

async fn process_review_events(
    session: Arc<Session>,
    parent_turn_context: Arc<TurnContext>,
    receiver: Receiver<Event>,
    event_forwarding: ReviewEventForwarding,
) -> Option<ReviewOutputEvent> {
    let mut prev_agent_message: Option<EventMsg> = None;
    let mut last_agent_message: Option<String> = None;
    let forward_parent_events = event_forwarding == ReviewEventForwarding::ForwardToParent;

    while let Ok(event) = receiver.recv().await {
        match event.msg {
            EventMsg::AgentMessage(agent_message) => {
                last_agent_message = Some(agent_message.message.clone());
                if forward_parent_events {
                    if let Some(prev) = prev_agent_message.take() {
                        session.send_event(parent_turn_context.as_ref(), prev).await;
                    }
                    prev_agent_message = Some(EventMsg::AgentMessage(agent_message));
                }
            }
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::AgentMessage(agent_message),
                ..
            }) => {
                last_agent_message = review_item_text(&agent_message);
            }
            // Suppress ItemCompleted only for assistant messages: forwarding it
            // would trigger legacy AgentMessage via as_legacy_events(), which this
            // review flow intentionally hides in favor of structured output.
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { .. })
            | EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent { .. }) => {}
            EventMsg::TurnComplete(task_complete) => {
                let out = last_agent_message
                    .or(task_complete.last_agent_message)
                    .as_deref()
                    .map(parse_review_output_event);
                return out;
            }
            EventMsg::TurnAborted(_) => {
                return None;
            }
            other => {
                if forward_parent_events {
                    session
                        .send_event(parent_turn_context.as_ref(), other)
                        .await;
                }
            }
        }
    }

    None
}

fn review_item_text(agent_message: &codex_protocol::items::AgentMessageItem) -> Option<String> {
    let text = agent_message
        .content
        .iter()
        .map(|content| match content {
            AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect::<String>();
    if text.is_empty() { None } else { Some(text) }
}

/// Parse a ReviewOutputEvent from a text blob returned by the reviewer model.
/// If the text is valid JSON matching ReviewOutputEvent, deserialize it.
/// Otherwise, attempt to extract the first JSON object substring and parse it.
/// If parsing still fails, return a structured fallback carrying the plain text
/// in `overall_explanation`.
pub(crate) fn parse_review_output_event(text: &str) -> ReviewOutputEvent {
    if let Ok(ev) = serde_json::from_str::<ReviewOutputEvent>(text) {
        return ev;
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
        && let Ok(ev) = serde_json::from_str::<ReviewOutputEvent>(slice)
    {
        return ev;
    }
    ReviewOutputEvent {
        overall_explanation: text.to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::make_session_and_context;
    use crate::config::test_config;
    use async_channel::unbounded;
    use codex_protocol::items::AgentMessageItem;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::TurnCompleteEvent;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[tokio::test]
    async fn process_review_events_uses_completed_agent_message_when_turn_complete_is_empty() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let (tx, rx) = unbounded();
        let review_json = serde_json::json!({
            "findings": [],
            "overall_correctness": "good",
            "overall_explanation": "Recovered from completed item",
            "overall_confidence_score": 0.9
        })
        .to_string();

        tx.send_blocking(Event {
            id: "evt-1".to_string(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: session.conversation_id,
                turn_id: turn.sub_id.clone(),
                item: TurnItem::AgentMessage(AgentMessageItem {
                    id: "msg-1".to_string(),
                    content: vec![AgentMessageContent::Text {
                        text: review_json.clone(),
                    }],
                    phase: None,
                    memory_citation: None,
                }),
            }),
        })
        .expect("send completed item");
        tx.send_blocking(Event {
            id: "evt-2".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                last_agent_message: None,
            }),
        })
        .expect("send turn complete");
        drop(tx);

        let output = process_review_events(session, turn, rx, ReviewEventForwarding::Suppress)
            .await
            .expect("review output should be recovered");

        assert_eq!(
            output.overall_explanation,
            "Recovered from completed item".to_string()
        );
        assert_eq!(output.overall_correctness, "good".to_string());
        assert_eq!(output.overall_confidence_score, 0.9);
    }

    #[test]
    fn inherited_review_delegate_preserves_parent_approval_policy() {
        let mut config = test_config();
        config
            .permissions
            .approval_policy
            .set(AskForApproval::UnlessTrusted)
            .expect("test config should allow approval policy override");

        let delegate_config =
            build_review_delegate_config(&config, "gpt-5-codex", ReviewApprovalMode::InheritParent);

        assert_eq!(
            delegate_config.permissions.approval_policy.value(),
            AskForApproval::UnlessTrusted
        );
    }

    #[test]
    fn forced_review_delegate_uses_never_approval_policy() {
        let mut config = test_config();
        config
            .permissions
            .approval_policy
            .set(AskForApproval::OnRequest)
            .expect("test config should allow approval policy override");

        let delegate_config =
            build_review_delegate_config(&config, "gpt-5-codex", ReviewApprovalMode::ForceNever);

        assert_eq!(
            delegate_config.permissions.approval_policy.value(),
            AskForApproval::Never
        );
    }
}
