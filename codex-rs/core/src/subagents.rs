use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::codex::Codex;
use crate::config::Config;
use crate::config::types::AgentConfigToml;
use crate::config::types::SubagentReasoningEffort;
use crate::openai_models::model_family::InstructionMode;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use codex_protocol::openai_models::ReasoningEffort;
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
            SubagentApprovalPolicy::Never => Some(-1),
        }
    }
}

/// Model setting for subagents.
///
/// - `inherit`: use the parent session's model/provider.
/// - `{provider}/{model}`: canonical model ID (e.g. `openai/gpt-4o-mini`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentModel {
    /// Inherit the parent session's model/provider settings.
    #[default]
    Inherit,
    /// Use an explicit canonical model ID (`{provider}/{model}`).
    #[serde(untagged)]
    Canonical(String),
}

impl SubagentModel {
    pub fn canonical_model_id(&self) -> Option<&str> {
        match self {
            SubagentModel::Inherit => None,
            SubagentModel::Canonical(canonical_model_id) => Some(canonical_model_id),
        }
    }
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
    #[serde(default)]
    pub model: SubagentModel,
    #[serde(default)]
    pub reasoning_effort: SubagentReasoningEffort,
    pub sandbox_policy: SubagentSandboxPolicy,
    pub approval_policy: SubagentApprovalPolicy,
    /// Whether this agent is internal (not visible to main agent by default).
    /// Internal agents can only be spawned by tool implementations or by agents
    /// that explicitly list them in `allowed_subagents`.
    #[serde(default)]
    pub internal: bool,
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
    /// Returns true if this agent is internal (not visible by default).
    pub fn is_internal(&self) -> bool {
        self.internal
    }

    pub fn is_inherit_reasoning_effort(&self) -> bool {
        self.reasoning_effort == SubagentReasoningEffort::Inherit
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort.to_reasoning_effort()
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(profile) = self.extra.get("profile") {
            let profile = match profile {
                serde_yaml::Value::String(profile) => profile.as_str(),
                _ => {
                    anyhow::bail!(
                        "Subagent frontmatter field 'profile' is no longer supported; use 'model: inherit' or 'model: {{provider}}/{{model}}'."
                    );
                }
            };

            match (&self.model, profile) {
                (SubagentModel::Inherit, "inherit") => {}
                _ => {
                    anyhow::bail!(
                        "Subagent frontmatter field 'profile' is no longer supported; use 'model: inherit' or 'model: {{provider}}/{{model}}'."
                    );
                }
            }
        }

        if let Some(canonical_model_id) = self.model.canonical_model_id() {
            crate::model_provider_info::parse_canonical_model_id(canonical_model_id)?;
        }

        Ok(())
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
    pub instruction_mode: InstructionMode,
}

pub struct SubagentRegistry {
    agents: HashMap<String, SubagentDefinition>,
}

const BUILTIN_AGENTS: &[(&str, &str)] = &[
    ("review", include_str!("subagents/review.md")),
    ("finder", include_str!("subagents/finder.md")),
    ("rush", include_str!("subagents/rush.md")),
    ("oracle", include_str!("subagents/oracle.md")),
    ("general", include_str!("subagents/general.md")),
    ("librarian", include_str!("subagents/librarian.md")),
    ("painter", include_str!("subagents/painter.md")),
    (
        "session_reader",
        include_str!("subagents/session_reader.md"),
    ),
];

/// Load and validate all subagents from the agents directory.
/// Returns both successfully loaded agents and errors for invalid ones.
pub async fn load_subagents(codex_home: &Path, config: &Config) -> SubagentLoadOutcome {
    let started = std::time::Instant::now();
    let mut outcome = SubagentLoadOutcome::default();
    let agents_dir = codex_home.join("agents");

    let mut agents_by_slug: HashMap<String, SubagentDefinition> = HashMap::new();

    for (slug, content) in BUILTIN_AGENTS {
        match SubagentRegistry::load_agent_from_str(slug, content) {
            Ok(agent) => {
                agents_by_slug.insert(slug.to_string(), agent);
            }
            Err(e) => {
                warn!("Failed to load built-in subagent '{slug}': {e}");
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && let Some(slug) = path.file_stem().and_then(|s| s.to_str())
            {
                if agents_by_slug.contains_key(slug) {
                    continue;
                }

                match SubagentRegistry::load_agent(&path, slug) {
                    Ok(agent) => {
                        agents_by_slug.insert(slug.to_string(), agent);
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
    }

    apply_agent_overrides(&mut agents_by_slug, &config.agent);

    outcome.agents = agents_by_slug.into_values().collect();

    info!(
        agents = outcome.agents.len(),
        errors = outcome.errors.len(),
        path = %agents_dir.display(),
        elapsed_ms = started.elapsed().as_millis(),
        "Loaded subagents (async)"
    );

    outcome
}

fn apply_agent_overrides(
    agents: &mut HashMap<String, SubagentDefinition>,
    agent_overrides: &[AgentConfigToml],
) {
    for override_config in agent_overrides {
        if override_config.enabled == Some(false) {
            agents.remove(&override_config.name);
            continue;
        }

        let Some(agent) = agents.get_mut(&override_config.name) else {
            continue;
        };

        if let Some(model) = &override_config.model {
            if model.trim().eq_ignore_ascii_case("inherit") {
                agent.metadata.model = SubagentModel::Inherit;
            } else {
                agent.metadata.model = SubagentModel::Canonical(model.clone());
            }
        }

        if let Some(reasoning_effort) = override_config.reasoning_effort {
            agent.metadata.reasoning_effort = reasoning_effort;
        }
    }
}

impl SubagentRegistry {
    /// Legacy constructor used by tool specs and handlers.
    /// Synchronously loads agents from `~/.codex/agents`, logging but ignoring
    /// any invalid definitions. Always includes embedded built-in agents.
    pub fn new(codex_home: &Path, config: &Config) -> Self {
        Self::new_with_agent_overrides(codex_home, &config.agent)
    }

    pub fn new_with_agent_overrides(
        codex_home: &Path,
        agent_overrides: &[AgentConfigToml],
    ) -> Self {
        let started = std::time::Instant::now();
        let mut agents = HashMap::new();
        let agents_dir = codex_home.join("agents");

        for (slug, content) in BUILTIN_AGENTS {
            match Self::load_agent_from_str(slug, content) {
                Ok(agent) => {
                    agents.insert(slug.to_string(), agent);
                }
                Err(e) => {
                    warn!("Failed to load built-in subagent '{slug}': {e}");
                }
            }
        }

        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "md")
                    && let Some(slug) = path.file_stem().and_then(|s| s.to_str())
                {
                    if agents.contains_key(slug) {
                        continue;
                    }

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

        apply_agent_overrides(&mut agents, agent_overrides);

        debug!(
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
        Self::load_agent_from_str(slug, &content)
    }

    fn load_agent_from_str(slug: &str, content: &str) -> Result<SubagentDefinition> {
        let (frontmatter, body) = Self::parse_frontmatter(content)?;

        anyhow::ensure!(
            !frontmatter.is_empty(),
            "Missing YAML frontmatter. Subagent files require frontmatter with at least 'model', 'sandbox_policy', and 'approval_policy' fields."
        );

        let metadata: SubagentMetadata =
            serde_yaml::from_str(frontmatter).context("Failed to parse YAML frontmatter")?;
        metadata.validate()?;

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

    #[tokio::test]
    async fn loads_builtins_without_agents_dir() {
        let codex_home = tempfile::tempdir().expect("tempdir");

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        assert!(outcome.errors.is_empty(), "Expected no errors");
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
    }

    #[tokio::test]
    async fn loads_valid_agent_with_inherit_model() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "test-agent",
            "---\nname: Test Agent\ndescription: A test agent\nmodel: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nYou are a test agent.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        assert!(outcome.errors.is_empty(), "Expected no errors");
        // Includes user agent + 8 built-in agents
        assert_eq!(outcome.agents.len(), 9);
        assert!(outcome.agents.iter().any(|a| a.slug == "test-agent"));
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
    }

    #[tokio::test]
    async fn loads_valid_agent_with_explicit_model() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "explorer",
            "---\nname: Explorer\ndescription: Fast agent\nmodel: openai/gpt-4o-mini\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are an explorer.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        assert!(outcome.errors.is_empty(), "Expected no errors");
        // Includes user agent + 8 built-in agents
        assert_eq!(outcome.agents.len(), 9);
        assert!(outcome.agents.iter().any(|a| a.slug == "explorer"));
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
    }

    #[tokio::test]
    async fn rejects_agent_with_invalid_model() {
        let codex_home = setup_codex_home();
        let path = write_agent(
            &codex_home,
            "explorer",
            "---\nname: Explorer\ndescription: Fast agent\nmodel: grok-4.1-fast\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nYou are an explorer.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        // Built-in agents should still load successfully
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, path);
        assert!(
            outcome.errors[0]
                .message
                .contains("Invalid model ID 'grok-4.1-fast'"),
            "Error message should mention invalid model: {}",
            outcome.errors[0].message
        );
    }

    #[tokio::test]
    async fn rejects_agent_with_invalid_yaml() {
        let codex_home = setup_codex_home();
        let path = write_agent(
            &codex_home,
            "broken",
            "---\nname: Broken\ninvalid yaml here\nmodel: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nYou are broken.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        // Built-in agents should still load successfully
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
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
            "---\nname: Incomplete\ndescription: Missing required fields\nmodel: inherit\n---\n\nYou are incomplete.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        // Built-in agents should still load successfully
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
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

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        // Built-in agents should still load successfully
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
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
            "---\nmodel: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        assert!(
            outcome.errors.is_empty(),
            "Expected no errors: {:?}",
            outcome.errors
        );
        // Includes user agent + 8 built-in agents
        assert_eq!(outcome.agents.len(), 9);
        let minimal = outcome
            .agents
            .iter()
            .find(|a| a.slug == "minimal")
            .expect("minimal agent");
        assert!(
            minimal.system_prompt.is_empty(),
            "Expected empty system_prompt for frontmatter-only agent"
        );
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
    }

    #[tokio::test]
    async fn handles_empty_agents_directory() {
        let codex_home = setup_codex_home();
        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        assert!(outcome.errors.is_empty());
        // 8 built-in agents are always available
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
    }

    #[tokio::test]
    async fn handles_missing_agents_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Don't create agents dir
        let outcome = load_subagents(temp.path(), &crate::config::test_config()).await;
        assert!(outcome.errors.is_empty());
        // 8 built-in agents are available even if ~/.codex/agents does not exist
        assert_eq!(outcome.agents.len(), 8);
        assert!(outcome.agents.iter().any(|a| a.slug == "review"));
    }

    #[test]
    fn registry_from_outcome_works() {
        let outcome = SubagentLoadOutcome {
            agents: vec![SubagentDefinition {
                slug: "test".to_string(),
                metadata: SubagentMetadata {
                    name: Some("Test".to_string()),
                    description: Some("A test".to_string()),
                    model: SubagentModel::Inherit,
                    reasoning_effort: SubagentReasoningEffort::Inherit,
                    sandbox_policy: SubagentSandboxPolicy::Inherit,
                    approval_policy: SubagentApprovalPolicy::Inherit,
                    allowed_subagents: None,
                    internal: false,
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

#[cfg(test)]
mod internal_visibility_tests {
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

    fn write_agent(codex_home: &TempDir, slug: &str, content: &str) {
        let agents_dir = codex_home.path().join("agents");
        let path = agents_dir.join(format!("{slug}.md"));
        fs::write(&path, content).unwrap();
    }

    #[tokio::test]
    async fn internal_agent_parsed_from_frontmatter() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "internal-test",
            "---\nname: Internal Test\ndescription: An internal agent\ninternal: true\nmodel: inherit\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are an internal test agent.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        let agent = outcome
            .agents
            .iter()
            .find(|a| a.slug == "internal-test")
            .expect("internal-test agent should exist");

        assert!(
            agent.metadata.is_internal(),
            "Agent should be marked internal"
        );
    }

    #[tokio::test]
    async fn public_agent_is_not_internal_by_default() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "public-test",
            "---\nname: Public Test\ndescription: A public agent\nmodel: inherit\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are a public test agent.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        let agent = outcome
            .agents
            .iter()
            .find(|a| a.slug == "public-test")
            .expect("public-test agent should exist");

        assert!(
            !agent.metadata.is_internal(),
            "Agent should not be internal by default"
        );
    }

    #[tokio::test]
    async fn reasoning_effort_parsed_from_frontmatter() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "reasoning-test",
            "---\nname: Reasoning Test\nreasoning_effort: high\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are a test agent.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        let agent = outcome
            .agents
            .iter()
            .find(|a| a.slug == "reasoning-test")
            .expect("reasoning-test agent should exist");

        assert_eq!(
            agent.metadata.reasoning_effort,
            SubagentReasoningEffort::High
        );
        assert_eq!(
            agent.metadata.reasoning_effort(),
            Some(ReasoningEffort::High)
        );
    }

    #[tokio::test]
    async fn reasoning_effort_default_is_inherit() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "inherit-test",
            "---\nname: Inherit Test\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are a test agent.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        let agent = outcome
            .agents
            .iter()
            .find(|a| a.slug == "inherit-test")
            .expect("inherit-test agent should exist");

        assert_eq!(
            agent.metadata.reasoning_effort,
            SubagentReasoningEffort::Inherit
        );
        assert_eq!(agent.metadata.reasoning_effort(), None);
    }

    #[tokio::test]
    async fn reasoning_effort_xhigh_parsed_from_frontmatter() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "xhigh-test",
            "---\nname: XHigh Test\nreasoning_effort: xhigh\nsandbox_policy: read-only\napproval_policy: never\n---\n\nYou are a test agent.",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        let agent = outcome
            .agents
            .iter()
            .find(|a| a.slug == "xhigh-test")
            .expect("xhigh-test agent should exist");

        assert_eq!(
            agent.metadata.reasoning_effort,
            SubagentReasoningEffort::XHigh
        );
        assert_eq!(
            agent.metadata.reasoning_effort(),
            Some(ReasoningEffort::XHigh)
        );
    }

    #[test]
    fn internal_agent_accessible_via_registry_get() {
        let outcome = SubagentLoadOutcome {
            agents: vec![SubagentDefinition {
                slug: "internal-agent".to_string(),
                metadata: SubagentMetadata {
                    name: Some("Internal".to_string()),
                    description: Some("Internal agent".to_string()),
                    internal: true,
                    model: SubagentModel::Inherit,
                    reasoning_effort: SubagentReasoningEffort::Inherit,
                    sandbox_policy: SubagentSandboxPolicy::ReadOnly,
                    approval_policy: SubagentApprovalPolicy::Never,
                    allowed_subagents: Some(vec![]),
                    extra: std::collections::HashMap::new(),
                },
                system_prompt: "Internal prompt".to_string(),
            }],
            errors: vec![],
        };

        let registry = SubagentRegistry::from_outcome(&outcome);

        // Internal agent should be accessible via get()
        let agent = registry.get("internal-agent");
        assert!(
            agent.is_some(),
            "Internal agent should be accessible via get()"
        );
        assert!(agent.unwrap().metadata.is_internal());

        // Internal agent should also appear in list()
        assert_eq!(registry.list().len(), 1);
    }

    #[tokio::test]
    async fn builtin_session_reader_is_internal() {
        let codex_home = setup_codex_home();

        let registry = SubagentRegistry::new(codex_home.path(), &crate::config::test_config());

        let session_reader = registry.get("session_reader");
        assert!(session_reader.is_some(), "session_reader should exist");
        assert!(
            session_reader.unwrap().metadata.is_internal(),
            "session_reader should be marked as internal"
        );
    }

    #[tokio::test]
    async fn skips_disk_agents_that_collide_with_builtins() {
        let codex_home = setup_codex_home();
        write_agent(
            &codex_home,
            "review",
            "---\nname: Override Reviewer\nmodel: inherit\nsandbox_policy: inherit\napproval_policy: inherit\n---\n\nOverride prompt",
        );

        let outcome = load_subagents(codex_home.path(), &crate::config::test_config()).await;
        let review = outcome
            .agents
            .iter()
            .find(|a| a.slug == "review")
            .expect("review agent should exist");

        assert_eq!(review.metadata.name.as_deref(), Some("Code Reviewer"));
    }

    #[tokio::test]
    async fn applies_config_overrides_to_loaded_agents() {
        let codex_home = setup_codex_home();
        let mut config = crate::config::test_config();
        config.agent = vec![
            crate::config::types::AgentConfigToml {
                name: "review".to_string(),
                model: Some("openai/gpt-4o-mini".to_string()),
                reasoning_effort: Some(SubagentReasoningEffort::High),
                enabled: None,
            },
            crate::config::types::AgentConfigToml {
                name: "oracle".to_string(),
                model: None,
                reasoning_effort: None,
                enabled: Some(false),
            },
            crate::config::types::AgentConfigToml {
                name: "unknown".to_string(),
                model: Some("openai/gpt-4o-mini".to_string()),
                reasoning_effort: None,
                enabled: Some(false),
            },
        ];

        let outcome = load_subagents(codex_home.path(), &config).await;
        assert!(!outcome.agents.iter().any(|a| a.slug == "oracle"));

        let review = outcome
            .agents
            .iter()
            .find(|a| a.slug == "review")
            .expect("review agent should exist");

        assert_eq!(
            review.metadata.model,
            SubagentModel::Canonical("openai/gpt-4o-mini".to_string())
        );
        assert_eq!(
            review.metadata.reasoning_effort,
            SubagentReasoningEffort::High
        );
    }
}
