use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use super::SkillLoadOutcome;
use super::load_skills_from_roots;
use super::repo_skills_root;
use super::system::install_system_skills;
use super::system_skills_root;
use super::user_skills_root;

#[derive(Clone)]
struct CachedSkills {
    outcome: SkillLoadOutcome,
    /// (root_path, mtime) for each source directory
    root_states: Vec<(PathBuf, Option<SystemTime>)>,
    /// (dir_path, mtime) for all directories visited during discovery
    visited_dir_states: Vec<(PathBuf, Option<SystemTime>)>,
    /// (skill_file_path, mtime) for each SKILL.md loaded
    file_states: Vec<(PathBuf, Option<SystemTime>)>,
}

/// Manages skill discovery and caching across sessions.
///
/// Skills are cached by working directory to avoid repeated filesystem scans.
pub struct SkillsManager {
    codex_home: PathBuf,
    cache_by_cwd: RwLock<HashMap<PathBuf, CachedSkills>>,
}

impl SkillsManager {
    pub fn new(codex_home: PathBuf) -> Self {
        if let Err(err) = install_system_skills(&codex_home) {
            tracing::error!("failed to install system skills: {err}");
        }

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
        // System skills have lowest priority (can be overridden by user/repo skills)
        roots.push(system_skills_root(&self.codex_home));

        // Get current mtimes
        let current_root_states: Vec<(PathBuf, Option<SystemTime>)> = roots
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

        if let Some(cached) = cached_entry {
            // Check roots first (fast fail for structural changes)
            if cached.root_states == current_root_states {
                // Check all visited directories
                let mut dirs_valid = true;
                for (path, cached_mtime) in &cached.visited_dir_states {
                    let current_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                    if current_mtime != *cached_mtime {
                        dirs_valid = false;
                        break;
                    }
                }

                if dirs_valid {
                    // Check individual files (detect content edits)
                    let mut files_valid = true;
                    for (path, cached_mtime) in &cached.file_states {
                        let current_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                        if current_mtime != *cached_mtime {
                            files_valid = false;
                            break;
                        }
                    }

                    if files_valid {
                        return cached.outcome;
                    }
                }
            }
        }

        // Load skills from roots (ownership transferred)
        let (outcome, visited_dir_states) = load_skills_from_roots(roots);

        // Collect mtimes for loaded skills to detect future edits
        let file_states: Vec<(PathBuf, Option<SystemTime>)> = outcome
            .skills
            .iter()
            .map(|skill| {
                (
                    skill.path.clone(),
                    std::fs::metadata(&skill.path)
                        .and_then(|m| m.modified())
                        .ok(),
                )
            })
            .collect();

        let new_entry = CachedSkills {
            outcome: outcome.clone(),
            root_states: current_root_states,
            visited_dir_states,
            file_states,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillMetadata;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn write_skill(root: &Path, dir: &str, name: &str, description: &str) {
        let skill_dir = root.join("skills").join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: {description}\n---\nBody");
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    // Number of system skills that are automatically installed
    const SYSTEM_SKILLS_COUNT: usize = 3;

    fn find_skill<'a>(outcome: &'a SkillLoadOutcome, name: &str) -> Option<&'a SkillMetadata> {
        outcome.skills.iter().find(|s| s.name == name)
    }

    #[test]
    fn test_cache_invalidation_on_new_skill() {
        let home = tempdir().unwrap();
        let manager = SkillsManager::new(home.path().to_path_buf());
        let cwd = home.path();

        // 1. Initial load
        write_skill(home.path(), "skill1", "skill1", "desc1");
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 1 + SYSTEM_SKILLS_COUNT);
        assert!(find_skill(&outcome, "skill1").is_some());

        // 2. Add new skill - directory mtime changes
        // Sleep to ensure mtime differs
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_skill(home.path(), "skill2", "skill2", "desc2");

        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 2 + SYSTEM_SKILLS_COUNT);
    }

    #[test]
    fn test_cache_invalidation_on_content_change() {
        let home = tempdir().unwrap();
        let manager = SkillsManager::new(home.path().to_path_buf());
        let cwd = home.path();

        write_skill(home.path(), "skill1", "skill1", "original");
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(
            find_skill(&outcome, "skill1").unwrap().description,
            "original"
        );

        // Modify content without changing directory structure
        let skill_file = home.path().join("skills/skill1/SKILL.md");

        // Sleep to ensure modification time would be different if we checked it
        std::thread::sleep(std::time::Duration::from_millis(50));

        let new_content = "---\nname: skill1\ndescription: CHANGED\n---\nBody";
        fs::write(&skill_file, new_content).unwrap();

        // Reload - expected to invalidate cache because file mtime changed
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(
            find_skill(&outcome, "skill1").unwrap().description,
            "CHANGED"
        );
    }

    #[test]
    fn test_cache_invalidation_on_skill_removal() {
        let home = tempdir().unwrap();
        let manager = SkillsManager::new(home.path().to_path_buf());
        let cwd = home.path();

        // 1. Initial load with two skills
        write_skill(home.path(), "skill1", "skill1", "desc1");
        write_skill(home.path(), "skill2", "skill2", "desc2");
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 2 + SYSTEM_SKILLS_COUNT);

        // 2. Remove one skill directory
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::remove_dir_all(home.path().join("skills/skill2")).unwrap();

        // 3. Reload - should detect removal
        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 1 + SYSTEM_SKILLS_COUNT);
        assert!(find_skill(&outcome, "skill1").is_some());
        assert!(find_skill(&outcome, "skill2").is_none());
    }

    #[test]
    fn test_repo_skills_override_user_skills() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();

        // Initialize git repo
        Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .output()
            .unwrap();

        let manager = SkillsManager::new(home.path().to_path_buf());

        // Create user skill
        write_skill(home.path(), "shared-skill", "shared-skill", "from-user");

        // Create repo skill with same name
        let repo_skills = repo.path().join(".codex/skills/shared-skill");
        fs::create_dir_all(&repo_skills).unwrap();
        fs::write(
            repo_skills.join("SKILL.md"),
            "---\nname: shared-skill\ndescription: from-repo\n---\nBody",
        )
        .unwrap();

        // Load skills with repo as cwd
        let outcome = manager.skills_for_cwd(repo.path());

        // Should have only one skill (deduplicated by name)
        // and it should be the repo version
        let shared = outcome.skills.iter().find(|s| s.name == "shared-skill");
        assert!(shared.is_some(), "shared-skill should exist");
        assert_eq!(
            shared.unwrap().description,
            "from-repo",
            "repo skill should override user skill"
        );
    }

    #[test]
    fn test_cache_isolation_between_cwds() {
        let home = tempdir().unwrap();
        let project_a = tempdir().unwrap();
        let project_b = tempdir().unwrap();

        // Initialize git repos
        Command::new("git")
            .args(["init"])
            .current_dir(project_a.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(project_b.path())
            .output()
            .unwrap();

        let manager = SkillsManager::new(home.path().to_path_buf());

        // Create skill in project A
        let skill_a = project_a.path().join(".codex/skills/skill-a");
        fs::create_dir_all(&skill_a).unwrap();
        fs::write(
            skill_a.join("SKILL.md"),
            "---\nname: skill-a\ndescription: from-a\n---\nBody",
        )
        .unwrap();

        // Create skill in project B
        let skill_b = project_b.path().join(".codex/skills/skill-b");
        fs::create_dir_all(&skill_b).unwrap();
        fs::write(
            skill_b.join("SKILL.md"),
            "---\nname: skill-b\ndescription: from-b\n---\nBody",
        )
        .unwrap();

        // Load skills for project A
        let outcome_a = manager.skills_for_cwd(project_a.path());
        assert!(outcome_a.skills.iter().any(|s| s.name == "skill-a"));
        assert!(!outcome_a.skills.iter().any(|s| s.name == "skill-b"));

        // Load skills for project B
        let outcome_b = manager.skills_for_cwd(project_b.path());
        assert!(outcome_b.skills.iter().any(|s| s.name == "skill-b"));
        assert!(!outcome_b.skills.iter().any(|s| s.name == "skill-a"));
    }

    #[test]
    fn test_nested_skill_invalidation() {
        let home = tempdir().unwrap();
        let manager = SkillsManager::new(home.path().to_path_buf());
        let cwd = home.path();

        // 1. Initial load with a nested skill
        // skills/category/skill1/SKILL.md
        write_skill(home.path(), "category/skill1", "skill1", "desc1");

        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(outcome.skills.len(), 1 + SYSTEM_SKILLS_COUNT);

        // 2. Add another skill in the same nested category
        // skills/category/skill2/SKILL.md
        // This updates mtime of "skills/category", but not "skills"
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_skill(home.path(), "category/skill2", "skill2", "desc2");

        let outcome = manager.skills_for_cwd(cwd);
        assert_eq!(
            outcome.skills.len(),
            2 + SYSTEM_SKILLS_COUNT,
            "Should detect new nested skill"
        );
    }
}
