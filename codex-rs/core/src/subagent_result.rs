use serde::Serialize;

use crate::loop_detector::LoopType;
use codex_protocol::protocol::TurnAbortReason;

/// Outcome of subagent execution loop (before worktree handling).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "output_type", rename_all = "snake_case")]
pub enum SubagentExecutionOutcome {
    /// Subagent completed with a final message.
    CompletedWithMessage { message: String },
    /// Subagent completed with tool output but no final message.
    CompletedWithToolOutput { tool_output: String },
    /// Subagent completed without any output.
    CompletedEmpty,
    /// Subagent was aborted (interrupted, replaced, etc).
    Aborted { reason: String, can_resume: bool },
}

impl SubagentExecutionOutcome {
    /// Default hint for empty output.
    pub const EMPTY_HINT: &'static str =
        "Subagent returned nothing. Use session_id to ask what happened or request to continue.";
}

impl From<LoopType> for SubagentExecutionOutcome {
    fn from(loop_type: LoopType) -> Self {
        let reason = match loop_type {
            LoopType::ConsecutiveIdenticalToolCalls => {
                "Subagent got stuck in a loop of identical tool calls"
            }
            LoopType::RepetitiveContent => "Subagent got stuck in a loop of repetitive content",
        };
        Self::Aborted {
            reason: reason.to_string(),
            can_resume: false,
        }
    }
}

impl From<TurnAbortReason> for SubagentExecutionOutcome {
    fn from(reason: TurnAbortReason) -> Self {
        match reason {
            TurnAbortReason::Interrupted => Self::Aborted {
                reason: "Interrupted by user".to_string(),
                can_resume: true,
            },
            TurnAbortReason::Replaced => Self::Aborted {
                reason: "Replaced by new task".to_string(),
                can_resume: false,
            },
            TurnAbortReason::ReviewEnded => Self::Aborted {
                reason: "Review session ended".to_string(),
                can_resume: false,
            },
        }
    }
}

/// Summary of a changed file.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    pub insertions: Option<i32>,
    pub deletions: Option<i32>,
}

/// File change status - each variant only has relevant fields.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "changes", rename_all = "snake_case")]
pub enum SubagentOutcome {
    /// Changes merged successfully.
    Applied {
        files_changed: Vec<FileChange>,
        note: String,
    },
    /// No file changes made.
    NoChanges { note: String },
    /// Merge conflict - patch saved for manual review.
    Conflict { patch_path: String, note: String },
    /// Merge failed for technical reasons.
    Failed { error: String, note: String },
    /// Subagent doesn't use filesystem (e.g., oracle, review).
    NoFileSystem { note: String },
    /// Subagent execution was aborted (interrupted, loop detected, error).
    Aborted {
        reason: String,
        note: String,
        can_resume: bool,
    },
}

impl SubagentOutcome {
    pub fn applied(files: Vec<FileChange>) -> Self {
        let note = format!(
            "Changes applied successfully. {} file(s) changed.",
            files.len()
        );
        Self::Applied {
            files_changed: files,
            note,
        }
    }

    pub fn no_changes() -> Self {
        Self::NoChanges {
            note: "Subagent completed but made no file changes.".into(),
        }
    }

    pub fn conflict(patch_path: String, failure_reason: Option<String>) -> Self {
        let note = if let Some(reason) = &failure_reason {
            format!(
                "Merge conflict: {reason}. Changes saved to '{patch_path}'. Apply manually or ask subagent to resolve."
            )
        } else {
            format!(
                "Merge conflict. Changes saved to '{patch_path}'. Apply manually or ask subagent to resolve."
            )
        };
        Self::Conflict { patch_path, note }
    }

    pub fn failed(error: String) -> Self {
        let note = format!("Failed to apply changes: {error}");
        Self::Failed { error, note }
    }

    pub fn no_filesystem() -> Self {
        Self::NoFileSystem {
            note: "This subagent operates without filesystem isolation.".into(),
        }
    }

    pub fn aborted(reason: String, can_resume: bool) -> Self {
        let note = if can_resume {
            format!("Subagent aborted: {reason}. Use session_id to resume or investigate.")
        } else {
            format!("Subagent aborted: {reason}.")
        };
        Self::Aborted {
            reason,
            note,
            can_resume,
        }
    }
}

/// The complete result returned to the parent agent.
#[derive(Debug, Clone, Serialize)]
pub struct SubagentTaskResult {
    /// Subagent's execution outcome (message, tool output, empty, or aborted).
    pub output: SubagentExecutionOutcome,
    /// Session ID to continue the conversation.
    pub session_id: String,
    /// File change outcome.
    pub outcome: SubagentOutcome,
}

impl SubagentTaskResult {
    pub fn new(
        output: SubagentExecutionOutcome,
        session_id: String,
        outcome: SubagentOutcome,
    ) -> Self {
        Self {
            output,
            session_id,
            outcome,
        }
    }
}

impl std::fmt::Display for SubagentTaskResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Render output first
        match &self.output {
            SubagentExecutionOutcome::CompletedWithMessage { message } => writeln!(f, "{message}")?,
            SubagentExecutionOutcome::CompletedWithToolOutput { tool_output } => {
                writeln!(f, "**Tool Output**:\n{tool_output}")?;
            }
            SubagentExecutionOutcome::CompletedEmpty => {
                writeln!(f, "{}", SubagentExecutionOutcome::EMPTY_HINT)?
            }
            SubagentExecutionOutcome::Aborted { reason, .. } => {
                writeln!(f, "Subagent was aborted: {reason}")?
            }
        }

        // Render summary footer
        writeln!(f)?;
        writeln!(f, "---")?;
        writeln!(f, "### Task Summary")?;

        match &self.outcome {
            SubagentOutcome::Applied {
                files_changed,
                note,
            } => {
                writeln!(f, "- **Status**: {note}")?;
                writeln!(f, "- **Files**:")?;
                for file in files_changed {
                    let ins = file.insertions.unwrap_or(0);
                    let del = file.deletions.unwrap_or(0);
                    writeln!(f, "  - `{}` (+{}, -{})", file.path, ins, del)?;
                }
            }
            SubagentOutcome::NoChanges { note } => {
                writeln!(f, "- **Status**: {note}")?;
            }
            SubagentOutcome::Conflict { patch_path, note } => {
                writeln!(f, "- **Status**: {note}")?;
                writeln!(f, "- **Patch**: `{patch_path}`")?;
            }
            SubagentOutcome::Failed { note, .. } => {
                writeln!(f, "- **Status**: {note}")?;
            }
            SubagentOutcome::NoFileSystem { note } => {
                writeln!(f, "- **Status**: {note}")?;
            }
            SubagentOutcome::Aborted {
                note, can_resume, ..
            } => {
                writeln!(f, "- **Status**: {note}")?;
                if *can_resume {
                    writeln!(f, "- **Resumable**: Yes")?;
                }
            }
        }

        write!(f, "- **Session ID**: `{}`", self.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_message_with_applied_changes() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::CompletedWithMessage {
                message: "I fixed the bug in user.rs".to_string(),
            },
            "abc-123".to_string(),
            SubagentOutcome::applied(vec![FileChange {
                path: "src/user.rs".to_string(),
                insertions: Some(5),
                deletions: Some(2),
            }]),
        );

        let output = result.to_string();
        assert!(output.contains("I fixed the bug in user.rs"));
        assert!(output.contains("### Task Summary"));
        assert!(output.contains("Changes applied successfully"));
        assert!(output.contains("`src/user.rs` (+5, -2)"));
        assert!(output.contains("`abc-123`"));
    }

    #[test]
    fn test_display_empty_output_no_changes() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::CompletedEmpty,
            "def-456".to_string(),
            SubagentOutcome::no_changes(),
        );

        let output = result.to_string();
        assert!(output.contains("Subagent returned nothing"));
        assert!(output.contains("made no file changes"));
        assert!(output.contains("`def-456`"));
    }

    #[test]
    fn test_display_tool_result_with_conflict() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::CompletedWithToolOutput {
                tool_output: "Found 3 files".to_string(),
            },
            "ghi-789".to_string(),
            SubagentOutcome::conflict("/path/to/patch.diff".to_string(), None),
        );

        let output = result.to_string();
        assert!(output.contains("**Tool Output**:"));
        assert!(output.contains("Found 3 files"));
        assert!(output.contains("Merge conflict"));
        assert!(output.contains("`/path/to/patch.diff`"));
    }

    #[test]
    fn test_display_no_filesystem() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::CompletedWithMessage {
                message: "Analysis complete.".to_string(),
            },
            "jkl-012".to_string(),
            SubagentOutcome::no_filesystem(),
        );

        let output = result.to_string();
        assert!(output.contains("Analysis complete"));
        assert!(output.contains("without filesystem isolation"));
    }

    #[test]
    fn test_display_failed() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::CompletedWithMessage {
                message: "I tried to apply changes.".to_string(),
            },
            "mno-345".to_string(),
            SubagentOutcome::failed("Permission denied".to_string()),
        );

        let output = result.to_string();
        assert!(output.contains("Failed to apply changes: Permission denied"));
    }

    #[test]
    fn test_display_aborted_resumable() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::Aborted {
                reason: "Interrupted".to_string(),
                can_resume: true,
            },
            "pqr-678".to_string(),
            SubagentOutcome::aborted("Interrupted".to_string(), true),
        );

        let output = result.to_string();
        assert!(output.contains("Subagent was aborted: Interrupted"));
        assert!(output.contains("**Resumable**: Yes"));
        assert!(output.contains("`pqr-678`"));
    }

    #[test]
    fn test_display_aborted_not_resumable() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::Aborted {
                reason: "LoopDetected".to_string(),
                can_resume: false,
            },
            "stu-901".to_string(),
            SubagentOutcome::aborted("LoopDetected".to_string(), false),
        );

        let output = result.to_string();
        assert!(output.contains("Subagent was aborted: LoopDetected"));
        assert!(!output.contains("**Resumable**"));
    }

    #[test]
    fn test_from_loop_type_consecutive() {
        let outcome = SubagentExecutionOutcome::from(LoopType::ConsecutiveIdenticalToolCalls);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Aborted { reason, can_resume }
                if reason.contains("identical tool calls") && !can_resume
        ));
    }

    #[test]
    fn test_from_loop_type_repetitive() {
        let outcome = SubagentExecutionOutcome::from(LoopType::RepetitiveContent);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Aborted { reason, can_resume }
                if reason.contains("repetitive content") && !can_resume
        ));
    }

    #[test]
    fn test_from_turn_abort_interrupted() {
        let outcome = SubagentExecutionOutcome::from(TurnAbortReason::Interrupted);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Aborted { can_resume, .. } if can_resume
        ));
    }

    #[test]
    fn test_from_turn_abort_replaced() {
        let outcome = SubagentExecutionOutcome::from(TurnAbortReason::Replaced);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Aborted { can_resume, .. } if !can_resume
        ));
    }
}
