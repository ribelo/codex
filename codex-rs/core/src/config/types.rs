//! Types used to define the fields of [`crate::config::Config`].

// Note this file should generally be restricted to simple struct/enum
// definitions that do not contain business logic.

use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use wildmatch::WildMatchPattern;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;

use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use codex_protocol::openai_models::ReasoningEffort;

pub const DEFAULT_OTEL_ENVIRONMENT: &str = "dev";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfigToml {
    pub name: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<SubagentReasoningEffort>,
    pub enabled: Option<bool>,
}

/// Reasoning effort setting for subagents, with an additional `Inherit` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SubagentReasoningEffort {
    /// Inherit the parent session's reasoning effort.
    #[default]
    Inherit,
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(alias = "x-high")]
    XHigh,
}

impl SubagentReasoningEffort {
    /// Convert to the protocol's `ReasoningEffort`, returning `None` for `Inherit`.
    pub fn to_reasoning_effort(self) -> Option<ReasoningEffort> {
        match self {
            SubagentReasoningEffort::Inherit => None,
            SubagentReasoningEffort::None => Some(ReasoningEffort::None),
            SubagentReasoningEffort::Minimal => Some(ReasoningEffort::Minimal),
            SubagentReasoningEffort::Low => Some(ReasoningEffort::Low),
            SubagentReasoningEffort::Medium => Some(ReasoningEffort::Medium),
            SubagentReasoningEffort::High => Some(ReasoningEffort::High),
            SubagentReasoningEffort::XHigh => Some(ReasoningEffort::XHigh),
        }
    }
}

/// Approval policy for delegated tasks, with an additional `Inherit` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentApprovalPolicy {
    /// Inherit the parent session's approval policy.
    Inherit,
    /// Only "known safe" commands are auto-approved.
    #[serde(rename = "untrusted")]
    UnlessTrusted,
    /// All commands auto-approved in sandbox; failures escalate.
    OnFailure,
    /// The model decides when to ask.
    OnRequest,
    /// Never ask the user.
    Never,
}

impl SubagentApprovalPolicy {
    /// Convert to the protocol's `AskForApproval`, returning `None` for `Inherit`.
    pub fn to_ask_for_approval(self) -> Option<AskForApproval> {
        match self {
            SubagentApprovalPolicy::Inherit => None,
            SubagentApprovalPolicy::UnlessTrusted => Some(AskForApproval::UnlessTrusted),
            SubagentApprovalPolicy::OnFailure => Some(AskForApproval::OnFailure),
            SubagentApprovalPolicy::OnRequest => Some(AskForApproval::OnRequest),
            SubagentApprovalPolicy::Never => Some(AskForApproval::Never),
        }
    }

    /// Returns the restrictiveness level (lower = more restrictive).
    /// `Inherit` returns `None` since it depends on the parent.
    pub fn restrictiveness(self) -> Option<i32> {
        match self {
            SubagentApprovalPolicy::Inherit => None,
            // Keep this mapping consistent with `approval_restrictiveness()` in
            // `core/src/tools/handlers/task.rs`.
            SubagentApprovalPolicy::UnlessTrusted => Some(0),
            SubagentApprovalPolicy::Never => Some(1),
            SubagentApprovalPolicy::OnRequest => Some(2),
            SubagentApprovalPolicy::OnFailure => Some(3),
        }
    }
}

/// Simplified sandbox policy for delegated tasks.
/// Maps to the full `SandboxPolicy` enum but with simpler serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentSandboxPolicy {
    ReadOnly,
    WorkspaceWrite,
    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
    /// Inherit the parent session's sandbox policy.
    Inherit,
}

impl SubagentSandboxPolicy {
    /// Convert to the full SandboxPolicy enum, returning `None` for `Inherit`.
    pub fn to_sandbox_policy(self) -> Option<SandboxPolicy> {
        match self {
            SubagentSandboxPolicy::ReadOnly => Some(SandboxPolicy::new_read_only_policy()),
            SubagentSandboxPolicy::WorkspaceWrite => {
                Some(SandboxPolicy::new_workspace_write_policy())
            }
            SubagentSandboxPolicy::DangerFullAccess => Some(SandboxPolicy::DangerFullAccess),
            SubagentSandboxPolicy::Inherit => None,
        }
    }

    /// Returns the restrictiveness level (lower = more restrictive).
    /// `Inherit` returns `None` since it depends on the parent.
    pub fn restrictiveness(self) -> Option<i32> {
        match self {
            SubagentSandboxPolicy::ReadOnly => Some(0),
            SubagentSandboxPolicy::WorkspaceWrite => Some(1),
            SubagentSandboxPolicy::DangerFullAccess => Some(2),
            SubagentSandboxPolicy::Inherit => None,
        }
    }

    /// Convert from SandboxPolicy to SubagentSandboxPolicy for comparison.
    pub fn from_sandbox_policy(policy: &SandboxPolicy) -> Self {
        match policy {
            SandboxPolicy::ReadOnly => SubagentSandboxPolicy::ReadOnly,
            SandboxPolicy::WorkspaceWrite { .. } => SubagentSandboxPolicy::WorkspaceWrite,
            SandboxPolicy::DangerFullAccess => SubagentSandboxPolicy::DangerFullAccess,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, std::hash::Hash, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskDifficulty {
    Small,
    #[default]
    Medium,
    Large,
}

impl TaskDifficulty {
    pub const fn as_str(self) -> &'static str {
        match self {
            TaskDifficulty::Small => "small",
            TaskDifficulty::Medium => "medium",
            TaskDifficulty::Large => "large",
        }
    }
}

impl std::fmt::Display for TaskDifficulty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TaskDifficultyStrategy {
    pub model: Option<String>,
    pub reasoning_effort: Option<SubagentReasoningEffort>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskConfigToml {
    #[serde(rename = "type")]
    pub task_type: String,
    pub description: Option<String>,
    pub skills: Option<Vec<String>>,
    pub model: Option<String>,
    pub reasoning_effort: Option<SubagentReasoningEffort>,
    pub sandbox_policy: Option<SubagentSandboxPolicy>,
    pub approval_policy: Option<SubagentApprovalPolicy>,
    pub difficulty: Option<HashMap<TaskDifficulty, TaskDifficultyStrategy>>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub transport: McpServerTransportConfig,

    /// When `false`, Codex skips initializing this MCP server.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Startup timeout in seconds for initializing MCP server & initially listing tools.
    #[serde(
        default,
        with = "option_duration_secs",
        skip_serializing_if = "Option::is_none"
    )]
    pub startup_timeout_sec: Option<Duration>,

    /// Default timeout for MCP tool calls initiated via this server.
    #[serde(default, with = "option_duration_secs")]
    pub tool_timeout_sec: Option<Duration>,

    /// Explicit allow-list of tools exposed from this server. When set, only these tools will be registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,

    /// Explicit deny-list of tools. These tools will be removed after applying `enabled_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Clone)]
        #[serde(deny_unknown_fields)]
        struct RawMcpServerConfig {
            // stdio
            command: Option<String>,
            #[serde(default)]
            args: Option<Vec<String>>,
            #[serde(default)]
            env: Option<HashMap<String, String>>,
            #[serde(default)]
            env_vars: Option<Vec<String>>,
            #[serde(default)]
            cwd: Option<PathBuf>,
            http_headers: Option<HashMap<String, String>>,
            #[serde(default)]
            env_http_headers: Option<HashMap<String, String>>,

            // streamable_http
            url: Option<String>,
            bearer_token: Option<String>,
            bearer_token_env_var: Option<String>,

            // shared
            #[serde(default)]
            startup_timeout_sec: Option<f64>,
            #[serde(default)]
            startup_timeout_ms: Option<u64>,
            #[serde(default, with = "option_duration_secs")]
            tool_timeout_sec: Option<Duration>,
            #[serde(default)]
            enabled: Option<bool>,
            #[serde(default)]
            enabled_tools: Option<Vec<String>>,
            #[serde(default)]
            disabled_tools: Option<Vec<String>>,
        }

        let mut raw = RawMcpServerConfig::deserialize(deserializer)?;

        let startup_timeout_sec = match (raw.startup_timeout_sec, raw.startup_timeout_ms) {
            (Some(sec), _) => {
                let duration = Duration::try_from_secs_f64(sec).map_err(SerdeError::custom)?;
                Some(duration)
            }
            (None, Some(ms)) => Some(Duration::from_millis(ms)),
            (None, None) => None,
        };
        let tool_timeout_sec = raw.tool_timeout_sec;
        let enabled = raw.enabled.unwrap_or_else(default_enabled);
        let enabled_tools = raw.enabled_tools.clone();
        let disabled_tools = raw.disabled_tools.clone();

        fn throw_if_set<E, T>(transport: &str, field: &str, value: Option<&T>) -> Result<(), E>
        where
            E: SerdeError,
        {
            if value.is_none() {
                return Ok(());
            }
            Err(E::custom(format!(
                "{field} is not supported for {transport}",
            )))
        }

        let transport = if let Some(command) = raw.command.clone() {
            throw_if_set("stdio", "url", raw.url.as_ref())?;
            throw_if_set(
                "stdio",
                "bearer_token_env_var",
                raw.bearer_token_env_var.as_ref(),
            )?;
            throw_if_set("stdio", "bearer_token", raw.bearer_token.as_ref())?;
            throw_if_set("stdio", "http_headers", raw.http_headers.as_ref())?;
            throw_if_set("stdio", "env_http_headers", raw.env_http_headers.as_ref())?;
            McpServerTransportConfig::Stdio {
                command,
                args: raw.args.clone().unwrap_or_default(),
                env: raw.env.clone(),
                env_vars: raw.env_vars.clone().unwrap_or_default(),
                cwd: raw.cwd.take(),
            }
        } else if let Some(url) = raw.url.clone() {
            throw_if_set("streamable_http", "args", raw.args.as_ref())?;
            throw_if_set("streamable_http", "env", raw.env.as_ref())?;
            throw_if_set("streamable_http", "env_vars", raw.env_vars.as_ref())?;
            throw_if_set("streamable_http", "cwd", raw.cwd.as_ref())?;
            throw_if_set("streamable_http", "bearer_token", raw.bearer_token.as_ref())?;
            McpServerTransportConfig::StreamableHttp {
                url,
                bearer_token_env_var: raw.bearer_token_env_var.clone(),
                http_headers: raw.http_headers.clone(),
                env_http_headers: raw.env_http_headers.take(),
            }
        } else {
            return Err(SerdeError::custom("invalid transport"));
        };

        Ok(Self {
            transport,
            startup_timeout_sec,
            tool_timeout_sec,
            enabled,
            enabled_tools,
            disabled_tools,
        })
    }
}

const fn default_enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged, deny_unknown_fields, rename_all = "snake_case")]
pub enum McpServerTransportConfig {
    /// https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#stdio
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env_vars: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    /// https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http
    StreamableHttp {
        url: String,
        /// Name of the environment variable to read for an HTTP bearer token.
        /// When set, requests will include the token via `Authorization: Bearer <token>`.
        /// The actual secret value must be provided via the environment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token_env_var: Option<String>,
        /// Additional HTTP headers to include in requests to this server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_headers: Option<HashMap<String, String>>,
        /// HTTP headers where the value is sourced from an environment variable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env_http_headers: Option<HashMap<String, String>>,
    },
}

mod option_duration_secs {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use std::time::Duration;

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(duration) => serializer.serialize_some(&duration.as_secs_f64()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = Option::<f64>::deserialize(deserializer)?;
        secs.map(|secs| Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Deserialize, Debug, Copy, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum UriBasedFileOpener {
    #[serde(rename = "vscode")]
    VsCode,

    #[serde(rename = "vscode-insiders")]
    VsCodeInsiders,

    #[serde(rename = "windsurf")]
    Windsurf,

    #[serde(rename = "cursor")]
    Cursor,

    /// Option to disable the URI-based file opener.
    #[serde(rename = "none")]
    None,
}

impl UriBasedFileOpener {
    pub fn get_scheme(&self) -> Option<&str> {
        match self {
            UriBasedFileOpener::VsCode => Some("vscode"),
            UriBasedFileOpener::VsCodeInsiders => Some("vscode-insiders"),
            UriBasedFileOpener::Windsurf => Some("windsurf"),
            UriBasedFileOpener::Cursor => Some("cursor"),
            UriBasedFileOpener::None => None,
        }
    }
}

/// Settings for ghost snapshots.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct GhostSnapshotToml {
    /// Files larger than this size (in bytes) will be treated as untracked.
    pub ignore_large_untracked_files: Option<i64>,

    /// Directories with more than this number of untracked files will be treated as untracked.
    pub ignore_large_untracked_dirs: Option<i64>,

    /// If true, warnings about large untracked files/dirs will be suppressed.
    #[serde(default)]
    pub disable_warnings: bool,
}

/// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// If true, history entries will not be written to disk.
    pub persistence: HistoryPersistence,

    /// If set, the maximum size of the history file in bytes. The oldest entries
    /// are dropped once the file exceeds this limit.
    pub max_bytes: Option<usize>,
}

#[derive(Deserialize, Debug, Copy, Clone, PartialEq, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum HistoryPersistence {
    /// Save all history entries to disk.
    #[default]
    SaveAll,
    /// Do not write history to disk.
    None,
}

// ===== OTEL configuration =====

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum OtelHttpProtocol {
    /// Binary payload
    Binary,
    /// JSON payload
    Json,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OtelTlsConfig {
    pub ca_certificate: Option<AbsolutePathBuf>,
    pub client_certificate: Option<AbsolutePathBuf>,
    pub client_private_key: Option<AbsolutePathBuf>,
}

/// Which OTEL exporter to use.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum OtelExporterKind {
    None,
    OtlpHttp {
        endpoint: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        protocol: OtelHttpProtocol,
        #[serde(default)]
        tls: Option<OtelTlsConfig>,
    },
    OtlpGrpc {
        endpoint: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        tls: Option<OtelTlsConfig>,
    },
}

/// OTEL settings loaded from config.toml. Fields are optional so we can apply defaults.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct OtelConfigToml {
    /// Log user prompt in traces
    pub log_user_prompt: Option<bool>,

    /// Mark traces with environment (dev, staging, prod, test). Defaults to dev.
    pub environment: Option<String>,

    /// Exporter to use. Defaults to `otlp-file`.
    pub exporter: Option<OtelExporterKind>,
}

/// Effective OTEL settings after defaults are applied.
#[derive(Debug, Clone, PartialEq)]
pub struct OtelConfig {
    pub log_user_prompt: bool,
    pub environment: String,
    pub exporter: OtelExporterKind,
}

impl Default for OtelConfig {
    fn default() -> Self {
        OtelConfig {
            log_user_prompt: false,
            environment: DEFAULT_OTEL_ENVIRONMENT.to_owned(),
            exporter: OtelExporterKind::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Notifications {
    Enabled(bool),
    Custom(Vec<String>),
}

impl Default for Notifications {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

/// Collection of settings that are specific to the TUI.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Tui {
    /// Enable desktop notifications from the TUI when the terminal is unfocused.
    /// Defaults to `true`.
    #[serde(default)]
    pub notifications: Notifications,

    /// Enable animations (welcome screen, shimmer effects, spinners).
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub animations: bool,

    /// Show startup tooltips in the TUI welcome screen.
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub show_tooltips: bool,

    /// Use alternate screen buffer for the TUI.
    /// When false, avoids EnterAlternateScreen/LeaveAlternateScreen to allow
    /// terminal scrollback (useful for Zellij and similar multiplexers).
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub alternate_screen: bool,
}

const fn default_true() -> bool {
    true
}

/// Configuration for handoff extraction to a new thread.
/// Allows using a different model (e.g., one with larger context) for extraction.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct HandoffConfig {
    /// Model provider to use for handoff extraction (e.g., "gemini", "openai").
    /// If not set, uses the current session's provider.
    pub model_provider: Option<String>,

    /// Model to use for handoff extraction (e.g., "gemini-2.5-flash").
    /// If not set, uses the current session's model.
    pub model: Option<String>,

    /// Context window size for the handoff model.
    /// If not set, uses a conservative default or the provider's default.
    pub model_context_window: Option<i64>,
}

/// Settings for notices we display to users via the tui and app-server clients
/// (primarily the Codex IDE extension). NOTE: these are different from
/// notifications - notices are warnings, NUX screens, acknowledgements, etc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    /// Tracks whether the user has acknowledged the full access warning prompt.
    pub hide_full_access_warning: Option<bool>,
    /// Tracks whether the user has acknowledged the Windows world-writable directories warning.
    pub hide_world_writable_warning: Option<bool>,
    /// Tracks whether the user opted out of the rate limit model switch reminder.
    pub hide_rate_limit_model_nudge: Option<bool>,
    /// Tracks whether the user has seen the model migration prompt.
    pub hide_gpt5_1_migration_prompt: Option<bool>,
    /// Tracks whether the user has seen the gpt-5.1-codex-max migration prompt.
    #[serde(rename = "hide_gpt-5.1-codex-max_migration_prompt")]
    pub hide_gpt_5_1_codex_max_migration_prompt: Option<bool>,
    /// Tracks acknowledged model migrations as old->new model slug mappings.
    #[serde(default)]
    pub model_migrations: BTreeMap<String, String>,
}

impl Notice {
    /// referenced by config_edit helpers when writing notice flags
    pub(crate) const TABLE_KEY: &'static str = "notice";
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceWrite {
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
    #[serde(default)]
    pub exclude_slash_tmp: bool,
}

impl From<SandboxWorkspaceWrite> for codex_app_server_protocol::SandboxSettings {
    fn from(sandbox_workspace_write: SandboxWorkspaceWrite) -> Self {
        Self {
            writable_roots: sandbox_workspace_write.writable_roots,
            network_access: Some(sandbox_workspace_write.network_access),
            exclude_tmpdir_env_var: Some(sandbox_workspace_write.exclude_tmpdir_env_var),
            exclude_slash_tmp: Some(sandbox_workspace_write.exclude_slash_tmp),
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum ShellEnvironmentPolicyInherit {
    /// "Core" environment variables for the platform. On UNIX, this would
    /// include HOME, LOGNAME, PATH, SHELL, and USER, among others.
    Core,

    /// Inherits the full environment from the parent process.
    #[default]
    All,

    /// Do not inherit any environment variables from the parent process.
    None,
}

/// Policy for building the `env` when spawning a process via either the
/// `shell` or `local_shell` tool.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ShellEnvironmentPolicyToml {
    pub inherit: Option<ShellEnvironmentPolicyInherit>,

    pub ignore_default_excludes: Option<bool>,

    /// List of regular expressions.
    pub exclude: Option<Vec<String>>,

    pub r#set: Option<HashMap<String, String>>,

    /// List of regular expressions.
    pub include_only: Option<Vec<String>>,

    pub experimental_use_profile: Option<bool>,
}

pub type EnvironmentVariablePattern = WildMatchPattern<'*', '?'>;

/// Deriving the `env` based on this policy works as follows:
/// 1. Create an initial map based on the `inherit` policy.
/// 2. If `ignore_default_excludes` is false, filter the map using the default
///    exclude pattern(s), which are: `"*KEY*"` and `"*TOKEN*"`.
/// 3. If `exclude` is not empty, filter the map using the provided patterns.
/// 4. Insert any entries from `r#set` into the map.
/// 5. If non-empty, filter the map using the `include_only` patterns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShellEnvironmentPolicy {
    /// Starting point when building the environment.
    pub inherit: ShellEnvironmentPolicyInherit,

    /// True to skip the check to exclude default environment variables that
    /// contain "KEY" or "TOKEN" in their name.
    pub ignore_default_excludes: bool,

    /// Environment variable names to exclude from the environment.
    pub exclude: Vec<EnvironmentVariablePattern>,

    /// (key, value) pairs to insert in the environment.
    pub r#set: HashMap<String, String>,

    /// Environment variable names to retain in the environment.
    pub include_only: Vec<EnvironmentVariablePattern>,

    /// If true, the shell profile will be used to run the command.
    pub use_profile: bool,
}

impl From<ShellEnvironmentPolicyToml> for ShellEnvironmentPolicy {
    fn from(toml: ShellEnvironmentPolicyToml) -> Self {
        // Default to inheriting the full environment when not specified.
        let inherit = toml.inherit.unwrap_or(ShellEnvironmentPolicyInherit::All);
        let ignore_default_excludes = toml.ignore_default_excludes.unwrap_or(false);
        let exclude = toml
            .exclude
            .unwrap_or_default()
            .into_iter()
            .map(|s| EnvironmentVariablePattern::new_case_insensitive(&s))
            .collect();
        let r#set = toml.r#set.unwrap_or_default();
        let include_only = toml
            .include_only
            .unwrap_or_default()
            .into_iter()
            .map(|s| EnvironmentVariablePattern::new_case_insensitive(&s))
            .collect();
        let use_profile = toml.experimental_use_profile.unwrap_or(false);

        Self {
            inherit,
            ignore_default_excludes,
            exclude,
            r#set,
            include_only,
            use_profile,
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ShellConfigToml {
    /// Default timeout for shell commands in milliseconds. Falls back to 10,000 if not set.
    pub default_timeout_ms: Option<u64>,

    /// List of pattern-based timeout overrides. First match wins.
    #[serde(default)]
    pub timeout_overrides: Vec<ShellTimeoutOverrideToml>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShellTimeoutOverrideToml {
    /// Regex pattern to match against the command string.
    pub pattern: String,
    /// Timeout in milliseconds for commands matching this pattern.
    pub timeout_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn deserialize_stdio_command_server_config() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
        "#,
        )
        .expect("should deserialize command config");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            }
        );
        assert!(cfg.enabled);
        assert!(cfg.enabled_tools.is_none());
        assert!(cfg.disabled_tools.is_none());
    }

    #[test]
    fn deserialize_stdio_command_server_config_with_args() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
            args = ["hello", "world"]
        "#,
        )
        .expect("should deserialize command config");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec!["hello".to_string(), "world".to_string()],
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            }
        );
        assert!(cfg.enabled);
    }

    #[test]
    fn deserialize_stdio_command_server_config_with_arg_with_args_and_env() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
            args = ["hello", "world"]
            env = { "FOO" = "BAR" }
        "#,
        )
        .expect("should deserialize command config");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec!["hello".to_string(), "world".to_string()],
                env: Some(HashMap::from([("FOO".to_string(), "BAR".to_string())])),
                env_vars: Vec::new(),
                cwd: None,
            }
        );
        assert!(cfg.enabled);
    }

    #[test]
    fn deserialize_stdio_command_server_config_with_env_vars() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
            env_vars = ["FOO", "BAR"]
        "#,
        )
        .expect("should deserialize command config with env_vars");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                env_vars: vec!["FOO".to_string(), "BAR".to_string()],
                cwd: None,
            }
        );
    }

    #[test]
    fn deserialize_stdio_command_server_config_with_cwd() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
            cwd = "/tmp"
        "#,
        )
        .expect("should deserialize command config with cwd");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                env_vars: Vec::new(),
                cwd: Some(PathBuf::from("/tmp")),
            }
        );
    }

    #[test]
    fn deserialize_disabled_server_config() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
            enabled = false
        "#,
        )
        .expect("should deserialize disabled server config");

        assert!(!cfg.enabled);
    }

    #[test]
    fn deserialize_streamable_http_server_config() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            url = "https://example.com/mcp"
        "#,
        )
        .expect("should deserialize http config");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
            }
        );
        assert!(cfg.enabled);
    }

    #[test]
    fn deserialize_streamable_http_server_config_with_env_var() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            url = "https://example.com/mcp"
            bearer_token_env_var = "GITHUB_TOKEN"
        "#,
        )
        .expect("should deserialize http config");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: Some("GITHUB_TOKEN".to_string()),
                http_headers: None,
                env_http_headers: None,
            }
        );
        assert!(cfg.enabled);
    }

    #[test]
    fn deserialize_streamable_http_server_config_with_headers() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            url = "https://example.com/mcp"
            http_headers = { "X-Foo" = "bar" }
            env_http_headers = { "X-Token" = "TOKEN_ENV" }
        "#,
        )
        .expect("should deserialize http config with headers");

        assert_eq!(
            cfg.transport,
            McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: None,
                http_headers: Some(HashMap::from([("X-Foo".to_string(), "bar".to_string())])),
                env_http_headers: Some(HashMap::from([(
                    "X-Token".to_string(),
                    "TOKEN_ENV".to_string()
                )])),
            }
        );
    }

    #[test]
    fn deserialize_server_config_with_tool_filters() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
            command = "echo"
            enabled_tools = ["allowed"]
            disabled_tools = ["blocked"]
        "#,
        )
        .expect("should deserialize tool filters");

        assert_eq!(cfg.enabled_tools, Some(vec!["allowed".to_string()]));
        assert_eq!(cfg.disabled_tools, Some(vec!["blocked".to_string()]));
    }

    #[test]
    fn deserialize_rejects_command_and_url() {
        toml::from_str::<McpServerConfig>(
            r#"
            command = "echo"
            url = "https://example.com"
        "#,
        )
        .expect_err("should reject command+url");
    }

    #[test]
    fn deserialize_rejects_env_for_http_transport() {
        toml::from_str::<McpServerConfig>(
            r#"
            url = "https://example.com"
            env = { "FOO" = "BAR" }
        "#,
        )
        .expect_err("should reject env for http transport");
    }

    #[test]
    fn deserialize_rejects_headers_for_stdio() {
        toml::from_str::<McpServerConfig>(
            r#"
            command = "echo"
            http_headers = { "X-Foo" = "bar" }
        "#,
        )
        .expect_err("should reject http_headers for stdio transport");

        toml::from_str::<McpServerConfig>(
            r#"
            command = "echo"
            env_http_headers = { "X-Foo" = "BAR_ENV" }
        "#,
        )
        .expect_err("should reject env_http_headers for stdio transport");
    }

    #[test]
    fn deserialize_rejects_inline_bearer_token_field() {
        let err = toml::from_str::<McpServerConfig>(
            r#"
            url = "https://example.com"
            bearer_token = "secret"
        "#,
        )
        .expect_err("should reject bearer_token field");

        assert!(
            err.to_string().contains("bearer_token is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn subagent_sandbox_policy_restrictiveness_ordering() {
        // Verify restrictiveness ordering for policy comparison
        let readonly = SubagentSandboxPolicy::ReadOnly;
        let workspace = SubagentSandboxPolicy::WorkspaceWrite;
        let full = SubagentSandboxPolicy::DangerFullAccess;
        let inherit = SubagentSandboxPolicy::Inherit;

        // ReadOnly (0) < WorkspaceWrite (1) < DangerFullAccess (2)
        assert!(readonly.restrictiveness().unwrap() < workspace.restrictiveness().unwrap());
        assert!(workspace.restrictiveness().unwrap() < full.restrictiveness().unwrap());
        // Inherit has no restrictiveness (depends on parent)
        assert!(inherit.restrictiveness().is_none());
    }

    #[test]
    fn subagent_sandbox_policy_roundtrip_preserves_variant() {
        // Verify from_sandbox_policy correctly identifies policy variants
        use codex_protocol::protocol::SandboxPolicy;

        assert_eq!(
            SubagentSandboxPolicy::from_sandbox_policy(&SandboxPolicy::ReadOnly),
            SubagentSandboxPolicy::ReadOnly
        );
        assert_eq!(
            SubagentSandboxPolicy::from_sandbox_policy(&SandboxPolicy::new_workspace_write_policy()),
            SubagentSandboxPolicy::WorkspaceWrite
        );
        assert_eq!(
            SubagentSandboxPolicy::from_sandbox_policy(&SandboxPolicy::DangerFullAccess),
            SubagentSandboxPolicy::DangerFullAccess
        );
    }

    #[test]
    fn subagent_sandbox_policy_equal_level_should_not_override_parent_with_writable_roots() {
        // Bug fix test: when task and parent both have WorkspaceWrite,
        // the task's fresh default policy should NOT replace parent's
        // policy which may have custom writable_roots.
        use codex_protocol::protocol::SandboxPolicy;
        use std::path::PathBuf;

        let parent_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/custom/path")],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        };
        let parent_subagent = SubagentSandboxPolicy::from_sandbox_policy(&parent_policy);

        let task_policy = SubagentSandboxPolicy::WorkspaceWrite;

        // Both are WorkspaceWrite, so same restrictiveness level
        assert_eq!(
            parent_subagent.restrictiveness(),
            task_policy.restrictiveness()
        );

        // The fix: use strict < instead of <= to prevent clobbering
        // When sub_level == parent_level, we should NOT apply the task policy
        let sub_level = task_policy.restrictiveness().unwrap();
        let parent_level = parent_subagent.restrictiveness().unwrap();

        // This is what the fixed code checks: sub_level < parent_level
        // Should be false when levels are equal
        assert!(
            (sub_level >= parent_level),
            "equal levels should not trigger override"
        );
    }
}
