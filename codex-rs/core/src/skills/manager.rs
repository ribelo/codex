use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use super::SkillLoadOutcome;
use super::load_skills_from_roots;
use super::repo_skills_root;
use super::user_skills_root;

#[derive(Clone)]
struct CachedSkills {
    outcome: SkillLoadOutcome,
    /// (root_path, mtime) for each source directory
    sources: Vec<(PathBuf, Option<SystemTime>)>,
}

/// Manages skill discovery and caching across sessions.
///
/// Skills are cached by working directory to avoid repeated filesystem scans.
/// The cache can be invalidated by calling `invalidate_cache`.
pub struct SkillsManager {
    codex_home: PathBuf,
    cache_by_cwd: RwLock<HashMap<PathBuf, CachedSkills>>,
}

impl SkillsManager {
    pub fn new(codex_home: PathBuf) -> Self {
        Self {
            codex_home,
            cache_by_cwd: RwLock::new(HashMap::new()),
        }
    }

    /// Returns skills for the given working directory.
    ///
    /// Results are cached - subsequent calls with the same cwd return cached data.
    pub fn skills_for_cwd(&self, cwd: &Path) -> SkillLoadOutcome {
        // Identify source roots first
        let mut roots = Vec::new();
        // Repo skills take precedence
        if let Some(repo_root) = repo_skills_root(cwd) {
            roots.push(repo_root);
        }
        roots.push(user_skills_root(&self.codex_home));

        // Get current mtimes
        let current_sources: Vec<(PathBuf, Option<SystemTime>)> = roots
            .iter()
            .map(|p| {
                (
                    p.path.clone(),
                    std::fs::metadata(&p.path).and_then(|m| m.modified()).ok(),
                )
            })
            .collect();

        // Check cache first
        let cached_entry = match self.cache_by_cwd.read() {
            Ok(cache) => cache.get(cwd).cloned(),
            Err(err) => err.into_inner().get(cwd).cloned(),
        };

        if let Some(cached) = cached_entry
            && cached.sources == current_sources
        {
            return cached.outcome;
        }

        // Load skills from roots (ownership transferred)
        let outcome = load_skills_from_roots(roots);

        let new_entry = CachedSkills {
            outcome: outcome.clone(),
            sources: current_sources,
        };

        // Cache the result
        match self.cache_by_cwd.write() {
            Ok(mut cache) => {
                cache.insert(cwd.to_path_buf(), new_entry);
            }
            Err(err) => {
                err.into_inner().insert(cwd.to_path_buf(), new_entry);
            }
        }

        outcome
    }

    /// Invalidates the entire cache, forcing skills to be reloaded on next access.
    #[allow(dead_code)]
    pub fn invalidate_cache(&self) {
        match self.cache_by_cwd.write() {
            Ok(mut cache) => cache.clear(),
            Err(err) => err.into_inner().clear(),
        }
    }

    /// Invalidates cache for a specific working directory.
    #[allow(dead_code)]
    pub fn invalidate_cwd(&self, cwd: &Path) {
        match self.cache_by_cwd.write() {
            Ok(mut cache) => {
                cache.remove(cwd);
            }
            Err(err) => {
                err.into_inner().remove(cwd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_skill(root: &Path, dir: &str, name: &str, description: &str) {
        let skill_dir = root.join("skills").join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: {description}\n---\nBody");
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn test_cache_invalidation_on_new_skill() {
        let home = tempdir().unwrap();
        let manager = SkillsManager::new(home.path().to_path_buf());
        let cwd = home.path();

        // 1. Initial load
        write_skill(home.path(), "skill1", "skill1", "desc1");
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 1);
        assert_eq!(outcome.skills[0].name, "skill1");

        // 2. Add new skill - directory mtime changes
        // Sleep to ensure mtime differs
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_skill(home.path(), "skill2", "skill2", "desc2");

        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 2);
    }

    #[test]
    fn test_cache_hit_ignoring_content_change() {
        let home = tempdir().unwrap();
        let manager = SkillsManager::new(home.path().to_path_buf());
        let cwd = home.path();

        write_skill(home.path(), "skill1", "skill1", "original");
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills[0].description, "original");

        // Modify content without changing directory structure
        let skill_file = home.path().join("skills/skill1/SKILL.md");

        // Sleep to ensure modification time would be different if we checked it
        std::thread::sleep(std::time::Duration::from_millis(50));

        let new_content = "---\nname: skill1\ndescription: CHANGED\n---\nBody";
        fs::write(&skill_file, new_content).unwrap();

        // Reload - expected to hit cache because root "skills" dir mtime hasn't changed
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills[0].description, "original");

        // Now force invalidation by modifying the root directory (adding a dummy file)
        // This updates the mtime of "skills" directory
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(home.path().join("skills/dummy"), "").unwrap();

        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills[0].description, "CHANGED");
    }
}
