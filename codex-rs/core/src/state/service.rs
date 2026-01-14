use std::collections::HashMap;
use std::sync::Arc;

use crate::AuthManager;
use crate::delegation::DelegationRegistry;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::openai_models::models_manager::ModelsManager;
use crate::project_doc_manager::ProjectDocManager;
use crate::rollout::RolloutRecorder;
use crate::session_log::SessionLog;
use crate::skills::SkillLoadOutcome;
use crate::skills::SkillsManager;
use crate::subagents::SubagentSession;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecSessionManager;
use crate::user_notification::UserNotifier;
use codex_otel::otel_event_manager::OtelEventManager;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

pub(crate) struct SessionServices {
    pub(crate) mcp_connection_manager: Arc<RwLock<McpConnectionManager>>,
    pub(crate) mcp_startup_cancellation_token: CancellationToken,
    pub(crate) unified_exec_manager: UnifiedExecSessionManager,
    pub(crate) notifier: UserNotifier,
    pub(crate) rollout: Mutex<Option<RolloutRecorder>>,
    pub(crate) session_log: Mutex<Option<SessionLog>>,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: Arc<ModelsManager>,
    pub(crate) otel_event_manager: OtelEventManager,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    pub(crate) subagent_sessions: Mutex<HashMap<String, SubagentSession>>,
    pub(crate) delegation_registry: DelegationRegistry,
    pub(crate) skills: Option<SkillLoadOutcome>,
    pub(crate) skills_manager: Arc<SkillsManager>,
    pub(crate) project_doc_manager: Arc<ProjectDocManager>,
}
