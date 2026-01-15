//! Session log module: new append-only session persistence with turn-based durability.
//!
//! This module provides a replacement for RolloutRecorder with:
//! - Append-only JSONL format using LogEntry/EntryKind
//! - Turn-based durability via fsync on TurnCommitted
//! - Background writer with bounded async channel
//! - Power-outage safe for completed turns
//! - Reader with uncommitted-data handling

pub mod graph;
pub mod list;
pub mod reader;
mod writer;

pub use graph::CommitGraph;
pub use graph::CommitKind;
pub use graph::CommitNode;
pub use graph::build_commit_graph;
pub use list::CursorV2;
pub use list::SessionV2Item;
pub use list::SessionsV2Page;
pub use list::find_session_log_v2_by_id;
pub use list::list_session_children_v2;
pub use list::list_sessions_v2;
pub use list::parse_cursor_v2;
pub use reader::ReadError;
pub use reader::SessionLogData;
pub use reader::build_model_context;
pub use reader::build_transcript;
pub use reader::get_parent_commit_id;
pub use reader::read_session;
pub use reader::read_session_at_head;
pub use writer::SessionLog;
pub use writer::SessionLogParams;

/// Subdirectory under codex_home for v2 session logs.
pub const SESSIONS_V2_SUBDIR: &str = "sessions/v2";
