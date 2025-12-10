use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use tracing::warn;

use crate::codex::Codex;
use crate::config::profile::ConfigProfile;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub profile: Option<String>,
    pub sandbox_policy: Option<SubagentSandboxPolicy>,
    pub approval_policy: Option<AskForApproval>,
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
        let Some(ref profile_name) = self.profile else {
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
}

impl SubagentSandboxPolicy {
    /// Convert to the full SandboxPolicy enum.
    pub fn to_sandbox_policy(self) -> SandboxPolicy {
        match self {
            SubagentSandboxPolicy::ReadOnly => SandboxPolicy::new_read_only_policy(),
            SubagentSandboxPolicy::WorkspaceWrite => SandboxPolicy::new_workspace_write_policy(),
            SubagentSandboxPolicy::DangerFullAccess => SandboxPolicy::DangerFullAccess,
        }
    }

    /// Returns the restrictiveness level (lower = more restrictive).
    /// Used to ensure subagents can only tighten, not loosen security.
    pub fn restrictiveness(self) -> i32 {
        match self {
            SubagentSandboxPolicy::ReadOnly => 0,
            SubagentSandboxPolicy::WorkspaceWrite => 1,
            SubagentSandboxPolicy::DangerFullAccess => 2,
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

impl SubagentRegistry {
    pub fn new(codex_home: &Path) -> Self {
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

        Self { agents }
    }

    fn load_agent(path: &Path, slug: &str) -> Result<SubagentDefinition> {
        let content = std::fs::read_to_string(path)?;
        let (frontmatter, body) = Self::parse_frontmatter(&content)?;

        let metadata: SubagentMetadata = if !frontmatter.is_empty() {
            serde_yaml::from_str(frontmatter).context("Failed to parse YAML frontmatter")?
        } else {
            SubagentMetadata {
                name: None,
                description: None,
                profile: None,
                sandbox_policy: None,
                approval_policy: None,
                allowed_subagents: None,
                extra: HashMap::new(),
            }
        };

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
