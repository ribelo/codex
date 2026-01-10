/// Generate subagent identity prompt with parent session context.
fn subagent_identity_prompt(parent_session_id: &str) -> String {
    format!(
        r#"# Subagent Context

You are a specialized subagent delegated by a parent agent to perform a specific task.
- Parent session ID: {parent_session_id}
- Focus strictly on completing the requested task.
- Provide a clear, concise summary of what you accomplished.
- You have your own conversation context separate from the parent agent.
"#
    )
}

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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
use crate::model_provider_info::ProviderKind;
use crate::model_provider_info::built_in_model_providers;
use crate::subagent_result::SubagentExecutionOutcome;
use crate::subagent_result::SubagentTaskResult;
use crate::subagents::SubagentRegistry;
use crate::subagents::SubagentSandboxPolicy;
use crate::subagents::SubagentSession;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::spec::JsonSchema;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SubagentEventPayload;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;

pub fn create_task_tool(codex_home: &Path, allowed_subagents: Option<&[String]>) -> ToolSpec {
    let registry = SubagentRegistry::new(codex_home);
    let agents_list: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|a| {
            match allowed_subagents {
                // If an allowlist exists, it is the sole source of truth.
                // Show the agent if it's allowed, regardless of internal status.
                Some(allowed) => allowed.contains(&a.slug),
                // If no allowlist, hide internal agents by default.
                None => !a.metadata.is_internal(),
            }
        })
        .collect();

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
        "Delegate a task to a specialized subagent. Returns the agent's output and a session_id.\n\n\
         Available subagents:\n{agents_desc}\n\n\
         Usage notes:\n\
         - Subagents run in the same working directory as the parent session.\n\
         - Pass session_id to continue a previous conversation with the same subagent.\n\
         - If merge conflicts occur, you will be notified and may need to resolve them."
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
        "subagent_name".to_string(),
        JsonSchema::String {
            description: Some("The type of subagent to use".to_string()),
        },
    );
    properties.insert(
        "agent_name".to_string(),
        JsonSchema::String {
            description: Some("Optional alias for the subagent".to_string()),
        },
    );
    properties.insert(
        "session_id".to_string(),
        JsonSchema::String {
            description: Some(
                "Session ID to continue an existing subagent conversation. \
                 Pass a previous session_id to preserve conversation history \
                 for multi-turn tasks, iterative refinement, or follow-up work."
                    .to_string(),
            ),
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
                "subagent_name".to_string(),
            ]),
            additional_properties: Some(false.into()),
        },
    })
}

#[derive(serde::Deserialize)]
struct TaskArgs {
    description: String,
    prompt: String,
    subagent_name: String,
    #[allow(dead_code)] // Kept for backwards compatibility with tools that pass it
    agent_name: Option<String>,
    session_id: Option<String>,
}

pub struct TaskHandler;

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

/// Returns the restrictiveness level for an approval policy (lower = more restrictive).
/// Used to ensure subagents can only tighten, not loosen approval requirements.
fn approval_restrictiveness(policy: AskForApproval) -> i32 {
    match policy {
        // Rank by AUTONOMY (ability to act without user approval)
        // Lower values = less autonomy = more user supervision required
        //
        // UnlessTrusted: Zero autonomy - must ask for EVERYTHING
        AskForApproval::UnlessTrusted => 0,
        // Never: Sandbox autonomy only - can execute in sandbox but cannot escalate
        AskForApproval::Never => 1,
        // OnRequest: Sandbox autonomy + can request escalation
        AskForApproval::OnRequest => 2,
        // OnFailure: Sandbox autonomy + auto-escalates on failure
        AskForApproval::OnFailure => 3,
    }
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
        let _invocation_order = invocation
            .turn
            .task_invocation_counter
            .fetch_add(1, Ordering::SeqCst);

        let codex_home = turn.client.config().codex_home.clone();

        let registry = SubagentRegistry::new(&codex_home);
        let subagent_def = registry.get(&args.subagent_name).ok_or_else(|| {
            let available: Vec<String> = registry.list().iter().map(|a| a.slug.clone()).collect();
            let available_str = if available.is_empty() {
                "(none found)".to_string()
            } else {
                available.join(", ")
            };
            FunctionCallError::RespondToModel(format!(
                "Unknown subagent_name '{}'. Available subagents: {}. Ensure a matching .md exists in ~/.codex/agents",
                args.subagent_name, available_str
            ))
        })?;

        info!(
            subagent = %args.subagent_name,
            profile = ?subagent_def.metadata.profile,
            "Task handler: resolved subagent definition"
        );

        // Enforce allowed_subagents restriction at execution time.
        // This prevents the model from bypassing restrictions by guessing subagent names.
        let config = turn.client.config();

        // Block internal agents from being spawned via the task tool.
        // Internal agents can only be spawned if explicitly listed in allowed_subagents.
        // This allows orchestrator agents to use internal subagents while protecting
        // root sessions from accidentally spawning them.
        let is_allowed = match config.allowed_subagents.as_ref() {
            // If allowlist is defined, it is the sole source of truth
            Some(allowed) => allowed.contains(&args.subagent_name),
            // If no allowlist, block internal agents
            None => !subagent_def.metadata.is_internal(),
        };

        if !is_allowed {
            if subagent_def.metadata.is_internal() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "Subagent '{}' is internal and cannot be spawned directly. Use the appropriate dedicated tool or add it to 'allowed_subagents'.",
                    args.subagent_name
                )));
            } else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "Subagent '{}' is not allowed by policy.",
                    args.subagent_name
                )));
            }
        }

        let is_new_session = args.session_id.is_none();
        let mut session_id = args
            .session_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Create or retrieve delegation context
        let delegation_context = {
            let registry = &invocation.session.services.delegation_registry;
            registry
                .enter_with_parent(invocation.call_id.clone(), None)
                .await
        };
        let delegation_id = Some(delegation_context.delegation_id.clone());
        let parent_delegation_id = delegation_context.parent_delegation_id.clone();
        let depth = Some(delegation_context.depth);

        let effective_cwd = turn.client.config().cwd.clone();
        let _ = is_new_session; // suppress unused warning

        let (subagent_session_ref, is_reused): (Arc<crate::codex::Codex>, bool) = {
            let services = &invocation.session.services;
            let mut sessions = services.subagent_sessions.lock().await;

            if let Some(session) = sessions.get(&session_id) {
                info!(
                    subagent = %args.subagent_name,
                    session_id = %session_id,
                    "Reusing existing subagent session"
                );
                let codex = session.codex.clone();
                drop(sessions);
                (codex, true)
            } else {
                let spawn_started = Instant::now();
                let config = turn.client.config();
                let mut sub_config = (*config).clone();
                sub_config.codex_home = codex_home.clone();
                sub_config.cwd = effective_cwd.clone();

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
                            args.subagent_name
                        ))
                    })?
                {
                    info!(
                        subagent = %args.subagent_name,
                        profile_name = ?subagent_def.metadata.profile,
                        profile_model = ?profile.model,
                        "Task handler: loaded profile configuration"
                    );

                    // Apply model from profile
                    if let Some(ref canonical_model_id) = profile.model {
                        // Profiles store model IDs in canonical form (`{provider}/{model}`), but the
                        // rest of the system expects the provider-specific model slug.
                        let (provider_id, model_name) =
                            crate::model_provider_info::parse_canonical_model_id(
                                canonical_model_id,
                            )
                            .map_err(|e| {
                                FunctionCallError::RespondToModel(format!(
                                    "Invalid model format in profile: {e}"
                                ))
                            })?;

                        sub_config.model = Some(model_name.to_string());

                        // Build combined providers map
                        let mut providers = built_in_model_providers();
                        for (key, provider) in config.model_providers.iter() {
                            providers
                                .entry(key.clone())
                                .or_insert_with(|| provider.clone());
                        }

                        if let Some(provider_info) = providers.get(provider_id) {
                            sub_config.model_provider_id = provider_id.to_string();
                            sub_config.model_provider = provider_info.clone();
                        } else {
                            return Err(FunctionCallError::Fatal(format!(
                                "Model provider '{provider_id}' from model '{canonical_model_id}' not found"
                            )));
                        }

                        info!(
                            subagent = %args.subagent_name,
                            canonical_model_id = %canonical_model_id,
                            model_name = %model_name,
                            provider_id = %provider_id,
                            "Task handler: applied model from profile"
                        );
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
                        subagent = %args.subagent_name,
                        profile_name = ?subagent_def.metadata.profile,
                        "Task handler: no profile configured or profile returned None"
                    );
                }

                let parent_session_id = invocation.session.conversation_id().to_string();
                let identity_prompt = subagent_identity_prompt(&parent_session_id);

                // Do not override `instructions` for the subagent session. Some model families
                // (e.g. GPT-5.x) reject custom `instructions` entirely. Put subagent context in
                // a developer message instead so it consistently reaches the model.
                let developer_prefix = if subagent_def.system_prompt.is_empty() {
                    identity_prompt
                } else {
                    format!("{}\n\n{}", subagent_def.system_prompt, identity_prompt)
                };
                sub_config.developer_instructions = Some(match sub_config.developer_instructions {
                    Some(existing) => format!("{existing}\n\n{developer_prefix}"),
                    None => developer_prefix,
                });
                sub_config.base_instructions = None;

                // Apply sandbox_policy override (only if more restrictive than parent)
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

                // Warn if subagent is using Gemini/Antigravity without OAuth
                // Warn if subagent is using Gemini without OAuth (using API key instead)
                check_gemini_api_key_warning(
                    &sub_config.model_provider.provider_kind,
                    &invocation.session.services.auth_manager,
                    &args.subagent_name,
                    &invocation.session,
                    &invocation.turn,
                )
                .await;

                let session_token = CancellationToken::new();

                let (codex, conversation_id) = run_codex_conversation_interactive(
                    &args.subagent_name,
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
                if is_new_session {
                    session_id = conversation_id.to_string();
                }

                let session = SubagentSession {
                    codex: codex_arc.clone(),
                    cancellation_token: session_token,
                    session_id: session_id.clone(),
                };

                sessions.insert(session_id.clone(), session);
                info!(
                    subagent = %args.subagent_name,
                    session_id = %session_id,
                    elapsed_ms = spawn_started.elapsed().as_millis(),
                    "Spawned subagent session"
                );
                (codex_arc, false)
            }
        };

        let _ = is_reused; // suppress unused warning

        let input = vec![UserInput::Text {
            text: args.prompt.clone(),
        }];

        // Send initial TaskStarted event so the TUI can create the cell immediately.
        // This ensures the parent cell exists before any nested subagent events arrive.
        let task_started = wrap_subagent_event(
            &invocation.call_id,
            &args.subagent_name,
            &args.description,
            delegation_id.clone(),
            parent_delegation_id.clone(),
            depth,
            Some(session_id.clone()),
            EventMsg::TaskStarted(TaskStartedEvent {
                model_context_window: None,
            }),
        );
        invocation
            .session
            .send_event(invocation.turn.as_ref(), task_started)
            .await;

        let exec_outcome = async {
            subagent_session_ref
                .submit(Op::UserInput { items: input })
                .await
                .map_err(|e| {
                    FunctionCallError::Fatal(format!("Failed to submit input to subagent: {e}"))
                })?;

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
                    EventMsg::TaskComplete(tc) => {
                        // Send a wrapped TaskComplete so the TUI can mark the cell as completed
                        let wrapped = wrap_subagent_event(
                            &invocation.call_id,
                            &args.subagent_name,
                            &args.description,
                            delegation_id.clone(),
                            parent_delegation_id.clone(),
                            depth,
                            Some(session_id.clone()),
                            EventMsg::TaskComplete(tc.clone()),
                        );
                        invocation
                            .session
                            .send_event(invocation.turn.as_ref(), wrapped)
                            .await;
                        break Ok(
                            match (
                                tc.last_agent_message.filter(|s| !s.trim().is_empty()),
                                tc.last_tool_output,
                            ) {
                                (Some(message), _) => {
                                    SubagentExecutionOutcome::CompletedWithMessage { message }
                                }
                                (None, Some(tool_output)) => {
                                    SubagentExecutionOutcome::CompletedWithToolOutput {
                                        tool_output,
                                    }
                                }
                                (None, None) => SubagentExecutionOutcome::CompletedEmpty,
                            },
                        );
                    }
                    EventMsg::TurnAborted(ta) => {
                        // Send a wrapped TurnAborted so the TUI can mark the cell as failed
                        let wrapped = wrap_subagent_event(
                            &invocation.call_id,
                            &args.subagent_name,
                            &args.description,
                            delegation_id.clone(),
                            parent_delegation_id.clone(),
                            depth,
                            Some(session_id.clone()),
                            EventMsg::TurnAborted(ta.clone()),
                        );
                        invocation
                            .session
                            .send_event(invocation.turn.as_ref(), wrapped)
                            .await;
                        break Ok(SubagentExecutionOutcome::from(ta.reason));
                    }
                    EventMsg::Error(ErrorEvent {
                        message,
                        codex_error_info,
                    }) => {
                        // Send a wrapped Error so the TUI can display it
                        let wrapped = wrap_subagent_event(
                            &invocation.call_id,
                            &args.subagent_name,
                            &args.description,
                            delegation_id.clone(),
                            parent_delegation_id.clone(),
                            depth,
                            Some(session_id.clone()),
                            EventMsg::Error(ErrorEvent {
                                message: message.clone(),
                                codex_error_info: codex_error_info.clone(),
                            }),
                        );
                        invocation
                            .session
                            .send_event(invocation.turn.as_ref(), wrapped)
                            .await;
                        // Determine if session can be resumed based on error type
                        let can_resume = matches!(
                            codex_error_info,
                            Some(CodexErrorInfo::ResponseStreamDisconnected { .. })
                                | Some(CodexErrorInfo::InternalServerError)
                        );
                        break Ok(SubagentExecutionOutcome::Failed {
                            reason: message,
                            can_resume,
                        });
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
                            &args.subagent_name,
                            &args.description,
                            delegation_id.clone(),
                            parent_delegation_id.clone(),
                            depth,
                            Some(session_id.clone()),
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
        }
        .await?;

        // Handle interrupted case
        if let SubagentExecutionOutcome::Interrupted = exec_outcome {
            let result = SubagentTaskResult::new(exec_outcome, session_id);
            return Ok(ToolOutput::Function {
                success: Some(false),
                content: result.to_string(),
                content_items: None,
            });
        }

        // Handle failed case
        if let SubagentExecutionOutcome::Failed { .. } = exec_outcome {
            let result = SubagentTaskResult::new(exec_outcome, session_id);
            return Ok(ToolOutput::Function {
                success: Some(false),
                content: result.to_string(),
                content_items: None,
            });
        }

        let result = SubagentTaskResult::new(exec_outcome, session_id);

        Ok(ToolOutput::Function {
            success: Some(true),
            content: result.to_string(),
            content_items: None,
        })
    }
}

/// Check if subagent is using Gemini provider with API key instead of OAuth and emit warning.
async fn check_gemini_api_key_warning(
    provider_kind: &ProviderKind,
    auth_manager: &std::sync::Arc<crate::AuthManager>,
    subagent_name: &str,
    session: &Arc<crate::codex::Session>,
    turn: &Arc<crate::codex::TurnContext>,
) {
    let auth = auth_manager.auth();

    let warning_message = match provider_kind {
        ProviderKind::Gemini => {
            let has_oauth = auth.as_ref().is_some_and(|a| a.gemini_account_count() > 0);
            let has_api_key = std::env::var("GEMINI_API_KEY").is_ok();

            if !has_oauth && has_api_key {
                Some(format!(
                    "Subagent '{subagent_name}' is using GEMINI_API_KEY instead of OAuth. \
                     Run `codex login gemini` for better rate limits."
                ))
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(message) = warning_message {
        session
            .send_event(turn.as_ref(), EventMsg::Warning(WarningEvent { message }))
            .await;
    }
}
