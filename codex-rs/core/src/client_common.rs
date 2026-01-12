use crate::client_common::tools::ToolSpec;
use crate::error::Result;
use crate::openai_models::model_family::ANTIGRAVITY_PREAMBLE;
use crate::openai_models::model_family::InstructionMode;
use crate::openai_models::model_family::ModelFamily;
use crate::openai_models::model_family::PRAXIS_REST;
pub use codex_api::common::ResponseEvent;
use codex_apply_patch::APPLY_PATCH_TOOL_INSTRUCTIONS;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Deref;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;

/// Sandbox and approvals instructions. Injected into all prompts.
pub const REVIEW_PROMPT: &str = include_str!("../review_prompt.md");

// Centralized templates for review-related user messages
#[allow(dead_code)]
pub const REVIEW_EXIT_SUCCESS_TMPL: &str = include_str!("../templates/review/exit_success.xml");
#[allow(dead_code)]
pub const REVIEW_EXIT_INTERRUPTED_TMPL: &str =
    include_str!("../templates/review/exit_interrupted.xml");

/// Sandbox and approvals instructions. Injected into all prompts.
pub const SANDBOX_AND_APPROVALS_PROMPT: &str = include_str!("../sandbox_and_approvals.md");

/// API request payload for a single model turn
#[derive(Default, Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Vec<ResponseItem>,

    /// Tools available to the model, including additional tools sourced from
    /// external MCP servers.
    pub(crate) tools: Vec<ToolSpec>,

    /// Optional override for the built-in BASE_INSTRUCTIONS.
    pub base_instructions_override: Option<String>,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,
}

impl Prompt {
    pub(crate) fn get_full_instructions<'a>(&'a self, model: &'a ModelFamily) -> Cow<'a, str> {
        match model.instruction_mode {
            InstructionMode::Strict => {
                // OpenAI GPT-5.x: use exact base_instructions unchanged.
                // Sandbox info and other context goes in user message.
                Cow::Borrowed(model.base_instructions.deref())
            }
            InstructionMode::Prefix => {
                // Antigravity: prepend Antigravity preamble to PRAXIS_REST or subagent prompt.
                let content = self
                    .base_instructions_override
                    .as_deref()
                    .unwrap_or(PRAXIS_REST);
                let mut result = format!("{ANTIGRAVITY_PREAMBLE}\n\n{content}");
                self.append_apply_patch_instructions_if_needed(model, &mut result);
                Cow::Owned(result)
            }
            InstructionMode::Flexible => {
                // Claude/Gemini/OpenRouter: full control over instructions.
                let base: Cow<'a, str> = match self.base_instructions_override.as_deref() {
                    Some(override_text) => Cow::Borrowed(override_text),
                    None => Cow::Borrowed(model.base_instructions.deref()),
                };
                let mut result = base.into_owned();
                self.append_apply_patch_instructions_if_needed(model, &mut result);
                Cow::Owned(result)
            }
        }
    }

    fn append_apply_patch_instructions_if_needed(&self, model: &ModelFamily, result: &mut String) {
        let is_apply_patch_tool_present = self.tools.iter().any(|tool| match tool {
            ToolSpec::Function(f) => f.name == "apply_patch",
            ToolSpec::Freeform(f) => f.name == "apply_patch",
            _ => false,
        });
        if model.needs_special_apply_patch_instructions && !is_apply_patch_tool_present {
            result.push('\n');
            result.push_str(APPLY_PATCH_TOOL_INSTRUCTIONS);
        }
    }

    pub(crate) fn get_formatted_input(&self) -> Vec<ResponseItem> {
        let mut input = self.input.clone();

        // when using the *Freeform* apply_patch tool specifically, tool outputs
        // should be structured text, not json. Do NOT reserialize when using
        // the Function tool - note that this differs from the check above for
        // instructions. We declare the result as a named variable for clarity.
        let is_freeform_apply_patch_tool_present = self.tools.iter().any(|tool| match tool {
            ToolSpec::Freeform(f) => f.name == "apply_patch",
            _ => false,
        });
        if is_freeform_apply_patch_tool_present {
            reserialize_shell_outputs(&mut input);
        }

        input
    }

    pub(crate) fn get_formatted_input_for_model(&self, model: &ModelFamily) -> Vec<ResponseItem> {
        let mut input = self.get_formatted_input();

        let strict_instructions_include_sandbox = model
            .base_instructions
            .contains("Filesystem sandboxing defines which files can be read or written.");
        if model.instruction_mode != InstructionMode::Strict || !strict_instructions_include_sandbox
        {
            prepend_sandbox_and_approvals_to_first_user_message(&mut input);
        }

        input
    }
}

fn prepend_sandbox_and_approvals_to_first_user_message(items: &mut Vec<ResponseItem>) {
    let sandbox_marker = "## Filesystem sandboxing";

    for item in items {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "user" {
            continue;
        }

        let already_present = content.iter().any(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                text.contains(sandbox_marker)
            }
            ContentItem::InputImage { .. } => false,
        });
        if !already_present {
            content.insert(
                0,
                ContentItem::InputText {
                    text: SANDBOX_AND_APPROVALS_PROMPT.to_string(),
                },
            );
        }

        break;
    }
}

fn reserialize_shell_outputs(items: &mut [ResponseItem]) {
    let mut shell_call_ids: HashSet<String> = HashSet::new();

    items.iter_mut().for_each(|item| match item {
        ResponseItem::CustomToolCall {
            id: _,
            status: _,
            call_id,
            name,
            input: _,
        } => {
            if name == "apply_patch" {
                shell_call_ids.insert(call_id.clone());
            }
        }
        ResponseItem::CustomToolCallOutput { call_id, output } => {
            if shell_call_ids.remove(call_id)
                && let Some(structured) = parse_structured_shell_output(output)
            {
                *output = structured
            }
        }
        ResponseItem::FunctionCall { name, call_id, .. }
            if is_shell_tool_name(name) || name == "apply_patch" =>
        {
            shell_call_ids.insert(call_id.clone());
        }
        ResponseItem::FunctionCallOutput { call_id, output } => {
            if shell_call_ids.remove(call_id)
                && let Some(structured) = parse_structured_shell_output(&output.content)
            {
                output.content = structured
            }
        }
        _ => {}
    })
}

fn is_shell_tool_name(name: &str) -> bool {
    matches!(name, "shell" | "shell_command" | "container.exec")
}

#[derive(Deserialize)]
struct ExecOutputJson {
    output: String,
    metadata: ExecOutputMetadataJson,
}

#[derive(Deserialize)]
struct ExecOutputMetadataJson {
    exit_code: i32,
    duration_seconds: f32,
}

fn parse_structured_shell_output(raw: &str) -> Option<String> {
    let parsed: ExecOutputJson = serde_json::from_str(raw).ok()?;
    Some(build_structured_output(&parsed))
}

fn build_structured_output(parsed: &ExecOutputJson) -> String {
    let mut sections = Vec::new();
    sections.push(format!("Exit code: {}", parsed.metadata.exit_code));
    sections.push(format!(
        "Wall time: {} seconds",
        parsed.metadata.duration_seconds
    ));

    let mut output = parsed.output.clone();
    if let Some((stripped, total_lines)) = strip_total_output_header(&parsed.output) {
        sections.push(format!("Total output lines: {total_lines}"));
        output = stripped.to_string();
    }

    sections.push("Output:".to_string());
    sections.push(output);

    sections.join("\n")
}

fn strip_total_output_header(output: &str) -> Option<(&str, u32)> {
    let after_prefix = output.strip_prefix("Total output lines: ")?;
    let (total_segment, remainder) = after_prefix.split_once('\n')?;
    let total_lines = total_segment.parse::<u32>().ok()?;
    let remainder = remainder.strip_prefix('\n').unwrap_or(remainder);
    Some((remainder, total_lines))
}

pub(crate) mod tools {
    use crate::tools::spec::JsonSchema;
    use serde::Deserialize;
    use serde::Serialize;

    /// When serialized as JSON, this produces a valid "Tool" in the OpenAI
    /// Responses API.
    #[derive(Debug, Clone, Serialize, PartialEq)]
    #[serde(tag = "type")]
    pub(crate) enum ToolSpec {
        #[serde(rename = "function")]
        Function(ResponsesApiTool),
        // TODO: Understand why we get an error on web_search although the API docs say it's supported.
        // https://platform.openai.com/docs/guides/tools-web-search?api-mode=responses#:~:text=%7B%20type%3A%20%22web_search%22%20%7D%2C
        #[serde(rename = "web_search")]
        WebSearch {},
        #[serde(rename = "custom")]
        Freeform(FreeformTool),
    }

    impl ToolSpec {
        pub(crate) fn name(&self) -> &str {
            match self {
                ToolSpec::Function(tool) => tool.name.as_str(),
                ToolSpec::WebSearch {} => "web_search",
                ToolSpec::Freeform(tool) => tool.name.as_str(),
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct FreeformTool {
        pub(crate) name: String,
        pub(crate) description: String,
        pub(crate) format: FreeformToolFormat,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct FreeformToolFormat {
        pub(crate) r#type: String,
        pub(crate) syntax: String,
        pub(crate) definition: String,
    }

    #[derive(Debug, Clone, Serialize, PartialEq)]
    pub struct ResponsesApiTool {
        pub(crate) name: String,
        pub(crate) description: String,
        /// TODO: Validation. When strict is set to true, the JSON schema,
        /// `required` and `additional_properties` must be present. All fields in
        /// `properties` must be present in `required`.
        pub(crate) strict: bool,
        pub(crate) parameters: JsonSchema,
    }
}

pub struct ResponseStream {
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use codex_api::ResponsesApiRequest;
    use codex_api::common::OpenAiVerbosity;
    use codex_api::common::TextControls;
    use codex_api::create_text_param_for_request;
    use pretty_assertions::assert_eq;

    use crate::config::test_config;
    use crate::model_provider_info::ProviderKind;
    use crate::openai_models::model_family::InstructionMode;
    use crate::openai_models::models_manager::ModelsManager;

    use super::*;

    struct InstructionsTestCase {
        pub slug: &'static str,
        pub expects_apply_patch_instructions: bool,
        pub expected_instruction_mode: InstructionMode,
    }
    #[test]
    fn get_full_instructions_no_user_content() {
        let prompt = Prompt {
            ..Default::default()
        };
        let test_cases = vec![
            InstructionsTestCase {
                slug: "gpt-3.5",
                expects_apply_patch_instructions: true,
                expected_instruction_mode: InstructionMode::Flexible,
            },
            InstructionsTestCase {
                slug: "gpt-4.1",
                expects_apply_patch_instructions: true,
                expected_instruction_mode: InstructionMode::Flexible,
            },
            InstructionsTestCase {
                slug: "gpt-4o",
                expects_apply_patch_instructions: true,
                expected_instruction_mode: InstructionMode::Flexible,
            },
            InstructionsTestCase {
                slug: "gpt-5.1",
                expects_apply_patch_instructions: false,
                expected_instruction_mode: InstructionMode::Strict,
            },
            InstructionsTestCase {
                slug: "gpt-oss:120b",
                expects_apply_patch_instructions: false,
                expected_instruction_mode: InstructionMode::Flexible,
            },
            InstructionsTestCase {
                slug: "gpt-5.1-codex",
                expects_apply_patch_instructions: false,
                expected_instruction_mode: InstructionMode::Strict,
            },
            InstructionsTestCase {
                slug: "gpt-5.1-codex-max",
                expects_apply_patch_instructions: false,
                expected_instruction_mode: InstructionMode::Strict,
            },
        ];
        for test_case in test_cases {
            let config = test_config();
            let model_family =
                ModelsManager::construct_model_family_offline(test_case.slug, &config);

            assert_eq!(
                model_family.instruction_mode, test_case.expected_instruction_mode,
                "model {} should have {:?} instruction mode",
                test_case.slug, test_case.expected_instruction_mode
            );

            let expected = match model_family.instruction_mode {
                InstructionMode::Strict => {
                    // Strict mode returns base_instructions unchanged
                    model_family.base_instructions.to_string()
                }
                InstructionMode::Prefix | InstructionMode::Flexible => {
                    let mut result = model_family.base_instructions.to_string();
                    if test_case.expects_apply_patch_instructions {
                        result.push('\n');
                        result.push_str(APPLY_PATCH_TOOL_INSTRUCTIONS);
                    }
                    result
                }
            };

            let full = prompt.get_full_instructions(&model_family);
            assert_eq!(
                full.as_ref(),
                expected.as_str(),
                "instructions mismatch for model {}",
                test_case.slug
            );
        }
    }

    #[test]
    fn formatted_input_prepends_sandbox_for_non_strict_modes() {
        let config = test_config();
        let model_family = ModelsManager::construct_model_family_offline("claude-sonnet", &config);

        assert_eq!(model_family.instruction_mode, InstructionMode::Flexible);

        let prompt = Prompt {
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
            }],
            ..Default::default()
        };
        let formatted = prompt.get_formatted_input_for_model(&model_family);

        let first_user = formatted
            .iter()
            .find_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "user" => Some(content),
                _ => None,
            })
            .expect("should have user message");

        let combined = first_user
            .iter()
            .filter_map(|item| match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                    (!text.is_empty()).then_some(text.as_str())
                }
                ContentItem::InputImage { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("## Filesystem sandboxing"));

        let mut prefix_config = test_config();
        prefix_config.model_provider.provider_kind = ProviderKind::Antigravity;
        let prefix_family = ModelsManager::construct_model_family_offline("gpt-4o", &prefix_config);
        assert_eq!(prefix_family.instruction_mode, InstructionMode::Prefix);

        let formatted_prefix = prompt.get_formatted_input_for_model(&prefix_family);
        let first_user_prefix = formatted_prefix
            .iter()
            .find_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "user" => Some(content),
                _ => None,
            })
            .expect("should have user message");
        let combined_prefix = first_user_prefix
            .iter()
            .filter_map(|item| match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                    (!text.is_empty()).then_some(text.as_str())
                }
                ContentItem::InputImage { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined_prefix.contains("## Filesystem sandboxing"));
    }

    #[test]
    fn instruction_mode_strict_uses_base_only() {
        let config = test_config();
        let model_family = ModelsManager::construct_model_family_offline("gpt-5.1-codex", &config);

        assert_eq!(model_family.instruction_mode, InstructionMode::Strict);

        let prompt = Prompt::default();
        let instructions = prompt.get_full_instructions(&model_family);

        assert_eq!(
            instructions.as_ref(),
            model_family.base_instructions.as_str(),
            "Strict mode should return base_instructions unchanged"
        );
    }

    #[test]
    fn instruction_mode_prefix_prepends_antigravity_preamble() {
        let mut config = test_config();
        config.model_provider.provider_kind = ProviderKind::Antigravity;
        let model_family = ModelsManager::construct_model_family_offline("gpt-4o", &config);

        assert_eq!(model_family.instruction_mode, InstructionMode::Prefix);

        let prompt = Prompt::default();
        let instructions = prompt.get_full_instructions(&model_family);

        assert!(
            instructions.starts_with(ANTIGRAVITY_PREAMBLE),
            "Prefix mode should start with Antigravity preamble"
        );
    }

    #[test]
    fn subagent_override_with_flexible_mode() {
        let config = test_config();
        let model_family = ModelsManager::construct_model_family_offline("claude-sonnet", &config);

        let prompt = Prompt {
            base_instructions_override: Some("Custom subagent prompt".to_string()),
            ..Default::default()
        };
        let instructions = prompt.get_full_instructions(&model_family);

        assert!(
            instructions.starts_with("Custom subagent prompt"),
            "Flexible mode should use override as base"
        );
        assert!(!instructions.contains("## Filesystem sandboxing"));
    }

    #[test]
    fn subagent_override_with_strict_mode_ignored() {
        let config = test_config();
        let model_family = ModelsManager::construct_model_family_offline("gpt-5.1", &config);

        let prompt = Prompt {
            base_instructions_override: Some("Custom subagent prompt".to_string()),
            ..Default::default()
        };
        let instructions = prompt.get_full_instructions(&model_family);

        // Strict mode ignores override - always uses base_instructions
        assert_eq!(
            instructions.as_ref(),
            model_family.base_instructions.as_str(),
            "Strict mode should ignore override"
        );
    }

    #[test]
    fn subagent_override_with_prefix_mode() {
        let mut config = test_config();
        config.model_provider.provider_kind = ProviderKind::Antigravity;
        let model_family = ModelsManager::construct_model_family_offline("gpt-4o", &config);

        assert_eq!(model_family.instruction_mode, InstructionMode::Prefix);

        let prompt = Prompt {
            base_instructions_override: Some("Custom subagent prompt".to_string()),
            ..Default::default()
        };
        let instructions = prompt.get_full_instructions(&model_family);

        assert!(
            instructions.starts_with(ANTIGRAVITY_PREAMBLE),
            "Prefix mode should prepend Antigravity preamble"
        );
        assert!(
            instructions.contains("Custom subagent prompt"),
            "Prefix mode should include the override content"
        );
    }

    #[test]
    fn serializes_text_verbosity_when_set() {
        let input: Vec<ResponseItem> = vec![];
        let tools: Vec<serde_json::Value> = vec![];
        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            instructions: "i",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            prompt_cache_key: None,
            text: Some(TextControls {
                verbosity: Some(OpenAiVerbosity::Low),
                format: None,
            }),
        };

        let v = serde_json::to_value(&req).expect("json");
        assert_eq!(
            v.get("text")
                .and_then(|t| t.get("verbosity"))
                .and_then(|s| s.as_str()),
            Some("low")
        );
    }

    #[test]
    fn serializes_text_schema_with_strict_format() {
        let input: Vec<ResponseItem> = vec![];
        let tools: Vec<serde_json::Value> = vec![];
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"}
            },
            "required": ["answer"],
        });
        let text_controls =
            create_text_param_for_request(None, &Some(schema.clone())).expect("text controls");

        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            instructions: "i",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            prompt_cache_key: None,
            text: Some(text_controls),
        };

        let v = serde_json::to_value(&req).expect("json");
        let text = v.get("text").expect("text field");
        assert!(text.get("verbosity").is_none());
        let format = text.get("format").expect("format field");

        assert_eq!(
            format.get("name"),
            Some(&serde_json::Value::String("codex_output_schema".into()))
        );
        assert_eq!(
            format.get("type"),
            Some(&serde_json::Value::String("json_schema".into()))
        );
        assert_eq!(format.get("strict"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(format.get("schema"), Some(&schema));
    }

    #[test]
    fn omits_text_when_not_set() {
        let input: Vec<ResponseItem> = vec![];
        let tools: Vec<serde_json::Value> = vec![];
        let req = ResponsesApiRequest {
            model: "gpt-5.1",
            instructions: "i",
            input: &input,
            tools: &tools,
            tool_choice: "auto",
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            include: vec![],
            prompt_cache_key: None,
            text: None,
        };

        let v = serde_json::to_value(&req).expect("json");
        assert!(v.get("text").is_none());
    }
}
