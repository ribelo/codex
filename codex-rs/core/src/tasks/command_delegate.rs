use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::codex::TurnContext;
use crate::codex_delegate::run_codex_conversation_one_shot;
use crate::state::TaskKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubagentEventPayload;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::user_input::UserInput;
use uuid::Uuid;

use super::SessionTask;
use super::SessionTaskContext;

pub(crate) struct CommandDelegateTask {
    pub(crate) description: String,
    pub(crate) prompt: String,
    pub(crate) agent: Option<String>,
    pub(crate) profile: Option<String>,
}

#[async_trait]
impl SessionTask for CommandDelegateTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let parent = session.clone_session();

        parent
            .send_event(
                ctx.as_ref(),
                EventMsg::TaskStarted(TaskStartedEvent {
                    model_context_window: Some(ctx.client.get_model_context_window()),
                }),
            )
            .await;

        let mut sub_config = ctx.client.config().as_ref().clone();
        sub_config.model = Some(ctx.client.get_model());

        let mut emit_subagent_trace = false;
        let mut subagent_call_id = String::new();
        let mut subagent_delegation_id: Option<String> = None;
        let task_description = summarize_task_description(&self.prompt);
        let mut trace_type: Option<String> = None;
        if let Some(ref agent) = self.agent {
            emit_subagent_trace = true;
            subagent_call_id = format!("command:{}:{}", self.description, Uuid::new_v4());
            subagent_delegation_id = Some(Uuid::new_v4().to_string());
            trace_type = Some(agent.clone());

            let registry = crate::subagents::SubagentRegistry::new(&sub_config.codex_home);
            let Some(agent_def) = registry.get(agent) else {
                parent
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message: format!("Agent '{agent}' not found"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    )
                    .await;
                return None;
            };

            sub_config.base_instructions = Some(agent_def.system_prompt.clone());

            match agent_def
                .metadata
                .load_profile(&sub_config.codex_home)
                .await
            {
                Ok(Some(profile)) => {
                    if let Err(message) = apply_profile_settings(&mut sub_config, &profile, agent) {
                        parent
                            .send_event(
                                ctx.as_ref(),
                                EventMsg::Error(ErrorEvent {
                                    message,
                                    codex_error_info: Some(CodexErrorInfo::Other),
                                }),
                            )
                            .await;
                        return None;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    parent
                        .send_event(
                            ctx.as_ref(),
                            EventMsg::Error(ErrorEvent {
                                message: format!("Subagent configuration error for '{agent}': {e}"),
                                codex_error_info: Some(CodexErrorInfo::Other),
                            }),
                        )
                        .await;
                    return None;
                }
            }
        } else if let Some(ref profile_name) = self.profile {
            emit_subagent_trace = true;
            subagent_call_id = format!("command:{}:{}", self.description, Uuid::new_v4());
            subagent_delegation_id = Some(Uuid::new_v4().to_string());
            trace_type = Some(format!("profile:{profile_name}"));

            let Some(profile) =
                crate::custom_prompts::load_profile_by_name(&sub_config.codex_home, profile_name)
                    .await
            else {
                parent
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message: format!("Profile '{profile_name}' not found"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    )
                    .await;
                return None;
            };

            if let Err(message) = apply_profile_settings(&mut sub_config, &profile, profile_name) {
                parent
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message,
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    )
                    .await;
                return None;
            }
        }

        if emit_subagent_trace && let Some(trace_type) = trace_type.as_deref() {
            let wrapped = wrap_subagent_event(
                &subagent_call_id,
                trace_type,
                &task_description,
                subagent_delegation_id.clone(),
                None,
                Some(0),
                EventMsg::TaskStarted(TaskStartedEvent {
                    model_context_window: None,
                }),
            );
            parent.send_event(ctx.as_ref(), wrapped).await;
        }

        let input = vec![UserInput::Text {
            text: self.prompt.clone(),
        }];

        let rx = match run_codex_conversation_one_shot(
            "command",
            sub_config,
            session.auth_manager(),
            session.models_manager(),
            input,
            Arc::clone(&parent),
            ctx.clone(),
            cancellation_token.clone(),
            None,
        )
        .await
        {
            Ok(io) => io.rx_event,
            Err(e) => {
                parent
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message: format!("Failed to start command session: {e}"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    )
                    .await;
                return None;
            }
        };

        let mut output: Option<String> = None;
        while let Ok(Event { id: _, msg }) = rx.recv().await {
            if emit_subagent_trace && let Some(trace_type) = trace_type.as_deref() {
                match msg {
                    EventMsg::SubagentEvent(mut nested) => {
                        let nested_depth = nested.depth.unwrap_or(0).saturating_add(1);
                        nested.depth = Some(nested_depth);
                        if nested.parent_delegation_id.is_none() {
                            nested.parent_delegation_id = subagent_delegation_id.clone();
                        }
                        parent
                            .send_event(ctx.as_ref(), EventMsg::SubagentEvent(nested))
                            .await;
                    }
                    EventMsg::TaskComplete(ev) => {
                        output = ev.last_agent_message.clone();
                        parent
                            .send_event(
                                ctx.as_ref(),
                                wrap_subagent_event(
                                    &subagent_call_id,
                                    trace_type,
                                    &task_description,
                                    subagent_delegation_id.clone(),
                                    None,
                                    Some(0),
                                    EventMsg::TaskComplete(ev),
                                ),
                            )
                            .await;
                        break;
                    }
                    EventMsg::TurnAborted(ev) => {
                        parent
                            .send_event(
                                ctx.as_ref(),
                                wrap_subagent_event(
                                    &subagent_call_id,
                                    trace_type,
                                    &task_description,
                                    subagent_delegation_id.clone(),
                                    None,
                                    Some(0),
                                    EventMsg::TurnAborted(ev),
                                ),
                            )
                            .await;
                        return None;
                    }
                    EventMsg::ExecCommandBegin(_)
                    | EventMsg::ExecCommandEnd(_)
                    | EventMsg::ExecCommandOutputDelta(_)
                    | EventMsg::McpToolCallBegin(_)
                    | EventMsg::McpToolCallEnd(_)
                    | EventMsg::PatchApplyBegin(_)
                    | EventMsg::PatchApplyEnd(_)
                    | EventMsg::AgentMessageDelta(_)
                    | EventMsg::AgentMessage(_) => {
                        parent
                            .send_event(
                                ctx.as_ref(),
                                wrap_subagent_event(
                                    &subagent_call_id,
                                    trace_type,
                                    &task_description,
                                    subagent_delegation_id.clone(),
                                    None,
                                    Some(0),
                                    msg,
                                ),
                            )
                            .await;
                    }
                    EventMsg::TaskStarted(_) | EventMsg::SessionConfigured(_) => {}
                    _ => {}
                }
            } else {
                match msg {
                    EventMsg::TaskComplete(ev) => {
                        output = ev.last_agent_message;
                        break;
                    }
                    EventMsg::TurnAborted(_) => {
                        return None;
                    }
                    // Show progress in the parent TUI while the command runs.
                    other => {
                        parent.send_event(ctx.as_ref(), other).await;
                    }
                }
            }
        }

        if cancellation_token.is_cancelled() {
            return None;
        }

        let description = self.description.as_str();
        let user_id = Some(format!("command:{description}:user"));
        let assistant_id = Some(format!("command:{description}:assistant"));
        let assistant_text =
            output.unwrap_or_else(|| "Command completed without output.".to_string());

        parent
            .record_conversation_items(
                &ctx,
                &[ResponseItem::Message {
                    id: user_id,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: self.prompt.clone(),
                    }],
                }],
            )
            .await;

        parent
            .record_response_item_and_emit_turn_item(
                ctx.as_ref(),
                ResponseItem::Message {
                    id: assistant_id,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: assistant_text,
                        signature: None,
                    }],
                },
            )
            .await;

        None
    }

    async fn abort(&self, _session: Arc<SessionTaskContext>, _ctx: Arc<TurnContext>) {}
}

fn wrap_subagent_event(
    parent_call_id: &str,
    subagent_name: &str,
    task_description: &str,
    delegation_id: Option<String>,
    parent_delegation_id: Option<String>,
    depth: Option<i32>,
    inner: EventMsg,
) -> EventMsg {
    EventMsg::SubagentEvent(SubagentEventPayload {
        parent_call_id: parent_call_id.to_string(),
        subagent_name: subagent_name.to_string(),
        task_description: task_description.to_string(),
        delegation_id,
        parent_delegation_id,
        depth,
        inner: Box::new(inner),
    })
}

fn summarize_task_description(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return "Run command".to_string();
    }
    let first_line = trimmed.lines().next().unwrap_or(trimmed).trim();
    if first_line.len() <= 120 {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..117])
    }
}

fn apply_profile_settings(
    sub_config: &mut crate::config::Config,
    profile: &crate::config::profile::ConfigProfile,
    profile_name: &str,
) -> Result<(), String> {
    if let Some(ref model) = profile.model {
        sub_config.model = Some(model.clone());
    }

    if let Some(ref provider_id) = profile.model_provider {
        if let Some(provider_info) = sub_config.model_providers.get(provider_id) {
            sub_config.model_provider_id = provider_id.clone();
            sub_config.model_provider = provider_info.clone();
        } else {
            return Err(format!(
                "Model provider '{provider_id}' from profile '{profile_name}' not found"
            ));
        }
    }

    if profile.model_reasoning_effort.is_some() {
        sub_config.model_reasoning_effort = profile.model_reasoning_effort;
    }
    if let Some(summary) = profile.model_reasoning_summary {
        sub_config.model_reasoning_summary = summary;
    }
    if profile.model_verbosity.is_some() {
        sub_config.model_verbosity = profile.model_verbosity;
    }

    Ok(())
}
