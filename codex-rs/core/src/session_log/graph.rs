//! Commit graph extraction from session logs.
//!
//! Parses v2 session log entries and builds a DAG of commits (TurnCommitted)
//! and head movements (HeadSet).

use std::collections::HashMap;

use codex_protocol::protocol::EntryKind;
use codex_protocol::protocol::LogEntry;
use uuid::Uuid;

/// The kind of commit in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitKind {
    /// A turn was committed.
    TurnCommitted { turn_id: Uuid },
    /// Head was explicitly set to a different commit.
    HeadSet {
        target_head_id: Uuid,
        reason: String,
    },
}

/// A node in the commit graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitNode {
    /// Unique ID of this commit (the LogEntry id).
    pub id: Uuid,
    /// Parent commit ID, if any.
    pub parent_id: Option<Uuid>,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// The kind of commit.
    pub kind: CommitKind,
}

/// A DAG of commits extracted from a session log.
#[derive(Debug, Clone)]
pub struct CommitGraph {
    /// All commit nodes in insertion order.
    pub nodes: Vec<CommitNode>,
    /// Map from commit ID to index in `nodes`.
    pub index: HashMap<Uuid, usize>,
    /// Map from parent ID to list of child IDs.
    pub children: HashMap<Uuid, Vec<Uuid>>,
    /// Current head ID (most recent commit or HeadSet target).
    pub head_id: Option<Uuid>,
}

impl CommitGraph {
    /// Get a node by ID.
    pub fn get(&self, id: Uuid) -> Option<&CommitNode> {
        self.index.get(&id).map(|&idx| &self.nodes[idx])
    }

    /// Get children of a node.
    pub fn get_children(&self, id: Uuid) -> &[Uuid] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Check if a node has multiple children (is a branch point).
    pub fn is_branch_point(&self, id: Uuid) -> bool {
        self.get_children(id).len() > 1
    }
}

/// Build a commit graph from session log entries.
///
/// Extracts TurnCommitted and HeadSet entries, building a DAG representation
/// with parent-child relationships.
pub fn build_commit_graph(entries: &[LogEntry]) -> CommitGraph {
    let mut nodes = Vec::new();
    let mut index = HashMap::new();
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut head_id = None;

    for entry in entries {
        let node = match &entry.kind {
            EntryKind::TurnCommitted => {
                let turn_id = entry.turn_id.unwrap_or(entry.id);
                Some(CommitNode {
                    id: entry.id,
                    parent_id: entry.parent_id,
                    timestamp: entry.timestamp.clone(),
                    kind: CommitKind::TurnCommitted { turn_id },
                })
            }
            EntryKind::HeadSet {
                target_head_id,
                reason,
                ..
            } => Some(CommitNode {
                id: entry.id,
                parent_id: entry.parent_id,
                timestamp: entry.timestamp.clone(),
                kind: CommitKind::HeadSet {
                    target_head_id: *target_head_id,
                    reason: reason.clone(),
                },
            }),
            _ => None,
        };

        if let Some(node) = node {
            let node_id = node.id;

            // Update children map
            if let Some(parent) = node.parent_id {
                children.entry(parent).or_default().push(node_id);
            }

            // Update head based on node kind
            head_id = match &node.kind {
                CommitKind::TurnCommitted { .. } => Some(node_id),
                CommitKind::HeadSet { target_head_id, .. } => Some(*target_head_id),
            };

            index.insert(node_id, nodes.len());
            nodes.push(node);
        }
    }

    CommitGraph {
        nodes,
        index,
        children,
        head_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn make_turn_committed_entry(
        session_id: Uuid,
        turn_id: Uuid,
        parent_id: Option<Uuid>,
    ) -> LogEntry {
        LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:00.000Z".to_string(),
            session_id,
            turn_id: Some(turn_id),
            parent_id,
            kind: EntryKind::TurnCommitted,
        }
    }

    fn make_head_set_entry(
        session_id: Uuid,
        target_head_id: Uuid,
        reason: &str,
        from_head_id: Option<Uuid>,
    ) -> LogEntry {
        LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:00.000Z".to_string(),
            session_id,
            turn_id: None,
            parent_id: None,
            kind: EntryKind::HeadSet {
                target_head_id,
                reason: reason.to_string(),
                from_head_id,
            },
        }
    }

    #[test]
    fn test_empty_entries() {
        let graph = build_commit_graph(&[]);
        assert!(graph.nodes.is_empty());
        assert!(graph.index.is_empty());
        assert!(graph.children.is_empty());
        assert!(graph.head_id.is_none());
    }

    #[test]
    fn test_single_commit() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();
        let entry = make_turn_committed_entry(session_id, turn_id, None);
        let entry_id = entry.id;

        let graph = build_commit_graph(&[entry]);

        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.head_id, Some(entry_id));

        let node = graph.get(entry_id).unwrap();
        assert_eq!(node.id, entry_id);
        assert_eq!(node.parent_id, None);
        assert_eq!(node.kind, CommitKind::TurnCommitted { turn_id });
    }

    #[test]
    fn test_linear_chain() {
        let session_id = Uuid::new_v4();
        let turn1 = Uuid::new_v4();
        let turn2 = Uuid::new_v4();
        let turn3 = Uuid::new_v4();

        let entry1 = make_turn_committed_entry(session_id, turn1, None);
        let entry1_id = entry1.id;

        let entry2 = make_turn_committed_entry(session_id, turn2, Some(entry1_id));
        let entry2_id = entry2.id;

        let entry3 = make_turn_committed_entry(session_id, turn3, Some(entry2_id));
        let entry3_id = entry3.id;

        let graph = build_commit_graph(&[entry1, entry2, entry3]);

        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.head_id, Some(entry3_id));

        // Check parent-child relationships
        assert_eq!(graph.get_children(entry1_id), &[entry2_id]);
        assert_eq!(graph.get_children(entry2_id), &[entry3_id]);
        assert!(graph.get_children(entry3_id).is_empty());

        // No branch points in a linear chain
        assert!(!graph.is_branch_point(entry1_id));
        assert!(!graph.is_branch_point(entry2_id));
    }

    #[test]
    fn test_head_set_changes_head() {
        let session_id = Uuid::new_v4();
        let turn1 = Uuid::new_v4();
        let turn2 = Uuid::new_v4();

        let entry1 = make_turn_committed_entry(session_id, turn1, None);
        let entry1_id = entry1.id;

        let entry2 = make_turn_committed_entry(session_id, turn2, Some(entry1_id));
        let entry2_id = entry2.id;

        // HeadSet reverts to entry1
        let head_set = make_head_set_entry(session_id, entry1_id, "undo", Some(entry2_id));
        let head_set_id = head_set.id;

        let graph = build_commit_graph(&[entry1, entry2, head_set]);

        assert_eq!(graph.nodes.len(), 3);
        // Head should now point to entry1 (the target of HeadSet)
        assert_eq!(graph.head_id, Some(entry1_id));

        // HeadSet node should be in the graph
        let head_set_node = graph.get(head_set_id).unwrap();
        assert_eq!(
            head_set_node.kind,
            CommitKind::HeadSet {
                target_head_id: entry1_id,
                reason: "undo".to_string(),
            }
        );
    }

    #[test]
    fn test_branch_detection() {
        let session_id = Uuid::new_v4();
        let turn1 = Uuid::new_v4();
        let turn2 = Uuid::new_v4();
        let turn3 = Uuid::new_v4();

        // Create a branch: entry1 -> entry2, entry1 -> entry3
        let entry1 = make_turn_committed_entry(session_id, turn1, None);
        let entry1_id = entry1.id;

        let entry2 = make_turn_committed_entry(session_id, turn2, Some(entry1_id));
        let entry2_id = entry2.id;

        let entry3 = make_turn_committed_entry(session_id, turn3, Some(entry1_id));
        let entry3_id = entry3.id;

        let graph = build_commit_graph(&[entry1, entry2, entry3]);

        assert_eq!(graph.nodes.len(), 3);

        // entry1 should be a branch point
        assert!(graph.is_branch_point(entry1_id));
        let children = graph.get_children(entry1_id);
        assert_eq!(children.len(), 2);
        assert!(children.contains(&entry2_id));
        assert!(children.contains(&entry3_id));

        // entry2 and entry3 are not branch points
        assert!(!graph.is_branch_point(entry2_id));
        assert!(!graph.is_branch_point(entry3_id));
    }

    #[test]
    fn test_ignores_other_entry_kinds() {
        let session_id = Uuid::new_v4();
        let turn_id = Uuid::new_v4();

        // Create a TurnStarted entry (should be ignored)
        let started = LogEntry {
            id: Uuid::new_v4(),
            timestamp: "2025-01-14T12:00:00.000Z".to_string(),
            session_id,
            turn_id: Some(turn_id),
            parent_id: None,
            kind: EntryKind::TurnStarted {
                sub_id: "sub-123".to_string(),
            },
        };

        let committed = make_turn_committed_entry(session_id, turn_id, None);
        let committed_id = committed.id;

        let graph = build_commit_graph(&[started, committed]);

        // Only TurnCommitted should be in the graph
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.head_id, Some(committed_id));
    }
}
