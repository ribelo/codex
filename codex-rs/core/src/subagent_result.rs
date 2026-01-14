use serde::Serialize;

use crate::loop_detector::LoopType;
use codex_protocol::protocol::TurnAbortReason;

/// Outcome of worker execution loop.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "output_type", rename_all = "snake_case")]
pub enum SubagentExecutionOutcome {
    /// Worker completed with a final message.
    CompletedWithMessage { message: String },
    /// Worker completed with tool output but no final message.
    CompletedWithToolOutput { tool_output: String },
    /// Worker completed without any output.
    CompletedEmpty,
    /// External interruption (Ctrl+C). Always resumable.
    Interrupted,
    /// Worker failed (loop detection, HTTP error, quota exceeded, etc).
    Failed { reason: String, can_resume: bool },
}

impl SubagentExecutionOutcome {
    /// Default hint for empty output.
    pub const EMPTY_HINT: &'static str =
        "Worker returned nothing. Use session_id to ask what happened or request to continue.";
}

impl From<LoopType> for SubagentExecutionOutcome {
    fn from(loop_type: LoopType) -> Self {
        let reason = match loop_type {
            LoopType::ConsecutiveIdenticalToolCalls => {
                "Worker got stuck in a loop of identical tool calls"
            }
            LoopType::RepetitiveContent => "Worker got stuck in a loop of repetitive content",
        };
        Self::Failed {
            reason: reason.to_string(),
            can_resume: false,
        }
    }
}

impl From<TurnAbortReason> for SubagentExecutionOutcome {
    fn from(reason: TurnAbortReason) -> Self {
        match reason {
            TurnAbortReason::Interrupted => Self::Interrupted,
            TurnAbortReason::Replaced => Self::Failed {
                reason: "Replaced by new task".to_string(),
                can_resume: false,
            },
            TurnAbortReason::ReviewEnded => Self::Failed {
                reason: "Review session ended".to_string(),
                can_resume: false,
            },
        }
    }
}

/// The complete result returned to the parent agent.
#[derive(Debug, Clone, Serialize)]
pub struct SubagentTaskResult {
    /// Worker's execution outcome (message, tool output, empty, or aborted).
    pub output: SubagentExecutionOutcome,
    /// Session ID to continue the conversation.
    pub session_id: String,
}

impl SubagentTaskResult {
    pub fn new(output: SubagentExecutionOutcome, session_id: String) -> Self {
        Self { output, session_id }
    }
}

impl std::fmt::Display for SubagentTaskResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Render output with appropriate header
        match &self.output {
            SubagentExecutionOutcome::CompletedWithMessage { message } => {
                writeln!(f, "### Message")?;
                writeln!(f, "{message}")?;
            }
            SubagentExecutionOutcome::CompletedWithToolOutput { tool_output } => {
                writeln!(f, "### Tool Output")?;
                writeln!(f, "{tool_output}")?;
            }
            SubagentExecutionOutcome::CompletedEmpty => {
                writeln!(f, "### Message")?;
                writeln!(f, "{}", SubagentExecutionOutcome::EMPTY_HINT)?;
            }
            SubagentExecutionOutcome::Interrupted => {
                writeln!(f, "### Message")?;
                writeln!(f, "Worker was interrupted.")?;
            }
            SubagentExecutionOutcome::Failed { reason, .. } => {
                writeln!(f, "### Message")?;
                writeln!(f, "Worker failed: {reason}")?;
            }
        }

        // Render summary footer
        writeln!(f)?;
        writeln!(f, "---")?;
        writeln!(f, "### Task Summary")?;

        // Status - always show
        let status = match &self.output {
            SubagentExecutionOutcome::CompletedWithMessage { .. }
            | SubagentExecutionOutcome::CompletedWithToolOutput { .. }
            | SubagentExecutionOutcome::CompletedEmpty => "Completed",
            SubagentExecutionOutcome::Interrupted => "Interrupted",
            SubagentExecutionOutcome::Failed { .. } => "Failed",
        };
        writeln!(f, "- **Status**: {status}")?;

        // Resumable - always show
        let resumable = match &self.output {
            SubagentExecutionOutcome::CompletedWithMessage { .. }
            | SubagentExecutionOutcome::CompletedWithToolOutput { .. }
            | SubagentExecutionOutcome::CompletedEmpty => false,
            SubagentExecutionOutcome::Interrupted => true,
            SubagentExecutionOutcome::Failed { can_resume, .. } => *can_resume,
        };
        writeln!(
            f,
            "- **Resumable**: {}",
            if resumable { "Yes" } else { "No" }
        )?;

        // Session ID - always show
        write!(f, "- **Session ID**: `{}`", self.session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_completed_with_message() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::CompletedWithMessage {
                message: "Task completed successfully.".to_string(),
            },
            "abc-123".to_string(),
        );
        let output = result.to_string();
        assert!(output.contains("Task completed successfully"));
        assert!(output.contains("### Task Summary"));
        assert!(output.contains("`abc-123`"));
    }

    #[test]
    fn test_display_interrupted() {
        let result =
            SubagentTaskResult::new(SubagentExecutionOutcome::Interrupted, "pqr-678".to_string());
        let output = result.to_string();
        assert!(output.contains("Worker was interrupted"));
        assert!(output.contains("**Resumable**: Yes"));
        assert!(output.contains("`pqr-678`"));
    }

    #[test]
    fn test_display_failed_resumable() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::Failed {
                reason: "Rate limit exceeded (429)".to_string(),
                can_resume: true,
            },
            "err-123".to_string(),
        );
        let output = result.to_string();
        assert!(output.contains("Worker failed: Rate limit exceeded"));
        assert!(output.contains("**Resumable**: Yes"));
        assert!(output.contains("`err-123`"));
    }

    #[test]
    fn test_display_failed_not_resumable() {
        let result = SubagentTaskResult::new(
            SubagentExecutionOutcome::Failed {
                reason: "Quota exceeded".to_string(),
                can_resume: false,
            },
            "err-456".to_string(),
        );
        let output = result.to_string();
        assert!(output.contains("Worker failed: Quota exceeded"));
        assert!(output.contains("**Resumable**: No"));
        assert!(output.contains("`err-456`"));
    }

    #[test]
    fn test_from_loop_type_consecutive() {
        let outcome = SubagentExecutionOutcome::from(LoopType::ConsecutiveIdenticalToolCalls);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Failed { reason, can_resume }
                if reason.contains("identical tool calls") && !can_resume
        ));
    }

    #[test]
    fn test_from_loop_type_repetitive() {
        let outcome = SubagentExecutionOutcome::from(LoopType::RepetitiveContent);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Failed { reason, can_resume }
                if reason.contains("repetitive content") && !can_resume
        ));
    }

    #[test]
    fn test_from_turn_abort_interrupted() {
        let outcome = SubagentExecutionOutcome::from(TurnAbortReason::Interrupted);
        assert!(matches!(outcome, SubagentExecutionOutcome::Interrupted));
    }

    #[test]
    fn test_from_turn_abort_replaced() {
        let outcome = SubagentExecutionOutcome::from(TurnAbortReason::Replaced);
        assert!(matches!(
            outcome,
            SubagentExecutionOutcome::Failed { can_resume, .. } if !can_resume
        ));
    }
}
