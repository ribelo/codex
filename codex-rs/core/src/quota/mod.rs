//! Provider-agnostic quota fetching abstraction.
//!
//! This module defines the [`ProviderQuotaClient`] trait for fetching quota/rate-limit
//! information from various providers (ChatGPT, Antigravity, etc.) and implementations
//! for each supported provider.

mod antigravity_client;
mod types;

pub use antigravity_client::AntigravityQuotaClient;
pub use types::AntigravityUserStatus;
pub use types::QuotaError;

use async_trait::async_trait;
use codex_protocol::protocol::RateLimitSnapshot;

/// Trait for fetching quota/rate-limit info from a provider.
///
/// Implementations should be async, non-blocking, and return `None` on any failure
/// rather than panicking.
#[async_trait]
pub trait ProviderQuotaClient: Send + Sync {
    /// Fetch current quota snapshot from the provider.
    ///
    /// Returns `None` if the quota information is unavailable (e.g., provider not
    /// running, network error, auth failure).
    async fn fetch_quota(&self) -> Option<RateLimitSnapshot>;

    /// Human-readable provider name for error messages and logging.
    fn provider_name(&self) -> &'static str;
}
