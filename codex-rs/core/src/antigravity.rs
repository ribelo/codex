//! Shared Antigravity OAuth and endpoint constants.

use std::io;

pub const ANTIGRAVITY_CLIENT_ID_ENV_VAR: &str = "CODEX_ANTIGRAVITY_OAUTH_CLIENT_ID";
pub const ANTIGRAVITY_CLIENT_SECRET_ENV_VAR: &str = "CODEX_ANTIGRAVITY_OAUTH_CLIENT_SECRET";
pub const ANTIGRAVITY_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const ANTIGRAVITY_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const ANTIGRAVITY_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";

pub const ANTIGRAVITY_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

pub fn antigravity_client_id() -> io::Result<String> {
    read_required_env(
        ANTIGRAVITY_CLIENT_ID_ENV_VAR,
        "Antigravity OAuth requires CODEX_ANTIGRAVITY_OAUTH_CLIENT_ID to be set.",
    )
}

pub fn antigravity_client_secret() -> Option<String> {
    read_optional_env(ANTIGRAVITY_CLIENT_SECRET_ENV_VAR)
}

fn read_required_env(env_var: &str, missing_message: &str) -> io::Result<String> {
    match std::env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(_) => Err(io::Error::other(missing_message)),
    }
}

fn read_optional_env(env_var: &str) -> Option<String> {
    std::env::var(env_var)
        .ok()
        .filter(|value| !value.trim().is_empty())
}
