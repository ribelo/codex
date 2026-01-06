//! Manager for project documentation (AGENTS.md) with mtime-based caching.
//!
//! Similar to `SkillsManager`, this caches user instructions and invalidates
//! the cache when any AGENTS.md file is modified.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use crate::config::Config;
use crate::project_doc::discover_project_doc_paths;
use crate::project_doc::get_user_instructions;
use crate::skills::SkillMetadata;

#[derive(Clone)]
struct CachedProjectDocs {
    user_instructions: Option<String>,
    /// (file_path, mtime) for each AGENTS.md found
    file_states: Vec<(PathBuf, Option<SystemTime>)>,
}

/// Manages project documentation discovery and caching.
///
/// Results are cached by working directory and invalidated when any
/// AGENTS.md file's mtime changes.
pub struct ProjectDocManager {
    cache_by_cwd: RwLock<HashMap<PathBuf, CachedProjectDocs>>,
}

impl Default for ProjectDocManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectDocManager {
    pub fn new() -> Self {
        Self {
            cache_by_cwd: RwLock::new(HashMap::new()),
        }
    }

    /// Returns user instructions, reloading if AGENTS.md files changed.
    pub async fn get_user_instructions(
        &self,
        config: &Config,
        skills: Option<&[SkillMetadata]>,
    ) -> Option<String> {
        let cwd = &config.cwd;

        // Discover current AGENTS.md paths
        let current_paths = match discover_project_doc_paths(config) {
            Ok(paths) => paths,
            Err(_) => {
                return get_user_instructions(config, skills).await;
            }
        };

        // Get current mtimes
        let current_states: Vec<(PathBuf, Option<SystemTime>)> = current_paths
            .iter()
            .map(|p| {
                (
                    p.clone(),
                    std::fs::metadata(p).and_then(|m| m.modified()).ok(),
                )
            })
            .collect();

        // Check cache
        let cached_entry = match self.cache_by_cwd.read() {
            Ok(cache) => cache.get(cwd).cloned(),
            Err(err) => err.into_inner().get(cwd).cloned(),
        };

        if let Some(cached) = cached_entry {
            // Compare file states (both paths and mtimes)
            if cached.file_states == current_states {
                return cached.user_instructions;
            }
        }

        // Cache miss or stale - reload
        let user_instructions = get_user_instructions(config, skills).await;

        // Update cache
        let new_entry = CachedProjectDocs {
            user_instructions: user_instructions.clone(),
            file_states: current_states,
        };

        match self.cache_by_cwd.write() {
            Ok(mut cache) => {
                cache.insert(cwd.clone(), new_entry);
            }
            Err(err) => {
                err.into_inner().insert(cwd.clone(), new_entry);
            }
        }

        user_instructions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigOverrides;
    use crate::config::ConfigToml;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn test_config(cwd: &Path) -> Config {
        let config_toml = ConfigToml {
            model_context_window: Some(128_000),
            ..Default::default()
        };
        let mut config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            cwd.to_path_buf(),
        )
        .expect("load test config");
        config.cwd = cwd.to_path_buf();
        config.user_instructions = None;
        config
    }

    #[tokio::test]
    async fn cache_returns_same_on_unchanged() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "test instructions").unwrap();

        // Initialize git repo so discovery works
        fs::create_dir(tmp.path().join(".git")).unwrap();

        let manager = ProjectDocManager::new();
        let config = test_config(tmp.path());

        let first = manager.get_user_instructions(&config, None).await;
        let second = manager.get_user_instructions(&config, None).await;

        assert_eq!(first, second);
        assert!(first.unwrap().contains("test instructions"));
    }

    #[tokio::test]
    async fn cache_invalidates_on_file_edit() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "original").unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();

        let manager = ProjectDocManager::new();
        let config = test_config(tmp.path());

        let first = manager.get_user_instructions(&config, None).await;
        assert!(first.as_ref().unwrap().contains("original"));

        // Sleep to ensure mtime differs
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Edit the file
        fs::write(tmp.path().join("AGENTS.md"), "modified").unwrap();

        let second = manager.get_user_instructions(&config, None).await;
        assert!(second.as_ref().unwrap().contains("modified"));
    }

    #[tokio::test]
    async fn cache_invalidates_on_file_delete() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "to be deleted").unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();

        let manager = ProjectDocManager::new();
        let config = test_config(tmp.path());

        let first = manager.get_user_instructions(&config, None).await;
        assert!(first.is_some());

        // Delete the file
        fs::remove_file(tmp.path().join("AGENTS.md")).unwrap();

        // Verify file is actually deleted
        assert!(
            !tmp.path().join("AGENTS.md").exists(),
            "File should be deleted"
        );

        let second = manager.get_user_instructions(&config, None).await;
        // Should be None or not contain the deleted content
        assert!(
            second.is_none() || !second.as_ref().unwrap().contains("to be deleted"),
            "Expected None or no 'to be deleted' content, got: {second:?}"
        );
    }
}
