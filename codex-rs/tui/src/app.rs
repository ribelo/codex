use crate::app_backtrack::BacktrackState;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ApprovalRequest;
use crate::chatwidget::ChatWidget;
use crate::diff_render::DiffSummary;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::file_search::FileSearchManager;
use crate::handoff_review::HandoffReviewOutcome;
use crate::handoff_review::run_handoff_review_with_edit;
use crate::history_cell::HistoryCell;
use crate::pager_overlay::Overlay;
use crate::render::highlight::highlight_bash_to_lines;
use crate::render::renderable::Renderable;
use crate::resume_picker::ResumeSelection;
use crate::skill_error_prompt::SkillErrorPromptOutcome;
use crate::skill_error_prompt::run_skill_error_prompt;
use crate::subagent_error_prompt::SubagentErrorPromptOutcome;
use crate::subagent_error_prompt::run_subagent_error_prompt;
use crate::tui;
use crate::tui::TuiEvent;
use crate::update_action::UpdateAction;
use codex_ansi_escape::ansi_escape_line;
use codex_core::AuthManager;
use codex_core::ConversationManager;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::edit::ConfigEditsBuilder;
#[cfg(target_os = "windows")]
use codex_core::features::Feature;
use codex_core::protocol::EventMsg;
use codex_core::protocol::FinalOutput;
use codex_core::protocol::Op;
use codex_core::protocol::SessionSource;
use codex_core::protocol::SkillLoadOutcomeInfo;
use codex_core::protocol::TokenUsage;
use codex_core::skills::SkillError;
use codex_core::subagents::load_subagents;
use codex_protocol::ConversationId;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;

#[cfg(not(debug_assertions))]
use crate::history_cell::UpdateAvailableHistoryCell;

#[derive(Debug, Clone)]
pub struct AppExitInfo {
    pub profile: Option<String>,
    pub token_usage: TokenUsage,
    pub conversation_id: Option<ConversationId>,
    pub update_action: Option<UpdateAction>,
}

fn session_summary(
    token_usage: TokenUsage,
    conversation_id: Option<ConversationId>,
    profile: Option<String>,
) -> Option<SessionSummary> {
    if token_usage.is_zero() {
        return None;
    }

    let usage_line = FinalOutput::from(token_usage).to_string();
    let resume_command = conversation_id.map(|id| {
        if let Some(ref p) = profile {
            format!("codex --profile {p} resume {id}")
        } else {
            format!("codex resume {id}")
        }
    });
    Some(SessionSummary {
        usage_line,
        resume_command,
    })
}

fn skill_errors_from_outcome(outcome: &SkillLoadOutcomeInfo) -> Vec<SkillError> {
    outcome
        .errors
        .iter()
        .map(|err| SkillError {
            path: err.path.clone(),
            message: err.message.clone(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSummary {
    usage_line: String,
    resume_command: Option<String>,
}

pub(crate) struct App {
    pub(crate) server: Arc<ConversationManager>,
    pub(crate) app_event_tx: AppEventSender,
    pub(crate) chat_widget: ChatWidget,
    pub(crate) auth_manager: Arc<AuthManager>,
    /// Config is stored here so we can recreate ChatWidgets as needed.
    pub(crate) config: Config,
    pub(crate) current_model: String,
    pub(crate) active_profile: Option<String>,
    pub(crate) available_profiles: Vec<String>,
    pub(crate) available_agents: Vec<String>,
    /// Stored to allow reloading config with a different profile.
    config_overrides: ConfigOverrides,
    /// Stored to allow reloading config with a different profile.
    cli_kv_overrides: Vec<(String, toml::Value)>,

    pub(crate) file_search: FileSearchManager,

    pub(crate) transcript_cells: Vec<Arc<dyn HistoryCell>>,

    // Pager overlay state (Transcript or Static like Diff)
    pub(crate) overlay: Option<Overlay>,
    pub(crate) deferred_history_lines: Vec<Line<'static>>,
    has_emitted_history_lines: bool,

    pub(crate) enhanced_keys_supported: bool,

    /// Controls the animation thread that sends CommitTick events.
    pub(crate) commit_anim_running: Arc<AtomicBool>,

    // Esc-backtracking state grouped
    pub(crate) backtrack: crate::app_backtrack::BacktrackState,
    pub(crate) feedback: codex_feedback::CodexFeedback,
    /// Set when the user confirms an update; propagated on exit.
    pub(crate) pending_update_action: Option<UpdateAction>,

    /// Ignore the next ShutdownComplete event when we're intentionally
    /// stopping a conversation (e.g., before starting a new one).
    suppress_shutdown_complete: bool,

    /// Exit after receiving ShutdownComplete when the user requests exit.
    exit_on_shutdown_complete: bool,

    // One-shot suppression of the next world-writable scan after user confirmation.
    skip_world_writable_scan_once: bool,
}

#[derive(Debug, Clone, Copy)]
enum ShutdownCurrentConversationMode {
    ReplaceConversation,
    Exit,
}

impl App {
    async fn shutdown_current_conversation(&mut self, mode: ShutdownCurrentConversationMode) {
        if let Some(conversation_id) = self.chat_widget.conversation_id() {
            match mode {
                ShutdownCurrentConversationMode::ReplaceConversation => {
                    self.suppress_shutdown_complete = true;
                    self.chat_widget.submit_op(Op::Shutdown);
                    self.server.remove_conversation(&conversation_id).await;
                }
                ShutdownCurrentConversationMode::Exit => {
                    if self.exit_on_shutdown_complete {
                        return;
                    }
                    self.exit_on_shutdown_complete = true;
                    self.chat_widget.submit_op(Op::Shutdown);
                    let tx = self.app_event_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        tx.send(AppEvent::ExitTimeout);
                    });
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        tui: &mut tui::Tui,
        auth_manager: Arc<AuthManager>,
        config: Config,
        active_profile: Option<String>,
        available_profiles: Vec<String>,
        available_agents: Vec<String>,
        config_overrides: ConfigOverrides,
        cli_kv_overrides: Vec<(String, toml::Value)>,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        resume_selection: ResumeSelection,
        feedback: codex_feedback::CodexFeedback,
        is_first_run: bool,
    ) -> Result<AppExitInfo> {
        use tokio_stream::StreamExt;
        let (app_event_tx, mut app_event_rx) = unbounded_channel();
        let app_event_tx = AppEventSender::new(app_event_tx);

        let conversation_manager = Arc::new(ConversationManager::new(
            auth_manager.clone(),
            SessionSource::Cli,
            config.codex_home.clone(),
        ));
        let mut model = conversation_manager
            .get_models_manager()
            .get_model(&config.model, &config)
            .await;
        if let Some(updated_model) = config.model.clone() {
            model = updated_model;
        }

        let subagent_outcome = load_subagents(&config.codex_home, &config).await;
        if !subagent_outcome.errors.is_empty() {
            match run_subagent_error_prompt(tui, &subagent_outcome.errors).await {
                SubagentErrorPromptOutcome::Exit => {
                    return Ok(AppExitInfo {
                        token_usage: TokenUsage::default(),
                        conversation_id: None,
                        update_action: None,
                        profile: None,
                    });
                }
                SubagentErrorPromptOutcome::Continue => {}
            }
        }

        let enhanced_keys_supported = tui.enhanced_keys_supported();
        let model_family = conversation_manager
            .get_models_manager()
            .construct_model_family(model.as_str(), &config)
            .await;
        let mut chat_widget = match resume_selection {
            ResumeSelection::StartFresh
            | ResumeSelection::Exit
            | ResumeSelection::ViewTranscript(_) => {
                let init = crate::chatwidget::ChatWidgetInit {
                    config: config.clone(),
                    frame_requester: tui.frame_requester(),
                    app_event_tx: app_event_tx.clone(),
                    initial_prompt: initial_prompt.clone(),
                    initial_images: initial_images.clone(),
                    enhanced_keys_supported,
                    auth_manager: auth_manager.clone(),
                    models_manager: conversation_manager.get_models_manager(),
                    feedback: feedback.clone(),
                    is_first_run,
                    available_profiles: available_profiles.clone(),
                    available_agents: available_agents.clone(),
                    model_family: model_family.clone(),
                };
                ChatWidget::new(init, conversation_manager.clone())
            }
            ResumeSelection::Resume(path) => {
                let resumed = conversation_manager
                    .resume_conversation_from_rollout(
                        config.clone(),
                        path.clone(),
                        auth_manager.clone(),
                    )
                    .await
                    .wrap_err_with(|| {
                        format!("Failed to resume session from {}", path.display())
                    })?;
                let init = crate::chatwidget::ChatWidgetInit {
                    config: config.clone(),
                    frame_requester: tui.frame_requester(),
                    app_event_tx: app_event_tx.clone(),
                    initial_prompt: initial_prompt.clone(),
                    initial_images: initial_images.clone(),
                    enhanced_keys_supported,
                    auth_manager: auth_manager.clone(),
                    models_manager: conversation_manager.get_models_manager(),
                    feedback: feedback.clone(),
                    is_first_run,
                    available_profiles: available_profiles.clone(),
                    available_agents: available_agents.clone(),
                    model_family: model_family.clone(),
                };
                ChatWidget::new_from_existing(
                    init,
                    resumed.conversation,
                    resumed.session_configured,
                )
            }
        };

        chat_widget.maybe_prompt_windows_sandbox_enable();

        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());
        #[cfg(not(debug_assertions))]
        let upgrade_version = crate::updates::get_upgrade_version(&config);

        let mut app = Self {
            server: conversation_manager.clone(),
            app_event_tx,
            chat_widget,
            auth_manager: auth_manager.clone(),
            config,
            current_model: model.clone(),
            active_profile,
            available_profiles,
            available_agents,
            config_overrides,
            cli_kv_overrides,
            file_search,
            enhanced_keys_supported,
            transcript_cells: Vec::new(),
            overlay: None,
            deferred_history_lines: Vec::new(),
            has_emitted_history_lines: false,
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            backtrack: BacktrackState::default(),
            feedback: feedback.clone(),
            pending_update_action: None,
            suppress_shutdown_complete: false,
            exit_on_shutdown_complete: false,
            skip_world_writable_scan_once: false,
        };

        // On startup, if Agent mode (workspace-write) or ReadOnly is active, warn about world-writable dirs on Windows.
        #[cfg(target_os = "windows")]
        {
            let should_check = codex_core::get_platform_sandbox().is_some()
                && matches!(
                    app.config.sandbox_policy,
                    codex_core::protocol::SandboxPolicy::WorkspaceWrite { .. }
                        | codex_core::protocol::SandboxPolicy::ReadOnly
                )
                && !app
                    .config
                    .notices
                    .hide_world_writable_warning
                    .unwrap_or(false);
            if should_check {
                let cwd = app.config.cwd.clone();
                let env_map: std::collections::HashMap<String, String> = std::env::vars().collect();
                let tx = app.app_event_tx.clone();
                let logs_base_dir = app.config.codex_home.clone();
                let sandbox_policy = app.config.sandbox_policy.clone();
                Self::spawn_world_writable_scan(cwd, env_map, logs_base_dir, sandbox_policy, tx);
            }
        }

        #[cfg(not(debug_assertions))]
        if let Some(latest_version) = upgrade_version {
            app.handle_event(
                tui,
                AppEvent::InsertHistoryCell(Box::new(UpdateAvailableHistoryCell::new(
                    latest_version,
                    crate::update_action::get_update_action(),
                ))),
            )
            .await?;
        }

        let tui_events = tui.event_stream();
        tokio::pin!(tui_events);

        tui.frame_requester().schedule_frame();

        while select! {
            Some(event) = app_event_rx.recv() => {
                app.handle_event(tui, event).await?
            }
            Some(event) = tui_events.next() => {
                app.handle_tui_event(tui, event).await?
            }
        } {}
        tui.terminal.clear()?;
        Ok(AppExitInfo {
            token_usage: app.token_usage(),
            conversation_id: app.chat_widget.conversation_id(),
            update_action: app.pending_update_action,
            profile: app.active_profile.clone(),
        })
    }

    /// Returns `true` to keep the main run loop alive, and `false` to exit.
    pub(crate) async fn handle_tui_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> Result<bool> {
        if self.overlay.is_some() {
            let _ = self.handle_backtrack_overlay_event(tui, event).await?;
        } else {
            match event {
                TuiEvent::Key(key_event) => {
                    self.handle_key_event(tui, key_event).await;
                }
                TuiEvent::Paste(pasted) => {
                    // Many terminals convert newlines to \r when pasting (e.g., iTerm2),
                    // but tui-textarea expects \n. Normalize CR to LF.
                    // [tui-textarea]: https://github.com/rhysd/tui-textarea/blob/4d18622eeac13b309e0ff6a55a46ac6706da68cf/src/textarea.rs#L782-L783
                    // [iTerm2]: https://github.com/gnachman/iTerm2/blob/5d0c0d9f68523cbd0494dad5422998964a2ecd8d/sources/iTermPasteHelper.m#L206-L216
                    let pasted = pasted.replace("\r", "\n");
                    self.chat_widget.handle_paste(pasted);
                }
                TuiEvent::Draw => {
                    self.chat_widget.maybe_post_pending_notification(tui);
                    if self
                        .chat_widget
                        .handle_paste_burst_tick(tui.frame_requester())
                    {
                        return Ok(true);
                    }
                    tui.draw(
                        self.chat_widget.desired_height(tui.terminal.size()?.width),
                        |frame| {
                            self.chat_widget.render(frame.area(), frame.buffer);
                            if let Some((x, y)) = self.chat_widget.cursor_pos(frame.area()) {
                                frame.set_cursor_position((x, y));
                            }
                        },
                    )?;
                }
            }
        }
        Ok(true)
    }

    /// Returns `true` to keep the main run loop alive, and `false` to exit.
    async fn handle_event(&mut self, tui: &mut tui::Tui, event: AppEvent) -> Result<bool> {
        let model_family = self
            .server
            .get_models_manager()
            .construct_model_family(self.current_model.as_str(), &self.config)
            .await;
        match event {
            AppEvent::NewSession => {
                let summary = session_summary(
                    self.chat_widget.token_usage(),
                    self.chat_widget.conversation_id(),
                    self.active_profile.clone(),
                );
                self.shutdown_current_conversation(
                    ShutdownCurrentConversationMode::ReplaceConversation,
                )
                .await;
                let init = crate::chatwidget::ChatWidgetInit {
                    config: self.config.clone(),
                    frame_requester: tui.frame_requester(),
                    app_event_tx: self.app_event_tx.clone(),
                    initial_prompt: None,
                    initial_images: Vec::new(),
                    enhanced_keys_supported: self.enhanced_keys_supported,
                    auth_manager: self.auth_manager.clone(),
                    models_manager: self.server.get_models_manager(),
                    feedback: self.feedback.clone(),
                    is_first_run: false,
                    available_profiles: self.available_profiles.clone(),
                    available_agents: self.available_agents.clone(),
                    model_family: model_family.clone(),
                };
                self.chat_widget = ChatWidget::new(init, self.server.clone());
                self.current_model = model_family.get_model_slug().to_string();
                if let Some(summary) = summary {
                    let mut lines: Vec<Line<'static>> = vec![summary.usage_line.clone().into()];
                    if let Some(command) = summary.resume_command {
                        let spans = vec!["To continue this session, run ".into(), command.cyan()];
                        lines.push(spans.into());
                    }
                    self.chat_widget.add_plain_history_lines(lines);
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::NewSessionWithProfile { profile_name } => {
                // Create new overrides with the selected profile.
                let mut new_overrides = self.config_overrides.clone();
                new_overrides.config_profile = Some(profile_name.clone());

                // Try to load config with the new profile BEFORE shutting down the current session.
                // This ensures we don't lose the current conversation if the new profile fails to load.
                match Config::load_with_cli_overrides(
                    self.cli_kv_overrides.clone(),
                    new_overrides.clone(),
                )
                .await
                {
                    Ok(new_config) => {
                        // Config loaded successfully - now safe to shut down the current session.
                        let summary = session_summary(
                            self.chat_widget.token_usage(),
                            self.chat_widget.conversation_id(),
                            self.active_profile.clone(),
                        );
                        self.shutdown_current_conversation(
                            ShutdownCurrentConversationMode::ReplaceConversation,
                        )
                        .await;

                        self.config = new_config;
                        self.active_profile = Some(profile_name.clone());
                        self.config_overrides = new_overrides;

                        // Recreate file search manager with the new config's cwd.
                        self.file_search = FileSearchManager::new(
                            self.config.cwd.clone(),
                            self.app_event_tx.clone(),
                        );

                        let init = crate::chatwidget::ChatWidgetInit {
                            config: self.config.clone(),
                            frame_requester: tui.frame_requester(),
                            app_event_tx: self.app_event_tx.clone(),
                            initial_prompt: None,
                            initial_images: Vec::new(),
                            enhanced_keys_supported: self.enhanced_keys_supported,
                            auth_manager: self.auth_manager.clone(),
                            models_manager: self.server.get_models_manager(),
                            feedback: self.feedback.clone(),
                            is_first_run: false,
                            available_profiles: self.available_profiles.clone(),
                            available_agents: self.available_agents.clone(),
                            model_family: model_family.clone(),
                        };
                        self.chat_widget = ChatWidget::new(init, self.server.clone());

                        // Show info about the profile switch.
                        let info_line: Line<'static> =
                            format!("Switched to profile: {profile_name}").into();
                        self.chat_widget.add_plain_history_lines(vec![info_line]);

                        if let Some(summary) = summary {
                            let mut lines: Vec<Line<'static>> =
                                vec![summary.usage_line.clone().into()];
                            if let Some(command) = summary.resume_command {
                                let spans =
                                    vec!["To continue this session, run ".into(), command.cyan()];
                                lines.push(spans.into());
                            }
                            self.chat_widget.add_plain_history_lines(lines);
                        }
                    }
                    Err(err) => {
                        tracing::error!(
                            "Failed to reload config with profile {profile_name}: {err}"
                        );
                        // Show error but keep the current session intact.
                        self.chat_widget.add_info_message(
                            format!("Failed to switch to profile '{profile_name}': {err}"),
                            None,
                        );
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenResumePicker => {
                match crate::resume_picker::run_resume_picker(
                    tui,
                    &self.config.codex_home,
                    &self.config.model_provider_id,
                    crate::resume_picker::PickerMode::Resume { show_all: false },
                )
                .await?
                {
                    ResumeSelection::Resume(path) => {
                        let summary = session_summary(
                            self.chat_widget.token_usage(),
                            self.chat_widget.conversation_id(),
                            self.active_profile.clone(),
                        );
                        match self
                            .server
                            .resume_conversation_from_rollout(
                                self.config.clone(),
                                path.clone(),
                                self.auth_manager.clone(),
                            )
                            .await
                        {
                            Ok(resumed) => {
                                self.shutdown_current_conversation(
                                    ShutdownCurrentConversationMode::ReplaceConversation,
                                )
                                .await;
                                let init = crate::chatwidget::ChatWidgetInit {
                                    config: self.config.clone(),
                                    frame_requester: tui.frame_requester(),
                                    app_event_tx: self.app_event_tx.clone(),
                                    initial_prompt: None,
                                    initial_images: Vec::new(),
                                    enhanced_keys_supported: self.enhanced_keys_supported,
                                    auth_manager: self.auth_manager.clone(),
                                    models_manager: self.server.get_models_manager(),
                                    feedback: self.feedback.clone(),
                                    is_first_run: false,
                                    available_profiles: self.available_profiles.clone(),
                                    available_agents: self.available_agents.clone(),
                                    model_family: model_family.clone(),
                                };
                                self.chat_widget = ChatWidget::new_from_existing(
                                    init,
                                    resumed.conversation,
                                    resumed.session_configured,
                                );
                                self.current_model = model_family.get_model_slug().to_string();
                                if let Some(summary) = summary {
                                    let mut lines: Vec<Line<'static>> =
                                        vec![summary.usage_line.clone().into()];
                                    if let Some(command) = summary.resume_command {
                                        let spans = vec![
                                            "To continue this session, run ".into(),
                                            command.cyan(),
                                        ];
                                        lines.push(spans.into());
                                    }
                                    self.chat_widget.add_plain_history_lines(lines);
                                }
                            }
                            Err(err) => {
                                self.chat_widget.add_error_message(format!(
                                    "Failed to resume session from {}: {err}",
                                    path.display()
                                ));
                            }
                        }
                    }
                    ResumeSelection::Exit
                    | ResumeSelection::StartFresh
                    | ResumeSelection::ViewTranscript(_) => {}
                }

                // Leaving alt-screen may blank the inline viewport; force a redraw either way.
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenChildrenPicker(parent_id) => {
                match crate::resume_picker::run_resume_picker(
                    tui,
                    &self.config.codex_home,
                    &self.config.model_provider_id,
                    crate::resume_picker::PickerMode::Children { parent_id },
                )
                .await?
                {
                    ResumeSelection::ViewTranscript(path) => {
                        // Load rollout and display in transcript overlay
                        match crate::resume_picker::rollout_to_cells(&path).await {
                            Ok(cells) => {
                                if cells.is_empty() {
                                    tracing::info!(
                                        "No transcript content found in {}",
                                        path.display()
                                    );
                                } else {
                                    let _ = tui.enter_alt_screen();
                                    self.overlay = Some(Overlay::new_transcript(cells));
                                    tui.frame_requester().schedule_frame();
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to load transcript from {}: {}",
                                    path.display(),
                                    e
                                );
                            }
                        }
                    }
                    ResumeSelection::Resume(path) => {
                        // Shouldn't happen in Children mode, but handle anyway
                        tracing::warn!("Resume selected in children mode: {}", path.display());
                    }
                    ResumeSelection::Exit | ResumeSelection::StartFresh => {}
                }

                // Leaving alt-screen may blank the viewport; force a redraw
                tui.frame_requester().schedule_frame();
            }
            AppEvent::InsertHistoryCell(cell) => {
                let cell: Arc<dyn HistoryCell> = cell.into();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_cell(cell.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.transcript_cells.push(cell.clone());
                let mut display = cell.display_lines(tui.terminal.last_known_screen_size.width);
                if !display.is_empty() {
                    // Only insert a separating blank line for new cells that are not
                    // part of an ongoing stream. Streaming continuations should not
                    // accrue extra blank lines between chunks.
                    if !cell.is_stream_continuation() {
                        if self.has_emitted_history_lines {
                            display.insert(0, Line::from(""));
                        } else {
                            self.has_emitted_history_lines = true;
                        }
                    }
                    if self.overlay.is_some() {
                        self.deferred_history_lines.extend(display);
                    } else {
                        tui.insert_history_lines(display);
                    }
                }
            }
            AppEvent::StartCommitAnimation => {
                if self
                    .commit_anim_running
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let tx = self.app_event_tx.clone();
                    let running = self.commit_anim_running.clone();
                    thread::spawn(move || {
                        while running.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                            tx.send(AppEvent::CommitTick);
                        }
                    });
                }
            }
            AppEvent::StopCommitAnimation => {
                self.commit_anim_running.store(false, Ordering::Release);
            }
            AppEvent::CommitTick => {
                self.chat_widget.on_commit_tick();
            }
            AppEvent::ExitTimeout => {
                if self.exit_on_shutdown_complete {
                    tracing::warn!("Graceful shutdown timed out; exiting anyway");
                    return Ok(false);
                }
            }
            AppEvent::CodexEvent(event) => {
                if self.exit_on_shutdown_complete && matches!(event.msg, EventMsg::ShutdownComplete)
                {
                    return Ok(false);
                }
                if self.suppress_shutdown_complete
                    && matches!(event.msg, EventMsg::ShutdownComplete)
                {
                    self.suppress_shutdown_complete = false;
                    return Ok(true);
                }
                if let EventMsg::HandoffDraft(event) = &event.msg {
                    // Create the draft and run the review prompt
                    let draft = codex_protocol::protocol::HandoffDraft {
                        summary: event.summary.clone(),
                        goal: event.goal.clone(),
                        relevant_files: event.relevant_files.clone(),
                        parent_id: event.parent_id,
                        target_profile: None,
                    };

                    let (final_draft, outcome) =
                        run_handoff_review_with_edit(tui, draft, &self.available_profiles).await;

                    match outcome {
                        HandoffReviewOutcome::Confirm => {
                            let mut handoff_config = self.config.clone();
                            let mut switched_profile = false;

                            if let Some(profile) = &final_draft.target_profile {
                                let mut overrides = self.config_overrides.clone();
                                overrides.config_profile = Some(profile.clone());
                                match Config::load_with_cli_overrides(
                                    self.cli_kv_overrides.clone(),
                                    overrides,
                                )
                                .await
                                {
                                    Ok(cfg) => {
                                        handoff_config = cfg;
                                        switched_profile = true;
                                    }
                                    Err(e) => {
                                        let msg =
                                            format!("Failed to load profile '{profile}': {e}");
                                        tracing::error!("{msg}");
                                        self.chat_widget.add_error_message(msg);
                                    }
                                }
                            }

                            let new_model = handoff_config
                                .model
                                .clone()
                                .unwrap_or_else(|| self.current_model.clone());
                            // Get model family for the new session
                            let model_family = self
                                .server
                                .get_models_manager()
                                .construct_model_family(&new_model, &handoff_config)
                                .await;

                            // Create the new handoff conversation
                            match self
                                .server
                                .create_handoff_conversation(
                                    final_draft.clone(),
                                    handoff_config.clone(),
                                )
                                .await
                            {
                                Ok(handoff_result) => {
                                    self.chat_widget.submit_op(Op::ConfirmHandoff {
                                        draft: final_draft.clone(),
                                        child_id: handoff_result.conversation.conversation_id,
                                    });

                                    // Save summary of current session
                                    let summary = session_summary(
                                        self.chat_widget.token_usage(),
                                        self.chat_widget.conversation_id(),
                                        self.active_profile.clone(),
                                    );

                                    // Shutdown current conversation
                                    self.shutdown_current_conversation(
                                        ShutdownCurrentConversationMode::ReplaceConversation,
                                    )
                                    .await;

                                    // Create new ChatWidget with the handoff conversation
                                    let init = crate::chatwidget::ChatWidgetInit {
                                        config: handoff_config.clone(),
                                        frame_requester: tui.frame_requester(),
                                        app_event_tx: self.app_event_tx.clone(),
                                        initial_prompt: Some(handoff_result.handoff_message),
                                        initial_images: Vec::new(),
                                        enhanced_keys_supported: self.enhanced_keys_supported,
                                        auth_manager: self.auth_manager.clone(),
                                        models_manager: self.server.get_models_manager(),
                                        feedback: self.feedback.clone(),
                                        is_first_run: false,
                                        available_profiles: self.available_profiles.clone(),
                                        available_agents: self.available_agents.clone(),
                                        model_family: model_family.clone(),
                                    };

                                    self.chat_widget = ChatWidget::new_from_existing(
                                        init,
                                        handoff_result.conversation.conversation,
                                        handoff_result.conversation.session_configured,
                                    );
                                    self.current_model = model_family.get_model_slug().to_string();

                                    if switched_profile {
                                        self.config = handoff_config;
                                        self.active_profile = final_draft.target_profile.clone();
                                    }

                                    // Show previous session summary
                                    if let Some(summary) = summary {
                                        let mut lines: Vec<Line<'static>> =
                                            vec![summary.usage_line.clone().into()];
                                        if let Some(command) = summary.resume_command {
                                            let spans = vec![
                                                "To continue previous session, run ".into(),
                                                command.cyan(),
                                            ];
                                            lines.push(spans.into());
                                        }
                                        self.chat_widget.add_plain_history_lines(lines);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Handoff failed: {e}");
                                    self.chat_widget
                                        .add_error_message(format!("Handoff failed: {e}"));
                                }
                            }
                        }
                        HandoffReviewOutcome::Cancel => {
                            self.chat_widget.submit_op(Op::CancelHandoff);
                        }
                    }
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }
                if let EventMsg::SessionConfigured(cfg) = &event.msg
                    && let Some(outcome) = cfg.skill_load_outcome.as_ref()
                    && !outcome.errors.is_empty()
                {
                    let errors = skill_errors_from_outcome(outcome);
                    match run_skill_error_prompt(tui, &errors).await {
                        SkillErrorPromptOutcome::Exit => {
                            self.shutdown_current_conversation(
                                ShutdownCurrentConversationMode::Exit,
                            )
                            .await;
                            return Ok(true);
                        }
                        SkillErrorPromptOutcome::Continue => {}
                    }
                }
                self.chat_widget.handle_codex_event(event);
            }
            AppEvent::ConversationHistory(ev) => {
                self.on_conversation_history_for_backtrack(tui, ev).await?;
            }
            AppEvent::ExitRequest => {
                if self.chat_widget.conversation_id().is_some() {
                    self.shutdown_current_conversation(ShutdownCurrentConversationMode::Exit)
                        .await;
                    return Ok(true);
                }
                return Ok(false);
            }
            AppEvent::CodexOp(op) => self.chat_widget.submit_op(op),
            AppEvent::DiffResult(text) => {
                // Clear the in-progress state in the bottom pane
                self.chat_widget.on_diff_complete();
                // Enter alternate screen using TUI helper and build pager lines
                let _ = tui.enter_alt_screen();
                let pager_lines: Vec<ratatui::text::Line<'static>> = if text.trim().is_empty() {
                    vec!["No changes detected.".italic().into()]
                } else {
                    text.lines().map(ansi_escape_line).collect()
                };
                self.overlay = Some(Overlay::new_static_with_lines(
                    pager_lines,
                    "D I F F".to_string(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::TreeResult(graph) => {
                let _ = tui.enter_alt_screen();
                self.overlay = Some(Overlay::new_tree(graph));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::StartFileSearch(query) => {
                if !query.is_empty() {
                    self.file_search.on_user_query(query);
                }
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.chat_widget.apply_file_search_result(query, matches);
            }
            AppEvent::RateLimitSnapshotFetched(snapshot) => {
                self.chat_widget.on_rate_limit_snapshot(Some(snapshot));
            }
            AppEvent::UpdateReasoningEffort(effort) => {
                self.on_update_reasoning_effort(effort);
            }
            AppEvent::UpdateModel(model) => {
                let model_family = self
                    .server
                    .get_models_manager()
                    .construct_model_family(&model, &self.config)
                    .await;
                self.chat_widget.set_model(&model, model_family);
                self.current_model = model;
            }
            AppEvent::OpenReasoningPopup { model } => {
                self.chat_widget.open_reasoning_popup(model);
            }
            AppEvent::OpenAllModelsPopup { models } => {
                self.chat_widget.open_all_models_popup(models);
            }
            AppEvent::OpenWorldWritableWarningConfirmation {
                preset,
                sample_paths,
                extra_count,
                failed_scan,
            } => {
                self.chat_widget.open_world_writable_warning_confirmation(
                    preset,
                    sample_paths,
                    extra_count,
                    failed_scan,
                );
            }
            AppEvent::OpenFeedbackNote {
                category,
                include_logs,
            } => {
                self.chat_widget.open_feedback_note(category, include_logs);
            }
            AppEvent::OpenFeedbackConsent { category } => {
                self.chat_widget.open_feedback_consent(category);
            }
            AppEvent::OpenWindowsSandboxEnablePrompt { preset } => {
                self.chat_widget.open_windows_sandbox_enable_prompt(preset);
            }
            AppEvent::EnableWindowsSandboxForAgentMode { preset } => {
                #[cfg(target_os = "windows")]
                {
                    let profile = self.active_profile.as_deref();
                    let feature_key = Feature::WindowsSandbox.key();
                    match ConfigEditsBuilder::new(&self.config.codex_home)
                        .with_profile(profile)
                        .set_feature_enabled(feature_key, true)
                        .apply()
                        .await
                    {
                        Ok(()) => {
                            self.config.set_windows_sandbox_globally(true);
                            self.chat_widget.clear_forced_auto_mode_downgrade();
                            if let Some((sample_paths, extra_count, failed_scan)) =
                                self.chat_widget.world_writable_warning_details()
                            {
                                self.app_event_tx.send(
                                    AppEvent::OpenWorldWritableWarningConfirmation {
                                        preset: Some(preset.clone()),
                                        sample_paths,
                                        extra_count,
                                        failed_scan,
                                    },
                                );
                            } else {
                                self.app_event_tx.send(AppEvent::CodexOp(
                                    Op::OverrideTurnContext {
                                        cwd: None,
                                        approval_policy: Some(preset.approval),
                                        sandbox_policy: Some(preset.sandbox.clone()),
                                        model: None,
                                        effort: None,
                                        summary: None,
                                    },
                                ));
                                self.app_event_tx
                                    .send(AppEvent::UpdateAskForApprovalPolicy(preset.approval));
                                self.app_event_tx
                                    .send(AppEvent::UpdateSandboxPolicy(preset.sandbox.clone()));
                                self.chat_widget.add_info_message(
                                    "Enabled experimental Windows sandbox.".to_string(),
                                    None,
                                );
                            }
                        }
                        Err(err) => {
                            tracing::error!(
                                error = %err,
                                "failed to enable Windows sandbox feature"
                            );
                            self.chat_widget.add_error_message(format!(
                                "Failed to enable the Windows sandbox feature: {err}"
                            ));
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = preset;
                }
            }
            AppEvent::PersistModelSelection { model, effort } => {
                let profile = self.active_profile.as_deref();
                match ConfigEditsBuilder::new(&self.config.codex_home)
                    .with_profile(profile)
                    .set_model(Some(model.as_str()), effort)
                    .apply()
                    .await
                {
                    Ok(()) => {
                        let mut message = format!("Model changed to {model}");
                        if let Some(label) = Self::reasoning_label_for(&model, effort) {
                            message.push(' ');
                            message.push_str(label);
                        }
                        if let Some(profile) = profile {
                            message.push_str(" for ");
                            message.push_str(profile);
                            message.push_str(" profile");
                        }
                        self.chat_widget.add_info_message(message, None);
                    }
                    Err(err) => {
                        tracing::error!(
                            error = %err,
                            "failed to persist model selection"
                        );
                        if let Some(profile) = profile {
                            self.chat_widget.add_error_message(format!(
                                "Failed to save model for profile `{profile}`: {err}"
                            ));
                        } else {
                            self.chat_widget
                                .add_error_message(format!("Failed to save default model: {err}"));
                        }
                    }
                }
            }
            AppEvent::UpdateAskForApprovalPolicy(policy) => {
                self.chat_widget.set_approval_policy(policy);
            }
            AppEvent::UpdateSandboxPolicy(policy) => {
                #[cfg(target_os = "windows")]
                let policy_is_workspace_write_or_ro = matches!(
                    policy,
                    codex_core::protocol::SandboxPolicy::WorkspaceWrite { .. }
                        | codex_core::protocol::SandboxPolicy::ReadOnly
                );

                self.config.sandbox_policy = policy.clone();
                #[cfg(target_os = "windows")]
                if !matches!(policy, codex_core::protocol::SandboxPolicy::ReadOnly)
                    || codex_core::get_platform_sandbox().is_some()
                {
                    self.config.forced_auto_mode_downgraded_on_windows = false;
                }
                self.chat_widget.set_sandbox_policy(policy);

                // If sandbox policy becomes workspace-write or read-only, run the Windows world-writable scan.
                #[cfg(target_os = "windows")]
                {
                    // One-shot suppression if the user just confirmed continue.
                    if self.skip_world_writable_scan_once {
                        self.skip_world_writable_scan_once = false;
                        return Ok(true);
                    }

                    let should_check = codex_core::get_platform_sandbox().is_some()
                        && policy_is_workspace_write_or_ro
                        && !self.chat_widget.world_writable_warning_hidden();
                    if should_check {
                        let cwd = self.config.cwd.clone();
                        let env_map: std::collections::HashMap<String, String> =
                            std::env::vars().collect();
                        let tx = self.app_event_tx.clone();
                        let logs_base_dir = self.config.codex_home.clone();
                        let sandbox_policy = self.config.sandbox_policy.clone();
                        Self::spawn_world_writable_scan(
                            cwd,
                            env_map,
                            logs_base_dir,
                            sandbox_policy,
                            tx,
                        );
                    }
                }
            }
            AppEvent::SkipNextWorldWritableScan => {
                self.skip_world_writable_scan_once = true;
            }
            AppEvent::UpdateWorldWritableWarningAcknowledged(ack) => {
                self.chat_widget
                    .set_world_writable_warning_acknowledged(ack);
            }
            AppEvent::UpdateRateLimitSwitchPromptHidden(hidden) => {
                self.chat_widget.set_rate_limit_switch_prompt_hidden(hidden);
            }
            AppEvent::PersistWorldWritableWarningAcknowledged => {
                if let Err(err) = ConfigEditsBuilder::new(&self.config.codex_home)
                    .set_hide_world_writable_warning(true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist world-writable warning acknowledgement"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save Agent mode warning preference: {err}"
                    ));
                }
            }
            AppEvent::PersistRateLimitSwitchPromptHidden => {
                if let Err(err) = ConfigEditsBuilder::new(&self.config.codex_home)
                    .set_hide_rate_limit_model_nudge(true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist rate limit switch prompt preference"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save rate limit reminder preference: {err}"
                    ));
                }
            }
            AppEvent::OpenApprovalsPopup => {
                self.chat_widget.open_approvals_popup();
            }
            AppEvent::OpenReviewBranchPicker(cwd) => {
                self.chat_widget.show_review_branch_picker(&cwd).await;
            }
            AppEvent::OpenReviewCommitPicker(cwd) => {
                self.chat_widget.show_review_commit_picker(&cwd).await;
            }
            AppEvent::OpenReviewCustomPrompt => {
                self.chat_widget.show_review_custom_prompt();
            }
            AppEvent::DelegateAgent {
                description,
                prompt,
                agent,
            } => {
                self.chat_widget.submit_op(Op::DelegateSubagent {
                    description,
                    prompt,
                    agent,
                });
            }
            AppEvent::FullScreenApprovalRequest(request) => match request {
                ApprovalRequest::ApplyPatch { cwd, changes, .. } => {
                    let _ = tui.enter_alt_screen();
                    let diff_summary = DiffSummary::new(changes, cwd);
                    self.overlay = Some(Overlay::new_static_with_renderables(
                        vec![diff_summary.into()],
                        "P A T C H".to_string(),
                    ));
                }
                ApprovalRequest::Exec { command, .. } => {
                    let _ = tui.enter_alt_screen();
                    let full_cmd = strip_bash_lc_and_escape(&command);
                    let full_cmd_lines = highlight_bash_to_lines(&full_cmd);
                    self.overlay = Some(Overlay::new_static_with_lines(
                        full_cmd_lines,
                        "E X E C".to_string(),
                    ));
                }
                ApprovalRequest::McpElicitation {
                    server_name,
                    message,
                    ..
                } => {
                    let _ = tui.enter_alt_screen();
                    let paragraph = Paragraph::new(vec![
                        Line::from(vec!["Server: ".into(), server_name.bold()]),
                        Line::from(""),
                        Line::from(message),
                    ])
                    .wrap(Wrap { trim: false });
                    self.overlay = Some(Overlay::new_static_with_renderables(
                        vec![Box::new(paragraph)],
                        "E L I C I T A T I O N".to_string(),
                    ));
                }
            },
        }
        Ok(true)
    }

    fn reasoning_label(reasoning_effort: Option<ReasoningEffortConfig>) -> &'static str {
        match reasoning_effort {
            Some(ReasoningEffortConfig::Minimal) => "minimal",
            Some(ReasoningEffortConfig::Low) => "low",
            Some(ReasoningEffortConfig::Medium) => "medium",
            Some(ReasoningEffortConfig::High) => "high",
            Some(ReasoningEffortConfig::XHigh) => "xhigh",
            None | Some(ReasoningEffortConfig::None) => "default",
        }
    }

    fn reasoning_label_for(
        model: &str,
        reasoning_effort: Option<ReasoningEffortConfig>,
    ) -> Option<&'static str> {
        (!model.starts_with("codex-auto-")).then(|| Self::reasoning_label(reasoning_effort))
    }

    pub(crate) fn token_usage(&self) -> codex_core::protocol::TokenUsage {
        self.chat_widget.token_usage()
    }

    fn on_update_reasoning_effort(&mut self, effort: Option<ReasoningEffortConfig>) {
        self.chat_widget.set_reasoning_effort(effort);
        self.config.model_reasoning_effort = effort;
    }

    async fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: crossterm::event::KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            } => {
                // Enter alternate screen and set viewport to full size.
                let _ = tui.enter_alt_screen();
                // Include both completed transcript cells and active worker tasks
                let mut cells = self.transcript_cells.clone();
                cells.extend(self.chat_widget.active_subagent_cells_as_history());
                self.overlay = Some(Overlay::new_transcript(cells));
                tui.frame_requester().schedule_frame();
            }
            // Esc primes/advances backtracking only in normal (not working) mode
            // with the composer focused and empty. In any other state, forward
            // Esc so the active UI (e.g. status indicator, modals, popups)
            // handles it.
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if self.chat_widget.is_normal_backtrack_mode()
                    && self.chat_widget.composer_is_empty()
                {
                    self.handle_backtrack_esc_key(tui);
                } else {
                    self.chat_widget.handle_key_event(key_event);
                }
            }
            // Enter confirms backtrack when primed + count > 0. Otherwise pass to widget.
            KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            } if self.backtrack.primed
                && self.backtrack.nth_backtrack_position != usize::MAX
                && self.chat_widget.composer_is_empty() =>
            {
                // Delegate to helper for clarity; preserves behavior.
                self.confirm_backtrack_from_main();
            }
            KeyEvent {
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                // Any non-Esc key press should cancel a primed backtrack.
                // This avoids stale "Esc-primed" state after the user starts typing
                // (even if they later backspace to empty).
                if key_event.code != KeyCode::Esc && self.backtrack.primed {
                    self.reset_backtrack_state();
                }
                self.chat_widget.handle_key_event(key_event);
            }
            _ => {
                // Ignore Release key events.
            }
        };
    }

    #[cfg(target_os = "windows")]
    fn spawn_world_writable_scan(
        cwd: PathBuf,
        env_map: std::collections::HashMap<String, String>,
        logs_base_dir: PathBuf,
        sandbox_policy: codex_core::protocol::SandboxPolicy,
        tx: AppEventSender,
    ) {
        tokio::task::spawn_blocking(move || {
            let result = codex_windows_sandbox::apply_world_writable_scan_and_denies(
                &logs_base_dir,
                &cwd,
                &env_map,
                &sandbox_policy,
                Some(logs_base_dir.as_path()),
            );
            if result.is_err() {
                // Scan failed: warn without examples.
                tx.send(AppEvent::OpenWorldWritableWarningConfirmation {
                    preset: None,
                    sample_paths: Vec::new(),
                    extra_count: 0usize,
                    failed_scan: true,
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_backtrack::BacktrackState;
    use crate::app_backtrack::backtrack_count;
    use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
    use crate::file_search::FileSearchManager;
    use crate::history_cell::AgentMessageCell;
    use crate::history_cell::HistoryCell;
    use crate::history_cell::UserHistoryCell;
    use crate::history_cell::new_session_info;
    use codex_core::AuthManager;
    use codex_core::CodexAuth;
    use codex_core::ConversationManager;
    use codex_core::protocol::AskForApproval;
    use codex_core::protocol::Event;
    use codex_core::protocol::EventMsg;
    use codex_core::protocol::SandboxPolicy;
    use codex_core::protocol::SessionConfiguredEvent;
    use codex_protocol::ConversationId;
    use ratatui::prelude::Line;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn make_test_app() -> App {
        let (chat_widget, app_event_tx, _rx, _op_rx) = make_chatwidget_manual_with_sender();
        let config = chat_widget.config_ref().clone();
        let current_model = chat_widget.get_model_family().get_model_slug().to_string();
        let server = Arc::new(ConversationManager::with_models_provider(
            CodexAuth::from_api_key("Test API Key"),
            config.model_provider.clone(),
            PathBuf::from("/tmp"),
        ));
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());

        App {
            server,
            app_event_tx,
            chat_widget,
            auth_manager,
            config,
            current_model,
            active_profile: None,
            available_profiles: Vec::new(),
            available_agents: Vec::new(),
            config_overrides: ConfigOverrides::default(),
            cli_kv_overrides: Vec::new(),
            file_search,
            transcript_cells: Vec::new(),
            overlay: None,
            deferred_history_lines: Vec::new(),
            has_emitted_history_lines: false,
            enhanced_keys_supported: false,
            commit_anim_running: Arc::new(AtomicBool::new(false)),
            backtrack: BacktrackState::default(),
            feedback: codex_feedback::CodexFeedback::new(),
            pending_update_action: None,
            suppress_shutdown_complete: false,
            exit_on_shutdown_complete: false,
            skip_world_writable_scan_once: false,
        }
    }

    fn make_test_app_with_channels() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        tokio::sync::mpsc::UnboundedReceiver<Op>,
    ) {
        let (chat_widget, app_event_tx, rx, op_rx) = make_chatwidget_manual_with_sender();
        let config = chat_widget.config_ref().clone();
        let current_model = chat_widget.get_model_family().get_model_slug().to_string();
        let server = Arc::new(ConversationManager::with_models_provider(
            CodexAuth::from_api_key("Test API Key"),
            config.model_provider.clone(),
            PathBuf::from("/tmp"),
        ));
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let file_search = FileSearchManager::new(config.cwd.clone(), app_event_tx.clone());

        (
            App {
                server,
                app_event_tx,
                chat_widget,
                auth_manager,
                config,
                current_model,
                active_profile: None,
                available_profiles: Vec::new(),
                available_agents: Vec::new(),
                config_overrides: ConfigOverrides::default(),
                cli_kv_overrides: Vec::new(),
                file_search,
                transcript_cells: Vec::new(),
                overlay: None,
                deferred_history_lines: Vec::new(),
                has_emitted_history_lines: false,
                enhanced_keys_supported: false,
                commit_anim_running: Arc::new(AtomicBool::new(false)),
                backtrack: BacktrackState::default(),
                feedback: codex_feedback::CodexFeedback::new(),
                pending_update_action: None,
                suppress_shutdown_complete: false,
                exit_on_shutdown_complete: false,
                skip_world_writable_scan_once: false,
            },
            rx,
            op_rx,
        )
    }

    #[test]
    fn update_reasoning_effort_updates_config() {
        let mut app = make_test_app();
        app.config.model_reasoning_effort = Some(ReasoningEffortConfig::Medium);
        app.chat_widget
            .set_reasoning_effort(Some(ReasoningEffortConfig::Medium));

        app.on_update_reasoning_effort(Some(ReasoningEffortConfig::High));

        assert_eq!(
            app.config.model_reasoning_effort,
            Some(ReasoningEffortConfig::High)
        );
        assert_eq!(
            app.chat_widget.config_ref().model_reasoning_effort,
            Some(ReasoningEffortConfig::High)
        );
    }

    #[test]
    fn backtrack_selection_with_duplicate_history_targets_unique_turn() {
        let mut app = make_test_app();

        let user_cell = |text: &str| -> Arc<dyn HistoryCell> {
            Arc::new(UserHistoryCell {
                message: text.to_string(),
            }) as Arc<dyn HistoryCell>
        };
        let agent_cell = |text: &str| -> Arc<dyn HistoryCell> {
            Arc::new(AgentMessageCell::new(
                vec![Line::from(text.to_string())],
                true,
            )) as Arc<dyn HistoryCell>
        };

        let make_header = |is_first| {
            let event = SessionConfiguredEvent {
                session_id: ConversationId::new(),
                model: "gpt-test".to_string(),
                model_provider_id: "test-provider".to_string(),
                approval_policy: AskForApproval::Never,
                sandbox_policy: SandboxPolicy::ReadOnly,
                cwd: PathBuf::from("/home/user/project"),
                reasoning_effort: None,
                history_log_id: 0,
                history_entry_count: 0,
                initial_messages: None,
                skill_load_outcome: None,
                rollout_path: PathBuf::new(),
            };
            Arc::new(new_session_info(
                app.chat_widget.config_ref(),
                app.current_model.as_str(),
                event,
                is_first,
            )) as Arc<dyn HistoryCell>
        };

        // Simulate the transcript after trimming for a fork, replaying history, and
        // appending the edited turn. The session header separates the retained history
        // from the forked conversation's replayed turns.
        app.transcript_cells = vec![
            make_header(true),
            user_cell("first question"),
            agent_cell("answer first"),
            user_cell("follow-up"),
            agent_cell("answer follow-up"),
            make_header(false),
            user_cell("first question"),
            agent_cell("answer first"),
            user_cell("follow-up (edited)"),
            agent_cell("answer edited"),
        ];

        assert_eq!(backtrack_count(&app.transcript_cells), 2);

        app.backtrack.base_id = Some(ConversationId::new());
        app.backtrack.primed = true;
        app.backtrack.nth_backtrack_position =
            backtrack_count(&app.transcript_cells).saturating_sub(1);

        app.confirm_backtrack_from_main();

        let (_, nth, prefill) = app.backtrack.pending.clone().expect("pending backtrack");
        assert_eq!(nth, 1);
        assert_eq!(prefill, "follow-up (edited)");
    }

    #[tokio::test]
    async fn new_session_requests_shutdown_for_previous_conversation() {
        let (mut app, mut app_event_rx, mut op_rx) = make_test_app_with_channels();

        let conversation_id = ConversationId::new();
        let event = SessionConfiguredEvent {
            session_id: conversation_id,
            model: "gpt-test".to_string(),
            model_provider_id: "test-provider".to_string(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::ReadOnly,
            cwd: PathBuf::from("/home/user/project"),
            reasoning_effort: None,
            history_log_id: 0,
            history_entry_count: 0,
            initial_messages: None,
            skill_load_outcome: None,
            rollout_path: PathBuf::new(),
        };

        app.chat_widget.handle_codex_event(Event {
            id: String::new(),
            msg: EventMsg::SessionConfigured(event),
        });

        while app_event_rx.try_recv().is_ok() {}
        while op_rx.try_recv().is_ok() {}

        app.shutdown_current_conversation(ShutdownCurrentConversationMode::ReplaceConversation)
            .await;

        match op_rx.try_recv() {
            Ok(Op::Shutdown) => {}
            Ok(other) => panic!("expected Op::Shutdown, got {other:?}"),
            Err(_) => panic!("expected shutdown op to be sent"),
        }
    }

    #[test]
    fn session_summary_skip_zero_usage() {
        assert!(session_summary(TokenUsage::default(), None, None).is_none());
    }

    #[test]
    fn session_summary_includes_resume_hint() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            total_tokens: 12,
            ..Default::default()
        };
        let conversation =
            ConversationId::from_string("123e4567-e89b-12d3-a456-426614174000").unwrap();

        let summary = session_summary(usage, Some(conversation), None).expect("summary");
        assert_eq!(
            summary.usage_line,
            "Token usage: total=12 input=10 output=2"
        );
        assert_eq!(
            summary.resume_command,
            Some("codex resume 123e4567-e89b-12d3-a456-426614174000".to_string())
        );
    }
}
