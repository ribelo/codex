use std::io::Result;
use std::time::Duration;

use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use codex_core::session_log::graph::CommitGraph;
use codex_core::session_log::graph::CommitKind;
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Widget;

pub(crate) struct TreeOverlay {
    graph: CommitGraph,
    selected_index: usize,
    is_done: bool,
}

const KEY_UP: KeyBinding = key_hint::plain(KeyCode::Up);
const KEY_DOWN: KeyBinding = key_hint::plain(KeyCode::Down);
const KEY_Q: KeyBinding = key_hint::plain(KeyCode::Char('q'));
const KEY_ESC: KeyBinding = key_hint::plain(KeyCode::Esc);

impl TreeOverlay {
    pub(crate) fn new(graph: CommitGraph) -> Self {
        let head_index = graph
            .head_id
            .and_then(|id| graph.index.get(&id).copied())
            .unwrap_or(0);

        Self {
            graph,
            selected_index: head_index,
            is_done: false,
        }
    }

    pub(crate) fn handle_event(&mut self, tui: &mut Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if KEY_Q.is_press(e) || KEY_ESC.is_press(e) => {
                    self.is_done = true;
                }
                e if KEY_UP.is_press(e) => {
                    self.selected_index = self.selected_index.saturating_sub(1);
                    tui.frame_requester()
                        .schedule_frame_in(Duration::from_millis(16));
                }
                e if KEY_DOWN.is_press(e) => {
                    if !self.graph.nodes.is_empty() {
                        self.selected_index =
                            (self.selected_index + 1).min(self.graph.nodes.len() - 1);
                    }
                    tui.frame_requester()
                        .schedule_frame_in(Duration::from_millis(16));
                }
                _ => {}
            },
            TuiEvent::Draw => {
                tui.draw(u16::MAX, |frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }

    pub(crate) fn render(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        let header_area = Rect::new(area.x, area.y, area.width, 1);
        let content_area = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(2),
        );
        let footer_area = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        );

        self.render_header(header_area, buf);
        self.render_content(content_area, buf);
        self.render_footer(footer_area, buf);
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        let title = " C O M M I T   G R A P H ";
        Line::from(title.bold().reversed())
            .centered()
            .render(area, buf);
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        let hints = vec![
            "↑/↓".bold(),
            " to navigate   ".into(),
            "Enter".bold(),
            " to view   ".into(),
            "Esc/q".bold(),
            " to close".into(),
        ];
        Line::from(hints).centered().render(area, buf);
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer) {
        if self.graph.nodes.is_empty() {
            Line::from("No commits found").centered().render(area, buf);
            return;
        }

        // Simple ASCII tree rendering
        // In the future, this can be improved to handle branching properly with lanes.
        // For now, we'll do a simple list with an indicator for the head and selection.

        let start_index = self.selected_index.saturating_sub(area.height as usize / 2);
        let end_index = (start_index + area.height as usize).min(self.graph.nodes.len());

        for (i, node_idx) in (start_index..end_index).enumerate() {
            let node = &self.graph.nodes[node_idx];
            let is_selected = node_idx == self.selected_index;
            let is_head = self.graph.head_id == Some(node.id);

            let mut spans = Vec::new();

            // Tree structure (simplified)
            if i > 0 {
                spans.push(" | ".dim());
            } else if start_index > 0 {
                spans.push("...".dim());
            } else {
                spans.push("   ".into());
            }

            let marker = if is_head { "*" } else { "o" };
            let mut marker_span = Span::from(marker);
            if is_head {
                marker_span = marker_span.bold().yellow();
            }
            spans.push(marker_span);
            spans.push(" ".into());

            let id_short = &node.id.to_string()[..7];
            spans.push(id_short.dim());
            spans.push(" ".into());

            match &node.kind {
                CommitKind::TurnCommitted { .. } => {
                    spans.push(format!("Turn {}", node_idx + 1).into());
                }
                CommitKind::HeadSet { reason, .. } => {
                    spans.push(format!("↩ {}", reason).magenta());
                }
            }

            if is_head {
                spans.push(" (HEAD)".cyan().bold());
            }

            let line_area = Rect::new(area.x, area.y + i as u16, area.width, 1);
            let mut line = Line::from(spans);
            if is_selected {
                line = line.reversed();
            }
            line.render(line_area, buf);
        }
    }
}
