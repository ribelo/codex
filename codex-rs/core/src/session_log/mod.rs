//! Session log module: new append-only session persistence with turn-based durability.
//!
//! This module provides a replacement for RolloutRecorder with:
//! - Append-only JSONL format using LogEntry/EntryKind
//! - Turn-based durability via fsync on TurnCommitted
//! - Background writer with bounded async channel
//! - Power-outage safe for completed turns
//! - Reader with uncommitted-data handling

pub mod reader;
mod writer;

pub use reader::ReadError;
pub use reader::SessionLogData;
pub use reader::build_model_context;
pub use reader::build_transcript;
pub use reader::read_session;
pub use writer::SessionLog;
pub use writer::SessionLogParams;

/// Subdirectory under codex_home for v2 session logs.
pub const SESSIONS_V2_SUBDIR: &str = "sessions/v2";
