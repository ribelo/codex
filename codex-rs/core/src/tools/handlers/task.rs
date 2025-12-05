use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::client_common::tools::ResponsesApiTool;
use crate::client_common::tools::ToolSpec;
use crate::codex_delegate::run_codex_conversation_interactive;
use crate::function_tool::FunctionCallError;
use crate::subagents::SubagentRegistry;
use crate::subagents::SubagentSession;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolHandler;
use crate::tools::spec::JsonSchema;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;

pub fn create_task_tool(codex_home: &Path) -> ToolSpec {
    let registry = SubagentRegistry::new(codex_home);
    let agents_list = registry.list();

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

        let session_id = args
            .session_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let subagent_session_ref: Arc<crate::codex::Codex> = {
            let services = &invocation.session.services;
            let mut sessions = services.subagent_sessions.lock().await;

            if let Some(session) = sessions.get(&session_id) {
                session.codex.clone()
            } else {
                let config = turn.client.config();
                let mut sub_config = (*config).clone();
                sub_config.base_instructions = Some(subagent_def.system_prompt.clone());
                sub_config.codex_home = codex_home.clone();

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
                codex_arc
            }
        };

        let input = vec![UserInput::Text {
            text: args.prompt.clone(),
        }];
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
                EventMsg::TaskComplete(tc) => {
                    if let Some(msg) = tc.last_agent_message {
                        final_output = msg;
                    }
                    break;
                }
                EventMsg::TurnAborted(ta) => {
                    let reason_str = format!("Turn aborted: {:?}", ta.reason);
                    return Err(FunctionCallError::RespondToModel(reason_str));
                }
                EventMsg::ExecCommandBegin(_)
                | EventMsg::ExecCommandEnd(_)
                | EventMsg::McpToolCallBegin(_)
                | EventMsg::McpToolCallEnd(_) => {
                    invocation
                        .session
                        .send_event(invocation.turn.as_ref(), event.msg)
                        .await;
                }
                _ => {}
            }
        }

        Ok(ToolOutput::Function {
            success: Some(true),
            content: final_output,
            content_items: None,
        })
    }
}
