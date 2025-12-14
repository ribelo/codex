use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use tracing::info;
use tracing::warn;

use crate::codex::Codex;
use crate::config::profile::ConfigProfile;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use tokio_util::sync::CancellationToken;

/// Approval policy for subagents, with an additional `Inherit` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
            SubagentApprovalPolicy::UnlessTrusted => Some(0),
            SubagentApprovalPolicy::OnFailure => Some(2),
            SubagentApprovalPolicy::OnRequest => Some(1),
            SubagentApprovalPolicy::Never => Some(3),
        }
    }
}

/// Profile setting for subagents.
/// Either inherits from parent session or uses a named profile from config.toml.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentProfile {
    /// Inherit the parent session's model/provider settings.
    Inherit,
    /// Use a named profile from config.toml.
    #[serde(untagged)]
    Named(String),
}

impl SubagentProfile {
    /// Returns the profile name if not `Inherit`.
    pub fn profile_name(&self) -> Option<&str> {
        match self {
            SubagentProfile::Inherit => None,
            SubagentProfile::Named(name) => Some(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct SubagentLoadOutcome {
    pub agents: Vec<SubagentDefinition>,
    pub errors: Vec<SubagentError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub profile: SubagentProfile,
    pub sandbox_policy: SubagentSandboxPolicy,
    pub approval_policy: SubagentApprovalPolicy,
    /// Optional list of subagent slugs this subagent is allowed to delegate to.
    /// - `None`: full access to all subagents (default for root sessions)
    /// - `Some(vec![])`: no access to any subagents
    /// - `Some(vec!["agent1", "agent2"])`: access only to listed subagents
    #[serde(default)]
    pub allowed_subagents: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

impl SubagentMetadata {
    /// Load the profile configuration from the config file if a profile name is specified.
    /// Uses async I/O to avoid blocking the executor.
    pub async fn load_profile(&self, codex_home: &Path) -> Result<Option<ConfigProfile>> {
        let Some(profile_name) = self.profile.profile_name() else {
            return Ok(None);
        };

        let config_path = codex_home.join("config.toml");
        let content = tokio::fs::read_to_string(&config_path)
            .await
            .context("Failed to read config.toml for subagent profile lookup")?;

        #[derive(Deserialize)]
        struct ProfilesOnly {
            #[serde(default)]
            profiles: HashMap<String, ConfigProfile>,
        }

        let parsed: ProfilesOnly =
            toml::from_str(&content).context("Failed to parse config.toml")?;

        parsed
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Profile '{}' not found in config.toml. Available profiles: {}",
                    profile_name,
                    parsed
                        .profiles
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .map(Some)
    }
}

/// Simplified sandbox policy for subagent frontmatter.
/// Maps to the full SandboxPolicy enum but with simpler serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone)]
pub struct SubagentDefinition {
    pub slug: String,
    pub metadata: SubagentMetadata,
    pub system_prompt: String,
}

pub struct SubagentSession {
    pub codex: Arc<Codex>,
    pub cancellation_token: CancellationToken,
    pub session_id: String,
}

pub struct SubagentRegistry {
    agents: HashMap<String, SubagentDefinition>,
}

/// Load and validate all subagents from the agents directory.
/// Returns both successfully loaded agents and errors for invalid ones.
pub async fn load_subagents(codex_home: &Path) -> SubagentLoadOutcome {
    let started = std::time::Instant::now();
    let mut outcome = SubagentLoadOutcome::default();
    let agents_dir = codex_home.join("agents");

    let entries = match std::fs::read_dir(&agents_dir) {
        Ok(entries) => entries,
        Err(_) => return outcome,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md")
            && let Some(slug) = path.file_stem().and_then(|s| s.to_str())
        {
            match SubagentRegistry::load_agent(&path, slug) {
                Ok(agent) => {
                    // Validate the profile if specified
                    if let Err(e) = agent.metadata.load_profile(codex_home).await {
                        outcome.errors.push(SubagentError {
                            path,
                            message: e.to_string(),
                        });
                    } else {
                        outcome.agents.push(agent);
                    }
                }
                Err(e) => {
                    outcome.errors.push(SubagentError {
                        path,
                        message: e.to_string(),
                    });
                }
            }
        }
    }

    info!(
        agents = outcome.agents.len(),
        errors = outcome.errors.len(),
        path = %agents_dir.display(),
        elapsed_ms = started.elapsed().as_millis(),
        "Loaded subagents (async)"
    );

    outcome
}

impl SubagentRegistry {
    /// Legacy constructor used by tool specs and handlers.
    /// Synchronously loads agents from `~/.codex/agents`, logging but ignoring
    /// any invalid definitions.
    pub fn new(codex_home: &Path) -> Self {
        let started = std::time::Instant::now();
        let mut agents = HashMap::new();
        let agents_dir = codex_home.join("agents");

        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "md")
                    && let Some(slug) = path.file_stem().and_then(|s| s.to_str())
                {
                    match Self::load_agent(&path, slug) {
                        Ok(agent) => {
                            agents.insert(slug.to_string(), agent);
                        }
                        Err(e) => {
                            warn!("Failed to load subagent from {}: {}", path.display(), e);
                        }
                    }
                }
            }
        }

        info!(
            agents = agents.len(),
            path = %agents_dir.display(),
            elapsed_ms = started.elapsed().as_millis(),
            "Loaded subagents registry (sync)"
        );

        Self { agents }
    }

    /// Create a registry from a pre-validated load outcome.
    pub fn from_outcome(outcome: &SubagentLoadOutcome) -> Self {
        let mut agents = HashMap::new();
        for agent in &outcome.agents {
            agents.insert(agent.slug.clone(), agent.clone());
        }
        Self { agents }
    }

    fn load_agent(path: &Path, slug: &str) -> Result<SubagentDefinition> {
        let content = std::fs::read_to_string(path)?;
        let (frontmatter, body) = Self::parse_frontmatter(&content)?;

        anyhow::ensure!(
            !frontmatter.is_empty(),
            "Missing YAML frontmatter. Subagent files require frontmatter with at least 'profile', 'sandbox_policy', and 'approval_policy' fields."
        );

        let metadata: SubagentMetadata =
            serde_yaml::from_str(frontmatter).context("Failed to parse YAML frontmatter")?;

        Ok(SubagentDefinition {
            slug: slug.to_string(),
            metadata,
            system_prompt: body.trim().to_string(),
        })
    }

    fn parse_frontmatter(content: &str) -> Result<(&str, &str)> {
        if content.starts_with("---\n")
            && let Some(end) = content[4..].find("\n---\n")
        {
            let frontmatter = &content[4..4 + end];
            let body = &content[4 + end + 5..];
            return Ok((frontmatter, body));
        }
        // Fallback if no frontmatter
        Ok(("", content))
    }

    pub fn get(&self, slug: &str) -> Option<&SubagentDefinition> {
        self.agents.get(slug)
    }

    pub fn list(&self) -> Vec<&SubagentDefinition> {
        self.agents.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    fn setup_codex_home() -> TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        let agents_dir = temp.path().join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        temp
    }

    fn write_agent(codex_home: &TempDir, slug: &str, content: &str) -> PathBuf {
        let agents_dir = codex_home.path().join("agents");
        let path = agents_dir.join(format!("{slug}.md"));
        fs::write(&path, content).unwrap();
        path
    }

    fn write_config(codex_home: &TempDir, profiles: &[(&str, &str)]) {
        let config_content = if profiles.is_empty() {
            String::new()
        } else {
            let mut profiles_toml = String::from("[profiles]\n");
            for (name, model) in profiles {
                profiles_toml.push_str(&format!("[profiles.{name}]\n"));
                profiles_toml.push_str(&format!("model = \"{model}\"\n"));
            }
            profiles_toml
        };
        fs::write(codex_home.path().join("config.toml"), config_content).unwrap();
    }

    #[tokio::test]
    async fn loads_valid_agent_without_profile() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "test-agent",
            "---\nname: Test Agent\ndescription: A test agent\nprofile: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nYou are a test agent.",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert!(outcome.errors.is_empty(), "Expected no errors");
        assert_eq!(outcome.agents.len(), 1);
        assert_eq!(outcome.agents[0].slug, "test-agent");
    }

    #[tokio::test]
    async fn loads_valid_agent_with_valid_profile() {
        let codex_home = setup_codex_home();
        write_config(&codex_home, &[("fast", "gpt-4o-mini")]);
        write_agent(
            &codex_home,
            "explorer",
            "---\nname: Explorer\ndescription: Fast agent\nprofile: fast\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are an explorer.",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert!(outcome.errors.is_empty(), "Expected no errors");
        assert_eq!(outcome.agents.len(), 1);
        assert_eq!(outcome.agents[0].slug, "explorer");
    }

    #[tokio::test]
    async fn rejects_agent_with_invalid_profile() {
        let codex_home = setup_codex_home();
        write_config(&codex_home, &[("fast", "gpt-4o-mini")]);
        let path = write_agent(
            &codex_home,
            "explorer",
            "---\nname: Explorer\ndescription: Fast agent\nprofile: x-ai/grok-4.1-fast\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nYou are an explorer.",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert_eq!(outcome.agents.len(), 0, "Should reject invalid agent");
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, path);
        assert!(
            outcome.errors[0]
                .message
                .contains("Profile 'x-ai/grok-4.1-fast' not found"),
            "Error message should mention missing profile: {}",
            outcome.errors[0].message
        );
        assert!(
            outcome.errors[0]
                .message
                .contains("Available profiles: fast"),
            "Error message should list available profiles: {}",
            outcome.errors[0].message
        );
    }

    #[tokio::test]
    async fn rejects_agent_with_invalid_yaml() {
        let codex_home = setup_codex_home();
        let path = write_agent(
            &codex_home,
            "broken",
            "---\nname: Broken\ninvalid yaml here\nprofile: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nYou are broken.",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert_eq!(outcome.agents.len(), 0);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, path);
        assert!(
            outcome.errors[0].message.contains("parse")
                || outcome.errors[0].message.contains("YAML"),
            "Error should mention YAML parsing issue: {}",
            outcome.errors[0].message
        );
    }

    #[tokio::test]
    async fn rejects_agent_missing_required_fields() {
        let codex_home = setup_codex_home();
        let path = write_agent(
            &codex_home,
            "incomplete",
            "---\nname: Incomplete\ndescription: Missing required fields\nprofile: inherit\n---\n\nYou are incomplete.",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert_eq!(outcome.agents.len(), 0);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, path);
        // The error message should indicate a YAML parsing issue due to missing required fields.
        // serde_yaml will report "missing field" for the required sandbox_policy/approval_policy.
        assert!(
            outcome.errors[0].message.contains("YAML")
                || outcome.errors[0].message.contains("parse")
                || outcome.errors[0].message.contains("missing field"),
            "Error should mention parsing issue or missing field: {}",
            outcome.errors[0].message
        );
    }

    #[tokio::test]
    async fn rejects_agent_without_frontmatter() {
        let codex_home = setup_codex_home();
        let path = write_agent(
            &codex_home,
            "no-frontmatter",
            "You are an agent without frontmatter.",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert_eq!(outcome.agents.len(), 0);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, path);
        assert!(
            outcome.errors[0].message.contains("frontmatter"),
            "Error should mention missing frontmatter: {}",
            outcome.errors[0].message
        );
    }

    #[tokio::test]
    async fn loads_agent_with_frontmatter_only() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "minimal",
            "---\nprofile: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n",
        );

        let outcome = load_subagents(codex_home.path()).await;
        assert!(
            outcome.errors.is_empty(),
            "Expected no errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.agents.len(), 1);
        assert_eq!(outcome.agents[0].slug, "minimal");
        assert!(
            outcome.agents[0].system_prompt.is_empty(),
            "Expected empty system_prompt for frontmatter-only agent"
        );
    }

    #[tokio::test]
    async fn handles_empty_agents_directory() {
        let codex_home = setup_codex_home();
        let outcome = load_subagents(codex_home.path()).await;
        assert!(outcome.errors.is_empty());
        assert!(outcome.agents.is_empty());
    }

    #[tokio::test]
    async fn handles_missing_agents_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Don't create agents dir
        let outcome = load_subagents(temp.path()).await;
        assert!(outcome.errors.is_empty());
        assert!(outcome.agents.is_empty());
    }

    #[test]
    fn registry_from_outcome_works() {
        let outcome = SubagentLoadOutcome {
            agents: vec![SubagentDefinition {
                slug: "test".to_string(),
                metadata: SubagentMetadata {
                    name: Some("Test".to_string()),
                    description: Some("A test".to_string()),
                    profile: SubagentProfile::Inherit,
                    sandbox_policy: SubagentSandboxPolicy::Inherit,
                    approval_policy: SubagentApprovalPolicy::Inherit,
                    allowed_subagents: None,
                    extra: HashMap::new(),
                },
                system_prompt: "Test prompt".to_string(),
            }],
            errors: vec![],
        };

        let registry = SubagentRegistry::from_outcome(&outcome);
        assert!(registry.get("test").is_some());
        assert_eq!(registry.list().len(), 1);
    }
}
