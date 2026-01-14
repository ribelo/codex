use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::Config;
use crate::config::types::SubagentApprovalPolicy;
use crate::config::types::SubagentReasoningEffort;
use crate::config::types::SubagentSandboxPolicy;
use crate::config::types::TaskConfigToml;
use crate::config::types::TaskDifficulty;
use crate::skills::system::system_cache_root_dir;
use serde::Deserialize;
use tracing::error;

/// Built-in task definitions.
///
/// - `description`: Short text shown in tool listings
/// - `system_prompt`: Full prompt used when building the worker system prompt (None = use description)
pub(crate) struct BuiltinTask {
    pub task_type: &'static str,
    pub description: &'static str,
    pub sandbox_policy: SubagentSandboxPolicy,
    pub system_prompt: Option<&'static str>,
    pub skills: &'static [&'static str],
}

/// System prompt for the `review` task.
pub(crate) const REVIEW_SYSTEM_PROMPT: &str = include_str!("subagents/review.md");

/// System prompt for the `search` task.
pub(crate) const FINDER_SYSTEM_PROMPT: &str = include_str!("subagents/finder.md");

pub(crate) const BUILTIN_TASKS: &[BuiltinTask] = &[
    BuiltinTask {
        task_type: "code",
        description: "General code changes and implementation work",
        sandbox_policy: SubagentSandboxPolicy::WorkspaceWrite,
        system_prompt: None,
        skills: &[],
    },
    BuiltinTask {
        task_type: "search",
        description: "Find code, definitions, and references",
        sandbox_policy: SubagentSandboxPolicy::ReadOnly,
        system_prompt: Some(FINDER_SYSTEM_PROMPT),
        skills: &[],
    },
    BuiltinTask {
        task_type: "planning",
        description: "Architecture and design decisions",
        sandbox_policy: SubagentSandboxPolicy::ReadOnly,
        system_prompt: None,
        skills: &[],
    },
    BuiltinTask {
        task_type: "review",
        description: "Code review and feedback",
        sandbox_policy: SubagentSandboxPolicy::ReadOnly,
        system_prompt: Some(REVIEW_SYSTEM_PROMPT),
        skills: &["review"],
    },
    BuiltinTask {
        task_type: "ui",
        description: "Frontend and UI work",
        sandbox_policy: SubagentSandboxPolicy::WorkspaceWrite,
        system_prompt: None,
        skills: &[],
    },
    BuiltinTask {
        task_type: "rust",
        description: "Rust code changes",
        sandbox_policy: SubagentSandboxPolicy::WorkspaceWrite,
        system_prompt: None,
        skills: &[],
    },
];

#[derive(Debug, Clone, Default)]
pub struct TaskStrategy {
    pub model: Option<String>,
    pub reasoning_effort: Option<SubagentReasoningEffort>,
}

#[derive(Debug, Clone)]
pub struct TaskDefinition {
    pub task_type: String,
    pub description: Option<String>,
    /// System prompt used when building the worker prompt.
    /// If None, falls back to description for config-defined tasks.
    /// For built-in tasks, this is set from the BUILTIN_TASKS system_prompt field.
    pub system_prompt: Option<String>,
    pub skills: Vec<String>,
    pub default_strategy: TaskStrategy,
    pub difficulty_overrides: HashMap<TaskDifficulty, TaskStrategy>,
    pub sandbox_policy: SubagentSandboxPolicy,
    pub approval_policy: SubagentApprovalPolicy,
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub path: PathBuf,
    pub contents: String,
}

#[derive(Debug, Clone)]
pub struct TaskRegistry {
    tasks: HashMap<String, TaskDefinition>,
    skills_paths: Vec<PathBuf>,
}

impl TaskRegistry {
    pub fn new(config: &Config) -> Self {
        let mut tasks = HashMap::new();

        for builtin in BUILTIN_TASKS {
            tasks.insert(
                builtin.task_type.to_string(),
                TaskDefinition {
                    task_type: builtin.task_type.to_string(),
                    description: Some(builtin.description.to_string()),
                    system_prompt: builtin.system_prompt.map(ToString::to_string),
                    skills: builtin.skills.iter().map(|s| (*s).to_string()).collect(),
                    default_strategy: TaskStrategy::default(),
                    difficulty_overrides: HashMap::new(),
                    sandbox_policy: builtin.sandbox_policy,
                    approval_policy: SubagentApprovalPolicy::Inherit,
                },
            );
        }

        for task in &config.tasks {
            // Merge config overrides with existing builtin, if any
            if let Some(existing) = tasks.get(&task.task_type) {
                tasks.insert(task.task_type.clone(), existing.merge_toml(task));
            } else {
                tasks.insert(task.task_type.clone(), TaskDefinition::from_toml(task));
            }
        }

        Self {
            tasks,
            skills_paths: vec![
                config.cwd.join(".codex/skills"),
                config.codex_home.join("skills"),
                system_cache_root_dir(&config.codex_home),
            ],
        }
    }

    pub fn get(&self, task_type: &str) -> Option<&TaskDefinition> {
        self.tasks.get(task_type)
    }

    pub fn list(&self) -> Vec<&TaskDefinition> {
        let mut tasks: Vec<&TaskDefinition> = self.tasks.values().collect();
        tasks.sort_by(|a, b| a.task_type.cmp(&b.task_type));
        tasks
    }

    pub fn resolve_strategy(
        &self,
        task_type: &str,
        difficulty: TaskDifficulty,
    ) -> Option<TaskStrategy> {
        let task = self.get(task_type)?;
        let mut strategy = task.default_strategy.clone();
        if let Some(override_strategy) = task.difficulty_overrides.get(&difficulty) {
            if override_strategy.model.is_some() {
                strategy.model = override_strategy.model.clone();
            }
            if override_strategy.reasoning_effort.is_some() {
                strategy.reasoning_effort = override_strategy.reasoning_effort;
            }
        }
        Some(strategy)
    }

    pub fn load_skill(&self, name: &str) -> Option<LoadedSkill> {
        if let Some(skill) = self.load_internal_system_skill(name) {
            return Some(skill);
        }

        for base in &self.skills_paths {
            let candidates = [
                base.join(name).join("SKILL.md"),
                base.join(format!("{name}.md")),
            ];

            for path in candidates {
                if path.is_file() {
                    let contents = std::fs::read_to_string(&path).ok()?;
                    return Some(LoadedSkill {
                        name: name.to_string(),
                        path,
                        contents,
                    });
                }
            }
        }

        None
    }

    fn load_internal_system_skill(&self, name: &str) -> Option<LoadedSkill> {
        let system_root = self
            .skills_paths
            .iter()
            .find(|path| path.file_name().is_some_and(|name| name == ".system"))?;
        let path = system_root.join(name).join("SKILL.md");
        if !path.is_file() {
            return None;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) => {
                error!(
                    "Failed to read internal system skill {name} at {}: {err:#}",
                    path.display()
                );
                return None;
            }
        };

        let Some(frontmatter) = extract_frontmatter(&contents) else {
            error!(
                "Missing YAML frontmatter for internal system skill {name} at {}",
                path.display()
            );
            return None;
        };

        let parsed: SkillInternalFrontmatter = match serde_yaml::from_str(&frontmatter) {
            Ok(parsed) => parsed,
            Err(err) => {
                error!(
                    "Failed to parse YAML frontmatter for internal system skill {name} at {}: {err:#}",
                    path.display()
                );
                return None;
            }
        };
        if !parsed.internal {
            return None;
        }

        Some(LoadedSkill {
            name: name.to_string(),
            path,
            contents,
        })
    }

    pub fn skills_paths(&self) -> &[PathBuf] {
        &self.skills_paths
    }

    pub fn available_task_types(&self, allowed: Option<&[String]>) -> Vec<&TaskDefinition> {
        let mut tasks: Vec<&TaskDefinition> = self
            .tasks
            .values()
            .filter(|task| allowed.is_none_or(|allowed| allowed.contains(&task.task_type)))
            .collect();
        tasks.sort_by(|a, b| a.task_type.cmp(&b.task_type));
        tasks
    }
}

#[derive(Debug, Deserialize)]
struct SkillInternalFrontmatter {
    #[serde(default)]
    internal: bool,
}

fn extract_frontmatter(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    if !matches!(lines.next(), Some(line) if line.trim() == "---") {
        return None;
    }

    let mut frontmatter_lines: Vec<&str> = Vec::new();
    let mut found_closing = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            found_closing = true;
            break;
        }
        frontmatter_lines.push(line);
    }

    if frontmatter_lines.is_empty() || !found_closing {
        return None;
    }

    Some(frontmatter_lines.join("\n"))
}

impl TaskDefinition {
    /// Effective system prompt used when building the worker system prompt.
    ///
    /// If a built-in `system_prompt` is present, it is appended after the (short)
    /// `description` so the worker sees both the task summary and the detailed
    /// template instructions.
    pub fn effective_system_prompt(&self) -> Option<String> {
        let description = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let system_prompt = self
            .system_prompt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match (description, system_prompt) {
            (Some(description), Some(system_prompt)) => {
                Some(format!("{description}\n\n{system_prompt}"))
            }
            (Some(description), None) => Some(description.to_string()),
            (None, Some(system_prompt)) => Some(system_prompt.to_string()),
            (None, None) => None,
        }
    }

    pub fn from_toml(task: &TaskConfigToml) -> Self {
        let mut difficulty_overrides = HashMap::new();
        if let Some(difficulty) = &task.difficulty {
            for (k, v) in difficulty {
                difficulty_overrides.insert(
                    *k,
                    TaskStrategy {
                        model: v.model.clone(),
                        reasoning_effort: v.reasoning_effort,
                    },
                );
            }
        }

        Self {
            task_type: task.task_type.clone(),
            description: task.description.clone(),
            // Config tasks use description as system_prompt (no separate field in TOML)
            system_prompt: None,
            skills: task.skills.clone().unwrap_or_default(),
            default_strategy: TaskStrategy {
                model: task.model.clone(),
                reasoning_effort: task.reasoning_effort,
            },
            difficulty_overrides,
            sandbox_policy: task
                .sandbox_policy
                .unwrap_or(SubagentSandboxPolicy::Inherit),
            approval_policy: task
                .approval_policy
                .unwrap_or(SubagentApprovalPolicy::Inherit),
        }
    }

    /// Merge TOML config overrides onto this definition, preserving unspecified fields.
    pub fn merge_toml(&self, task: &TaskConfigToml) -> Self {
        // Merge difficulty overrides: start with existing, overlay config values
        let mut difficulty_overrides = self.difficulty_overrides.clone();
        if let Some(difficulty) = &task.difficulty {
            for (k, v) in difficulty {
                let existing = difficulty_overrides.get(k).cloned().unwrap_or_default();
                difficulty_overrides.insert(
                    *k,
                    TaskStrategy {
                        model: v.model.clone().or(existing.model),
                        reasoning_effort: v.reasoning_effort.or(existing.reasoning_effort),
                    },
                );
            }
        }

        let mut skills = task.skills.clone().unwrap_or_else(|| self.skills.clone());
        if self.task_type == "review" && !skills.iter().any(|skill| skill == "review") {
            skills.push("review".to_string());
        }

        Self {
            task_type: task.task_type.clone(),
            // Override description only if provided in config
            description: task
                .description
                .clone()
                .or_else(|| self.description.clone()),
            // Config tasks don't override system_prompt; preserve builtin if present
            system_prompt: self.system_prompt.clone(),
            // Override skills only if provided in config
            skills,
            // Merge default strategy: overlay config values onto existing
            default_strategy: TaskStrategy {
                model: task
                    .model
                    .clone()
                    .or_else(|| self.default_strategy.model.clone()),
                reasoning_effort: task
                    .reasoning_effort
                    .or(self.default_strategy.reasoning_effort),
            },
            difficulty_overrides,
            // Override sandbox_policy only if explicitly set in config
            sandbox_policy: task.sandbox_policy.unwrap_or(self.sandbox_policy),
            // Override approval_policy only if explicitly set in config
            approval_policy: task.approval_policy.unwrap_or(self.approval_policy),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use pretty_assertions::assert_eq;

    use crate::config::test_config;
    use crate::config::types::SubagentApprovalPolicy;
    use crate::config::types::SubagentReasoningEffort;
    use crate::config::types::SubagentSandboxPolicy;
    use crate::config::types::TaskConfigToml;
    use crate::config::types::TaskDifficulty;
    use crate::config::types::TaskDifficultyStrategy;
    use crate::skills::system::system_cache_root_dir;

    use super::TaskRegistry;

    #[test]
    fn task_registry_includes_builtins() {
        let config = test_config();
        let registry = TaskRegistry::new(&config);

        assert!(registry.get("code").is_some());
        assert!(registry.get("search").is_some());
        assert!(registry.get("review").is_some());
    }

    #[test]
    fn config_task_overrides_builtin_task() {
        let mut config = test_config();
        config.tasks = vec![TaskConfigToml {
            task_type: "search".to_string(),
            description: Some("Custom search prompt".to_string()),
            skills: None,
            model: None,
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        assert_eq!(
            registry.get("search").unwrap().description.as_deref(),
            Some("Custom search prompt")
        );
    }

    #[test]
    fn task_registry_applies_difficulty_override() {
        let mut config = test_config();
        let mut difficulty = HashMap::new();
        difficulty.insert(
            TaskDifficulty::Large,
            TaskDifficultyStrategy {
                model: Some("openai/test-gpt-5.1-codex-large".to_string()),
                reasoning_effort: Some(SubagentReasoningEffort::High),
            },
        );
        config.tasks = vec![TaskConfigToml {
            task_type: "rust".to_string(),
            description: Some("Rust changes".to_string()),
            skills: None,
            model: Some("openai/test-gpt-5.1-codex-medium".to_string()),
            reasoning_effort: Some(SubagentReasoningEffort::Low),
            sandbox_policy: None,
            approval_policy: None,
            difficulty: Some(difficulty),
        }];

        let registry = TaskRegistry::new(&config);
        let large = registry
            .resolve_strategy("rust", TaskDifficulty::Large)
            .expect("strategy");

        assert_eq!(
            large.model.as_deref(),
            Some("openai/test-gpt-5.1-codex-large")
        );
        assert_eq!(large.reasoning_effort, Some(SubagentReasoningEffort::High));
    }

    #[test]
    fn config_partial_override_preserves_builtin_defaults() {
        // Bug fix test: partial config overrides should merge, not replace
        let mut config = test_config();
        // Only override description; all other builtin fields should be preserved
        config.tasks = vec![TaskConfigToml {
            task_type: "search".to_string(),
            description: Some("Custom search description".to_string()),
            skills: None, // Not specified - should preserve builtin (empty vec)
            model: None,
            reasoning_effort: None,
            sandbox_policy: None, // Not specified - should preserve builtin (ReadOnly)
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("search").expect("search task should exist");

        // Description should be overridden
        assert_eq!(
            task.description.as_deref(),
            Some("Custom search description")
        );
        // Sandbox policy should be preserved from builtin (ReadOnly for search)
        assert_eq!(task.sandbox_policy, SubagentSandboxPolicy::ReadOnly);
        // Approval policy should be preserved (Inherit is the builtin default)
        assert_eq!(task.approval_policy, SubagentApprovalPolicy::Inherit);
    }

    #[test]
    fn config_explicit_sandbox_policy_overrides_builtin() {
        let mut config = test_config();
        // Explicitly set sandbox_policy - should override
        config.tasks = vec![TaskConfigToml {
            task_type: "search".to_string(),
            description: None, // Should preserve builtin description
            skills: None,
            model: None,
            reasoning_effort: None,
            sandbox_policy: Some(SubagentSandboxPolicy::WorkspaceWrite),
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("search").expect("search task should exist");

        // Description should be preserved from builtin
        assert_eq!(
            task.description.as_deref(),
            Some("Find code, definitions, and references")
        );
        // Sandbox policy should be overridden
        assert_eq!(task.sandbox_policy, SubagentSandboxPolicy::WorkspaceWrite);
    }

    #[test]
    fn config_skills_override_builtin_skills() {
        let mut config = test_config();
        config.tasks = vec![TaskConfigToml {
            task_type: "code".to_string(),
            description: None,
            skills: Some(vec!["rust".to_string(), "testing".to_string()]),
            model: None,
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("code").expect("code task should exist");

        assert_eq!(task.skills, vec!["rust".to_string(), "testing".to_string()]);
        // Other fields preserved
        assert_eq!(task.sandbox_policy, SubagentSandboxPolicy::WorkspaceWrite);
    }

    #[test]
    fn config_difficulty_overrides_merge_with_builtin() {
        let mut config = test_config();
        let mut difficulty = HashMap::new();
        difficulty.insert(
            TaskDifficulty::Large,
            TaskDifficultyStrategy {
                model: Some("openai/large-model".to_string()),
                reasoning_effort: None,
            },
        );
        config.tasks = vec![TaskConfigToml {
            task_type: "code".to_string(),
            description: None,
            skills: None,
            model: Some("openai/default-model".to_string()),
            reasoning_effort: Some(SubagentReasoningEffort::Medium),
            sandbox_policy: None,
            approval_policy: None,
            difficulty: Some(difficulty),
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("code").expect("code task should exist");

        // Default strategy should be set from config
        assert_eq!(
            task.default_strategy.model.as_deref(),
            Some("openai/default-model")
        );
        assert_eq!(
            task.default_strategy.reasoning_effort,
            Some(SubagentReasoningEffort::Medium)
        );

        // Difficulty override should be set
        let large = task.difficulty_overrides.get(&TaskDifficulty::Large);
        assert!(large.is_some());
        assert_eq!(large.unwrap().model.as_deref(), Some("openai/large-model"));

        // Builtin description should be preserved
        assert_eq!(
            task.description.as_deref(),
            Some("General code changes and implementation work")
        );
    }

    #[test]
    fn builtin_review_task_has_system_prompt() {
        let config = test_config();
        let registry = TaskRegistry::new(&config);
        let task = registry.get("review").expect("review task should exist");

        // Description should be the short version
        assert_eq!(
            task.description.as_deref(),
            Some("Code review and feedback")
        );

        // system_prompt should contain the full review.md content
        assert!(
            task.system_prompt.is_some(),
            "review task should have a system_prompt"
        );
        let system_prompt = task.system_prompt.as_deref().unwrap();
        assert!(
            system_prompt.contains("Review guidelines"),
            "review system_prompt should contain review guidelines"
        );
        assert!(
            system_prompt.contains("FORMATTING GUIDELINES"),
            "review system_prompt should contain formatting guidelines"
        );

        let effective = task
            .effective_system_prompt()
            .expect("review should have an effective system prompt");
        assert!(effective.contains("Code review and feedback"));
        assert!(effective.contains("Review guidelines"));
    }

    #[test]
    fn builtin_search_task_has_system_prompt() {
        let config = test_config();
        let registry = TaskRegistry::new(&config);
        let task = registry.get("search").expect("search task should exist");

        // Description should be the short version
        assert_eq!(
            task.description.as_deref(),
            Some("Find code, definitions, and references")
        );

        // system_prompt should contain the full finder.md content
        assert!(
            task.system_prompt.is_some(),
            "search task should have a system_prompt"
        );
        let system_prompt = task.system_prompt.as_deref().unwrap();
        assert!(
            system_prompt.contains("code search agent"),
            "search system_prompt should describe a code search agent"
        );

        let effective = task
            .effective_system_prompt()
            .expect("search should have an effective system prompt");
        assert!(effective.contains("Find code, definitions, and references"));
        assert!(effective.contains("code search agent"));
    }

    #[test]
    fn builtin_code_task_has_no_system_prompt() {
        let config = test_config();
        let registry = TaskRegistry::new(&config);
        let task = registry.get("code").expect("code task should exist");

        // system_prompt should be None for tasks without special prompts
        assert!(
            task.system_prompt.is_none(),
            "code task should not have a separate system_prompt"
        );

        // effective_system_prompt should fall back to description
        assert_eq!(
            task.effective_system_prompt().as_deref(),
            Some("General code changes and implementation work")
        );
    }

    #[test]
    fn config_override_preserves_builtin_system_prompt() {
        let mut config = test_config();
        // Override only description; system_prompt should be preserved from builtin
        config.tasks = vec![TaskConfigToml {
            task_type: "review".to_string(),
            description: Some("Custom review description".to_string()),
            skills: None,
            model: None,
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("review").expect("review task should exist");

        // Description should be overridden
        assert_eq!(
            task.description.as_deref(),
            Some("Custom review description")
        );

        // But system_prompt should still be the original review.md content
        assert!(task.system_prompt.is_some());
        let system_prompt = task.system_prompt.as_deref().unwrap();
        assert!(system_prompt.contains("Review guidelines"));

        let effective = task
            .effective_system_prompt()
            .expect("review should have an effective system prompt");
        assert!(effective.contains("Custom review description"));
        assert!(effective.contains("Review guidelines"));
    }

    #[test]
    fn config_task_uses_description_as_effective_system_prompt() {
        let mut config = test_config();
        // Create a new config-only task (not a builtin)
        config.tasks = vec![TaskConfigToml {
            task_type: "custom".to_string(),
            description: Some("My custom task description".to_string()),
            skills: None,
            model: None,
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("custom").expect("custom task should exist");

        // Config tasks have no system_prompt
        assert!(task.system_prompt.is_none());

        // effective_system_prompt should fall back to description
        assert_eq!(
            task.effective_system_prompt().as_deref(),
            Some("My custom task description")
        );
    }

    #[test]
    fn internal_system_skill_cannot_be_shadowed_in_task_registry_load_skill() {
        let config = test_config();

        let user_skill_dir = config.codex_home.join("skills/review");
        fs::create_dir_all(&user_skill_dir).unwrap();
        fs::write(
            user_skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: user override\n---\n\nUser body\n",
        )
        .unwrap();

        let internal_skill_dir = system_cache_root_dir(&config.codex_home).join("review");
        fs::create_dir_all(&internal_skill_dir).unwrap();
        fs::write(
            internal_skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: internal\ninternal: true\n---\n\nInternal body\n",
        )
        .unwrap();

        let registry = TaskRegistry::new(&config);
        let loaded = registry
            .load_skill("review")
            .expect("review skill should load");

        let path_str = loaded.path.to_string_lossy().replace('\\', "/");
        assert!(
            path_str.contains("/skills/.system/review/SKILL.md"),
            "expected internal system skill path, got {path_str}"
        );
    }

    #[test]
    fn config_override_cannot_remove_review_skill() {
        let mut config = test_config();
        config.tasks = vec![TaskConfigToml {
            task_type: "review".to_string(),
            description: None,
            skills: Some(Vec::new()),
            model: None,
            reasoning_effort: None,
            sandbox_policy: None,
            approval_policy: None,
            difficulty: None,
        }];

        let registry = TaskRegistry::new(&config);
        let task = registry.get("review").expect("review task should exist");

        assert!(
            task.skills.iter().any(|skill| skill == "review"),
            "expected review task to always include the review contract skill"
        );
    }
}
