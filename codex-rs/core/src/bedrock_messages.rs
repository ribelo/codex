#![allow(clippy::too_many_arguments)]

use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::Region;
use aws_sdk_bedrockruntime::types::ContentBlock;
use aws_sdk_bedrockruntime::types::ConversationRole;
use aws_sdk_bedrockruntime::types::InferenceConfiguration;
use aws_sdk_bedrockruntime::types::Message;
use aws_sdk_bedrockruntime::types::SystemContentBlock;
use aws_sdk_bedrockruntime::types::Tool;
use aws_sdk_bedrockruntime::types::ToolConfiguration;
use aws_sdk_bedrockruntime::types::ToolInputSchema;
use aws_sdk_bedrockruntime::types::ToolResultBlock;
use aws_sdk_bedrockruntime::types::ToolResultContentBlock;
use aws_sdk_bedrockruntime::types::ToolResultStatus;
use aws_sdk_bedrockruntime::types::ToolSpecification;
use aws_sdk_bedrockruntime::types::ToolUseBlock;
use aws_smithy_types::Document;
use aws_smithy_types::Number;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::tools::ToolSpec;
use crate::config::Config;
use crate::error::CodexErr;
use crate::error::Result;
use crate::model_provider_info::BedrockProvider;
use crate::openai_models::model_family::ModelFamily;
use crate::truncate::approx_token_count;
use codex_otel::otel_event_manager::OtelEventManager;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::TokenUsage;

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64_000;

/// Create an AWS Bedrock Runtime client with the given provider configuration.
async fn create_bedrock_client(provider: &BedrockProvider) -> Result<Client> {
    let mut config_loader = aws_config::defaults(BehaviorVersion::latest());

    config_loader = config_loader.region(Region::new(provider.region.clone()));

    if let Some(profile) = &provider.profile {
        config_loader = config_loader.profile_name(profile);
    }

    let sdk_config = config_loader.load().await;
    Ok(Client::new(&sdk_config))
}

/// Stream a conversation turn via the AWS Bedrock Converse API.
pub(crate) async fn stream_bedrock_messages(
    prompt: &Prompt,
    config: &Config,
    model_family: &ModelFamily,
    model_id: &str,
    provider: &BedrockProvider,
    _otel_event_manager: &OtelEventManager,
) -> Result<ResponseStream> {
    let client = create_bedrock_client(provider).await?;

    let full_instructions = prompt.get_full_instructions(model_family).to_string();
    let (system_prompts, messages) =
        build_bedrock_messages(prompt, &full_instructions, model_family)?;
    let tool_config = build_tool_config(&prompt.tools)?;

    let thinking = model_family
        .supports_reasoning_summaries
        .then_some(BedrockThinking {
            r#type: "enabled",
            budget_tokens: None,
        });

    let prompt_tokens =
        bedrock_prompt_token_estimate(&system_prompts, &messages, tool_config.as_ref(), &thinking);

    let max_tokens = resolve_max_tokens(
        config,
        model_family,
        prompt_tokens,
        DEFAULT_MAX_OUTPUT_TOKENS,
    );

    let thinking = thinking.map(|mut cfg| {
        cfg.budget_tokens = Some(resolve_thinking_budget(
            config
                .model_reasoning_effort
                .or(model_family.default_reasoning_effort),
            max_tokens,
        ));
        cfg
    });

    let inference_config = InferenceConfiguration::builder()
        .max_tokens(max_tokens as i32)
        .build();

    let mut request = client
        .converse_stream()
        .model_id(model_id)
        .set_system(if system_prompts.is_empty() {
            None
        } else {
            Some(system_prompts)
        })
        .set_messages(Some(messages))
        .inference_config(inference_config);

    if let Some(tc) = tool_config {
        request = request.tool_config(tc);
    }

    if let Some(thinking) = thinking {
        let budget = thinking.budget_tokens.unwrap_or(4096);
        let mut fields = std::collections::HashMap::new();
        fields.insert("type".to_string(), Document::String("enabled".to_string()));
        fields.insert(
            "budget_tokens".to_string(),
            Document::Number(Number::PosInt(budget as u64)),
        );

        let mut thinking_fields = std::collections::HashMap::new();
        thinking_fields.insert("thinking".to_string(), Document::Object(fields));

        request =
            request.set_additional_model_request_fields(Some(Document::Object(thinking_fields)));
    }

    debug!("Sending Bedrock converse_stream request for model: {model_id}");

    let output = request.send().await.map_err(|e| {
        let detailed_error = match &e {
            aws_sdk_bedrockruntime::error::SdkError::ServiceError(err) => {
                format!("ServiceError: {:?} - meta: {:?}", err.err(), err.raw())
            }
            _ => format!("{e:?}"),
        };
        CodexErr::Fatal(format!("Bedrock converse_stream failed: {detailed_error}"))
    })?;

    let (tx_event, rx_event) = mpsc::channel(32);

    tokio::spawn(async move {
        if let Err(e) = process_bedrock_stream(output.stream, tx_event.clone()).await {
            let _ = tx_event.send(Err(e)).await;
        }
    });

    Ok(ResponseStream { rx_event })
}

async fn process_bedrock_stream(
    mut stream: aws_sdk_bedrockruntime::primitives::event_stream::EventReceiver<
        aws_sdk_bedrockruntime::types::ConverseStreamOutput,
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
    >,
    tx: mpsc::Sender<Result<ResponseEvent>>,
) -> Result<()> {
    use aws_sdk_bedrockruntime::types::ConverseStreamOutput;

    // Send Created event first
    tx.send(Ok(ResponseEvent::Created)).await.ok();

    // State for accumulating tool use
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_json = String::new();
    let mut current_text = String::new();
    let mut current_thinking = String::new();
    let response_id = uuid::Uuid::new_v4().to_string();
    let mut message_added = false;
    let mut thinking_added = false;
    let mut final_token_usage: Option<TokenUsage> = None;

    loop {
        let event = match stream.recv().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                return Err(CodexErr::Stream(format!("Bedrock stream error: {e}"), None));
            }
        };

        match event {
            ConverseStreamOutput::MessageStart(_) => {
                trace!("Bedrock: MessageStart");
            }

            ConverseStreamOutput::ContentBlockStart(e) => {
                if let Some(start) = e.start() {
                    if let aws_sdk_bedrockruntime::types::ContentBlockStart::ToolUse(t) = start {
                        // Flush any pending text if it was part of a message
                        if !current_text.is_empty() && message_added {
                            tx.send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                                id: None,
                                role: "assistant".to_string(),
                                content: vec![ContentItem::OutputText {
                                    text: std::mem::take(&mut current_text),
                                    signature: None,
                                }],
                            })))
                            .await
                            .ok();
                            message_added = false;
                        }

                        current_tool_id = t.tool_use_id().to_string();
                        current_tool_name = t.name().to_string();
                        current_tool_json.clear();
                        trace!("Bedrock: ToolUse start - {}", current_tool_name);
                    }
                }
            }

            ConverseStreamOutput::ContentBlockDelta(e) => {
                if let Some(delta) = e.delta() {
                    match delta {
                        aws_sdk_bedrockruntime::types::ContentBlockDelta::Text(txt) => {
                            let text = txt.as_str();

                            if !message_added {
                                tx.send(Ok(ResponseEvent::OutputItemAdded(
                                    ResponseItem::Message {
                                        id: None,
                                        role: "assistant".to_string(),
                                        content: Vec::new(),
                                    },
                                )))
                                .await
                                .ok();
                                message_added = true;
                            }

                            tx.send(Ok(ResponseEvent::OutputTextDelta(text.to_string())))
                                .await
                                .ok();
                            current_text.push_str(text);
                        }
                        aws_sdk_bedrockruntime::types::ContentBlockDelta::ToolUse(t) => {
                            current_tool_json.push_str(t.input());
                        }
                        aws_sdk_bedrockruntime::types::ContentBlockDelta::ReasoningContent(r) => {
                            use aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta;
                            if let ReasoningContentBlockDelta::Text(text) = r {
                                if !thinking_added {
                                    tx.send(Ok(ResponseEvent::OutputItemAdded(
                                        ResponseItem::Reasoning {
                                            id: String::new(),
                                            summary: Vec::new(),
                                            content: Some(Vec::new()),
                                            encrypted_content: None,
                                        },
                                    )))
                                    .await
                                    .ok();
                                    thinking_added = true;
                                }

                                tx.send(Ok(ResponseEvent::ReasoningSummaryDelta {
                                    delta: text.clone(),
                                    summary_index: 0,
                                }))
                                .await
                                .ok();
                                current_thinking.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
            }

            ConverseStreamOutput::ContentBlockStop(_) => {
                // If we were building a thinking block, emit it
                if thinking_added && !current_thinking.is_empty() {
                    tx.send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                        id: String::new(),
                        summary: vec![ReasoningItemReasoningSummary::SummaryText {
                            text: std::mem::take(&mut current_thinking),
                        }],
                        content: Some(Vec::new()),
                        encrypted_content: None,
                    })))
                    .await
                    .ok();
                    thinking_added = false;
                }

                // If we were building a tool call, emit it
                if !current_tool_id.is_empty() {
                    trace!(
                        "Bedrock: ToolUse complete - {} args: {}",
                        current_tool_name, current_tool_json
                    );
                    tx.send(Ok(ResponseEvent::OutputItemDone(
                        ResponseItem::FunctionCall {
                            id: None,
                            name: std::mem::take(&mut current_tool_name),
                            arguments: std::mem::take(&mut current_tool_json),
                            call_id: std::mem::take(&mut current_tool_id),
                        },
                    )))
                    .await
                    .ok();
                }
            }

            ConverseStreamOutput::MessageStop(e) => {
                trace!("Bedrock: MessageStop, reason: {:?}", e.stop_reason());

                // Flush thinking if still present
                if thinking_added && !current_thinking.is_empty() {
                    tx.send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                        id: String::new(),
                        summary: vec![ReasoningItemReasoningSummary::SummaryText {
                            text: std::mem::take(&mut current_thinking),
                        }],
                        content: Some(Vec::new()),
                        encrypted_content: None,
                    })))
                    .await
                    .ok();
                    thinking_added = false;
                }

                // Flush any remaining text
                if !current_text.is_empty() {
                    tx.send(Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText {
                            text: std::mem::take(&mut current_text),
                            signature: None,
                        }],
                    })))
                    .await
                    .ok();
                }
            }

            ConverseStreamOutput::Metadata(e) => {
                final_token_usage = e.usage().map(|u| TokenUsage {
                    input_tokens: u.input_tokens() as i64,
                    output_tokens: u.output_tokens() as i64,
                    total_tokens: u.total_tokens() as i64,
                    ..Default::default()
                });
            }

            _ => {}
        }
    }

    // Always send Completed event at end of stream
    tx.send(Ok(ResponseEvent::Completed {
        response_id: response_id.clone(),
        token_usage: final_token_usage,
    }))
    .await
    .ok();

    Ok(())
}

/// Build Bedrock messages from prompt items.
fn build_bedrock_messages(
    prompt: &Prompt,
    system_prompt: &str,
    _model_family: &ModelFamily,
) -> Result<(Vec<SystemContentBlock>, Vec<Message>)> {
    let mut system_blocks = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    // Add system prompt
    if !system_prompt.is_empty() {
        system_blocks.push(SystemContentBlock::Text(system_prompt.to_string()));
    }

    let input = prompt.get_formatted_input();

    for item in input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if role == "system" {
                    // Additional system content
                    let text = flatten_content(&content);
                    if !text.is_empty() {
                        system_blocks.push(SystemContentBlock::Text(text));
                    }
                    continue;
                }

                if role != "user" && role != "assistant" {
                    continue;
                }

                let bedrock_role = if role == "user" {
                    ConversationRole::User
                } else {
                    ConversationRole::Assistant
                };

                let blocks = content_to_bedrock_blocks(&content);

                if blocks.is_empty() {
                    continue;
                }

                // Check if we should merge with previous message of same role
                if let Some(last) = messages.last_mut()
                    && last.role() == &bedrock_role
                {
                    // Merge content blocks
                    let mut existing = last.content().to_vec();
                    // Bedrock requires ToolUse blocks at the end of the message.
                    // Insert new text blocks before any ToolUse blocks.
                    let insert_idx = existing
                        .iter()
                        .position(|b| matches!(b, ContentBlock::ToolUse(_)))
                        .unwrap_or(existing.len());
                    existing.splice(insert_idx..insert_idx, blocks);
                    *last = Message::builder()
                        .role(bedrock_role)
                        .set_content(Some(existing))
                        .build()
                        .map_err(|e| CodexErr::Fatal(format!("Failed to build message: {e}")))?;
                    continue;
                }

                let msg = Message::builder()
                    .role(bedrock_role)
                    .set_content(Some(blocks))
                    .build()
                    .map_err(|e| CodexErr::Fatal(format!("Failed to build message: {e}")))?;
                messages.push(msg);
            }

            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let input_doc = parse_json_to_document(&arguments);
                let tool_use = ToolUseBlock::builder()
                    .tool_use_id(&call_id)
                    .name(&name)
                    .input(input_doc)
                    .build()
                    .map_err(|e| CodexErr::Fatal(format!("Failed to build tool use: {e}")))?;

                // Tool use goes in assistant message
                let block = ContentBlock::ToolUse(tool_use);
                append_to_assistant_message(&mut messages, block)?;
            }

            ResponseItem::FunctionCallOutput { call_id, output } => {
                let status = if output.success.unwrap_or(true) {
                    ToolResultStatus::Success
                } else {
                    ToolResultStatus::Error
                };
                let tool_result = ToolResultBlock::builder()
                    .tool_use_id(&call_id)
                    .content(ToolResultContentBlock::Text(output.content.clone()))
                    .status(status)
                    .build()
                    .map_err(|e| CodexErr::Fatal(format!("Failed to build tool result: {e}")))?;

                // Tool results go in user message
                let block = ContentBlock::ToolResult(tool_result);
                append_to_user_message(&mut messages, block)?;
            }

            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let input_doc = parse_json_to_document(&input);
                let tool_use = ToolUseBlock::builder()
                    .tool_use_id(&call_id)
                    .name(&name)
                    .input(input_doc)
                    .build()
                    .map_err(|e| CodexErr::Fatal(format!("Failed to build tool use: {e}")))?;

                let block = ContentBlock::ToolUse(tool_use);
                append_to_assistant_message(&mut messages, block)?;
            }

            ResponseItem::CustomToolCallOutput { call_id, output } => {
                let tool_result = ToolResultBlock::builder()
                    .tool_use_id(&call_id)
                    .content(ToolResultContentBlock::Text(output))
                    .status(ToolResultStatus::Success)
                    .build()
                    .map_err(|e| CodexErr::Fatal(format!("Failed to build tool result: {e}")))?;

                let block = ContentBlock::ToolResult(tool_result);
                append_to_user_message(&mut messages, block)?;
            }

            // Skip other item types
            _ => {}
        }
    }

    Ok((system_blocks, messages))
}

#[derive(serde::Serialize)]
struct BedrockThinking {
    #[serde(rename = "type")]
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<i64>,
}

fn bedrock_prompt_token_estimate(
    system: &[SystemContentBlock],
    messages: &[Message],
    tool_config: Option<&ToolConfiguration>,
    thinking: &Option<BedrockThinking>,
) -> i64 {
    let prompt_json = json!({
        "system": system.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>(),
        "messages": messages.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>(),
        "tool_config": tool_config.map(|t| format!("{t:?}")),
        "thinking": thinking,
    });
    i64::try_from(approx_token_count(&prompt_json.to_string())).unwrap_or(i64::MAX)
}

fn resolve_max_tokens(
    config: &Config,
    model_family: &ModelFamily,
    prompt_tokens: i64,
    hard_cap: i64,
) -> i64 {
    let window = effective_context_window(config, model_family);
    let remaining = window.saturating_sub(prompt_tokens).max(1);
    remaining.min(hard_cap)
}

fn effective_context_window(config: &Config, model_family: &ModelFamily) -> i64 {
    config
        .model_context_window
        .saturating_mul(model_family.effective_context_window_percent)
        / 100
}

fn resolve_thinking_budget(effort: Option<ReasoningEffort>, max_tokens: i64) -> i64 {
    let target_budget = match effort {
        Some(ReasoningEffort::Low)
        | Some(ReasoningEffort::Minimal)
        | Some(ReasoningEffort::None) => 4_096,
        Some(ReasoningEffort::Medium) => 16_384,
        Some(ReasoningEffort::High) | Some(ReasoningEffort::XHigh) => 32_768,
        None => max_tokens * 4 / 5,
    };

    let reserved = (max_tokens / 5).clamp(1024, 4096);
    let hard_cap = max_tokens.saturating_sub(reserved).max(1);

    if target_budget < max_tokens {
        target_budget.min(hard_cap)
    } else {
        hard_cap
    }
}

fn append_to_assistant_message(messages: &mut Vec<Message>, block: ContentBlock) -> Result<()> {
    if let Some(last) = messages.last_mut()
        && last.role() == &ConversationRole::Assistant
    {
        let mut existing = last.content().to_vec();

        // Bedrock requires ToolUse blocks at the end of a message.
        // If the new block is text and there are existing ToolUse blocks,
        // insert text before them; otherwise append normally.
        let is_tool_use = matches!(block, ContentBlock::ToolUse(_));
        if is_tool_use {
            // ToolUse always goes at the end
            existing.push(block);
        } else {
            // Non-tool blocks go before any existing ToolUse blocks
            let tool_use_start = existing
                .iter()
                .position(|b| matches!(b, ContentBlock::ToolUse(_)));
            match tool_use_start {
                Some(idx) => existing.insert(idx, block),
                None => existing.push(block),
            }
        }

        *last = Message::builder()
            .role(ConversationRole::Assistant)
            .set_content(Some(existing))
            .build()
            .map_err(|e| CodexErr::Fatal(format!("Failed to build message: {e}")))?;
        return Ok(());
    }

    // Create new assistant message
    let msg = Message::builder()
        .role(ConversationRole::Assistant)
        .set_content(Some(vec![block]))
        .build()
        .map_err(|e| CodexErr::Fatal(format!("Failed to build message: {e}")))?;
    messages.push(msg);
    Ok(())
}

fn append_to_user_message(messages: &mut Vec<Message>, block: ContentBlock) -> Result<()> {
    if let Some(last) = messages.last_mut()
        && last.role() == &ConversationRole::User
    {
        let mut existing = last.content().to_vec();
        existing.push(block);
        *last = Message::builder()
            .role(ConversationRole::User)
            .set_content(Some(existing))
            .build()
            .map_err(|e| CodexErr::Fatal(format!("Failed to build message: {e}")))?;
        return Ok(());
    }

    let msg = Message::builder()
        .role(ConversationRole::User)
        .set_content(Some(vec![block]))
        .build()
        .map_err(|e| CodexErr::Fatal(format!("Failed to build message: {e}")))?;
    messages.push(msg);
    Ok(())
}

fn content_to_bedrock_blocks(content: &[ContentItem]) -> Vec<ContentBlock> {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                Some(ContentBlock::Text(text.clone()))
            }
            ContentItem::InputImage { image_url } => {
                // TODO: Implement proper image handling for Bedrock
                // Bedrock requires raw bytes, not URLs. For now, include as text reference.
                warn!("Image input not fully supported for Bedrock: {image_url}");
                Some(ContentBlock::Text(format!("[Image: {image_url}]")))
            }
        })
        .collect()
}

fn flatten_content(content: &[ContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_json_to_document(json_str: &str) -> Document {
    match serde_json::from_str::<Value>(json_str) {
        Ok(value) => value_to_document(&value),
        Err(_) => Document::String(json_str.to_string()),
    }
}

fn value_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else if let Some(f) = n.as_f64() {
                Document::Number(Number::Float(f))
            } else {
                Document::Null
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(arr) => Document::Array(arr.iter().map(value_to_document).collect()),
        Value::Object(obj) => Document::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), value_to_document(v)))
                .collect(),
        ),
    }
}

/// Build Bedrock tool configuration from tool specs.
fn build_tool_config(tools: &[ToolSpec]) -> Result<Option<ToolConfiguration>> {
    if tools.is_empty() {
        return Ok(None);
    }

    let bedrock_tools: Vec<Tool> = tools
        .iter()
        .map(|tool| {
            if let ToolSpec::Function(spec) = tool {
                let schema_doc = value_to_document(&serde_json::to_value(&spec.parameters)?);

                let spec = ToolSpecification::builder()
                    .name(&spec.name)
                    .description(spec.description.clone())
                    .input_schema(ToolInputSchema::Json(schema_doc))
                    .build()
                    .map_err(|e| CodexErr::Fatal(format!("Failed to build tool spec: {e}")))?;

                Ok(Tool::ToolSpec(spec))
            } else {
                Err(CodexErr::Fatal(
                    "Unsupported tool type for Bedrock".to_string(),
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let config = ToolConfiguration::builder()
        .set_tools(Some(bedrock_tools))
        .build()
        .map_err(|e| CodexErr::Fatal(format!("Failed to build tool config: {e}")))?;

    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_types::Document;
    use codex_protocol::models::ContentItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn test_value_to_document_simple_types() {
        // String
        assert_eq!(
            value_to_document(&json!("hello")),
            Document::String("hello".to_string())
        );

        // Number (float)
        let num_doc = value_to_document(&json!(42.5));
        assert!(matches!(num_doc, Document::Number(_)));

        // Boolean
        assert_eq!(value_to_document(&json!(true)), Document::Bool(true));
        assert_eq!(value_to_document(&json!(false)), Document::Bool(false));

        // Null
        assert_eq!(value_to_document(&json!(null)), Document::Null);
    }

    #[test]
    fn test_value_to_document_nested() {
        let input = json!({
            "name": "test",
            "count": 5,
            "items": [1, 2, 3],
            "nested": {"a": true}
        });

        let doc = value_to_document(&input);

        // Check it's an object
        if let Document::Object(map) = doc {
            assert!(map.contains_key("name"));
            assert!(map.contains_key("items"));
            assert!(map.contains_key("nested"));

            // Check nested array
            if let Some(Document::Array(arr)) = map.get("items") {
                assert_eq!(arr.len(), 3);
            } else {
                panic!("Expected array for 'items'");
            }
        } else {
            panic!("Expected Document::Object");
        }
    }

    #[test]
    fn test_parse_json_to_document_valid() {
        let json_str = r#"{"key": "value", "num": 42}"#;
        let doc = parse_json_to_document(json_str);

        if let Document::Object(map) = doc {
            assert!(map.contains_key("key"));
            assert!(map.contains_key("num"));
        } else {
            panic!("Expected Document::Object");
        }
    }

    #[test]
    fn test_parse_json_to_document_invalid() {
        let invalid_json = "not valid json";
        let doc = parse_json_to_document(invalid_json);

        // Should fall back to string
        assert_eq!(doc, Document::String("not valid json".to_string()));
    }

    #[test]
    fn test_flatten_content() {
        let content = vec![
            ContentItem::InputText {
                text: "Hello".to_string(),
            },
            ContentItem::OutputText {
                text: "World".to_string(),
                signature: None,
            },
        ];

        let result = flatten_content(&content);
        assert_eq!(result, "Hello\nWorld");
    }

    #[test]
    fn test_flatten_content_empty() {
        let content: Vec<ContentItem> = vec![];
        let result = flatten_content(&content);
        assert_eq!(result, "");
    }

    #[test]
    fn test_content_to_bedrock_blocks() {
        let content = vec![
            ContentItem::InputText {
                text: "Hello".to_string(),
            },
            ContentItem::OutputText {
                text: "World".to_string(),
                signature: None,
            },
        ];

        let blocks = content_to_bedrock_blocks(&content);
        assert_eq!(blocks.len(), 2);

        match &blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, "Hello"),
            _ => panic!("Expected Text block"),
        }
    }
}
