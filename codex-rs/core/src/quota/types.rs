//! Types for provider quota responses.

use codex_protocol::protocol::AntigravityQuota;
use codex_protocol::protocol::ModelQuota;
use codex_protocol::protocol::RateLimitSnapshot;
use serde::Deserialize;
use std::fmt;

/// Errors that can occur during quota fetching.
#[derive(Debug)]
pub enum QuotaError {
    /// Language server process not found.
    ProcessNotFound,
    /// CSRF token not found in process command line.
    CsrfTokenNotFound,
    /// No listening ports found for the language server.
    NoPortsFound,
    /// All ports failed to respond.
    ConnectionFailed,
    /// HTTP request failed.
    RequestFailed(String),
    /// Invalid JSON response from the server.
    InvalidResponse(String),
    /// Command execution failed.
    CommandFailed(String),
}

impl fmt::Display for QuotaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuotaError::ProcessNotFound => {
                write!(f, "Antigravity language server process not found")
            }
            QuotaError::CsrfTokenNotFound => {
                write!(f, "CSRF token not found in language server command line")
            }
            QuotaError::NoPortsFound => {
                write!(f, "No listening ports found for language server")
            }
            QuotaError::ConnectionFailed => {
                write!(f, "Failed to connect to any language server port")
            }
            QuotaError::RequestFailed(msg) => write!(f, "HTTP request failed: {msg}"),
            QuotaError::InvalidResponse(msg) => write!(f, "Invalid response: {msg}"),
            QuotaError::CommandFailed(msg) => write!(f, "Command failed: {msg}"),
        }
    }
}

impl std::error::Error for QuotaError {}

// ============================================================================
// Antigravity API Response Types
// ============================================================================

/// Top-level response from Antigravity's GetUserStatus endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityUserStatus {
    pub user_status: UserStatus,
}

/// User status information.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserStatus {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    pub plan_status: Option<PlanStatus>,
    pub cascade_model_config_data: Option<CascadeModelConfigData>,
    pub user_tier: Option<UserTier>,
}

/// User tier information.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTier {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// Plan status with credit information.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStatus {
    pub plan_info: Option<PlanInfo>,
    #[serde(default)]
    pub available_prompt_credits: i64,
    #[serde(default)]
    pub available_flow_credits: i64,
}

/// Plan information including monthly credit limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanInfo {
    #[serde(default)]
    pub teams_tier: String,
    #[serde(default)]
    pub plan_name: String,
    #[serde(default)]
    pub monthly_prompt_credits: i64,
    #[serde(default)]
    pub monthly_flow_credits: i64,
}

/// Model configuration data including per-model quotas.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CascadeModelConfigData {
    #[serde(default)]
    pub client_model_configs: Vec<ClientModelConfig>,
}

/// Individual model configuration with quota info.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientModelConfig {
    #[serde(default)]
    pub label: String,
    pub model_or_alias: Option<ModelOrAlias>,
    pub quota_info: Option<QuotaInfo>,
}

/// Model identifier.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelOrAlias {
    #[serde(default)]
    pub model: String,
}

/// Per-model quota information.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaInfo {
    /// Fraction of quota remaining (0.0-1.0).
    pub remaining_fraction: Option<f64>,
    /// ISO 8601 timestamp when quota resets.
    #[serde(default)]
    pub reset_time: String,
}

// ============================================================================
// Conversion to RateLimitSnapshot
// ============================================================================

impl AntigravityUserStatus {
    /// Convert Antigravity user status to a `RateLimitSnapshot`.
    ///
    /// Maps prompt credits to the credits field and the most restrictive
    /// per-model quota to the primary rate limit window.
    pub fn to_rate_limit_snapshot(&self) -> RateLimitSnapshot {
        // Build full AntigravityQuota with all data
        let antigravity = self.build_antigravity_quota();

        // Antigravity data goes only in the antigravity field.
        // primary/secondary/credits are reserved for OpenAI/ChatGPT data.
        RateLimitSnapshot {
            primary: None,
            secondary: None,
            credits: None,
            plan_type: None,
            antigravity,
            gemini: None,
        }
    }

    /// Build the full AntigravityQuota struct with all available data.
    fn build_antigravity_quota(&self) -> Option<AntigravityQuota> {
        let user_status = &self.user_status;

        // Extract plan info
        let plan_status = user_status.plan_status.as_ref();
        let plan_info = plan_status.and_then(|ps| ps.plan_info.as_ref());

        // Build model quotas
        let model_quotas: Vec<ModelQuota> = user_status
            .cascade_model_config_data
            .as_ref()
            .map(|config_data| {
                config_data
                    .client_model_configs
                    .iter()
                    .filter_map(|config| {
                        let quota_info = config.quota_info.as_ref()?;
                        let remaining_fraction = quota_info.remaining_fraction?;
                        let resets_at = parse_iso_timestamp(&quota_info.reset_time);
                        let model_id = config
                            .model_or_alias
                            .as_ref()
                            .map(|m| m.model.clone())
                            .unwrap_or_default();

                        Some(ModelQuota {
                            label: config.label.clone(),
                            model_id,
                            remaining_fraction,
                            resets_at,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Some(AntigravityQuota {
            user_name: if user_status.name.is_empty() {
                None
            } else {
                Some(user_status.name.clone())
            },
            email: if user_status.email.is_empty() {
                None
            } else {
                Some(user_status.email.clone())
            },
            plan_name: plan_info
                .map(|pi| pi.plan_name.clone())
                .filter(|s| !s.is_empty()),
            user_tier: user_status
                .user_tier
                .as_ref()
                .map(|ut| ut.name.clone())
                .filter(|s| !s.is_empty()),
            available_prompt_credits: plan_status
                .map(|ps| ps.available_prompt_credits)
                .unwrap_or(0),
            monthly_prompt_credits: plan_info.map(|pi| pi.monthly_prompt_credits).unwrap_or(0),
            available_flow_credits: plan_status.map(|ps| ps.available_flow_credits).unwrap_or(0),
            monthly_flow_credits: plan_info.map(|pi| pi.monthly_flow_credits).unwrap_or(0),
            model_quotas,
        })
    }
}

/// Parse an ISO 8601 timestamp to Unix timestamp (seconds since epoch).
fn parse_iso_timestamp(iso: &str) -> Option<i64> {
    if iso.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_antigravity_response() {
        let json = r#"{
            "userStatus": {
                "name": "Test User",
                "email": "test@example.com",
                "planStatus": {
                    "planInfo": {
                        "teamsTier": "free",
                        "planName": "Free",
                        "monthlyPromptCredits": 1000,
                        "monthlyFlowCredits": 500
                    },
                    "availablePromptCredits": 750,
                    "availableFlowCredits": 400
                },
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "label": "Claude 3.5 Sonnet",
                            "modelOrAlias": { "model": "claude-3.5-sonnet" },
                            "quotaInfo": {
                                "remainingFraction": 0.8,
                                "resetTime": "2025-01-15T00:00:00Z"
                            }
                        },
                        {
                            "label": "GPT-4",
                            "modelOrAlias": { "model": "gpt-4" },
                            "quotaInfo": {
                                "remainingFraction": 0.3,
                                "resetTime": "2025-01-15T00:00:00Z"
                            }
                        }
                    ]
                },
                "userTier": {
                    "id": "test-tier",
                    "name": "Test Tier"
                }
            }
        }"#;

        let response: AntigravityUserStatus = serde_json::from_str(json).unwrap();
        let snapshot = response.to_rate_limit_snapshot();

        // primary/secondary/credits are reserved for OpenAI, not populated by Antigravity
        assert!(snapshot.primary.is_none());
        assert!(snapshot.secondary.is_none());
        assert!(snapshot.credits.is_none());

        // Check antigravity data
        assert!(snapshot.antigravity.is_some());
        let antigravity = snapshot.antigravity.unwrap();
        assert_eq!(antigravity.plan_name, Some("Free".to_string()));
        assert_eq!(antigravity.user_tier, Some("Test Tier".to_string()));
        assert_eq!(antigravity.available_prompt_credits, 750);
        assert_eq!(antigravity.monthly_prompt_credits, 1000);
        assert_eq!(antigravity.available_flow_credits, 400);
        assert_eq!(antigravity.monthly_flow_credits, 500);
        assert_eq!(antigravity.model_quotas.len(), 2);
        assert_eq!(antigravity.model_quotas[0].label, "Claude 3.5 Sonnet");
        assert!((antigravity.model_quotas[0].remaining_fraction - 0.8).abs() < 0.01);
    }

    #[test]
    fn test_parse_minimal_response() {
        let json = r#"{"userStatus": {"name": "", "email": ""}}"#;
        let response: AntigravityUserStatus = serde_json::from_str(json).unwrap();
        let snapshot = response.to_rate_limit_snapshot();

        assert!(snapshot.primary.is_none());
        assert!(snapshot.secondary.is_none());
        assert!(snapshot.credits.is_none());
    }

    #[test]
    fn test_parse_iso_timestamp() {
        assert_eq!(
            parse_iso_timestamp("2025-01-15T00:00:00Z"),
            Some(1736899200)
        );
        assert_eq!(parse_iso_timestamp(""), None);
        assert_eq!(parse_iso_timestamp("invalid"), None);
    }
}
