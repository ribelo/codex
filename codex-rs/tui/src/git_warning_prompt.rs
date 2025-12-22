use crate::tui::FrameRequester;
use crate::tui::Tui;
use crate::tui::TuiEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use tokio_stream::StreamExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitWarningPromptOutcome {
    Continue,
    Exit,
}

pub(crate) async fn run_git_warning_prompt(tui: &mut Tui) -> GitWarningPromptOutcome {
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
    let mut screen = GitWarningScreen::new(alt.tui.frame_requester());

    let _ = alt.tui.draw(u16::MAX, |frame| {
        frame.render_widget_ref(&screen, frame.area());
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
                        frame.render_widget_ref(&screen, frame.area());
                    });
                }
            }
        } else {
            screen.confirm_continue();
            break;
        }
    }

    screen.outcome()
}

struct GitWarningScreen {
    request_frame: FrameRequester,
    lines: Vec<Line<'static>>,
    done: bool,
    exit: bool,
}

impl GitWarningScreen {
    fn new(request_frame: FrameRequester) -> Self {
        let lines: Vec<Line<'static>> = vec![
            Line::from("Current directory is not a git repository".bold().red()),
            Line::from(""),
            Line::from("Subagent file isolation is DISABLED. This means:"),
            Line::from(""),
            Line::from("  - All file changes made by subagents will be applied IMMEDIATELY"),
            Line::from("  - Changes cannot be automatically tracked or merged"),
            Line::from("  - Parallel subagents may conflict without detection"),
            Line::from("  - There is no automatic rollback capability"),
            Line::from(""),
            Line::from("To enable isolation, run 'git init' in your project directory."),
            Line::from(""),
            Line::from("Press Enter to continue without isolation, or Ctrl+C to exit.".dim()),
        ];

        Self {
            request_frame,
            lines,
            done: false,
            exit: false,
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }

    fn confirm_continue(&mut self) {
        self.done = true;
        self.exit = false;
        self.request_frame.schedule_frame();
    }

    fn confirm_exit(&mut self) {
        self.done = true;
        self.exit = true;
        self.request_frame.schedule_frame();
    }

    fn outcome(&self) -> GitWarningPromptOutcome {
        if self.exit {
            GitWarningPromptOutcome::Exit
        } else {
            GitWarningPromptOutcome::Continue
        }
    }

    fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Release {
            return;
        }

        if key_event
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::META)
            && matches!(key_event.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            self.confirm_exit();
            return;
        }

        match key_event.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ') | KeyCode::Char('q') => {
                self.confirm_continue();
            }
            _ => {}
        }
    }
}

impl WidgetRef for &GitWarningScreen {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        let block = Block::default()
            .title("Git Repository Warning".bold().red())
            .borders(Borders::ALL);
        Paragraph::new(self.lines.clone())
            .block(block)
            .wrap(Wrap { trim: true })
            .render(area, buf);
    }
}
