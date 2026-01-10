use codex_core::MCP_SANDBOX_STATE_NOTIFICATION;
use codex_core::SandboxState;
use codex_core::protocol::SandboxPolicy;
use rmcp::ClientHandler;
use rmcp::ErrorData as McpError;
use rmcp::RoleClient;
use rmcp::Service;
use rmcp::model::ClientCapabilities;
use rmcp::model::ClientInfo;
use rmcp::model::CreateElicitationRequestParam;
use rmcp::model::CreateElicitationResult;
use rmcp::model::CustomClientNotification;
use rmcp::model::ElicitationAction;
use rmcp::service::RunningService;
use rmcp::transport::ConfigureCommandExt;
use rmcp::transport::TokioChildProcess;
use serde_json::json;
use std::collections::HashSet;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::process::Command;

pub async fn create_transport<P>(
    codex_home: P,
    dotslash_cache: P,
) -> anyhow::Result<TokioChildProcess>
where
    P: AsRef<Path>,
{
    let mcp_executable = assert_cmd::Command::cargo_bin("codex-exec-mcp-server")?;
    let execve_wrapper = assert_cmd::Command::cargo_bin("codex-execve-wrapper")?;
    let dotslash_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("suite")
        .join("bash");

    // The Bash binary referenced by the DotSlash manifest must be materialized
    // before the sandbox is enabled (since a read-only sandbox cannot write to
    // the DotSlash cache).
    let mut dotslash_cmd = Command::new("dotslash");
    dotslash_cmd.arg("--").arg("fetch").arg(&dotslash_manifest);
    dotslash_cmd.env("DOTSLASH_CACHE", dotslash_cache.as_ref());
    let mut paths: Vec<PathBuf> =
        env::split_paths(&env::var_os("PATH").unwrap_or_default()).collect();
    if let Some(home) = env::var_os("HOME") {
        let cargo_bin = PathBuf::from(home).join(".cargo").join("bin");
        if !paths.iter().any(|p| p == &cargo_bin) {
            paths.insert(0, cargo_bin);
        }
    }
    dotslash_cmd.env("PATH", env::join_paths(paths)?);

    let dotslash_output = dotslash_cmd.output().await?;
    if !dotslash_output.status.success() {
        return Err(anyhow::anyhow!(
            "dotslash fetch failed: {}",
            String::from_utf8_lossy(&dotslash_output.stderr)
        ));
    }
    let bash = String::from_utf8(dotslash_output.stdout)?;
    let bash = PathBuf::from(bash.trim());

    let transport =
        TokioChildProcess::new(Command::new(mcp_executable.get_program()).configure(|cmd| {
            cmd.arg("--bash").arg(bash);
            cmd.arg("--execve").arg(execve_wrapper.get_program());
            cmd.env("CODEX_HOME", codex_home.as_ref());
            cmd.env("DOTSLASH_CACHE", dotslash_cache.as_ref());

            // Important: pipe stdio so rmcp can speak JSON-RPC over stdin/stdout
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());

            // Optional but very helpful while debugging:
            cmd.stderr(Stdio::inherit());
        }))?;

    Ok(transport)
}

pub async fn write_default_execpolicy<P>(policy: &str, codex_home: P) -> anyhow::Result<()>
where
    P: AsRef<Path>,
{
    let policy_dir = codex_home.as_ref().join("policy");
    tokio::fs::create_dir_all(&policy_dir).await?;
    tokio::fs::write(policy_dir.join("default.codexpolicy"), policy).await?;
    Ok(())
}

pub async fn notify_readable_sandbox<P, S>(
    sandbox_cwd: P,
    codex_linux_sandbox_exe: Option<PathBuf>,
    service: &RunningService<RoleClient, S>,
) -> anyhow::Result<()>
where
    P: AsRef<Path>,
    S: Service<RoleClient> + ClientHandler,
{
    let sandbox_state = SandboxState {
        sandbox_policy: SandboxPolicy::ReadOnly,
        codex_linux_sandbox_exe,
        sandbox_cwd: sandbox_cwd.as_ref().to_path_buf(),
    };
    send_sandbox_notification(sandbox_state, service).await
}

pub async fn notify_writable_sandbox_only_one_folder<P, S>(
    writable_folder: P,
    codex_linux_sandbox_exe: Option<PathBuf>,
    service: &RunningService<RoleClient, S>,
) -> anyhow::Result<()>
where
    P: AsRef<Path>,
    S: Service<RoleClient> + ClientHandler,
{
    let sandbox_state = SandboxState {
        sandbox_policy: SandboxPolicy::WorkspaceWrite {
            // Note that sandbox_cwd will already be included as a writable root
            // when the sandbox policy is expanded.
            writable_roots: vec![],
            network_access: false,
            // Disable writes to temp dir because this is a test, so
            // writable_folder is likely also under /tmp and we want to be
            // strict about what is writable.
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        },
        codex_linux_sandbox_exe,
        sandbox_cwd: writable_folder.as_ref().to_path_buf(),
    };
    send_sandbox_notification(sandbox_state, service).await
}

async fn send_sandbox_notification<S>(
    sandbox_state: SandboxState,
    service: &RunningService<RoleClient, S>,
) -> anyhow::Result<()>
where
    S: Service<RoleClient> + ClientHandler,
{
    let sandbox_state_notification = CustomClientNotification::new(
        MCP_SANDBOX_STATE_NOTIFICATION,
        Some(serde_json::to_value(sandbox_state)?),
    );
    service
        .send_notification(sandbox_state_notification.into())
        .await?;
    Ok(())
}

pub struct InteractiveClient {
    pub elicitations_to_accept: HashSet<String>,
    pub elicitation_requests: Arc<Mutex<Vec<CreateElicitationRequestParam>>>,
}

impl ClientHandler for InteractiveClient {
    fn get_info(&self) -> ClientInfo {
        let capabilities = ClientCapabilities::builder().enable_elicitation().build();
        ClientInfo {
            capabilities,
            ..Default::default()
        }
    }

    fn create_elicitation(
        &self,
        request: CreateElicitationRequestParam,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, McpError>> + Send + '_
    {
        self.elicitation_requests
            .lock()
            .unwrap()
            .push(request.clone());

        let accept = self.elicitations_to_accept.contains(&request.message);
        async move {
            if accept {
                Ok(CreateElicitationResult {
                    action: ElicitationAction::Accept,
                    content: Some(json!({ "approve": true })),
                })
            } else {
                Ok(CreateElicitationResult {
                    action: ElicitationAction::Decline,
                    content: None,
                })
            }
        }
    }
}
