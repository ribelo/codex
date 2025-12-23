use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::codex::TurnContext;
use crate::state::TaskKind;
use crate::tools::ToolRouter;
use crate::tools::context::ToolPayload;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::user_input::UserInput;

use super::SessionTask;
use super::SessionTaskContext;

/// Task for delegating to a subagent via the `task` tool.
pub(crate) struct SubagentDelegateTask {
    pub(crate) call_id: String,
    pub(crate) tool_name: String,
    pub(crate) args_str: String,
}

#[async_trait]
impl SessionTask for SubagentDelegateTask {
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
        let sess = session.clone_session();

        // Emit TaskStarted so TUI shows "Working" and Ctrl+C interrupts
        let event = EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: Some(ctx.client.get_model_context_window()),
        });
        sess.send_event(&ctx, event).await;

        // Setup ToolRouter
        let mcp_tools = {
            let tools = sess
                .services
                .mcp_connection_manager
                .read()
                .await
                .list_all_tools()
                .await;
            tools
                .into_iter()
                .map(|(n, t)| (n, t.tool))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let router = ToolRouter::from_config(&ctx.tools_config, Some(mcp_tools));

        // Execute Tool
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let result = router
            .dispatch_tool_call(
                sess.clone(),
                ctx.clone(),
                tracker.clone(),
                crate::tools::router::ToolCall {
                    tool_name: self.tool_name.clone(),
                    call_id: self.call_id.clone(),
                    payload: ToolPayload::Function {
                        arguments: self.args_str.clone(),
                    },
                },
            )
            .await;

        // Record Output
        let output_item = match result {
            Ok(input_item) => ResponseItem::from(input_item),
            Err(e) => ResponseItem::CustomToolCallOutput {
                call_id: self.call_id.clone(),
                output: format!("Error: {e}"),
            },
        };

        sess.record_conversation_items(&ctx, &[output_item]).await;

        // Resume Turn (let model see history and respond)
        crate::codex::run_task(sess, ctx, vec![], cancellation_token).await
    }

    async fn abort(&self, session: Arc<SessionTaskContext>, ctx: Arc<TurnContext>) {
        // Record an interrupted output so the conversation history stays consistent
        let sess = session.clone_session();
        let output_item = ResponseItem::CustomToolCallOutput {
            call_id: self.call_id.clone(),
            output: "Task interrupted by user".to_string(),
        };
        sess.record_conversation_items(&ctx, &[output_item]).await;
    }
}
