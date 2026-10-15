//! Upstream MCP clients shared across sessions per (server identity, account, working dir).

mod stdio;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation, Tool,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::sync::broadcast;

use crate::config::{ResolvedServer, ServerIdentity, ServerTransport};
use crate::paths::Paths;
use crate::{credentials, identity, oauth, secrets};

use self::stdio::StdioProcess;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Generous: some tools legitimately run for minutes, and clients enforce their own limits too.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamKey {
    pub identity: ServerIdentity,
    pub account: String,
    /// Stdio servers run per working dir (each worktree gets its own process); remote ones don't care.
    pub working_dir: Option<PathBuf>,
    /// Hash of the unresolved transport config (full URL, header and env templates, args), so two
    /// projects that send different credentials or query parameters never share a connection.
    pub transport_fingerprint: String,
}

impl UpstreamKey {
    pub fn for_server(server: &ResolvedServer, working_dir: Option<&PathBuf>) -> Self {
        let working_dir = match &server.transport {
            ServerTransport::Stdio { cwd: Some(cwd), .. } => Some(cwd.clone()),
            ServerTransport::Stdio { cwd: None, .. } => working_dir.cloned(),
            ServerTransport::Http { .. } => None,
        };
        Self {
            identity: server.identity.clone(),
            account: server.account.clone(),
            working_dir,
            transport_fingerprint: transport_fingerprint(&server.transport),
        }
    }
}

/// Hashes the `Debug` form, which is deterministic here (strings, ordered maps, lists) and
/// holds only secret references, never resolved values.
fn transport_fingerprint(transport: &ServerTransport) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(format!("{transport:?}").as_bytes()))
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Carries the server's `WWW-Authenticate` challenge, which tells the login where to discover OAuth.
    #[error("not logged in")]
    NeedsLogin { challenge: String },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// One live connection to an upstream server, with its tool list cached until it reports a change.
pub struct Upstream {
    key: UpstreamKey,
    service: RunningService<RoleClient, UpstreamClient>,
    tools: tokio::sync::Mutex<Option<Vec<Tool>>>,
    /// Set for stdio servers; stops their process group on drop or daemon shutdown.
    process: Option<parking_lot::Mutex<StdioProcess>>,
    /// Set when the pool retires it (credentials changed): closed for every holder from then on.
    retired: AtomicBool,
}

impl Upstream {
    pub fn key(&self) -> &UpstreamKey {
        &self.key
    }

    fn stop_process(&self) {
        if let Some(process) = &self.process {
            process.lock().terminate();
        }
    }

    /// True once the upstream exited, dropped the connection, or was shut down.
    pub fn is_closed(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
            || self.service.is_closed()
            || self.service.peer().is_transport_closed()
    }

    pub async fn tools(&self) -> anyhow::Result<Vec<Tool>> {
        let mut cached = self.tools.lock().await;
        if self.service.service().take_tools_changed() {
            *cached = None;
        }
        if let Some(tools) = cached.as_ref() {
            return Ok(tools.clone());
        }
        let tools = tokio::time::timeout(REQUEST_TIMEOUT, self.service.peer().list_all_tools())
            .await
            .context("listing tools timed out")?
            .context("listing tools failed")?;
        *cached = Some(tools.clone());
        Ok(tools)
    }

    pub async fn call_tool(
        &self,
        request: CallToolRequestParams,
    ) -> anyhow::Result<CallToolResult> {
        tokio::time::timeout(CALL_TIMEOUT, self.service.peer().call_tool(request))
            .await
            .context("the tool call timed out")?
            .context("the tool call failed")
    }
}

/// Our side of an upstream connection: identifies as plugbay and relays tool list changes.
pub struct UpstreamClient {
    key: UpstreamKey,
    changes: broadcast::Sender<UpstreamKey>,
    tools_changed: AtomicBool,
}

impl UpstreamClient {
    /// Reads and clears the flag set when the upstream announced a tool list change.
    fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::SeqCst)
    }
}

impl ClientHandler for UpstreamClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("plugbay", crate::protocol::VERSION),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tracing::debug!(identity = %self.key.identity, account = self.key.account, "upstream tools changed");
        self.tools_changed.store(true, Ordering::SeqCst);
        if self.changes.send(self.key.clone()).is_err() {
            tracing::debug!("no session is listening for upstream changes");
        }
    }
}

/// Connections shared by every session in the daemon, closed after sitting unused for a grace period.
pub struct UpstreamPool {
    paths: Paths,
    entries: parking_lot::Mutex<HashMap<UpstreamKey, PoolEntry>>,
    changes: broadcast::Sender<UpstreamKey>,
    /// Upstreams whose verified account changed, so sessions re-apply `expect`.
    verified: broadcast::Sender<UpstreamKey>,
}

struct PoolEntry {
    /// Serialises connection attempts so concurrent sessions share one connection.
    slot: Arc<tokio::sync::Mutex<Option<Arc<Upstream>>>>,
    leases: usize,
    idle_since: Option<Instant>,
}

/// A session's claim on an upstream. Dropping it lets the pool close the connection once nobody uses it.
pub struct Lease {
    pub upstream: Arc<Upstream>,
    pool: Arc<UpstreamPool>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.release(&self.upstream.key);
    }
}

impl UpstreamPool {
    pub fn new(paths: Paths) -> Arc<Self> {
        let (changes, _) = broadcast::channel(64);
        let (verified, _) = broadcast::channel(64);
        Arc::new(Self {
            paths,
            entries: parking_lot::Mutex::new(HashMap::new()),
            changes,
            verified,
        })
    }

    /// Notifies about upstreams whose tool lists changed.
    pub fn subscribe_changes(&self) -> broadcast::Receiver<UpstreamKey> {
        self.changes.subscribe()
    }

    /// Notifies about upstreams whose login was verified as a different account than recorded.
    pub fn subscribe_verified(&self) -> broadcast::Receiver<UpstreamKey> {
        self.verified.subscribe()
    }

    pub async fn acquire(
        self: &Arc<Self>,
        server: &ResolvedServer,
        working_dir: Option<&PathBuf>,
    ) -> Result<Lease, ConnectError> {
        let key = UpstreamKey::for_server(server, working_dir);
        let slot = self.claim(&key);

        let mut connection = slot.lock().await;
        if let Some(upstream) = connection.as_ref().filter(|upstream| !upstream.is_closed()) {
            return Ok(Lease {
                upstream: upstream.clone(),
                pool: self.clone(),
            });
        }

        match connect(server, &key, self.changes.clone()).await {
            Ok(upstream) => {
                let upstream = Arc::new(upstream);
                *connection = Some(upstream.clone());
                self.verify_account(&upstream);
                Ok(Lease {
                    upstream,
                    pool: self.clone(),
                })
            }
            Err(error) => {
                drop(connection);
                self.release(&key);
                Err(error)
            }
        }
    }

    /// Once per connection: the credentials it was made with are the ones the answer describes.
    fn verify_account(&self, upstream: &Arc<Upstream>) {
        let verified = self.verified.clone();
        let key = upstream.key.clone();
        identity::spawn_check(&self.paths, upstream.clone(), move || {
            if verified.send(key).is_err() {
                tracing::debug!("no session is listening for verified accounts");
            }
        });
    }

    /// Closes connections nobody has leased for `grace`.
    pub async fn close_idle(&self, grace: Duration) {
        let idle_slots: Vec<_> = {
            let mut entries = self.lock_entries();
            let idle_keys: Vec<UpstreamKey> = entries
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .idle_since
                        .is_some_and(|since| since.elapsed() >= grace)
                })
                .map(|(key, _)| key.clone())
                .collect();
            idle_keys
                .into_iter()
                .filter_map(|key| entries.remove(&key).map(|entry| (key, entry.slot)))
                .collect()
        };

        for (key, slot) in idle_slots {
            let upstream = slot.lock().await.take();
            if let Some(upstream) = upstream {
                tracing::info!(identity = %key.identity, account = key.account, "closing idle upstream");
                close(upstream).await;
            }
        }
    }

    /// Closes the connections of one server account, e.g. after its credentials changed. Sessions
    /// see their lease closed on the next refresh and reconnect with the new credentials.
    pub async fn close_account(&self, identity: &ServerIdentity, account: &str) -> usize {
        let slots: Vec<_> = self
            .lock_entries()
            .iter()
            .filter(|(key, _)| &key.identity == identity && key.account == account)
            .map(|(_, entry)| entry.slot.clone())
            .collect();
        let mut closed = 0;
        for slot in slots {
            if let Some(upstream) = slot.lock().await.take() {
                upstream.retired.store(true, Ordering::SeqCst);
                upstream.stop_process();
                close(upstream).await;
                closed += 1;
            }
        }
        closed
    }

    /// Closes every connection; used when the daemon exits so no upstream process outlives it.
    pub async fn shutdown(&self) {
        let slots: Vec<_> = self
            .lock_entries()
            .drain()
            .map(|(_, entry)| entry.slot)
            .collect();
        for slot in slots {
            if let Some(upstream) = slot.lock().await.take() {
                upstream.stop_process();
                close(upstream).await;
            }
        }
    }

    pub fn connection_count(&self) -> usize {
        self.lock_entries().len()
    }

    fn claim(&self, key: &UpstreamKey) -> Arc<tokio::sync::Mutex<Option<Arc<Upstream>>>> {
        let mut entries = self.lock_entries();
        let entry = entries.entry(key.clone()).or_insert_with(|| PoolEntry {
            slot: Arc::new(tokio::sync::Mutex::new(None)),
            leases: 0,
            idle_since: None,
        });
        entry.leases += 1;
        entry.idle_since = None;
        entry.slot.clone()
    }

    fn release(&self, key: &UpstreamKey) {
        let mut entries = self.lock_entries();
        if let Some(entry) = entries.get_mut(key) {
            entry.leases = entry.leases.saturating_sub(1);
            if entry.leases == 0 {
                entry.idle_since = Some(Instant::now());
            }
        }
    }

    fn lock_entries(&self) -> parking_lot::MutexGuard<'_, HashMap<UpstreamKey, PoolEntry>> {
        self.entries.lock()
    }
}

async fn close(upstream: Arc<Upstream>) {
    match Arc::try_unwrap(upstream) {
        Ok(upstream) => {
            if let Err(error) = upstream.service.cancel().await {
                tracing::warn!(%error, "upstream did not shut down cleanly");
            }
        }
        Err(still_shared) => {
            // A session still holds it mid-call; cancelling via the token stops it once that ends.
            still_shared.service.cancellation_token().cancel();
        }
    }
}

async fn connect(
    server: &ResolvedServer,
    key: &UpstreamKey,
    changes: broadcast::Sender<UpstreamKey>,
) -> Result<Upstream, ConnectError> {
    let client = UpstreamClient {
        key: key.clone(),
        changes,
        tools_changed: AtomicBool::new(false),
    };
    tracing::info!(server = %server.display_name(), identity = %key.identity, "connecting upstream");

    let (service, process) = match &server.transport {
        ServerTransport::Http { url, headers } => (
            connect_http(server, url.as_str(), headers, client).await?,
            None,
        ),
        ServerTransport::Stdio { env, .. } => {
            let (service, process) =
                connect_stdio(server, env, key.working_dir.as_deref(), client).await?;
            (service, Some(parking_lot::Mutex::new(process)))
        }
    };

    Ok(Upstream {
        key: key.clone(),
        service,
        tools: tokio::sync::Mutex::new(None),
        process,
        retired: AtomicBool::new(false),
    })
}

/// Credentials come from config headers, else a stored token, else a stored OAuth login.
async fn connect_http(
    server: &ResolvedServer,
    url: &str,
    headers: &BTreeMap<String, String>,
    client: UpstreamClient,
) -> Result<RunningService<RoleClient, UpstreamClient>, ConnectError> {
    let mut resolved_headers = secrets::resolve_map(headers)
        .await
        .with_context(|| format!("resolving headers for {}", server.display_name()))?;
    if !server.uses_stored_login() {
        return serve_http(client, url, &resolved_headers, None).await;
    }

    let stored_token = credentials::load_token(&server.identity, &server.account)
        .await
        .with_context(|| format!("reading the stored token for {}", server.display_name()))?;
    let Some(token) = stored_token else {
        let oauth_client = stored_oauth_client(server).await;
        return serve_http(client, url, &resolved_headers, oauth_client).await;
    };

    // Headers set in config win: they are the more specific choice for this project.
    resolved_headers.entry(token.header).or_insert(token.value);
    match serve_http(client, url, &resolved_headers, None).await {
        Err(ConnectError::NeedsLogin { .. }) => Err(ConnectError::Failed(anyhow::anyhow!(
            "the server rejected the stored token; run `plugbay login {} --token` with a new one",
            server.display_name()
        ))),
        other => other,
    }
}

/// With an `oauth_client`, rmcp adds (and refreshes) the OAuth token on every request.
async fn serve_http(
    client: UpstreamClient,
    url: &str,
    headers: &BTreeMap<String, String>,
    oauth_client: Option<AuthClient<reqwest::Client>>,
) -> Result<RunningService<RoleClient, UpstreamClient>, ConnectError> {
    let config =
        StreamableHttpClientTransportConfig::with_uri(url).custom_headers(header_map(headers)?);
    match oauth_client {
        Some(auth_client) => {
            let transport = StreamableHttpClientTransport::with_client(auth_client, config);
            timed_serve(client, transport).await
        }
        None => {
            let transport =
                StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
            timed_serve(client, transport).await
        }
    }
}

async fn connect_stdio(
    server: &ResolvedServer,
    env: &BTreeMap<String, String>,
    working_dir: Option<&Path>,
    client: UpstreamClient,
) -> Result<(RunningService<RoleClient, UpstreamClient>, StdioProcess), ConnectError> {
    let ServerTransport::Stdio { command, args, .. } = &server.transport else {
        return Err(ConnectError::Failed(anyhow::anyhow!(
            "{} is not a stdio server",
            server.display_name()
        )));
    };
    let resolved_env = secrets::resolve_map(env)
        .await
        .with_context(|| format!("resolving env for {}", server.display_name()))?;

    let (process, pipes) = stdio::spawn(stdio::SpawnRequest {
        server_name: server.display_name(),
        command,
        args,
        env: &resolved_env,
        secret_values: stdio::secret_env_values(env, &resolved_env),
        working_dir,
    })?;
    let service = timed_serve(client, (pipes.stdout, pipes.stdin)).await?;
    Ok((service, process))
}

/// A keychain or discovery failure must not hide the login path: connect without a token and
/// let the server's 401 turn into the `<server>__login` tool.
async fn stored_oauth_client(server: &ResolvedServer) -> Option<AuthClient<reqwest::Client>> {
    match oauth::authorized_client(server).await {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(server = %server.display_name(), error = format!("{error:#}"), "could not load the stored login");
            None
        }
    }
}

async fn timed_serve<T, E, A>(
    client: UpstreamClient,
    transport: T,
) -> Result<RunningService<RoleClient, UpstreamClient>, ConnectError>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(CONNECT_TIMEOUT, client.serve(transport)).await {
        Err(_) => Err(ConnectError::Failed(anyhow::anyhow!(
            "no initialize response within {}s",
            CONNECT_TIMEOUT.as_secs()
        ))),
        Ok(Ok(service)) => Ok(service),
        Ok(Err(error)) => {
            let challenge = error.auth_challenge().map(str::to_string);
            match challenge {
                Some(challenge) => Err(ConnectError::NeedsLogin { challenge }),
                None => Err(ConnectError::Failed(error.into())),
            }
        }
    }
}

fn header_map(
    headers: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>> {
    let mut map = HashMap::new();
    for (name, value) in headers {
        let header_name = reqwest::header::HeaderName::try_from(name.as_str())
            .with_context(|| format!("invalid header name `{name}`"))?;
        let mut header_value = reqwest::header::HeaderValue::try_from(value.as_str())
            .with_context(|| format!("invalid value for header `{name}`"))?;
        header_value.set_sensitive(true);
        map.insert(header_name, header_value);
    }
    Ok(map)
}
