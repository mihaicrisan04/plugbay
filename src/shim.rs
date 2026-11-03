//! `plugbay connect`: the stdio MCP server each agent launches. It relays the agent's messages
//! to the daemon, and moves the session to a new daemon when that one exits (an upgrade, a
//! `plugbay stop`, a crash).

mod relay;
mod replay;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use tokio::net::UnixStream;

use crate::daemon;
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, ControlRequest, Hello};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_RESTART_INTERVAL: Duration = Duration::from_secs(1);

pub async fn run(paths: &Paths, project: Option<PathBuf>) -> anyhow::Result<()> {
    let hints = ProjectHints {
        explicit: project,
        claude_project_dir: std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
        root: None,
        cwd: std::env::current_dir().ok(),
    };

    let stream = match connect_as_session(paths, hints.clone(), Reconnects::Yes).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "no daemon available, serving in-process"
            );
            return daemon::serve_in_process(paths, hints).await;
        }
    };

    relay::run(paths, hints, stream).await;
    // stdin is read on a blocking thread that can't be cancelled; exit instead of waiting for it.
    std::process::exit(0)
}

/// A daemon connection that carries MCP for a session at `hints`, exactly like an agent's
/// (used by `plugbay tools` and `call`). Starts the daemon if none is running.
pub async fn open_session(paths: &Paths, hints: ProjectHints) -> anyhow::Result<UnixStream> {
    connect_as_session(paths, hints, Reconnects::No).await
}

#[derive(Clone, Copy)]
enum Reconnects {
    Yes,
    No,
}

async fn connect_as_session(
    paths: &Paths,
    hints: ProjectHints,
    reconnects: Reconnects,
) -> anyhow::Result<UnixStream> {
    retire_older_daemon(paths).await;
    let mut stream = connect_or_start_daemon(paths).await?;
    let hello = Hello::Session {
        version: protocol::version().to_string(),
        hints,
        reconnects: matches!(reconnects, Reconnects::Yes),
    };
    protocol::write_line(&mut stream, &hello)
        .await
        .context("greeting the daemon")?;
    Ok(stream)
}

/// What became of a running daemon older than this plugbay.
pub enum Handover {
    /// None runs, or it runs this version or a newer one.
    NotNeeded,
    /// It exited; the next connection starts a daemon of this version.
    Retired { old_version: String },
    /// It keeps running; the warning logged says why.
    Refused,
}

static WARNED_ABOUT_NEWER_DAEMON: AtomicBool = AtomicBool::new(false);

/// Asks a daemon older than this plugbay to make way. Only strictly newer retires older, and the
/// daemon checks that too, so a stale binary can never bounce a current daemon.
pub async fn retire_older_daemon(paths: &Paths) -> Handover {
    let Ok(status) = protocol::send_control(paths, ControlRequest::Status).await else {
        return Handover::NotNeeded;
    };
    let ours = protocol::version();
    if !protocol::is_newer(ours, &status.version) {
        if protocol::is_newer(&status.version, ours)
            && !WARNED_ABOUT_NEWER_DAEMON.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                daemon_version = status.version,
                version = ours,
                "the daemon is newer than this plugbay; using it as is"
            );
        }
        return Handover::NotNeeded;
    }

    let old_version = status.version;
    let refused = |reason: String| {
        tracing::warn!(
            daemon_version = old_version,
            reason,
            "the older daemon keeps running"
        );
        Handover::Refused
    };
    if !status.can_retire {
        return refused("it predates retiring; `plugbay stop` restarts it".to_string());
    }
    tracing::info!(
        daemon_version = old_version,
        version = ours,
        "asking the older daemon to retire"
    );
    match protocol::send_control(paths, ControlRequest::Retire).await {
        Ok(response) if response.message == protocol::RETIRING => {
            tracing::info!(daemon_version = old_version, "the older daemon retired");
            Handover::Retired { old_version }
        }
        Ok(response) => refused(response.message),
        Err(error) => refused(format!("{error:#}")),
    }
}

async fn connect_or_start_daemon(paths: &Paths) -> anyhow::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(paths.socket()).await {
        return Ok(stream);
    }

    start_daemon(paths)?;
    let started_at = tokio::time::Instant::now();
    let deadline = started_at + DAEMON_START_TIMEOUT;
    let mut next_start = started_at + DAEMON_RESTART_INTERVAL;
    let mut delay = Duration::from_millis(20);
    loop {
        match UnixStream::connect(paths.socket()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(error).context("the daemon did not come up");
            }
            Err(_) => {
                // The one we started gives way while an exiting daemon still holds the lock.
                if tokio::time::Instant::now() >= next_start {
                    start_daemon(paths)?;
                    next_start += DAEMON_RESTART_INTERVAL;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(250));
            }
        }
    }
}

/// Starts `plugbay daemon` detached in its own process group, so it outlives the agent that spawned it.
fn start_daemon(paths: &Paths) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    paths.ensure_state_dir()?;
    let executable = daemon_executable().context(
        "this plugbay binary is gone (upgraded?) and no `plugbay` is on PATH to start the daemon",
    )?;
    tracing::info!(executable = %executable.display(), "starting the plugbay daemon");
    std::process::Command::new(executable)
        .arg("daemon")
        // The override stands for this process's version, not for the binary's.
        .env_remove(protocol::TEST_VERSION_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawning the daemon")?;
    Ok(())
}

/// The binary to run as the daemon. `current_exe` is the path this process was started from
/// (on macOS a link like `/opt/homebrew/bin/plugbay` stays a link, so it already runs an upgrade);
/// when an upgrade deleted that file (a Cellar path, or Linux, where it is always the resolved
/// file), the `plugbay` on PATH is the package manager's link to the new version.
fn daemon_executable() -> Option<PathBuf> {
    if let Ok(executable) = std::env::current_exe()
        && executable.is_file()
    {
        return Some(executable);
    }
    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path)
        .map(|directory| directory.join("plugbay"))
        .find(|candidate| candidate.is_file())
}
