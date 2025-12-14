//! OpenRouter-specific types for provider routing configuration.
//!
//! These types model the `provider` field in OpenRouter's Responses API.
//! Reference: https://openrouter.ai/docs/api/api-reference/responses/create-responses#request.body.provider

use serde::Deserialize;
use serde::Serialize;

/// OpenRouter provider routing configuration.
/// Controls which providers serve requests and under what conditions.
///
/// Set via `[profiles.<name>.provider.routing]` in config.toml.
///
/// # Example
///
/// ```toml
/// [profiles.my-profile.provider]
/// routing.order = ["xai", "openai"]
/// routing.allow_fallbacks = false
/// routing.data_collection = "deny"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterRouting {
    /// Ordered list of provider names to try. Empty = use OpenRouter's default ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,

    /// If true, allow fallback to other providers when preferred ones fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,

    /// If true, only route to providers that support all request parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,

    /// Data collection policy. Controls whether providers can use request data for training.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollectionPolicy>,

    /// Zero Data Retention: if true, route only to providers with ZDR agreements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,

    /// If true, only route to providers that guarantee distillable output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enforce_distillable_text: Option<bool>,

    /// Only use these specific providers (exclusive list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,

    /// Never use these providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,

    /// Allowed quantization levels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Vec<String>>,

    /// Sort preference for provider ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<ProviderSort>,

    /// Maximum price per million tokens (in USD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_price: Option<MaxPrice>,
}

/// Data collection policy for OpenRouter requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataCollectionPolicy {
    Allow,
    Deny,
}

/// Sort preference for OpenRouter provider ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSort {
    /// Sort by price (cheapest first)
    Price,
    /// Sort by throughput (fastest first)
    Throughput,
    /// Sort by latency (lowest first)
    Latency,
}

/// Maximum price constraints for OpenRouter requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaxPrice {
    /// Max price for prompt tokens (per million)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<f64>,
    /// Max price for completion tokens (per million)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<f64>,
    /// Max price for request (flat rate)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<f64>,
    /// Max price for image processing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<f64>,
}

/// OpenRouter-specific profile configuration.
/// Parsed from `[profiles.<name>.provider]` when provider_kind is OpenRouter.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterProfileConfig {
    /// Routing preferences for this profile.
    #[serde(default)]
    pub routing: Option<OpenRouterRouting>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_openrouter_routing_minimal() {
        let toml = r#"
routing.order = ["xai"]
        "#;
        let config: OpenRouterProfileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.routing.unwrap().order, Some(vec!["xai".to_string()]));
    }

    #[test]
    fn parse_openrouter_routing_full() {
        let toml = r#"
[routing]
order = ["anthropic", "openai"]
allow_fallbacks = false
require_parameters = true
data_collection = "deny"
zdr = true
        "#;
        let config: OpenRouterProfileConfig = toml::from_str(toml).unwrap();
        let routing = config.routing.unwrap();
        assert_eq!(routing.allow_fallbacks, Some(false));
        assert_eq!(routing.data_collection, Some(DataCollectionPolicy::Deny));
        assert_eq!(routing.zdr, Some(true));
    }

    #[test]
    fn reject_unknown_fields() {
        let toml = r#"
routing.unknown_field = true
        "#;
        let result: Result<OpenRouterProfileConfig, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn serialize_for_request_body() {
        let routing = OpenRouterRouting {
            order: Some(vec!["xai".to_string()]),
            allow_fallbacks: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_value(&routing).unwrap();
        assert_eq!(json["order"], serde_json::json!(["xai"]));
        assert_eq!(json["allow_fallbacks"], serde_json::json!(false));
        // Verify None fields are skipped
        assert!(json.get("data_collection").is_none());
    }

    #[test]
    fn parse_max_price() {
        let toml = r#"
[routing.max_price]
prompt = 0.5
completion = 1.0
        "#;
        let config: OpenRouterProfileConfig = toml::from_str(toml).unwrap();
        let max_price = config.routing.unwrap().max_price.unwrap();
        assert_eq!(max_price.prompt, Some(0.5));
        assert_eq!(max_price.completion, Some(1.0));
        assert_eq!(max_price.request, None);
    }

    #[test]
    fn parse_provider_sort() {
        let toml = r#"
routing.sort = "price"
        "#;
        let config: OpenRouterProfileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.routing.unwrap().sort, Some(ProviderSort::Price));
    }
}
