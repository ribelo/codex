use async_trait::async_trait;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::review_execution::ReviewApprovalMode;
use crate::review_execution::ReviewEventForwarding;
use crate::review_execution::run_review_delegate;
use crate::review_prompts::resolve_review_request;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct ReviewHandler;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ReviewToolArgs {
    UncommittedChanges,
    BaseBranch { branch: String },
    Commit { sha: String, title: Option<String> },
    Custom { instructions: String },
}

impl ReviewToolArgs {
    fn into_review_request(self) -> ReviewRequest {
        let target = match self {
            ReviewToolArgs::UncommittedChanges => ReviewTarget::UncommittedChanges,
            ReviewToolArgs::BaseBranch { branch } => ReviewTarget::BaseBranch { branch },
            ReviewToolArgs::Commit { sha, title } => ReviewTarget::Commit { sha, title },
            ReviewToolArgs::Custom { instructions } => ReviewTarget::Custom { instructions },
        };

        ReviewRequest {
            target,
            user_facing_hint: None,
        }
    }
}

#[async_trait]
impl ToolHandler for ReviewHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        true
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "review handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: ReviewToolArgs = parse_arguments(&arguments)?;
        let review_prompt = resolve_review_request(args.into_review_request(), turn.cwd.as_path())
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?
            .prompt;
        let cancellation_token =
            current_turn_cancellation_token(session.as_ref(), turn.as_ref()).await?;

        let review_output = run_review_delegate(
            session,
            turn,
            vec![UserInput::Text {
                text: review_prompt,
                text_elements: Vec::new(),
            }],
            cancellation_token,
            ReviewEventForwarding::Suppress,
            ReviewApprovalMode::InheritParent,
        )
        .await
        .map_err(|err| FunctionCallError::RespondToModel(format!("review failed: {err}")))?
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "review was cancelled before producing output".to_string(),
            )
        })?;

        let content = serde_json::to_string(&review_output).map_err(|err| {
            FunctionCallError::Fatal(format!("failed to serialize review output: {err}"))
        })?;

        Ok(FunctionToolOutput::from_text(content, Some(true)))
    }
}

async fn current_turn_cancellation_token(
    session: &Session,
    turn: &TurnContext,
) -> Result<CancellationToken, FunctionCallError> {
    let active_turn = session.active_turn.lock().await;
    let Some(active_turn) = active_turn.as_ref() else {
        return Err(FunctionCallError::Fatal(
            "review tool requires an active turn".to_string(),
        ));
    };
    let Some(task) = active_turn.tasks.get(turn.sub_id.as_str()) else {
        return Err(FunctionCallError::Fatal(format!(
            "review tool could not find active task for turn {}",
            turn.sub_id
        )));
    };

    Ok(task.cancellation_token.child_token())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::make_session_and_context;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[test]
    fn review_tool_args_deserialize_commit_variant() {
        let args: ReviewToolArgs =
            serde_json::from_str(r#"{"type":"commit","sha":"abc123","title":"Tighten tests"}"#)
                .expect("deserialize args");

        assert_eq!(
            args,
            ReviewToolArgs::Commit {
                sha: "abc123".to_string(),
                title: Some("Tighten tests".to_string()),
            }
        );
    }

    #[test]
    fn review_tool_args_build_review_request_without_hint() {
        let request = ReviewToolArgs::Custom {
            instructions: "Review this".to_string(),
        }
        .into_review_request();

        assert_eq!(
            request,
            ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Review this".to_string(),
                },
                user_facing_hint: None,
            }
        );
    }

    #[tokio::test]
    async fn review_handler_is_mutating() {
        let (session, turn) = make_session_and_context().await;
        let handler = ReviewHandler;
        let invocation = ToolInvocation {
            session: Arc::new(session),
            turn: Arc::new(turn),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "call-1".to_string(),
            tool_name: "review".to_string(),
            payload: ToolPayload::Function {
                arguments: r#"{"type":"uncommitted_changes"}"#.to_string(),
            },
        };

        assert!(handler.is_mutating(&invocation).await);
    }
}
