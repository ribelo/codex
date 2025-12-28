//! Provider-specific profile configuration resolution.

use crate::error::CodexErr;
use crate::model_provider_info::ProviderKind;
use crate::openrouter_types::OpenRouterProfileConfig;
use serde::Deserialize;
use toml::Table as TomlTable;

/// Resolved provider-specific profile configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderProfileConfig {
    OpenRouter(OpenRouterProfileConfig),
    OpenAi(OpenAiProfileConfig),
    Anthropic(AnthropicProfileConfig),
    Gemini(GeminiProfileConfig),
    Bedrock(BedrockProfileConfig),
    /// Provider has no specific config or none was provided
    None,
}

impl Default for ProviderProfileConfig {
    fn default() -> Self {
        Self::None
    }
}

/// OpenAI-specific profile configuration (placeholder for future).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiProfileConfig {}

/// Anthropic-specific profile configuration (placeholder for future).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicProfileConfig {}

/// Gemini-specific profile configuration (placeholder for future).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeminiProfileConfig {}

/// Bedrock-specific profile configuration (placeholder for future).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockProfileConfig {}

/// Resolve raw provider config table into typed struct based on provider kind.
pub fn resolve_provider_config(
    raw_table: Option<&TomlTable>,
    provider_kind: ProviderKind,
    profile_name: &str,
) -> Result<ProviderProfileConfig, CodexErr> {
    let Some(table) = raw_table else {
        return Ok(ProviderProfileConfig::None);
    };

    if table.is_empty() {
        return Ok(ProviderProfileConfig::None);
    }

    let value = toml::Value::Table(table.clone());

    match provider_kind {
        ProviderKind::OpenRouter => {
            let config: OpenRouterProfileConfig = value.try_into().map_err(|e| {
                CodexErr::ConfigError(format!(
                    "Invalid OpenRouter config in profile '{profile_name}': {e}"
                ))
            })?;
            Ok(ProviderProfileConfig::OpenRouter(config))
        }
        ProviderKind::OpenAi => {
            let config: OpenAiProfileConfig = value.try_into().map_err(|e| {
                CodexErr::ConfigError(format!(
                    "Invalid OpenAI config in profile '{profile_name}': {e}"
                ))
            })?;
            Ok(ProviderProfileConfig::OpenAi(config))
        }
        ProviderKind::Anthropic => {
            let config: AnthropicProfileConfig = value.try_into().map_err(|e| {
                CodexErr::ConfigError(format!(
                    "Invalid Anthropic config in profile '{profile_name}': {e}"
                ))
            })?;
            Ok(ProviderProfileConfig::Anthropic(config))
        }
        ProviderKind::Gemini => {
            let config: GeminiProfileConfig = value.try_into().map_err(|e| {
                CodexErr::ConfigError(format!(
                    "Invalid Gemini config in profile '{profile_name}': {e}"
                ))
            })?;
            Ok(ProviderProfileConfig::Gemini(config))
        }
        ProviderKind::Bedrock => {
            let config: BedrockProfileConfig = value.try_into().map_err(|e| {
                CodexErr::ConfigError(format!(
                    "Invalid Bedrock config in profile '{profile_name}': {e}"
                ))
            })?;
            Ok(ProviderProfileConfig::Bedrock(config))
        }
        ProviderKind::Antigravity => Err(CodexErr::ConfigError(format!(
            "Profile '{profile_name}' has [provider] config but Antigravity doesn't support provider-specific configuration"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn resolve_openrouter_provider_config() {
        let mut table = TomlTable::new();
        let mut routing = TomlTable::new();
        routing.insert("order".into(), toml::Value::Array(vec!["xai".into()]));
        table.insert("routing".into(), toml::Value::Table(routing));

        let result =
            resolve_provider_config(Some(&table), ProviderKind::OpenRouter, "test-profile")
                .unwrap();

        match result {
            ProviderProfileConfig::OpenRouter(config) => {
                assert_eq!(config.routing.unwrap().order, Some(vec!["xai".to_string()]));
            }
            _ => panic!("Expected OpenRouter config"),
        }
    }

    #[test]
    fn reject_openrouter_config_on_openai_provider() {
        let mut table = TomlTable::new();
        let mut routing = TomlTable::new();
        routing.insert("order".into(), toml::Value::Array(vec!["xai".into()]));
        table.insert("routing".into(), toml::Value::Table(routing));

        let result = resolve_provider_config(Some(&table), ProviderKind::OpenAi, "test-profile");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid OpenAI config"));
    }

    #[test]
    fn empty_provider_table_returns_none() {
        let table = TomlTable::new();
        let result =
            resolve_provider_config(Some(&table), ProviderKind::OpenRouter, "test-profile")
                .unwrap();

        assert!(matches!(result, ProviderProfileConfig::None));
    }

    #[test]
    fn no_provider_table_returns_none() {
        let result =
            resolve_provider_config(None, ProviderKind::OpenRouter, "test-profile").unwrap();

        assert!(matches!(result, ProviderProfileConfig::None));
    }

    #[test]
    fn antigravity_rejects_provider_config() {
        let mut table = TomlTable::new();
        table.insert("some_field".into(), toml::Value::Boolean(true));

        let result =
            resolve_provider_config(Some(&table), ProviderKind::Antigravity, "test-profile");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Antigravity doesn't support"));
    }
}
