//! Gemini quota client implementation.
//!
//! Fetches quota information from Google's CodeAssist API using OAuth credentials.

use super::ProviderQuotaClient;
use super::types::QuotaError;
use async_trait::async_trait;
use codex_protocol::protocol::GeminiBucket;
use codex_protocol::protocol::GeminiQuota;
use codex_protocol::protocol::RateLimitSnapshot;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

use crate::CodexAuth;
use crate::gemini::GEMINI_CODE_ASSIST_CLIENT_METADATA;
use crate::gemini::GEMINI_CODE_ASSIST_ENDPOINT;
use crate::gemini::GEMINI_CODE_ASSIST_USER_AGENT;
use crate::gemini::GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT;

/// Client for fetching quota information from Google's CodeAssist API.
pub struct GeminiQuotaClient {
    http_client: Client,
    auth: Arc<CodexAuth>,
    timeout: Duration,
}

impl GeminiQuotaClient {
    /// Create a new Gemini quota client.
    pub fn new(auth: Arc<CodexAuth>) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http_client,
            auth,
            timeout: Duration::from_secs(10),
        }
    }

    /// Fetch quota from the CodeAssist API.
    async fn fetch_quota_internal(&self) -> Result<GeminiQuotaResponse, QuotaError> {
        // Get valid Gemini tokens and project ID
        let (tokens, project_id) = self
            .auth
            .gemini_oauth_context_for_account(0)
            .await
            .map_err(|e| {
                QuotaError::RequestFailed(format!("Failed to get Gemini credentials: {e}"))
            })?;

        let url = format!("{GEMINI_CODE_ASSIST_ENDPOINT}/v1internal:retrieveUserQuota");

        let request_body = RetrieveUserQuotaRequest {
            project: project_id.clone(),
            user_agent: None,
        };

        debug!("Fetching Gemini quota for project: {project_id}");

        let response = self
            .http_client
            .post(&url)
            .timeout(self.timeout)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", tokens.access_token),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, GEMINI_CODE_ASSIST_USER_AGENT)
            .header("X-Goog-Api-Client", GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT)
            .header("Client-Metadata", GEMINI_CODE_ASSIST_CLIENT_METADATA)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| QuotaError::RequestFailed(format!("HTTP request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(QuotaError::RequestFailed(format!(
                "API returned {status}: {body}"
            )));
        }

        let quota_response: GeminiQuotaResponse = response
            .json()
            .await
            .map_err(|e| QuotaError::InvalidResponse(format!("Failed to parse response: {e}")))?;

        debug!("Received Gemini quota: {:?}", quota_response);

        Ok(quota_response)
    }
}

#[async_trait]
impl ProviderQuotaClient for GeminiQuotaClient {
    async fn fetch_quota(&self) -> Option<RateLimitSnapshot> {
        match self.fetch_quota_internal().await {
            Ok(response) => {
                let snapshot = response.to_rate_limit_snapshot();
                Some(snapshot)
            }
            Err(e) => {
                debug!("Failed to fetch Gemini quota: {e}");
                None
            }
        }
    }

    fn provider_name(&self) -> &'static str {
        "Gemini"
    }
}

// ============================================================================
// API Request/Response Types
// ============================================================================

#[derive(Debug, Serialize)]
struct RetrieveUserQuotaRequest {
    project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_agent: Option<String>,
}

/// Response from the retrieveUserQuota API endpoint.
#[derive(Debug, Deserialize)]
pub struct GeminiQuotaResponse {
    #[serde(default)]
    pub buckets: Vec<BucketInfo>,
}

/// A single quota bucket from the API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BucketInfo {
    pub remaining_amount: Option<String>,
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
    pub token_type: Option<String>,
    pub model_id: Option<String>,
}

impl GeminiQuotaResponse {
    /// Convert API response to RateLimitSnapshot.
    pub fn to_rate_limit_snapshot(&self) -> RateLimitSnapshot {
        let buckets: Vec<GeminiBucket> = self
            .buckets
            .iter()
            .map(|bucket| GeminiBucket {
                model_id: bucket.model_id.clone(),
                token_type: bucket.token_type.clone(),
                remaining_amount: bucket.remaining_amount.clone(),
                remaining_fraction: bucket.remaining_fraction,
                reset_time: parse_iso_timestamp(bucket.reset_time.as_deref()),
            })
            .collect();

        let gemini = if buckets.is_empty() {
            None
        } else {
            Some(GeminiQuota {
                project_id: None, // We could pass this through if needed
                buckets,
            })
        };

        RateLimitSnapshot {
            primary: None,
            secondary: None,
            credits: None,
            plan_type: None,
            antigravity: None,
            gemini,
        }
    }
}

/// Parse an ISO 8601 timestamp to Unix timestamp (seconds since epoch).
fn parse_iso_timestamp(iso: Option<&str>) -> Option<i64> {
    let iso = iso?;
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
    fn test_parse_gemini_quota_response() {
        let json = r#"{
            "buckets": [
                {
                    "modelId": "gemini-2.5-pro",
                    "tokenType": "input",
                    "remainingAmount": "1000000",
                    "remainingFraction": 0.8,
                    "resetTime": "2025-01-15T00:00:00Z"
                },
                {
                    "modelId": "gemini-2.5-pro",
                    "tokenType": "output",
                    "remainingAmount": "500000",
                    "remainingFraction": 0.5,
                    "resetTime": "2025-01-15T00:00:00Z"
                }
            ]
        }"#;

        let response: GeminiQuotaResponse = serde_json::from_str(json).unwrap();
        let snapshot = response.to_rate_limit_snapshot();

        assert!(snapshot.gemini.is_some());
        let gemini = snapshot.gemini.unwrap();
        assert_eq!(gemini.buckets.len(), 2);

        let first = &gemini.buckets[0];
        assert_eq!(first.model_id, Some("gemini-2.5-pro".to_string()));
        assert_eq!(first.token_type, Some("input".to_string()));
        assert_eq!(first.remaining_amount, Some("1000000".to_string()));
        assert!((first.remaining_fraction.unwrap() - 0.8).abs() < 0.01);
        assert_eq!(first.reset_time, Some(1736899200));
    }

    #[test]
    fn test_parse_empty_buckets() {
        let json = r#"{"buckets": []}"#;
        let response: GeminiQuotaResponse = serde_json::from_str(json).unwrap();
        let snapshot = response.to_rate_limit_snapshot();

        assert!(snapshot.gemini.is_none());
    }

    #[test]
    fn test_parse_missing_fields() {
        let json = r#"{
            "buckets": [
                {
                    "modelId": "gemini-2.5-flash"
                }
            ]
        }"#;

        let response: GeminiQuotaResponse = serde_json::from_str(json).unwrap();
        let snapshot = response.to_rate_limit_snapshot();

        assert!(snapshot.gemini.is_some());
        let gemini = snapshot.gemini.unwrap();
        assert_eq!(gemini.buckets.len(), 1);
        assert_eq!(
            gemini.buckets[0].model_id,
            Some("gemini-2.5-flash".to_string())
        );
        assert!(gemini.buckets[0].remaining_fraction.is_none());
        assert!(gemini.buckets[0].reset_time.is_none());
    }

    #[test]
    fn test_parse_iso_timestamp() {
        assert_eq!(
            parse_iso_timestamp(Some("2025-01-15T00:00:00Z")),
            Some(1736899200)
        );
        assert_eq!(parse_iso_timestamp(Some("")), None);
        assert_eq!(parse_iso_timestamp(None), None);
        assert_eq!(parse_iso_timestamp(Some("invalid")), None);
    }
}
