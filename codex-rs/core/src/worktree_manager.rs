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
use std::time::Duration;
use tokio::io::AsyncWriteExt;
pub use tokio::process::Command;
use tokio::time::sleep;
use tracing::warn;
use uuid::Uuid;

/// Maximum retries for git commands that might fail due to index.lock contention
const MAX_GIT_RETRY_ATTEMPTS: u32 = 3;
/// Initial delay between retries (doubles each attempt)
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Check if git command failed due to index.lock contention
fn is_index_lock_error(stderr: &[u8]) -> bool {
    let stderr_str = String::from_utf8_lossy(stderr);
    stderr_str.contains("index.lock") || stderr_str.contains("Unable to create")
}

/// Check if a process with the given PID is still running
async fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // On Linux, check /proc/<pid> which is reliable, locale-independent, and fast.
        // On other Unix (macOS, BSD), /proc might not exist or work differently,
        // so we fall back to kill -0.
        #[cfg(target_os = "linux")]
        {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Use kill -0 which is POSIX standard for macOS/BSD
            let output = Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .output()
                .await;

            match output {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    // Exit code 1 can mean either "No such process" or "Permission denied"
                    let stderr = String::from_utf8_lossy(&o.stderr).to_lowercase();
                    if stderr.contains("no such process") {
                        false
                    } else {
                        true // Permission denied or unknown - assume alive to be safe
                    }
                }
                Err(_) => true, // Conservative: assume alive if we can't check
            }
        }
    }
    #[cfg(windows)]
    {
        // On Windows, use tasklist to check if process exists
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .await
            .map(|o| {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // tasklist returns "INFO: No tasks are running..." if not found
                o.status.success() && !stdout.contains("No tasks")
            })
            .unwrap_or(true) // Conservative: assume alive if we can't check
    }
    #[cfg(not(any(unix, windows)))]
    {
        true // Conservative default for unknown platforms
    }
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
    pub patch: Vec<u8>,
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
    Conflict {
        patch_path: PathBuf,
        /// Files that were modified before the failure (partial application)
        partially_applied_files: Vec<String>,
        /// The error message from git apply (for debugging)
        failure_reason: String,
    },
}

/// Manages git worktree lifecycle for subagent isolation
pub struct WorktreeManager {
    /// Root directory where worktrees are created (.codex/agents/)
    worktrees_root: PathBuf,
    /// Directory for storing conflict patches (.codex/patches/)
    patches_dir: PathBuf,
}

impl WorktreeManager {
    /// Create a new WorktreeManager for per-repo storage
    /// Storage will be at <repo_root>/.codex/worktrees/ and <repo_root>/.codex/patches/
    pub fn new_for_repo(repo_root: &Path) -> Self {
        let codex_dir = repo_root.join(".codex");
        Self {
            worktrees_root: codex_dir.join("worktrees"),
            patches_dir: codex_dir.join("patches"),
        }
    }

    /// Create a new WorktreeManager for global storage (fallback for non-git dirs)
    /// Storage will be at <codex_home>/worktrees/ and <codex_home>/patches/
    pub fn new_global(codex_home: &Path) -> Self {
        Self {
            worktrees_root: codex_home.join("worktrees"),
            patches_dir: codex_home.join("patches"),
        }
    }

    /// Get the repository root directory for a given directory
    pub async fn get_repo_root(dir: &Path) -> Result<PathBuf> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(dir)
            .output()
            .await
            .context("Failed to find git repo root")?;

        if !output.status.success() {
            bail!(
                "Failed to find git repo root: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Use trim_end_matches to only strip trailing newlines, preserving paths with trailing spaces
        let path_str = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();
        Ok(PathBuf::from(path_str))
    }

    /// Add .codex/ to .git/info/exclude
    pub async fn ensure_gitignore(repo_root: &Path) -> Result<()> {
        // Use git rev-parse --git-path to correctly resolve the exclude file path
        // This works in both main repos and worktrees (where .git is a file, not a directory)
        let output = Command::new("git")
            .args(["rev-parse", "--git-path", "info/exclude"])
            .current_dir(repo_root)
            .output()
            .await;

        let exclude_file = match output {
            Ok(o) if o.status.success() => {
                let path_str = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if path_str.is_empty() {
                    return Ok(());
                }
                // The path may be relative to repo_root
                let path = PathBuf::from(&path_str);
                if path.is_absolute() {
                    path
                } else {
                    repo_root.join(path)
                }
            }
            _ => return Ok(()), // Not a git repo or git not available
        };

        if !exclude_file.exists() {
            // Create parent directories if needed
            if let Some(parent) = exclude_file.parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
        }

        let content = tokio::fs::read_to_string(&exclude_file)
            .await
            .unwrap_or_default();

        // Check for exact line match to avoid false positives like "my.codex/"
        let has_codex_entry = content
            .lines()
            .any(|line| line.trim() == ".codex/" || line.trim() == ".codex");

        if !has_codex_entry {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&exclude_file)
                .await?;
            file.write_all(b"\n# Codex worktrees and patches\n.codex/\n")
                .await?;
        }

        Ok(())
    }

    /// Clean up any orphaned worktrees from previous sessions.
    /// Call this on startup to prune stale worktree directories.
    pub async fn prune_orphaned_worktrees(&self) -> Result<()> {
        // Skip if the worktrees directory doesn't exist
        if !self.worktrees_root.exists() {
            return Ok(());
        }

        let mut entries = match tokio::fs::read_dir(&self.worktrees_root).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!(
                    path = %self.worktrees_root.display(),
                    error = %e,
                    "Could not read worktrees directory for cleanup"
                );
                return Ok(());
            }
        };

        let mut cleaned_count = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Check if this looks like a worktree (has .git file/dir)
            let git_marker = path.join(".git");
            if !git_marker.exists() {
                continue;
            }

            // Skip recently created directories to avoid race with create_worktree
            if let Ok(metadata) = tokio::fs::metadata(&path).await
                && let Ok(modified) = metadata.modified()
                && let Ok(elapsed) = modified.elapsed()
                && elapsed < Duration::from_secs(60)
            {
                tracing::debug!(
                    path = %path.display(),
                    "Skipping recently created worktree (less than 60s old)"
                );
                continue;
            }

            // Check PID file to see if the owning process is still alive
            let pid_file = path.join("lock.pid");
            if pid_file.exists()
                && let Ok(pid_str) = tokio::fs::read_to_string(&pid_file).await
                && let Ok(pid) = pid_str.trim().parse::<u32>()
                && is_process_alive(pid).await
            {
                tracing::debug!(
                    path = %path.display(),
                    pid,
                    "Skipping active worktree (process still running)"
                );
                continue;
            }

            // Worktree is orphaned, remove it
            if let Err(e) = tokio::fs::remove_dir_all(&path).await {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "Failed to clean up orphaned worktree"
                );
            } else {
                tracing::info!(path = %path.display(), "Cleaned up orphaned worktree");
                cleaned_count += 1;
            }
        }

        if cleaned_count > 0 {
            tracing::info!(
                count = cleaned_count,
                "Pruned orphaned worktrees from previous sessions"
            );
        }

        Ok(())
    }

    /// Check if repository has submodules
    async fn has_submodules(repo_dir: &Path) -> bool {
        repo_dir.join(".gitmodules").exists()
    }

    /// Get list of submodule paths from .gitmodules
    async fn get_submodule_paths(repo_dir: &Path) -> Vec<String> {
        let output = Command::new("git")
            .args(["config", "--file", ".gitmodules", "--get-regexp", "path"])
            .current_dir(repo_dir)
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().last())
                .map(ToString::to_string)
                .collect(),
            _ => vec![],
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

    /// Copy untracked files from parent directory to worktree.
    /// Uses `git ls-files --others --exclude-standard` to list untracked files,
    /// respecting .gitignore patterns.
    async fn copy_untracked_files(parent_dir: &Path, worktree_path: &Path) -> Result<()> {
        // List untracked files (excluding ignored ones), null-terminated for safe parsing
        let output = Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .current_dir(parent_dir)
            .output()
            .await
            .context("Failed to list untracked files")?;

        if !output.status.success() {
            tracing::debug!("git ls-files failed, skipping untracked file copy");
            return Ok(());
        }

        // Parse null-terminated paths
        let paths: Vec<String> = output
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect();

        if paths.is_empty() {
            return Ok(());
        }

        tracing::debug!(count = paths.len(), "Copying untracked files to worktree");

        // Copy files in parallel with bounded concurrency to avoid fd exhaustion
        use futures::stream::StreamExt;
        const MAX_CONCURRENT_COPIES: usize = 64;

        let copy_stream = futures::stream::iter(paths.into_iter().map(|path_str| {
            let src = parent_dir.join(&path_str);
            let dst = worktree_path.join(&path_str);
            async move {
                // Check metadata asynchronously to avoid blocking
                let metadata = match tokio::fs::symlink_metadata(&src).await {
                    Ok(m) => m,
                    Err(_) => return, // File doesn't exist or can't be read
                };

                // Skip directories
                if metadata.is_dir() {
                    return;
                }

                // Create parent directories if needed (handles existing dirs gracefully)
                if let Some(parent) = dst.parent() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        tracing::debug!(path = %path_str, error = %e, "Failed to create parent dir");
                        return;
                    }
                }

                // Copy file (dereferences symlinks for safety - prevents escape from sandbox)
                if let Err(e) = tokio::fs::copy(&src, &dst).await {
                    tracing::debug!(path = %path_str, error = %e, "Failed to copy untracked file");
                }
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_COPIES)
        .collect::<Vec<_>>();

        copy_stream.await;

        Ok(())
    }

    /// Capture current state of parent_dir and create isolated worktree.
    /// Uses `git stash create` to snapshot dirty state, then copies untracked files.
    pub async fn create_worktree(&self, parent_dir: &Path) -> Result<WorktreeHandle> {
        let repo_root = Self::get_repo_root(parent_dir).await?;
        Self::ensure_gitignore(&repo_root).await?;

        // Verify git repo
        if !Self::is_git_repo(parent_dir).await? {
            bail!("Not a git repository: {}", parent_dir.display());
        }

        // Create directories if needed
        tokio::fs::create_dir_all(&self.worktrees_root).await?;

        // Generate unique ID
        let id = Uuid::new_v4().to_string();
        let worktree_path = self.worktrees_root.join(&id);

        // Capture dirty state of tracked files: git stash create
        // Note: stash create does NOT support -u flag (it's interpreted as message)
        // Untracked files are copied separately after worktree creation
        let stash_output = Command::new("git")
            .args(["stash", "create"])
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

        // Write PID file to mark this worktree as active
        let pid_file = worktree_path.join("lock.pid");
        let pid = std::process::id();
        if let Err(e) = tokio::fs::write(&pid_file, pid.to_string()).await {
            tracing::debug!(
                path = %pid_file.display(),
                error = %e,
                "Failed to write PID file for worktree"
            );
        }

        // Warn about submodules (they won't be initialized in worktrees)
        if Self::has_submodules(parent_dir).await {
            let submodule_paths = Self::get_submodule_paths(parent_dir).await;
            if !submodule_paths.is_empty() {
                tracing::warn!(
                    worktree_id = %id,
                    submodule_count = submodule_paths.len(),
                    submodules = ?submodule_paths,
                    "Worktree created without submodule initialization. Submodule directories will be empty."
                );
            }
        }

        // Copy untracked files from parent to worktree
        // This ensures subagents can see and edit files that aren't tracked by git
        if let Err(e) = Self::copy_untracked_files(parent_dir, &worktree_path).await {
            tracing::warn!(error = %e, "Failed to copy some untracked files to worktree");
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
        // Remove lock.pid before staging - it's an internal file that shouldn't be in the diff
        let pid_file = handle.path.join("lock.pid");
        if pid_file.exists() {
            let _ = tokio::fs::remove_file(&pid_file).await;
        }

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

        let patch = diff_output.stdout;
        let has_changes = !patch.is_empty() && !patch.iter().all(|&b| b.is_ascii_whitespace());

        // Get file stats: git diff --cached --numstat <base_sha>
        let stats_output = Command::new("git")
            .args([
                "-c",
                "core.quotePath=false",
                "diff",
                "--cached",
                "--numstat",
                &handle.base_sha,
            ])
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
                    // Git returns "-" for binary files
                    let insertions = if parts[0] == "-" {
                        None
                    } else {
                        parts[0].parse().ok()
                    };
                    let deletions = if parts[1] == "-" {
                        None
                    } else {
                        parts[1].parse().ok()
                    };
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
        patch: &[u8],
        target_dir: &Path,
        task_id: &str,
    ) -> Result<PatchApplyResult> {
        if patch.is_empty() || patch.iter().all(|&b| b.is_ascii_whitespace()) {
            return Ok(PatchApplyResult::NoChanges);
        }

        // Snapshot dirty files before attempting patch application
        // Used to filter out pre-existing dirty files from "partially applied" warnings
        let pre_existing_dirty: HashSet<String> = Self::get_modified_files(target_dir)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        // Create temp file for patch
        let patch_file = self.patches_dir.join(format!("{task_id}.diff"));
        tokio::fs::create_dir_all(&self.patches_dir).await?;
        tokio::fs::write(&patch_file, patch).await?;

        // Track the last error from git apply for debugging
        let mut last_apply_error = String::new();

        // 1. Try clean git apply (fast path) with retry for index.lock
        for attempt in 0..MAX_GIT_RETRY_ATTEMPTS {
            let output = Command::new("git")
                .arg("apply")
                .arg(&patch_file)
                .current_dir(target_dir)
                .output()
                .await
                .context("Failed to apply patch")?;

            if output.status.success() {
                let _ = tokio::fs::remove_file(&patch_file).await;
                return Ok(PatchApplyResult::Applied);
            }

            last_apply_error = String::from_utf8_lossy(&output.stderr).into_owned();

            if is_index_lock_error(&output.stderr) && attempt < MAX_GIT_RETRY_ATTEMPTS - 1 {
                let delay_ms = INITIAL_RETRY_DELAY_MS * (1 << attempt);
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms,
                    "git apply failed due to index.lock, retrying..."
                );
                sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }

            break;
        }

        // 2. Fall back to git apply --3way with retry for index.lock
        for attempt in 0..MAX_GIT_RETRY_ATTEMPTS {
            let output = Command::new("git")
                .args(["apply", "--3way"])
                .arg(&patch_file)
                .current_dir(target_dir)
                .output()
                .await
                .context("Failed to run git apply --3way")?;

            if output.status.success() {
                let _ = tokio::fs::remove_file(&patch_file).await;
                return Ok(PatchApplyResult::Applied);
            }

            last_apply_error = String::from_utf8_lossy(&output.stderr).into_owned();

            if is_index_lock_error(&output.stderr) && attempt < MAX_GIT_RETRY_ATTEMPTS - 1 {
                let delay_ms = INITIAL_RETRY_DELAY_MS * (1 << attempt);
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms,
                    "git apply --3way failed due to index.lock, retrying..."
                );
                sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }

            break;
        }

        // 3. Check if we have conflict markers (soft failure) or hard failure
        let conflicted_files = Self::detect_conflict_markers(target_dir).await?;

        if !conflicted_files.is_empty() {
            // Soft failure: conflicts left in files for agent to resolve
            let _ = tokio::fs::remove_file(&patch_file).await;
            return Ok(PatchApplyResult::AppliedWithConflicts { conflicted_files });
        }

        // 4. Hard failure: check if any files were partially applied
        // Filter out pre-existing dirty files to only report files that were
        // modified by the failed patch application, not files that were already dirty
        let current_dirty: HashSet<String> = Self::get_modified_files(target_dir)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        let partially_applied_files: Vec<String> = current_dirty
            .difference(&pre_existing_dirty)
            .cloned()
            .collect();

        if !partially_applied_files.is_empty() {
            tracing::warn!(
                file_count = partially_applied_files.len(),
                "Patch partially applied. Some files were modified before failure."
            );
        }

        Ok(PatchApplyResult::Conflict {
            patch_path: patch_file,
            partially_applied_files,
            failure_reason: last_apply_error,
        })
    }

    /// Detect files with conflict markers using `git diff --check`
    async fn detect_conflict_markers(target_dir: &Path) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(["-c", "core.quotePath=false", "diff", "--check"])
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to run git diff --check")?;

        // git diff --check exits non-zero if markers found
        // Output format: "filename:line_number: leftover conflict marker"
        let stdout = String::from_utf8_lossy(&output.stdout);

        let files_set: HashSet<String> = stdout
            .lines()
            .filter_map(|line| {
                // Parse from the right side to handle filenames with colons
                // 1. Strip the known suffix
                let content = line.strip_suffix(": leftover conflict marker")?;
                // 2. Split at the LAST colon to separate path from line number
                let (path, line_num) = content.rsplit_once(':')?;
                // 3. Verify the split part is a line number (guard against malformed lines)
                if line_num.chars().all(|c| c.is_ascii_digit()) {
                    Some(path.to_string())
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();
        let mut files: Vec<_> = files_set.into_iter().collect();
        files.sort();

        Ok(files)
    }

    /// Get list of modified files using `git status --porcelain`
    async fn get_modified_files(target_dir: &Path) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(["-c", "core.quotePath=false", "status", "--porcelain"])
            .current_dir(target_dir)
            .output()
            .await
            .context("Failed to run git status")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<String> = stdout
            .lines()
            .filter(|line| {
                // git status --porcelain format: XY filename
                // Check for any change indicator: M (modified), A (added), D (deleted), ? (untracked)
                if line.len() < 4 {
                    return false;
                }
                let status = &line[0..2];
                status.contains('M')
                    || status.contains('A')
                    || status.contains('D')
                    || status.contains('?')
            })
            .filter_map(|line| line.get(3..).map(std::string::ToString::to_string))
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
