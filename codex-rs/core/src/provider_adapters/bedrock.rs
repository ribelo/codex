use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::Region;
use aws_sdk_bedrockruntime::error::DisplayErrorContext;
use aws_sdk_bedrockruntime::error::ProvideErrorMetadata;
use aws_sdk_bedrockruntime::error::SdkError;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
use aws_sdk_bedrockruntime::types::CachePointBlock;
use aws_sdk_bedrockruntime::types::CachePointType;
use aws_sdk_bedrockruntime::types::ContentBlock;
use aws_sdk_bedrockruntime::types::ContentBlockDelta;
use aws_sdk_bedrockruntime::types::ConversationRole;
use aws_sdk_bedrockruntime::types::ConverseStreamOutput;
use aws_sdk_bedrockruntime::types::InferenceConfiguration;
use aws_sdk_bedrockruntime::types::JsonSchemaDefinition;
use aws_sdk_bedrockruntime::types::Message;
use aws_sdk_bedrockruntime::types::OutputConfig;
use aws_sdk_bedrockruntime::types::OutputFormat;
use aws_sdk_bedrockruntime::types::OutputFormatStructure;
use aws_sdk_bedrockruntime::types::OutputFormatType;
use aws_sdk_bedrockruntime::types::ReasoningContentBlock;
use aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta;
use aws_sdk_bedrockruntime::types::ReasoningTextBlock;
use aws_sdk_bedrockruntime::types::ServiceTier as BedrockServiceTier;
use aws_sdk_bedrockruntime::types::ServiceTierType as BedrockServiceTierType;
use aws_sdk_bedrockruntime::types::SystemContentBlock;
use aws_sdk_bedrockruntime::types::Tool;
use aws_sdk_bedrockruntime::types::ToolConfiguration;
use aws_sdk_bedrockruntime::types::ToolInputSchema;
use aws_sdk_bedrockruntime::types::ToolResultBlock;
use aws_sdk_bedrockruntime::types::ToolResultContentBlock;
use aws_sdk_bedrockruntime::types::ToolResultStatus;
use aws_sdk_bedrockruntime::types::ToolSpecification;
use aws_sdk_bedrockruntime::types::ToolUseBlock;
use aws_smithy_types::Blob;
use aws_smithy_types::Document;
use aws_smithy_types::Number;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::TokenUsage;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::trace;
use tracing::warn;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::tools::ToolSpec;
use crate::error::CodexErr;
use crate::error::Result;
use crate::model_provider_info::ModelProviderInfo;
use crate::models_manager::model_info::is_bedrock_claude_slug;
use crate::provider_adapters::max_output_tokens;
use crate::provider_adapters::record_native_api_request;
use crate::util::backoff;

const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 64_000;

enum BlockState {
    Reasoning(ReasoningState),
    ToolUse(ToolUseState),
}

struct AssistantState {
    item: ResponseItem,
    added: bool,
}

struct ReasoningState {
    item: ResponseItem,
    added: bool,
    streamed_delta_count: i64,
    accumulated_text: String,
    signature: Option<String>,
}

struct ToolUseState {
    item: ResponseItem,
    buffer: String,
}

pub(crate) async fn stream_bedrock_converse(
    provider: &ModelProviderInfo,
    prompt: &Prompt,
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    service_tier: Option<ServiceTier>,
    session_telemetry: &SessionTelemetry,
) -> Result<ResponseStream> {
    let client = build_bedrock_client(provider).await?;
    let system = build_system(prompt, model_info)?;
    let messages = build_messages(prompt, model_info)?;
    let tool_config = build_tool_config(&prompt.tools)?;
    let additional_model_request_fields =
        build_additional_model_request_fields(model_info, effort, summary);
    let output_config = build_output_config(prompt.output_schema.as_ref())?;
    let service_tier = build_service_tier(service_tier);
    let inference_config = InferenceConfiguration::builder()
        .max_tokens(max_output_tokens(model_info, DEFAULT_MAX_OUTPUT_TOKENS) as i32)
        .build();

    let mut attempt = 0_u64;
    let max_retries = provider.request_max_retries();
    loop {
        attempt += 1;
        let started_at = Instant::now();
        let response = client
            .converse_stream()
            .model_id(model_info.slug.clone())
            .set_system((!system.is_empty()).then_some(system.clone()))
            .set_messages(Some(messages.clone()))
            .inference_config(inference_config.clone())
            .set_tool_config(tool_config.clone())
            .set_additional_model_request_fields(additional_model_request_fields.clone())
            .set_service_tier(service_tier.clone())
            .set_output_config(output_config.clone())
            .send()
            .await;
        let duration = started_at.elapsed();
        record_native_api_request(
            session_telemetry,
            attempt,
            response.as_ref().ok().map(|_| 200),
            response
                .as_ref()
                .err()
                .map(|err| format!("{}", DisplayErrorContext(err)))
                .as_deref(),
            duration,
            "/converse-stream",
        );

        match response {
            Ok(output) => {
                let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
                tokio::spawn(process_bedrock_stream(
                    output.stream,
                    tx_event,
                    provider.stream_idle_timeout(),
                ));
                return Ok(ResponseStream { rx_event });
            }
            Err(err) if is_retryable_request_error(&err) && attempt <= max_retries => {
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(err) => return Err(map_request_error(err)),
        }
    }
}

async fn build_bedrock_client(provider: &ModelProviderInfo) -> Result<Client> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(region) = provider.aws_region.clone() {
        loader = loader.region(Region::new(region));
    }
    if let Some(profile) = provider.aws_profile.clone() {
        loader = loader.profile_name(profile);
    }
    let shared_config = loader.load().await;
    let Some(region) = shared_config.region().cloned() else {
        return Err(CodexErr::InvalidRequest(format!(
            "Bedrock provider `{}` requires an AWS region; set aws_region or configure AWS_REGION/AWS_DEFAULT_REGION/shared config",
            provider.name
        )));
    };

    let mut config = aws_sdk_bedrockruntime::config::Builder::from(&shared_config).region(region);
    if let Some(endpoint_url) = provider.base_url.as_deref() {
        config = config.endpoint_url(endpoint_url);
    }

    Ok(Client::from_conf(config.build()))
}

fn build_system(prompt: &Prompt, model_info: &ModelInfo) -> Result<Vec<SystemContentBlock>> {
    let mut blocks = Vec::new();
    if !prompt.base_instructions.text.trim().is_empty() {
        blocks.push(SystemContentBlock::Text(
            prompt.base_instructions.text.clone(),
        ));
    }
    for item in prompt.get_formatted_input() {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "system"
        {
            let text = flatten_content(&content);
            if !text.is_empty() {
                blocks.push(SystemContentBlock::Text(text));
            }
        }
    }

    if is_bedrock_claude_slug(&model_info.slug) && !blocks.is_empty() {
        blocks.push(SystemContentBlock::CachePoint(default_cache_point()?));
    }

    Ok(blocks)
}

fn build_messages(prompt: &Prompt, model_info: &ModelInfo) -> Result<Vec<Message>> {
    let input = prompt.get_formatted_input();
    let mut messages = Vec::new();

    for item in &input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if role == "system" {
                    continue;
                }
                let Some(conversation_role) = conversation_role(role) else {
                    continue;
                };
                let blocks = map_message_content(content);
                if blocks.is_empty() {
                    continue;
                }
                append_message_blocks(&mut messages, conversation_role, blocks)?;
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => append_assistant_block(
                &mut messages,
                ContentBlock::ToolUse(build_tool_use_block(name, call_id, arguments)?),
            )?,
            ResponseItem::FunctionCallOutput { call_id, output }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => append_user_block(
                &mut messages,
                ContentBlock::ToolResult(build_tool_result_block(call_id, output)?),
            )?,
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => append_assistant_block(
                &mut messages,
                ContentBlock::ToolUse(build_tool_use_block(name, call_id, input)?),
            )?,
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                if let Some(block) =
                    build_reasoning_history_block(summary, content.as_deref(), encrypted_content)?
                {
                    append_assistant_block(&mut messages, block)?;
                }
            }
            ResponseItem::LocalShellCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::Other => {}
        }
    }

    if is_bedrock_claude_slug(&model_info.slug)
        && let Some(index) = messages
            .iter()
            .rposition(|message| message.role() == &ConversationRole::User)
    {
        let mut content = messages[index].content().to_vec();
        content.push(ContentBlock::CachePoint(default_cache_point()?));
        messages[index] = Message::builder()
            .role(ConversationRole::User)
            .set_content(Some(content))
            .build()
            .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock message: {err}")))?;
    }

    Ok(messages)
}

fn default_cache_point() -> Result<CachePointBlock> {
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .build()
        .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock cache point: {err}")))
}

fn build_tool_config(tools: &[ToolSpec]) -> Result<Option<ToolConfiguration>> {
    let mut out = Vec::new();
    for tool in tools {
        if let ToolSpec::Function(spec) = tool {
            let schema = serde_json::to_value(&spec.parameters)?;
            let tool_spec = ToolSpecification::builder()
                .name(spec.name.clone())
                .description(spec.description.clone())
                .input_schema(ToolInputSchema::Json(value_to_document(&schema)))
                .build()
                .map_err(|err| {
                    CodexErr::Fatal(format!("failed to build Bedrock tool spec: {err}"))
                })?;
            out.push(Tool::ToolSpec(tool_spec));
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(
            ToolConfiguration::builder()
                .set_tools(Some(out))
                .build()
                .map_err(|err| {
                    CodexErr::Fatal(format!("failed to build Bedrock tool config: {err}"))
                })?,
        ))
    }
}

fn build_additional_model_request_fields(
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
) -> Option<Document> {
    if !model_info.supports_reasoning_summaries {
        return None;
    }

    let budget_tokens = thinking_budget(effort.or(model_info.default_reasoning_level), summary);
    if budget_tokens == 0 {
        return None;
    }

    Some(Document::Object(HashMap::from([(
        "thinking".to_string(),
        Document::Object(HashMap::from([
            ("type".to_string(), Document::String("enabled".to_string())),
            (
                "budget_tokens".to_string(),
                Document::Number(Number::PosInt(budget_tokens as u64)),
            ),
        ])),
    )])))
}

fn build_output_config(output_schema: Option<&Value>) -> Result<Option<OutputConfig>> {
    let Some(schema) = output_schema else {
        return Ok(None);
    };

    let schema = JsonSchemaDefinition::builder()
        .schema(serde_json::to_string(schema)?)
        .set_name(
            schema
                .get("title")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        )
        .set_description(
            schema
                .get("description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        )
        .build()
        .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock output schema: {err}")))?;
    let text_format = OutputFormat::builder()
        .r#type(OutputFormatType::JsonSchema)
        .structure(OutputFormatStructure::JsonSchema(schema))
        .build()
        .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock output format: {err}")))?;
    Ok(Some(
        OutputConfig::builder().text_format(text_format).build(),
    ))
}

fn build_service_tier(service_tier: Option<ServiceTier>) -> Option<BedrockServiceTier> {
    let tier_type = match service_tier {
        Some(ServiceTier::Fast) => Some(BedrockServiceTierType::Priority),
        Some(ServiceTier::Flex) => Some(BedrockServiceTierType::Flex),
        None => None,
    }?;
    BedrockServiceTier::builder().r#type(tier_type).build().ok()
}

fn conversation_role(role: &str) -> Option<ConversationRole> {
    match role {
        "user" => Some(ConversationRole::User),
        "assistant" => Some(ConversationRole::Assistant),
        _ => None,
    }
}

fn map_message_content(content: &[ContentItem]) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text(text.clone()));
                }
            }
            ContentItem::InputImage { image_url } => {
                warn!(
                    "Bedrock image replay is downgraded to text for URL-based images: {image_url}"
                );
                blocks.push(ContentBlock::Text(format!("[image: {image_url}]")));
            }
        }
    }
    blocks
}

fn flatten_content(content: &[ContentItem]) -> String {
    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text.clone(),
            ContentItem::InputImage { image_url } => format!("[image: {image_url}]"),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn append_message_blocks(
    messages: &mut Vec<Message>,
    role: ConversationRole,
    blocks: Vec<ContentBlock>,
) -> Result<()> {
    match role {
        ConversationRole::Assistant => {
            for block in blocks {
                append_assistant_block(messages, block)?;
            }
        }
        ConversationRole::User => {
            for block in blocks {
                append_user_block(messages, block)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn append_assistant_block(messages: &mut Vec<Message>, block: ContentBlock) -> Result<()> {
    if let Some(last) = messages.last_mut()
        && last.role() == &ConversationRole::Assistant
    {
        let mut content = last.content().to_vec();
        if matches!(block, ContentBlock::ToolUse(_)) {
            content.push(block);
        } else if let Some(tool_use_index) = content
            .iter()
            .position(|existing| matches!(existing, ContentBlock::ToolUse(_)))
        {
            content.insert(tool_use_index, block);
        } else {
            content.push(block);
        }
        *last = Message::builder()
            .role(ConversationRole::Assistant)
            .set_content(Some(content))
            .build()
            .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock message: {err}")))?;
        return Ok(());
    }

    messages.push(
        Message::builder()
            .role(ConversationRole::Assistant)
            .set_content(Some(vec![block]))
            .build()
            .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock message: {err}")))?,
    );
    Ok(())
}

fn append_user_block(messages: &mut Vec<Message>, block: ContentBlock) -> Result<()> {
    if let Some(last) = messages.last_mut()
        && last.role() == &ConversationRole::User
    {
        let mut content = last.content().to_vec();
        content.push(block);
        *last = Message::builder()
            .role(ConversationRole::User)
            .set_content(Some(content))
            .build()
            .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock message: {err}")))?;
        return Ok(());
    }

    messages.push(
        Message::builder()
            .role(ConversationRole::User)
            .set_content(Some(vec![block]))
            .build()
            .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock message: {err}")))?,
    );
    Ok(())
}

fn build_tool_use_block(name: &str, call_id: &str, input: &str) -> Result<ToolUseBlock> {
    ToolUseBlock::builder()
        .tool_use_id(call_id)
        .name(name)
        .input(parse_json_to_document(input))
        .build()
        .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock tool use block: {err}")))
}

fn build_tool_result_block(
    call_id: &str,
    output: &FunctionCallOutputPayload,
) -> Result<ToolResultBlock> {
    let content = tool_result_content_blocks(output);
    let status = match output.success {
        Some(false) => Some(ToolResultStatus::Error),
        Some(true) | None => Some(ToolResultStatus::Success),
    };

    ToolResultBlock::builder()
        .tool_use_id(call_id)
        .set_content(Some(content))
        .set_status(status)
        .build()
        .map_err(|err| CodexErr::Fatal(format!("failed to build Bedrock tool result block: {err}")))
}

fn tool_result_content_blocks(output: &FunctionCallOutputPayload) -> Vec<ToolResultContentBlock> {
    if let Some(items) = output.content_items() {
        let mut blocks = Vec::new();
        for item in items {
            match item {
                FunctionCallOutputContentItem::InputText { text } => {
                    blocks.push(ToolResultContentBlock::Text(text.clone()));
                }
                FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                    warn!(
                        "Bedrock tool-result image replay is downgraded to text for URL-based images: {image_url}"
                    );
                    blocks.push(ToolResultContentBlock::Text(format!(
                        "[image: {image_url}]"
                    )));
                }
            }
        }
        if !blocks.is_empty() {
            return blocks;
        }
    }

    let Some(text) = output.body.to_text() else {
        return vec![ToolResultContentBlock::Text(String::new())];
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::String(text)) => vec![ToolResultContentBlock::Text(text)],
        Ok(value) => vec![ToolResultContentBlock::Json(value_to_document(&value))],
        Err(_) => vec![ToolResultContentBlock::Text(text)],
    }
}

fn build_reasoning_history_block(
    summary: &[ReasoningItemReasoningSummary],
    content: Option<&[ReasoningItemContent]>,
    encrypted_content: &Option<String>,
) -> Result<Option<ContentBlock>> {
    let text = if let Some(content) = content {
        content
            .iter()
            .map(|item| match item {
                ReasoningItemContent::ReasoningText { text }
                | ReasoningItemContent::Text { text } => text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        summary
            .iter()
            .map(|item| match item {
                ReasoningItemReasoningSummary::SummaryText { text } => text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    if text.is_empty() && encrypted_content.is_none() {
        return Ok(None);
    }

    if !text.is_empty() {
        let mut block = ReasoningTextBlock::builder().text(text);
        if let Some(signature) = encrypted_content {
            block = block.signature(signature.clone());
        }
        let block = block.build().map_err(|err| {
            CodexErr::Fatal(format!("failed to build Bedrock reasoning block: {err}"))
        })?;
        return Ok(Some(ContentBlock::ReasoningContent(
            ReasoningContentBlock::ReasoningText(block),
        )));
    }

    Ok(Some(ContentBlock::ReasoningContent(
        ReasoningContentBlock::RedactedContent(Blob::new(
            encrypted_content.clone().unwrap_or_default(),
        )),
    )))
}

fn parse_json_to_document(raw: &str) -> Document {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value_to_document(&value),
        Err(_) => Document::String(raw.to_string()),
    }
}

fn value_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::Number(value) => {
            if let Some(value) = value.as_u64() {
                Document::Number(Number::PosInt(value))
            } else if let Some(value) = value.as_i64() {
                if value < 0 {
                    Document::Number(Number::NegInt(value))
                } else {
                    Document::Number(Number::PosInt(value as u64))
                }
            } else if let Some(value) = value.as_f64() {
                Document::Number(Number::Float(value))
            } else {
                Document::Null
            }
        }
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(values.iter().map(value_to_document).collect()),
        Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), value_to_document(value)))
                .collect(),
        ),
    }
}

fn thinking_budget(effort: Option<ReasoningEffortConfig>, summary: ReasoningSummaryConfig) -> i64 {
    let base = match effort.unwrap_or(ReasoningEffortConfig::Medium) {
        ReasoningEffortConfig::None | ReasoningEffortConfig::Minimal => 0,
        ReasoningEffortConfig::Low => 4_096,
        ReasoningEffortConfig::Medium => 8_192,
        ReasoningEffortConfig::High => 16_384,
        ReasoningEffortConfig::XHigh => 32_768,
    };
    if base == 0 {
        0
    } else if summary == ReasoningSummaryConfig::None {
        (base / 2).max(1_024)
    } else {
        base
    }
}

fn is_retryable_request_error(err: &SdkError<ConverseStreamError>) -> bool {
    if let Some(service_error) = err.as_service_error() {
        return service_error.is_internal_server_exception()
            || service_error.is_model_not_ready_exception()
            || service_error.is_model_timeout_exception()
            || service_error.is_service_unavailable_exception()
            || service_error.is_throttling_exception()
            || service_error.is_model_stream_error_exception();
    }

    matches!(
        err,
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) | SdkError::ResponseError(_)
    )
}

fn map_request_error(err: SdkError<ConverseStreamError>) -> CodexErr {
    let message = format!("{}", DisplayErrorContext(&err));
    if let Some(service_error) = err.as_service_error()
        && service_error.is_access_denied_exception()
    {
        return CodexErr::Fatal(format!("Bedrock access denied: {message}"));
    }
    if let Some(code) = err.code() {
        return CodexErr::Fatal(format!("Bedrock request failed ({code}): {message}"));
    }
    CodexErr::Fatal(format!("Bedrock request failed: {message}"))
}

async fn process_bedrock_stream(
    mut stream: aws_sdk_bedrockruntime::primitives::event_stream::EventReceiver<
        ConverseStreamOutput,
        aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError,
    >,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
) {
    let mut usage: Option<TokenUsage> = None;
    let mut saw_message_start = false;
    let mut assistant_state: Option<AssistantState> = None;
    let mut blocks = HashMap::<i32, BlockState>::new();

    loop {
        let next_event = timeout(idle_timeout, stream.recv()).await;
        let event = match next_event {
            Ok(Ok(Some(event))) => event,
            Ok(Ok(None)) => break,
            Ok(Err(err)) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(
                        format!("Bedrock stream error: {}", DisplayErrorContext(&err)),
                        None,
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(
                        "idle timeout waiting for Bedrock stream".to_string(),
                        None,
                    )))
                    .await;
                return;
            }
        };

        match event {
            ConverseStreamOutput::MessageStart(event) => {
                trace!("Bedrock message start: {:?}", event.role());
                if !saw_message_start {
                    saw_message_start = true;
                    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
                        return;
                    }
                }
            }
            ConverseStreamOutput::ContentBlockStart(event) => {
                handle_content_block_start(&tx_event, &mut blocks, &mut assistant_state, event)
                    .await;
            }
            ConverseStreamOutput::ContentBlockDelta(event) => {
                handle_content_block_delta(&tx_event, &mut blocks, &mut assistant_state, event)
                    .await;
            }
            ConverseStreamOutput::ContentBlockStop(event) => {
                handle_content_block_stop(&tx_event, &mut blocks, event).await;
            }
            ConverseStreamOutput::MessageStop(event) => {
                trace!("Bedrock message stop: {}", event.stop_reason().as_str());
                flush_blocks(&tx_event, &mut blocks).await;
                if let Some(state) = assistant_state.take() {
                    finalize_assistant_state(&tx_event, state).await;
                }
                let _ = tx_event
                    .send(Ok(ResponseEvent::Completed {
                        response_id: String::new(),
                        token_usage: usage,
                    }))
                    .await;
                return;
            }
            ConverseStreamOutput::Metadata(event) => {
                usage = event.usage().map(map_bedrock_usage);
            }
            _ => {
                trace!("Ignoring unknown Bedrock stream event");
            }
        }
    }

    flush_blocks(&tx_event, &mut blocks).await;
    if let Some(state) = assistant_state.take() {
        finalize_assistant_state(&tx_event, state).await;
    }
    if !saw_message_start {
        let _ = tx_event
            .send(Err(CodexErr::Stream(
                "Bedrock returned no content".to_string(),
                None,
            )))
            .await;
        return;
    }
    let _ = tx_event
        .send(Ok(ResponseEvent::Completed {
            response_id: String::new(),
            token_usage: usage,
        }))
        .await;
}

fn map_bedrock_usage(usage: &aws_sdk_bedrockruntime::types::TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens() as i64,
        cached_input_tokens: usage.cache_read_input_tokens().unwrap_or_default() as i64,
        cache_write_input_tokens: usage.cache_write_input_tokens().unwrap_or_default() as i64,
        output_tokens: usage.output_tokens() as i64,
        reasoning_output_tokens: 0,
        total_tokens: usage.total_tokens() as i64,
    }
}

async fn handle_content_block_start(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i32, BlockState>,
    assistant_state: &mut Option<AssistantState>,
    event: aws_sdk_bedrockruntime::types::ContentBlockStartEvent,
) {
    let Some(start) = event.start() else {
        return;
    };
    if let aws_sdk_bedrockruntime::types::ContentBlockStart::ToolUse(tool_use) = start {
        if let Some(state) = assistant_state.take() {
            finalize_assistant_state(tx_event, state).await;
        }
        let state = ToolUseState {
            item: ResponseItem::FunctionCall {
                id: None,
                name: tool_use.name().to_string(),
                namespace: None,
                arguments: String::new(),
                call_id: tool_use.tool_use_id().to_string(),
            },
            buffer: String::new(),
        };
        if tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
            .await
            .is_err()
        {
            return;
        }
        blocks.insert(event.content_block_index(), BlockState::ToolUse(state));
    }
}

async fn handle_content_block_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i32, BlockState>,
    assistant_state: &mut Option<AssistantState>,
    event: aws_sdk_bedrockruntime::types::ContentBlockDeltaEvent,
) {
    let Some(delta) = event.delta() else {
        return;
    };
    match delta {
        ContentBlockDelta::Text(text) => {
            append_text_delta(tx_event, assistant_state, text).await;
        }
        ContentBlockDelta::ToolUse(tool_use) => {
            let Some(BlockState::ToolUse(state)) = blocks.get_mut(&event.content_block_index())
            else {
                return;
            };
            state.buffer.push_str(tool_use.input());
        }
        ContentBlockDelta::ReasoningContent(delta) => {
            append_reasoning_delta(tx_event, blocks, event.content_block_index(), delta).await;
        }
        ContentBlockDelta::Citation(_) => {}
        _ => {}
    }
}

async fn handle_content_block_stop(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i32, BlockState>,
    event: aws_sdk_bedrockruntime::types::ContentBlockStopEvent,
) {
    let Some(state) = blocks.remove(&event.content_block_index()) else {
        return;
    };
    finalize_block_state(tx_event, state).await;
}

async fn append_text_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    assistant_state: &mut Option<AssistantState>,
    text: &str,
) {
    ensure_assistant_state(assistant_state);
    let Some(state) = assistant_state.as_mut() else {
        return;
    };
    if !state.added {
        state.added = true;
        if tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = tx_event
        .send(Ok(ResponseEvent::OutputTextDelta(text.to_string())))
        .await;
    if let ResponseItem::Message { content, .. } = &mut state.item {
        content.push(ContentItem::OutputText {
            text: text.to_string(),
        });
    }
}

async fn append_reasoning_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i32, BlockState>,
    index: i32,
    delta: &ReasoningContentBlockDelta,
) {
    let state = blocks.entry(index).or_insert_with(|| {
        BlockState::Reasoning(ReasoningState {
            item: ResponseItem::Reasoning {
                id: String::new(),
                summary: Vec::new(),
                content: Some(Vec::new()),
                encrypted_content: None,
            },
            added: false,
            streamed_delta_count: 0,
            accumulated_text: String::new(),
            signature: None,
        })
    });
    let BlockState::Reasoning(state) = state else {
        return;
    };

    match delta {
        ReasoningContentBlockDelta::Text(text) => {
            if !state.added {
                state.added = true;
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            state.accumulated_text.push_str(text);
            let content_index = state.streamed_delta_count;
            state.streamed_delta_count += 1;
            let _ = tx_event
                .send(Ok(ResponseEvent::ReasoningContentDelta {
                    delta: text.to_string(),
                    content_index,
                }))
                .await;
        }
        ReasoningContentBlockDelta::Signature(signature) => {
            state.signature = Some(signature.clone());
        }
        ReasoningContentBlockDelta::RedactedContent(blob) => {
            state.signature = Some(String::from_utf8_lossy(blob.as_ref()).to_string());
        }
        _ => {}
    }
}

async fn flush_blocks(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    blocks: &mut HashMap<i32, BlockState>,
) {
    let mut indices = blocks.keys().copied().collect::<Vec<_>>();
    indices.sort_unstable();
    for index in indices {
        if let Some(state) = blocks.remove(&index) {
            finalize_block_state(tx_event, state).await;
        }
    }
}

async fn finalize_block_state(tx_event: &mpsc::Sender<Result<ResponseEvent>>, state: BlockState) {
    match state {
        BlockState::Reasoning(mut state) => {
            if let ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } = &mut state.item
            {
                if !state.accumulated_text.is_empty() {
                    let text = std::mem::take(&mut state.accumulated_text);
                    summary.push(ReasoningItemReasoningSummary::SummaryText { text: text.clone() });
                    if let Some(content) = content {
                        content.push(ReasoningItemContent::ReasoningText { text });
                    }
                }
                *encrypted_content = state.signature.take();
            }
            if !state.added {
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(state.item.clone())))
                    .await;
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                .await;
        }
        BlockState::ToolUse(mut state) => {
            if let ResponseItem::FunctionCall { arguments, .. } = &mut state.item {
                *arguments = std::mem::take(&mut state.buffer);
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(state.item)))
                .await;
        }
    }
}

async fn finalize_assistant_state(
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
    state: AssistantState,
) {
    let _ = tx_event
        .send(Ok(ResponseEvent::OutputItemDone(state.item)))
        .await;
}

fn ensure_assistant_state(assistant_state: &mut Option<AssistantState>) {
    if assistant_state.is_some() {
        return;
    }
    *assistant_state = Some(AssistantState {
        item: ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
            end_turn: None,
            phase: None,
        },
        added: false,
    });
}

#[cfg(test)]
mod tests {
    use aws_sdk_bedrockruntime::types::ContentBlockDeltaEvent;
    use aws_sdk_bedrockruntime::types::ContentBlockStart;
    use aws_sdk_bedrockruntime::types::ContentBlockStartEvent;
    use aws_sdk_bedrockruntime::types::ContentBlockStopEvent;
    use aws_sdk_bedrockruntime::types::OutputFormatStructure;
    use aws_sdk_bedrockruntime::types::OutputFormatType;
    use aws_sdk_bedrockruntime::types::TokenUsage as BedrockTokenUsage;
    use aws_sdk_bedrockruntime::types::ToolUseBlockStart;
    use pretty_assertions::assert_eq;

    use super::*;

    fn sample_model_info(slug: &str) -> ModelInfo {
        crate::models_manager::model_info::model_info_from_slug(slug)
    }

    #[test]
    fn build_messages_keeps_tool_use_after_assistant_text() {
        let prompt = Prompt {
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "hello".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{\"cmd\":\"pwd\"}".to_string(),
                    call_id: "call_1".to_string(),
                },
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "after".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
            ],
            ..Prompt::default()
        };

        let messages =
            build_messages(&prompt, &sample_model_info("amazon.nova-lite-v1:0")).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), &ConversationRole::Assistant);
        assert_eq!(messages[0].content().len(), 3);
        assert!(matches!(messages[0].content()[0], ContentBlock::Text(_)));
        assert!(matches!(messages[0].content()[1], ContentBlock::Text(_)));
        assert!(matches!(messages[0].content()[2], ContentBlock::ToolUse(_)));
    }

    #[test]
    fn tool_result_content_blocks_parse_json_text() {
        let payload = FunctionCallOutputPayload::from_text("{\"ok\":true}".to_string());
        assert_eq!(
            tool_result_content_blocks(&payload),
            vec![ToolResultContentBlock::Json(Document::Object(
                HashMap::from([("ok".to_string(), Document::Bool(true),)])
            ))]
        );
    }

    #[test]
    fn build_output_config_maps_json_schema() {
        let output_config = build_output_config(Some(&serde_json::json!({
            "title": "codex_response",
            "description": "Structured response",
            "type": "object",
            "properties": {
                "answer": { "type": "string" }
            },
            "required": ["answer"],
            "additionalProperties": false
        })))
        .expect("output config")
        .expect("output config should exist");
        let text_format = output_config.text_format().expect("text format");
        assert_eq!(text_format.r#type(), &OutputFormatType::JsonSchema);
        let structure = text_format.structure().expect("structure");
        assert!(matches!(
            structure,
            OutputFormatStructure::JsonSchema(schema)
                if schema.name() == Some("codex_response")
                    && schema.description() == Some("Structured response")
                    && schema.schema().contains("\"answer\"")
        ));
    }

    #[test]
    fn build_service_tier_maps_fast_and_flex() {
        let fast = build_service_tier(Some(ServiceTier::Fast)).expect("fast tier");
        let flex = build_service_tier(Some(ServiceTier::Flex)).expect("flex tier");

        assert_eq!(fast.r#type(), &BedrockServiceTierType::Priority);
        assert_eq!(flex.r#type(), &BedrockServiceTierType::Flex);
        assert_eq!(build_service_tier(None), None);
    }

    #[test]
    fn build_system_adds_cache_point_for_bedrock_claude() {
        let mut prompt = Prompt::default();
        prompt.base_instructions.text = "Be terse.".to_string();

        let system = build_system(
            &prompt,
            &sample_model_info("anthropic.claude-3-7-sonnet-20250219-v1:0"),
        )
        .expect("system");

        assert_eq!(system.len(), 2);
        assert!(matches!(system[0], SystemContentBlock::Text(_)));
        assert!(matches!(system[1], SystemContentBlock::CachePoint(_)));
    }

    #[test]
    fn build_messages_adds_cache_point_to_last_user_message_for_bedrock_claude() {
        let prompt = Prompt {
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "first".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: "middle".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "last".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
            ],
            ..Prompt::default()
        };

        let messages = build_messages(
            &prompt,
            &sample_model_info("anthropic.claude-3-7-sonnet-20250219-v1:0"),
        )
        .expect("messages");

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role(), &ConversationRole::User);
        assert!(matches!(messages[2].content()[0], ContentBlock::Text(_)));
        assert!(matches!(
            messages[2].content()[1],
            ContentBlock::CachePoint(_)
        ));
    }

    #[test]
    fn build_messages_does_not_create_synthetic_user_cache_point() {
        let prompt = Prompt {
            input: vec![ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "only assistant".to_string(),
                }],
                end_turn: None,
                phase: None,
            }],
            ..Prompt::default()
        };

        let messages = build_messages(
            &prompt,
            &sample_model_info("anthropic.claude-3-7-sonnet-20250219-v1:0"),
        )
        .expect("messages");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), &ConversationRole::Assistant);
        assert_eq!(messages[0].content().len(), 1);
        assert!(matches!(messages[0].content()[0], ContentBlock::Text(_)));
    }

    #[test]
    fn build_messages_skips_cache_point_for_non_claude_bedrock_models() {
        let prompt = Prompt {
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            }],
            ..Prompt::default()
        };

        let messages =
            build_messages(&prompt, &sample_model_info("amazon.nova-lite-v1:0")).expect("messages");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content().len(), 1);
        assert!(matches!(messages[0].content()[0], ContentBlock::Text(_)));
    }

    #[test]
    fn map_bedrock_usage_includes_cache_write_tokens() {
        let usage = BedrockTokenUsage::builder()
            .input_tokens(120)
            .cache_read_input_tokens(40)
            .cache_write_input_tokens(8)
            .output_tokens(30)
            .total_tokens(150)
            .build()
            .expect("usage");

        assert_eq!(
            map_bedrock_usage(&usage),
            TokenUsage {
                input_tokens: 120,
                cached_input_tokens: 40,
                cache_write_input_tokens: 8,
                output_tokens: 30,
                reasoning_output_tokens: 0,
                total_tokens: 150,
            }
        );
    }

    #[tokio::test]
    async fn stream_translation_emits_reasoning_and_tool_use_events() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut blocks = HashMap::new();
        let mut assistant_state = None;

        handle_content_block_delta(
            &tx,
            &mut blocks,
            &mut assistant_state,
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::ReasoningContent(
                    ReasoningContentBlockDelta::Text("think".to_string()),
                ))
                .build()
                .expect("delta"),
        )
        .await;
        handle_content_block_delta(
            &tx,
            &mut blocks,
            &mut assistant_state,
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::ReasoningContent(
                    ReasoningContentBlockDelta::Signature("sig".to_string()),
                ))
                .build()
                .expect("delta"),
        )
        .await;
        handle_content_block_stop(
            &tx,
            &mut blocks,
            ContentBlockStopEvent::builder()
                .content_block_index(0)
                .build()
                .expect("stop"),
        )
        .await;

        handle_content_block_start(
            &tx,
            &mut blocks,
            &mut assistant_state,
            ContentBlockStartEvent::builder()
                .content_block_index(1)
                .start(ContentBlockStart::ToolUse(
                    ToolUseBlockStart::builder()
                        .tool_use_id("call_1")
                        .name("shell")
                        .build()
                        .expect("tool start"),
                ))
                .build()
                .expect("start"),
        )
        .await;
        handle_content_block_delta(
            &tx,
            &mut blocks,
            &mut assistant_state,
            ContentBlockDeltaEvent::builder()
                .content_block_index(1)
                .delta(ContentBlockDelta::ToolUse(
                    aws_sdk_bedrockruntime::types::ToolUseBlockDelta::builder()
                        .input("{\"cmd\":\"pwd\"}".to_string())
                        .build()
                        .expect("tool delta"),
                ))
                .build()
                .expect("delta"),
        )
        .await;
        handle_content_block_stop(
            &tx,
            &mut blocks,
            ContentBlockStopEvent::builder()
                .content_block_index(1)
                .build()
                .expect("stop"),
        )
        .await;

        let events = [
            rx.recv().await.expect("event").expect("ok"),
            rx.recv().await.expect("event").expect("ok"),
            rx.recv().await.expect("event").expect("ok"),
            rx.recv().await.expect("event").expect("ok"),
            rx.recv().await.expect("event").expect("ok"),
        ];

        assert!(matches!(
            &events[0],
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
        ));
        assert!(matches!(
            &events[1],
            ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "think"
        ));
        assert!(matches!(
            &events[2],
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                summary,
                encrypted_content,
                ..
            }) if summary == &vec![ReasoningItemReasoningSummary::SummaryText {
                text: "think".to_string(),
            }] && encrypted_content.as_deref() == Some("sig")
        ));
        assert!(matches!(
            &events[3],
            ResponseEvent::OutputItemAdded(ResponseItem::FunctionCall { name, .. }) if name == "shell"
        ));
        assert!(matches!(
            &events[4],
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { arguments, .. })
                if arguments == "{\"cmd\":\"pwd\"}"
        ));
    }
}
