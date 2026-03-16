use crate::config::CollaborationModeProfile;
use crate::config::CollaborationModeProfiles;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;
use codex_protocol::openai_models::ReasoningEffort;

const COLLABORATION_MODE_PLAN: &str = include_str!("../../templates/collaboration_mode/plan.md");
const COLLABORATION_MODE_DEFAULT: &str =
    include_str!("../../templates/collaboration_mode/default.md");
const COLLABORATION_MODE_SMART: &str = include_str!("../../templates/collaboration_mode/smart.md");
const COLLABORATION_MODE_DEEP: &str = include_str!("../../templates/collaboration_mode/deep.md");
const COLLABORATION_MODE_RUSH: &str = include_str!("../../templates/collaboration_mode/rush.md");
const KNOWN_MODE_NAMES_PLACEHOLDER: &str = "{{KNOWN_MODE_NAMES}}";
const REQUEST_USER_INPUT_AVAILABILITY_PLACEHOLDER: &str = "{{REQUEST_USER_INPUT_AVAILABILITY}}";
const ASKING_QUESTIONS_GUIDANCE_PLACEHOLDER: &str = "{{ASKING_QUESTIONS_GUIDANCE}}";

/// Stores feature flags that control collaboration-mode behavior.
///
/// Keep mode-related flags here so new collaboration-mode capabilities can be
/// added without large cross-cutting diffs to constructor and call-site
/// signatures.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollaborationModesConfig {
    /// Enables `request_user_input` availability in Default mode.
    pub default_mode_request_user_input: bool,
    pub collaboration_mode_profiles: CollaborationModeProfiles,
}

pub(crate) fn builtin_collaboration_mode_presets(
    collaboration_modes_config: CollaborationModesConfig,
) -> Vec<CollaborationModeMask> {
    let CollaborationModesConfig {
        default_mode_request_user_input,
        collaboration_mode_profiles,
    } = collaboration_modes_config;

    let mut presets = vec![default_preset(CollaborationModesConfig {
        default_mode_request_user_input,
        collaboration_mode_profiles: collaboration_mode_profiles.clone(),
    })];
    if let Some(profile) = collaboration_mode_profiles.smart.as_ref() {
        presets.push(profile_backed_preset(
            ModeKind::Smart,
            profile,
            COLLABORATION_MODE_SMART,
        ));
    }
    if let Some(profile) = collaboration_mode_profiles.deep.as_ref() {
        presets.push(profile_backed_preset(
            ModeKind::Deep,
            profile,
            COLLABORATION_MODE_DEEP,
        ));
    }
    if let Some(profile) = collaboration_mode_profiles.rush.as_ref() {
        presets.push(profile_backed_preset(
            ModeKind::Rush,
            profile,
            COLLABORATION_MODE_RUSH,
        ));
    }
    presets.push(plan_preset());
    presets
}

fn plan_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: ModeKind::Plan.display_name().to_string(),
        mode: Some(ModeKind::Plan),
        model: None,
        reasoning_effort: Some(Some(ReasoningEffort::Medium)),
        developer_instructions: Some(Some(COLLABORATION_MODE_PLAN.to_string())),
    }
}

fn default_preset(collaboration_modes_config: CollaborationModesConfig) -> CollaborationModeMask {
    CollaborationModeMask {
        name: ModeKind::Default.display_name().to_string(),
        mode: Some(ModeKind::Default),
        model: None,
        reasoning_effort: None,
        developer_instructions: Some(Some(default_mode_instructions(collaboration_modes_config))),
    }
}

fn profile_backed_preset(
    mode: ModeKind,
    profile: &CollaborationModeProfile,
    instructions: &str,
) -> CollaborationModeMask {
    CollaborationModeMask {
        name: mode.display_name().to_string(),
        mode: Some(mode),
        model: profile.model.clone(),
        reasoning_effort: Some(profile.model_reasoning_effort),
        developer_instructions: Some(Some(instructions.to_string())),
    }
}

fn default_mode_instructions(collaboration_modes_config: CollaborationModesConfig) -> String {
    let known_mode_names = format_mode_names(&configured_visible_modes(
        &collaboration_modes_config.collaboration_mode_profiles,
    ));
    let request_user_input_availability = request_user_input_availability_message(
        ModeKind::Default,
        collaboration_modes_config.default_mode_request_user_input,
    );
    let asking_questions_guidance = asking_questions_guidance_message(
        collaboration_modes_config.default_mode_request_user_input,
    );
    COLLABORATION_MODE_DEFAULT
        .replace(KNOWN_MODE_NAMES_PLACEHOLDER, &known_mode_names)
        .replace(
            REQUEST_USER_INPUT_AVAILABILITY_PLACEHOLDER,
            &request_user_input_availability,
        )
        .replace(
            ASKING_QUESTIONS_GUIDANCE_PLACEHOLDER,
            &asking_questions_guidance,
        )
}

fn configured_visible_modes(profiles: &CollaborationModeProfiles) -> Vec<ModeKind> {
    let mut modes = vec![ModeKind::Default];
    if profiles.smart.is_some() {
        modes.push(ModeKind::Smart);
    }
    if profiles.deep.is_some() {
        modes.push(ModeKind::Deep);
    }
    if profiles.rush.is_some() {
        modes.push(ModeKind::Rush);
    }
    modes.push(ModeKind::Plan);
    modes
}

fn format_mode_names(modes: &[ModeKind]) -> String {
    let mode_names: Vec<&str> = modes.iter().map(|mode| mode.display_name()).collect();
    match mode_names.as_slice() {
        [] => "none".to_string(),
        [mode_name] => (*mode_name).to_string(),
        [first, second] => format!("{first} and {second}"),
        [..] => mode_names.join(", "),
    }
}

fn request_user_input_availability_message(
    mode: ModeKind,
    default_mode_request_user_input: bool,
) -> String {
    let mode_name = mode.display_name();
    if mode.allows_request_user_input()
        || (default_mode_request_user_input && mode == ModeKind::Default)
    {
        format!("The `request_user_input` tool is available in {mode_name} mode.")
    } else {
        format!(
            "The `request_user_input` tool is unavailable in {mode_name} mode. If you call it while in {mode_name} mode, it will return an error."
        )
    }
}

fn asking_questions_guidance_message(default_mode_request_user_input: bool) -> String {
    if default_mode_request_user_input {
        "In Default mode, strongly prefer making reasonable assumptions and executing the user's request rather than stopping to ask questions. If you absolutely must ask a question because the answer cannot be discovered from local context and a reasonable assumption would be risky, prefer using the `request_user_input` tool rather than writing a multiple choice question as a textual assistant message. Never write a multiple choice question as a textual assistant message.".to_string()
    } else {
        "In Default mode, strongly prefer making reasonable assumptions and executing the user's request rather than stopping to ask questions. If you absolutely must ask a question because the answer cannot be discovered from local context and a reasonable assumption would be risky, ask the user directly with a concise plain-text question. Never write a multiple choice question as a textual assistant message.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn preset_names_use_mode_display_names() {
        assert_eq!(plan_preset().name, ModeKind::Plan.display_name());
        assert_eq!(
            default_preset(CollaborationModesConfig::default()).name,
            ModeKind::Default.display_name()
        );
        assert_eq!(
            plan_preset().reasoning_effort,
            Some(Some(ReasoningEffort::Medium))
        );
    }

    #[test]
    fn profile_backed_modes_are_in_tui_order_and_hidden_when_unset() {
        let presets = builtin_collaboration_mode_presets(CollaborationModesConfig {
            default_mode_request_user_input: false,
            collaboration_mode_profiles: CollaborationModeProfiles {
                smart: Some(CollaborationModeProfile {
                    profile: "smart".to_string(),
                    model_provider_id: "openai".to_string(),
                    model_provider: crate::built_in_model_providers()["openai"].clone(),
                    model: Some("gpt-5".to_string()),
                    model_reasoning_effort: Some(ReasoningEffort::Medium),
                }),
                deep: None,
                rush: Some(CollaborationModeProfile {
                    profile: "rush".to_string(),
                    model_provider_id: "openai".to_string(),
                    model_provider: crate::built_in_model_providers()["openai"].clone(),
                    model: Some("gpt-5-mini".to_string()),
                    model_reasoning_effort: None,
                }),
            },
        });
        let actual = presets
            .into_iter()
            .map(|preset| preset.mode.expect("preset mode"))
            .collect::<Vec<_>>();
        let expected = vec![
            ModeKind::Default,
            ModeKind::Smart,
            ModeKind::Rush,
            ModeKind::Plan,
        ];
        assert_eq!(expected, actual);
    }

    #[test]
    fn default_mode_instructions_replace_mode_names_placeholder() {
        let default_instructions = default_preset(CollaborationModesConfig {
            default_mode_request_user_input: true,
            ..Default::default()
        })
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");

        assert!(!default_instructions.contains(KNOWN_MODE_NAMES_PLACEHOLDER));
        assert!(!default_instructions.contains(REQUEST_USER_INPUT_AVAILABILITY_PLACEHOLDER));
        assert!(!default_instructions.contains(ASKING_QUESTIONS_GUIDANCE_PLACEHOLDER));

        let known_mode_names = format_mode_names(&configured_visible_modes(
            &CollaborationModeProfiles::default(),
        ));
        let expected_snippet = format!("Known mode names are {known_mode_names}.");
        assert!(default_instructions.contains(&expected_snippet));

        let expected_availability_message =
            request_user_input_availability_message(ModeKind::Default, true);
        assert!(default_instructions.contains(&expected_availability_message));
        assert!(default_instructions.contains("prefer using the `request_user_input` tool"));
    }

    #[test]
    fn default_mode_instructions_use_plain_text_questions_when_feature_disabled() {
        let default_instructions = default_preset(CollaborationModesConfig::default())
            .developer_instructions
            .expect("default preset should include instructions")
            .expect("default instructions should be set");

        assert!(!default_instructions.contains("prefer using the `request_user_input` tool"));
        assert!(
            default_instructions
                .contains("ask the user directly with a concise plain-text question")
        );
    }

    #[test]
    fn default_mode_instructions_only_list_configured_visible_modes() {
        let default_instructions = default_preset(CollaborationModesConfig {
            collaboration_mode_profiles: CollaborationModeProfiles {
                smart: Some(CollaborationModeProfile {
                    profile: "smart".to_string(),
                    model_provider_id: "openai".to_string(),
                    model_provider: crate::built_in_model_providers()["openai"].clone(),
                    model: Some("gpt-5".to_string()),
                    model_reasoning_effort: Some(ReasoningEffort::Medium),
                }),
                deep: None,
                rush: Some(CollaborationModeProfile {
                    profile: "rush".to_string(),
                    model_provider_id: "openai".to_string(),
                    model_provider: crate::built_in_model_providers()["openai"].clone(),
                    model: Some("gpt-5-mini".to_string()),
                    model_reasoning_effort: None,
                }),
            },
            ..Default::default()
        })
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");

        assert!(default_instructions.contains("Known mode names are Default, Smart, Rush, Plan."));
        assert!(!default_instructions.contains("Default, Smart, Deep, Rush, Plan"));
    }
}
