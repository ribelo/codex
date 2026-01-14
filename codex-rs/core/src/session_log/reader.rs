//! Session log reader with uncommitted-data handling.
//!
//! Parses v2 session logs (JSONL format), tracking committed turns and
//! building transcript/model context from the active head path.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::BufRead;
use std::io::BufReader;
use std::io::{self};
use std::path::Path;

use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EntryKind;
use codex_protocol::protocol::LogEntry;
use codex_protocol::protocol::SessionMetaLine;
use tracing::warn;
use uuid::Uuid;

/// Result of reading a session log file.
#[derive(Debug, Clone)]
pub struct SessionLogData {
    /// Session header metadata, if found.
    pub header: SessionMetaLine,
    /// All committed log entries on the active head path.
    pub entries: Vec<LogEntry>,
    /// Current head ID (most recent committed entry or HeadSet target).
    pub head_id: Option<Uuid>,
}

/// Error type for session log reading.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Empty session file")]
    EmptyFile,
    #[error("No session header found")]
    NoHeader,
}

/// Read a v2 session log file and return structured data.
///
/// This function:
/// - Parses JSONL line-by-line into `LogEntry`
/// - Tracks last committed head_id (updated on TurnCommitted/HeadSet)
/// - Ignores entries for turns without TurnCommitted
/// - Handles truncated/unparseable final line gracefully
/// - Builds entries list: all events on active head path
pub fn read_session(path: &Path) -> Result<SessionLogData, ReadError> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut header: Option<SessionMetaLine> = None;
    let mut all_entries: Vec<LogEntry> = Vec::new();
    let mut committed_turn_ids: HashSet<Uuid> = HashSet::new();
    let mut current_head_id: Option<Uuid> = None;
    let mut commits: HashMap<Uuid, (Option<Uuid>, Option<Uuid>)> = HashMap::new();
    let mut lines_parsed = 0;

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                // Handle truncated/incomplete last line gracefully
                if line_num > 0 {
                    warn!(
                        "Failed to read line {}: {e}, treating as truncated",
                        line_num + 1
                    );
                    break;
                }
                return Err(ReadError::Io(e));
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        // Parse JSON line
        let entry: LogEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                // Handle unparseable line - if it's the last line, treat as truncated
                warn!("Failed to parse line {}: {e}, skipping", line_num + 1);
                continue;
            }
        };

        lines_parsed += 1;

        // Extract header if this is a SessionHeader entry
        if let EntryKind::SessionHeader { ref meta } = entry.kind {
            if header.is_none() {
                header = Some(meta.clone());
            }
        }

        // Track TurnCommitted entries to know which turns are complete
        if let EntryKind::TurnCommitted = entry.kind {
            if let Some(turn_id) = entry.turn_id {
                committed_turn_ids.insert(turn_id);
            }
            // Update head to this entry's ID
            current_head_id = Some(entry.id);
            commits.insert(entry.id, (entry.parent_id, entry.turn_id));
        }

        // Track HeadSet entries for explicit head changes (undo/redo)
        if let EntryKind::HeadSet { target_head_id, .. } = entry.kind {
            current_head_id = Some(target_head_id);
        }

        all_entries.push(entry);
    }

    if lines_parsed == 0 {
        return Err(ReadError::EmptyFile);
    }

    let header = header.ok_or(ReadError::NoHeader)?;

    // Compute active head commit chain and turn IDs.
    let mut active_commit_ids: HashSet<Uuid> = HashSet::new();
    let mut active_turn_ids: HashSet<Uuid> = HashSet::new();
    let mut cursor = current_head_id;
    while let Some(commit_id) = cursor {
        if !active_commit_ids.insert(commit_id) {
            break;
        }
        let Some((parent_id, turn_id)) = commits.get(&commit_id) else {
            break;
        };
        if let Some(turn_id) = *turn_id {
            active_turn_ids.insert(turn_id);
        }
        cursor = *parent_id;
    }

    // Filter entries to only include those from committed turns
    // Entries without turn_id are included if they are relevant to the active head path.
    let committed_entries: Vec<LogEntry> = all_entries
        .into_iter()
        .filter(|entry| match entry.turn_id {
            Some(turn_id) => {
                committed_turn_ids.contains(&turn_id) && active_turn_ids.contains(&turn_id)
            }
            None => match &entry.kind {
                EntryKind::SessionHeader { .. } | EntryKind::SessionEnd => true,
                EntryKind::HeadSet { target_head_id, .. } => {
                    active_commit_ids.contains(target_head_id)
                }
                EntryKind::CompactionApplied {
                    replaces_up_to_head_id,
                    ..
                } => replaces_up_to_head_id.is_none_or(|id| active_commit_ids.contains(&id)),
                _ => true,
            },
        })
        .collect();

    Ok(SessionLogData {
        header,
        entries: committed_entries,
        head_id: current_head_id,
    })
}

/// Build model context from session log data.
///
/// Returns ResponseItems suitable for feeding back to the model.
/// Uses the most recent CompactionApplied if present, otherwise builds from all ResponseItems.
pub fn build_model_context(data: &SessionLogData) -> Vec<ResponseItem> {
    let mut items: Vec<ResponseItem> = Vec::new();
    let mut last_compaction_idx: Option<usize> = None;
    let mut compaction_replacement: Option<Vec<ResponseItem>> = None;

    // Find the last CompactionApplied and collect ResponseItems
    for (idx, entry) in data.entries.iter().enumerate() {
        match &entry.kind {
            EntryKind::CompactionApplied {
                replacement_history,
                ..
            } => {
                last_compaction_idx = Some(idx);
                compaction_replacement = replacement_history.clone();
            }
            EntryKind::ResponseItem { item } => {
                items.push(item.clone());
            }
            _ => {}
        }
    }

    // If we have a compaction, start from replacement_history + items after compaction
    if let Some(compaction_idx) = last_compaction_idx {
        let mut result = compaction_replacement.unwrap_or_default();

        // Add ResponseItems after the compaction
        for entry in data.entries.iter().skip(compaction_idx + 1) {
            if let EntryKind::ResponseItem { item } = &entry.kind {
                result.push(item.clone());
            }
        }

        result
    } else {
        // No compaction, return all ResponseItems
        items
    }
}

/// Build transcript from session log data.
///
/// Returns all log entries on the active head path, suitable for display.
pub fn build_transcript(data: &SessionLogData) -> Vec<&LogEntry> {
    // For now, return all committed entries as the transcript.
    // A more sophisticated implementation would trace the parent_id chain
    // from the current head, but for linear histories this is equivalent.
    data.entries.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    fn make_session_header_entry(session_id: Uuid) -> LogEntry {
        LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:00.000Z".to_string(),
            session_id,
            turn_id: None,
            parent_id: None,
            kind: EntryKind::SessionHeader {
                meta: SessionMetaLine {
                    meta: SessionMeta {
                        id: codex_protocol::ConversationId::from_string(&session_id.to_string())
                            .unwrap(),
                        timestamp: "2025-01-14T12:00:00.000Z".to_string(),
                        cwd: std::path::PathBuf::from("/test"),
                        originator: "test".to_string(),
                        cli_version: "0.1.0".to_string(),
                        instructions: None,
                        source: codex_protocol::protocol::SessionSource::Cli,
                        model: Some("test-model".to_string()),
                        model_provider: None,
                    },
                    git: None,
                },
            },
        }
    }

    fn make_turn_started_entry(session_id: Uuid, turn_id: Uuid) -> LogEntry {
        LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:01.000Z".to_string(),
            session_id,
            turn_id: Some(turn_id),
            parent_id: None,
            kind: EntryKind::TurnStarted,
        }
    }

    fn make_response_item_entry(session_id: Uuid, turn_id: Uuid, text: &str) -> LogEntry {
        LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:02.000Z".to_string(),
            session_id,
            turn_id: Some(turn_id),
            parent_id: None,
            kind: EntryKind::ResponseItem {
                item: ResponseItem::Message {
                    id: Some(Uuid::new_v4().to_string()),
                    role: "assistant".to_string(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: text.to_string(),
                        signature: None,
                    }],
                },
            },
        }
    }

    fn make_turn_committed_entry(session_id: Uuid, turn_id: Uuid) -> LogEntry {
        let id = Uuid::new_v4();
        LogEntry {
            id,
            timestamp: "2025-01-14T12:00:03.000Z".to_string(),
            session_id,
            turn_id: Some(turn_id),
            parent_id: None,
            kind: EntryKind::TurnCommitted,
        }
    }

    fn write_entries_to_file(entries: &[LogEntry]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        for entry in entries {
            let line = serde_json::to_string(entry).unwrap();
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_read_session_basic() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();

        let entries = vec![
            make_session_header_entry(session_id),
            make_turn_started_entry(session_id, turn_id),
            make_response_item_entry(session_id, turn_id, "Hello world"),
            make_turn_committed_entry(session_id, turn_id),
        ];

        let file = write_entries_to_file(&entries);
        let result = read_session(file.path()).unwrap();

        assert_eq!(
            result.header.meta.id.to_string(),
            codex_protocol::ConversationId::from_string(&session_id.to_string())
                .unwrap()
                .to_string()
        );
        assert_eq!(result.entries.len(), 4);
        assert!(result.head_id.is_some());
    }

    #[test]
    fn test_read_session_uncommitted_turn_ignored() {
        let session_id = Uuid::new_v4();
        let committed_turn = Uuid::new_v4();
        let uncommitted_turn = Uuid::new_v4();

        let entries = vec![
            make_session_header_entry(session_id),
            // Committed turn
            make_turn_started_entry(session_id, committed_turn),
            make_response_item_entry(session_id, committed_turn, "Committed response"),
            make_turn_committed_entry(session_id, committed_turn),
            // Uncommitted turn (no TurnCommitted)
            make_turn_started_entry(session_id, uncommitted_turn),
            make_response_item_entry(session_id, uncommitted_turn, "Uncommitted response"),
        ];

        let file = write_entries_to_file(&entries);
        let result = read_session(file.path()).unwrap();

        // Should only have 4 entries (header + committed turn entries)
        // The 2 uncommitted entries should be filtered out
        assert_eq!(result.entries.len(), 4);

        // Verify no uncommitted content
        let has_uncommitted = result.entries.iter().any(|e| {
            if let EntryKind::ResponseItem { item } = &e.kind {
                if let ResponseItem::Message { content, .. } = item {
                    return content.iter().any(|c| {
                        if let codex_protocol::models::ContentItem::OutputText { text, .. } = c {
                            text.contains("Uncommitted")
                        } else {
                            false
                        }
                    });
                }
            }
            false
        });
        assert!(!has_uncommitted);
    }

    #[test]
    fn test_read_session_truncated_last_line() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();

        let entries = vec![
            make_session_header_entry(session_id),
            make_turn_started_entry(session_id, turn_id),
            make_turn_committed_entry(session_id, turn_id),
        ];

        let mut file = NamedTempFile::new().unwrap();
        for entry in &entries {
            let line = serde_json::to_string(entry).unwrap();
            writeln!(file, "{line}").unwrap();
        }
        // Write truncated/invalid JSON at the end
        write!(file, r#"{{"id": "abc", "truncated"#).unwrap();
        file.flush().unwrap();

        // Should succeed, ignoring the truncated line
        let result = read_session(file.path()).unwrap();
        assert_eq!(result.entries.len(), 3);
    }

    #[test]
    fn test_read_session_empty_file() {
        let file = NamedTempFile::new().unwrap();
        let result = read_session(file.path());
        assert!(matches!(result, Err(ReadError::EmptyFile)));
    }

    #[test]
    fn test_read_session_no_header() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();

        let entries = vec![
            make_turn_started_entry(session_id, turn_id),
            make_turn_committed_entry(session_id, turn_id),
        ];

        let file = write_entries_to_file(&entries);
        let result = read_session(file.path());
        assert!(matches!(result, Err(ReadError::NoHeader)));
    }

    #[test]
    fn test_read_session_head_set() {
        let session_id = Uuid::new_v4();
        let turn1 = Uuid::new_v4();
        let turn2 = Uuid::new_v4();

        let committed1 = make_turn_committed_entry(session_id, turn1);
        let committed1_id = committed1.id;
        let committed2 = make_turn_committed_entry(session_id, turn2);

        let entries = vec![
            make_session_header_entry(session_id),
            make_turn_started_entry(session_id, turn1),
            committed1,
            make_turn_started_entry(session_id, turn2),
            committed2,
            // HeadSet to revert to an earlier point
            LogEntry {
                id: Uuid::new_v4(),
                timestamp: "2025-01-14T12:00:10.000Z".to_string(),
                session_id,
                turn_id: None,
                parent_id: None,
                kind: EntryKind::HeadSet {
                    target_head_id: committed1_id,
                    reason: "undo".to_string(),
                    from_head_id: Some(committed1_id),
                },
            },
        ];

        let file = write_entries_to_file(&entries);
        let result = read_session(file.path()).unwrap();

        // Head should be the HeadSet target
        assert_eq!(result.head_id, Some(committed1_id));

        // Entries from turn2 should be excluded because the active head is turn1.
        assert!(
            !result
                .entries
                .iter()
                .any(|entry| entry.turn_id == Some(turn2)),
            "expected turn2 entries to be excluded from active head transcript"
        );
    }

    #[test]
    fn test_build_model_context_no_compaction() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();

        let entries = vec![
            make_session_header_entry(session_id),
            make_turn_started_entry(session_id, turn_id),
            make_response_item_entry(session_id, turn_id, "Response 1"),
            make_response_item_entry(session_id, turn_id, "Response 2"),
            make_turn_committed_entry(session_id, turn_id),
        ];

        let file = write_entries_to_file(&entries);
        let data = read_session(file.path()).unwrap();
        let context = build_model_context(&data);

        assert_eq!(context.len(), 2);
    }

    #[test]
    fn test_build_model_context_with_compaction() {
        let session_id = Uuid::new_v4();
        let turn1 = Uuid::new_v4();
        let turn2 = Uuid::new_v4();

        let replacement_item = ResponseItem::Message {
            id: Some("compacted".to_string()),
            role: "assistant".to_string(),
            content: vec![codex_protocol::models::ContentItem::OutputText {
                text: "Compacted summary".to_string(),
                signature: None,
            }],
        };

        let mut entries = vec![
            make_session_header_entry(session_id),
            make_turn_started_entry(session_id, turn1),
            make_response_item_entry(session_id, turn1, "Old response 1"),
            make_response_item_entry(session_id, turn1, "Old response 2"),
            make_turn_committed_entry(session_id, turn1),
        ];

        // Add compaction entry
        entries.push(LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:05.000Z".to_string(),
            session_id,
            turn_id: None,
            parent_id: None,
            kind: EntryKind::CompactionApplied {
                tokens_before: 1000,
                summary: "Compacted".to_string(),
                replacement_history: Some(vec![replacement_item.clone()]),
                replaces_up_to_head_id: None,
            },
        });

        // Add new turn after compaction
        entries.push(make_turn_started_entry(session_id, turn2));
        entries.push(make_response_item_entry(session_id, turn2, "New response"));
        entries.push(make_turn_committed_entry(session_id, turn2));

        let file = write_entries_to_file(&entries);
        let data = read_session(file.path()).unwrap();
        let context = build_model_context(&data);

        // Should have: 1 compacted item + 1 new response
        assert_eq!(context.len(), 2);
    }

    #[test]
    fn test_build_transcript() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();

        let entries = vec![
            make_session_header_entry(session_id),
            make_turn_started_entry(session_id, turn_id),
            make_response_item_entry(session_id, turn_id, "Hello"),
            make_turn_committed_entry(session_id, turn_id),
        ];

        let file = write_entries_to_file(&entries);
        let data = read_session(file.path()).unwrap();
        let transcript = build_transcript(&data);

        assert_eq!(transcript.len(), 4);
    }
}
