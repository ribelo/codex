use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use codex_protocol::protocol::HandoffDraft;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::prelude::*;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use std::path::PathBuf;
use tokio_stream::StreamExt;

use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffReviewOutcome {
    Confirm,
    Cancel,
}

/// Run handoff review prompt with built-in edit loop.
/// Returns the final draft (possibly edited) and the outcome.
pub(crate) async fn run_handoff_review_with_edit(
    tui: &mut Tui,
    initial_draft: HandoffDraft,
    available_profiles: &[String],
) -> (HandoffDraft, HandoffReviewOutcome) {
    let mut draft = initial_draft;

    loop {
        match run_handoff_review_prompt_inner(tui, &mut draft, available_profiles).await {
            InnerOutcome::Confirm => return (draft, HandoffReviewOutcome::Confirm),
            InnerOutcome::Cancel => return (draft, HandoffReviewOutcome::Cancel),
            InnerOutcome::Edit => {
                let current_profile = draft.target_profile.clone();
                match spawn_editor_for_draft(tui, &draft) {
                    Ok(mut new_draft) => {
                        new_draft.target_profile = current_profile;
                        draft = new_draft;
                    }
                    Err(e) => {
                        // Show error to user via eprintln since we're outside TUI temporarily
                        eprintln!("\nError editing handoff draft: {e}");
                        eprintln!("Press Enter to continue with original draft...");
                        let _ = std::io::stdin().read_line(&mut String::new());
                    }
                }
                // Loop back to show review again
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InnerOutcome {
    Confirm,
    Edit,
    Cancel,
}

fn spawn_editor_for_draft(tui: &mut Tui, draft: &HandoffDraft) -> Result<HandoffDraft, String> {
    let content = serialize_draft_to_markdown(draft);
    let temp_path =
        std::env::temp_dir().join(format!("codex_handoff_edit_{}.md", std::process::id()));

    std::fs::write(&temp_path, &content).map_err(|e| format!("Failed to write temp file: {e}"))?;

    // Leave raw mode and alternate screen to give editor full control
    let _ = disable_raw_mode();
    let _ = tui.leave_alt_screen();

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let parts: Vec<String> = shlex::split(&editor).unwrap_or_else(|| vec![editor.clone()]);

    let status = if let Some((cmd, args)) = parts.split_first() {
        std::process::Command::new(cmd)
            .args(args)
            .arg(&temp_path)
            .status()
    } else {
        std::process::Command::new("vim").arg(&temp_path).status()
    };

    // Restore raw mode only - alt screen will be re-entered by the calling loop
    let _ = enable_raw_mode();

    let result: Result<HandoffDraft, String> = match status {
        Ok(s) if s.success() => match std::fs::read_to_string(&temp_path) {
            Ok(edited) => match parse_markdown_to_draft(&edited, draft) {
                Ok(new_draft) => Ok(new_draft),
                Err(e) => {
                    tracing::warn!("Failed to parse edited handoff draft: {e}");
                    Err(format!("Failed to parse edited draft: {e}"))
                }
            },
            Err(e) => {
                tracing::warn!("Failed to read edited handoff file: {e}");
                Err(format!("Failed to read edited file: {e}"))
            }
        },
        Ok(s) => {
            tracing::warn!("Editor exited with non-zero status: {s}");
            Err(format!("Editor exited with non-zero status: {s}"))
        }
        Err(e) => {
            tracing::warn!("Failed to spawn editor: {e}");
            Err(format!("Failed to spawn editor: {e}"))
        }
    };

    let _ = std::fs::remove_file(temp_path);
    result
}

async fn run_handoff_review_prompt_inner(
    tui: &mut Tui,
    draft: &mut HandoffDraft,
    available_profiles: &[String],
) -> InnerOutcome {
    struct AltScreenGuard<'a> {
        tui: &'a mut Tui,
    }
    impl<'a> AltScreenGuard<'a> {
        fn enter(tui: &'a mut Tui) -> Self {
            let _ = tui.enter_alt_screen();
            Self { tui }
        }
    }
    impl Drop for AltScreenGuard<'_> {
        fn drop(&mut self) {
            let _ = self.tui.leave_alt_screen();
        }
    }

    let alt = AltScreenGuard::enter(tui);
    let mut screen = HandoffReviewScreen::new(draft, alt.tui.frame_requester(), available_profiles);

    // Initial draw
    let _ = alt.tui.draw(u16::MAX, |frame| {
        screen.render_ref(frame.area(), frame.buffer_mut());
    });

    let events = alt.tui.event_stream();
    tokio::pin!(events);

    while !screen.is_done() {
        if let Some(event) = events.next().await {
            match event {
                TuiEvent::Key(key_event) => screen.handle_key(key_event),
                TuiEvent::Paste(_) => {}
                TuiEvent::Draw => {
                    let _ = alt.tui.draw(u16::MAX, |frame| {
                        screen.render_ref(frame.area(), frame.buffer_mut());
                    });
                }
            }
        } else {
            // Stream ended? Should not happen usually, but cancel if it does.
            screen.cancel();
            break;
        }
    }

    screen.outcome()
}

struct HandoffReviewScreen<'a> {
    draft: &'a mut HandoffDraft,
    request_frame: FrameRequester,
    available_profiles: &'a [String],
    done: bool,
    outcome: InnerOutcome,
    scroll_offset: u16,
}

impl<'a> HandoffReviewScreen<'a> {
    fn new(
        draft: &'a mut HandoffDraft,
        request_frame: FrameRequester,
        available_profiles: &'a [String],
    ) -> Self {
        Self {
            draft,
            request_frame,
            available_profiles,
            done: false,
            outcome: InnerOutcome::Cancel, // Default to cancel
            scroll_offset: 0,
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }

    fn outcome(&self) -> InnerOutcome {
        self.outcome
    }

    fn confirm(&mut self) {
        self.outcome = InnerOutcome::Confirm;
        self.done = true;
        self.request_frame.schedule_frame();
    }

    fn edit(&mut self) {
        self.outcome = InnerOutcome::Edit;
        self.done = true;
        self.request_frame.schedule_frame();
    }

    fn cancel(&mut self) {
        self.outcome = InnerOutcome::Cancel;
        self.done = true;
        self.request_frame.schedule_frame();
    }

    fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
        self.request_frame.schedule_frame();
    }

    fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
        self.request_frame.schedule_frame();
    }

    fn cycle_profile(&mut self) {
        if self.available_profiles.is_empty() {
            return;
        }

        let current_idx = self
            .draft
            .target_profile
            .as_ref()
            .and_then(|p| self.available_profiles.iter().position(|x| x == p));

        let next_idx = match current_idx {
            Some(i) => (i + 1) % (self.available_profiles.len() + 1),
            None => 0,
        };

        if next_idx < self.available_profiles.len() {
            self.draft.target_profile = Some(self.available_profiles[next_idx].clone());
        } else {
            self.draft.target_profile = None;
        }
        self.request_frame.schedule_frame();
    }

    fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Release {
            return;
        }

        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            match key_event.code {
                KeyCode::Char('c') | KeyCode::Char('d') => {
                    self.cancel();
                    return;
                }
                _ => {}
            }
        }

        match key_event.code {
            KeyCode::Enter => self.confirm(),
            KeyCode::Esc | KeyCode::Char('q') => self.cancel(),
            KeyCode::Char('e') => self.edit(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(),
            KeyCode::Char('p') => self.cycle_profile(),
            _ => {}
        }
    }
}

impl WidgetRef for HandoffReviewScreen<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        use ratatui::style::Stylize;

        // Clear background
        Clear.render(area, buf);

        let block = Block::default()
            .title(" Handoff to new thread ")
            .borders(Borders::ALL);
        let inner = block.inner(area);
        block.render(area, buf);

        // Layout: goal, summary, files, help
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Goal
                Constraint::Min(5),    // Summary (takes remaining space)
                Constraint::Length(7), // Files
                Constraint::Length(2), // Help
            ])
            .split(inner);

        // Goal section
        let goal_text = format!("Goal: {}", self.draft.goal);
        let goal = Paragraph::new(goal_text)
            .bold()
            .block(Block::default().borders(Borders::BOTTOM))
            .wrap(Wrap { trim: true });
        goal.render(chunks[0], buf);

        // Summary section
        let summary = Paragraph::new(self.draft.summary.as_str())
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset, 0))
            .block(
                Block::default()
                    .title("Extracted Context (Up/Down to scroll)".dim())
                    .borders(Borders::BOTTOM),
            );
        summary.render(chunks[1], buf);

        // Files section
        let max_display = 5;
        let files = &self.draft.relevant_files;
        let file_lines: Vec<Line> = if files.is_empty() {
            vec!["(no files)".into()]
        } else if files.len() > max_display {
            let mut lines: Vec<Line> = files
                .iter()
                .take(max_display)
                .map(|f| Line::from(vec!["  • ".dim(), f.display().to_string().into()]))
                .collect();
            lines.push(
                format!("  ...and {} more", files.len() - max_display)
                    .dim()
                    .into(),
            );
            lines
        } else {
            files
                .iter()
                .map(|f| Line::from(vec!["  • ".dim(), f.display().to_string().into()]))
                .collect()
        };
        let files_widget = Paragraph::new(file_lines).block(
            Block::default()
                .title("Relevant Files".dim())
                .borders(Borders::BOTTOM),
        );
        files_widget.render(chunks[2], buf);

        // Help section
        let current_profile = self.draft.target_profile.as_deref().unwrap_or("default");
        let help_text = Line::from(vec![
            "[Enter] Confirm  ".dim(),
            "[p] Profile: ".dim(),
            current_profile.white(),
            "  [e] Edit  [Esc] Cancel".dim(),
        ]);
        let help = Paragraph::new(help_text);
        help.render(chunks[3], buf);
    }
}

/// Serialize draft to Markdown format for editing
pub fn serialize_draft_to_markdown(draft: &HandoffDraft) -> String {
    let mut md = String::new();
    md.push_str("# Goal\n");
    md.push_str(&draft.goal);
    md.push_str("\n\n# Summary\n");
    md.push_str(&draft.summary);
    md.push_str("\n\n# Relevant Files\n");
    md.push_str("# One file per line. Lines starting with # are comments.\n");
    for file in &draft.relevant_files {
        md.push_str(&file.display().to_string());
        md.push('\n');
    }
    md
}

fn is_header_match(line: &str, header_name: &str) -> bool {
    let trimmed = line.trim();
    // Must start with at least one #
    if !trimmed.starts_with('#') {
        return false;
    }
    // Strip all leading # and whitespace
    let content = trimmed.trim_start_matches('#').trim();
    // Check if it starts with the header name (case-insensitive)
    content
        .to_ascii_lowercase()
        .starts_with(&header_name.to_ascii_lowercase())
}

/// Parse Markdown back to draft fields (goal, summary, files)
pub fn parse_markdown_to_draft(
    content: &str,
    original: &HandoffDraft,
) -> Result<HandoffDraft, String> {
    let mut goal = String::new();
    let mut summary = String::new();
    let mut files = Vec::new();

    let mut current_section: Option<&str> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if is_header_match(line, "goal") {
            current_section = Some("goal");
        } else if is_header_match(line, "summary") {
            current_section = Some("summary");
        } else if is_header_match(line, "relevant files") || is_header_match(line, "files") {
            current_section = Some("files");
        } else if let Some(section) = current_section {
            match section {
                "goal" => {
                    if !goal.is_empty() {
                        goal.push('\n');
                    }
                    goal.push_str(line);
                }
                "summary" => {
                    if !summary.is_empty() {
                        summary.push('\n');
                    }
                    summary.push_str(line);
                }
                "files" => {
                    if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        // Strip surrounding quotes if present
                        let path_str = if trimmed.len() >= 2
                            && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
                                || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
                        {
                            &trimmed[1..trimmed.len() - 1]
                        } else {
                            trimmed
                        };
                        files.push(PathBuf::from(path_str));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(HandoffDraft {
        goal: goal.trim().to_string(),
        summary: summary.trim().to_string(),
        relevant_files: files,
        parent_id: original.parent_id,
        target_profile: original.target_profile.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ConversationId;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_serialize_parse_roundtrip() {
        let parent_id = ConversationId::default();
        let original = HandoffDraft {
            goal: "Finish the UI".to_string(),
            summary: "I've started working on the TUI components.\nThere are some pending tasks."
                .to_string(),
            relevant_files: vec![
                PathBuf::from("src/main.rs"),
                PathBuf::from("tui/src/app.rs"),
            ],
            parent_id,
            target_profile: Some("test-profile".to_string()),
        };

        let serialized = serialize_draft_to_markdown(&original);
        let parsed = parse_markdown_to_draft(&serialized, &original).unwrap();

        assert_eq!(original.goal, parsed.goal);
        assert_eq!(original.summary, parsed.summary);
        assert_eq!(original.relevant_files, parsed.relevant_files);
        assert_eq!(parent_id, parsed.parent_id);
        assert_eq!(original.target_profile, parsed.target_profile);
    }

    #[test]
    fn test_parse_quoted_paths() {
        let parent_id = ConversationId::default();
        let original = HandoffDraft {
            goal: "Goal".to_string(),
            summary: "Summary".to_string(),
            relevant_files: vec![],
            parent_id,
            target_profile: None,
        };

        let md = "# Goal\nGoal\n\n# Summary\nSummary\n\n# Relevant Files\n\"quoted/path.rs\"\n'single/quoted.rs'\nnormal/path.rs\n";
        let parsed = parse_markdown_to_draft(md, &original).unwrap();

        let expected = vec![
            PathBuf::from("quoted/path.rs"),
            PathBuf::from("single/quoted.rs"),
            PathBuf::from("normal/path.rs"),
        ];
        assert_eq!(parsed.relevant_files, expected);
    }
}
