use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
pub use tokio::process::Command;
use tracing::warn;
use uuid::Uuid;

/// Summary of a single file change (matches protocol type)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangeSummary {
    pub path: String,
    pub insertions: i32,
    pub deletions: i32,
}

/// Result of creating a worktree
#[derive(Debug)]
pub struct WorktreeHandle {
    /// Unique ID for this worktree
    pub id: String,
    /// Path to the worktree directory
    pub path: PathBuf,
    /// The base SHA this worktree was created from
    pub base_sha: String,
    /// The parent directory this was created from (for cleanup)
    pub parent_dir: PathBuf,
    /// Whether this handle has already been cleaned up
    pub(crate) cleaned_up: AtomicBool,
}

impl Drop for WorktreeHandle {
    fn drop(&mut self) {
        if !self.cleaned_up.swap(true, Ordering::SeqCst) {
            warn!(
                "WorktreeHandle for {} was dropped without explicit cleanup. Performing emergency cleanup.",
                self.id
            );
            // Synchronous cleanup in Drop
            let _ = std::process::Command::new("git")
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&self.path)
                .current_dir(&self.parent_dir)
                .output();

            // Prune if remove failed or just to be safe
            let _ = std::process::Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(&self.parent_dir)
                .output();
        }
    }
}

/// Result of generating a diff
#[derive(Debug, Clone)]
pub struct WorktreeDiff {
    /// The patch content (may be empty if no changes)
    pub patch: String,
    /// Parsed file change summaries
    pub files_changed: Vec<FileChangeSummary>,
    /// Whether there are any changes
    pub has_changes: bool,
}

/// Result of applying a patch
#[derive(Debug, Clone)]
pub enum PatchApplyResult {
    /// Patch applied successfully
    Applied,
    /// No changes to apply
    NoChanges,
    /// Conflict - patch saved to file
    Conflict { patch_path: PathBuf },
}

/// Manages git worktree lifecycle for subagent isolation
pub struct WorktreeManager {
    /// Root directory where worktrees are created (.codex/agents/)
    worktrees_root: PathBuf,
    /// Directory for storing conflict patches (.codex/patches/)
    patches_dir: PathBuf,
}

impl WorktreeManager {
    pub fn new(codex_dir: &Path) -> Self {
        Self {
            worktrees_root: codex_dir.join("agents"),
            patches_dir: codex_dir.join("patches"),
        }
    }

    /// Check if a directory is inside a git repository
    pub async fn is_git_repo(dir: &Path) -> Result<bool> {
        let output = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        Ok(output.success())
    }

    /// Capture current state of parent_dir and create isolated worktree.
    /// Uses `git stash create -u` to snapshot dirty state.
    pub async fn create_worktree(&self, parent_dir: &Path) -> Result<WorktreeHandle> {
        // Verify git repo
        if !Self::is_git_repo(parent_dir).await? {
            bail!("Not a git repository: {}", parent_dir.display());
        }

        // Create directories if needed
        tokio::fs::create_dir_all(&self.worktrees_root).await?;

        // Generate unique ID
        let id = Uuid::new_v4().to_string();
        let worktree_path = self.worktrees_root.join(&id);

        // Capture dirty state: git stash create -u
        // Returns empty string if nothing to stash (clean state)
        let stash_output = Command::new("git")
            .args(["stash", "create", "-u"])
            .current_dir(parent_dir)
            .output()
            .await
            .context("Failed to run git stash create")?;

        if !stash_output.status.success() {
            let stderr = String::from_utf8_lossy(&stash_output.stderr);
            bail!("git stash create failed: {stderr}");
        }

        let base_sha = String::from_utf8_lossy(&stash_output.stdout)
            .trim()
            .to_string();

        // If stash is empty, use HEAD
        let base_sha = if base_sha.is_empty() {
            let head_output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(parent_dir)
                .output()
                .await
                .context("Failed to get HEAD")?;

            if !head_output.status.success() {
                bail!(
                    "Failed to get HEAD: {}",
                    String::from_utf8_lossy(&head_output.stderr)
                );
            }
            String::from_utf8_lossy(&head_output.stdout)
                .trim()
                .to_string()
        } else {
            base_sha
        };

        // Create worktree: git worktree add --detach <path> <sha>
        let worktree_output = Command::new("git")
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(&worktree_path)
            .arg(&base_sha)
            .current_dir(parent_dir)
            .output()
            .await
            .context("Failed to create worktree")?;

        if !worktree_output.status.success() {
            bail!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&worktree_output.stderr)
            );
        }

        Ok(WorktreeHandle {
            id,
            path: worktree_path,
            base_sha,
            parent_dir: parent_dir.to_path_buf(),
            cleaned_up: AtomicBool::new(false),
        })
    }

    /// Generate diff of all changes in worktree since base_sha.
    pub async fn generate_diff(&self, handle: &WorktreeHandle) -> Result<WorktreeDiff> {
        // Stage all changes: git add -A
        let add_output = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&handle.path)
            .output()
            .await
            .context("Failed to stage changes")?;

        if !add_output.status.success() {
            bail!(
                "Failed to stage changes: {}",
                String::from_utf8_lossy(&add_output.stderr)
            );
        }

        // Generate diff: git diff --binary --cached <base_sha>
        let diff_output = Command::new("git")
            .args(["diff", "--binary", "--cached", &handle.base_sha])
            .current_dir(&handle.path)
            .output()
            .await
            .context("Failed to generate diff")?;

        if !diff_output.status.success() {
            bail!(
                "Failed to generate diff: {}",
                String::from_utf8_lossy(&diff_output.stderr)
            );
        }

        let patch = String::from_utf8_lossy(&diff_output.stdout).to_string();
        let has_changes = !patch.trim().is_empty();

        // Get file stats: git diff --cached --numstat <base_sha>
        let stats_output = Command::new("git")
            .args(["diff", "--cached", "--numstat", &handle.base_sha])
            .current_dir(&handle.path)
            .output()
            .await
            .context("Failed to get diff stats")?;

        let files_changed = if stats_output.status.success() {
            Self::parse_numstat(&String::from_utf8_lossy(&stats_output.stdout))
        } else {
            vec![]
        };

        Ok(WorktreeDiff {
            patch,
            files_changed,
            has_changes,
        })
    }

    /// Parse git diff --numstat output into FileChangeSummary list
    fn parse_numstat(output: &str) -> Vec<FileChangeSummary> {
        output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    let insertions = parts[0].parse().unwrap_or(0);
                    let deletions = parts[1].parse().unwrap_or(0);
                    let path = parts[2].to_string();
                    Some(FileChangeSummary {
                        path,
                        insertions,
                        deletions,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Apply patch to target directory.
    pub async fn apply_patch(
        &self,
        patch: &str,
        target_dir: &Path,
        task_id: &str,
    ) -> Result<PatchApplyResult> {
        if patch.trim().is_empty() {
            return Ok(PatchApplyResult::NoChanges);
        }

        // Create temp file for patch
        let patch_file = self.patches_dir.join(format!("{task_id}.diff"));
        tokio::fs::create_dir_all(&self.patches_dir).await?;
        tokio::fs::write(&patch_file, patch).await?;

        // Check if patch applies: git apply --check
        let check_output = Command::new("git")
            .arg("apply")
            .arg("--check")
            .arg(&patch_file)
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to check patch")?;

        if !check_output.status.success() {
            // Conflict - keep the patch file
            return Ok(PatchApplyResult::Conflict {
                patch_path: patch_file,
            });
        }

        // Apply patch: git apply
        let apply_output = Command::new("git")
            .arg("apply")
            .arg(&patch_file)
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to apply patch")?;

        if !apply_output.status.success() {
            // Unexpected failure after check passed
            return Ok(PatchApplyResult::Conflict {
                patch_path: patch_file,
            });
        }
        // Clean up patch file on success
        let _ = tokio::fs::remove_file(&patch_file).await;
        Ok(PatchApplyResult::Applied)
    }
    /// Remove worktree and cleanup.
    pub async fn cleanup(&self, handle: &WorktreeHandle) -> Result<()> {
        if handle.cleaned_up.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let output = Command::new("git")
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(&handle.path)
            .current_dir(&handle.parent_dir)
            .output()
            .await
            .context("Failed to remove worktree")?;

        if !output.status.success() {
            // Try manual cleanup if git worktree remove fails
            if handle.path.exists() {
                tokio::fs::remove_dir_all(&handle.path).await?;
            }

            // Prune worktree list
            let _ = Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(&handle.parent_dir)
                .output()
                .await;
        }

        Ok(())
    }
}
