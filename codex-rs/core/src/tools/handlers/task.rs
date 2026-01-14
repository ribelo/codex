/// Generate worker identity prompt with parent session context.
fn parallel_execution_constraints(parallel_worker_count: i32) -> Option<String> {
    if parallel_worker_count <= 1 {
        return None;
    }

    Some(format!(
        r#"# Parallel Execution Constraints

This parent request spawned {parallel_worker_count} worker sessions in parallel.
- Assume other workers may be editing the repo at the same time.
- Avoid editing the same files or shared contracts unless explicitly required.
- Keep changes narrowly scoped to your assigned area; do not do opportunistic refactors.
- If you detect overlap or a dependency on another worker, stop and report it back to the parent."#
    ))
}

fn subagent_identity_prompt(parent_session_id: &str, parallel_worker_count: i32) -> String {
    let parallel_constraints = parallel_execution_constraints(parallel_worker_count)
        .map(|constraints| format!("\n\n{constraints}"))
        .unwrap_or_default();

    format!(
        r#"# Worker Context

You are an internal worker session delegated by a parent agent to execute a specific task.
- Parent session ID: {parent_session_id}

Guidelines:
- Focus on the requested task.
- Use available tools as needed, respecting sandbox and approval policies.
- If the task requires a specific output format, follow it exactly. Otherwise, summarize what you did and why.
{parallel_constraints}
"#
    )
}

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
use crate::config::types::TaskConfigToml;
use crate::config::types::TaskDifficulty;
use crate::function_tool::FunctionCallError;
use crate::model_provider_info::ProviderKind;
use crate::openai_models::model_family::ANTIGRAVITY_PREAMBLE;
use crate::openai_models::model_family::InstructionMode;
use crate::openai_models::model_family::PRAXIS_REST;
use crate::subagent_result::SubagentExecutionOutcome;
use crate::subagent_result::SubagentTaskResult;
use crate::subagents::SubagentSession;
use crate::task_registry::LoadedSkill;
use crate::task_registry::TaskRegistry;
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

fn build_subagent_prompt_plan(
    instruction_mode: InstructionMode,
    subagent_system_prompt: &str,
    identity_prompt: &str,
    task_prompt: &str,
    model_family_base_instructions: &str,
) -> (Option<String>, String) {
    match instruction_mode {
        InstructionMode::Strict => {
            let subagent_context = if subagent_system_prompt.is_empty() {
                identity_prompt.to_string()
            } else {
                format!("{subagent_system_prompt}\n\n{identity_prompt}")
            };

            (
                None,
                format!("<agent_context>\n{subagent_context}\n</agent_context>\n\n{task_prompt}"),
            )
        }
        InstructionMode::Prefix => {
            let body = if subagent_system_prompt.is_empty() {
                PRAXIS_REST
            } else {
                subagent_system_prompt
            };
            (
                Some(format!(
                    "{ANTIGRAVITY_PREAMBLE}\n\n{body}\n\n{identity_prompt}"
                )),
                task_prompt.to_string(),
            )
        }
        InstructionMode::Flexible => {
            let base = if subagent_system_prompt.is_empty() {
                model_family_base_instructions
            } else {
                subagent_system_prompt
            };

            (
                Some(format!("{base}\n\n{identity_prompt}")),
                task_prompt.to_string(),
            )
        }
    }
}

fn build_task_system_prompt(task_description: Option<&str>, skills: &[LoadedSkill]) -> String {
    let mut system_prompt = String::new();
    if let Some(task_description) = task_description {
        let task_description = task_description.trim();
        if !task_description.is_empty() {
            system_prompt.push_str(task_description);
        }
    }

    for skill in skills {
        if !system_prompt.is_empty() {
            system_prompt.push_str("\n\n");
        }

        let name = &skill.name;
        let path = skill.path.display();
        let contents = &skill.contents;
        system_prompt.push_str(&format!(
            "<skill>\n<name>{name}</name>\n<path>{path}</path>\n{contents}\n</skill>"
        ));
    }

    system_prompt
}

pub fn create_task_tool(
    tasks: &[TaskConfigToml],
    allowed_task_types: Option<&[String]>,
    public_skill_names: &[String],
) -> ToolSpec {
    use crate::task_registry::BUILTIN_TASKS;
    use std::collections::BTreeMap;

    let mut task_descs: BTreeMap<String, String> = BTreeMap::new();
    for builtin in BUILTIN_TASKS {
        task_descs.insert(
            builtin.task_type.to_string(),
            builtin.description.to_string(),
        );
    }
    for task in tasks {
        if let Some(description) = task
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            task_descs.insert(task.task_type.clone(), description.to_string());
        } else {
            task_descs
                .entry(task.task_type.clone())
                .or_insert_with(|| "No description provided".to_string());
        }
    }

    task_descs.retain(|task_type, _| {
        allowed_task_types.is_none_or(|allowed| allowed.contains(task_type))
    });

    let task_types_desc = if task_descs.is_empty() {
        "(No task types configured)".to_string()
    } else {
        task_descs
            .iter()
            .map(|(task_type, description)| format!("- {task_type}: {description}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let description = format!(
        "Submit a task to an internal worker session. Returns the worker output and a session_id.\n\n\
         Available task types:\n{task_types_desc}\n\n\
         Difficulty:\n\
         - small: trivial\n\
         - medium: standard (default)\n\
         - large: complex\n\n\
         Usage notes:\n\
         - Workers run in the same working directory as the parent session.\n\
         - Pass session_id to continue a previous conversation with the same worker.\n\
         - If merge conflicts occur, you will be notified and may need to resolve them."
    );

    let mut properties = std::collections::BTreeMap::new();
    properties.insert(
        "description".to_string(),
        JsonSchema::String {
            description: Some("Description of the task".to_string()),
            enum_values: None,
        },
    );
    properties.insert(
        "prompt".to_string(),
        JsonSchema::String {
            description: Some("The prompt for the worker".to_string()),
            enum_values: None,
        },
    );
    properties.insert(
        "task_type".to_string(),
        JsonSchema::String {
            description: Some("Category of work (e.g. rust, search, ui)".to_string()),
            enum_values: None,
        },
    );
    properties.insert(
        "difficulty".to_string(),
        JsonSchema::String {
            description: Some(
                "Task complexity. small=trivial, medium=standard (default), large=complex"
                    .to_string(),
            ),
            enum_values: Some(vec![
                "small".to_string(),
                "medium".to_string(),
                "large".to_string(),
            ]),
        },
    );
    properties.insert(
        "session_id".to_string(),
        JsonSchema::String {
            description: Some(
                "Session ID to continue an existing worker conversation. \
                 Pass a previous session_id to preserve conversation history \
                 for multi-turn tasks, iterative refinement, or follow-up work."
                    .to_string(),
            ),
            enum_values: None,
        },
    );
    // Add skills field - only valid for task_type="general"
    if !public_skill_names.is_empty() {
        properties.insert(
            "skills".to_string(),
            JsonSchema::Array {
                items: Box::new(JsonSchema::String {
                    description: None,
                    enum_values: Some(public_skill_names.to_vec()),
                }),
                description: Some(
                    "Skills to inject into the worker session. Only valid for task_type=\"general\"."
                        .to_string(),
                ),
            },
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: "task".to_string(),
        description,
        strict: false,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec![
                "description".to_string(),
                "prompt".to_string(),
                "task_type".to_string(),
            ]),
            additional_properties: Some(false.into()),
        },
    })
}

#[derive(serde::Deserialize)]
struct TaskArgs {
    description: String,
    prompt: String,
    task_type: String,
    #[serde(default)]
    difficulty: Option<TaskDifficulty>,
    session_id: Option<String>,
    #[serde(default)]
    skills: Option<Vec<String>>,
}

pub struct TaskHandler;

/// Helper to wrap an inner event with worker context.
fn wrap_subagent_event(
    parent_call_id: &str,
    task_type: &str,
    difficulty: TaskDifficulty,
    task_description: &str,
    task_prompt: Option<&str>,
    delegation_id: Option<String>,
    parent_delegation_id: Option<String>,
    depth: Option<i32>,
    session_id: Option<String>,
    inner: EventMsg,
) -> EventMsg {
    EventMsg::SubagentEvent(SubagentEventPayload {
        parent_call_id: parent_call_id.to_string(),
        task_type: task_type.to_string(),
        difficulty: difficulty.to_string(),
        task_description: task_description.to_string(),
        task_prompt: task_prompt.map(ToString::to_string),
        delegation_id,
        parent_delegation_id,
        depth,
        session_id,
        inner: Box::new(inner),
    })
}

/// Returns the restrictiveness level for an approval policy (lower = more restrictive).
/// Used to ensure workers can only tighten, not loosen approval requirements.
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

fn validate_requested_skills(
    task_type: &str,
    requested_skills: Option<&[String]>,
    available_skills: &[String],
) -> Result<(), FunctionCallError> {
    if requested_skills.is_some() && task_type != "general" {
        return Err(FunctionCallError::RespondToModel(format!(
            "The 'skills' parameter is only valid for task_type=\"general\", not \"{task_type}\"."
        )));
    }

    if let Some(skills) = requested_skills {
        for skill_name in skills {
            if !available_skills.contains(skill_name) {
                let available_str = if available_skills.is_empty() {
                    "(none)".to_string()
                } else {
                    available_skills.join(", ")
                };
                return Err(FunctionCallError::RespondToModel(format!(
                    "Unknown skill '{skill_name}'. Available skills: {available_str}."
                )));
            }
        }
    }

    Ok(())
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

        let TaskArgs {
            description,
            prompt,
            task_type,
            difficulty: difficulty_override,
            session_id,
            skills: requested_skills,
        } = args;

        let task_type = task_type.trim().to_string();
        if task_type.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "task_type cannot be empty".to_string(),
            ));
        }

        validate_requested_skills(
            &task_type,
            requested_skills.as_deref(),
            &turn.tools_config.task_tool_skill_names,
        )?;

        let requested_difficulty = difficulty_override.unwrap_or_default();

        let config = turn.client.config();
        let allowed_task_types = config.allowed_subagents.as_deref();
        if let Some(allowed_task_types) = allowed_task_types
            && !allowed_task_types.contains(&task_type)
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "Task type '{task_type}' is not allowed by policy.",
            )));
        }

        let registry = TaskRegistry::new(config.as_ref());
        let task_def = registry.get(&task_type).ok_or_else(|| {
            let available: Vec<&str> = registry
                .available_task_types(allowed_task_types)
                .into_iter()
                .map(|t| t.task_type.as_str())
                .collect();
            let available_str = if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            };
            FunctionCallError::RespondToModel(format!(
                "Unknown task_type '{task_type}'. Available task types: {available_str}.",
            ))
        })?;

        let is_new_session = session_id.is_none();
        let mut session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());

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

        let codex_home = config.codex_home.clone();
        let effective_cwd = config.cwd.clone();

        let parent_session_id = invocation.session.conversation_id().to_string();
        invocation
            .turn
            .wait_task_tool_calls_finalized_for_request()
            .await;
        let parallel_worker_count = invocation.turn.task_tool_calls_in_request();
        let parallel_constraints = parallel_execution_constraints(parallel_worker_count);
        let identity_prompt = subagent_identity_prompt(&parent_session_id, parallel_worker_count);

        let skills_to_load = if task_type == "general" {
            requested_skills.unwrap_or_else(|| task_def.skills.clone())
        } else {
            task_def.skills.clone()
        };

        let (loaded_skills, missing_skills) = tokio::task::spawn_blocking({
            let registry = registry.clone();
            move || {
                let mut loaded = Vec::new();
                let mut missing = Vec::new();
                for name in skills_to_load {
                    if let Some(skill) = registry.load_skill(&name) {
                        loaded.push(skill);
                    } else {
                        missing.push(name);
                    }
                }
                (loaded, missing)
            }
        })
        .await
        .map_err(|e| FunctionCallError::Fatal(format!("Failed to load skills: {e}")))?;

        let task_system_prompt_base = task_def.effective_system_prompt();
        let task_system_prompt =
            build_task_system_prompt(task_system_prompt_base.as_deref(), &loaded_skills);

        let mut reused_session = false;
        let (subagent_session_ref, instruction_mode, difficulty, spawn_warnings): (
            Arc<crate::codex::Codex>,
            InstructionMode,
            TaskDifficulty,
            Vec<String>,
        ) = {
            let services = &invocation.session.services;
            let sessions = services.subagent_sessions.lock().await;

            if let Some(session) = sessions.get(&session_id) {
                if session.task_type != task_type {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Session '{session_id}' is for task_type '{}', not '{task_type}'.",
                        session.task_type
                    )));
                }

                if let Some(requested) = difficulty_override
                    && requested != session.difficulty
                {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Session '{session_id}' is for difficulty '{}', not '{requested}'.",
                        session.difficulty
                    )));
                }

                reused_session = true;
                info!(
                    task_type = %task_type,
                    difficulty = %session.difficulty,
                    session_id = %session_id,
                    "Reusing existing task session"
                );
                let codex = session.codex.clone();
                let instruction_mode = session.instruction_mode;
                let difficulty = session.difficulty;
                drop(sessions);
                (codex, instruction_mode, difficulty, Vec::new())
            } else {
                drop(sessions);
                let strategy = registry
                    .resolve_strategy(&task_type, requested_difficulty)
                    .ok_or_else(|| {
                        FunctionCallError::Fatal(format!(
                            "Failed to resolve strategy for task_type '{task_type}'"
                        ))
                    })?;

                info!(
                    task_type = %task_type,
                    difficulty = %requested_difficulty,
                    model = ?strategy.model,
                    reasoning_effort = ?strategy.reasoning_effort,
                    "Task handler: resolved task strategy"
                );

                let spawn_started = Instant::now();
                let config = turn.client.config();
                let mut sub_config = (*config).clone();
                sub_config.codex_home = codex_home.clone();
                sub_config.cwd = effective_cwd.clone();

                if let Some(canonical_model_id) = strategy.model.as_deref() {
                    let (provider_id, model_name) =
                        crate::model_provider_info::parse_canonical_model_id(canonical_model_id)
                            .map_err(|e| {
                                FunctionCallError::RespondToModel(format!(
                                    "Invalid task model '{canonical_model_id}': {e}"
                                ))
                            })?;

                    sub_config.model = Some(model_name.to_string());

                    if provider_id == config.model_provider_id {
                        sub_config.model_provider_id = provider_id.to_string();
                        sub_config.model_provider = config.model_provider.clone();
                    } else if let Some(provider_info) = config.model_providers.get(provider_id) {
                        sub_config.model_provider_id = provider_id.to_string();
                        sub_config.model_provider = provider_info.clone();
                    } else {
                        return Err(FunctionCallError::Fatal(format!(
                            "Model provider '{provider_id}' from task model '{canonical_model_id}' not found"
                        )));
                    }

                    info!(
                        task_type = %task_type,
                        canonical_model_id = %canonical_model_id,
                        model_name = %model_name,
                        provider_id = %provider_id,
                        "Task handler: applied explicit task model"
                    );
                }

                let subagent_model = services
                    .models_manager
                    .get_model(&sub_config.model, &sub_config)
                    .await;
                let subagent_model_family = services
                    .models_manager
                    .construct_model_family(&subagent_model, &sub_config)
                    .await;
                let instruction_mode = subagent_model_family.instruction_mode;

                let (base_instructions, _) = build_subagent_prompt_plan(
                    instruction_mode,
                    &task_system_prompt,
                    &identity_prompt,
                    &prompt,
                    &subagent_model_family.base_instructions,
                );
                sub_config.base_instructions = base_instructions;

                if let Some(task_sandbox_policy) = task_def.sandbox_policy.to_sandbox_policy() {
                    let parent_sandbox =
                        crate::config::types::SubagentSandboxPolicy::from_sandbox_policy(
                            &config.sandbox_policy,
                        );
                    let task_restrictiveness = task_def.sandbox_policy.restrictiveness();
                    // Only apply if worker policy is strictly more restrictive (lower value).
                    // Equal levels should NOT override because the parent's policy may have
                    // custom fields (e.g., WorkspaceWrite { writable_roots }) that would be lost.
                    // `Inherit` (None restrictiveness) never overrides.
                    if let Some(sub_level) = task_restrictiveness
                        && sub_level < parent_sandbox.restrictiveness().unwrap_or(i32::MAX)
                    {
                        sub_config.sandbox_policy = task_sandbox_policy;
                    }
                }

                if let Some(task_approval_policy) = task_def.approval_policy.to_ask_for_approval() {
                    let task_restrictiveness = task_def.approval_policy.restrictiveness();
                    let parent_restrictiveness = approval_restrictiveness(config.approval_policy);
                    // Only apply if worker policy is strictly more restrictive (lower value).
                    // Equal levels should NOT override to maintain consistency with sandbox policy.
                    // `Inherit` (None restrictiveness) never overrides.
                    if let Some(sub_level) = task_restrictiveness
                        && sub_level < parent_restrictiveness
                    {
                        sub_config.approval_policy = task_approval_policy;
                    }
                }

                if let Some(effort) = strategy
                    .reasoning_effort
                    .and_then(crate::config::types::SubagentReasoningEffort::to_reasoning_effort)
                {
                    sub_config.model_reasoning_effort = Some(effort);
                }

                // Worker sessions inherit the parent's allowed_task_types policy.
                sub_config.allowed_subagents = config.allowed_subagents.clone();

                check_gemini_api_key_warning(
                    &sub_config.model_provider.provider_kind,
                    &invocation.session.services.auth_manager,
                    &task_type,
                    &invocation.session,
                    &invocation.turn,
                )
                .await;

                let session_token = CancellationToken::new();

                let (codex, conversation_id) = run_codex_conversation_interactive(
                    &task_type,
                    sub_config,
                    invocation.session.services.auth_manager.clone(),
                    invocation.session.services.models_manager.clone(),
                    invocation.session.clone(),
                    invocation.turn.clone(),
                    session_token.clone(),
                    None,
                )
                .await
                .map_err(|e| FunctionCallError::Fatal(format!("Failed to spawn worker: {e}")))?;

                let codex_arc = Arc::new(codex);
                if is_new_session {
                    session_id = conversation_id.to_string();
                }

                let session = SubagentSession {
                    codex: codex_arc.clone(),
                    cancellation_token: session_token,
                    session_id: session_id.clone(),
                    task_type: task_type.clone(),
                    difficulty: requested_difficulty,
                    instruction_mode,
                };

                let mut sessions = services.subagent_sessions.lock().await;
                if let Some(existing) = sessions.get(&session_id) {
                    // Another task call created this session while we were spawning.
                    // Prefer the existing session and cancel the one we just created.
                    session.cancellation_token.cancel();
                    reused_session = true;
                    info!(
                        task_type = %task_type,
                        difficulty = %existing.difficulty,
                        session_id = %session_id,
                        "Task session created concurrently; reusing existing task session"
                    );
                    (
                        existing.codex.clone(),
                        existing.instruction_mode,
                        existing.difficulty,
                        Vec::new(),
                    )
                } else {
                    sessions.insert(session_id.clone(), session);
                    info!(
                        task_type = %task_type,
                        difficulty = %requested_difficulty,
                        session_id = %session_id,
                        elapsed_ms = spawn_started.elapsed().as_millis(),
                        "Spawned task session"
                    );
                    (
                        codex_arc,
                        instruction_mode,
                        requested_difficulty,
                        missing_skills,
                    )
                }
            }
        };

        let worker_turn_prompt = if reused_session && instruction_mode != InstructionMode::Strict {
            if let Some(parallel_constraints) = parallel_constraints.as_deref() {
                format!("{parallel_constraints}\n\n{prompt}")
            } else {
                prompt.clone()
            }
        } else {
            prompt.clone()
        };

        let (_, input_text) = build_subagent_prompt_plan(
            instruction_mode,
            &task_system_prompt,
            &identity_prompt,
            &worker_turn_prompt,
            "",
        );
        let input = vec![UserInput::Text { text: input_text }];

        // Send initial TaskStarted event so the TUI can create the cell immediately.
        // This ensures the parent cell exists before any nested worker events arrive.
        let task_started = wrap_subagent_event(
            &invocation.call_id,
            &task_type,
            difficulty,
            &description,
            Some(prompt.as_str()),
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

        for name in spawn_warnings {
            let message = format!("Skill '{name}' not found.");
            let warning = wrap_subagent_event(
                &invocation.call_id,
                &task_type,
                difficulty,
                &description,
                None,
                delegation_id.clone(),
                parent_delegation_id.clone(),
                depth,
                Some(session_id.clone()),
                EventMsg::Warning(WarningEvent { message }),
            );
            invocation
                .session
                .send_event(invocation.turn.as_ref(), warning)
                .await;
        }

        let exec_outcome = async {
            subagent_session_ref
                .submit(Op::UserInput { items: input })
                .await
                .map_err(|e| {
                    FunctionCallError::Fatal(format!("Failed to submit input to task session: {e}"))
                })?;

            loop {
                let event = subagent_session_ref
                    .next_event()
                    .await
                    .map_err(|e| FunctionCallError::Fatal(format!("Worker event error: {e}")))?;

                match event.msg {
                    EventMsg::SubagentEvent(mut nested) => {
                        // Nested worker invocation inside this delegated task.
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
                            &task_type,
                            difficulty,
                            &description,
                            None,
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
                            &task_type,
                            difficulty,
                            &description,
                            None,
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
                            &task_type,
                            difficulty,
                            &description,
                            None,
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
                            &task_type,
                            difficulty,
                            &description,
                            None,
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

/// Check if a task session is using Gemini provider with API key instead of OAuth and emit warning.
async fn check_gemini_api_key_warning(
    provider_kind: &ProviderKind,
    auth_manager: &std::sync::Arc<crate::AuthManager>,
    task_type: &str,
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
                    "Task session '{task_type}' is using GEMINI_API_KEY instead of OAuth. \
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn build_subagent_prompt_plan_strict_puts_context_in_user_message() {
        let (base_instructions, user_prompt) = build_subagent_prompt_plan(
            InstructionMode::Strict,
            "SYSTEM",
            "IDENTITY",
            "TASK",
            "BASE",
        );

        assert_eq!(base_instructions, None);
        assert_eq!(
            user_prompt,
            "<agent_context>\nSYSTEM\n\nIDENTITY\n</agent_context>\n\nTASK"
        );
    }

    #[test]
    fn build_subagent_prompt_plan_prefix_sets_base_instructions() {
        let (base_instructions, user_prompt) = build_subagent_prompt_plan(
            InstructionMode::Prefix,
            "SYSTEM",
            "IDENTITY",
            "TASK",
            "BASE",
        );

        assert_eq!(
            base_instructions,
            Some(format!("{ANTIGRAVITY_PREAMBLE}\n\nSYSTEM\n\nIDENTITY"))
        );
        assert_eq!(user_prompt, "TASK");
    }

    #[test]
    fn build_subagent_prompt_plan_prefix_uses_praxis_rest_when_empty() {
        let (base_instructions, user_prompt) =
            build_subagent_prompt_plan(InstructionMode::Prefix, "", "IDENTITY", "TASK", "BASE");

        assert_eq!(
            base_instructions,
            Some(format!(
                "{ANTIGRAVITY_PREAMBLE}\n\n{PRAXIS_REST}\n\nIDENTITY"
            ))
        );
        assert_eq!(user_prompt, "TASK");
    }

    #[test]
    fn build_subagent_prompt_plan_flexible_uses_system_prompt_when_set() {
        let (base_instructions, user_prompt) = build_subagent_prompt_plan(
            InstructionMode::Flexible,
            "SYSTEM",
            "IDENTITY",
            "TASK",
            "BASE",
        );

        assert_eq!(base_instructions, Some("SYSTEM\n\nIDENTITY".to_string()));
        assert_eq!(user_prompt, "TASK");
    }

    #[test]
    fn build_subagent_prompt_plan_flexible_uses_model_base_when_empty() {
        let (base_instructions, user_prompt) =
            build_subagent_prompt_plan(InstructionMode::Flexible, "", "IDENTITY", "TASK", "BASE");

        assert_eq!(base_instructions, Some("BASE\n\nIDENTITY".to_string()));
        assert_eq!(user_prompt, "TASK");
    }

    #[test]
    fn identity_prompt_omits_parallel_constraints_for_single_worker() {
        let prompt = subagent_identity_prompt("parent-1", 1);
        assert!(!prompt.contains("Parallel Execution Constraints"));
    }

    #[test]
    fn identity_prompt_includes_parallel_constraints_for_multiple_workers() {
        let prompt = subagent_identity_prompt("parent-1", 2);
        assert!(prompt.contains("Parallel Execution Constraints"));
        assert!(prompt.contains("spawned 2 worker sessions"));
    }

    #[test]
    fn test_task_args_deserialize_with_skills() {
        let json = r#"{"description":"desc","prompt":"prompt","task_type":"general","skills":["skill-a","skill-b"]}"#;
        let args: TaskArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.description, "desc");
        assert_eq!(args.prompt, "prompt");
        assert_eq!(args.task_type, "general");
        assert_eq!(
            args.skills,
            Some(vec!["skill-a".to_string(), "skill-b".to_string()])
        );
    }

    #[test]
    fn test_task_args_deserialize_without_skills() {
        let json = r#"{"description":"desc","prompt":"prompt","task_type":"code"}"#;
        let args: TaskArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.description, "desc");
        assert_eq!(args.prompt, "prompt");
        assert_eq!(args.task_type, "code");
        assert!(args.skills.is_none());
    }

    #[test]
    fn test_task_args_deserialize_with_empty_skills() {
        let json = r#"{"description":"desc","prompt":"prompt","task_type":"general","skills":[]}"#;
        let args: TaskArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.skills, Some(vec![]));
    }

    #[test]
    fn validate_requested_skills_rejects_non_general_even_empty() {
        let err = validate_requested_skills("rust", Some(&[]), &[]).unwrap_err();
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("expected RespondToModel");
        };
        assert!(message.contains("only valid for task_type=\"general\""));
    }

    #[test]
    fn validate_requested_skills_rejects_unknown_skill() {
        let requested = vec!["skill-x".to_string()];
        let available = vec!["skill-a".to_string(), "skill-b".to_string()];
        let err = validate_requested_skills("general", Some(&requested), &available).unwrap_err();
        let FunctionCallError::RespondToModel(message) = err else {
            panic!("expected RespondToModel");
        };
        assert!(message.contains("Unknown skill 'skill-x'"));
        assert!(message.contains("skill-a"));
        assert!(message.contains("skill-b"));
    }
}
