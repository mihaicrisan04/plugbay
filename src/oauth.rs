//! OAuth login for remote servers (discovery, PKCE, CIMD/DCR via rmcp) with a loopback callback.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use rmcp::transport::auth::{
    AuthClient, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

use crate::config::{ResolvedServer, ServerTransport};
use crate::credentials::{self, KeychainCredentialStore};

const CLIENT_NAME: &str = "plugbay";
const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

const SUCCESS_PAGE: &str = "<!doctype html><title>plugbay</title>\
    <p>Logged in. You can close this tab and go back to your terminal.</p>";
const FAILURE_PAGE: &str = "<!doctype html><title>plugbay</title>\
    <p>Login failed. Check your terminal for details.</p>";

/// A started login: show `authorization_url` to the human, then `finish` waits for the browser redirect.
pub struct LoginFlow {
    pub authorization_url: String,
    session: AuthorizationSession,
    listener: TcpListener,
    server_display_name: String,
}

/// Discovers the server's authorization server, registers a client if needed, binds a
/// loopback callback listener and builds the authorization URL. Rejects metadata that
/// points at non-https endpoints (except for a loopback server) — server metadata is hostile input.
///
/// `challenge` is the `WWW-Authenticate` header of the server's 401, when there was one:
/// its `resource_metadata` URL and `scope` hint steer discovery.
pub async fn start_login(
    server: &ResolvedServer,
    challenge: Option<&str>,
) -> anyhow::Result<LoginFlow> {
    let server_display_name = server.display_name();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding the loopback OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let manager = discovered_manager(server, challenge).await?;
    let request = AuthorizationRequest::new(redirect_uri).with_client_name(CLIENT_NAME);
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_manager, error)| error)
        .with_context(|| format!("starting the OAuth flow for {server_display_name}"))?;

    let authorization_url =
        validate_endpoint_url(session.get_authorization_url(), oauth_url(server)?)
            .context("the authorization URL the server pointed us to is not safe to open")?
            .to_string();
    tracing::info!(server = %server_display_name, "OAuth login started");

    Ok(LoginFlow {
        authorization_url,
        session,
        listener,
        server_display_name,
    })
}

impl LoginFlow {
    /// Waits (up to 10 minutes) for the callback, exchanges the code and stores the
    /// credentials in the keychain under the server's identity + account.
    pub async fn finish(self) -> anyhow::Result<()> {
        let callback_url = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&self.listener))
            .await
            .with_context(|| {
                format!(
                    "timed out after {} minutes waiting for the browser to finish logging in to {}",
                    LOGIN_TIMEOUT.as_secs() / 60,
                    self.server_display_name
                )
            })??;

        self.session
            .handle_callback_url(&callback_url)
            .await
            .with_context(|| {
                format!(
                    "exchanging the authorization code for {}",
                    self.server_display_name
                )
            })?;
        tracing::info!(server = %self.server_display_name, "OAuth login finished");
        Ok(())
    }
}

/// An HTTP client that adds (and refreshes) the stored OAuth token, or `None` when
/// nothing is stored for this server's identity + account.
///
/// Pass it to `StreamableHttpClientTransport::with_client`. A server that rejects the
/// token surfaces as `StreamableHttpError::AuthRequired` once a silent refresh failed.
/// `http_client` carries the MCP requests; rmcp's own client (no cross-origin redirects, 30s
/// timeout) handles discovery and token requests.
pub async fn authorized_client(
    server: &ResolvedServer,
    http_client: reqwest::Client,
) -> anyhow::Result<Option<AuthClient<reqwest::Client>>> {
    oauth_url(server)?;
    if !credentials::has_stored_credentials(&server.identity, &server.account).await? {
        return Ok(None);
    }

    // With metadata already set, rmcp reuses it instead of rediscovering unvalidated
    // endpoints, so the refresh token only ever goes to a validated token endpoint.
    let mut manager = discovered_manager(server, None).await?;
    let usable = manager.initialize_from_store().await.with_context(|| {
        format!(
            "loading stored OAuth credentials for {}",
            server.display_name()
        )
    })?;
    if !usable {
        tracing::warn!(
            server = %server.display_name(),
            "stored OAuth credentials were issued by another authorization server and were dropped"
        );
        return Ok(None);
    }
    Ok(Some(AuthClient::new(http_client, manager)))
}

fn oauth_url(server: &ResolvedServer) -> anyhow::Result<&Url> {
    match &server.transport {
        ServerTransport::Http { url, .. } => Ok(url),
        ServerTransport::Stdio { .. } => bail!(
            "{} is a stdio server; only remote (http) servers log in with OAuth",
            server.display_name()
        ),
    }
}

/// An rmcp manager with our keychain store and freshly discovered, validated metadata.
async fn discovered_manager(
    server: &ResolvedServer,
    challenge: Option<&str>,
) -> anyhow::Result<AuthorizationManager> {
    let url = oauth_url(server)?;
    let server_display_name = server.display_name();
    let mut manager = AuthorizationManager::new(url.as_str())
        .await
        .with_context(|| format!("preparing OAuth for {server_display_name}"))?;
    manager.set_credential_store(KeychainCredentialStore::new(
        &server.identity,
        &server.account,
    ));

    let resolution = manager
        .resolve_metadata_from_challenge(challenge)
        .await
        .with_context(|| format!("discovering how {server_display_name} authorizes clients"))?;
    if !resolution.source.is_discovered() {
        tracing::warn!(
            server = %server_display_name,
            "server publishes no OAuth metadata; trying the legacy default endpoints"
        );
    }
    validate_metadata(&resolution.metadata, url)
        .with_context(|| format!("{server_display_name} published unsafe OAuth metadata"))?;
    manager.set_metadata(resolution.metadata);
    Ok(manager)
}

fn validate_metadata(metadata: &AuthorizationMetadata, server_url: &Url) -> anyhow::Result<()> {
    validate_endpoint_url(&metadata.authorization_endpoint, server_url)
        .context("rejecting the authorization endpoint")?;
    validate_endpoint_url(&metadata.token_endpoint, server_url)
        .context("rejecting the token endpoint")?;
    if let Some(registration_endpoint) = &metadata.registration_endpoint {
        validate_endpoint_url(registration_endpoint, server_url)
            .context("rejecting the client registration endpoint")?;
    }
    Ok(())
}

/// Accepts `https://` URLs. Plain `http://` only when both the endpoint and the MCP server
/// are on loopback (local test servers); a remote server never gets to send tokens to a local port.
fn validate_endpoint_url(value: &str, server_url: &Url) -> anyhow::Result<Url> {
    let url = Url::parse(value).with_context(|| format!("`{value}` is not a valid URL"))?;
    let Some(host) = url.host_str() else {
        bail!("`{value}` has no host");
    };
    let server_is_local = server_url.host_str().is_some_and(is_loopback_host);
    match url.scheme() {
        "https" => Ok(url),
        "http" if server_is_local && is_loopback_host(host) => Ok(url),
        "http" => bail!("`{value}` uses plain http on a non-loopback host; only https is allowed"),
        scheme => bail!("`{value}` uses the `{scheme}` scheme; only https is allowed"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Serves the loopback listener until the browser hits `/callback`, and returns the
/// full callback URL (with `code`, `state` and possibly `iss`).
async fn wait_for_callback(listener: &TcpListener) -> anyhow::Result<String> {
    let port = listener.local_addr()?.port();
    loop {
        let (mut stream, _peer) = listener
            .accept()
            .await
            .context("accepting the OAuth callback connection")?;
        let request_head = match read_request_head(&mut stream).await {
            Ok(request_head) => request_head,
            Err(error) => {
                tracing::debug!(%error, "ignoring an unreadable callback connection");
                continue;
            }
        };

        let outcome = classify_callback(&request_head, port);
        match outcome {
            CallbackOutcome::Unrelated => {
                respond(&mut stream, "404 Not Found", "").await;
            }
            CallbackOutcome::Denied(reason) => {
                respond(&mut stream, "200 OK", FAILURE_PAGE).await;
                bail!("the authorization server refused the login: {reason}");
            }
            CallbackOutcome::Authorized(callback_url) => {
                respond(&mut stream, "200 OK", SUCCESS_PAGE).await;
                return Ok(callback_url);
            }
        }
    }
}

async fn read_request_head(stream: &mut TcpStream) -> anyhow::Result<String> {
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = tokio::time::timeout(CALLBACK_READ_TIMEOUT, stream.read(&mut buffer))
            .await
            .context("the callback request stalled")??;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
        if head.len() > MAX_REQUEST_HEAD_BYTES {
            bail!("callback request head is too large");
        }
    }
    String::from_utf8(head).context("callback request is not UTF-8")
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(error) = stream.write_all(response.as_bytes()).await {
        tracing::debug!(%error, "could not answer the OAuth callback request");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CallbackOutcome {
    /// Something else, like `/favicon.ico`.
    Unrelated,
    /// The authorization server redirected back with `error=...`.
    Denied(String),
    Authorized(String),
}

fn classify_callback(request_head: &str, port: u16) -> CallbackOutcome {
    let Some(target) = request_target(request_head) else {
        return CallbackOutcome::Unrelated;
    };
    let Ok(url) = Url::parse(&format!("http://127.0.0.1:{port}{target}")) else {
        return CallbackOutcome::Unrelated;
    };
    if url.path() != CALLBACK_PATH {
        return CallbackOutcome::Unrelated;
    }

    let mut error = None;
    let mut error_description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    match (error, error_description) {
        (Some(error), Some(description)) => {
            CallbackOutcome::Denied(format!("{error}: {description}"))
        }
        (Some(error), None) => CallbackOutcome::Denied(error),
        (None, _) => CallbackOutcome::Authorized(url.to_string()),
    }
}

/// The path and query of a `GET <target> HTTP/1.1` request line.
fn request_target(request_head: &str) -> Option<&str> {
    let request_line = request_head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" || !target.starts_with('/') {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_server() -> Url {
        Url::parse("https://mcp.example.test/mcp").unwrap()
    }

    fn local_server() -> Url {
        Url::parse("http://127.0.0.1:3000/mcp").unwrap()
    }

    #[test]
    fn https_endpoints_are_accepted() {
        assert!(
            validate_endpoint_url("https://mcp.linear.app/authorize", &remote_server()).is_ok()
        );
    }

    #[test]
    fn plain_http_is_only_accepted_on_loopback_for_a_loopback_server() {
        assert!(validate_endpoint_url("http://127.0.0.1:8080/authorize", &local_server()).is_ok());
        assert!(validate_endpoint_url("http://localhost/authorize", &local_server()).is_ok());
        assert!(validate_endpoint_url("http://[::1]:9000/token", &local_server()).is_ok());
        assert!(validate_endpoint_url("http://evil.example/authorize", &local_server()).is_err());
    }

    #[test]
    fn remote_server_cannot_point_endpoints_at_local_ports() {
        assert!(validate_endpoint_url("http://127.0.0.1:8080/token", &remote_server()).is_err());
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        // mcp-remote CVE-2025-6514: a crafted endpoint reached a shell through the browser opener.
        let server = remote_server();
        assert!(validate_endpoint_url("file:///etc/passwd", &server).is_err());
        assert!(validate_endpoint_url("javascript:alert(1)", &server).is_err());
        assert!(validate_endpoint_url("$(touch /tmp/pwned)", &server).is_err());
    }

    #[test]
    fn callback_with_code_returns_the_full_url() {
        let head = "GET /callback?code=test-code&state=test-state&iss=https%3A%2F%2Fauth.example HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let CallbackOutcome::Authorized(url) = classify_callback(head, 4242) else {
            panic!("expected an authorized callback");
        };
        assert!(url.starts_with("http://127.0.0.1:4242/callback?code=test-code&state=test-state"));
    }

    #[test]
    fn callback_with_error_is_a_denial() {
        let head =
            "GET /callback?error=access_denied&error_description=User+said+no HTTP/1.1\r\n\r\n";
        assert_eq!(
            classify_callback(head, 1),
            CallbackOutcome::Denied("access_denied: User said no".to_string())
        );
    }

    #[test]
    fn other_paths_are_ignored() {
        let head = "GET /favicon.ico HTTP/1.1\r\n\r\n";
        assert_eq!(classify_callback(head, 1), CallbackOutcome::Unrelated);
    }

    #[tokio::test]
    async fn loopback_listener_returns_the_callback_after_ignoring_other_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let browser = tokio::spawn(async move {
            for target in ["/favicon.ico", "/callback?code=test-code&state=test-state"] {
                let mut stream = TcpStream::connect(address).await.unwrap();
                let request = format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n");
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).await.unwrap();
            }
        });

        let callback_url = wait_for_callback(&listener).await.unwrap();
        browser.await.unwrap();
        assert_eq!(
            callback_url,
            format!(
                "http://127.0.0.1:{}/callback?code=test-code&state=test-state",
                address.port()
            )
        );
    }

    /// Serves `metadata` as the authorization server metadata of a loopback MCP server
    /// and 404s everything else, like a server without protected resource metadata.
    async fn serve_authorization_metadata(metadata: serde_json::Value) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        let mut metadata = metadata;
        metadata["issuer"] = serde_json::Value::String(issuer.clone());
        let body = metadata.to_string();

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _peer)) = listener.accept().await else {
                    return;
                };
                let Ok(head) = read_request_head(&mut stream).await else {
                    continue;
                };
                let target = request_target(&head).unwrap_or_default();
                let response = if target == "/.well-known/oauth-authorization-server" {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Url::parse(&format!("{issuer}/")).unwrap()
    }

    fn server_at(url: Url) -> ResolvedServer {
        ResolvedServer {
            name: "local".to_string(),
            account: "test".to_string(),
            tool_prefix: "local".to_string(),
            identity: crate::config::ServerIdentity(url.to_string()),
            transport: ServerTransport::Http {
                url,
                headers: std::collections::BTreeMap::new(),
            },
            tool_filter: crate::config::ToolFilter::default(),
            account_metadata: std::collections::BTreeMap::new(),
            provenance: std::collections::BTreeMap::new(),
            requires_trust_from: Vec::new(),
        }
    }

    #[tokio::test]
    async fn rediscovered_metadata_with_a_plaintext_token_endpoint_is_rejected() {
        let url = serve_authorization_metadata(serde_json::json!({
            "authorization_endpoint": "https://auth.example.test/authorize",
            "token_endpoint": "http://evil.example.test/token",
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        }))
        .await;

        let Err(error) = discovered_manager(&server_at(url), None).await else {
            panic!("metadata with a plaintext token endpoint was accepted");
        };
        assert!(format!("{error:#}").contains("rejecting the token endpoint"));
    }

    #[tokio::test]
    async fn rediscovered_metadata_with_safe_endpoints_is_accepted() {
        let url = serve_authorization_metadata(serde_json::json!({
            "authorization_endpoint": "https://auth.example.test/authorize",
            "token_endpoint": "https://auth.example.test/token",
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
        }))
        .await;

        assert!(discovered_manager(&server_at(url), None).await.is_ok());
    }
}
