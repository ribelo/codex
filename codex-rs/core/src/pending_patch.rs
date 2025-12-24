use crate::worktree_manager::WorktreeDiff;
use crate::worktree_manager::WorktreeHandle;
use std::path::PathBuf;

/// Result from a completed subagent, pending merge
#[derive(Debug)]
pub struct PendingSubagentResult {
    /// Invocation order (0 = first spawned in this turn)
    pub invocation_order: u32,
    /// The subagent type
    pub subagent_name: String,
    /// Task description for this subagent invocation
    pub task_description: String,
    /// Session ID
    pub session_id: String,
    /// Call ID for this tool invocation
    pub call_id: String,
    /// The subagent's final response
    pub result: String,
    /// Last tool output
    pub last_tool_output: Option<String>,
    /// The generated diff
    pub diff: WorktreeDiff,
    /// Worktree handle for cleanup
    pub worktree_handle: WorktreeHandle,
    /// Parent worktree to merge into
    pub parent_worktree: PathBuf,
}
