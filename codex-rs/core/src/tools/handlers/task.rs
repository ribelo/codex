/// Sandbox and approvals documentation to prepend to subagent prompts.
const SANDBOX_AND_APPROVALS_PROMPT: &str = include_str!("../../../sandbox_and_approvals.md");

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::codex_delegate::run_codex_conversation_interactive;
use crate::function_tool::FunctionCallError;
use crate::model_provider_info::built_in_model_providers;
use crate::subagents::SubagentRegistry;
use crate::subagents::SubagentSandboxPolicy;
use crate::subagents::SubagentSession;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::spec::JsonSchema;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SubagentEventPayload;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::user_input::UserInput;

pub fn create_task_tool(codex_home: &Path, allowed_subagents: Option<&[String]>) -> ToolSpec {
    let registry = SubagentRegistry::new(codex_home);
    let agents_list: Vec<_> = match allowed_subagents {
        // None means full access to all agents
        None => registry.list(),
        // Some([...]) means only include agents in the allowed list
        Some(allowed) => registry
            .list()
            .into_iter()
            .filter(|a| allowed.contains(&a.slug))
            .collect(),
    };

    let agents_desc = if agents_list.is_empty() {
        "(No subagents found in ~/.codex/agents)".to_string()
    } else {
        agents_list
            .iter()
            .map(|a| {
                format!(
                    "- {}: {}",
                    a.slug,
                    a.metadata
                        .description
                        .as_deref()
                        .unwrap_or("No description provided")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let description = format!(
        "Delegate a task to a specialized subagent. Returns the agent's output and a session_id.\n\nAvailable subagents:\n{agents_desc}\n\nWhen to use:\n- When you are instructed to execute custom slash commands or use a specific subagent."
    );

    let mut properties = std::collections::BTreeMap::new();
    properties.insert(
        "description".to_string(),
        JsonSchema::String {
            description: Some("Description of the task".to_string()),
        },
    );
    properties.insert(
        "prompt".to_string(),
        JsonSchema::String {
            description: Some("The prompt for the subagent".to_string()),
        },
    );
    properties.insert(
        "subagent_type".to_string(),
        JsonSchema::String {
            description: Some("The type of subagent to use".to_string()),
        },
    );
    properties.insert(
        "session_id".to_string(),
        JsonSchema::String {
            description: Some("Existing session ID to continue".to_string()),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "task".to_string(),
        description,
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec![
                "description".to_string(),
                "prompt".to_string(),
                "subagent_type".to_string(),
            ]),
            additional_properties: Some(false.into()),
        },
    })
}

#[derive(serde::Deserialize)]
struct TaskArgs {
    description: String,
    prompt: String,
    subagent_type: String,
    session_id: Option<String>,
}

pub struct TaskHandler;

/// Helper to wrap an inner event with subagent context.
fn wrap_subagent_event(
    parent_call_id: &str,
    subagent_type: &str,
    task_description: &str,
    delegation_id: Option<String>,
    parent_delegation_id: Option<String>,
    depth: Option<i32>,
    inner: EventMsg,
) -> EventMsg {
    EventMsg::SubagentEvent(SubagentEventPayload {
        parent_call_id: parent_call_id.to_string(),
        subagent_type: subagent_type.to_string(),
        task_description: task_description.to_string(),
        delegation_id,
        parent_delegation_id,
        depth,
        inner: Box::new(inner),
    })
}

#[async_trait]
impl ToolHandler for TaskHandler {
    fn kind(&self) -> crate::tools::registry::ToolKind {
        crate::tools::registry::ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let arguments = match invocation.payload {
            ToolPayload::Function { arguments } => arguments,
            _ => return Err(FunctionCallError::Fatal("Invalid payload type".to_string())),
        };

        let args: TaskArgs = serde_json::from_str(&arguments)
            .map_err(|e| FunctionCallError::Fatal(format!("Failed to parse arguments: {e}")))?;

        let turn = &invocation.turn;
        let codex_home = turn.client.config().codex_home.clone();

        let registry = SubagentRegistry::new(&codex_home);
        let subagent_def = registry.get(&args.subagent_type).ok_or_else(|| {
            let available: Vec<String> = registry.list().iter().map(|a| a.slug.clone()).collect();
            let available_str = if available.is_empty() {
                "(none found)".to_string()
            } else {
                available.join(", ")
            };
            FunctionCallError::RespondToModel(format!(
                "Unknown subagent_type '{}'. Available subagents: {}. Ensure a matching .md exists in ~/.codex/agents",
                args.subagent_type, available_str
            ))
        })?;

        info!(
            subagent = %args.subagent_type,
            profile = ?subagent_def.metadata.profile,
            "Task handler: resolved subagent definition"
        );

        // Enforce allowed_subagents restriction at execution time.
        // This prevents the model from bypassing restrictions by guessing subagent names.
        let config = turn.client.config();
        if let Some(ref allowed) = config.allowed_subagents
            && !allowed.contains(&args.subagent_type)
        {
            let allowed_str = if allowed.is_empty() {
                "(none)".to_string()
            } else {
                allowed.join(", ")
            };
            return Err(FunctionCallError::RespondToModel(format!(
                "Subagent '{}' is not allowed. Allowed subagents: {}",
                args.subagent_type, allowed_str
            )));
        }

        let session_id = args
            .session_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Create or retrieve delegation context
        let delegation_context = {
            let registry = &invocation.session.services.delegation_registry;
            registry.enter(invocation.call_id.clone()).await
        };
        let delegation_id = Some(delegation_context.delegation_id.clone());
        let parent_delegation_id = delegation_context.parent_delegation_id.clone();
        let depth = Some(delegation_context.depth);

        let subagent_session_ref: Arc<crate::codex::Codex> = {
            let services = &invocation.session.services;
            let mut sessions = services.subagent_sessions.lock().await;

            if let Some(session) = sessions.get(&session_id) {
                info!(
                    subagent = %args.subagent_type,
                    session_id = %session_id,
                    "Reusing existing subagent session"
                );
                session.codex.clone()
            } else {
                let spawn_started = Instant::now();
                let config = turn.client.config();
                let mut sub_config = (*config).clone();
                sub_config.codex_home = codex_home.clone();

                // Apply profile settings from frontmatter.
                // We re-read config.toml on each invocation rather than caching because:
                // 1. The config file is small and TOML parsing is fast
                // 2. This ensures we always use fresh config if user edits it mid-session
                if let Some(profile) = subagent_def
                    .metadata
                    .load_profile(&codex_home)
                    .await
                    .map_err(|e| {
                        FunctionCallError::RespondToModel(format!(
                            "Subagent configuration error for '{}': {e}",
                            args.subagent_type
                        ))
                    })?
                {
                    info!(
                        subagent = %args.subagent_type,
                        profile_name = ?subagent_def.metadata.profile,
                        profile_model = ?profile.model,
                        profile_provider = ?profile.model_provider,
                        "Task handler: loaded profile configuration"
                    );

                    // Apply model from profile
                    if let Some(ref model) = profile.model {
                        sub_config.model = Some(model.clone());
                        info!(
                            subagent = %args.subagent_type,
                            model = %model,
                            "Task handler: applied model from profile"
                        );
                    }

                    // Apply model_provider from profile
                    if let Some(ref provider_id) = profile.model_provider {
                        // Build combined providers map
                        let mut providers = built_in_model_providers();
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
                } else {
                    info!(
                        subagent = %args.subagent_type,
                        profile_name = ?subagent_def.metadata.profile,
                        "Task handler: no profile configured or profile returned None"
                    );
                }

                // Determine the base instructions for the subagent.
                // If the agent file has a custom system prompt, use it.
                // Otherwise, fall back to the model family's default instructions.
                let base_prompt = if subagent_def.system_prompt.is_empty() {
                    // Use the parent session's base instructions as fallback.
                    // This inherits the model-appropriate prompt from the parent.
                    config.base_instructions.clone().unwrap_or_default()
                } else {
                    subagent_def.system_prompt.clone()
                };

                sub_config.base_instructions =
                    Some(format!("{base_prompt}\n\n{SANDBOX_AND_APPROVALS_PROMPT}"));

                // Apply sandbox_policy override (only if more restrictive than parent)
                // Apply sandbox_policy override (only if more restrictive than parent).
                // If subagent specifies `Inherit`, we keep the parent's policy.
                if let Some(subagent_sandbox_policy) =
                    subagent_def.metadata.sandbox_policy.to_sandbox_policy()
                {
                    let parent_sandbox =
                        SubagentSandboxPolicy::from_sandbox_policy(&config.sandbox_policy);
                    let subagent_restrictiveness =
                        subagent_def.metadata.sandbox_policy.restrictiveness();
                    // Only apply if subagent policy is more restrictive (lower value).
                    // `Inherit` (None restrictiveness) never overrides.
                    if let Some(sub_level) = subagent_restrictiveness
                        && sub_level <= parent_sandbox.restrictiveness().unwrap_or(i32::MAX)
                    {
                        sub_config.sandbox_policy = subagent_sandbox_policy;
                    }
                }

                // Apply approval_policy override (only if more restrictive than parent).
                // If subagent specifies `Inherit`, we keep the parent's policy.
                if let Some(subagent_approval_policy) =
                    subagent_def.metadata.approval_policy.to_ask_for_approval()
                {
                    let subagent_restrictiveness =
                        subagent_def.metadata.approval_policy.restrictiveness();
                    let parent_restrictiveness = approval_restrictiveness(config.approval_policy);
                    // Only apply if subagent policy is more restrictive (lower value).
                    // `Inherit` (None restrictiveness) never overrides.
                    if let Some(sub_level) = subagent_restrictiveness
                        && sub_level <= parent_restrictiveness
                    {
                        sub_config.approval_policy = subagent_approval_policy;
                    }
                }

                // Set allowed_subagents for the subagent session.
                // Use the subagent metadata if specified, otherwise default to no access.
                sub_config.allowed_subagents = subagent_def
                    .metadata
                    .allowed_subagents
                    .clone()
                    .or(Some(vec![]));

                let session_token = CancellationToken::new();

                let codex = run_codex_conversation_interactive(
                    sub_config,
                    invocation.session.services.auth_manager.clone(),
                    invocation.session.services.models_manager.clone(),
                    invocation.session.clone(),
                    invocation.turn.clone(),
                    session_token.clone(),
                    None,
                )
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("Failed to spawn subagent: {e}")))?;

                let codex_arc = Arc::new(codex);
                let session = SubagentSession {
                    codex: codex_arc.clone(),
                    cancellation_token: session_token,
                    session_id: session_id.clone(),
                };

                sessions.insert(session_id.clone(), session);
                info!(
                    subagent = %args.subagent_type,
                    session_id = %session_id,
                    elapsed_ms = spawn_started.elapsed().as_millis(),
                    "Spawned subagent session"
                );
                codex_arc
            }
        };

        let input = vec![UserInput::Text {
            text: args.prompt.clone(),
        }];

        // Send initial TaskStarted event so the TUI can create the cell immediately.
        // This ensures the parent cell exists before any nested subagent events arrive.
        let task_started = wrap_subagent_event(
            &invocation.call_id,
            &args.subagent_type,
            &args.description,
            delegation_id.clone(),
            parent_delegation_id.clone(),
            depth,
            EventMsg::TaskStarted(TaskStartedEvent {
                model_context_window: None,
            }),
        );
        invocation
            .session
            .send_event(invocation.turn.as_ref(), task_started)
            .await;

        subagent_session_ref
            .submit(Op::UserInput { items: input })
            .await
            .map_err(|e| {
                FunctionCallError::Fatal(format!("Failed to submit input to subagent: {e}"))
            })?;

        let mut final_output = String::new();
        loop {
            let event = subagent_session_ref
                .next_event()
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("Subagent event error: {e}")))?;

            match event.msg {
                EventMsg::SubagentEvent(mut nested) => {
                    // Nested subagent invocation inside this delegated task.
                    // Bump depth relative to the current delegation so the TUI
                    // can render a visually nested tree.
                    let parent_depth = depth.unwrap_or(0);
                    let new_depth = parent_depth.saturating_add(1);
                    nested.depth = Some(new_depth);
                    if nested.parent_delegation_id.is_none() {
                        nested.parent_delegation_id = delegation_id.clone();
                    }

                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), EventMsg::SubagentEvent(nested))
                        .await;
                }
                EventMsg::TaskComplete(tc) => {
                    if let Some(ref msg) = tc.last_agent_message {
                        final_output = msg.clone();
                    }
                    // Send a wrapped TaskComplete so the TUI can mark the cell as completed
                    let wrapped = wrap_subagent_event(
                        &invocation.call_id,
                        &args.subagent_type,
                        &args.description,
                        delegation_id.clone(),
                        parent_delegation_id.clone(),
                        depth,
                        EventMsg::TaskComplete(tc),
                    );
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), wrapped)
                        .await;
                    // Exit delegation context
                    {
                        invocation.session.services.delegation_registry.exit().await;
                    }
                    break;
                }
                EventMsg::TurnAborted(ta) => {
                    let reason_str = format!("Turn aborted: {:?}", ta.reason);
                    // Send a wrapped TurnAborted so the TUI can mark the cell as failed
                    let wrapped = wrap_subagent_event(
                        &invocation.call_id,
                        &args.subagent_type,
                        &args.description,
                        delegation_id.clone(),
                        parent_delegation_id.clone(),
                        depth,
                        EventMsg::TurnAborted(ta),
                    );
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), wrapped)
                        .await;
                    // Exit delegation context
                    {
                        invocation.session.services.delegation_registry.exit().await;
                    }
                    return Err(FunctionCallError::RespondToModel(reason_str));
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
                    let wrapped = wrap_subagent_event(
                        &invocation.call_id,
                        &args.subagent_type,
                        &args.description,
                        delegation_id.clone(),
                        parent_delegation_id.clone(),
                        depth,
                        event.msg,
                    );
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), wrapped)
                        .await;
                }
                _ => {}
            }
        }

        // Build a structured response that includes the session_id for follow-up calls
        let response = serde_json::json!({
            "result": final_output,
            "session_id": session_id,
        });

        Ok(ToolOutput::Function {
            success: Some(true),
            content: response.to_string(),
            content_items: None,
        })
    }
}

/// Returns the restrictiveness level for an approval policy (lower = more restrictive).
/// Used to ensure subagents can only tighten, not loosen approval requirements.
fn approval_restrictiveness(policy: AskForApproval) -> i32 {
    match policy {
        // UnlessTrusted is most restrictive - asks for approval on almost everything
        AskForApproval::UnlessTrusted => 0,
        // OnRequest - model decides when to ask
        AskForApproval::OnRequest => 1,
        // OnFailure - auto-approve in sandbox, escalate on failure
        AskForApproval::OnFailure => 2,
        // Never - never ask, most permissive
        AskForApproval::Never => 3,
    }
}
