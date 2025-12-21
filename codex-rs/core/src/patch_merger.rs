use crate::pending_patch::PendingSubagentResult;
use crate::worktree_manager::PatchApplyResult;
use crate::worktree_manager::WorktreeManager;
use codex_protocol::FileChangeSummary;
use codex_protocol::SubagentChanges;
use codex_protocol::SubagentChangesStatus;
use std::path::Path;

/// Result of merging a pending patch
#[derive(Debug, Clone)]
pub struct MergeResult {
    pub call_id: String,
    pub session_id: String,
    pub subagent_type: String,
    pub changes: SubagentChanges,
}

/// Convert internal FileChangeSummary to protocol type
fn convert_file_changes(
    changes: Vec<crate::worktree_manager::FileChangeSummary>,
) -> Vec<FileChangeSummary> {
    changes
        .into_iter()
        .map(|c| FileChangeSummary {
            path: c.path,
            insertions: c.insertions,
            deletions: c.deletions,
        })
        .collect()
}

/// Merge all pending subagent patches in invocation order.
/// Returns the merge result for each subagent.
pub async fn merge_pending_patches(
    pending: Vec<PendingSubagentResult>,
    codex_home: &Path,
) -> Vec<MergeResult> {
    if pending.is_empty() {
        return Vec::new();
    }

    let worktree_manager = WorktreeManager::new(codex_home);

    // Sort by invocation order
    let mut sorted = pending;
    sorted.sort_by_key(|p| p.invocation_order);

    let mut results = Vec::new();

    for p in sorted {
        let changes = if !p.diff.has_changes {
            // No changes - just cleanup
            let _ = worktree_manager.cleanup(&p.worktree_handle).await;
            SubagentChanges {
                status: SubagentChangesStatus::NoChanges,
                files_changed: vec![],
                patch_path: None,
            }
        } else {
            // Apply patch
            let task_id = format!("{}-{}-{}", p.subagent_type, p.session_id, p.call_id);
            match worktree_manager
                .apply_patch(&p.diff.patch, &p.parent_worktree, &task_id)
                .await
            {
                Ok(PatchApplyResult::Applied) => {
                    let _ = worktree_manager.cleanup(&p.worktree_handle).await;
                    SubagentChanges {
                        status: SubagentChangesStatus::Applied,
                        files_changed: convert_file_changes(p.diff.files_changed),
                        patch_path: None,
                    }
                }
                Ok(PatchApplyResult::NoChanges) => {
                    let _ = worktree_manager.cleanup(&p.worktree_handle).await;
                    SubagentChanges {
                        status: SubagentChangesStatus::NoChanges,
                        files_changed: vec![],
                        patch_path: None,
                    }
                }
                Ok(PatchApplyResult::Conflict { patch_path }) => {
                    let _ = worktree_manager.cleanup(&p.worktree_handle).await;
                    SubagentChanges {
                        status: SubagentChangesStatus::Conflict,
                        files_changed: vec![],
                        patch_path: Some(patch_path.to_string_lossy().to_string()),
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to apply patch");
                    let _ = worktree_manager.cleanup(&p.worktree_handle).await;
                    SubagentChanges {
                        status: SubagentChangesStatus::Conflict,
                        files_changed: vec![],
                        patch_path: None,
                    }
                }
            }
        };

        results.push(MergeResult {
            call_id: p.call_id,
            session_id: p.session_id,
            subagent_type: p.subagent_type,
            changes,
        });
    }

    results
}
