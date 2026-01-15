//! Listing and discovery of v2 session log files.
//!
//! Directory layout: `~/.codex/sessions/v2/YYYY/MM/DD/session-{timestamp}-{session_id}.jsonl`

use std::cmp::Reverse;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;

use time::OffsetDateTime;
use time::PrimitiveDateTime;
use time::format_description::FormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use uuid::Uuid;

use super::SESSIONS_V2_SUBDIR;
use crate::model_provider_info::parse_canonical_model_id;
use codex_protocol::protocol::EntryKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::LogEntry;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;

use codex_protocol::ConversationId;

/// Returned page of v2 session summaries.
#[derive(Debug, Default, PartialEq)]
pub struct SessionsV2Page {
    /// Session summaries ordered newest first.
    pub items: Vec<SessionV2Item>,
    /// Opaque pagination token to resume after the last item, or `None` if end.
    pub next_cursor: Option<CursorV2>,
    /// Total number of files touched while scanning this request.
    pub num_scanned_files: usize,
    /// True if a hard scan cap was hit; consider resuming with `next_cursor`.
    pub reached_scan_cap: bool,
}

/// Summary information for a v2 session log file.
#[derive(Debug, PartialEq)]
pub struct SessionV2Item {
    /// Absolute path to the session log file.
    pub path: PathBuf,
    /// Session ID.
    pub session_id: Uuid,
    /// First user message preview text.
    pub preview: String,
    /// CWD from session metadata.
    pub cwd: Option<PathBuf>,
    /// Git branch from session metadata.
    pub git_branch: Option<String>,
    /// Session source (Cli, VSCode, etc.).
    pub source: Option<SessionSource>,
    /// Parent session ID (for subagents).
    pub parent_id: Option<Uuid>,
    /// Model provider ID.
    pub model_provider: Option<String>,
    /// RFC3339 timestamp for when the session was created.
    pub created_at: Option<String>,
    /// RFC3339 timestamp for the most recent update (from file mtime).
    pub updated_at: Option<String>,
}

/// Hard cap to bound worst-case work per request.
const MAX_SCAN_FILES: usize = 10000;

/// Bound how much we scan per file when searching for a preview message.
const MAX_PREVIEW_SCAN_LINES: i32 = 500;

/// Bound how much we scan per file looking for the session header.
const MAX_HEADER_SCAN_LINES: i32 = 50;

/// Pagination cursor identifying a file by timestamp and UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorV2 {
    ts: OffsetDateTime,
    id: Uuid,
}

impl CursorV2 {
    fn new(ts: OffsetDateTime, id: Uuid) -> Self {
        Self { ts, id }
    }
}

impl serde::Serialize for CursorV2 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let id = self.id;
        let ts_str = self
            .ts
            .format(&format_description!(
                "[year]-[month]-[day]T[hour]-[minute]-[second]"
            ))
            .map_err(|e| serde::ser::Error::custom(format!("format error: {e}")))?;
        serializer.serialize_str(&format!("{ts_str}|{id}"))
    }
}

impl<'de> serde::Deserialize<'de> for CursorV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_cursor_v2(&s).ok_or_else(|| serde::de::Error::custom("invalid cursor"))
    }
}

/// Parse a cursor token string into a CursorV2.
pub fn parse_cursor_v2(token: &str) -> Option<CursorV2> {
    let (file_ts, uuid_str) = token.split_once('|')?;

    let Ok(uuid) = Uuid::parse_str(uuid_str) else {
        return None;
    };

    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let ts = PrimitiveDateTime::parse(file_ts, format).ok()?.assume_utc();

    Some(CursorV2::new(ts, uuid))
}

/// List direct children of a parent session (for subagent browsing).
/// Scans all v2 session files and filters by parent_id.
pub async fn list_session_children_v2(
    codex_home: &Path,
    parent_id: ConversationId,
    page_size: usize,
    cursor: Option<&CursorV2>,
) -> io::Result<SessionsV2Page> {
    let mut root = codex_home.to_path_buf();
    root.push(SESSIONS_V2_SUBDIR);

    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Ok(SessionsV2Page {
            items: Vec::new(),
            next_cursor: None,
            num_scanned_files: 0,
            reached_scan_cap: false,
        });
    }

    let parent_uuid = parent_id.as_uuid();
    let anchor = cursor.cloned();
    let page = traverse_v2_directories_with_cap(
        root,
        page_size,
        anchor,
        Some(parent_uuid),
        &[],
        None,
        MAX_SCAN_FILES,
    )
    .await?;

    if page.reached_scan_cap {
        tracing::warn!(
            parent_id = %parent_uuid,
            scanned_files = page.num_scanned_files,
            max_scan_files = MAX_SCAN_FILES,
            "Reached scan cap while listing session children; results may be incomplete"
        );
    }

    Ok(page)
}

/// Retrieve v2 session file paths with token pagination.
pub async fn list_sessions_v2(
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&CursorV2>,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    default_provider: &str,
) -> io::Result<SessionsV2Page> {
    let mut root = codex_home.to_path_buf();
    root.push(SESSIONS_V2_SUBDIR);

    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Ok(SessionsV2Page {
            items: Vec::new(),
            next_cursor: None,
            num_scanned_files: 0,
            reached_scan_cap: false,
        });
    }

    let anchor = cursor.cloned();
    let provider_matcher =
        model_providers.and_then(|filters| ProviderMatcher::new(filters, default_provider));

    traverse_v2_directories(
        root,
        page_size,
        anchor,
        None,
        allowed_sources,
        provider_matcher.as_ref(),
    )
    .await
}

/// Find the v2 session log file path for a given session ID.
///
/// Scans `~/.codex/sessions/v2/` directories (newest first) looking for a file
/// with the matching UUID in its filename. Returns the first match.
pub async fn find_session_log_v2_by_id(
    codex_home: &Path,
    session_id: Uuid,
) -> io::Result<Option<PathBuf>> {
    let mut root = codex_home.to_path_buf();
    root.push(SESSIONS_V2_SUBDIR);

    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Ok(None);
    }

    let year_dirs = collect_dirs_desc(&root, |s| s.parse::<i32>().ok()).await?;

    for (_year, year_path) in year_dirs.iter() {
        let month_dirs = collect_dirs_desc(year_path, |s| s.parse::<i32>().ok()).await?;
        for (_month, month_path) in month_dirs.iter() {
            let day_dirs = collect_dirs_desc(month_path, |s| s.parse::<i32>().ok()).await?;
            for (_day, day_path) in day_dirs.iter() {
                let day_files = collect_files(day_path, |name_str, path| {
                    // v2 files: session-YYYY-MM-DDThh-mm-ss-{uuid}.jsonl
                    if !name_str.starts_with("session-") || !name_str.ends_with(".jsonl") {
                        return None;
                    }
                    parse_timestamp_uuid_from_v2_filename(name_str)
                        .filter(|(_, id)| *id == session_id)
                        .map(|_| path.to_path_buf())
                })
                .await?;

                if let Some(path) = day_files.into_iter().next() {
                    return Ok(Some(path));
                }
            }
        }
    }

    Ok(None)
}

/// Load v2 session file paths from disk using directory traversal.
async fn traverse_v2_directories(
    root: PathBuf,
    page_size: usize,
    anchor: Option<CursorV2>,
    required_parent_id: Option<Uuid>,
    allowed_sources: &[SessionSource],
    provider_matcher: Option<&ProviderMatcher<'_>>,
) -> io::Result<SessionsV2Page> {
    traverse_v2_directories_with_cap(
        root,
        page_size,
        anchor,
        required_parent_id,
        allowed_sources,
        provider_matcher,
        MAX_SCAN_FILES,
    )
    .await
}

async fn traverse_v2_directories_with_cap(
    root: PathBuf,
    page_size: usize,
    anchor: Option<CursorV2>,
    required_parent_id: Option<Uuid>,
    allowed_sources: &[SessionSource],
    provider_matcher: Option<&ProviderMatcher<'_>>,
    max_scan_files: usize,
) -> io::Result<SessionsV2Page> {
    let mut items: Vec<SessionV2Item> = Vec::with_capacity(page_size);
    let mut scanned_files = 0usize;
    let mut anchor_passed = anchor.is_none();
    let (anchor_ts, anchor_id) = match anchor {
        Some(c) => (c.ts, c.id),
        None => (OffsetDateTime::UNIX_EPOCH, Uuid::nil()),
    };
    let mut more_matches_available = false;
    let mut reached_scan_cap = false;
    let mut last_scanned_cursor: Option<CursorV2> = None;

    let year_dirs = collect_dirs_desc(&root, |s| s.parse::<i32>().ok()).await?;

    'outer: for (_year, year_path) in year_dirs.iter() {
        let month_dirs = collect_dirs_desc(year_path, |s| s.parse::<i32>().ok()).await?;
        for (_month, month_path) in month_dirs.iter() {
            let day_dirs = collect_dirs_desc(month_path, |s| s.parse::<i32>().ok()).await?;
            for (_day, day_path) in day_dirs.iter() {
                let mut day_files = collect_files(day_path, |name_str, path| {
                    // v2 files: session-YYYY-MM-DDThh-mm-ss-{uuid}.jsonl
                    if !name_str.starts_with("session-") || !name_str.ends_with(".jsonl") {
                        return None;
                    }
                    parse_timestamp_uuid_from_v2_filename(name_str)
                        .map(|(ts, id)| (ts, id, path.to_path_buf()))
                })
                .await?;
                // Stable ordering: (timestamp desc, uuid desc)
                day_files.sort_by_key(|(ts, sid, _path)| (Reverse(*ts), Reverse(*sid)));

                for (ts, sid, path) in day_files.into_iter() {
                    if !anchor_passed {
                        if ts < anchor_ts || (ts == anchor_ts && sid < anchor_id) {
                            anchor_passed = true;
                        } else {
                            continue;
                        }
                    }

                    if items.len() == page_size {
                        more_matches_available = true;
                        break 'outer;
                    }

                    if scanned_files == max_scan_files {
                        reached_scan_cap = true;
                        more_matches_available = true;
                        break 'outer;
                    }
                    scanned_files += 1;

                    last_scanned_cursor = Some(CursorV2::new(ts, sid));

                    let Some(summary) = read_v2_session_summary(
                        &path,
                        required_parent_id,
                        allowed_sources,
                        provider_matcher,
                    )
                    .await
                    else {
                        continue;
                    };

                    items.push(summary);
                }
            }
        }
    }

    let next_cursor = if more_matches_available {
        last_scanned_cursor
    } else {
        None
    };

    Ok(SessionsV2Page {
        items,
        next_cursor,
        num_scanned_files: scanned_files,
        reached_scan_cap,
    })
}

/// Read a v2 session file and extract summary information.
async fn read_v2_session_summary(
    path: &Path,
    required_parent_id: Option<Uuid>,
    allowed_sources: &[SessionSource],
    provider_matcher: Option<&ProviderMatcher<'_>>,
) -> Option<SessionV2Item> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut lines = BufReader::new(file).lines();

    let header_meta = read_session_header(&mut lines).await?;

    let session_id = header_meta.meta.id.as_uuid();
    let cwd = Some(header_meta.meta.cwd.clone());
    let git_branch = header_meta.git.as_ref().and_then(|g| g.branch.clone());
    let source = Some(header_meta.meta.source.clone());
    let parent_id = header_meta.meta.source.parent_id().map(|id| id.as_uuid());
    let created_at = Some(header_meta.meta.timestamp.clone());

    if let Some(required_parent_id) = required_parent_id
        && parent_id != Some(required_parent_id)
    {
        return None;
    }

    if !allowed_sources.is_empty()
        && !source
            .as_ref()
            .is_some_and(|source| allowed_sources.iter().any(|candidate| candidate == source))
    {
        return None;
    }

    // Extract model provider
    let model_provider = header_meta
        .meta
        .model
        .as_deref()
        .and_then(|m| parse_canonical_model_id(m).ok())
        .map(|(provider, _)| provider.to_string())
        .or_else(|| header_meta.meta.model_provider.clone());

    if let Some(matcher) = provider_matcher
        && !matcher.matches(model_provider.as_deref())
    {
        return None;
    }

    // Find first user message for preview
    let preview = extract_first_user_message_from_lines(&mut lines).await;
    if preview.is_empty() {
        return None;
    }

    // Get updated_at from file mtime
    let updated_at = tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .map(OffsetDateTime::from)
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .or_else(|| created_at.clone());

    Some(SessionV2Item {
        path: path.to_path_buf(),
        session_id,
        preview,
        cwd,
        git_branch,
        source,
        parent_id,
        model_provider,
        created_at,
        updated_at,
    })
}

async fn read_session_header<R>(lines: &mut tokio::io::Lines<R>) -> Option<SessionMetaLine>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut scanned_lines = 0i32;
    loop {
        if scanned_lines == MAX_HEADER_SCAN_LINES {
            return None;
        }
        scanned_lines += 1;

        let line = lines.next_line().await.ok()??;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: LogEntry = serde_json::from_str(trimmed).ok()?;
        return match entry.kind {
            EntryKind::SessionHeader { meta } => Some(meta),
            _ => None,
        };
    }
}

async fn extract_first_user_message_from_lines<R>(lines: &mut tokio::io::Lines<R>) -> String
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut scanned_lines = 0i32;
    loop {
        if scanned_lines == MAX_PREVIEW_SCAN_LINES {
            return String::new();
        }
        scanned_lines += 1;

        let Some(line) = lines.next_line().await.ok().flatten() else {
            return String::new();
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<LogEntry>(trimmed) else {
            continue;
        };
        match &entry.kind {
            EntryKind::Event { msg } => {
                if let EventMsg::UserMessage(user_msg) = msg {
                    let text = user_msg.message.trim();
                    if !text.is_empty() {
                        return text.to_string();
                    }
                }
            }
            EntryKind::ResponseItem { item } => {
                if let codex_protocol::models::ResponseItem::Message { role, content, .. } = item
                    && role == "user"
                {
                    for c in content {
                        if let codex_protocol::models::ContentItem::InputText { text } = c {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                return trimmed.to_string();
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Collects immediate subdirectories of `parent`, parses their (string) names with `parse`,
/// and returns them sorted descending by the parsed key.
async fn collect_dirs_desc<T, F>(parent: &Path, parse: F) -> io::Result<Vec<(T, PathBuf)>>
where
    T: Ord + Copy,
    F: Fn(&str) -> Option<T>,
{
    let mut dir = tokio::fs::read_dir(parent).await?;
    let mut vec: Vec<(T, PathBuf)> = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        if entry
            .file_type()
            .await
            .map(|ft| ft.is_dir())
            .unwrap_or(false)
            && let Some(s) = entry.file_name().to_str()
            && let Some(v) = parse(s)
        {
            vec.push((v, entry.path()));
        }
    }
    vec.sort_by_key(|(v, _)| Reverse(*v));
    Ok(vec)
}

/// Collects files in a directory and parses them with `parse`.
async fn collect_files<T, F>(parent: &Path, parse: F) -> io::Result<Vec<T>>
where
    F: Fn(&str, &Path) -> Option<T>,
{
    let mut dir = tokio::fs::read_dir(parent).await?;
    let mut collected: Vec<T> = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        if entry
            .file_type()
            .await
            .map(|ft| ft.is_file())
            .unwrap_or(false)
            && let Some(s) = entry.file_name().to_str()
            && let Some(v) = parse(s, &entry.path())
        {
            collected.push(v);
        }
    }
    Ok(collected)
}

/// Parse timestamp and UUID from v2 session filename.
/// Expected format: session-YYYY-MM-DDThh-mm-ss-{uuid}.jsonl
fn parse_timestamp_uuid_from_v2_filename(name: &str) -> Option<(OffsetDateTime, Uuid)> {
    let core = name.strip_prefix("session-")?.strip_suffix(".jsonl")?;

    // Scan from the right for a '-' such that the suffix parses as a UUID.
    let (sep_idx, uuid) = core
        .match_indices('-')
        .rev()
        .find_map(|(i, _)| Uuid::parse_str(&core[i + 1..]).ok().map(|u| (i, u)))?;

    let ts_str = &core[..sep_idx];
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let ts = PrimitiveDateTime::parse(ts_str, format).ok()?.assume_utc();
    Some((ts, uuid))
}

struct ProviderMatcher<'a> {
    filters: &'a [String],
    matches_default_provider: bool,
}

impl<'a> ProviderMatcher<'a> {
    fn new(filters: &'a [String], default_provider: &'a str) -> Option<Self> {
        if filters.is_empty() {
            return None;
        }
        let matches_default_provider = filters.iter().any(|provider| provider == default_provider);
        Some(Self {
            filters,
            matches_default_provider,
        })
    }

    fn matches(&self, session_provider: Option<&str>) -> bool {
        match session_provider {
            Some(provider) => self.filters.iter().any(|candidate| candidate == provider),
            None => self.matches_default_provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use time::Month;

    fn write_session_header_only(
        path: &Path,
        session_id: Uuid,
        timestamp: &str,
        source: SessionSource,
    ) -> io::Result<()> {
        let meta = codex_protocol::protocol::SessionMeta {
            id: ConversationId::from_string(&session_id.to_string()).expect("valid uuid"),
            timestamp: timestamp.to_string(),
            cwd: PathBuf::from("/tmp"),
            originator: String::new(),
            cli_version: String::new(),
            instructions: None,
            source,
            model: None,
            model_provider: None,
        };

        let entry = LogEntry {
            id: Uuid::new_v4(),
            timestamp: timestamp.to_string(),
            session_id,
            turn_id: None,
            parent_id: None,
            kind: EntryKind::SessionHeader {
                meta: SessionMetaLine { meta, git: None },
            },
        };

        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
    }

    #[test]
    fn test_parse_timestamp_uuid_from_v2_filename() {
        let name = "session-2025-01-15T14-30-45-12345678-1234-5678-9abc-def012345678.jsonl";
        let result = parse_timestamp_uuid_from_v2_filename(name);
        assert!(result.is_some());
        let (ts, uuid) = result.unwrap();
        assert_eq!(ts.year(), 2025);
        assert_eq!(ts.month(), Month::January);
        assert_eq!(i32::from(ts.day()), 15);
        assert_eq!(i32::from(ts.hour()), 14);
        assert_eq!(i32::from(ts.minute()), 30);
        assert_eq!(i32::from(ts.second()), 45);
        assert_eq!(uuid.to_string(), "12345678-1234-5678-9abc-def012345678");
    }

    #[test]
    fn test_parse_cursor_v2() {
        let token = "2025-01-15T14-30-45|12345678-1234-5678-9abc-def012345678";
        let cursor = parse_cursor_v2(token);
        assert!(cursor.is_some());
        let c = cursor.unwrap();
        assert_eq!(c.ts.year(), 2025);
        assert_eq!(c.id.to_string(), "12345678-1234-5678-9abc-def012345678");
    }

    #[test]
    fn test_parse_cursor_v2_invalid() {
        assert!(parse_cursor_v2("invalid").is_none());
        assert!(parse_cursor_v2("2025-01-15T14-30-45|not-a-uuid").is_none());
        assert!(parse_cursor_v2("not-a-date|12345678-1234-5678-9abc-def012345678").is_none());
    }

    #[tokio::test]
    async fn list_sessions_v2_paginates_when_scan_cap_hits_with_no_matches() -> io::Result<()> {
        let tmp = TempDir::new()?;
        let mut root = tmp.path().to_path_buf();
        root.push(SESSIONS_V2_SUBDIR);
        let day_dir = root.join("2025").join("01").join("15");
        std::fs::create_dir_all(&day_dir)?;

        let file_names = vec![
            "session-2025-01-15T14-30-45-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl",
            "session-2025-01-15T14-30-44-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.jsonl",
            "session-2025-01-15T14-30-43-cccccccc-cccc-cccc-cccc-cccccccccccc.jsonl",
            "session-2025-01-15T14-30-42-dddddddd-dddd-dddd-dddd-dddddddddddd.jsonl",
            "session-2025-01-15T14-30-41-eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee.jsonl",
        ];

        for name in &file_names {
            let (ts, session_id) = parse_timestamp_uuid_from_v2_filename(name).expect("parse");
            let ts = ts.format(&Rfc3339).expect("timestamp");
            let path = day_dir.join(name);
            write_session_header_only(&path, session_id, &ts, SessionSource::VSCode)?;
        }

        let allowed_sources = vec![SessionSource::Cli];

        let page_1 = traverse_v2_directories_with_cap(
            root.clone(),
            10,
            None,
            None,
            &allowed_sources,
            None,
            2,
        )
        .await?;
        assert_eq!(page_1.items.len(), 0);
        assert_eq!(page_1.num_scanned_files, 2);
        assert_eq!(page_1.reached_scan_cap, true);
        let cursor_1 = page_1.next_cursor.clone().expect("cursor");

        let page_2 = traverse_v2_directories_with_cap(
            root.clone(),
            10,
            Some(cursor_1.clone()),
            None,
            &allowed_sources,
            None,
            2,
        )
        .await?;
        assert_eq!(page_2.items.len(), 0);
        assert_eq!(page_2.num_scanned_files, 2);
        assert_eq!(page_2.reached_scan_cap, true);
        let cursor_2 = page_2.next_cursor.clone().expect("cursor");
        assert_ne!(cursor_1, cursor_2);

        let page_3 = traverse_v2_directories_with_cap(
            root.clone(),
            10,
            Some(cursor_2),
            None,
            &allowed_sources,
            None,
            2,
        )
        .await?;
        assert_eq!(page_3.items.len(), 0);
        assert_eq!(page_3.num_scanned_files, 1);
        assert_eq!(page_3.reached_scan_cap, false);
        assert_eq!(page_3.next_cursor, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_find_session_log_v2_by_id() -> io::Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path();
        let mut root = codex_home.to_path_buf();
        root.push(SESSIONS_V2_SUBDIR);
        let day_dir = root.join("2025").join("01").join("15");
        std::fs::create_dir_all(&day_dir)?;

        let session_id = Uuid::new_v4();
        let filename = format!("session-2025-01-15T14-30-45-{session_id}.jsonl");
        let path = day_dir.join(filename);
        std::fs::write(&path, "{}")?;

        let found = find_session_log_v2_by_id(codex_home, session_id).await?;
        assert_eq!(found, Some(path));

        let not_found = find_session_log_v2_by_id(codex_home, Uuid::new_v4()).await?;
        assert_eq!(not_found, None);

        Ok(())
    }
}
