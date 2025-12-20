use codex_file_search::FileMatch;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Span;
use ratatui::widgets::WidgetRef;

use crate::render::Insets;
use crate::render::RectExt;

use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::render_rows;
use codex_common::fuzzy_match::fuzzy_match;

#[derive(Clone, Debug)]
pub(crate) enum MentionItem {
    Profile(String),
    Agent(String),
    File(FileMatch),
}

#[derive(Clone, Debug)]
pub(crate) struct RenderableMentionItem {
    item: MentionItem,
    score: i32,
    indices: Vec<usize>,
}

/// Visual state for the mentions popup (profiles and files).
pub(crate) struct MentionsPopup {
    /// Query corresponding to the `matches` currently shown.
    display_query: String,
    /// Latest query typed by the user. May differ from `display_query` when
    /// a search is still in-flight.
    pending_query: String,
    /// When `true` we are still waiting for results for `pending_query`.
    waiting: bool,
    /// All available profiles (static list).
    profiles: Vec<String>,
    /// All available agents (static list).
    agents: Vec<String>,
    /// Unified sorted matches.
    sorted_matches: Vec<RenderableMentionItem>,
    /// Shared selection/scroll state.
    state: ScrollState,
}

impl MentionsPopup {
    pub(crate) fn new(profiles: Vec<String>, agents: Vec<String>) -> Self {
        Self {
            display_query: String::new(),
            pending_query: String::new(),
            waiting: true,
            profiles,
            agents,
            sorted_matches: Vec::new(),
            state: ScrollState::new(),
        }
    }

    fn combined_len(&self) -> usize {
        self.sorted_matches.len()
    }

    /// Update the query and reset state to *waiting*.
    pub(crate) fn set_query(&mut self, query: &str) {
        if query == self.pending_query {
            return;
        }

        self.pending_query.clear();
        self.pending_query.push_str(query);

        let mut new_matches = Vec::new();

        // Filter profiles
        for p in &self.profiles {
            if let Some((indices, score)) = fuzzy_match(p, query) {
                new_matches.push(RenderableMentionItem {
                    item: MentionItem::Profile(p.clone()),
                    score,
                    indices,
                });
            }
        }

        // Filter agents
        for a in &self.agents {
            if let Some((indices, score)) = fuzzy_match(a, query) {
                new_matches.push(RenderableMentionItem {
                    item: MentionItem::Agent(a.clone()),
                    score,
                    indices,
                });
            }
        }

        new_matches.sort_by(|a, b| a.score.cmp(&b.score));
        self.sorted_matches = new_matches;

        self.waiting = true; // waiting for new results
        self.state.reset();
    }

    /// Put the popup into an "idle" state used for an empty query (just "@").
    /// Shows a hint instead of matches until the user types more characters.
    pub(crate) fn set_empty_prompt(&mut self) {
        self.display_query.clear();
        self.pending_query.clear();
        self.waiting = false;

        let mut new_matches = Vec::new();
        for p in &self.profiles {
            new_matches.push(RenderableMentionItem {
                item: MentionItem::Profile(p.clone()),
                score: 0,
                indices: Vec::new(),
            });
        }
        for a in &self.agents {
            new_matches.push(RenderableMentionItem {
                item: MentionItem::Agent(a.clone()),
                score: 0,
                indices: Vec::new(),
            });
        }

        self.sorted_matches = new_matches;
        self.state.reset();
    }

    /// Replace matches when a `FileSearchResult` arrives.
    /// Replace matches. Only applied when `query` matches `pending_query`.
    pub(crate) fn set_file_matches(&mut self, query: &str, matches: Vec<FileMatch>) {
        if query != self.pending_query {
            return; // stale
        }

        self.display_query = query.to_string();
        self.waiting = false;

        // Remove existing files from sorted_matches (if any)
        self.sorted_matches
            .retain(|m| !matches!(m.item, MentionItem::File(_)));

        // Add new file matches, re-scoring them
        for m in matches {
            if let Some((indices, score)) = fuzzy_match(&m.path, query) {
                self.sorted_matches.push(RenderableMentionItem {
                    item: MentionItem::File(m),
                    score,
                    indices,
                });
            }
        }

        // Re-sort everything
        self.sorted_matches.sort_by(|a, b| a.score.cmp(&b.score));

        let len = self.combined_len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, len.min(MAX_POPUP_ROWS));
    }

    /// Move selection cursor up.
    pub(crate) fn move_up(&mut self) {
        let len = self.combined_len();
        self.state.move_up_wrap(len);
        self.state.ensure_visible(len, len.min(MAX_POPUP_ROWS));
    }

    /// Move selection cursor down.
    pub(crate) fn move_down(&mut self) {
        let len = self.combined_len();
        self.state.move_down_wrap(len);
        self.state.ensure_visible(len, len.min(MAX_POPUP_ROWS));
    }

    pub(crate) fn selected_item(&self) -> Option<MentionItem> {
        let idx = self.state.selected_idx?;
        self.sorted_matches.get(idx).map(|r| r.item.clone())
    }

    pub(crate) fn calculate_required_height(&self) -> u16 {
        // Row count depends on whether we already have matches. If no matches
        // yet (e.g. initial search or query with no results) reserve a single
        // row so the popup is still visible. When matches are present we show
        // up to MAX_RESULTS regardless of the waiting flag so the list
        // remains stable while a newer search is in-flight.

        self.combined_len().clamp(1, MAX_POPUP_ROWS) as u16
    }
}

impl WidgetRef for &MentionsPopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        // Convert matches to GenericDisplayRow, translating indices to usize at the UI boundary.
        let total_len = self.combined_len();
        let rows_all: Vec<GenericDisplayRow> = if total_len == 0 {
            Vec::new()
        } else {
            self.sorted_matches
                .iter()
                .map(|render_item| {
                    let (name, badge) = match &render_item.item {
                        MentionItem::Profile(p) => (
                            p.clone(),
                            Some(Span::styled(
                                "Profile: ",
                                Style::default().fg(Color::Magenta),
                            )),
                        ),
                        MentionItem::Agent(a) => (
                            a.clone(),
                            Some(Span::styled("Agent:   ", Style::default().fg(Color::Cyan))),
                        ),
                        MentionItem::File(m) => (
                            m.path.clone(),
                            Some(Span::styled("File:    ", Style::default().dim())),
                        ),
                    };

                    GenericDisplayRow {
                        name,
                        match_indices: Some(render_item.indices.clone()),
                        display_shortcut: None,
                        description: None,
                        wrap_indent: None,
                        badge,
                    }
                })
                .collect()
        };

        let empty_message = if self.waiting {
            "loading..."
        } else {
            "no matches"
        };

        render_rows(
            area.inset(Insets::tlbr(0, 2, 0, 0)),
            buf,
            &rows_all,
            &self.state,
            MAX_POPUP_ROWS,
            empty_message,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mentions_popup_mixed_content() {
        let profiles = vec!["profile1".to_string(), "profile2".to_string()];
        let agents = vec!["agent1".to_string(), "agent2".to_string()];
        let mut popup = MentionsPopup::new(profiles, agents);

        // Initial state (empty query)
        popup.set_empty_prompt();
        assert_eq!(popup.combined_len(), 4); // 2 profiles + 2 agents

        // Select first item (Profile)
        popup.state.selected_idx = Some(0);
        match popup.selected_item() {
            Some(MentionItem::Profile(p)) => assert_eq!(p, "profile1"),
            _ => panic!("Expected profile1"),
        }

        // Select third item (Agent)
        popup.state.selected_idx = Some(2);
        match popup.selected_item() {
            Some(MentionItem::Agent(a)) => assert_eq!(a, "agent1"),
            _ => panic!("Expected agent1"),
        }
    }
}
