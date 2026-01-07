//! Handler for the `read_session` tool that spawns a session_reader subagent
//! to analyze past session logs.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::codex_delegate::run_codex_conversation_interactive;
use crate::function_tool::FunctionCallError;
use crate::rollout::list::find_conversation_path_by_id_str;
use crate::subagents::SubagentRegistry;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::spec::JsonSchema;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SubagentEventPayload;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::user_input::UserInput;

/// Creates the tool specification for read_session.
pub fn create_read_session_tool() -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "session_id".to_string(),
        JsonSchema::String {
            description: Some("The session ID (UUID) to read".to_string()),
        },
    );
    properties.insert(
        "question".to_string(),
        JsonSchema::String {
            description: Some("Question to answer about the session".to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "read_session".to_string(),
        description: "Query a past session's history. Spawns an internal agent to analyze the session log and answer your question.".to_string(),
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["session_id".to_string(), "question".to_string()]),
            additional_properties: Some(false.into()),
        },
    })
}

#[derive(Deserialize)]
struct ReadSessionArgs {
    session_id: String,
    question: String,
}

pub struct ReadSessionHandler;

/// Helper to wrap an inner event with subagent context.
fn wrap_subagent_event(
    parent_call_id: &str,
    subagent_name: &str,
    task_description: &str,
    delegation_id: Option<String>,
    parent_delegation_id: Option<String>,
    depth: Option<i32>,
    session_id: Option<String>,
    inner: EventMsg,
) -> EventMsg {
    EventMsg::SubagentEvent(SubagentEventPayload {
        parent_call_id: parent_call_id.to_string(),
        subagent_name: subagent_name.to_string(),
        task_description: task_description.to_string(),
        delegation_id,
        parent_delegation_id,
        depth,
        session_id,
        inner: Box::new(inner),
    })
}

#[async_trait]
impl ToolHandler for ReadSessionHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "read_session handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: ReadSessionArgs = serde_json::from_str(&arguments).map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "Failed to parse read_session arguments: {e}"
            ))
        })?;

        let turn = invocation.turn.clone();
        let config = turn.client.config();
        let codex_home = config.codex_home.clone();

        // Check for recursion: block reading current session
        let current_session_id = invocation.session.conversation_id.to_string();
        if args.session_id == current_session_id {
            return Err(FunctionCallError::RespondToModel(
                "Cannot read the current session to avoid infinite recursion".to_string(),
            ));
        }

        // Resolve session_id to file path
        let session_path = find_conversation_path_by_id_str(&codex_home, &args.session_id)
            .await
            .map_err(|e| {
                FunctionCallError::RespondToModel(format!("Failed to search for session: {e}"))
            })?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "Session '{}' not found in ~/.codex/sessions/",
                    args.session_id
                ))
            })?;

        info!(
            session_id = %args.session_id,
            path = %session_path.display(),
            "read_session: resolved session path"
        );

        // Look up the session_reader agent
        let registry = SubagentRegistry::new(&codex_home);
        let subagent_def = registry.get("session_reader").ok_or_else(|| {
            FunctionCallError::Fatal(
                "Internal agent 'session_reader' not found. Ensure built-in agents are installed."
                    .to_string(),
            )
        })?;

        // Create delegation context
        let delegation_context = {
            let registry = &invocation.session.services.delegation_registry;
            registry
                .enter_with_parent(invocation.call_id.clone(), None)
                .await
        };
        let delegation_id = Some(delegation_context.delegation_id.clone());
        let parent_delegation_id = delegation_context.parent_delegation_id.clone();
        let depth = Some(delegation_context.depth);

        // Build subagent config
        let mut sub_config = (*config).clone();
        sub_config.codex_home = codex_home.clone();

        // Apply profile settings from frontmatter.
        if let Some(profile) = subagent_def
            .metadata
            .load_profile(&codex_home)
            .await
            .map_err(|e| {
                FunctionCallError::RespondToModel(format!(
                    "Subagent configuration error for 'session_reader': {e}"
                ))
            })?
        {
            info!(
                subagent = "session_reader",
                profile_name = ?subagent_def.metadata.profile,
                profile_model = ?profile.model,
                profile_provider = ?profile.model_provider,
                "read_session: loaded profile configuration"
            );

            // Apply model from profile
            if let Some(ref model) = profile.model {
                sub_config.model = Some(model.clone());
                info!(
                    subagent = "session_reader",
                    model = %model,
                    "read_session: applied model from profile"
                );
            }

            // Apply model_provider from profile
            if let Some(ref provider_id) = profile.model_provider {
                // Build combined providers map
                let mut providers = crate::model_provider_info::built_in_model_providers();
                for (key, provider) in config.model_providers.iter() {
                    providers
                        .entry(key.clone())
                        .or_insert_with(|| provider.clone());
                }

                if let Some(provider_info) = providers.get(provider_id) {
                    sub_config.model_provider_id = provider_id.clone();
                    sub_config.model_provider = provider_info.clone();
                } else {
                    return Err(FunctionCallError::Fatal(format!(
                        "Model provider '{provider_id}' from profile not found"
                    )));
                }
            }

            // Apply reasoning settings from profile
            if profile.model_reasoning_effort.is_some() {
                sub_config.model_reasoning_effort = profile.model_reasoning_effort;
            }
            if let Some(summary) = profile.model_reasoning_summary {
                sub_config.model_reasoning_summary = summary;
            }
            if profile.model_verbosity.is_some() {
                sub_config.model_verbosity = profile.model_verbosity;
            }
        }

        // Use the session_reader's system prompt
        sub_config.base_instructions = Some(subagent_def.system_prompt.clone());

        // Apply sandbox_policy from session_reader metadata (read-only)
        if let Some(sandbox_policy) = subagent_def.metadata.sandbox_policy.to_sandbox_policy() {
            sub_config.sandbox_policy = sandbox_policy;
        }

        // Apply approval_policy from session_reader metadata (never)
        if let Some(approval_policy) = subagent_def.metadata.approval_policy.to_ask_for_approval() {
            sub_config.approval_policy = approval_policy;
        }

        // No subagent access for session_reader
        sub_config.allowed_subagents = Some(vec![]);

        // Bind the session log path - this enables session_log tools and disables shell
        sub_config.session_log_path = Some(session_path.clone());

        let session_token = CancellationToken::new();
        let spawn_started = Instant::now();

        let (codex, conversation_id) = run_codex_conversation_interactive(
            "session_reader",
            sub_config,
            invocation.session.services.auth_manager.clone(),
            invocation.session.services.models_manager.clone(),
            invocation.session.clone(),
            invocation.turn.clone(),
            session_token.clone(),
            None,
        )
        .await
        .map_err(|e| FunctionCallError::Fatal(format!("Failed to spawn session_reader: {e}")))?;

        let session_id_str = conversation_id.to_string();

        info!(
            session_id = %session_id_str,
            elapsed_ms = spawn_started.elapsed().as_millis(),
            "Spawned session_reader subagent"
        );

        let task_description = format!("Reading session {}", args.session_id);

        // Send TaskStarted event
        let task_started = wrap_subagent_event(
            &invocation.call_id,
            "session_reader",
            &task_description,
            delegation_id.clone(),
            parent_delegation_id.clone(),
            depth,
            Some(session_id_str.clone()),
            EventMsg::TaskStarted(TaskStartedEvent {
                model_context_window: None,
            }),
        );
        invocation
            .session
            .send_event(invocation.turn.as_ref(), task_started)
            .await;

        // Build the prompt for session_reader
        let prompt = format!(
            "Analyze the session log using your available tools (search_session_log, read_session_log).\n\nQuestion: {}",
            args.question
        );

        // Submit the prompt
        let input = vec![UserInput::Text { text: prompt }];
        codex
            .submit(Op::UserInput { items: input })
            .await
            .map_err(|e| {
                FunctionCallError::Fatal(format!("Failed to submit prompt to session_reader: {e}"))
            })?;

        // Wait for completion and collect output
        let mut final_output = String::new();
        loop {
            let event = codex.next_event().await.map_err(|e| {
                FunctionCallError::Fatal(format!("Session reader event error: {e}"))
            })?;

            match event.msg {
                EventMsg::SubagentEvent(mut nested) => {
                    // Forward nested subagent events with adjusted depth
                    let parent_depth = depth.unwrap_or(0);
                    let nested_depth = nested.depth.unwrap_or(0);
                    let new_depth = parent_depth.saturating_add(nested_depth).saturating_add(1);
                    nested.depth = Some(new_depth);
                    if nested.parent_delegation_id.is_none() {
                        nested.parent_delegation_id = delegation_id.clone();
                    }

                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), EventMsg::SubagentEvent(nested))
                        .await;
                }
                EventMsg::AgentMessage(ref msg) => {
                    final_output = msg.message.clone();
                    let wrapped = wrap_subagent_event(
                        &invocation.call_id,
                        "session_reader",
                        &task_description,
                        delegation_id.clone(),
                        parent_delegation_id.clone(),
                        depth,
                        Some(session_id_str.clone()),
                        event.msg.clone(),
                    );
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), wrapped)
                        .await;
                }
                EventMsg::TaskComplete(_) => {
                    let wrapped = wrap_subagent_event(
                        &invocation.call_id,
                        "session_reader",
                        &task_description,
                        delegation_id.clone(),
                        parent_delegation_id.clone(),
                        depth,
                        Some(session_id_str.clone()),
                        event.msg,
                    );
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), wrapped)
                        .await;
                    break;
                }
                _ => {
                    // Forward other events wrapped
                    let wrapped = wrap_subagent_event(
                        &invocation.call_id,
                        "session_reader",
                        &task_description,
                        delegation_id.clone(),
                        parent_delegation_id.clone(),
                        depth,
                        Some(session_id_str.clone()),
                        event.msg,
                    );
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), wrapped)
                        .await;
                }
            }
        }

        // Cancel the subagent session
        session_token.cancel();

        // Build result
        let result = serde_json::json!({
            "result": final_output,
            "session_id": session_id_str,
        });

        Ok(ToolOutput::Function {
            content: result.to_string(),
            content_items: None,
            success: Some(true),
        })
    }
}
