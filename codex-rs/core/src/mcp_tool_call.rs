use std::time::Instant;

use tracing::error;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::protocol::EventMsg;
use crate::protocol::McpInvocation;
use crate::protocol::McpToolCallBeginEvent;
use crate::protocol::McpToolCallEndEvent;
use crate::truncate::TruncationPolicy;
use crate::truncate::truncate_text;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use mcp_types::CallToolResult;
use mcp_types::ContentBlock;
use mcp_types::EmbeddedResource;
use mcp_types::EmbeddedResourceResource;
use mcp_types::TextContent;
use mcp_types::TextResourceContents;

/// Handles the specified tool call dispatches the appropriate
/// `McpToolCallBegin` and `McpToolCallEnd` events to the `Session`.
pub(crate) async fn handle_mcp_tool_call(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: String,
    server: String,
    tool_name: String,
    arguments: String,
) -> ResponseInputItem {
    // Parse the `arguments` as JSON. An empty string is OK, but invalid JSON
    // is not.
    let arguments_value = if arguments.trim().is_empty() {
        None
    } else {
        match serde_json::from_str::<serde_json::Value>(&arguments) {
            Ok(value) => Some(value),
            Err(e) => {
                error!("failed to parse tool call arguments: {e}");
                return ResponseInputItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: FunctionCallOutputPayload {
                        content: format!("err: {e}"),
                        success: Some(false),
                        ..Default::default()
                    },
                };
            }
        }
    };

    let invocation = McpInvocation {
        server: server.clone(),
        tool: tool_name.clone(),
        arguments: arguments_value.clone(),
    };

    let tool_call_begin_event = EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
        call_id: call_id.clone(),
        invocation: invocation.clone(),
    });
    notify_mcp_tool_call_event(sess, turn_context, tool_call_begin_event).await;

    let start = Instant::now();
    // Perform the tool call.
    let result = sess
        .call_tool(&server, &tool_name, arguments_value.clone())
        .await
        .map_err(|e| format!("tool call error: {e:?}"));
    if let Err(e) = &result {
        tracing::warn!("MCP tool call error: {e:?}");
    }
    let tool_call_end_event = EventMsg::McpToolCallEnd(McpToolCallEndEvent {
        call_id: call_id.clone(),
        invocation,
        duration: start.elapsed(),
        result: result.clone(),
    });

    notify_mcp_tool_call_event(sess, turn_context, tool_call_end_event.clone()).await;

    // Truncate the result content using the MCP-specific truncation policy.
    let truncated_result = result.map(|r| truncate_call_tool_result(r, turn_context));

    ResponseInputItem::McpToolCallOutput {
        call_id,
        result: truncated_result,
    }
}

async fn notify_mcp_tool_call_event(sess: &Session, turn_context: &TurnContext, event: EventMsg) {
    sess.send_event(turn_context, event).await;
}

/// Truncate the text content in a CallToolResult using the MCP truncation policy.
fn truncate_call_tool_result(result: CallToolResult, turn_context: &TurnContext) -> CallToolResult {
    let policy = turn_context.mcp_truncation_policy;
    let truncated_content = result
        .content
        .into_iter()
        .map(|item| match item {
            ContentBlock::TextContent(tc) => ContentBlock::TextContent(TextContent {
                text: truncate_text(&tc.text, policy),
                annotations: tc.annotations,
                r#type: tc.r#type,
            }),
            ContentBlock::EmbeddedResource(er) => {
                ContentBlock::EmbeddedResource(truncate_embedded_resource(er, policy))
            }
            other => other,
        })
        .collect();

    // Note: structured_content is NOT truncated here because truncating a serialized
    // JSON string would produce invalid JSON. Instead, structured_content is serialized
    // to a string when converted to FunctionCallOutputPayload, and that string is
    // truncated by the downstream history processing in process_item().

    CallToolResult {
        content: truncated_content,
        is_error: result.is_error,
        structured_content: result.structured_content,
    }
}

/// Truncate text content within an EmbeddedResource.
fn truncate_embedded_resource(er: EmbeddedResource, policy: TruncationPolicy) -> EmbeddedResource {
    let truncated_resource = match er.resource {
        EmbeddedResourceResource::TextResourceContents(trc) => {
            EmbeddedResourceResource::TextResourceContents(TextResourceContents {
                text: truncate_text(&trc.text, policy),
                mime_type: trc.mime_type,
                uri: trc.uri,
            })
        }
        // BlobResourceContents is base64-encoded binary; truncating would corrupt it.
        blob => blob,
    };

    EmbeddedResource {
        annotations: er.annotations,
        resource: truncated_resource,
        r#type: er.r#type,
    }
}
