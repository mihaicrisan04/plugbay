//! The long-running process every shim talks to over a unix socket.

use std::collections::BTreeMap;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use parking_lot::Mutex;
use rmcp::ServiceExt;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, SimplexStream,
    WriteHalf,
};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::TaskTracker;

use crate::config::{ServerIdentity, SessionPin};
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, ControlRequest, ControlResponse, Hello, Skew, write_line};
use crate::session::{Session, Shared};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HELLO_BYTES: u64 = 64 * 1024;
/// How long a reconnect request waits before answering; the client gives up after 5 s.
const RECONNECT_ANSWER_WAIT: Duration = Duration::from_secs(2);
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// How long an upstream stays connected after its last session leaves, so a quick restart reuses it.
const UPSTREAM_GRACE: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `stop` and `retire` wait for the socket to go before answering.
const SOCKET_REMOVAL_WAIT: Duration = Duration::from_secs(2);
/// How long an exiting daemon waits for control answers still being written.
const ANSWER_WAIT: Duration = Duration::from_secs(1);
/// Client bytes in flight between the hangup watch and rmcp.
const RELAY_BUFFER_BYTES: usize = 64 * 1024;
/// Client bytes rmcp hasn't read yet; a client further ahead than one maximum-size message (as for
/// stdio upstreams) is flooding the session.
const MAX_UNREAD_CLIENT_BYTES: usize = 16 * 1024 * 1024;

pub async fn run(paths: &Paths, idle_timeout: Duration) -> anyhow::Result<()> {
    paths.ensure_state_dir()?;
    let Some(lock) = acquire_daemon_lock(paths)? else {
        tracing::info!("another daemon is already running");
        return Ok(());
    };

    let listener = bind_socket(paths)?;
    tracing::info!(socket = %paths.socket().display(), version = protocol::version(), "daemon listening");

    let (stop, mut stop_requests) = Stop::new();
    let daemon = Daemon {
        shared: Shared::new(paths.clone()),
        activity: Arc::new(Activity::new()),
        stop,
        control_answers: TaskTracker::new(),
    };
    spawn_upstream_reaper(&daemon.shared);

    let mut waiting_for_socket_removal = Vec::new();
    let mut idle_check = tokio::time::interval(IDLE_CHECK_INTERVAL);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let greeting = daemon.activity.greeting();
                    tokio::spawn(handle_connection(stream, daemon.clone(), greeting));
                }
                Err(error) => tracing::warn!(%error, "accept failed"),
            },
            _ = idle_check.tick() => {
                if daemon.activity.idle_for() >= Some(idle_timeout) {
                    tracing::info!("no sessions for {}s, exiting", idle_timeout.as_secs());
                    break;
                }
            }
            Some(asker) = stop_requests.recv() => {
                waiting_for_socket_removal.push(asker);
                break;
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }

    // The socket goes while we still hold the lock, so it can never be a successor's socket.
    // Releasing the lock before closing upstreams lets the next shim start a fresh daemon now.
    drop(listener);
    if let Err(error) = std::fs::remove_file(paths.socket()) {
        tracing::debug!(%error, "could not remove the socket");
    }
    drop(lock);
    stop_requests.close();
    while let Ok(asker) = stop_requests.try_recv() {
        waiting_for_socket_removal.push(asker);
    }
    for asker in waiting_for_socket_removal {
        if asker.send(()).is_err() {
            tracing::debug!("a stop request stopped waiting before the socket went");
        }
    }
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, daemon.shared.pool.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            "upstreams did not close within {}s, exiting anyway",
            SHUTDOWN_TIMEOUT.as_secs()
        );
    }
    daemon.control_answers.close();
    if tokio::time::timeout(ANSWER_WAIT, daemon.control_answers.wait())
        .await
        .is_err()
    {
        tracing::debug!("exiting before every control answer was written");
    }
    Ok(())
}

/// What every connection handler shares.
#[derive(Clone)]
struct Daemon {
    shared: Arc<Shared>,
    activity: Arc<Activity>,
    stop: Stop,
    /// Control answers still being written; the daemon waits for them before it exits.
    control_answers: TaskTracker,
}

/// Serves one session over stdio inside the shim, for when no daemon can be started.
pub async fn serve_in_process(paths: &Paths, hints: ProjectHints) -> anyhow::Result<()> {
    let shared = Shared::new(paths.clone());
    spawn_upstream_reaper(&shared);
    let pins = BTreeMap::new();
    serve_session(shared, hints, pins, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn handle_connection(stream: UnixStream, daemon: Daemon, greeting: Greeting) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_hello(&mut reader)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(error)) => {
            tracing::warn!(error = format!("{error:#}"), "rejected connection");
            return;
        }
        Err(_) => {
            tracing::warn!("connection sent no hello in time");
            return;
        }
    };

    match hello {
        Hello::Session {
            version,
            hints,
            reconnects,
            pins,
        } => {
            warn_on_version_mismatch(&version);
            let admitted = daemon.activity.admit_session(reconnects);
            drop(greeting);
            if !admitted {
                tracing::info!("refused a session: this daemon is exiting");
                return;
            }
            let served = serve_session(daemon.shared, hints, pins, reader, write_half).await;
            if let Err(error) = served {
                tracing::warn!(error = format!("{error:#}"), "session ended with an error");
            }
            daemon.activity.session_ended(reconnects);
        }
        Hello::Control { version, request } => {
            warn_on_version_mismatch(&version);
            drop(greeting);
            let answering = daemon.control_answers.clone();
            answering.spawn(async move {
                let response = handle_control(request, &version, &daemon).await;
                if let Err(error) = write_line(&mut write_half, &response).await {
                    tracing::debug!(error = format!("{error:#}"), "control client left early");
                }
            });
        }
    }
}

async fn serve_session<R, W>(
    shared: Arc<Shared>,
    hints: ProjectHints,
    pins: BTreeMap<String, SessionPin>,
    from_client: R,
    to_client: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tracing::info!(?hints, "session connected");
    let session = Session::new(shared, hints);
    session.restore_pins(pins);
    let (to_rmcp, relay) = tokio::io::simplex(RELAY_BUFFER_BYTES);
    let hangup_watch = tokio::spawn(relay_until_hangup(from_client, relay, session.clone()));
    let served = serve_until_closed(session.clone(), (to_rmcp, to_client)).await;
    session.end();
    hangup_watch.abort();
    served
}

/// rmcp reads nothing while our `initialize` handler waits for upstreams, so a client leaving
/// then would go unnoticed and its pending connections would still start. Relaying its bytes
/// ourselves sees the hangup at once and ends the session; rmcp then reads EOF too. Reading
/// never waits for rmcp: what it hasn't taken yet queues here, up to one message's worth.
async fn relay_until_hangup<R: AsyncRead + Unpin>(
    from_client: R,
    to_rmcp: WriteHalf<SimplexStream>,
    session: Arc<Session>,
) {
    let (chunks, queued) = tokio::sync::mpsc::unbounded_channel();
    let unread_bytes = AtomicUsize::new(0);
    tokio::join!(
        async {
            read_client(from_client, chunks, &unread_bytes).await;
            session.end();
        },
        forward_to_rmcp(queued, to_rmcp, &unread_bytes),
    );
}

async fn read_client<R: AsyncRead + Unpin>(
    mut from_client: R,
    chunks: UnboundedSender<Vec<u8>>,
    unread_bytes: &AtomicUsize,
) {
    let mut buffer = vec![0; RELAY_BUFFER_BYTES];
    loop {
        let read = match from_client.read(&mut buffer).await {
            Ok(0) => break tracing::info!("client hung up"),
            Ok(read) => read,
            Err(error) => break tracing::debug!(%error, "client connection failed"),
        };
        let unread = unread_bytes.fetch_add(read, Ordering::Relaxed) + read;
        if unread > MAX_UNREAD_CLIENT_BYTES {
            break tracing::warn!("client sent more than the session could take; ending it");
        }
        if chunks.send(buffer[..read].to_vec()).is_err() {
            break;
        }
    }
}

async fn forward_to_rmcp(
    mut queued: UnboundedReceiver<Vec<u8>>,
    mut to_rmcp: WriteHalf<SimplexStream>,
    unread_bytes: &AtomicUsize,
) {
    while let Some(chunk) = queued.recv().await {
        if let Err(error) = to_rmcp.write_all(&chunk).await {
            tracing::debug!(%error, "rmcp stopped reading the client");
            return;
        }
        unread_bytes.fetch_sub(chunk.len(), Ordering::Relaxed);
    }
    if let Err(error) = to_rmcp.shutdown().await {
        tracing::debug!(%error, "could not pass the hangup on");
    }
}

async fn serve_until_closed<T, E, A>(session: Arc<Session>, transport: T) -> anyhow::Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let service = session
        .serve(transport)
        .await
        .context("MCP handshake with the client failed")?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "session closed");
    Ok(())
}

async fn handle_control(
    request: ControlRequest,
    client_version: &str,
    daemon: &Daemon,
) -> ControlResponse {
    let Daemon {
        shared,
        activity,
        stop,
        ..
    } = daemon;
    let mut retiring = false;
    let message = match request {
        ControlRequest::Status => format!(
            "running, {} upstream connection(s)",
            shared.pool.connection_count()
        ),
        ControlRequest::ForgetSessionTrust => {
            shared.forget_session_trust().await;
            "forgot this session's trust grants".to_string()
        }
        ControlRequest::Reload => {
            crate::vault::forget_cached();
            shared.reload_all().await;
            "reloaded".to_string()
        }
        ControlRequest::CredentialsChanged { identity, account } => {
            let only = ServerAccount {
                identity: ServerIdentity(identity),
                account,
            };
            reconnect(shared, Some(only)).await
        }
        ControlRequest::ReconnectAll => reconnect(shared, None).await,
        ControlRequest::Stop => {
            tracing::info!("stop requested");
            activity.stop_admitting();
            stop.request_and_wait().await;
            "stopping".to_string()
        }
        ControlRequest::Retire => match retire(client_version, activity, stop).await {
            Ok(()) => {
                retiring = true;
                "retiring".to_string()
            }
            Err(refusal) => refusal,
        },
    };
    ControlResponse {
        version: protocol::version().to_string(),
        sessions: activity.active_sessions(),
        message,
        pid: Some(std::process::id()),
        can_retire: true,
        retiring,
    }
}

/// Exits so a newer plugbay's daemon takes over, once no client would be left without one.
/// Err is the refusal.
async fn retire(client_version: &str, activity: &Activity, stop: &Stop) -> Result<(), String> {
    if protocol::skew(client_version) != Skew::PeerNewer {
        tracing::warn!(
            client_version,
            "refused to retire for a plugbay that isn't newer"
        );
        return Err(format!(
            "not retiring: {client_version} is not newer than {}",
            protocol::version()
        ));
    }
    if let Err(stuck) = activity.stop_admitting_unless_stuck() {
        tracing::info!(
            client_version,
            stuck,
            "not retiring: some connections can't reconnect"
        );
        return Err(format!(
            "not retiring: {stuck} connection(s) from an older plugbay or the CLI, or still greeting, can't reconnect"
        ));
    }
    tracing::info!(client_version, "retiring for a newer plugbay");
    stop.request_and_wait().await;
    Ok(())
}

struct ServerAccount {
    identity: ServerIdentity,
    account: String,
}

/// Closes the pooled connections of `only` (or all of them) and reloads every session. Runs as a
/// task of its own: closing a connection that is still being made waits until it is up, and the
/// answer must not wait that long.
async fn reconnect(shared: &Arc<Shared>, only: Option<ServerAccount>) -> String {
    let task = tokio::spawn({
        let shared = shared.clone();
        async move {
            crate::vault::forget_cached();
            let closed = match &only {
                Some(ServerAccount { identity, account }) => {
                    shared.pool.close_account(identity, account).await
                }
                None => shared.pool.close_all().await,
            };
            shared
                .reload_after_retiring(|key| {
                    only.as_ref().is_none_or(|only| {
                        key.identity == only.identity && key.account == only.account
                    })
                })
                .await;
            tracing::info!(closed, "closed upstream connections and reloaded");
            closed
        }
    });
    match tokio::time::timeout(RECONNECT_ANSWER_WAIT, task).await {
        Ok(Ok(closed)) => format!("closed {closed} connection(s) and reloaded"),
        Ok(Err(error)) => {
            tracing::error!(%error, "reconnecting failed");
            "reconnecting failed; `plugbay logs` shows why".to_string()
        }
        Err(_) => "reconnecting; servers still connecting are closed and reloaded once they are up"
            .to_string(),
    }
}

async fn read_hello(reader: &mut BufReader<OwnedReadHalf>) -> anyhow::Result<Hello> {
    let mut line = String::new();
    let mut limited = reader.take(MAX_HELLO_BYTES);
    limited
        .read_line(&mut line)
        .await
        .context("reading hello")?;
    serde_json::from_str(&line).context("parsing hello")
}

fn warn_on_version_mismatch(client_version: &str) {
    if protocol::skew(client_version) != Skew::Same {
        tracing::warn!(
            client_version,
            daemon_version = protocol::version(),
            "client and daemon versions differ"
        );
    }
}

fn spawn_upstream_reaper(shared: &Arc<Shared>) {
    let pool = shared.pool.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPSTREAM_GRACE / 2);
        loop {
            ticker.tick().await;
            pool.close_idle(UPSTREAM_GRACE).await;
        }
    });
}

/// Held for the daemon's lifetime so two shims racing to start one never get two daemons.
fn acquire_daemon_lock(paths: &Paths) -> anyhow::Result<Option<File>> {
    let lock_path = paths.state_dir.join("daemon.lock");
    let lock_file =
        File::create(&lock_path).with_context(|| format!("creating {}", lock_path.display()))?;
    match lock_file.try_lock() {
        Ok(()) => Ok(Some(lock_file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking {}", lock_path.display()))
        }
    }
}

fn bind_socket(paths: &Paths) -> anyhow::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    let socket = paths.socket();
    // We hold the lock, so any existing socket is left over from a daemon that died.
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Asks the accept loop to end, and lets the asker wait until the socket is gone, so a client
/// that hears the answer can never reach this daemon again.
#[derive(Clone)]
struct Stop {
    requests: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl Stop {
    fn new() -> (Self, mpsc::UnboundedReceiver<oneshot::Sender<()>>) {
        let (requests, received) = mpsc::unbounded_channel();
        (Self { requests }, received)
    }

    async fn request_and_wait(&self) {
        let (removed, socket_gone) = oneshot::channel();
        if self.requests.send(removed).is_err() {
            tracing::debug!("the daemon is already exiting");
            return;
        }
        // A dropped sender means the daemon got past removing the socket too.
        if tokio::time::timeout(SOCKET_REMOVAL_WAIT, socket_gone)
            .await
            .is_err()
        {
            tracing::warn!("the socket was not removed in time");
        }
    }
}

/// Counts live sessions and remembers when the daemon last became idle. Admitting a session and
/// deciding to retire take the same lock, so no session slips onto a daemon that is leaving.
struct Activity {
    state: Mutex<ActivityState>,
}

struct ActivityState {
    active_sessions: usize,
    /// Sessions whose client can't find a new daemon on its own.
    stuck_sessions: usize,
    /// Connections that haven't sent their hello yet (at most 5 s each); they may be stuck sessions.
    greeting: usize,
    /// False once the daemon decided to exit: later session hellos are refused.
    admitting: bool,
    idle_since: Option<Instant>,
}

/// A connection that hasn't said hello yet, counted until it is dropped.
struct Greeting(Arc<Activity>);

impl Drop for Greeting {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.greeting = state.greeting.saturating_sub(1);
    }
}

impl Activity {
    fn new() -> Self {
        Self {
            state: Mutex::new(ActivityState {
                active_sessions: 0,
                stuck_sessions: 0,
                greeting: 0,
                admitting: true,
                idle_since: Some(Instant::now()),
            }),
        }
    }

    fn greeting(self: &Arc<Self>) -> Greeting {
        self.state.lock().greeting += 1;
        Greeting(self.clone())
    }

    /// False when the daemon is exiting; the client then finds the next one.
    fn admit_session(&self, reconnects: bool) -> bool {
        let mut state = self.state.lock();
        if !state.admitting {
            return false;
        }
        state.active_sessions += 1;
        if !reconnects {
            state.stuck_sessions += 1;
        }
        state.idle_since = None;
        true
    }

    fn session_ended(&self, reconnects: bool) {
        let mut state = self.state.lock();
        state.active_sessions = state.active_sessions.saturating_sub(1);
        if !reconnects {
            state.stuck_sessions = state.stuck_sessions.saturating_sub(1);
        }
        if state.active_sessions == 0 {
            state.idle_since = Some(Instant::now());
        }
    }

    fn stop_admitting(&self) {
        self.state.lock().admitting = false;
    }

    /// Stops admitting sessions unless a connection might be one that can't reconnect; Err
    /// counts those.
    fn stop_admitting_unless_stuck(&self) -> Result<(), usize> {
        let mut state = self.state.lock();
        let stuck = state.stuck_sessions + state.greeting;
        if stuck > 0 {
            return Err(stuck);
        }
        state.admitting = false;
        Ok(())
    }

    fn active_sessions(&self) -> usize {
        self.state.lock().active_sessions
    }

    fn idle_for(&self) -> Option<Duration> {
        self.state.lock().idle_since.map(|since| since.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_still_greeting_keeps_the_daemon_from_retiring() {
        let activity = Arc::new(Activity::new());
        let greeting = activity.greeting();
        assert_eq!(activity.stop_admitting_unless_stuck(), Err(1));
        drop(greeting);
        assert_eq!(activity.stop_admitting_unless_stuck(), Ok(()));
    }

    #[test]
    fn a_retiring_daemon_admits_no_session() {
        let activity = Arc::new(Activity::new());
        assert!(activity.admit_session(true));
        assert_eq!(activity.stop_admitting_unless_stuck(), Ok(()));
        assert!(
            !activity.admit_session(true),
            "admitted after deciding to retire"
        );
        assert_eq!(activity.active_sessions(), 1);
    }

    #[test]
    fn sessions_that_cannot_reconnect_keep_the_daemon_from_retiring() {
        let activity = Arc::new(Activity::new());
        assert!(activity.admit_session(false));
        assert_eq!(activity.stop_admitting_unless_stuck(), Err(1));
        activity.session_ended(false);
        assert_eq!(activity.stop_admitting_unless_stuck(), Ok(()));
    }
}
