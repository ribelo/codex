use crate::pkce::PkceCodes;
use crate::pkce::generate_pkce;
use crate::server::ShutdownHandle;
use base64::Engine;
use chrono::Utc;
use codex_core::antigravity::ANTIGRAVITY_AUTH_URL;
use codex_core::antigravity::ANTIGRAVITY_CLIENT_ID;
use codex_core::antigravity::ANTIGRAVITY_CLIENT_SECRET;
use codex_core::antigravity::ANTIGRAVITY_SCOPES;
use codex_core::antigravity::ANTIGRAVITY_TOKEN_URL;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::AuthDotJson;
use codex_core::auth::save_auth;
use codex_core::gemini::GEMINI_CLIENT_ID;
use codex_core::gemini::GEMINI_CLIENT_SECRET;
use codex_core::gemini::GEMINI_TOKEN_URL;
use codex_core::token_data::GeminiTokenData;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::io;
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tiny_http::Header;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;
use url::Url;

const DEFAULT_PORT: u16 = 1456;

#[derive(Debug, Clone, Copy)]
pub enum GoogleProviderKind {
    Gemini,
    Antigravity,
}

#[derive(Debug, Clone)]
pub struct GoogleServerOptions {
    pub codex_home: PathBuf,
    pub open_browser: bool,
    pub project_id: Option<String>,
    pub cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    pub provider_kind: GoogleProviderKind,
}

#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GeminiUserInfo {
    #[serde(default)]
    email: Option<String>,
}

pub struct GoogleLoginServer {
    pub server_handle: tokio::task::JoinHandle<std::io::Result<()>>,
    pub shutdown_handle: ShutdownHandle,
    pub auth_url: String,
}

impl GoogleLoginServer {
    pub async fn block_until_done(self) -> std::io::Result<()> {
        self.server_handle.await?
    }
}

fn provider_credentials(
    provider_kind: GoogleProviderKind,
) -> (&'static str, &'static str, &'static str) {
    match provider_kind {
        GoogleProviderKind::Gemini => (GEMINI_CLIENT_ID, GEMINI_CLIENT_SECRET, GEMINI_TOKEN_URL),
        GoogleProviderKind::Antigravity => (
            ANTIGRAVITY_CLIENT_ID,
            ANTIGRAVITY_CLIENT_SECRET,
            ANTIGRAVITY_TOKEN_URL,
        ),
    }
}

fn provider_scopes(provider_kind: GoogleProviderKind) -> String {
    match provider_kind {
        GoogleProviderKind::Gemini => {
            "openid profile email https://www.googleapis.com/auth/generative-language".to_string()
        }
        GoogleProviderKind::Antigravity => {
            let mut scopes = vec!["openid"];
            scopes.extend_from_slice(ANTIGRAVITY_SCOPES);
            scopes.join(" ")
        }
    }
}

fn provider_accounts_mut(
    auth: &mut AuthDotJson,
    provider_kind: GoogleProviderKind,
) -> &mut Vec<GeminiTokenData> {
    match provider_kind {
        GoogleProviderKind::Gemini => &mut auth.gemini_accounts,
        GoogleProviderKind::Antigravity => &mut auth.antigravity_accounts,
    }
}

pub async fn run_google_login_server(
    opts: GoogleServerOptions,
) -> std::io::Result<GoogleLoginServer> {
    let pkce = generate_pkce();
    let project_id = opts
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|project| !project.is_empty())
        .map(str::to_string);

    let state = generate_state_with_project_id(&pkce.code_verifier, project_id.as_deref());
    let provider_kind = opts.provider_kind;

    let server = bind_server(DEFAULT_PORT)?;
    let actual_port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "Unable to determine the server port",
            ));
        }
    };
    let server = std::sync::Arc::new(server);

    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let auth_url = build_authorize_url(
        &redirect_uri,
        &state,
        &pkce,
        project_id.as_deref(),
        provider_kind,
    );
    let shutdown_handle = ShutdownHandle::new();
    let shutdown_clone = shutdown_handle.clone();

    let auth_url_for_browser = auth_url.clone();
    let server_handle = tokio::spawn(async move {
        run_server(
            server,
            redirect_uri,
            auth_url,
            pkce,
            opts.codex_home,
            project_id,
            provider_kind,
            shutdown_clone,
            opts.cli_auth_credentials_store_mode,
        )
        .await
    });

    if opts.open_browser {
        let browser_url = auth_url_for_browser.clone();
        let result = tokio::task::spawn_blocking(move || webbrowser::open(&browser_url)).await?;
        if let Err(err) = result {
            eprintln!("Unable to open browser: {err}. Please open the URL manually instead:");
            eprintln!("{auth_url_for_browser}");
        }
    } else {
        eprintln!("Open this URL in your browser to log in:\n\n{auth_url_for_browser}\n");
    }

    Ok(GoogleLoginServer {
        server_handle,
        shutdown_handle,
        auth_url: auth_url_for_browser,
    })
}

fn bind_server(port: u16) -> std::io::Result<tiny_http::Server> {
    let addr = format!("127.0.0.1:{port}");
    Server::http(addr).map_err(|err| {
        err.downcast::<std::io::Error>()
            .map(|io_err| *io_err)
            .unwrap_or_else(|other| std::io::Error::other(other.to_string()))
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_server(
    server: std::sync::Arc<Server>,
    redirect_uri: String,
    auth_url: String,
    pkce: PkceCodes,
    codex_home: PathBuf,
    project_id: Option<String>,
    provider_kind: GoogleProviderKind,
    shutdown_handle: ShutdownHandle,
    cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> std::io::Result<()> {
    let shutdown_guard = shutdown_handle.clone();
    let shutdown_listener = shutdown_handle.clone();
    let server_clone = server.clone();
    let server_thread = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        loop {
            if shutdown_guard.is_shutdown() {
                break;
            }
            if let Ok(Some(request)) = server_clone.recv_timeout(Duration::from_millis(250)) {
                match handle_request(
                    &request,
                    &redirect_uri,
                    &auth_url,
                    &pkce,
                    &codex_home,
                    project_id.as_deref(),
                    provider_kind,
                    cli_auth_credentials_store_mode,
                ) {
                    HandledRequest::Unhandled => {
                        let response = Response::from_string("Not Found").with_status_code(404);
                        let _ = request.respond(response);
                    }
                    HandledRequest::Response(response) => {
                        let _ = request.respond(response);
                    }
                    HandledRequest::ResponseAndExit {
                        headers,
                        body,
                        result,
                    } => {
                        let mut response =
                            Response::from_data(body).with_status_code(StatusCode(200));
                        for header in headers {
                            response.add_header(header);
                        }
                        let _ = request.respond(response);
                        shutdown_guard.shutdown();
                        return result;
                    }
                }
            }
        }
        Ok(())
    });

    shutdown_listener.notifier().notified().await;
    tokio::task::spawn_blocking(move || server.unblock()).await?;
    match server_thread.await {
        Ok(result) => result,
        Err(err) => Err(std::io::Error::other(format!(
            "google login server thread panicked: {err}"
        ))),
    }
}

enum HandledRequest {
    Unhandled,
    Response(Response<Cursor<Vec<u8>>>),
    ResponseAndExit {
        headers: Vec<Header>,
        body: Vec<u8>,
        result: std::io::Result<()>,
    },
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    request: &Request,
    redirect_uri: &str,
    auth_url: &str,
    pkce: &PkceCodes,
    codex_home: &Path,
    project_id: Option<&str>,
    provider_kind: GoogleProviderKind,
    cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
) -> HandledRequest {
    let path = request.url().to_string();

    match path.as_str() {
        "/auth" => HandledRequest::Response(Response::from_string("OK")),
        _ if path.starts_with("/auth/callback") => {
            match exchange_code_for_tokens(redirect_uri, auth_url, &path, pkce, provider_kind) {
                Ok(tokens) => {
                    let user_info = match fetch_user_info(&tokens.access_token) {
                        Ok(u) => u,
                        Err(e) => {
                            eprintln!("User info fetch error: {e}");
                            return HandledRequest::Response(
                                Response::from_string(format!("Failed to fetch user info: {e}"))
                                    .with_status_code(500),
                            );
                        }
                    };

                    let gemini_tokens = GeminiTokenData {
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                        id_token: tokens.id_token,
                        project_id: tokens
                            .project_id
                            .clone()
                            .or_else(|| project_id.map(str::to_string)),
                        managed_project_id: None,
                        email: user_info.email,
                        expires_at: tokens
                            .expires_in
                            .map(|secs| Utc::now() + chrono::Duration::seconds(secs)),
                    };

                    if let Err(err) = persist_google_tokens(
                        codex_home,
                        cli_auth_credentials_store_mode,
                        gemini_tokens,
                        provider_kind,
                    ) {
                        eprintln!("Persist error: {err}");
                        return HandledRequest::Response(
                            Response::from_string(format!("Unable to persist auth file: {err}"))
                                .with_status_code(500),
                        );
                    }

                    let page = success_page();
                    let mut headers = vec![];
                    if let Ok(h) =
                        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                    {
                        headers.push(h);
                    }
                    HandledRequest::ResponseAndExit {
                        headers,
                        body: page.into_bytes(),
                        result: Ok(()),
                    }
                }
                Err(err) => HandledRequest::Response(Response::from_string(err.to_string())),
            }
        }
        "/success" => HandledRequest::ResponseAndExit {
            headers: vec![],
            body: success_page().into_bytes(),
            result: Ok(()),
        },
        "/cancel" => HandledRequest::ResponseAndExit {
            headers: vec![],
            body: cancel_page().into_bytes(),
            result: Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Login cancelled",
            )),
        },
        _ => HandledRequest::Unhandled,
    }
}

fn exchange_code_for_tokens(
    redirect_uri: &str,
    auth_url: &str,
    request_path: &str,
    _pkce: &PkceCodes,
    provider_kind: GoogleProviderKind,
) -> Result<TokenExchangeResponse, Box<dyn std::error::Error>> {
    let request_url = Url::parse(&format!("http://localhost{request_path}"))?;
    let params: std::collections::HashMap<_, _> = request_url.query_pairs().into_owned().collect();
    let Some(code) = params.get("code") else {
        return Err("Code parameter missing".into());
    };
    let Some(state) = params.get("state") else {
        return Err("State parameter missing".into());
    };

    // Validate state matches what we sent (CSRF protection)
    let expected_state = Url::parse(auth_url)?.query_pairs().find_map(|(k, v)| {
        if k == "state" {
            Some(v.to_string())
        } else {
            None
        }
    });
    if expected_state.as_deref() != Some(state.as_str()) {
        return Err("State mismatch".into());
    }

    let state_payload = parse_state(state)?;
    let code_verifier = state_payload.verifier;
    let state_project_id = state_payload.project_id;

    let (client_id, client_secret, token_url) = provider_credentials(provider_kind);

    #[derive(Serialize)]
    struct TokenRequest {
        grant_type: &'static str,
        code: String,
        code_verifier: String,
        redirect_uri: String,
        client_id: String,
        client_secret: String,
    }

    let token_request = TokenRequest {
        grant_type: "authorization_code",
        code: code.clone(),
        code_verifier,
        redirect_uri: redirect_uri.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client.post(token_url).form(&token_request).send()?;

    let status = response.status();
    let body = response.text()?;

    if !status.is_success() {
        return Err(format!("Token exchange failed: {status} {body}").into());
    }

    let mut payload: TokenExchangeResponse = serde_json::from_str(&body)?;
    payload.project_id = payload.project_id.or(state_project_id);
    Ok(payload)
}

fn fetch_user_info(access_token: &str) -> Result<GeminiUserInfo, Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let resp = client
        .get("https://openidconnect.googleapis.com/v1/userinfo")
        .bearer_auth(access_token)
        .header(
            reqwest::header::USER_AGENT,
            format!("codex/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()?;

    if !resp.status().is_success() {
        return Err(format!("User info request failed: {}", resp.status()).into());
    }

    let data: JsonValue = resp.json()?;
    let email = data["email"].as_str().map(std::string::ToString::to_string);

    Ok(GeminiUserInfo { email })
}

fn persist_google_tokens(
    codex_home: &std::path::Path,
    creds_mode: AuthCredentialsStoreMode,
    gemini_tokens: GeminiTokenData,
    provider_kind: GoogleProviderKind,
) -> std::io::Result<AuthDotJson> {
    // Preserve existing auth fields; only update Google token fields
    let mut auth = match codex_core::auth::load_auth_dot_json(codex_home, creds_mode)? {
        Some(data) => data,
        None => AuthDotJson {
            openai_api_key: None,
            tokens: None,
            gemini_accounts: Vec::new(),
            antigravity_accounts: Vec::new(),
            last_refresh: None,
        },
    };

    let accounts = provider_accounts_mut(&mut auth, provider_kind);
    accounts.retain(|existing| match (&gemini_tokens.email, &existing.email) {
        (Some(new_email), Some(existing_email)) => existing_email != new_email,
        _ => true,
    });
    accounts.insert(0, gemini_tokens.clone());
    auth.last_refresh = Some(Utc::now());

    save_auth(codex_home, &auth, creds_mode)?;
    Ok(auth)
}

fn generate_state_with_project_id(verifier: &str, project_id: Option<&str>) -> String {
    #[derive(Serialize)]
    struct StatePayload {
        verifier: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
    }

    let payload = StatePayload {
        verifier: verifier.to_string(),
        project_id: project_id.filter(|s| !s.is_empty()).map(str::to_string),
    };

    let json = serde_json::to_string(&payload).expect("JSON serialization failed");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

struct StatePayload {
    verifier: String,
    project_id: Option<String>,
}

fn parse_state(state: &str) -> Result<StatePayload, Box<dyn std::error::Error>> {
    #[derive(Deserialize)]
    struct StatePayloadInner {
        verifier: String,
        #[serde(default)]
        project_id: Option<String>,
    }

    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(state)?;
    let json = std::string::String::from_utf8(decoded)?;
    let payload: StatePayloadInner = serde_json::from_str(&json)?;
    Ok(StatePayload {
        verifier: payload.verifier,
        project_id: payload.project_id,
    })
}

fn build_authorize_url(
    redirect_uri: &str,
    state: &str,
    pkce: &PkceCodes,
    project_id: Option<&str>,
    provider_kind: GoogleProviderKind,
) -> String {
    let auth_url = match provider_kind {
        GoogleProviderKind::Gemini => "https://accounts.google.com/o/oauth2/v2/auth",
        GoogleProviderKind::Antigravity => ANTIGRAVITY_AUTH_URL,
    };
    let (client_id, _, _) = provider_credentials(provider_kind);
    let scope = provider_scopes(provider_kind);
    let mut url = match Url::parse(auth_url) {
        Ok(url) => url,
        Err(_) => return auth_url.to_string(),
    };
    let mut query_pairs = url.query_pairs_mut();
    query_pairs.append_pair("response_type", "code");
    query_pairs.append_pair("client_id", client_id);
    query_pairs.append_pair("redirect_uri", redirect_uri);
    query_pairs.append_pair("scope", &scope);
    query_pairs.append_pair("state", state);
    query_pairs.append_pair("code_challenge", &pkce.code_challenge);
    query_pairs.append_pair("code_challenge_method", "S256");
    query_pairs.append_pair("access_type", "offline");
    if let Some(project) = project_id {
        query_pairs.append_pair("cloudaicompanion_project", project);
    }
    drop(query_pairs);
    url.into()
}

fn success_page() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Login Successful</title>
    <style>
        body { font-family: Arial, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; }
        .container { text-align: center; max-width: 600px; padding: 20px; border-radius: 12px; box-shadow: 0 10px 30px rgba(0,0,0,0.1); }
        h1 { color: #28a745; margin-bottom: 16px; }
        p { margin-bottom: 24px; color: #333; }
        button { padding: 12px 20px; font-size: 16px; background-color: #28a745; color: white; border: none; border-radius: 6px; cursor: pointer; transition: background-color 0.2s; }
        button:hover { background-color: #218838; }
    </style>
</head>
<body>
    <div class="container">
        <h1>Login Successful</h1>
        <p>You can now close this window and return to Codex.</p>
        <button onclick="window.close()">Close Window</button>
    </div>
</body>
</html>"#
        .to_string()
}

fn cancel_page() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Login Cancelled</title>
    <style>
        body { font-family: Arial, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; }
        .container { text-align: center; max-width: 600px; padding: 20px; border-radius: 12px; box-shadow: 0 10px 30px rgba(0,0,0,0.1); }
        h1 { color: #dc3545; margin-bottom: 16px; }
        p { margin-bottom: 24px; color: #333; }
        button { padding: 12px 20px; font-size: 16px; background-color: #dc3545; color: white; border: none; border-radius: 6px; cursor: pointer; transition: background-color 0.2s; }
        button:hover { background-color: #c82333; }
    </style>
</head>
<body>
    <div class="container">
        <h1>Login Cancelled</h1>
        <p>You can close this window and return to Codex.</p>
        <button onclick="window.close()">Close Window</button>
    </div>
</body>
</html>"#
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn sample_tokens(label: &str) -> GeminiTokenData {
        GeminiTokenData {
            access_token: format!("access-{label}"),
            refresh_token: format!("refresh-{label}"),
            id_token: None,
            project_id: None,
            managed_project_id: None,
            email: Some(format!("{label}@example.com")),
            expires_at: None,
        }
    }

    #[test]
    fn first_login_seeds_accounts() -> std::io::Result<()> {
        let dir = tempdir()?;
        let tokens = sample_tokens("one");

        let auth = persist_google_tokens(
            dir.path(),
            AuthCredentialsStoreMode::File,
            tokens.clone(),
            GoogleProviderKind::Gemini,
        )?;

        assert_eq!(auth.gemini_accounts, vec![tokens]);
        Ok(())
    }

    #[test]
    fn second_login_promotes_newest_to_front() -> std::io::Result<()> {
        let dir = tempdir()?;
        let first = sample_tokens("one");
        let second = sample_tokens("two");

        let initial = persist_google_tokens(
            dir.path(),
            AuthCredentialsStoreMode::File,
            first.clone(),
            GoogleProviderKind::Gemini,
        )?;
        assert_eq!(initial.gemini_accounts, vec![first.clone()]);

        let updated = persist_google_tokens(
            dir.path(),
            AuthCredentialsStoreMode::File,
            second.clone(),
            GoogleProviderKind::Gemini,
        )?;

        assert_eq!(updated.gemini_accounts, vec![second, first]);
        Ok(())
    }

    #[test]
    fn relogin_replaces_existing_account_entry() -> std::io::Result<()> {
        let dir = tempdir()?;
        let first = sample_tokens("one");
        let mut updated_token = sample_tokens("two");
        updated_token.email = first.email.clone();

        let initial = persist_google_tokens(
            dir.path(),
            AuthCredentialsStoreMode::File,
            first.clone(),
            GoogleProviderKind::Gemini,
        )?;
        assert_eq!(initial.gemini_accounts, vec![first]);

        let updated = persist_google_tokens(
            dir.path(),
            AuthCredentialsStoreMode::File,
            updated_token.clone(),
            GoogleProviderKind::Gemini,
        )?;

        assert_eq!(updated.gemini_accounts, vec![updated_token]);
        Ok(())
    }

    #[test]
    fn antigravity_tokens_persist_to_separate_collection() -> std::io::Result<()> {
        let dir = tempdir()?;
        let tokens = sample_tokens("antigravity");

        let saved = persist_google_tokens(
            dir.path(),
            AuthCredentialsStoreMode::File,
            tokens.clone(),
            GoogleProviderKind::Antigravity,
        )?;

        assert_eq!(saved.antigravity_accounts, vec![tokens]);
        assert!(saved.gemini_accounts.is_empty());
        Ok(())
    }
}
