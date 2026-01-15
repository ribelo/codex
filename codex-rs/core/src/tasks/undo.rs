use std::sync::Arc;

use crate::codex::TurnContext;
use crate::protocol::EventMsg;
use crate::protocol::UndoCompletedEvent;
use crate::protocol::UndoStartedEvent;
use crate::session_log::build_model_context;
use crate::session_log::get_parent_commit_id;
use crate::session_log::read_session;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use async_trait::async_trait;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;

pub(crate) struct UndoTask;

impl UndoTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SessionTask for UndoTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let sess = session.clone_session();
        sess.send_event(
            ctx.as_ref(),
            EventMsg::UndoStarted(UndoStartedEvent {
                message: Some("Undo in progress...".to_string()),
            }),
        )
        .await;

        if cancellation_token.is_cancelled() {
            sess.send_event(
                ctx.as_ref(),
                EventMsg::UndoCompleted(UndoCompletedEvent {
                    success: false,
                    message: Some("Undo cancelled.".to_string()),
                }),
            )
            .await;
            return None;
        }

        let mut history = sess.clone_history().await;
        let mut items = history.get_history();
        let mut completed = UndoCompletedEvent {
            success: false,
            message: None,
        };

        // First, try to find the parent commit in session log for session undo
        let sess_log_path = sess.get_session_log_path().await;
        let parent_commit_result = match sess_log_path.clone() {
            Some(path) => tokio::task::spawn_blocking(move || get_parent_commit_id(&path))
                .await
                .ok()
                .and_then(|res| res.ok().flatten()),
            None => None,
        };

        // Try to undo session state using HeadSet
        if let Some(parent_commit_id) = parent_commit_result {
            // Set the session log head to the parent commit
            match sess
                .set_session_log_head(parent_commit_id, "undo".to_string())
                .await
            {
                Ok(()) => {
                    let short_id: String = parent_commit_id.to_string().chars().take(7).collect();
                    info!(parent_commit_id = %parent_commit_id, "Undo set head to parent commit");
                    completed.success = true;
                    completed.message = Some(format!("Undo reverted to commit {short_id}."));

                    if let Some(path) = sess_log_path.clone() {
                        let read_result =
                            tokio::task::spawn_blocking(move || read_session(&path)).await;
                        match read_result {
                            Ok(Ok(data)) => {
                                items = build_model_context(&data);
                                if let Some((idx, ghost_commit)) =
                                    items.iter().enumerate().rev().find_map(
                                        |(idx, item)| match item {
                                            ResponseItem::GhostSnapshot { ghost_commit } => {
                                                Some((idx, ghost_commit.clone()))
                                            }
                                            _ => None,
                                        },
                                    )
                                {
                                    let commit_id = ghost_commit.id().to_string();
                                    let repo_path = ctx.cwd.clone();
                                    let ghost_snapshot_config =
                                        sess.config().await.ghost_snapshot.clone();
                                    let restore_result = tokio::task::spawn_blocking(move || {
                                        let options =
                                            codex_git::RestoreGhostCommitOptions::new(&repo_path)
                                                .ghost_snapshot(ghost_snapshot_config);
                                        codex_git::restore_ghost_commit_with_options(
                                            &options,
                                            &ghost_commit,
                                        )
                                    })
                                    .await;

                                    match restore_result {
                                        Ok(Ok(())) => {
                                            items.remove(idx);
                                            let git_short_id: String =
                                                commit_id.chars().take(7).collect();
                                            info!(
                                                commit_id = commit_id,
                                                "Undo also restored ghost snapshot"
                                            );
                                            completed.message = Some(format!(
                                                "Undo reverted to commit {short_id} and restored snapshot {git_short_id}."
                                            ));
                                        }
                                        Ok(Err(err)) => {
                                            warn!("Git restore failed after session undo: {err}");
                                        }
                                        Err(err) => {
                                            warn!(
                                                "Git restore task failed after session undo: {err}"
                                            );
                                        }
                                    }
                                }
                                sess.replace_history(items).await;
                            }
                            Ok(Err(err)) => {
                                warn!("Failed to read session log after undo: {err}");
                            }
                            Err(err) => {
                                warn!("Failed to spawn session read task after undo: {err}");
                            }
                        }
                    }

                    sess.send_event(ctx.as_ref(), EventMsg::UndoCompleted(completed))
                        .await;
                    return None;
                }
                Err(err) => {
                    warn!("Failed to set session log head: {err}");
                }
            }
        }

        // Fall back to git-only undo if no parent commit or HeadSet failed
        let Some((idx, ghost_commit)) =
            items
                .iter()
                .enumerate()
                .rev()
                .find_map(|(idx, item)| match item {
                    ResponseItem::GhostSnapshot { ghost_commit } => {
                        Some((idx, ghost_commit.clone()))
                    }
                    _ => None,
                })
        else {
            completed.message = Some("No ghost snapshot available to undo.".to_string());
            sess.send_event(ctx.as_ref(), EventMsg::UndoCompleted(completed))
                .await;
            return None;
        };

        let commit_id = ghost_commit.id().to_string();
        let repo_path = ctx.cwd.clone();
        let ghost_snapshot_config = sess.config().await.ghost_snapshot.clone();
        let restore_result = tokio::task::spawn_blocking(move || {
            let options = codex_git::RestoreGhostCommitOptions::new(&repo_path)
                .ghost_snapshot(ghost_snapshot_config);
            codex_git::restore_ghost_commit_with_options(&options, &ghost_commit)
        })
        .await;

        match restore_result {
            Ok(Ok(())) => {
                items.remove(idx);
                sess.replace_history(items).await;
                let short_id: String = commit_id.chars().take(7).collect();
                info!(commit_id = commit_id, "Undo restored ghost snapshot");
                completed.success = true;
                completed.message = Some(format!("Undo restored snapshot {short_id}."));
            }
            Ok(Err(err)) => {
                let message = format!("Failed to restore snapshot {commit_id}: {err}");
                warn!("{message}");
                completed.message = Some(message);
            }
            Err(err) => {
                let message = format!("Failed to restore snapshot {commit_id}: {err}");
                error!("{message}");
                completed.message = Some(message);
            }
        }

        sess.send_event(ctx.as_ref(), EventMsg::UndoCompleted(completed))
            .await;
        None
    }
}
