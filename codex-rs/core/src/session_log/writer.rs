//! SessionLog writer: background writer task with bounded async channel and turn-commit fsync.

use std::fs;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::protocol::EntryKind;
use codex_protocol::protocol::LogEntry;
use codex_protocol::protocol::SessionMetaLine;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::BufWriter;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::error;
use tracing::warn;
use uuid::Uuid;

use super::SESSIONS_V2_SUBDIR;

/// Channel buffer size for log entries.
const CHANNEL_BUFFER_SIZE: i32 = 256;

/// Parameters for creating or resuming a SessionLog.
#[derive(Clone)]
pub enum SessionLogParams {
    /// Create a new session log file.
    Create {
        /// Session ID for this log.
        session_id: Uuid,
        /// Session metadata to write as the header.
        meta: SessionMetaLine,
    },
    /// Resume writing to an existing session log file.
    Resume {
        /// Path to the existing session log file.
        path: PathBuf,
        /// Session ID from the existing log.
        session_id: Uuid,
    },
}

/// Commands sent to the background writer task.
enum WriterCmd {
    /// Append a log entry.
    Append(LogEntry),
    /// Write TurnCommitted entry and fsync, then ack.
    CommitTurn {
        turn_id: Uuid,
        ack: oneshot::Sender<Result<(), IoError>>,
    },
    /// Shutdown: drain, flush, sync_data, close.
    Shutdown {
        ack: oneshot::Sender<Result<(), IoError>>,
    },
}

/// Session log writer with background task and bounded async channel.
///
/// Provides:
/// - `append(entry)`: non-blocking send to background writer
/// - `commit_turn(turn_id)`: blocks until TurnCommitted is written and fsynced
/// - `shutdown()`: drains pending writes, flushes, sync_data, closes
#[derive(Clone)]
pub struct SessionLog {
    tx: mpsc::Sender<WriterCmd>,
    session_id: Uuid,
    log_path: PathBuf,
}

impl SessionLog {
    /// Create a new SessionLog.
    ///
    /// For `Create` params, creates a new file in `~/.codex/sessions/v2/YYYY/MM/DD/`.
    /// For `Resume` params, opens the existing file in append mode.
    pub fn new(codex_home: &Path, params: SessionLogParams) -> std::io::Result<Self> {
        let (session_id, log_path, meta) = match params {
            SessionLogParams::Create { session_id, meta } => {
                let log_path = create_log_file(codex_home, session_id)?;
                (session_id, log_path, Some(meta))
            }
            SessionLogParams::Resume { path, session_id } => {
                // Ensure the file exists and is writable
                if !path.exists() {
                    return Err(IoError::other(format!(
                        "session log file does not exist: {path:?}"
                    )));
                }
                (session_id, path, None)
            }
        };

        let (tx, rx) = mpsc::channel(CHANNEL_BUFFER_SIZE as usize);

        // Spawn the background writer task
        let path_clone = log_path.clone();
        let session_id_clone = session_id;
        tokio::spawn(async move {
            if let Err(e) = writer_task(path_clone, rx, session_id_clone, meta).await {
                error!("SessionLog writer task failed: {e}");
            }
        });

        Ok(Self {
            tx,
            session_id,
            log_path,
        })
    }

    /// Get the session ID.
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    /// Get the path to the log file.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Append a log entry (non-blocking).
    ///
    /// Returns immediately after queueing the entry. The entry will be written
    /// by the background task but is not guaranteed to be durable until
    /// `commit_turn` is called.
    pub async fn append(&self, entry: LogEntry) -> Result<(), IoError> {
        self.tx
            .send(WriterCmd::Append(entry))
            .await
            .map_err(|e| IoError::other(format!("failed to queue log entry: {e}")))
    }

    /// Commit a turn: writes TurnCommitted entry and waits for fsync.
    ///
    /// This ensures all prior entries for this turn are durably persisted.
    pub async fn commit_turn(&self, turn_id: Uuid) -> Result<(), IoError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(WriterCmd::CommitTurn {
                turn_id,
                ack: ack_tx,
            })
            .await
            .map_err(|e| IoError::other(format!("failed to queue turn commit: {e}")))?;

        ack_rx
            .await
            .map_err(|e| IoError::other(format!("failed waiting for turn commit ack: {e}")))?
    }

    /// Shutdown the session log: drains pending writes, flushes, sync_data, closes.
    pub async fn shutdown(&self) -> Result<(), IoError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(WriterCmd::Shutdown { ack: ack_tx })
            .await
            .map_err(|e| IoError::other(format!("failed to queue shutdown: {e}")))?;

        ack_rx
            .await
            .map_err(|e| IoError::other(format!("failed waiting for shutdown ack: {e}")))?
    }
}

/// Create a new log file in the v2 session directory structure.
///
/// Path: `{codex_home}/sessions/v2/YYYY/MM/DD/session-{timestamp}-{session_id}.jsonl`
fn create_log_file(codex_home: &Path, session_id: Uuid) -> std::io::Result<PathBuf> {
    let timestamp = OffsetDateTime::now_local().unwrap_or_else(|e| {
        warn!("failed to resolve local time: {e}; falling back to UTC");
        OffsetDateTime::now_utc()
    });

    // Build directory: ~/.codex/sessions/v2/YYYY/MM/DD/
    let mut dir = codex_home.to_path_buf();
    dir.push(SESSIONS_V2_SUBDIR);
    let dir_format: &[FormatItem] = format_description!("[year]/[month]/[day]");
    let dir_suffix = timestamp
        .format(dir_format)
        .map_err(|e| IoError::other(format!("failed to format date: {e}")))?;
    for component in dir_suffix.split('/') {
        dir.push(component);
    }
    fs::create_dir_all(&dir)?;

    // Filename: session-YYYY-MM-DDThh-mm-ss-{session_id}.jsonl
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let date_str = timestamp
        .format(format)
        .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;
    let filename = format!("session-{date_str}-{session_id}.jsonl");

    let path = dir.join(filename);

    // Create the file (will fail if already exists, which is desired)
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;

    Ok(path)
}

async fn scan_current_head_id(path: &Path) -> std::io::Result<Option<Uuid>> {
    let file = tokio::fs::File::open(path).await?;
    let mut lines = BufReader::new(file).lines();
    let mut head_id: Option<Uuid> = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<LogEntry>(&line) else {
            continue;
        };

        match entry.kind {
            EntryKind::TurnCommitted => {
                head_id = Some(entry.id);
            }
            EntryKind::HeadSet { target_head_id, .. } => {
                head_id = Some(target_head_id);
            }
            _ => {}
        }
    }

    Ok(head_id)
}

/// Background writer task.
///
/// Uses BufWriter for efficient batched writes. Flushes on TurnCommitted and Shutdown.
async fn writer_task(
    path: PathBuf,
    mut rx: mpsc::Receiver<WriterCmd>,
    session_id: Uuid,
    meta: Option<SessionMetaLine>,
) -> std::io::Result<()> {
    let mut current_head_id: Option<Uuid> = if meta.is_some() {
        None
    } else {
        scan_current_head_id(&path).await.unwrap_or_else(|e| {
            warn!("failed to scan existing head_id from {path:?}: {e}");
            None
        })
    };

    // Open file in append mode with buffering.
    let file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .await?;
    let mut writer = BufWriter::new(file);

    // Write session header if this is a new session
    if let Some(session_meta) = meta {
        let entry = create_entry(
            session_id,
            None,
            None,
            EntryKind::SessionHeader { meta: session_meta },
        );
        write_entry(&mut writer, &entry).await?;
        // Flush header immediately to ensure it's visible
        writer.flush().await?;
    }

    // Process commands
    while let Some(cmd) = rx.recv().await {
        match cmd {
            WriterCmd::Append(entry) => {
                if let Err(e) = write_entry(&mut writer, &entry).await {
                    warn!("failed to write log entry: {e}");
                }
            }
            WriterCmd::CommitTurn { turn_id, ack } => {
                let result = (async || -> std::io::Result<()> {
                    // Write TurnCommitted entry
                    let entry = create_entry(
                        session_id,
                        Some(turn_id),
                        current_head_id,
                        EntryKind::TurnCommitted,
                    );
                    write_entry(&mut writer, &entry).await?;
                    current_head_id = Some(entry.id);

                    // Flush buffer to OS
                    writer.flush().await?;

                    // Fsync to disk
                    writer.get_ref().sync_data().await?;

                    Ok(())
                })()
                .await;

                let _ = ack.send(result);
            }
            WriterCmd::Shutdown { ack } => {
                let result = (async || -> std::io::Result<()> {
                    // Drain remaining entries in channel
                    while let Ok(cmd) = rx.try_recv() {
                        if let WriterCmd::Append(entry) = cmd {
                            write_entry(&mut writer, &entry).await?;
                        }
                    }

                    // Flush buffer to OS
                    writer.flush().await?;

                    // Fsync to disk
                    writer.get_ref().sync_data().await?;

                    Ok(())
                })()
                .await;

                let _ = ack.send(result);
                break;
            }
        }
    }

    Ok(())
}

/// Create a LogEntry with the given parameters.
fn create_entry(
    session_id: Uuid,
    turn_id: Option<Uuid>,
    parent_id: Option<Uuid>,
    kind: EntryKind,
) -> LogEntry {
    let timestamp_format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    let timestamp = OffsetDateTime::now_utc()
        .format(timestamp_format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_string());

    LogEntry {
        id: Uuid::new_v4(),
        timestamp,
        session_id,
        turn_id,
        parent_id,
        kind,
    }
}

/// Write a single LogEntry as a JSON line.
async fn write_entry<W: AsyncWrite + Unpin>(
    writer: &mut W,
    entry: &LogEntry,
) -> std::io::Result<()> {
    let json = serde_json::to_string(entry)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::BufRead;
    use std::path::PathBuf;

    use codex_protocol::ConversationId;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::WarningEvent;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn create_test_meta(session_id: Uuid) -> SessionMetaLine {
        SessionMetaLine {
            meta: SessionMeta {
                id: ConversationId::from_string(&session_id.to_string()).unwrap(),
                timestamp: "2025-01-01T00:00:00.000Z".to_string(),
                cwd: PathBuf::from("/tmp/test"),
                originator: "test".to_string(),
                cli_version: "0.0.0-test".to_string(),
                instructions: None,
                source: SessionSource::Cli,
                model: None,
                model_provider: None,
            },
            git: None,
        }
    }

    #[tokio::test]
    async fn test_create_and_write_entries() -> anyhow::Result<()> {
        let temp_dir = TempDir::new()?;
        let session_id = Uuid::new_v4();
        let meta = create_test_meta(session_id);

        let session_log = SessionLog::new(
            temp_dir.path(),
            SessionLogParams::Create { session_id, meta },
        )?;

        // Verify path is in v2 directory structure
        let path = session_log.log_path();
        assert!(path.to_string_lossy().contains("sessions/v2"));
        assert!(path.to_string_lossy().contains(&session_id.to_string()));

        // Append some entries
        let turn_id = Uuid::new_v4();
        let entry1 = create_entry(session_id, Some(turn_id), None, EntryKind::TurnStarted);
        session_log.append(entry1.clone()).await?;

        // Commit the turn (this also writes TurnCommitted)
        session_log.commit_turn(turn_id).await?;

        // Shutdown to ensure all writes are complete
        session_log.shutdown().await?;

        // Read back and verify
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let entries: Vec<LogEntry> = reader
            .lines()
            .filter_map(|line| line.ok())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();

        assert_eq!(entries.len(), 3); // SessionHeader, TurnStarted, TurnCommitted

        // Verify entry kinds
        assert!(matches!(entries[0].kind, EntryKind::SessionHeader { .. }));
        assert!(matches!(entries[1].kind, EntryKind::TurnStarted));
        assert!(matches!(entries[2].kind, EntryKind::TurnCommitted));

        // Verify session_id is consistent
        for entry in &entries {
            assert_eq!(entry.session_id, session_id);
        }

        // Verify turn_id on turn entries
        assert_eq!(entries[1].turn_id, Some(turn_id));
        assert_eq!(entries[2].turn_id, Some(turn_id));

        Ok(())
    }

    #[tokio::test]
    async fn test_resume_session() -> anyhow::Result<()> {
        let temp_dir = TempDir::new()?;
        let session_id = Uuid::new_v4();
        let meta = create_test_meta(session_id);

        // Create initial session
        let session_log = SessionLog::new(
            temp_dir.path(),
            SessionLogParams::Create { session_id, meta },
        )?;

        let turn1_id = Uuid::new_v4();
        let entry1 = create_entry(session_id, Some(turn1_id), None, EntryKind::TurnStarted);
        session_log.append(entry1).await?;
        session_log.commit_turn(turn1_id).await?;
        let path = session_log.log_path().to_path_buf();
        session_log.shutdown().await?;

        // Resume the session
        let resumed = SessionLog::new(
            temp_dir.path(),
            SessionLogParams::Resume {
                path: path.clone(),
                session_id,
            },
        )?;

        // Add more entries
        let turn2_id = Uuid::new_v4();
        let entry2 = create_entry(session_id, Some(turn2_id), None, EntryKind::TurnStarted);
        resumed.append(entry2).await?;
        resumed.commit_turn(turn2_id).await?;
        resumed.shutdown().await?;

        // Read back and verify
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let entries: Vec<LogEntry> = reader
            .lines()
            .filter_map(|line| line.ok())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();

        // Should have: SessionHeader, Turn1Started, Turn1Committed, Turn2Started, Turn2Committed
        assert_eq!(entries.len(), 5);

        // Verify that the active head chain includes both turns after resumption.
        let data = crate::session_log::read_session(&path)?;
        assert_eq!(
            data.entries
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::TurnCommitted))
                .count(),
            2
        );
        assert!(data.entries.iter().any(|e| e.turn_id == Some(turn1_id)));
        assert!(data.entries.iter().any(|e| e.turn_id == Some(turn2_id)));

        Ok(())
    }

    #[tokio::test]
    async fn test_event_entry() -> anyhow::Result<()> {
        let temp_dir = TempDir::new()?;
        let session_id = Uuid::new_v4();
        let meta = create_test_meta(session_id);

        let session_log = SessionLog::new(
            temp_dir.path(),
            SessionLogParams::Create { session_id, meta },
        )?;

        let turn_id = Uuid::new_v4();

        // Create an Event entry
        let event_entry = create_entry(
            session_id,
            Some(turn_id),
            None,
            EntryKind::Event {
                msg: EventMsg::Warning(WarningEvent {
                    message: "test warning".to_string(),
                }),
            },
        );
        session_log.append(event_entry).await?;
        session_log.commit_turn(turn_id).await?;
        session_log.shutdown().await?;

        // Read back
        let file = std::fs::File::open(session_log.log_path())?;
        let reader = std::io::BufReader::new(file);
        let entries: Vec<LogEntry> = reader
            .lines()
            .filter_map(|line| line.ok())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();

        assert_eq!(entries.len(), 3); // Header, Event, TurnCommitted

        // Verify the Event entry
        match &entries[1].kind {
            EntryKind::Event { msg } => {
                assert!(matches!(msg, EventMsg::Warning(_)));
            }
            other => panic!("expected Event, got {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_turns() -> anyhow::Result<()> {
        let temp_dir = TempDir::new()?;
        let session_id = Uuid::new_v4();
        let meta = create_test_meta(session_id);

        let session_log = SessionLog::new(
            temp_dir.path(),
            SessionLogParams::Create { session_id, meta },
        )?;

        // Write multiple turns
        for i in 0..3 {
            let turn_id = Uuid::new_v4();
            let entry = create_entry(session_id, Some(turn_id), None, EntryKind::TurnStarted);
            session_log.append(entry).await?;

            // Simulate some events within the turn
            let event = create_entry(
                session_id,
                Some(turn_id),
                None,
                EntryKind::Event {
                    msg: EventMsg::Warning(WarningEvent {
                        message: format!("turn {i} warning"),
                    }),
                },
            );
            session_log.append(event).await?;

            session_log.commit_turn(turn_id).await?;
        }

        session_log.shutdown().await?;

        // Read back
        let file = std::fs::File::open(session_log.log_path())?;
        let reader = std::io::BufReader::new(file);
        let entries: Vec<LogEntry> = reader
            .lines()
            .filter_map(|line| line.ok())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect();

        // SessionHeader + 3 turns * (TurnStarted + Event + TurnCommitted) = 1 + 9 = 10
        assert_eq!(entries.len(), 10);

        // Count each type
        let headers = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::SessionHeader { .. }))
            .count();
        let turn_starts = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::TurnStarted))
            .count();
        let events = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Event { .. }))
            .count();
        let commits = entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::TurnCommitted))
            .count();

        assert_eq!(headers, 1);
        assert_eq!(turn_starts, 3);
        assert_eq!(events, 3);
        assert_eq!(commits, 3);

        Ok(())
    }
}
