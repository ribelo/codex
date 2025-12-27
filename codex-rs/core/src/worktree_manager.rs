use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_protocol::FileChangeSummary;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
pub use tokio::process::Command;
use tracing::warn;
use uuid::Uuid;

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
    /// Patch applied with conflict markers left in files
    AppliedWithConflicts {
        /// Files that contain conflict markers
        conflicted_files: Vec<String>,
    },
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
    ///
    /// Uses a hybrid approach:
    /// 1. Try clean `git apply` first (fast path)
    /// 2. On failure, fall back to `git apply --3way` (leaves conflict markers)
    /// 3. Detect conflicts via `git diff --check`
    /// 4. Hard failures save patch file as fallback
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

        // 1. Try clean git apply (fast path)
        let apply_output = Command::new("git")
            .arg("apply")
            .arg(&patch_file)
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to apply patch")?;

        if apply_output.status.success() {
            // Clean apply succeeded
            let _ = tokio::fs::remove_file(&patch_file).await;
            return Ok(PatchApplyResult::Applied);
        }

        // 2. Fall back to git apply --3way
        let threeway_output = Command::new("git")
            .args(["apply", "--3way"])
            .arg(&patch_file)
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to run git apply --3way")?;

        if threeway_output.status.success() {
            // 3way merge resolved automatically
            let _ = tokio::fs::remove_file(&patch_file).await;
            return Ok(PatchApplyResult::Applied);
        }

        // 3. Check if we have conflict markers (soft failure) or hard failure
        let conflicted_files = Self::detect_conflict_markers(target_dir).await?;

        if !conflicted_files.is_empty() {
            // Soft failure: conflicts left in files for agent to resolve
            let _ = tokio::fs::remove_file(&patch_file).await;
            return Ok(PatchApplyResult::AppliedWithConflicts { conflicted_files });
        }

        // 4. Hard failure: save patch file
        Ok(PatchApplyResult::Conflict {
            patch_path: patch_file,
        })
    }

    /// Detect files with conflict markers using `git diff --check`
    async fn detect_conflict_markers(target_dir: &Path) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(["diff", "--check"])
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to run git diff --check")?;

        // git diff --check exits non-zero if markers found
        // Output format: "filename:line: leftover conflict marker"
        let stdout = String::from_utf8_lossy(&output.stdout);

        let files: Vec<String> = stdout
            .lines()
            .filter(|line| line.contains("leftover conflict marker"))
            .filter_map(|line| line.split(':').next().map(|s| s.to_string()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        Ok(files)
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
