mod storage;

use chrono::Utc;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
#[cfg(test)]
use serial_test::serial;
use std::env;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_app_server_protocol::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;

use crate::antigravity::ANTIGRAVITY_CLIENT_ID;
use crate::antigravity::ANTIGRAVITY_CLIENT_SECRET;
use crate::antigravity::ANTIGRAVITY_TOKEN_URL;
pub use crate::auth::storage::AuthCredentialsStoreMode;
pub use crate::auth::storage::AuthDotJson;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::create_auth_storage;
use crate::config::Config;
use crate::error::RefreshTokenFailedError;
use crate::error::RefreshTokenFailedReason;
use crate::gemini::GEMINI_CLIENT_ID;
use crate::gemini::GEMINI_CLIENT_SECRET;
use crate::gemini::GEMINI_CODE_ASSIST_CLIENT_METADATA;
use crate::gemini::GEMINI_CODE_ASSIST_ENDPOINT;
use crate::gemini::GEMINI_CODE_ASSIST_USER_AGENT;
use crate::gemini::GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT;
use crate::gemini::GEMINI_METADATA_IDE_TYPE;
use crate::gemini::GEMINI_METADATA_PLATFORM;
use crate::gemini::GEMINI_METADATA_PLUGIN;
use crate::gemini::GEMINI_TOKEN_URL;
use crate::token_data::GeminiTokenData;
use crate::token_data::KnownPlan as InternalKnownPlan;
use crate::token_data::PlanType as InternalPlanType;
use crate::token_data::TokenData;
use crate::token_data::parse_id_token;
use crate::util::try_parse_error_message;
use codex_client::CodexHttpClient;
use codex_protocol::account::PlanType as AccountPlanType;
use once_cell::sync::Lazy;
use serde_json::Value;
use tempfile::TempDir;
use thiserror::Error;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct CodexAuth {
    pub mode: AuthMode,

    pub(crate) api_key: Option<String>,
    pub(crate) auth_dot_json: Arc<Mutex<Option<AuthDotJson>>>,
    storage: Arc<dyn AuthStorageBackend>,
    pub(crate) client: CodexHttpClient,
}

impl PartialEq for CodexAuth {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
    }
}

// TODO(pakrym): use token exp field to check for expiration instead
const TOKEN_REFRESH_INTERVAL: i64 = 8;

const REFRESH_TOKEN_EXPIRED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.";
const REFRESH_TOKEN_REUSED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.";
const REFRESH_TOKEN_INVALIDATED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.";
const REFRESH_TOKEN_UNKNOWN_MESSAGE: &str =
    "Your access token could not be refreshed. Please log out and sign in again.";
const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
const GEMINI_PROJECT_REQUIRED_MESSAGE: &str = "Gemini requires a Google Cloud project. Run `codex login --gemini` and supply a project ID with the Gemini API enabled.";
const ANTIGRAVITY_PROJECT_REQUIRED_MESSAGE: &str = "Antigravity requires a Google Cloud project. Run `codex login --antigravity` and supply a project ID with the Gemini API enabled.";
const GEMINI_ACCESS_TOKEN_LEEWAY_SECONDS: i64 = 300;
const GEMINI_ONBOARD_MAX_ATTEMPTS: usize = 10;
const GEMINI_ONBOARD_DELAY_MILLIS: u64 = 500;

#[allow(dead_code)]
static TEST_AUTH_TEMP_DIRS: Lazy<Mutex<Vec<TempDir>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Debug, Error)]
pub enum RefreshTokenError {
    #[error("{0}")]
    Permanent(#[from] RefreshTokenFailedError),
    #[error(transparent)]
    Transient(#[from] std::io::Error),
}

impl RefreshTokenError {
    pub fn failed_reason(&self) -> Option<RefreshTokenFailedReason> {
        match self {
            Self::Permanent(error) => Some(error.reason),
            Self::Transient(_) => None,
        }
    }

    fn other_with_message(message: impl Into<String>) -> Self {
        Self::Transient(std::io::Error::other(message.into()))
    }
}

impl From<RefreshTokenError> for std::io::Error {
    fn from(err: RefreshTokenError) -> Self {
        match err {
            RefreshTokenError::Permanent(failed) => std::io::Error::other(failed),
            RefreshTokenError::Transient(inner) => inner,
        }
    }
}

impl CodexAuth {
    pub async fn refresh_token(&self) -> Result<String, RefreshTokenError> {
        tracing::info!("Refreshing token");

        if self.mode == AuthMode::Gemini {
            let (tokens, _) = self
                .gemini_oauth_context_for_account(0)
                .await
                .map_err(RefreshTokenError::from)?;
            return Ok(tokens.access_token);
        }

        if self.mode == AuthMode::Antigravity {
            let (tokens, _) = self
                .antigravity_oauth_context_for_account(0)
                .await
                .map_err(RefreshTokenError::from)?;
            return Ok(tokens.access_token);
        }

        let token_data = self.get_current_token_data().ok_or_else(|| {
            RefreshTokenError::Transient(std::io::Error::other("Token data is not available."))
        })?;
        let token = token_data.refresh_token;

        let refresh_response = try_refresh_token(token, &self.client).await?;

        let updated = update_tokens(
            &self.storage,
            refresh_response.id_token,
            refresh_response.access_token,
            refresh_response.refresh_token,
        )
        .await
        .map_err(RefreshTokenError::from)?;

        if let Ok(mut auth_lock) = self.auth_dot_json.lock() {
            *auth_lock = Some(updated.clone());
        }

        let access = match updated.tokens {
            Some(t) => t.access_token,
            None => {
                return Err(RefreshTokenError::other_with_message(
                    "Token data is not available after refresh.",
                ));
            }
        };
        Ok(access)
    }

    /// Loads the available auth information from auth storage.
    pub fn from_auth_storage(
        codex_home: &Path,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
    ) -> std::io::Result<Option<CodexAuth>> {
        load_auth(codex_home, false, auth_credentials_store_mode)
    }

    pub async fn get_token_data(&self) -> Result<TokenData, std::io::Error> {
        let auth_dot_json: Option<AuthDotJson> = self.get_current_auth_json();
        match auth_dot_json {
            Some(AuthDotJson {
                tokens: Some(mut tokens),
                last_refresh: Some(last_refresh),
                ..
            }) => {
                if last_refresh < Utc::now() - chrono::Duration::days(TOKEN_REFRESH_INTERVAL) {
                    let refresh_result = tokio::time::timeout(
                        Duration::from_secs(60),
                        try_refresh_token(tokens.refresh_token.clone(), &self.client),
                    )
                    .await;
                    let refresh_response = match refresh_result {
                        Ok(Ok(response)) => response,
                        Ok(Err(err)) => return Err(err.into()),
                        Err(_) => {
                            return Err(std::io::Error::new(
                                ErrorKind::TimedOut,
                                "timed out while refreshing OpenAI API key",
                            ));
                        }
                    };

                    let updated_auth_dot_json = update_tokens(
                        &self.storage,
                        refresh_response.id_token,
                        refresh_response.access_token,
                        refresh_response.refresh_token,
                    )
                    .await?;

                    tokens = updated_auth_dot_json
                        .tokens
                        .clone()
                        .ok_or(std::io::Error::other(
                            "Token data is not available after refresh.",
                        ))?;

                    #[expect(clippy::unwrap_used)]
                    let mut auth_lock = self.auth_dot_json.lock().unwrap();
                    *auth_lock = Some(updated_auth_dot_json);
                }

                Ok(tokens)
            }
            _ => Err(std::io::Error::other("Token data is not available.")),
        }
    }

    pub async fn get_token(&self) -> Result<String, std::io::Error> {
        match self.mode {
            AuthMode::ApiKey => Ok(self.api_key.clone().unwrap_or_default()),
            AuthMode::ChatGPT => {
                let id_token = self.get_token_data().await?.access_token;
                Ok(id_token)
            }
            AuthMode::Gemini => Err(std::io::Error::other(
                "Gemini tokens cannot be used as ChatGPT access tokens",
            )),
            AuthMode::Antigravity => Err(std::io::Error::other(
                "Antigravity tokens cannot be used as ChatGPT access tokens",
            )),
        }
    }

    pub fn get_account_id(&self) -> Option<String> {
        self.get_current_token_data().and_then(|t| t.account_id)
    }

    pub fn get_account_email(&self) -> Option<String> {
        self.get_current_token_data().and_then(|t| t.id_token.email)
    }

    pub fn get_gemini_tokens(&self) -> Option<GeminiTokenData> {
        self.get_all_gemini_tokens().first().cloned()
    }

    pub fn get_all_gemini_tokens(&self) -> Vec<GeminiTokenData> {
        self.get_current_auth_json()
            .map(|auth| auth.gemini_accounts)
            .unwrap_or_default()
    }

    pub fn gemini_account_count(&self) -> usize {
        self.get_all_gemini_tokens().len()
    }

    pub fn gemini_account_emails(&self) -> Vec<String> {
        self.get_all_gemini_tokens()
            .into_iter()
            .filter_map(|token| token.email)
            .collect()
    }

    pub fn get_antigravity_tokens(&self) -> Option<GeminiTokenData> {
        self.get_all_antigravity_tokens().first().cloned()
    }

    pub fn get_all_antigravity_tokens(&self) -> Vec<GeminiTokenData> {
        self.get_current_auth_json()
            .map(|auth| auth.antigravity_accounts)
            .unwrap_or_default()
    }

    pub fn antigravity_account_count(&self) -> usize {
        self.get_all_antigravity_tokens().len()
    }

    pub fn antigravity_account_emails(&self) -> Vec<String> {
        self.get_all_antigravity_tokens()
            .into_iter()
            .filter_map(|token| token.email)
            .collect()
    }

    pub(crate) async fn gemini_oauth_context_for_account(
        &self,
        index: usize,
    ) -> std::io::Result<(GeminiTokenData, String)> {
        let tokens = self.ensure_valid_gemini_tokens_for(index).await?;
        let project_id = self.ensure_gemini_project_id_for(index, &tokens).await?;
        Ok((tokens, project_id))
    }

    pub(crate) async fn antigravity_oauth_context_for_account(
        &self,
        index: usize,
    ) -> std::io::Result<(GeminiTokenData, String)> {
        let tokens = self.ensure_valid_antigravity_tokens_for(index).await?;
        let project_id = self
            .ensure_antigravity_project_id_for(index, &tokens)
            .await?;
        Ok((tokens, project_id))
    }

    /// Account-facing plan classification derived from the current token.
    /// Returns a high-level `AccountPlanType` (e.g., Free/Plus/Pro/Team/…)
    /// mapped from the ID token's internal plan value. Prefer this when you
    /// need to make UI or product decisions based on the user's subscription.
    pub fn account_plan_type(&self) -> Option<AccountPlanType> {
        let map_known = |kp: &InternalKnownPlan| match kp {
            InternalKnownPlan::Free => AccountPlanType::Free,
            InternalKnownPlan::Plus => AccountPlanType::Plus,
            InternalKnownPlan::Pro => AccountPlanType::Pro,
            InternalKnownPlan::Team => AccountPlanType::Team,
            InternalKnownPlan::Business => AccountPlanType::Business,
            InternalKnownPlan::Enterprise => AccountPlanType::Enterprise,
            InternalKnownPlan::Edu => AccountPlanType::Edu,
        };

        self.get_current_token_data()
            .and_then(|t| t.id_token.chatgpt_plan_type)
            .map(|pt| match pt {
                InternalPlanType::Known(k) => map_known(&k),
                InternalPlanType::Unknown(_) => AccountPlanType::Unknown,
            })
    }

    fn get_current_auth_json(&self) -> Option<AuthDotJson> {
        #[expect(clippy::unwrap_used)]
        self.auth_dot_json.lock().unwrap().clone()
    }

    fn save_auth_snapshot(&self, auth: &AuthDotJson) {
        if let Ok(mut guard) = self.auth_dot_json.lock() {
            *guard = Some(auth.clone());
        }
    }

    fn persist_auth(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.storage.save(auth)?;
        self.save_auth_snapshot(auth);
        Ok(())
    }

    fn get_current_token_data(&self) -> Option<TokenData> {
        self.get_current_auth_json().and_then(|t| t.tokens)
    }

    async fn ensure_valid_gemini_tokens_for(
        &self,
        index: usize,
    ) -> std::io::Result<GeminiTokenData> {
        let tokens = self.update_gemini_account(index, |gemini| gemini.clone())?;

        let should_refresh = tokens
            .expires_at
            .map(|expires_at| {
                expires_at
                    <= Utc::now() + chrono::Duration::seconds(GEMINI_ACCESS_TOKEN_LEEWAY_SECONDS)
            })
            .unwrap_or(true);

        if !should_refresh {
            return Ok(tokens);
        }

        self.refresh_gemini_access_token_for(index, &tokens.refresh_token)
            .await
    }

    async fn ensure_valid_antigravity_tokens_for(
        &self,
        index: usize,
    ) -> std::io::Result<GeminiTokenData> {
        let tokens = self.update_antigravity_account(index, |antigravity| antigravity.clone())?;

        let should_refresh = tokens
            .expires_at
            .map(|expires_at| {
                expires_at
                    <= Utc::now() + chrono::Duration::seconds(GEMINI_ACCESS_TOKEN_LEEWAY_SECONDS)
            })
            .unwrap_or(true);

        if !should_refresh {
            return Ok(tokens);
        }

        self.refresh_antigravity_access_token_for(index, &tokens.refresh_token)
            .await
    }

    async fn ensure_antigravity_project_id_for(
        &self,
        index: usize,
        tokens: &GeminiTokenData,
    ) -> std::io::Result<String> {
        if let Ok(env_project) = env::var("ANTIGRAVITY_PROJECT_ID") {
            let trimmed = env_project.trim();
            if !trimmed.is_empty() {
                let project = trimmed.to_string();
                self.update_antigravity_account(index, |account| {
                    account.project_id = Some(project.clone());
                })?;
                return Ok(project);
            }
        }

        let access_token = tokens.access_token.clone();
        let project_hint = tokens.project_id.clone();

        // First try to resolve from stored project_id or managed_project_id
        if let Some(project_id) = Self::resolve_project_id(tokens) {
            return Ok(project_id);
        }

        // Use Antigravity-specific endpoint discovery
        let load_payload =
            load_managed_project_antigravity(&self.client, &access_token, project_hint.as_deref())
                .await?;

        if let Some(payload) = load_payload {
            tracing::debug!(
                cloudaicompanion_project = ?payload.cloudaicompanion_project,
                tiers = ?payload.tiers,
                "Antigravity loadCodeAssist payload"
            );

            if let Some(managed_id) = payload.cloudaicompanion_project {
                let managed_clone = managed_id.clone();
                self.update_antigravity_account(index, |account| {
                    account.managed_project_id = Some(managed_id);
                })?;
                return Ok(managed_clone);
            }

            let tier_id = match select_gemini_tier(&payload) {
                Some(tier) => tier,
                None => {
                    tracing::debug!("No default tier found, falling back to Gemini project");
                    // Fall through to Gemini fallback instead of erroring
                    return self.try_gemini_fallback_for_antigravity(index).await;
                }
            };
            let is_free_tier =
                tier_id.eq_ignore_ascii_case("FREE") || tier_id.eq_ignore_ascii_case("free-tier");
            if !is_free_tier {
                tracing::debug!(
                    tier_id = %tier_id,
                    "Non-free tier without project, falling back to Gemini"
                );
                return self.try_gemini_fallback_for_antigravity(index).await;
            }

            if let Some(managed_id) = onboard_managed_project(
                &self.client,
                &access_token,
                &tier_id,
                project_hint.as_deref(),
            )
            .await?
            {
                let managed_clone = managed_id.clone();
                self.update_antigravity_account(index, |account| {
                    account.managed_project_id = Some(managed_id);
                })?;
                return Ok(managed_clone);
            }

            tracing::debug!("Onboarding failed, falling back to Gemini project");
            return self.try_gemini_fallback_for_antigravity(index).await;
        }

        self.try_gemini_fallback_for_antigravity(index).await
    }

    async fn try_gemini_fallback_for_antigravity(&self, index: usize) -> std::io::Result<String> {
        // Fallback: try to use Gemini managed project if available
        // Antigravity can use the same Google Cloud project as Gemini
        if let Some(gemini_tokens) = self.get_gemini_tokens()
            && let Some(project_id) = Self::resolve_project_id(&gemini_tokens)
        {
            tracing::debug!(
                project_id = %project_id,
                "Using Gemini managed project for Antigravity"
            );
            self.update_antigravity_account(index, |account| {
                account.managed_project_id = Some(project_id.clone());
            })?;
            return Ok(project_id);
        }

        Err(std::io::Error::other(
            ANTIGRAVITY_PROJECT_REQUIRED_MESSAGE.to_string(),
        ))
    }

    async fn ensure_gemini_project_id_for(
        &self,
        index: usize,
        tokens: &GeminiTokenData,
    ) -> std::io::Result<String> {
        if let Some(project_id) = Self::resolve_project_id(tokens) {
            return Ok(project_id);
        }

        let access_token = tokens.access_token.clone();
        let project_hint = tokens.project_id.clone();
        let load_payload =
            load_managed_project(&self.client, &access_token, project_hint.as_deref()).await?;

        if let Some(payload) = load_payload {
            if let Some(managed_id) = payload.cloudaicompanion_project {
                let managed_clone = managed_id.clone();
                self.update_gemini_account(index, |gemini| {
                    gemini.managed_project_id = Some(managed_id);
                })?;
                return Ok(managed_clone);
            }

            let tier_id = select_gemini_tier(&payload)
                .ok_or_else(|| std::io::Error::other(GEMINI_PROJECT_REQUIRED_MESSAGE))?;
            let is_free_tier =
                tier_id.eq_ignore_ascii_case("FREE") || tier_id.eq_ignore_ascii_case("free-tier");
            if !is_free_tier {
                return Err(std::io::Error::other(GEMINI_PROJECT_REQUIRED_MESSAGE));
            }

            if let Some(managed_id) = onboard_managed_project(
                &self.client,
                &access_token,
                &tier_id,
                project_hint.as_deref(),
            )
            .await?
            {
                let managed_clone = managed_id.clone();
                self.update_gemini_account(index, |gemini| {
                    gemini.managed_project_id = Some(managed_id);
                })?;
                return Ok(managed_clone);
            }

            return Err(std::io::Error::other(GEMINI_PROJECT_REQUIRED_MESSAGE));
        }

        Err(std::io::Error::other(GEMINI_PROJECT_REQUIRED_MESSAGE))
    }

    fn resolve_project_id(tokens: &GeminiTokenData) -> Option<String> {
        tokens
            .project_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                tokens
                    .managed_project_id
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
    }

    fn update_antigravity_account<F, R>(&self, index: usize, updater: F) -> std::io::Result<R>
    where
        F: FnOnce(&mut GeminiTokenData) -> R,
    {
        let mut auth_dot_json = self.storage.load()?.ok_or_else(|| {
            std::io::Error::other("auth.json missing while updating Antigravity tokens")
        })?;
        if auth_dot_json.antigravity_accounts.is_empty() {
            return Err(std::io::Error::other("Antigravity tokens not available"));
        }
        if index >= auth_dot_json.antigravity_accounts.len() {
            return Err(std::io::Error::other(format!(
                "Antigravity account index {index} out of range"
            )));
        }
        let result = updater(&mut auth_dot_json.antigravity_accounts[index]);
        self.persist_auth(&auth_dot_json)?;
        Ok(result)
    }

    async fn refresh_gemini_access_token_for(
        &self,
        index: usize,
        refresh_token: &str,
    ) -> std::io::Result<GeminiTokenData> {
        #[derive(Deserialize)]
        struct RefreshResponse {
            access_token: String,
            expires_in: i64,
            refresh_token: Option<String>,
        }

        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
            urlencoding::encode(refresh_token),
            urlencoding::encode(GEMINI_CLIENT_ID),
            urlencoding::encode(GEMINI_CLIENT_SECRET)
        );

        let response = self
            .client
            .post(GEMINI_TOKEN_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(std::io::Error::other)?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            if error_text.contains("invalid_grant") {
                return Err(std::io::Error::other(
                    "Gemini credentials were revoked. Run `codex login --gemini` to reauthenticate.",
                ));
            }
            return Err(std::io::Error::other(format!(
                "Failed to refresh Gemini access token ({status}): {error_text}"
            )));
        }

        let payload: RefreshResponse = response.json().await.map_err(std::io::Error::other)?;
        let expires_at = Utc::now() + chrono::Duration::seconds(payload.expires_in);
        let refresh_override = payload.refresh_token.clone();
        self.update_gemini_account(index, |gemini| {
            gemini.access_token = payload.access_token.clone();
            gemini.expires_at = Some(expires_at);
            if let Some(new_refresh) = refresh_override.clone() {
                gemini.refresh_token = new_refresh;
            }
        })?;

        self.update_gemini_account(index, |gemini| gemini.clone())
    }

    async fn refresh_antigravity_access_token_for(
        &self,
        index: usize,
        refresh_token: &str,
    ) -> std::io::Result<GeminiTokenData> {
        #[derive(Deserialize)]
        struct RefreshResponse {
            access_token: String,
            expires_in: i64,
            refresh_token: Option<String>,
        }

        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
            urlencoding::encode(refresh_token),
            urlencoding::encode(ANTIGRAVITY_CLIENT_ID),
            urlencoding::encode(ANTIGRAVITY_CLIENT_SECRET)
        );

        let response = self
            .client
            .post(ANTIGRAVITY_TOKEN_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(std::io::Error::other)?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            if error_text.contains("invalid_grant") {
                return Err(std::io::Error::other(
                    "Antigravity credentials were revoked. Reauthenticate to continue.",
                ));
            }
            return Err(std::io::Error::other(format!(
                "Failed to refresh Antigravity access token ({status}): {error_text}"
            )));
        }

        let payload: RefreshResponse = response.json().await.map_err(std::io::Error::other)?;
        let expires_at = Utc::now() + chrono::Duration::seconds(payload.expires_in);
        let refresh_override = payload.refresh_token.clone();
        self.update_antigravity_account(index, |account| {
            account.access_token = payload.access_token.clone();
            account.expires_at = Some(expires_at);
            if let Some(new_refresh) = refresh_override.clone() {
                account.refresh_token = new_refresh;
            }
        })?;

        self.update_antigravity_account(index, |account| account.clone())
    }

    fn update_gemini_account<F, R>(&self, index: usize, updater: F) -> std::io::Result<R>
    where
        F: FnOnce(&mut GeminiTokenData) -> R,
    {
        let mut auth_dot_json = self.storage.load()?.ok_or_else(|| {
            std::io::Error::other("auth.json missing while updating Gemini tokens")
        })?;
        if auth_dot_json.gemini_accounts.is_empty() {
            return Err(std::io::Error::other("Gemini tokens not available"));
        }
        if index >= auth_dot_json.gemini_accounts.len() {
            return Err(std::io::Error::other(format!(
                "Gemini account index {index} out of range"
            )));
        }
        let result = updater(&mut auth_dot_json.gemini_accounts[index]);
        self.persist_auth(&auth_dot_json)?;
        Ok(result)
    }

    pub fn remove_gemini_account(&self, index: usize) -> std::io::Result<GeminiTokenData> {
        let mut auth_dot_json = self.storage.load()?.ok_or_else(|| {
            std::io::Error::other("auth.json missing while removing Gemini account")
        })?;
        if index >= auth_dot_json.gemini_accounts.len() {
            return Err(std::io::Error::other(format!(
                "Gemini account index {index} out of range"
            )));
        }
        let removed = auth_dot_json.gemini_accounts.remove(index);
        self.persist_auth(&auth_dot_json)?;
        Ok(removed)
    }

    /// Consider this private to integration tests.
    pub fn create_dummy_chatgpt_auth_for_testing() -> Self {
        let auth_dot_json = AuthDotJson {
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: Default::default(),
                access_token: "Access Token".to_string(),
                refresh_token: "test".to_string(),
                account_id: Some("account_id".to_string()),
            }),
            gemini_accounts: Vec::new(),
            antigravity_accounts: Vec::new(),
            last_refresh: Some(Utc::now()),
        };

        let auth_dot_json = Arc::new(Mutex::new(Some(auth_dot_json)));
        Self {
            api_key: None,
            mode: AuthMode::ChatGPT,
            storage: create_auth_storage(PathBuf::new(), AuthCredentialsStoreMode::File),
            auth_dot_json,
            client: crate::default_client::create_client(),
        }
    }

    fn from_api_key_with_client(api_key: &str, client: CodexHttpClient) -> Self {
        Self {
            api_key: Some(api_key.to_owned()),
            mode: AuthMode::ApiKey,
            storage: create_auth_storage(PathBuf::new(), AuthCredentialsStoreMode::File),
            auth_dot_json: Arc::new(Mutex::new(None)),
            client,
        }
    }

    pub fn from_api_key(api_key: &str) -> Self {
        Self::from_api_key_with_client(api_key, crate::default_client::create_client())
    }
}

pub const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
pub const CODEX_API_KEY_ENV_VAR: &str = "CODEX_API_KEY";

pub fn read_openai_api_key_from_env() -> Option<String> {
    env::var(OPENAI_API_KEY_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn read_codex_api_key_from_env() -> Option<String> {
    env::var(CODEX_API_KEY_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Delete the auth.json file inside `codex_home` if it exists. Returns `Ok(true)`
/// if a file was removed, `Ok(false)` if no auth file was present.
pub fn logout(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<bool> {
    let storage = create_auth_storage(codex_home.to_path_buf(), auth_credentials_store_mode);
    storage.delete()
}

/// Writes an `auth.json` that contains only the API key.
pub fn login_with_api_key(
    codex_home: &Path,
    api_key: &str,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<()> {
    let auth_dot_json = AuthDotJson {
        openai_api_key: Some(api_key.to_string()),
        tokens: None,
        gemini_accounts: Vec::new(),
        antigravity_accounts: Vec::new(),
        last_refresh: None,
    };
    save_auth(codex_home, &auth_dot_json, auth_credentials_store_mode)
}

/// Persist the provided auth payload using the specified backend.
pub fn save_auth(
    codex_home: &Path,
    auth: &AuthDotJson,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<()> {
    let storage = create_auth_storage(codex_home.to_path_buf(), auth_credentials_store_mode);
    storage.save(auth)
}

/// Load CLI auth data using the configured credential store backend.
/// Returns `None` when no credentials are stored. This function is
/// provided only for tests. Production code should not directly load
/// from the auth.json storage. It should use the AuthManager abstraction
/// instead.
pub fn load_auth_dot_json(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<Option<AuthDotJson>> {
    let storage = create_auth_storage(codex_home.to_path_buf(), auth_credentials_store_mode);
    storage.load()
}

pub async fn enforce_login_restrictions(config: &Config) -> std::io::Result<()> {
    let Some(auth) = load_auth(
        &config.codex_home,
        true,
        config.cli_auth_credentials_store_mode,
    )?
    else {
        return Ok(());
    };

    if let Some(required_method) = config.forced_login_method {
        let method_violation = match (required_method, auth.mode) {
            (ForcedLoginMethod::Api, AuthMode::ApiKey) => None,
            (ForcedLoginMethod::Chatgpt, AuthMode::ChatGPT) => None,
            (ForcedLoginMethod::Api, AuthMode::Gemini)
            | (ForcedLoginMethod::Chatgpt, AuthMode::Gemini)
            | (ForcedLoginMethod::Api, AuthMode::Antigravity)
            | (ForcedLoginMethod::Chatgpt, AuthMode::Antigravity) => Some(
                "Gemini or Antigravity login is not permitted by the configured forced login method."
                    .to_string(),
            ),
            (ForcedLoginMethod::Api, AuthMode::ChatGPT) => Some(
                "API key login is required, but ChatGPT is currently being used. Logging out."
                    .to_string(),
            ),
            (ForcedLoginMethod::Chatgpt, AuthMode::ApiKey) => Some(
                "ChatGPT login is required, but an API key is currently being used. Logging out."
                    .to_string(),
            ),
        };

        if let Some(message) = method_violation {
            return logout_with_message(
                &config.codex_home,
                message,
                config.cli_auth_credentials_store_mode,
            );
        }
    }

    if let Some(expected_account_id) = config.forced_chatgpt_workspace_id.as_deref() {
        if auth.mode != AuthMode::ChatGPT {
            return Ok(());
        }

        let token_data = match auth.get_token_data().await {
            Ok(data) => data,
            Err(err) => {
                return logout_with_message(
                    &config.codex_home,
                    format!(
                        "Failed to load ChatGPT credentials while enforcing workspace restrictions: {err}. Logging out."
                    ),
                    config.cli_auth_credentials_store_mode,
                );
            }
        };

        // workspace is the external identifier for account id.
        let chatgpt_account_id = token_data.id_token.chatgpt_account_id.as_deref();
        if chatgpt_account_id != Some(expected_account_id) {
            let message = match chatgpt_account_id {
                Some(actual) => format!(
                    "Login is restricted to workspace {expected_account_id}, but current credentials belong to {actual}. Logging out."
                ),
                None => format!(
                    "Login is restricted to workspace {expected_account_id}, but current credentials lack a workspace identifier. Logging out."
                ),
            };
            return logout_with_message(
                &config.codex_home,
                message,
                config.cli_auth_credentials_store_mode,
            );
        }
    }

    Ok(())
}

fn logout_with_message(
    codex_home: &Path,
    message: String,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<()> {
    match logout(codex_home, auth_credentials_store_mode) {
        Ok(_) => Err(std::io::Error::other(message)),
        Err(err) => Err(std::io::Error::other(format!(
            "{message}. Failed to remove auth.json: {err}"
        ))),
    }
}

fn load_auth(
    codex_home: &Path,
    enable_codex_api_key_env: bool,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<Option<CodexAuth>> {
    if enable_codex_api_key_env && let Some(api_key) = read_codex_api_key_from_env() {
        let client = crate::default_client::create_client();
        return Ok(Some(CodexAuth::from_api_key_with_client(
            api_key.as_str(),
            client,
        )));
    }

    let storage = create_auth_storage(codex_home.to_path_buf(), auth_credentials_store_mode);

    let client = crate::default_client::create_client();
    let auth_dot_json = match storage.load()? {
        Some(auth) => auth,
        None => return Ok(None),
    };

    let AuthDotJson {
        openai_api_key: auth_json_api_key,
        tokens,
        gemini_accounts,
        antigravity_accounts,
        last_refresh,
    } = auth_dot_json;

    let snapshot = AuthDotJson {
        openai_api_key: None,
        tokens,
        gemini_accounts,
        antigravity_accounts,
        last_refresh,
    };

    // Prefer API key if present; else prefer ChatGPT tokens; Gemini-only credentials are a fallback.
    if let Some(api_key) = &auth_json_api_key {
        return Ok(Some(CodexAuth {
            api_key: Some(api_key.to_owned()),
            mode: AuthMode::ApiKey,
            storage: storage.clone(),
            auth_dot_json: Arc::new(Mutex::new(Some(snapshot))),
            client,
        }));
    }
    if snapshot.tokens.is_some() {
        return Ok(Some(CodexAuth {
            api_key: None,
            mode: AuthMode::ChatGPT,
            storage: storage.clone(),
            auth_dot_json: Arc::new(Mutex::new(Some(snapshot))),
            client,
        }));
    }
    if !snapshot.antigravity_accounts.is_empty() {
        return Ok(Some(CodexAuth {
            api_key: None,
            mode: AuthMode::Antigravity,
            storage: storage.clone(),
            auth_dot_json: Arc::new(Mutex::new(Some(snapshot))),
            client,
        }));
    }
    if !snapshot.gemini_accounts.is_empty() {
        return Ok(Some(CodexAuth {
            api_key: None,
            mode: AuthMode::Gemini,
            storage: storage.clone(),
            auth_dot_json: Arc::new(Mutex::new(Some(snapshot))),
            client,
        }));
    }

    Ok(Some(CodexAuth {
        api_key: None,
        mode: AuthMode::ChatGPT,
        storage: storage.clone(),
        auth_dot_json: Arc::new(Mutex::new(Some(snapshot))),
        client,
    }))
}

#[derive(Debug, Deserialize)]
struct GeminiTier {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "isDefault")]
    is_default: Option<bool>,
}

#[derive(Deserialize)]
struct LoadCodeAssistPayload {
    #[serde(default)]
    tiers: Option<Vec<GeminiTier>>,
    #[serde(default)]
    cloudaicompanion_project: Option<String>,
}

#[derive(Deserialize)]
struct OnboardUserPayload {
    #[serde(default)]
    done: Option<bool>,
    #[serde(default)]
    response: Option<OnboardUserResponse>,
}

#[derive(Deserialize)]
struct OnboardUserResponse {
    #[serde(default, rename = "cloudaicompanionProject")]
    cloudaicompanion_project: Option<OnboardUserProject>,
}

#[derive(Deserialize)]
struct OnboardUserProject {
    #[serde(default)]
    id: Option<String>,
}

fn select_gemini_tier(payload: &LoadCodeAssistPayload) -> Option<String> {
    payload
        .tiers
        .as_ref()
        .and_then(|tiers| tiers.iter().find(|tier| tier.is_default.unwrap_or(false)))
        .and_then(|tier| tier.id.clone())
}

fn build_gemini_metadata(project_id: Option<&str>) -> Value {
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "ideType".to_string(),
        Value::String(GEMINI_METADATA_IDE_TYPE.to_string()),
    );
    metadata.insert(
        "platform".to_string(),
        Value::String(GEMINI_METADATA_PLATFORM.to_string()),
    );
    metadata.insert(
        "pluginType".to_string(),
        Value::String(GEMINI_METADATA_PLUGIN.to_string()),
    );
    if let Some(project) = project_id {
        metadata.insert(
            "duetProject".to_string(),
            Value::String(project.to_string()),
        );
    }
    Value::Object(metadata)
}

async fn load_managed_project(
    client: &CodexHttpClient,
    access_token: &str,
    project_id: Option<&str>,
) -> std::io::Result<Option<LoadCodeAssistPayload>> {
    load_managed_project_with_endpoint(
        client,
        access_token,
        project_id,
        GEMINI_CODE_ASSIST_ENDPOINT,
    )
    .await
}

async fn load_managed_project_with_endpoint(
    client: &CodexHttpClient,
    access_token: &str,
    project_id: Option<&str>,
    endpoint: &str,
) -> std::io::Result<Option<LoadCodeAssistPayload>> {
    let mut body = serde_json::Map::new();
    body.insert("metadata".to_string(), build_gemini_metadata(project_id));
    body.insert(
        "cloudaicompanionProject".to_string(),
        project_id
            .map(|project| Value::String(project.to_string()))
            .unwrap_or(Value::Null),
    );

    let url = format!(
        "{}/v1internal:loadCodeAssist",
        endpoint.trim_end_matches('/')
    );
    let response = client
        .post(&url)
        .bearer_auth(access_token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, GEMINI_CODE_ASSIST_USER_AGENT)
        .header("X-Goog-Api-Client", GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT)
        .header("Client-Metadata", GEMINI_CODE_ASSIST_CLIENT_METADATA)
        .json(&body)
        .send()
        .await
        .map_err(std::io::Error::other)?;

    if !response.status().is_success() {
        tracing::debug!(
            endpoint = %endpoint,
            status = %response.status(),
            "loadCodeAssist request failed"
        );
        return Ok(None);
    }

    let payload = response.json().await.map_err(std::io::Error::other)?;
    Ok(Some(payload))
}

/// Try multiple endpoints for Antigravity project discovery.
/// Antigravity may use different endpoints than standard Gemini.
async fn load_managed_project_antigravity(
    client: &CodexHttpClient,
    access_token: &str,
    project_id: Option<&str>,
) -> std::io::Result<Option<LoadCodeAssistPayload>> {
    use crate::antigravity::ANTIGRAVITY_ENDPOINT;

    let endpoints = [
        GEMINI_CODE_ASSIST_ENDPOINT,
        ANTIGRAVITY_ENDPOINT,
        "https://autopush-cloudcode-pa.sandbox.googleapis.com",
    ];

    for endpoint in endpoints {
        if let Some(payload) =
            load_managed_project_with_endpoint(client, access_token, project_id, endpoint).await?
        {
            tracing::debug!(endpoint = %endpoint, "loadCodeAssist succeeded for Antigravity");
            return Ok(Some(payload));
        }
    }

    Ok(None)
}

async fn onboard_managed_project(
    client: &CodexHttpClient,
    access_token: &str,
    tier_id: &str,
    project_id: Option<&str>,
) -> std::io::Result<Option<String>> {
    for _ in 0..GEMINI_ONBOARD_MAX_ATTEMPTS {
        let mut body = serde_json::Map::new();
        body.insert("tierId".to_string(), Value::String(tier_id.to_string()));
        body.insert("metadata".to_string(), build_gemini_metadata(project_id));

        if tier_id != "FREE" {
            if let Some(project) = project_id {
                body.insert(
                    "cloudaicompanionProject".to_string(),
                    Value::String(project.to_string()),
                );
            } else {
                return Err(std::io::Error::other(GEMINI_PROJECT_REQUIRED_MESSAGE));
            }
        } else {
            body.insert(
                "cloudaicompanionProject".to_string(),
                project_id
                    .map(|project| Value::String(project.to_string()))
                    .unwrap_or(Value::Null),
            );
        }

        let response = client
            .post(format!(
                "{GEMINI_CODE_ASSIST_ENDPOINT}/v1internal:onboardUser"
            ))
            .bearer_auth(access_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, GEMINI_CODE_ASSIST_USER_AGENT)
            .header("X-Goog-Api-Client", GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT)
            .header("Client-Metadata", GEMINI_CODE_ASSIST_CLIENT_METADATA)
            .json(&body)
            .send()
            .await
            .map_err(std::io::Error::other)?;

        if !response.status().is_success() {
            return Ok(None);
        }

        let payload: OnboardUserPayload = response.json().await.map_err(std::io::Error::other)?;
        if payload.done.unwrap_or(false) {
            if let Some(id) = payload
                .response
                .as_ref()
                .and_then(|resp| resp.cloudaicompanion_project.as_ref())
                .and_then(|project| project.id.clone())
            {
                return Ok(Some(id));
            }
            if let Some(project) = project_id {
                return Ok(Some(project.to_string()));
            }
        }

        sleep(Duration::from_millis(GEMINI_ONBOARD_DELAY_MILLIS)).await;
    }

    Ok(None)
}

async fn update_tokens(
    storage: &Arc<dyn AuthStorageBackend>,
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> std::io::Result<AuthDotJson> {
    let mut auth_dot_json = storage
        .load()?
        .ok_or(std::io::Error::other("Token data is not available."))?;

    let tokens = auth_dot_json.tokens.get_or_insert_with(TokenData::default);
    if let Some(id_token) = id_token {
        tokens.id_token = parse_id_token(&id_token).map_err(std::io::Error::other)?;
    }
    if let Some(access_token) = access_token {
        tokens.access_token = access_token;
    }
    if let Some(refresh_token) = refresh_token {
        tokens.refresh_token = refresh_token;
    }
    auth_dot_json.last_refresh = Some(Utc::now());
    storage.save(&auth_dot_json)?;
    Ok(auth_dot_json)
}

async fn try_refresh_token(
    refresh_token: String,
    client: &CodexHttpClient,
) -> Result<RefreshResponse, RefreshTokenError> {
    let refresh_request = RefreshRequest {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token,
        scope: "openid profile email",
    };

    let endpoint = refresh_token_endpoint();

    // Use shared client factory to include standard headers
    let response = client
        .post(endpoint.as_str())
        .header("Content-Type", "application/json")
        .json(&refresh_request)
        .send()
        .await
        .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?;

    let status = response.status();
    if status.is_success() {
        let refresh_response = response
            .json::<RefreshResponse>()
            .await
            .map_err(|err| RefreshTokenError::Transient(std::io::Error::other(err)))?;
        Ok(refresh_response)
    } else {
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            let failed = classify_refresh_token_failure(&body);
            Err(RefreshTokenError::Permanent(failed))
        } else {
            let message = try_parse_error_message(&body);
            Err(RefreshTokenError::Transient(std::io::Error::other(
                format!("Failed to refresh token: {status}: {message}"),
            )))
        }
    }
}

fn classify_refresh_token_failure(body: &str) -> RefreshTokenFailedError {
    let code = extract_refresh_token_error_code(body);

    let normalized_code = code.as_deref().map(str::to_ascii_lowercase);
    let reason = match normalized_code.as_deref() {
        Some("refresh_token_expired") => RefreshTokenFailedReason::Expired,
        Some("refresh_token_reused") => RefreshTokenFailedReason::Exhausted,
        Some("refresh_token_invalidated") => RefreshTokenFailedReason::Revoked,
        _ => RefreshTokenFailedReason::Other,
    };

    if reason == RefreshTokenFailedReason::Other {
        tracing::warn!(
            backend_code = normalized_code.as_deref(),
            backend_body = body,
            "Encountered unknown 401 response while refreshing token"
        );
    }

    let message = match reason {
        RefreshTokenFailedReason::Expired => REFRESH_TOKEN_EXPIRED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Exhausted => REFRESH_TOKEN_REUSED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Revoked => REFRESH_TOKEN_INVALIDATED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Other => REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
    };

    RefreshTokenFailedError::new(reason, message)
}

fn extract_refresh_token_error_code(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }

    let Value::Object(map) = serde_json::from_str::<Value>(body).ok()? else {
        return None;
    };

    if let Some(error_value) = map.get("error") {
        match error_value {
            Value::Object(obj) => {
                if let Some(code) = obj.get("code").and_then(Value::as_str) {
                    return Some(code.to_string());
                }
            }
            Value::String(code) => {
                return Some(code.to_string());
            }
            _ => {}
        }
    }

    map.get("code").and_then(Value::as_str).map(str::to_string)
}

#[derive(Serialize)]
struct RefreshRequest {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: String,
    scope: &'static str,
}

#[derive(Deserialize, Clone)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

// Shared constant for token refresh (client id used for oauth token refresh flow)
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

fn refresh_token_endpoint() -> String {
    std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| REFRESH_TOKEN_URL.to_string())
}

use std::sync::RwLock;

/// Internal cached auth state.
#[derive(Clone, Debug)]
struct CachedAuth {
    auth: Option<CodexAuth>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::storage::FileAuthStorage;
    use crate::auth::storage::get_auth_file;
    use crate::config::Config;
    use crate::config::ConfigOverrides;
    use crate::config::ConfigToml;
    use crate::token_data::IdTokenInfo;
    use crate::token_data::KnownPlan as InternalKnownPlan;
    use crate::token_data::PlanType as InternalPlanType;
    use codex_protocol::account::PlanType as AccountPlanType;

    use base64::Engine;
    use chrono::Utc;
    use codex_protocol::config_types::ForcedLoginMethod;
    use pretty_assertions::assert_eq;
    use serde::Serialize;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn refresh_without_id_token() {
        let codex_home = tempdir().unwrap();
        let fake_jwt = write_auth_file(
            AuthFileParams {
                openai_api_key: None,
                chatgpt_plan_type: "pro".to_string(),
                chatgpt_account_id: None,
            },
            codex_home.path(),
        )
        .expect("failed to write auth file");

        let storage = create_auth_storage(
            codex_home.path().to_path_buf(),
            AuthCredentialsStoreMode::File,
        );
        let updated = super::update_tokens(
            &storage,
            None,
            Some("new-access-token".to_string()),
            Some("new-refresh-token".to_string()),
        )
        .await
        .expect("update_tokens should succeed");

        let tokens = updated.tokens.expect("tokens should exist");
        assert_eq!(tokens.id_token.raw_jwt, fake_jwt);
        assert_eq!(tokens.access_token, "new-access-token");
        assert_eq!(tokens.refresh_token, "new-refresh-token");
    }

    #[test]
    fn login_with_api_key_overwrites_existing_auth_json() {
        let dir = tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        let stale_auth = json!({
            "OPENAI_API_KEY": "sk-old",
            "tokens": {
                "id_token": "stale.header.payload",
                "access_token": "stale-access",
                "refresh_token": "stale-refresh",
                "account_id": "stale-acc"
            }
        });
        std::fs::write(
            &auth_path,
            serde_json::to_string_pretty(&stale_auth).unwrap(),
        )
        .unwrap();

        super::login_with_api_key(dir.path(), "sk-new", AuthCredentialsStoreMode::File)
            .expect("login_with_api_key should succeed");

        let storage = FileAuthStorage::new(dir.path().to_path_buf());
        let auth = storage
            .try_read_auth_json(&auth_path)
            .expect("auth.json should parse");
        assert_eq!(auth.openai_api_key.as_deref(), Some("sk-new"));
        assert!(auth.tokens.is_none(), "tokens should be cleared");
    }

    #[test]
    fn missing_auth_json_returns_none() {
        let dir = tempdir().unwrap();
        let auth = CodexAuth::from_auth_storage(dir.path(), AuthCredentialsStoreMode::File)
            .expect("call should succeed");
        assert_eq!(auth, None);
    }

    #[tokio::test]
    #[serial(codex_api_key)]
    async fn pro_account_with_no_api_key_uses_chatgpt_auth() {
        let codex_home = tempdir().unwrap();
        let fake_jwt = write_auth_file(
            AuthFileParams {
                openai_api_key: None,
                chatgpt_plan_type: "pro".to_string(),
                chatgpt_account_id: None,
            },
            codex_home.path(),
        )
        .expect("failed to write auth file");

        let CodexAuth {
            api_key,
            mode,
            auth_dot_json,
            storage: _,
            ..
        } = super::load_auth(codex_home.path(), false, AuthCredentialsStoreMode::File)
            .unwrap()
            .unwrap();
        assert_eq!(None, api_key);
        assert_eq!(AuthMode::ChatGPT, mode);

        let guard = auth_dot_json.lock().unwrap();
        let auth_dot_json = guard.as_ref().expect("AuthDotJson should exist");
        let last_refresh = auth_dot_json
            .last_refresh
            .expect("last_refresh should be recorded");

        assert_eq!(
            &AuthDotJson {
                openai_api_key: None,
                tokens: Some(TokenData {
                    id_token: IdTokenInfo {
                        email: Some("user@example.com".to_string()),
                        chatgpt_plan_type: Some(InternalPlanType::Known(InternalKnownPlan::Pro)),
                        chatgpt_account_id: None,
                        raw_jwt: fake_jwt,
                    },
                    access_token: "test-access-token".to_string(),
                    refresh_token: "test-refresh-token".to_string(),
                    account_id: None,
                }),
                gemini_accounts: Vec::new(),
                antigravity_accounts: Vec::new(),
                last_refresh: Some(last_refresh),
            },
            auth_dot_json
        );
    }

    #[tokio::test]
    #[serial(codex_api_key)]
    async fn loads_api_key_from_auth_json() {
        let dir = tempdir().unwrap();
        let auth_file = dir.path().join("auth.json");
        std::fs::write(
            auth_file,
            r#"{"OPENAI_API_KEY":"sk-test-key","tokens":null,"last_refresh":null}"#,
        )
        .unwrap();

        let auth = super::load_auth(dir.path(), false, AuthCredentialsStoreMode::File)
            .unwrap()
            .unwrap();
        assert_eq!(auth.mode, AuthMode::ApiKey);
        assert_eq!(auth.api_key, Some("sk-test-key".to_string()));

        assert!(auth.get_token_data().await.is_err());
    }

    #[tokio::test]
    async fn get_token_errors_for_gemini_mode() {
        let dir = tempdir().unwrap();
        let auth_dot_json = AuthDotJson {
            openai_api_key: None,
            tokens: None,
            gemini_accounts: vec![GeminiTokenData {
                access_token: "access-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                id_token: None,
                project_id: Some("project-1".to_string()),
                managed_project_id: None,
                email: None,
                expires_at: None,
            }],
            antigravity_accounts: Vec::new(),
            last_refresh: None,
        };
        super::save_auth(dir.path(), &auth_dot_json, AuthCredentialsStoreMode::File).unwrap();

        let mut auth = super::load_auth(dir.path(), false, AuthCredentialsStoreMode::File)
            .unwrap()
            .unwrap();
        auth.mode = AuthMode::Gemini;

        assert!(auth.get_token().await.is_err());
    }

    #[test]
    fn load_auth_with_api_key_retains_gemini_accounts() {
        let dir = tempdir().unwrap();
        let gemini_token = GeminiTokenData {
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            id_token: None,
            project_id: Some("project-1".to_string()),
            managed_project_id: None,
            email: Some("user@example.com".to_string()),
            expires_at: Some(Utc::now()),
        };
        let auth_dot_json = AuthDotJson {
            openai_api_key: Some("sk-test-key".to_string()),
            tokens: None,
            gemini_accounts: vec![gemini_token.clone()],
            antigravity_accounts: Vec::new(),
            last_refresh: Some(Utc::now()),
        };
        super::save_auth(dir.path(), &auth_dot_json, AuthCredentialsStoreMode::File).unwrap();

        let auth = super::load_auth(dir.path(), false, AuthCredentialsStoreMode::File)
            .unwrap()
            .unwrap();
        assert_eq!(auth.mode, AuthMode::ApiKey);
        assert_eq!(auth.api_key, Some("sk-test-key".to_string()));
        assert_eq!(auth.gemini_account_count(), 1);
        assert_eq!(auth.get_all_gemini_tokens(), vec![gemini_token]);
    }

    #[test]
    fn logout_removes_auth_file() -> Result<(), std::io::Error> {
        let dir = tempdir()?;
        let auth_dot_json = AuthDotJson {
            openai_api_key: Some("sk-test-key".to_string()),
            tokens: None,
            gemini_accounts: Vec::new(),
            antigravity_accounts: Vec::new(),
            last_refresh: None,
        };
        super::save_auth(dir.path(), &auth_dot_json, AuthCredentialsStoreMode::File)?;
        let auth_file = get_auth_file(dir.path());
        assert!(auth_file.exists());
        assert!(logout(dir.path(), AuthCredentialsStoreMode::File)?);
        assert!(!auth_file.exists());
        Ok(())
    }

    struct AuthFileParams {
        openai_api_key: Option<String>,
        chatgpt_plan_type: String,
        chatgpt_account_id: Option<String>,
    }

    fn write_auth_file(params: AuthFileParams, codex_home: &Path) -> std::io::Result<String> {
        let auth_file = get_auth_file(codex_home);
        // Create a minimal valid JWT for the id_token field.
        #[derive(Serialize)]
        struct Header {
            alg: &'static str,
            typ: &'static str,
        }
        let header = Header {
            alg: "none",
            typ: "JWT",
        };
        let mut auth_payload = serde_json::json!({
            "chatgpt_plan_type": params.chatgpt_plan_type,
            "chatgpt_user_id": "user-12345",
            "user_id": "user-12345",
        });

        if let Some(chatgpt_account_id) = params.chatgpt_account_id {
            let org_value = serde_json::Value::String(chatgpt_account_id);
            auth_payload["chatgpt_account_id"] = org_value;
        }

        let payload = serde_json::json!({
            "email": "user@example.com",
            "email_verified": true,
            "https://api.openai.com/auth": auth_payload,
        });
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let header_b64 = b64(&serde_json::to_vec(&header)?);
        let payload_b64 = b64(&serde_json::to_vec(&payload)?);
        let signature_b64 = b64(b"sig");
        let fake_jwt = format!("{header_b64}.{payload_b64}.{signature_b64}");

        let auth_json_data = json!({
            "OPENAI_API_KEY": params.openai_api_key,
            "tokens": {
                "id_token": fake_jwt,
                "access_token": "test-access-token",
                "refresh_token": "test-refresh-token"
            },
            "last_refresh": Utc::now(),
        });
        let auth_json = serde_json::to_string_pretty(&auth_json_data)?;
        std::fs::write(auth_file, auth_json)?;
        Ok(fake_jwt)
    }

    fn build_config(
        codex_home: &Path,
        forced_login_method: Option<ForcedLoginMethod>,
        forced_chatgpt_workspace_id: Option<String>,
    ) -> Config {
        let mut config = Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            codex_home.to_path_buf(),
        )
        .expect("config should load");
        config.forced_login_method = forced_login_method;
        config.forced_chatgpt_workspace_id = forced_chatgpt_workspace_id;
        config
    }

    /// Use sparingly.
    /// TODO (gpeal): replace this with an injectable env var provider.
    #[cfg(test)]
    struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    #[cfg(test)]
    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    #[cfg(test)]
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    #[tokio::test]
    async fn enforce_login_restrictions_logs_out_for_method_mismatch() {
        let codex_home = tempdir().unwrap();
        login_with_api_key(codex_home.path(), "sk-test", AuthCredentialsStoreMode::File)
            .expect("seed api key");

        let config = build_config(codex_home.path(), Some(ForcedLoginMethod::Chatgpt), None);

        let err = super::enforce_login_restrictions(&config)
            .await
            .expect_err("expected method mismatch to error");
        assert!(err.to_string().contains("ChatGPT login is required"));
        assert!(
            !codex_home.path().join("auth.json").exists(),
            "auth.json should be removed on mismatch"
        );
    }

    #[tokio::test]
    #[serial(codex_api_key)]
    async fn enforce_login_restrictions_logs_out_for_workspace_mismatch() {
        let codex_home = tempdir().unwrap();
        let _jwt = write_auth_file(
            AuthFileParams {
                openai_api_key: None,
                chatgpt_plan_type: "pro".to_string(),
                chatgpt_account_id: Some("org_another_org".to_string()),
            },
            codex_home.path(),
        )
        .expect("failed to write auth file");

        let config = build_config(codex_home.path(), None, Some("org_mine".to_string()));

        let err = super::enforce_login_restrictions(&config)
            .await
            .expect_err("expected workspace mismatch to error");
        assert!(err.to_string().contains("workspace org_mine"));
        assert!(
            !codex_home.path().join("auth.json").exists(),
            "auth.json should be removed on mismatch"
        );
    }

    #[tokio::test]
    #[serial(codex_api_key)]
    async fn enforce_login_restrictions_allows_matching_workspace() {
        let codex_home = tempdir().unwrap();
        let _jwt = write_auth_file(
            AuthFileParams {
                openai_api_key: None,
                chatgpt_plan_type: "pro".to_string(),
                chatgpt_account_id: Some("org_mine".to_string()),
            },
            codex_home.path(),
        )
        .expect("failed to write auth file");

        let config = build_config(codex_home.path(), None, Some("org_mine".to_string()));

        super::enforce_login_restrictions(&config)
            .await
            .expect("matching workspace should succeed");
        assert!(
            codex_home.path().join("auth.json").exists(),
            "auth.json should remain when restrictions pass"
        );
    }

    #[tokio::test]
    async fn enforce_login_restrictions_allows_api_key_if_login_method_not_set_but_forced_chatgpt_workspace_id_is_set()
     {
        let codex_home = tempdir().unwrap();
        login_with_api_key(codex_home.path(), "sk-test", AuthCredentialsStoreMode::File)
            .expect("seed api key");

        let config = build_config(codex_home.path(), None, Some("org_mine".to_string()));

        super::enforce_login_restrictions(&config)
            .await
            .expect("matching workspace should succeed");
        assert!(
            codex_home.path().join("auth.json").exists(),
            "auth.json should remain when restrictions pass"
        );
    }

    #[tokio::test]
    #[serial(codex_api_key)]
    async fn enforce_login_restrictions_blocks_env_api_key_when_chatgpt_required() {
        let _guard = EnvVarGuard::set(CODEX_API_KEY_ENV_VAR, "sk-env");
        let codex_home = tempdir().unwrap();

        let config = build_config(codex_home.path(), Some(ForcedLoginMethod::Chatgpt), None);

        let err = super::enforce_login_restrictions(&config)
            .await
            .expect_err("environment API key should not satisfy forced ChatGPT login");
        assert!(
            err.to_string()
                .contains("ChatGPT login is required, but an API key is currently being used.")
        );
    }

    #[test]
    fn plan_type_maps_known_plan() {
        let codex_home = tempdir().unwrap();
        let _jwt = write_auth_file(
            AuthFileParams {
                openai_api_key: None,
                chatgpt_plan_type: "pro".to_string(),
                chatgpt_account_id: None,
            },
            codex_home.path(),
        )
        .expect("failed to write auth file");

        let auth = super::load_auth(codex_home.path(), false, AuthCredentialsStoreMode::File)
            .expect("load auth")
            .expect("auth available");

        pretty_assertions::assert_eq!(auth.account_plan_type(), Some(AccountPlanType::Pro));
    }

    #[test]
    fn plan_type_maps_unknown_to_unknown() {
        let codex_home = tempdir().unwrap();
        let _jwt = write_auth_file(
            AuthFileParams {
                openai_api_key: None,
                chatgpt_plan_type: "mystery-tier".to_string(),
                chatgpt_account_id: None,
            },
            codex_home.path(),
        )
        .expect("failed to write auth file");

        let auth = super::load_auth(codex_home.path(), false, AuthCredentialsStoreMode::File)
            .expect("load auth")
            .expect("auth available");

        pretty_assertions::assert_eq!(auth.account_plan_type(), Some(AccountPlanType::Unknown));
    }
}

/// Central manager providing a single source of truth for auth.json derived
/// authentication data. It loads once (or on preference change) and then
/// hands out cloned `CodexAuth` values so the rest of the program has a
/// consistent snapshot.
///
/// External modifications to `auth.json` will NOT be observed until
/// `reload()` is called explicitly. This matches the design goal of avoiding
/// different parts of the program seeing inconsistent auth data mid‑run.
#[derive(Debug)]
pub struct AuthManager {
    codex_home: PathBuf,
    inner: RwLock<CachedAuth>,
    enable_codex_api_key_env: bool,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
}

impl AuthManager {
    /// Create a new manager loading the initial auth using the provided
    /// preferred auth method. Errors loading auth are swallowed; `auth()` will
    /// simply return `None` in that case so callers can treat it as an
    /// unauthenticated state.
    pub fn new(
        codex_home: PathBuf,
        enable_codex_api_key_env: bool,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
    ) -> Self {
        let auth = load_auth(
            &codex_home,
            enable_codex_api_key_env,
            auth_credentials_store_mode,
        )
        .ok()
        .flatten();
        Self {
            codex_home,
            inner: RwLock::new(CachedAuth { auth }),
            enable_codex_api_key_env,
            auth_credentials_store_mode,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[expect(clippy::expect_used)]
    /// Create an AuthManager with a specific CodexAuth, for testing only.
    pub fn from_auth_for_testing(auth: CodexAuth) -> Arc<Self> {
        let cached = CachedAuth { auth: Some(auth) };
        let temp_dir = tempfile::tempdir().expect("temp codex home");
        let codex_home = temp_dir.path().to_path_buf();
        TEST_AUTH_TEMP_DIRS
            .lock()
            .expect("lock test codex homes")
            .push(temp_dir);
        Arc::new(Self {
            codex_home,
            inner: RwLock::new(cached),
            enable_codex_api_key_env: false,
            auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        })
    }

    /// Current cached auth (clone). May be `None` if not logged in or load failed.
    pub fn auth(&self) -> Option<CodexAuth> {
        self.inner.read().ok().and_then(|c| c.auth.clone())
    }

    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    /// Force a reload of the auth information from auth.json. Returns
    /// whether the auth value changed.
    pub fn reload(&self) -> bool {
        let new_auth = load_auth(
            &self.codex_home,
            self.enable_codex_api_key_env,
            self.auth_credentials_store_mode,
        )
        .ok()
        .flatten();
        if let Ok(mut guard) = self.inner.write() {
            let changed = !AuthManager::auths_equal(&guard.auth, &new_auth);
            guard.auth = new_auth;
            changed
        } else {
            false
        }
    }

    fn auths_equal(a: &Option<CodexAuth>, b: &Option<CodexAuth>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Convenience constructor returning an `Arc` wrapper.
    pub fn shared(
        codex_home: PathBuf,
        enable_codex_api_key_env: bool,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
    ) -> Arc<Self> {
        Arc::new(Self::new(
            codex_home,
            enable_codex_api_key_env,
            auth_credentials_store_mode,
        ))
    }

    /// Attempt to refresh the current auth token (if any). On success, reload
    /// the auth state from disk so other components observe refreshed token.
    /// If the token refresh fails in a permanent (non‑transient) way, logs out
    /// to clear invalid auth state.
    pub async fn refresh_token(&self) -> Result<Option<String>, RefreshTokenError> {
        let auth = match self.auth() {
            Some(a) => a,
            None => return Ok(None),
        };
        match auth.refresh_token().await {
            Ok(token) => {
                // Reload to pick up persisted changes.
                self.reload();
                Ok(Some(token))
            }
            Err(e) => {
                tracing::error!("Failed to refresh token: {}", e);
                Err(e)
            }
        }
    }

    /// Log out by deleting the on‑disk auth.json (if present). Returns Ok(true)
    /// if a file was removed, Ok(false) if no auth file existed. On success,
    /// reloads the in‑memory auth cache so callers immediately observe the
    /// unauthenticated state.
    pub fn logout(&self) -> std::io::Result<bool> {
        let removed = super::auth::logout(&self.codex_home, self.auth_credentials_store_mode)?;
        // Always reload to clear any cached auth (even if file absent).
        self.reload();
        Ok(removed)
    }

    pub fn get_auth_mode(&self) -> Option<AuthMode> {
        self.auth().map(|a| a.mode)
    }
}
