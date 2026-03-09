//! Shared Gemini OAuth and Code Assist constants.

use std::io;

pub const GEMINI_CLIENT_ID_ENV_VAR: &str = "CODEX_GEMINI_OAUTH_CLIENT_ID";
pub const GEMINI_CLIENT_SECRET_ENV_VAR: &str = "CODEX_GEMINI_OAUTH_CLIENT_SECRET";
pub const GEMINI_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

pub const GEMINI_CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const GEMINI_CODE_ASSIST_USER_AGENT: &str = "google-cloud-sdk vscode_cloudshelleditor/0.1";
pub const GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT: &str = "gl-node/22.17.0";
pub const GEMINI_CODE_ASSIST_CLIENT_METADATA: &str = "{\"ideType\":\"IDE_UNSPECIFIED\",\"platform\":\"PLATFORM_UNSPECIFIED\",\"pluginType\":\"GEMINI\"}";

pub const GEMINI_METADATA_IDE_TYPE: &str = "IDE_UNSPECIFIED";
pub const GEMINI_METADATA_PLATFORM: &str = "PLATFORM_UNSPECIFIED";
pub const GEMINI_METADATA_PLUGIN: &str = "GEMINI";

pub fn gemini_client_id() -> io::Result<String> {
    read_required_env(
        GEMINI_CLIENT_ID_ENV_VAR,
        "Gemini OAuth requires CODEX_GEMINI_OAUTH_CLIENT_ID to be set.",
    )
}

pub fn gemini_client_secret() -> Option<String> {
    read_optional_env(GEMINI_CLIENT_SECRET_ENV_VAR)
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
