//! One agent connection: resolves its project's config and serves the aggregated upstream tools.

mod enable;
mod login;
mod prompt;
mod render;
mod tools;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ErrorData as McpError, Peer, RoleServer, ServerHandler};
use tokio::time::Instant;

use crate::config::{self, ResolvedConfig, ResolvedServer, ServerIdentity, SessionPin};
use crate::identity::{self, VerifiedAccounts};
use crate::index;
use crate::paths::Paths;
use crate::project::{self, ProjectHints, ProjectLocation};
use crate::trust::Trust;
use crate::upstream::{ConnectError, Lease, MayConnect, Upstream, UpstreamKey, UpstreamPool};

use self::prompt::TrustPromptKey;
use self::tools::{
    DISABLE_TOOL, ENABLE_TOOL, STATUS_TOOL, TOOL_SEPARATOR, disable_tool, enable_tool,
    error_result, exposed_tools, login_tool, status_tool, text_result,
};

const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long a session start (`initialize` plus the first tool listing together), or a later
/// refresh or listing, waits for upstreams. Codex gives an MCP server 10 s to start and list its
/// tools; later arrivals are announced with `tools/list_changed`.
const READY_DEADLINE: Duration = Duration::from_secs(8);

/// [`READY_DEADLINE`], or less via `PLUGBAY_READY_DEADLINE_MS` (for tests, which would otherwise
/// sit through it). It can only shorten the wait.
fn ready_deadline() -> Duration {
    static DEADLINE: OnceLock<Duration> = OnceLock::new();
    *DEADLINE.get_or_init(|| {
        let configured = std::env::var("PLUGBAY_READY_DEADLINE_MS")
            .ok()
            .and_then(|millis| millis.parse().ok())
            .map(Duration::from_millis);
        configured.map_or(READY_DEADLINE, |deadline| deadline.min(READY_DEADLINE))
    })
}

/// State every session in one daemon shares.
pub struct Shared {
    pub paths: Paths,
    pub pool: Arc<UpstreamPool>,
    trust: Mutex<Trust>,
    sessions: Mutex<Vec<Weak<Session>>>,
    /// Untrusted config already put to the human this daemon run; an edit makes a new key, so it asks again.
    trust_prompted: Mutex<HashSet<TrustPromptKey>>,
    /// Authorization URLs of logins waiting for the browser, so repeated `__login` calls reuse them.
    logins_in_progress: Mutex<HashMap<(ServerIdentity, String), String>>,
}

impl Shared {
    pub fn new(paths: Paths) -> Arc<Self> {
        Arc::new(Self {
            trust: Mutex::new(Trust::new(&paths)),
            pool: UpstreamPool::new(paths.clone()),
            paths,
            sessions: Mutex::new(Vec::new()),
            trust_prompted: Mutex::new(HashSet::new()),
            logins_in_progress: Mutex::new(HashMap::new()),
        })
    }

    pub fn live_sessions(&self) -> Vec<Arc<Session>> {
        let mut sessions = self.sessions.lock();
        sessions.retain(|session| session.strong_count() > 0);
        sessions.iter().filter_map(Weak::upgrade).collect()
    }

    /// Re-resolves every session, e.g. after `plugbay login` or `plugbay trust`.
    pub async fn reload_all(&self) {
        let sessions = self.live_sessions();
        futures::future::join_all(sessions.iter().map(|session| session.refresh_and_notify()))
            .await;
    }

    /// Drops every `allow_once` grant, e.g. after `plugbay trust --revoke`, and re-checks every session.
    pub async fn forget_session_trust(&self) {
        *self.trust.lock() = Trust::new(&self.paths);
        self.trust_prompted.lock().clear();
        self.reload_all().await;
    }

    fn register(&self, session: &Arc<Session>) {
        self.sessions.lock().push(Arc::downgrade(session));
    }
}

pub struct Session {
    this: Weak<Session>,
    shared: Arc<Shared>,
    hints: Mutex<ProjectHints>,
    /// Servers the agent switched on or off with `plugbay__enable` / `plugbay__disable`; never saved.
    pins: Mutex<BTreeMap<String, SessionPin>>,
    /// Never held across an `.await`, so a slow upstream can't block status or other servers' calls.
    state: Mutex<SessionState>,
    /// Held from resolving config to applying it, so an older resolution never overwrites a newer one.
    refreshing: tokio::sync::Mutex<()>,
    /// Held by `plugbay__enable` / `__disable` from changing a pin to rolling it back, so a failed
    /// enable never restores its old pin over a newer one.
    pin_changes: tokio::sync::Mutex<()>,
    first_refresh: tokio::sync::OnceCell<()>,
    /// Signalled whenever a connection attempt finishes.
    connections_settled: tokio::sync::watch::Sender<()>,
    next_attempt: AtomicU64,
    /// `initialize` and the first tool listing share this one budget, counted from the connection.
    startup_deadline: Instant,
    /// Set when the client disconnects; pending connection attempts stop for good.
    ended: AtomicBool,
    peer: OnceLock<Peer<RoleServer>>,
    initialized: AtomicBool,
    tools_listed: AtomicBool,
}

#[derive(Default)]
struct SessionState {
    resolved: Option<Resolved>,
    servers: Vec<ActiveServer>,
}

struct Resolved {
    location: ProjectLocation,
    /// Config file stamps taken just before reading, so any later edit shows up as a difference.
    stamps: Vec<FileStamp>,
    outcome: Result<ResolvedConfig, String>,
}

/// Modification time and size of a config path; `None` when the file doesn't exist.
type FileStamp = Option<(SystemTime, u64)>;

struct ActiveServer {
    server: ResolvedServer,
    status: ServerStatus,
}

enum ServerStatus {
    /// A background task is connecting; it fills in the outcome for this `attempt`. The session
    /// aborts it when the entry is replaced or the session ends.
    Connecting {
        attempt: u64,
        key: UpstreamKey,
        task: tokio::task::AbortHandle,
    },
    Connected(Lease),
    NeedsLogin {
        challenge: String,
    },
    Untrusted,
    Failed(String),
    /// Connected, but the verified login is not the account this project expects.
    WrongAccount(String),
}

/// What a tool call turns into, decided under the state lock and carried out after it.
enum Route {
    Upstream {
        upstream: Arc<Upstream>,
        display_name: String,
    },
    Login {
        server: Box<ResolvedServer>,
        challenge: String,
    },
    Refuse(String),
}

impl Session {
    pub fn new(shared: Arc<Shared>, hints: ProjectHints) -> Arc<Self> {
        let session = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            shared: shared.clone(),
            hints: Mutex::new(hints),
            pins: Mutex::new(BTreeMap::new()),
            state: Mutex::new(SessionState::default()),
            refreshing: tokio::sync::Mutex::new(()),
            pin_changes: tokio::sync::Mutex::new(()),
            first_refresh: tokio::sync::OnceCell::new(),
            connections_settled: tokio::sync::watch::Sender::new(()),
            next_attempt: AtomicU64::new(0),
            startup_deadline: Instant::now() + ready_deadline(),
            ended: AtomicBool::new(false),
            peer: OnceLock::new(),
            initialized: AtomicBool::new(false),
            tools_listed: AtomicBool::new(false),
        });
        shared.register(&session);
        session
    }

    fn location(&self) -> ProjectLocation {
        project::detect(&self.hints.lock(), &self.shared.paths.home)
    }

    fn resolve_config(&self) -> Resolved {
        let location = self.location();
        let stamps = stamp_files(&config::watched_paths(&self.shared.paths, &location));
        let pins = self.pins.lock().clone();
        let outcome = config::resolve_with_pins(&self.shared.paths, &location, &pins)
            .map_err(|error| format!("{error:#}"));
        Resolved {
            location,
            stamps,
            outcome,
        }
    }

    /// The first refresh runs once, however many requests arrive while it is in flight.
    async fn ensure_ready(&self) {
        self.first_refresh
            .get_or_init(|| self.refresh_and_notify())
            .await;
    }

    /// Called when the client disconnects. Background work (a trust prompt's reload, a watcher
    /// tick) may still hold the session, so connection attempts are stopped here, not on drop.
    pub fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        for active in &self.state.lock().servers {
            active.stop_connecting();
        }
        self.connections_settled.send_replace(());
    }

    /// When the current wait for upstreams gives up: the startup budget while it lasts, so
    /// `initialize` and the first listing don't each get a full one; afterwards a fresh one.
    fn wait_deadline(&self) -> Instant {
        let now = Instant::now();
        if now < self.startup_deadline {
            self.startup_deadline
        } else {
            now + ready_deadline()
        }
    }

    pub async fn refresh_and_notify(&self) {
        let changed = self.refresh().await;
        if changed {
            self.notify_tools_changed().await;
        }
        self.offer_trust_prompt();
    }

    /// Re-resolves config and reconnects whatever changed, waiting until [`Self::wait_deadline`]
    /// for new connections. Returns true when the exposed tools may differ.
    async fn refresh(&self) -> bool {
        let _refreshing = self.refreshing.lock().await;
        if self.ended.load(Ordering::SeqCst) {
            return false;
        }
        let deadline = self.wait_deadline();
        let resolved = self.resolve_config();
        if let Ok(config) = &resolved.outcome {
            index::record(&self.shared.paths, config).await;
        }
        let wanted = self.wanted_servers(&resolved);
        let working_dir = resolved.location.working_dir.clone();

        let (previous_shape, attempts, replaced) = {
            let mut state = self.state.lock();
            let previous = std::mem::take(&mut state.servers);
            let previous_shape = tool_shape(&previous);
            let plan = self.plan_servers(wanted, previous, working_dir.as_ref());
            state.servers = plan.servers;
            state.resolved = Some(resolved);
            (previous_shape, plan.attempts, plan.replaced)
        };

        // A replaced attempt (e.g. its trust was revoked) must not go on to start anything.
        for active in &replaced {
            active.stop_connecting();
        }
        self.wait_for_attempts(&attempts, deadline).await;
        // Old leases go only after new ones exist, so an unchanged upstream isn't closed and reopened.
        drop(replaced);

        let verified = VerifiedAccounts::load(&self.shared.paths).await;
        let mut state = self.state.lock();
        turn_off_wrong_accounts(&mut state.servers, &verified);
        tool_shape(&state.servers) != previous_shape
    }

    /// Every server config enables, with whether its executable parts are trusted.
    fn wanted_servers(&self, resolved: &Resolved) -> Vec<(ResolvedServer, bool)> {
        let config = match &resolved.outcome {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, "config did not resolve");
                return Vec::new();
            }
        };
        let trust = self.shared.trust.lock();
        config
            .servers
            .iter()
            .map(|server| (server.clone(), trust.is_server_trusted(server, config)))
            .collect()
    }

    fn plan_servers(
        &self,
        wanted: Vec<(ResolvedServer, bool)>,
        previous: Vec<ActiveServer>,
        working_dir: Option<&PathBuf>,
    ) -> RefreshPlan {
        let mut reusable = previous;
        let mut plan = RefreshPlan::default();
        for (server, trusted) in wanted {
            // Trust first: a revoked grant must drop an already connected server too.
            if !trusted {
                plan.servers.push(ActiveServer {
                    server,
                    status: ServerStatus::Untrusted,
                });
                continue;
            }
            let key = UpstreamKey::for_server(&server, working_dir);
            if let Some(index) = reusable
                .iter()
                .position(|active| active.can_be_reused_for(&key))
            {
                let mut active = reusable.swap_remove(index);
                active.server = server;
                plan.servers.push(active);
                continue;
            }
            let attempt = self.next_attempt.fetch_add(1, Ordering::SeqCst);
            let task = self.spawn_connect(attempt, server.clone(), working_dir.cloned());
            plan.servers.push(ActiveServer {
                server,
                status: ServerStatus::Connecting { attempt, key, task },
            });
            plan.attempts.push(attempt);
        }
        plan.replaced = reusable;
        plan
    }

    /// Connects in the background; the outcome lands in whichever entry still waits for `attempt`.
    fn spawn_connect(
        &self,
        attempt: u64,
        server: ResolvedServer,
        working_dir: Option<PathBuf>,
    ) -> tokio::task::AbortHandle {
        let pool = self.shared.pool.clone();
        let session = self.this.clone();
        let task = tokio::spawn(async move {
            let may_connect = || {
                session
                    .upgrade()
                    .is_some_and(|session| session.may_still_connect(attempt))
            };
            let status = connect(&pool, &server, working_dir.as_ref(), &may_connect).await;
            if let Some(session) = session.upgrade() {
                session.finish_connecting(attempt, status).await;
            }
        });
        task.abort_handle()
    }

    /// Asked by a connect task once its secrets resolved, right before it starts the process or
    /// sends credentials: trust is read afresh, so a revoke that a pending refresh hasn't applied
    /// yet still stops it.
    fn may_still_connect(&self, attempt: u64) -> bool {
        if self.ended.load(Ordering::SeqCst) {
            return false;
        }
        let state = self.state.lock();
        let Some(config) = state.config() else {
            return false;
        };
        let Some(active) = state
            .servers
            .iter()
            .find(|active| active.is_connecting(attempt))
        else {
            return false;
        };
        self.shared
            .trust
            .lock()
            .is_server_trusted(&active.server, config)
    }

    async fn finish_connecting(&self, attempt: u64, status: ServerStatus) {
        let verified = VerifiedAccounts::load(&self.shared.paths).await;
        let exposes_tools = {
            let mut state = self.state.lock();
            let Some(active) = state
                .servers
                .iter_mut()
                .find(|active| active.is_connecting(attempt))
            else {
                // A newer refresh dropped this server; its lease goes with `status`.
                return;
            };
            active.status = status;
            turn_off_wrong_accounts(std::slice::from_mut(active), &verified);
            active.exposes_tools()
        };
        self.connections_settled.send_replace(());
        if exposes_tools {
            self.notify_tools_changed().await;
        }
    }

    /// Waits until none of `attempts` is still connecting, or `deadline` passes.
    async fn wait_for_attempts(&self, attempts: &[u64], deadline: Instant) {
        if attempts.is_empty() {
            return;
        }
        let mut settled = self.connections_settled.subscribe();
        let all_settled = settled
            .wait_for(|()| self.ended.load(Ordering::SeqCst) || !self.any_connecting(attempts));
        match tokio::time::timeout_at(deadline, all_settled).await {
            Ok(Ok(_settled)) => {}
            Ok(Err(error)) => tracing::debug!(%error, "stopped waiting for connections"),
            Err(_elapsed) => tracing::info!(
                "some servers are still connecting; the client hears when their tools arrive"
            ),
        }
    }

    fn any_connecting(&self, attempts: &[u64]) -> bool {
        let state = self.state.lock();
        attempts.iter().any(|attempt| {
            state
                .servers
                .iter()
                .any(|active| active.is_connecting(*attempt))
        })
    }

    /// A crashed stdio server or dropped remote connection is replaced on the next call instead of failing until a config edit.
    async fn reconnect_if_an_upstream_died(&self) {
        let any_closed = self.state.lock().servers.iter().any(|active| {
            matches!(&active.status, ServerStatus::Connected(lease) if lease.upstream.is_closed())
        });
        if any_closed {
            tracing::info!("an upstream connection closed, reconnecting");
            self.refresh_and_notify().await;
        }
    }

    async fn notify_tools_changed(&self) {
        let listed = self.tools_listed.load(Ordering::SeqCst);
        let Some(peer) = self.peer.get().filter(|_| listed) else {
            return;
        };
        if let Err(error) = peer.notify_tool_list_changed().await {
            tracing::debug!(%error, "could not send tools/list_changed");
        }
    }

    async fn list_all_tools(&self) -> Vec<Tool> {
        self.ensure_ready().await;
        let mut tools = vec![status_tool(), disable_tool()];
        let mut connected = Vec::new();
        {
            let state = self.state.lock();
            if let Some(config) = state.config() {
                tools.push(enable_tool(&enable::servers_to_enable(config)));
            }
            for active in &state.servers {
                match &active.status {
                    ServerStatus::Connected(lease) => {
                        connected.push((active.server.clone(), lease.upstream.clone()));
                    }
                    ServerStatus::NeedsLogin { .. } => tools.push(login_tool(&active.server)),
                    ServerStatus::Connecting { .. }
                    | ServerStatus::Untrusted
                    | ServerStatus::Failed(_)
                    | ServerStatus::WrongAccount(_) => {}
                }
            }
        }

        let deadline = self.wait_deadline();
        let listed = futures::future::join_all(
            connected
                .into_iter()
                .map(|(server, upstream)| self.tools_of(server, upstream, deadline)),
        )
        .await;
        tools.extend(listed.into_iter().flatten());
        tools
    }

    /// One server's tools, or none when it doesn't answer by `deadline`: the listing carries on
    /// and the client is told when it lands.
    async fn tools_of(
        &self,
        server: ResolvedServer,
        upstream: Arc<Upstream>,
        deadline: Instant,
    ) -> Vec<Tool> {
        let mut listing = tokio::spawn(async move { upstream.tools().await });
        let Ok(joined) = tokio::time::timeout_at(deadline, &mut listing).await else {
            tracing::info!(server = %server.display_name(), "listing tools is slow; the client hears when they arrive");
            self.notify_when_listed(listing);
            return Vec::new();
        };
        let listed = match joined {
            Ok(listed) => listed,
            Err(error) => Err(error.into()),
        };
        match listed {
            Ok(upstream_tools) => exposed_tools(&server, upstream_tools),
            Err(error) => {
                tracing::warn!(server = %server.display_name(), error = format!("{error:#}"), "listing tools failed");
                Vec::new()
            }
        }
    }

    fn notify_when_listed(&self, listing: tokio::task::JoinHandle<anyhow::Result<Vec<Tool>>>) {
        let session = self.this.clone();
        tokio::spawn(async move {
            let listed = matches!(listing.await, Ok(Ok(_)));
            if let Some(session) = session.upgrade().filter(|_| listed) {
                session.notify_tools_changed().await;
            }
        });
    }

    async fn call(&self, request: CallToolRequestParams) -> CallToolResult {
        self.ensure_ready().await;
        if request.name == STATUS_TOOL {
            return text_result(self.status_report().await);
        }
        self.reconnect_if_an_upstream_died().await;
        match request.name.as_ref() {
            ENABLE_TOOL => return self.enable(request.arguments).await,
            DISABLE_TOOL => return self.disable(request.arguments).await,
            _ => {}
        }
        let Some((prefix, tool_name)) = request.name.split_once(TOOL_SEPARATOR) else {
            return error_result(format!("unknown tool `{}`", request.name));
        };

        match self.route(prefix, tool_name) {
            Route::Refuse(message) => error_result(message),
            Route::Login { server, challenge } => self.start_login(*server, &challenge).await,
            Route::Upstream {
                upstream,
                display_name,
            } => {
                let mut upstream_request = request.clone();
                upstream_request.name = tool_name.to_string().into();
                match upstream.call_tool(upstream_request).await {
                    Ok(result) => result,
                    Err(error) => error_result(format!("{display_name} failed: {error:#}")),
                }
            }
        }
    }

    fn route(&self, prefix: &str, tool_name: &str) -> Route {
        let state = self.state.lock();
        let Some(active) = state
            .servers
            .iter()
            .find(|active| active.server.tool_prefix == prefix)
        else {
            return Route::Refuse(format!(
                "no server `{prefix}` here; call {STATUS_TOOL} to see what is configured"
            ));
        };
        let display_name = active.server.display_name();

        match &active.status {
            ServerStatus::NeedsLogin { challenge } if tool_name == "login" => Route::Login {
                server: Box::new(active.server.clone()),
                challenge: challenge.clone(),
            },
            ServerStatus::NeedsLogin { .. } => Route::Refuse(format!(
                "{display_name} needs a login: call `{prefix}__login`, or ask the human to run `plugbay login {display_name}` (with `--token` if it takes an API token)"
            )),
            ServerStatus::Connecting { .. } => Route::Refuse(format!(
                "{display_name} is still connecting; try again in a moment, or call {STATUS_TOOL}"
            )),
            ServerStatus::Untrusted => Route::Refuse(format!(
                "{display_name} runs a local command or reads secrets from config that is not trusted yet; ask the human to run `plugbay trust` in the project directory"
            )),
            ServerStatus::Failed(error) => {
                Route::Refuse(format!("{display_name} is unavailable: {error}"))
            }
            ServerStatus::WrongAccount(message) => Route::Refuse(message.clone()),
            ServerStatus::Connected(_) if !active.server.tool_filter.allows(tool_name) => {
                Route::Refuse(format!(
                    "`{tool_name}` is filtered out for {display_name} by plugbay config"
                ))
            }
            ServerStatus::Connected(lease) => Route::Upstream {
                upstream: lease.upstream.clone(),
                display_name,
            },
        }
    }

    async fn status_report(&self) -> String {
        let verified = VerifiedAccounts::load(&self.shared.paths).await;
        let mut report = {
            let state = self.state.lock();
            let Some(resolved) = &state.resolved else {
                return "plugbay has not resolved any config yet".to_string();
            };
            render::status_text(resolved, &state.servers, &verified)
        };
        let pins = self.pins.lock();
        if !pins.is_empty() {
            let described: Vec<String> = pins
                .iter()
                .map(|(name, pin)| match pin {
                    SessionPin::On {
                        account: Some(account),
                    } => format!("{name}:{account} on"),
                    SessionPin::On { account: None } => format!("{name} on"),
                    SessionPin::Off => format!("{name} off"),
                })
                .collect();
            report.push_str(&format!(
                "\nthis session only (not saved): {}",
                described.join(", ")
            ));
        }
        report
    }

    /// Learns the project from MCP roots when the shim's cwd was `/` or `$HOME`.
    #[expect(
        deprecated,
        reason = "roots are deprecated since spec 2026-07-28 but some clients still only offer them"
    )]
    async fn discover_project_from_roots(&self, peer: &Peer<RoleServer>) -> bool {
        let needs_roots = {
            let hints = self.hints.lock();
            hints.explicit.is_none()
                && hints.claude_project_dir.is_none()
                && project::cwd_is_ambiguous(hints.cwd.as_deref(), &self.shared.paths.home)
        };
        let client_offers_roots = peer
            .peer_info()
            .is_some_and(|info| info.capabilities.roots.is_some());
        if !needs_roots || !client_offers_roots {
            return false;
        }

        let roots = match peer.list_roots().await {
            Ok(result) => result.roots,
            Err(error) => {
                tracing::debug!(%error, "client did not answer roots/list");
                return false;
            }
        };
        let first_root = roots
            .iter()
            .find_map(|root| url::Url::parse(&root.uri).ok()?.to_file_path().ok());
        let Some(root) = first_root else {
            return false;
        };
        tracing::info!(root = %root.display(), "using MCP root as the project");
        self.hints.lock().root = Some(root);
        true
    }

    /// Re-resolves when a config file changes and relays upstream tool changes, until the session ends.
    fn spawn_watcher(self: &Arc<Self>) {
        let session = Arc::downgrade(self);
        let mut upstream_changes = self.shared.pool.subscribe_changes();
        let mut verified_accounts = self.shared.pool.subscribe_verified();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CONFIG_POLL_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    change = upstream_changes.recv() => {
                        let Some(session) = session.upgrade() else { break };
                        if change.is_ok() {
                            session.notify_tools_changed().await;
                        }
                        continue;
                    }
                    verified = verified_accounts.recv() => {
                        let Some(session) = session.upgrade() else { break };
                        if verified.is_ok() {
                            session.refresh_and_notify().await;
                        }
                        continue;
                    }
                }
                let Some(session) = session.upgrade() else {
                    break;
                };
                if session.config_changed_on_disk() {
                    tracing::info!("config changed, re-resolving");
                    session.refresh_and_notify().await;
                }
            }
        });
    }

    fn config_changed_on_disk(&self) -> bool {
        let state = self.state.lock();
        let Some(resolved) = &state.resolved else {
            return false;
        };
        let watched = config::watched_paths(&self.shared.paths, &resolved.location);
        stamp_files(&watched) != resolved.stamps
    }

    async fn after_initialized(self: Arc<Self>) {
        let Some(peer) = self.peer.get().cloned() else {
            return;
        };
        if self.discover_project_from_roots(&peer).await {
            self.refresh_and_notify().await;
        } else {
            self.offer_trust_prompt();
        }
    }
}

impl ServerHandler for Session {
    fn get_info(&self) -> InitializeResult {
        let verified = VerifiedAccounts::read_or_empty(&self.shared.paths);
        let instructions = {
            let state = self.state.lock();
            match &state.resolved {
                Some(resolved) => render::instructions(resolved, &state.servers, &verified),
                None => render::not_resolved_yet(),
            }
        };
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new("plugbay", crate::protocol::VERSION))
            .with_instructions(instructions)
    }

    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    /// Connects upstreams before answering, so the instructions name only servers that work.
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        context.peer.set_peer_info(request.clone());
        if self.peer.set(context.peer.clone()).is_err() {
            tracing::debug!("client initialized twice");
        }
        tracing::info!(client = %request.client_info.name, "session started");
        self.ensure_ready().await;
        self.negotiate_initialize(&request)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        let Some(session) = self.this.upgrade() else {
            return;
        };
        self.initialized.store(true, Ordering::SeqCst);
        session.spawn_watcher();
        tokio::spawn(session.after_initialized());
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Set first, so a server that lands while this listing runs still triggers a change.
        self.tools_listed.store(true, Ordering::SeqCst);
        let tools = self.list_all_tools().await;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        Ok(self.call(request).await.into())
    }
}

/// What a refresh keeps, starts and lets go.
#[derive(Default)]
struct RefreshPlan {
    servers: Vec<ActiveServer>,
    /// Connection attempts it started.
    attempts: Vec<u64>,
    /// Entries no longer wanted, held until the new connections exist.
    replaced: Vec<ActiveServer>,
}

impl SessionState {
    fn config(&self) -> Option<&ResolvedConfig> {
        self.resolved.as_ref()?.outcome.as_ref().ok()
    }
}

async fn connect(
    pool: &Arc<UpstreamPool>,
    server: &ResolvedServer,
    working_dir: Option<&PathBuf>,
    may_connect: MayConnect<'_>,
) -> ServerStatus {
    match pool.acquire(server, working_dir, may_connect).await {
        Ok(lease) => ServerStatus::Connected(lease),
        Err(ConnectError::NeedsLogin { challenge }) => ServerStatus::NeedsLogin { challenge },
        Err(ConnectError::Withdrawn) => {
            tracing::info!(server = %server.display_name(), "trust was withdrawn before connecting");
            ServerStatus::Untrusted
        }
        Err(ConnectError::Failed(error)) => {
            tracing::warn!(server = %server.display_name(), error = format!("{error:#}"), "upstream failed");
            ServerStatus::Failed(format!("{error:#}"))
        }
    }
}

/// Applies `expect` to every connected server; its lease goes, so the agent sees no tools for it.
fn turn_off_wrong_accounts(servers: &mut [ActiveServer], verified: &VerifiedAccounts) {
    for active in servers {
        if !matches!(active.status, ServerStatus::Connected(_)) {
            continue;
        }
        if let Some(message) = identity::expectation_failure(&active.server, verified) {
            tracing::warn!(server = %active.server.display_name(), "logged in as the wrong account");
            active.status = ServerStatus::WrongAccount(message);
        }
    }
}

impl ActiveServer {
    /// A live or pending connection to the same upstream serves the new config too (only filters
    /// or metadata changed).
    fn can_be_reused_for(&self, key: &UpstreamKey) -> bool {
        match &self.status {
            ServerStatus::Connected(lease) => {
                !lease.upstream.is_closed() && lease.upstream.key() == key
            }
            ServerStatus::Connecting {
                key: pending_key, ..
            } => pending_key == key,
            ServerStatus::NeedsLogin { .. }
            | ServerStatus::Untrusted
            | ServerStatus::Failed(_)
            | ServerStatus::WrongAccount(_) => false,
        }
    }

    fn stop_connecting(&self) {
        if let ServerStatus::Connecting { task, .. } = &self.status {
            task.abort();
        }
    }

    fn is_connecting(&self, attempt: u64) -> bool {
        matches!(self.status, ServerStatus::Connecting { attempt: pending, .. } if pending == attempt)
    }

    /// Real tools or a login stub.
    fn exposes_tools(&self) -> bool {
        matches!(
            self.status,
            ServerStatus::Connected(_) | ServerStatus::NeedsLogin { .. }
        )
    }
}

/// Everything that decides the exposed tools: each server's full config (prefix, filters, transport)
/// plus whether it exposes real tools, a login stub, or nothing.
fn tool_shape(servers: &[ActiveServer]) -> Vec<(ResolvedServer, &'static str)> {
    servers
        .iter()
        .map(|active| {
            let status = match active.status {
                ServerStatus::Connected(_) => "connected",
                ServerStatus::NeedsLogin { .. } => "login",
                ServerStatus::Connecting { .. }
                | ServerStatus::Untrusted
                | ServerStatus::Failed(_)
                | ServerStatus::WrongAccount(_) => "hidden",
            };
            (active.server.clone(), status)
        })
        .collect()
}

fn stamp_files(paths: &[PathBuf]) -> Vec<FileStamp> {
    paths
        .iter()
        .map(|path| {
            let metadata = std::fs::metadata(path).ok()?;
            Some((metadata.modified().ok()?, metadata.len()))
        })
        .collect()
}
