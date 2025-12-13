use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use ts_rs::TS;

/// Base namespace for custom command slash commands (without trailing colon).
/// Example usage forms constructed in code:
/// - Command token after '/': `"{COMMANDS_CMD_PREFIX}:name"`
/// - Full slash prefix: `"/{COMMANDS_CMD_PREFIX}:"`
pub const COMMANDS_CMD_PREFIX: &str = "commands";

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, TS)]
pub struct CustomCommand {
    pub name: String,
    pub path: PathBuf,
    /// Markdown body = user message for the delegated session.
    pub content: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// Mutually exclusive with `profile`.
    pub agent: Option<String>,
    /// Mutually exclusive with `agent`.
    pub profile: Option<String>,
}
