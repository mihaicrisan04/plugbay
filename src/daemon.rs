//! The long-running process every shim talks to over a unix socket.

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use parking_lot::Mutex;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::config::ServerIdentity;
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, ControlRequest, ControlResponse, Hello, write_line};
use crate::session::{Session, Shared};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HELLO_BYTES: u64 = 64 * 1024;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// How long an upstream stays connected after its last session leaves, so a quick restart reuses it.
const UPSTREAM_GRACE: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(paths: &Paths, idle_timeout: Duration) -> anyhow::Result<()> {
    paths.ensure_state_dir()?;
    let Some(lock) = acquire_daemon_lock(paths)? else {
        tracing::info!("another daemon is already running");
        return Ok(());
    };

    let listener = bind_socket(paths)?;
    tracing::info!(socket = %paths.socket().display(), version = protocol::VERSION, "daemon listening");

    let shared = Shared::new(paths.clone());
    let activity = Arc::new(Activity::new());
    let stop = Arc::new(Notify::new());
    spawn_upstream_reaper(&shared);

    let mut idle_check = tokio::time::interval(IDLE_CHECK_INTERVAL);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(handle_connection(stream, shared.clone(), activity.clone(), stop.clone()));
                }
                Err(error) => tracing::warn!(%error, "accept failed"),
            },
            _ = idle_check.tick() => {
                if activity.idle_for() >= Some(idle_timeout) {
                    tracing::info!("no sessions for {}s, exiting", idle_timeout.as_secs());
                    break;
                }
            }
            _ = stop.notified() => {
                tracing::info!("stop requested");
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
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, shared.pool.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            "upstreams did not close within {}s, exiting anyway",
            SHUTDOWN_TIMEOUT.as_secs()
        );
    }
    Ok(())
}

/// Serves one session over stdio inside the shim, for when no daemon can be started.
pub async fn serve_in_process(paths: &Paths, hints: ProjectHints) -> anyhow::Result<()> {
    let shared = Shared::new(paths.clone());
    spawn_upstream_reaper(&shared);
    serve_session(shared, hints, rmcp::transport::stdio()).await
}

async fn handle_connection(
    stream: UnixStream,
    shared: Arc<Shared>,
    activity: Arc<Activity>,
    stop: Arc<Notify>,
) {
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
        Hello::Session { version, hints } => {
            warn_on_version_mismatch(&version);
            activity.session_started();
            if let Err(error) = serve_session(shared, hints, (reader, write_half)).await {
                tracing::warn!(error = format!("{error:#}"), "session ended with an error");
            }
            activity.session_ended();
        }
        Hello::Control { version, request } => {
            warn_on_version_mismatch(&version);
            let response = handle_control(request, &shared, &activity, &stop).await;
            if let Err(error) = write_line(&mut write_half, &response).await {
                tracing::debug!(error = format!("{error:#}"), "control client left early");
            }
        }
    }
}

async fn serve_session<T, E, A>(
    shared: Arc<Shared>,
    hints: ProjectHints,
    transport: T,
) -> anyhow::Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    tracing::info!(?hints, "session connected");
    let session = Session::new(shared, hints);
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
    shared: &Arc<Shared>,
    activity: &Activity,
    stop: &Notify,
) -> ControlResponse {
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
            crate::vault::forget_cached();
            let closed = shared
                .pool
                .close_account(&ServerIdentity(identity), &account)
                .await;
            shared.reload_all().await;
            format!("closed {closed} connection(s) and reloaded")
        }
        ControlRequest::Stop => {
            stop.notify_one();
            "stopping".to_string()
        }
    };
    ControlResponse {
        version: protocol::VERSION.to_string(),
        sessions: activity.active_sessions(),
        message,
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
    if client_version != protocol::VERSION {
        tracing::warn!(
            client_version,
            daemon_version = protocol::VERSION,
            "client and daemon versions differ; run `plugbay stop` to restart the daemon"
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

/// Counts live sessions and remembers when the daemon last became idle.
struct Activity {
    state: Mutex<ActivityState>,
}

struct ActivityState {
    active_sessions: usize,
    idle_since: Option<Instant>,
}

impl Activity {
    fn new() -> Self {
        Self {
            state: Mutex::new(ActivityState {
                active_sessions: 0,
                idle_since: Some(Instant::now()),
            }),
        }
    }

    fn session_started(&self) {
        let mut state = self.state.lock();
        state.active_sessions += 1;
        state.idle_since = None;
    }

    fn session_ended(&self) {
        let mut state = self.state.lock();
        state.active_sessions = state.active_sessions.saturating_sub(1);
        if state.active_sessions == 0 {
            state.idle_since = Some(Instant::now());
        }
    }

    fn active_sessions(&self) -> usize {
        self.state.lock().active_sessions
    }

    fn idle_for(&self) -> Option<Duration> {
        self.state.lock().idle_since.map(|since| since.elapsed())
    }
}
