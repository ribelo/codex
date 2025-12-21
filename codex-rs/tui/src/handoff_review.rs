use codex_protocol::protocol::HandoffDraft;
use ratatui::prelude::*;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::path::PathBuf;

/// Widget to display a handoff draft for review.
pub struct HandoffReviewWidget<'a> {
    draft: &'a HandoffDraft,
}

impl<'a> HandoffReviewWidget<'a> {
    pub fn new(draft: &'a HandoffDraft) -> Self {
        Self { draft }
    }
}

impl Widget for HandoffReviewWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        use ratatui::style::Stylize;

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
            .block(Block::default().borders(Borders::BOTTOM))
            .wrap(Wrap { trim: true });
        goal.render(chunks[0], buf);

        // Summary section
        let summary = Paragraph::new(self.draft.summary.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("Extracted Context".dim())
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
        let help = Paragraph::new("[Enter] Confirm  [e] Edit  [Esc] Cancel".dim());
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
        parent_id: original.parent_id.clone(),
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
        };

        let serialized = serialize_draft_to_markdown(&original);
        let parsed = parse_markdown_to_draft(&serialized, &original).unwrap();

        assert_eq!(original.goal, parsed.goal);
        assert_eq!(original.summary, parsed.summary);
        assert_eq!(original.relevant_files, parsed.relevant_files);
        assert_eq!(parent_id, parsed.parent_id);
    }

    #[test]
    fn test_parse_quoted_paths() {
        let parent_id = ConversationId::default();
        let original = HandoffDraft {
            goal: "Goal".to_string(),
            summary: "Summary".to_string(),
            relevant_files: vec![],
            parent_id,
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
