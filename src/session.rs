//! One agent connection: resolves its project's config and serves the aggregated upstream tools.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
    ElicitationAction, ElicitationSchema, EnumSchema, Implementation, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, PrimitiveSchemaDefinition,
    ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::{ElicitationMode, NotificationContext, RequestContext};
use rmcp::{ErrorData as McpError, Peer, RoleServer, ServerHandler};
use serde::Deserialize;

use crate::config::{self, ConfigFile, ResolvedConfig, ResolvedServer, ServerIdentity, SessionPin};
use crate::identity::{self, VerifiedAccounts};
use crate::index;
use crate::paths::Paths;
use crate::project::{self, ProjectHints, ProjectLocation};
use crate::trust::{self, Trust};
use crate::upstream::{ConnectError, Lease, UpstreamPool};
use crate::{oauth, registry};

const TOOL_SEPARATOR: &str = "__";
const MAX_TOOL_NAME_LENGTH: usize = 64;
const STATUS_TOOL: &str = "plugbay__status";
const ENABLE_TOOL: &str = "plugbay__enable";
const DISABLE_TOOL: &str = "plugbay__disable";
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// State every session in one daemon shares.
pub struct Shared {
    pub paths: Paths,
    pub pool: Arc<UpstreamPool>,
    trust: Mutex<Trust>,
    sessions: Mutex<Vec<Weak<Session>>>,
    /// Config dirs whose trust prompt was already shown; one prompt per folder per daemon run.
    trust_prompted: Mutex<HashSet<PathBuf>>,
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
        for session in self.live_sessions() {
            session.refresh_and_notify().await;
        }
    }

    /// Drops every `allow_once` grant, e.g. after `plugbay trust --revoke`, and re-checks every session.
    pub async fn forget_session_trust(&self) {
        *self.trust.lock() = Trust::new(&self.paths);
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
    state: tokio::sync::Mutex<SessionState>,
    peer: OnceLock<Peer<RoleServer>>,
    tools_listed: std::sync::atomic::AtomicBool,
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
    Connected(Lease),
    NeedsLogin {
        challenge: String,
    },
    Untrusted,
    Failed(String),
    /// Connected, but the verified login is not the account this project expects.
    WrongAccount(String),
}

impl Session {
    pub fn new(shared: Arc<Shared>, hints: ProjectHints) -> Arc<Self> {
        let session = Arc::new_cyclic(|this| Self {
            this: this.clone(),
            shared: shared.clone(),
            hints: Mutex::new(hints),
            pins: Mutex::new(BTreeMap::new()),
            state: tokio::sync::Mutex::new(SessionState::default()),
            peer: OnceLock::new(),
            tools_listed: std::sync::atomic::AtomicBool::new(false),
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

    async fn ensure_ready(&self) {
        let needs_first_refresh = self.state.lock().await.resolved.is_none();
        if needs_first_refresh {
            self.refresh().await;
        }
    }

    /// Re-resolves config and reconnects whatever changed. Returns true when the exposed tools may differ.
    async fn refresh(&self) -> bool {
        let resolved = self.resolve_config();
        if let Ok(config) = &resolved.outcome {
            trust::grant_files_written_here(&self.shared.paths, config).await;
            index::record(&self.shared.paths, config).await;
        }
        let verified = VerifiedAccounts::load(&self.shared.paths).await;
        let mut state = self.state.lock().await;
        let previous_servers = std::mem::take(&mut state.servers);
        let previous_shape = tool_shape(&previous_servers);

        let wanted = match &resolved.outcome {
            Ok(config) => config.servers.clone(),
            Err(error) => {
                tracing::warn!(%error, "config did not resolve");
                Vec::new()
            }
        };
        let working_dir = resolved.location.working_dir.clone();
        let mut reusable = previous_servers;
        let mut next_servers = Vec::with_capacity(wanted.len());
        let mut connecting = Vec::new();

        for server in wanted {
            // Trust first: a revoked grant must drop an already connected server too.
            if !self.is_trusted(&server, resolved.outcome.as_ref().ok()) {
                next_servers.push(ActiveServer {
                    server,
                    status: ServerStatus::Untrusted,
                });
            } else if let Some(index) = reusable
                .iter()
                .position(|active| active.can_be_reused_for(&server))
            {
                next_servers.push(reusable.swap_remove(index));
            } else {
                connecting.push(server);
            }
        }
        // Old leases go only after new ones exist, so an unchanged upstream isn't closed and reopened.
        let connected = futures::future::join_all(
            connecting
                .into_iter()
                .map(|server| self.connect(server, working_dir.clone())),
        )
        .await;
        next_servers.extend(connected);
        drop(reusable);
        turn_off_wrong_accounts(&mut next_servers, &verified);

        let changed = tool_shape(&next_servers) != previous_shape || state.resolved.is_none();
        state.servers = next_servers;
        state.resolved = Some(resolved);
        changed
    }

    async fn connect(&self, server: ResolvedServer, working_dir: Option<PathBuf>) -> ActiveServer {
        let status = match self
            .shared
            .pool
            .acquire(&server, working_dir.as_ref())
            .await
        {
            Ok(lease) => ServerStatus::Connected(lease),
            Err(ConnectError::NeedsLogin { challenge }) => ServerStatus::NeedsLogin { challenge },
            Err(ConnectError::Failed(error)) => {
                tracing::warn!(server = %server.display_name(), error = format!("{error:#}"), "upstream failed");
                ServerStatus::Failed(format!("{error:#}"))
            }
        };
        ActiveServer { server, status }
    }

    fn is_trusted(&self, server: &ResolvedServer, config: Option<&ResolvedConfig>) -> bool {
        let Some(config) = config else {
            return false;
        };
        self.shared.trust.lock().is_server_trusted(server, config)
    }

    /// A crashed stdio server or dropped remote connection is replaced on the next call instead of failing until a config edit.
    async fn reconnect_if_an_upstream_died(&self) {
        let any_closed = self.state.lock().await.servers.iter().any(|active| {
            matches!(&active.status, ServerStatus::Connected(lease) if lease.upstream.is_closed())
        });
        if any_closed {
            tracing::info!("an upstream connection closed, reconnecting");
            self.refresh_and_notify().await;
        }
    }

    pub async fn refresh_and_notify(&self) {
        let changed = self.refresh().await;
        if changed {
            self.notify_tools_changed().await;
        }
    }

    async fn notify_tools_changed(&self) {
        let listed = self.tools_listed.load(std::sync::atomic::Ordering::SeqCst);
        let Some(peer) = self.peer.get().filter(|_| listed) else {
            return;
        };
        if let Err(error) = peer.notify_tool_list_changed().await {
            tracing::debug!(%error, "could not send tools/list_changed");
        }
    }

    async fn list_all_tools(&self) -> Vec<Tool> {
        self.ensure_ready().await;
        let state = self.state.lock().await;
        let mut tools = vec![status_tool(), disable_tool()];
        if let Some(Resolved {
            outcome: Ok(config),
            ..
        }) = &state.resolved
        {
            tools.push(enable_tool(&servers_to_enable(config)));
        }

        for active in &state.servers {
            match &active.status {
                ServerStatus::Connected(lease) => match lease.upstream.tools().await {
                    Ok(upstream_tools) => {
                        tools.extend(exposed_tools(&active.server, upstream_tools))
                    }
                    Err(error) => {
                        tracing::warn!(server = %active.server.display_name(), error = format!("{error:#}"), "listing tools failed");
                    }
                },
                ServerStatus::NeedsLogin { .. } => tools.push(login_tool(&active.server)),
                ServerStatus::Untrusted
                | ServerStatus::Failed(_)
                | ServerStatus::WrongAccount(_) => {}
            }
        }
        tools
    }

    async fn call(&self, request: CallToolRequestParams) -> CallToolResult {
        self.ensure_ready().await;
        self.reconnect_if_an_upstream_died().await;
        if request.name == STATUS_TOOL {
            return text_result(self.status_report().await);
        }
        if request.name == ENABLE_TOOL {
            return self.enable(request.arguments).await;
        }
        if request.name == DISABLE_TOOL {
            return self.disable(request.arguments).await;
        }
        let Some((prefix, tool_name)) = request.name.split_once(TOOL_SEPARATOR) else {
            return error_result(format!("unknown tool `{}`", request.name));
        };

        let state = self.state.lock().await;
        let Some(active) = state
            .servers
            .iter()
            .find(|active| active.server.tool_prefix == prefix)
        else {
            return error_result(format!(
                "no server `{prefix}` here; call {STATUS_TOOL} to see what is configured"
            ));
        };
        let display_name = active.server.display_name();

        match &active.status {
            ServerStatus::NeedsLogin { challenge } if tool_name == "login" => {
                let server = active.server.clone();
                let challenge = challenge.clone();
                drop(state);
                self.start_login(server, &challenge).await
            }
            ServerStatus::NeedsLogin { .. } => error_result(format!(
                "{display_name} needs a login: call `{prefix}__login`, or ask the human to run `plugbay login {display_name}` (with `--token` if it takes an API token)"
            )),
            ServerStatus::Untrusted => error_result(format!(
                "{display_name} runs a local command from config that is not trusted yet; ask the human to run `plugbay trust` in the project directory"
            )),
            ServerStatus::Failed(error) => {
                error_result(format!("{display_name} is unavailable: {error}"))
            }
            ServerStatus::WrongAccount(message) => error_result(message.clone()),
            ServerStatus::Connected(_) if !active.server.tool_filter.allows(tool_name) => {
                error_result(format!(
                    "`{tool_name}` is filtered out for {display_name} by plugbay config"
                ))
            }
            ServerStatus::Connected(lease) => {
                let mut upstream_request = request.clone();
                upstream_request.name = tool_name.to_string().into();
                let upstream = lease.upstream.clone();
                drop(state);
                match upstream.call_tool(upstream_request).await {
                    Ok(result) => result,
                    Err(error) => error_result(format!("{display_name} failed: {error:#}")),
                }
            }
        }
    }

    async fn start_login(&self, server: ResolvedServer, challenge: &str) -> CallToolResult {
        let login_key = (server.identity.clone(), server.account.clone());
        if let Some(url) = self.shared.logins_in_progress.lock().get(&login_key) {
            return text_result(format!(
                "A login for {} is already waiting. Open this URL to finish it: {url}",
                server.display_name()
            ));
        }

        let flow = match oauth::start_login(&server, Some(challenge)).await {
            Ok(flow) => flow,
            Err(error) => {
                return error_result(format!(
                    "could not start login for {}: {error:#}",
                    server.display_name()
                ));
            }
        };
        let url = flow.authorization_url.clone();
        self.shared
            .logins_in_progress
            .lock()
            .insert(login_key.clone(), url.clone());

        tokio::spawn({
            let shared = self.shared.clone();
            let display_name = server.display_name();
            async move {
                match flow.finish().await {
                    Ok(()) => tracing::info!(server = %display_name, "login completed"),
                    Err(error) => {
                        tracing::warn!(server = %display_name, error = format!("{error:#}"), "login failed")
                    }
                }
                shared.logins_in_progress.lock().remove(&login_key);
                shared.reload_all().await;
            }
        });
        self.show_login_url(&server, &url);

        text_result(format!(
            "Opened the browser to log in to {name}. If nothing opened, visit:\n{url}\n\n\
             Once the login completes its tools appear automatically (clients that ignore tool list \
             changes, like Codex, pick them up in the next session).",
            name = server.display_name()
        ))
    }

    /// Asks the client to open the URL when it supports URL elicitation, else opens the browser ourselves.
    fn show_login_url(&self, server: &ResolvedServer, url: &str) {
        let peer = self
            .peer
            .get()
            .filter(|peer| {
                peer.supported_elicitation_modes()
                    .contains(&ElicitationMode::Url)
            })
            .cloned();
        let Some(peer) = peer.zip(url::Url::parse(url).ok()) else {
            if let Err(error) = open::that_detached(url) {
                tracing::warn!(%error, "could not open the browser");
            }
            return;
        };

        let (peer, parsed_url) = peer;
        let message = format!("Log in to {} for plugbay", server.display_name());
        let elicitation_id = format!("plugbay-login-{}", server.display_name());
        tokio::spawn(async move {
            if let Err(error) = peer.elicit_url(message, parsed_url, elicitation_id).await {
                tracing::debug!(%error, "URL elicitation failed");
            }
        });
    }

    /// Switches a catalog or registry server on for this session only, keeping the pin only if it works.
    async fn enable(&self, arguments: Option<rmcp::model::JsonObject>) -> CallToolResult {
        let arguments: EnableArguments = match parse_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(message) => return error_result(message),
        };
        let account_is_valid = arguments
            .account
            .as_deref()
            .is_none_or(config::is_valid_name);
        if !config::is_valid_name(&arguments.server) || !account_is_valid {
            return error_result(
                "server names and accounts use lowercase letters, digits and `-` only".to_string(),
            );
        }

        let pin = SessionPin::On {
            account: arguments.account.clone(),
        };
        let previous = self.pins.lock().insert(arguments.server.clone(), pin);
        self.refresh_and_notify().await;

        match self.describe_enabled(&arguments.server).await {
            Ok(message) => text_result(message),
            Err(message) => {
                self.restore_pin(&arguments.server, previous).await;
                error_result(message)
            }
        }
    }

    async fn disable(&self, arguments: Option<rmcp::model::JsonObject>) -> CallToolResult {
        let arguments: DisableArguments = match parse_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(message) => return error_result(message),
        };
        let is_on = self
            .state
            .lock()
            .await
            .servers
            .iter()
            .any(|active| active.server.name == arguments.server);
        if !is_on {
            return error_result(format!(
                "{} is not on in this session; call {STATUS_TOOL} to see what is",
                arguments.server
            ));
        }
        self.pins
            .lock()
            .insert(arguments.server.clone(), SessionPin::Off);
        self.refresh_and_notify().await;
        text_result(format!(
            "{} is off for the rest of this session (config files are unchanged).",
            arguments.server
        ))
    }

    async fn restore_pin(&self, server: &str, previous: Option<SessionPin>) {
        {
            let mut pins = self.pins.lock();
            match previous {
                Some(pin) => pins.insert(server.to_string(), pin),
                None => pins.remove(server),
            };
        }
        self.refresh_and_notify().await;
    }

    /// What enabling `name` led to, or why it can't stay on.
    async fn describe_enabled(&self, name: &str) -> Result<String, String> {
        let state = self.state.lock().await;
        let config = match state.resolved.as_ref().map(|resolved| &resolved.outcome) {
            Some(Ok(config)) => config,
            Some(Err(error)) => return Err(format!("the config does not resolve: {error}")),
            None => return Err("plugbay has not resolved any config yet".to_string()),
        };
        let active: Vec<&ActiveServer> = state
            .servers
            .iter()
            .filter(|active| active.server.name == name)
            .collect();
        if active.is_empty() {
            return Err(why_not_enabled(config, name));
        }

        let mut lines = Vec::new();
        for active in active {
            let display_name = active.server.display_name();
            let prefix = &active.server.tool_prefix;
            match &active.status {
                ServerStatus::Connected(_) => lines.push(format!(
                    "{display_name} is on for this session; its tools are {prefix}{TOOL_SEPARATOR}*. \
                     Nothing is saved: to keep it for this project the human can run `plugbay use {display_name}`."
                )),
                ServerStatus::NeedsLogin { .. } => lines.push(format!(
                    "{display_name} is on but needs a login: call {prefix}{TOOL_SEPARATOR}login, \
                     or ask the human to run `plugbay login {display_name}`."
                )),
                ServerStatus::Untrusted => {
                    return Err(format!(
                        "{display_name} runs a command from config that is not trusted yet; ask the human to run `plugbay trust`"
                    ));
                }
                ServerStatus::Failed(error) => {
                    return Err(format!("{display_name} could not start: {error}"));
                }
                ServerStatus::WrongAccount(message) => return Err(message.clone()),
            }
        }
        Ok(lines.join("\n"))
    }

    async fn status_report(&self) -> String {
        let state = self.state.lock().await;
        let Some(resolved) = &state.resolved else {
            return "plugbay has not resolved any config yet".to_string();
        };
        let verified = VerifiedAccounts::load(&self.shared.paths).await;
        let mut report = status_text(resolved, &state.servers, &verified);
        let pins = self.pins.lock();
        if !pins.is_empty() {
            let described: Vec<String> = pins
                .iter()
                .map(|(name, pin)| match pin {
                    SessionPin::On { .. } => format!("{name} on"),
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

    /// One in-agent prompt per folder for config that runs commands or reads secrets.
    async fn prompt_for_trust(&self, peer: &Peer<RoleServer>) -> bool {
        let Some(pending) = self.pending_trust().await else {
            return false;
        };
        if !self
            .shared
            .trust_prompted
            .lock()
            .insert(pending.config_dir.clone())
        {
            return false;
        }
        if !peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
        {
            tracing::info!(
                "client cannot show the trust prompt; instructions point to `plugbay trust`"
            );
            return false;
        }

        let message = format!(
            "plugbay: the config for {} runs local commands or reads secrets:\n{}\n\nAllow them?",
            pending.config_dir.display(),
            pending.commands.join("\n")
        );
        let Some(decision) = ask_trust_decision(peer, message).await else {
            return false;
        };

        let mut trust = self.shared.trust.lock();
        match decision {
            TrustDecision::Deny => return false,
            TrustDecision::AllowOnce => {
                for file in &pending.files {
                    trust.grant_for_session(file);
                }
            }
            TrustDecision::AllowAlways => {
                for file in &pending.files {
                    if let Err(error) = trust.grant_always(file) {
                        tracing::warn!(
                            error = format!("{error:#}"),
                            "could not save trust; allowing for this session"
                        );
                        trust.grant_for_session(file);
                    }
                }
            }
        }
        true
    }

    async fn pending_trust(&self) -> Option<PendingTrust> {
        let state = self.state.lock().await;
        let resolved = state.resolved.as_ref()?;
        let config = resolved.outcome.as_ref().ok()?;
        let config_dir = resolved.location.config_dir.clone()?;

        let files: Vec<ConfigFile> = self
            .shared
            .trust
            .lock()
            .untrusted_files(config)
            .into_iter()
            .cloned()
            .collect();
        if files.is_empty() {
            return None;
        }
        let commands = state
            .servers
            .iter()
            .filter(|active| matches!(active.status, ServerStatus::Untrusted))
            .map(|active| {
                format!(
                    "  {}: {}",
                    active.server.display_name(),
                    active.server.transport.target()
                )
            })
            .collect();
        Some(PendingTrust {
            config_dir,
            files,
            commands,
        })
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
                if session.config_changed_on_disk().await {
                    tracing::info!("config changed, re-resolving");
                    session.refresh_and_notify().await;
                }
            }
        });
    }

    async fn config_changed_on_disk(&self) -> bool {
        let state = self.state.lock().await;
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
        let root_changed_project = self.discover_project_from_roots(&peer).await;
        self.ensure_ready().await;
        if root_changed_project {
            self.refresh_and_notify().await;
        }
        if self.prompt_for_trust(&peer).await {
            self.refresh_and_notify().await;
        }
    }
}

impl ServerHandler for Session {
    fn get_info(&self) -> InitializeResult {
        let resolved = self.resolve_config();
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new("plugbay", crate::protocol::VERSION))
            .with_instructions(instructions(
                &resolved,
                &self.shared.trust.lock(),
                &VerifiedAccounts::read_or_empty(&self.shared.paths),
            ))
    }

    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

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
        self.negotiate_initialize(&request)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        let Some(session) = self.this.upgrade() else {
            return;
        };
        session.spawn_watcher();
        tokio::spawn(session.after_initialized());
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self.list_all_tools().await;
        self.tools_listed
            .store(true, std::sync::atomic::Ordering::SeqCst);
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

/// Config waiting for the human's trust decision.
struct PendingTrust {
    config_dir: PathBuf,
    files: Vec<ConfigFile>,
    /// One line per server that would run: `name:account: command args`.
    commands: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TrustDecision {
    AllowOnce,
    AllowAlways,
    Deny,
}

/// A single-enum form, so clients that map elicitations onto approval UIs (T3's Codex) show three buttons.
async fn ask_trust_decision(peer: &Peer<RoleServer>, message: String) -> Option<TrustDecision> {
    let choices = EnumSchema::builder(vec![
        "allow_once".to_string(),
        "allow_always".to_string(),
        "deny".to_string(),
    ])
    .enum_titles(vec![
        "Allow for this session".to_string(),
        "Always allow in this folder".to_string(),
        "Deny".to_string(),
    ])
    .map_err(|error| tracing::warn!(%error, "building the trust prompt"))
    .ok()?
    .build();
    let schema = ElicitationSchema::new(BTreeMap::from([(
        "decision".to_string(),
        PrimitiveSchemaDefinition::Enum(choices),
    )]))
    .with_required(vec!["decision".to_string()]);

    let request = ElicitRequestParams::FormElicitationParams {
        meta: None,
        message,
        requested_schema: schema,
    };
    let response = match peer.create_elicitation(request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::info!(%error, "trust prompt failed");
            return None;
        }
    };
    if response.action != ElicitationAction::Accept {
        return None;
    }
    let decision = response.content?.get("decision")?.clone();
    serde_json::from_value(decision)
        .map_err(|error| tracing::warn!(%error, "unexpected trust prompt answer"))
        .ok()
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
    fn can_be_reused_for(&self, server: &ResolvedServer) -> bool {
        let still_connected =
            matches!(&self.status, ServerStatus::Connected(lease) if !lease.upstream.is_closed());
        still_connected && self.server == *server
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
                ServerStatus::Untrusted
                | ServerStatus::Failed(_)
                | ServerStatus::WrongAccount(_) => "hidden",
            };
            (active.server.clone(), status)
        })
        .collect()
}

fn exposed_tools(server: &ResolvedServer, upstream_tools: Vec<Tool>) -> Vec<Tool> {
    let mut exposed = Vec::new();
    for mut tool in upstream_tools {
        if !server.tool_filter.allows(&tool.name) {
            continue;
        }
        let name = format!("{}{TOOL_SEPARATOR}{}", server.tool_prefix, tool.name);
        if name.len() > MAX_TOOL_NAME_LENGTH {
            tracing::warn!(tool = %name, "skipping tool: name longer than {MAX_TOOL_NAME_LENGTH} characters");
            continue;
        }
        tool.name = name.into();
        exposed.push(tool);
    }
    exposed
}

fn status_tool() -> Tool {
    Tool::new(
        STATUS_TOOL,
        "Show which MCP servers and accounts plugbay resolved for this project, and why any are missing.",
        Arc::new(empty_input_schema()),
    )
}

#[derive(Deserialize)]
struct EnableArguments {
    server: String,
    account: Option<String>,
}

#[derive(Deserialize)]
struct DisableArguments {
    server: String,
}

fn parse_arguments<T: serde::de::DeserializeOwned>(
    arguments: Option<rmcp::model::JsonObject>,
) -> Result<T, String> {
    let arguments = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

/// Servers plugbay knows how to reach that are off here: the catalog, ones switched off, the registry.
fn servers_to_enable(config: &ResolvedConfig) -> Vec<String> {
    let mut names: Vec<String> = config.catalog.clone();
    names.extend(config.disabled.iter().map(|disabled| disabled.name.clone()));
    names.extend(registry::all().iter().map(|entry| entry.name.to_string()));
    names.retain(|name| config.server(name).is_empty());
    names.sort();
    names.dedup();
    names
}

fn why_not_enabled(config: &ResolvedConfig, name: &str) -> String {
    if let Some(problem) = config.problems.iter().find(|problem| {
        problem.servers.iter().any(|server| server == name) && !problem.is_unknown_server()
    }) {
        return format!("{name} can't be enabled: {}", problem.message);
    }
    format!(
        "plugbay doesn't know a server named {name}. Known: {}. The human can define one with \
         `plugbay add {name} --url <url>` or `plugbay add {name} -- <command>`.",
        servers_to_enable(config).join(", ")
    )
}

fn enable_tool(available: &[String]) -> Tool {
    let description = format!(
        "Turn on an MCP server for this session only (nothing is saved); its tools appear right after. \
         Use only when the human asks for a server. Pass the account the human names; if a server has \
         several accounts and they did not say which, ask. Available: {}.",
        available.join(", ")
    );
    Tool::new(
        ENABLE_TOOL,
        description,
        Arc::new(object_schema(
            serde_json::json!({
                "server": { "type": "string", "description": "Server name, e.g. `linear`." },
                "account": { "type": "string", "description": "Account label, e.g. `initech`. Defaults to the project's account." }
            }),
            &["server"],
        )),
    )
}

fn disable_tool() -> Tool {
    Tool::new(
        DISABLE_TOOL,
        "Turn off an MCP server for the rest of this session (nothing is saved).",
        Arc::new(object_schema(
            serde_json::json!({
                "server": { "type": "string", "description": "Server name, e.g. `linear`." }
            }),
            &["server"],
        )),
    )
}

fn object_schema(properties: serde_json::Value, required: &[&str]) -> rmcp::model::JsonObject {
    let mut schema = empty_input_schema();
    schema.insert("properties".to_string(), properties);
    schema.insert("required".to_string(), serde_json::json!(required));
    schema
}

fn login_tool(server: &ResolvedServer) -> Tool {
    Tool::new(
        format!("{}{TOOL_SEPARATOR}login", server.tool_prefix),
        format!(
            "Log in to {} ({}) so its tools become available. Opens the browser for the human to approve.",
            server.display_name(),
            server.transport.target()
        ),
        Arc::new(empty_input_schema()),
    )
}

fn empty_input_schema() -> rmcp::model::JsonObject {
    let mut schema = rmcp::model::JsonObject::new();
    schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    schema
}

/// Server instructions: the agent (and the human reading along) see which account each server uses.
fn instructions(resolved: &Resolved, trust: &Trust, verified: &VerifiedAccounts) -> String {
    let config = match &resolved.outcome {
        Ok(config) => config,
        Err(error) => {
            return format!(
                "plugbay could not read the MCP config for this project: {error}. Ask the human to run `plugbay doctor`."
            );
        }
    };
    let project = match &resolved.location.config_dir {
        Some(dir) => dir.display().to_string(),
        None => "no project (global config only)".to_string(),
    };

    let mut lines = vec![format!(
        "plugbay routes MCP servers for {project}. Tools are named <server>__<tool>."
    )];
    if config.servers.is_empty() {
        lines.push("No servers are configured here.".to_string());
    } else {
        lines.push("Accounts in use (use the right one for this project):".to_string());
        lines.extend(
            config
                .servers
                .iter()
                .map(|server| account_line(server, verified)),
        );
    }
    let has_untrusted = config
        .servers
        .iter()
        .any(|server| !trust.is_server_trusted(server, config));
    if has_untrusted {
        lines.push(
            "Some servers run local commands from config that is not trusted yet, so they are off. \
             Ask the human to run `plugbay trust` in the project directory."
                .to_string(),
        );
    }
    if !config.problems.is_empty() {
        lines.push(format!("Config problems: {}", problem_summary(config)));
    }
    lines.push(format!(
        "If a server or tool seems missing, call {STATUS_TOOL}. \
         To turn on another server for this session when the human asks, call {ENABLE_TOOL}."
    ));
    lines.join("\n")
}

fn account_line(server: &ResolvedServer, verified: &VerifiedAccounts) -> String {
    let title = registry::lookup(&server.name).map_or(server.name.as_str(), |entry| entry.title);
    let details: Vec<String> = server
        .account_metadata
        .iter()
        .filter(|(key, _)| key.as_str() != identity::EXPECT_KEY)
        .map(|(key, value)| format!("{key} {value}"))
        .collect();
    let mut line = format!("- {title} → {}", server.account);
    if !details.is_empty() {
        line.push_str(&format!(" ({})", details.join(", ")));
    }
    if let Some(shown) = verified.shown(&server.identity, &server.account) {
        line.push_str(&format!(" (verified: {shown})"));
    }
    if server.tool_prefix != server.name {
        line.push_str(&format!(
            ", tools prefixed {}{TOOL_SEPARATOR}",
            server.tool_prefix
        ));
    }
    if let Some(failure) = identity::expectation_failure(server, verified) {
        line.push_str(&format!(". Off: {failure}"));
    }
    line
}

fn problem_summary(config: &ResolvedConfig) -> String {
    config
        .problems
        .iter()
        .map(|problem| problem.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

fn status_text(
    resolved: &Resolved,
    servers: &[ActiveServer],
    verified: &VerifiedAccounts,
) -> String {
    let mut lines = Vec::new();
    match &resolved.location.config_dir {
        Some(dir) => lines.push(format!("project: {}", dir.display())),
        None => lines.push("project: none (global config only)".to_string()),
    }
    if let Some(working_dir) = &resolved.location.working_dir
        && Some(working_dir) != resolved.location.config_dir.as_ref()
    {
        lines.push(format!("working dir: {}", working_dir.display()));
    }

    let config = match &resolved.outcome {
        Ok(config) => config,
        Err(error) => {
            lines.push(format!("config error: {error}"));
            return lines.join("\n");
        }
    };
    lines.push("config files:".to_string());
    lines.extend(
        config
            .files
            .iter()
            .map(|file| format!("  {}", file.path.display())),
    );

    lines.push("servers:".to_string());
    if servers.is_empty() {
        lines.push("  (none)".to_string());
    }
    for active in servers {
        let status = match &active.status {
            ServerStatus::Connected(_) => {
                match verified.shown(&active.server.identity, &active.server.account) {
                    Some(shown) => format!("connected (verified: {shown})"),
                    None => "connected".to_string(),
                }
            }
            ServerStatus::NeedsLogin { .. } => format!(
                "needs login: call {}__login or run `plugbay login {}` (add `--token` for an API token)",
                active.server.tool_prefix,
                active.server.display_name()
            ),
            ServerStatus::Untrusted => "off: config not trusted, run `plugbay trust`".to_string(),
            ServerStatus::Failed(error) => format!("failed: {error}"),
            ServerStatus::WrongAccount(message) => format!("off: {message}"),
        };
        lines.push(format!(
            "  {} ({}) — {status}",
            active.server.display_name(),
            active.server.transport.target()
        ));
    }
    for disabled in &config.disabled {
        lines.push(format!(
            "  {} — disabled by {}",
            disabled.name,
            disabled.disabled_by.display()
        ));
    }
    for problem in &config.problems {
        lines.push(format!("problem: {}", problem.message));
    }
    lines.join("\n")
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

fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn error_result(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR: &str = "https://mcp.linear.app/mcp";

    fn linear(account: &str, metadata: &[(&str, &str)]) -> ResolvedServer {
        let mut server = ResolvedServer::test_http("linear", account, LINEAR);
        for (key, value) in metadata {
            server
                .account_metadata
                .insert(key.to_string(), value.to_string());
        }
        server
    }

    #[test]
    fn account_line_shows_metadata_and_the_verified_login_but_not_expect() {
        let server = linear("initech", &[("team", "WP"), ("expect", "Initech")]);
        let verified = VerifiedAccounts::test_with(&[(LINEAR, "initech", "Initech")]);
        assert_eq!(
            account_line(&server, &verified),
            "- Linear → initech (team WP) (verified: Initech)"
        );
    }

    #[test]
    fn account_line_says_why_a_wrong_account_is_off() {
        let server = linear("personal", &[("expect", "mihaicrisan")]);
        let verified = VerifiedAccounts::test_with(&[(LINEAR, "personal", "Initech")]);
        let line = account_line(&server, &verified);
        assert!(
            line.starts_with("- Linear → personal (verified: Initech). Off: linear:personal is logged in as Initech"),
            "{line}"
        );
    }

    #[test]
    fn account_line_without_a_verified_login_is_unchanged() {
        let server = linear("personal", &[("expect", "mihaicrisan")]);
        assert_eq!(
            account_line(&server, &VerifiedAccounts::default()),
            "- Linear → personal"
        );
    }
}
