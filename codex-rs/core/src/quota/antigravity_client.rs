//! Antigravity quota client implementation.
//!
//! Fetches quota information from the local Antigravity language server by:
//! 1. Finding the running language server process
//! 2. Extracting the CSRF token from its command line
//! 3. Discovering listening ports
//! 4. Making an HTTPS request to the GetUserStatus endpoint

use super::ProviderQuotaClient;
use super::types::AntigravityUserStatus;
use super::types::QuotaError;
use async_trait::async_trait;
use codex_protocol::protocol::RateLimitSnapshot;
use regex::Regex;
use reqwest::Client;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

/// Client for fetching quota information from the Antigravity language server.
pub struct AntigravityQuotaClient {
    http_client: Client,
    timeout: Duration,
}

impl Default for AntigravityQuotaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityQuotaClient {
    /// Create a new Antigravity quota client.
    pub fn new() -> Self {
        // Build an HTTP client that skips TLS verification (self-signed cert)
        let http_client = Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http_client,
            timeout: Duration::from_secs(5),
        }
    }

    /// Find the Antigravity language server process and extract connection info.
    async fn find_process_info(&self) -> Result<ProcessInfo, QuotaError> {
        let (process_name, process_cmd) = get_platform_process_info();

        // Run the process discovery command
        let output = Command::new("sh")
            .arg("-c")
            .arg(&process_cmd)
            .output()
            .await
            .map_err(|e| QuotaError::CommandFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.trim().is_empty() {
            debug!("Antigravity process not found: {process_name}. Tried command: {process_cmd}");
            return Err(QuotaError::ProcessNotFound);
        }

        // Parse the output to extract PID and command line
        let (pid, cmd_line) = parse_process_output(&stdout, &process_name)?;

        // Extract CSRF token from command line
        let csrf_token = extract_csrf_token(&cmd_line).ok_or(QuotaError::CsrfTokenNotFound)?;

        // Get listening ports for this process
        let ports = self.get_listening_ports(pid).await?;

        if ports.is_empty() {
            return Err(QuotaError::NoPortsFound);
        }

        Ok(ProcessInfo {
            pid,
            csrf_token,
            ports,
        })
    }

    /// Get listening TCP ports for a process.
    async fn get_listening_ports(&self, pid: i32) -> Result<Vec<i32>, QuotaError> {
        let cmd = get_port_list_command(pid);

        let output = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
            .map_err(|e| QuotaError::CommandFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ports = parse_listening_ports(&stdout);

        debug!(
            "Found {} listening ports for PID {pid}: {:?}",
            ports.len(),
            ports
        );

        Ok(ports)
    }

    /// Try to connect to a port and make the GetUserStatus request.
    async fn try_port(
        &self,
        port: i32,
        csrf_token: &str,
    ) -> Result<AntigravityUserStatus, QuotaError> {
        let url = format!(
            "https://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/GetUserStatus"
        );

        let body = serde_json::json!({
            "metadata": {
                "ideName": "codex",
                "extensionName": "codex",
                "locale": "en"
            }
        });

        let response = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .header("X-Codeium-Csrf-Token", csrf_token)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| QuotaError::RequestFailed(e.to_string()))?;

        if !response.status().is_success() {
            return Err(QuotaError::RequestFailed(format!(
                "HTTP {}",
                response.status()
            )));
        }

        let user_status: AntigravityUserStatus = response
            .json()
            .await
            .map_err(|e| QuotaError::InvalidResponse(e.to_string()))?;

        Ok(user_status)
    }

    /// Fetch quota by trying each discovered port until one works.
    async fn fetch_quota_inner(&self) -> Result<RateLimitSnapshot, QuotaError> {
        let process_info = self.find_process_info().await?;

        debug!(
            "Found Antigravity process PID={}, {} ports to try",
            process_info.pid,
            process_info.ports.len()
        );

        for port in &process_info.ports {
            match self.try_port(*port, &process_info.csrf_token).await {
                Ok(user_status) => {
                    debug!("Successfully connected to Antigravity on port {port}");
                    return Ok(user_status.to_rate_limit_snapshot());
                }
                Err(e) => {
                    debug!("Port {port} failed: {e}");
                }
            }
        }

        Err(QuotaError::ConnectionFailed)
    }
}

#[async_trait]
impl ProviderQuotaClient for AntigravityQuotaClient {
    async fn fetch_quota(&self) -> Option<RateLimitSnapshot> {
        match self.fetch_quota_inner().await {
            Ok(snapshot) => Some(snapshot),
            Err(e) => {
                debug!("Failed to fetch Antigravity quota: {e}");
                None
            }
        }
    }

    fn provider_name(&self) -> &'static str {
        "Antigravity"
    }
}

// ============================================================================
// Process Discovery Helpers
// ============================================================================

struct ProcessInfo {
    pid: i32,
    csrf_token: String,
    ports: Vec<i32>,
}

/// Get the process name and discovery command for the current platform.
fn get_platform_process_info() -> (String, String) {
    let arch = std::env::consts::ARCH;

    #[cfg(target_os = "windows")]
    {
        let process_name = "language_server_windows_x64.exe".to_string();
        let cmd = format!(
            "powershell -NoProfile -Command \"Get-CimInstance Win32_Process -Filter \\\"name='{process_name}'\\\" | Select-Object ProcessId,CommandLine | ConvertTo-Json\""
        );
        (process_name, cmd)
    }

    #[cfg(target_os = "macos")]
    {
        let suffix = if arch == "aarch64" { "_arm" } else { "" };
        let process_name = format!("language_server_macos{suffix}");
        let cmd = format!("pgrep -fl {process_name}");
        (process_name, cmd)
    }

    #[cfg(target_os = "linux")]
    {
        let suffix = if arch == "aarch64" { "_arm" } else { "_x64" };
        let process_name = format!("language_server_linux{suffix}");
        let cmd = format!("pgrep -af {process_name}");
        (process_name, cmd)
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        // Fallback for other platforms
        let process_name = "language_server".to_string();
        let cmd = format!("pgrep -af {process_name}");
        (process_name, cmd)
    }
}

/// Get the command to list listening ports for a process.
fn get_port_list_command(pid: i32) -> String {
    #[cfg(target_os = "windows")]
    {
        format!(
            "powershell -NoProfile -Command \"Get-NetTCPConnection -OwningProcess {pid} -State Listen | Select-Object -ExpandProperty LocalPort | ConvertTo-Json\""
        )
    }

    #[cfg(target_os = "macos")]
    {
        format!("lsof -iTCP -sTCP:LISTEN -n -P -p {pid}")
    }

    #[cfg(target_os = "linux")]
    {
        format!(
            "ss -tlnp 2>/dev/null | grep \"pid={pid}\" || lsof -iTCP -sTCP:LISTEN -n -P -p {pid} 2>/dev/null"
        )
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        format!("lsof -iTCP -sTCP:LISTEN -n -P -p {pid}")
    }
}

/// Parse process discovery output to extract PID and command line.
fn parse_process_output(stdout: &str, process_name: &str) -> Result<(i32, String), QuotaError> {
    // Try parsing as JSON first (Windows PowerShell output)
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        return parse_windows_json(&json);
    }

    // Parse Unix pgrep output: "PID command line args..."
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // The process must have --csrf_token for authentication.
        // We identify the process by name (already filtered by pgrep) and require csrf_token.
        // Note: --extension_server_port is optional (server may use default port).
        if !line.contains("--csrf_token") {
            continue;
        }

        // Additionally verify this is actually the language server process
        if !line.contains(process_name) {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() >= 2 {
            if let Ok(pid) = parts[0].parse::<i32>() {
                return Ok((pid, parts[1].to_string()));
            }
        }
    }

    Err(QuotaError::ProcessNotFound)
}

/// Parse Windows PowerShell JSON output.
fn parse_windows_json(json: &serde_json::Value) -> Result<(i32, String), QuotaError> {
    // Handle both array and single object responses
    let items = match json.as_array() {
        Some(arr) => arr.clone(),
        None => vec![json.clone()],
    };

    for item in items {
        let cmd_line = item
            .get("CommandLine")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Check if this is an Antigravity process
        if !cmd_line.contains("--app_data_dir") || !cmd_line.to_lowercase().contains("antigravity")
        {
            continue;
        }

        if let Some(pid) = item.get("ProcessId").and_then(serde_json::Value::as_i64) {
            return Ok((pid as i32, cmd_line.to_string()));
        }
    }

    Err(QuotaError::ProcessNotFound)
}

/// Extract CSRF token from command line.
fn extract_csrf_token(cmd_line: &str) -> Option<String> {
    let re = Regex::new(r"--csrf_token[=\s]+([a-zA-Z0-9\-]+)").ok()?;
    re.captures(cmd_line)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Parse listening ports from lsof/ss/netstat output.
fn parse_listening_ports(stdout: &str) -> Vec<i32> {
    let mut ports = Vec::new();

    // Try parsing as JSON first (Windows PowerShell output)
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        if let Some(arr) = json.as_array() {
            for item in arr {
                if let Some(port) = item.as_i64() {
                    let port = port as i32;
                    if !ports.contains(&port) {
                        ports.push(port);
                    }
                }
            }
            return ports;
        }
        if let Some(port) = json.as_i64() {
            return vec![port as i32];
        }
    }

    // Parse ss output: LISTEN ... *:PORT or 127.0.0.1:PORT
    let ss_regex = Regex::new(r"LISTEN\s+\d+\s+\d+\s+(?:\*|[\d.]+|\[[\da-f:]*\]):(\d+)").ok();

    // Parse lsof output: TCP *:PORT (LISTEN) or TCP 127.0.0.1:PORT (LISTEN)
    let lsof_regex =
        Regex::new(r"(?:TCP|UDP)\s+(?:\*|[\d.]+|\[[\da-f:]+\]):(\d+)\s+\(LISTEN\)").ok();

    for line in stdout.lines() {
        // Try ss regex
        if let Some(re) = &ss_regex {
            if let Some(caps) = re.captures(line) {
                if let Some(port_match) = caps.get(1) {
                    if let Ok(port) = port_match.as_str().parse::<i32>() {
                        if !ports.contains(&port) {
                            ports.push(port);
                        }
                    }
                }
            }
        }

        // Try lsof regex
        if let Some(re) = &lsof_regex {
            if let Some(caps) = re.captures(line) {
                if let Some(port_match) = caps.get(1) {
                    if let Ok(port) = port_match.as_str().parse::<i32>() {
                        if !ports.contains(&port) {
                            ports.push(port);
                        }
                    }
                }
            }
        }
    }

    ports.sort();
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_csrf_token() {
        let cmd = "/path/to/binary --csrf_token abc123-def456 --other_arg";
        assert_eq!(extract_csrf_token(cmd), Some("abc123-def456".to_string()));

        let cmd_eq = "/path/to/binary --csrf_token=xyz789 --other_arg";
        assert_eq!(extract_csrf_token(cmd_eq), Some("xyz789".to_string()));

        let cmd_none = "/path/to/binary --other_arg";
        assert_eq!(extract_csrf_token(cmd_none), None);
    }

    #[test]
    fn test_parse_process_output_unix() {
        // Test with --extension_server_port present
        let stdout = "12345 /path/to/language_server_linux_x64 --extension_server_port 8080 --csrf_token abc123";
        let result = parse_process_output(stdout, "language_server_linux_x64").unwrap();
        assert_eq!(result.0, 12345);
        assert!(result.1.contains("--csrf_token"));

        // Test without --extension_server_port (uses default port)
        let stdout_no_port =
            "12345 /path/to/language_server_linux_x64 --csrf_token abc123 --other_flag";
        let result = parse_process_output(stdout_no_port, "language_server_linux_x64").unwrap();
        assert_eq!(result.0, 12345);
        assert!(result.1.contains("--csrf_token"));

        // Test that process without csrf_token is rejected
        let stdout_no_token =
            "12345 /path/to/language_server_linux_x64 --extension_server_port 8080";
        let result = parse_process_output(stdout_no_token, "language_server_linux_x64");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_listening_ports_lsof() {
        let stdout = r#"
COMMAND   PID USER   FD   TYPE DEVICE SIZE/OFF NODE NAME
lang_srv 1234 user   12u  IPv4 123456      0t0  TCP *:42100 (LISTEN)
lang_srv 1234 user   13u  IPv4 123457      0t0  TCP 127.0.0.1:42101 (LISTEN)
"#;
        let ports = parse_listening_ports(stdout);
        assert_eq!(ports, vec![42100, 42101]);
    }

    #[test]
    fn test_parse_listening_ports_ss() {
        let stdout = r#"
LISTEN    0      128          *:42100              *:*    users:(("lang_srv",pid=1234,fd=12))
LISTEN    0      128    127.0.0.1:42101              *:*    users:(("lang_srv",pid=1234,fd=13))
"#;
        let ports = parse_listening_ports(stdout);
        assert_eq!(ports, vec![42100, 42101]);
    }
}
