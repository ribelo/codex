use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Status of subagent's file changes after merge attempt
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SubagentChangesStatus {
    /// Changes applied successfully to parent worktree
    Applied,
    /// Changes applied with conflict markers left in files
    AppliedWithConflicts,
    /// Merge conflict - patch saved to patch_path
    Conflict,
    /// Subagent made no file changes
    NoChanges,
}

/// Summary of a single file change
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
pub struct FileChangeSummary {
    pub path: String,
    pub insertions: i32,
    pub deletions: i32,
}

/// Subagent file changes info, separate from subagent's response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
pub struct SubagentChanges {
    pub status: SubagentChangesStatus,
    /// List of changed files (empty if no_changes or conflict)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_changed: Vec<FileChangeSummary>,
    /// Path to saved patch file (only present if conflict)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_path: Option<String>,
    /// List of files that have conflict markers (only present if applied_with_conflicts)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicted_files: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subagent_changes_applied_serialization() {
        let changes = SubagentChanges {
            status: SubagentChangesStatus::Applied,
            files_changed: vec![FileChangeSummary {
                path: "src/main.rs".to_string(),
                insertions: 10,
                deletions: 3,
            }],
            patch_path: None,
            conflicted_files: None,
        };
        let json = serde_json::to_string(&changes).unwrap();
        let parsed: SubagentChanges = serde_json::from_str(&json).unwrap();
        assert_eq!(changes, parsed);
    }

    #[test]
    fn test_subagent_changes_conflict_serialization() {
        let changes = SubagentChanges {
            status: SubagentChangesStatus::Conflict,
            files_changed: vec![],
            patch_path: Some(".codex/patches/abc123.diff".to_string()),
            conflicted_files: None,
        };
        let json = serde_json::to_string(&changes).unwrap();
        assert!(json.contains("\"status\":\"conflict\""));
        assert!(json.contains("\"patch_path\""));
        let parsed: SubagentChanges = serde_json::from_str(&json).unwrap();
        assert_eq!(changes, parsed);
    }

    #[test]
    fn test_subagent_changes_no_changes_serialization() {
        let changes = SubagentChanges {
            status: SubagentChangesStatus::NoChanges,
            files_changed: vec![],
            patch_path: None,
            conflicted_files: None,
        };
        let json = serde_json::to_string(&changes).unwrap();
        // Empty vec and None should be skipped
        assert!(!json.contains("files_changed"));
        assert!(!json.contains("patch_path"));
        let parsed: SubagentChanges = serde_json::from_str(&json).unwrap();
        assert_eq!(changes, parsed);
    }

    #[test]
    fn test_subagent_changes_applied_with_conflicts_serialization() {
        let changes = SubagentChanges {
            status: SubagentChangesStatus::AppliedWithConflicts,
            files_changed: vec![FileChangeSummary {
                path: "src/main.rs".to_string(),
                insertions: 5,
                deletions: 2,
            }],
            patch_path: None,
            conflicted_files: Some(vec!["src/main.rs".to_string()]),
        };
        let json = serde_json::to_string(&changes).unwrap();
        assert!(json.contains("\"status\":\"applied_with_conflicts\""));
        assert!(json.contains("\"conflicted_files\""));
        let parsed: SubagentChanges = serde_json::from_str(&json).unwrap();
        assert_eq!(changes, parsed);
    }
}
