use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use tracing::warn;

use crate::codex::Codex;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
pub struct SubagentMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub model: Option<String>,
    pub sandbox_policy: Option<SubagentSandboxPolicy>,
    pub approval_policy: Option<AskForApproval>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
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
                model: None,
                sandbox_policy: None,
                approval_policy: None,
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
