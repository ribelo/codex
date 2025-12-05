//! Shared Gemini constants and helpers.

pub const GEMINI_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
pub const GEMINI_CLIENT_SECRET: &str = "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";
pub const GEMINI_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

pub const GEMINI_CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const GEMINI_CODE_ASSIST_USER_AGENT: &str = "google-api-nodejs-client/9.15.1";
pub const GEMINI_CODE_ASSIST_X_GOOG_API_CLIENT: &str = "gl-node/22.17.0";
pub const GEMINI_CODE_ASSIST_CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";

pub const GEMINI_METADATA_IDE_TYPE: &str = "IDE_UNSPECIFIED";
pub const GEMINI_METADATA_PLATFORM: &str = "PLATFORM_UNSPECIFIED";
pub const GEMINI_METADATA_PLUGIN: &str = "GEMINI";

pub const GEMINI_MODEL_FALLBACKS: &[(&str, &str)] = &[
    ("gemini-2.5-flash-image", "gemini-2.5-flash"),
    ("gemini-3-pro", "gemini-3-pro-preview"),
];
