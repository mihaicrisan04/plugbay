//! `plugbay connect`: the stdio MCP server each agent launches. It relays the agent's messages
//! to the daemon, and moves the session to a new daemon when that one exits (an upgrade, a
//! `plugbay stop`, a crash).

mod conversation;
mod relay;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use tokio::net::UnixStream;

use crate::config::SessionPin;
use crate::daemon;
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, ControlRequest, Hello, Skew};

// Timing. Starting a daemon waits up to DAEMON_START_TIMEOUT for its socket, polling from
// FIRST_CONNECT_POLL up to MAX_CONNECT_POLL and starting another every DAEMON_RESTART_INTERVAL
// (one started while an exiting daemon holds the lock gives way). A move retries that from
// FIRST_RETRY_DELAY, doubling to MAX_RETRY_DELAY, each attempt replaying `initialize` within
// HANDSHAKE_TIMEOUT, all of it within RECONNECT_DEADLINE; then the shim gives up.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_RESTART_INTERVAL: Duration = Duration::from_secs(1);
const FIRST_CONNECT_POLL: Duration = Duration::from_millis(20);
const MAX_CONNECT_POLL: Duration = Duration::from_millis(250);
const RECONNECT_DEADLINE: Duration = Duration::from_secs(30);
/// The agent hanging up is seen before the first attempt, so a client that just left starts no daemon.
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
/// The daemon answers `initialize` only once upstreams are ready, which takes up to 8 s.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn run(paths: &Paths, project: Option<PathBuf>) -> anyhow::Result<()> {
    remember_stable_link();
    let hints = ProjectHints {
        explicit: project,
        claude_project_dir: std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
        root: None,
        cwd: std::env::current_dir().ok(),
    };

    let first = Session {
        hints: hints.clone(),
        reconnects: Reconnects::Yes,
        pins: BTreeMap::new(),
    };
    let stream = match connect_as_session(paths, first).await {
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
    remember_stable_link();
    let session = Session {
        hints,
        reconnects: Reconnects::No,
        pins: BTreeMap::new(),
    };
    connect_as_session(paths, session).await
}

#[derive(Clone, Copy)]
enum Reconnects {
    Yes,
    No,
}

/// What the `Session` hello says.
struct Session {
    hints: ProjectHints,
    reconnects: Reconnects,
    pins: BTreeMap<String, SessionPin>,
}

async fn connect_as_session(paths: &Paths, session: Session) -> anyhow::Result<UnixStream> {
    retire_older_daemon(paths).await;
    let mut stream = connect_or_start_daemon(paths, &session.hints).await?;
    let hello = Hello::Session {
        version: protocol::version().to_string(),
        hints: session.hints,
        reconnects: matches!(session.reconnects, Reconnects::Yes),
        pins: session.pins,
    };
    protocol::write_line(&mut stream, &hello)
        .await
        .context("greeting the daemon")?;
    Ok(stream)
}

static WARNED_ABOUT_NEWER_DAEMON: AtomicBool = AtomicBool::new(false);

/// Asks a daemon older than this plugbay to make way, and returns its version when it did (the
/// next connection starts a daemon of this version). Only strictly newer retires older, and the
/// daemon checks that too, so a stale binary can never bounce a current daemon.
pub async fn retire_older_daemon(paths: &Paths) -> Option<String> {
    let status = protocol::send_control(paths, ControlRequest::Status)
        .await
        .ok()?;
    match protocol::skew(&status.version) {
        Skew::PeerOlder => {}
        Skew::PeerNewer => {
            if !WARNED_ABOUT_NEWER_DAEMON.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    daemon_version = status.version,
                    version = protocol::version(),
                    "the daemon is newer than this plugbay; using it as is"
                );
            }
            return None;
        }
        Skew::Same | Skew::Unknown => return None,
    }

    let old_version = status.version;
    if !status.can_retire {
        tracing::warn!(
            daemon_version = old_version,
            "the older daemon keeps running: it predates retiring; `plugbay stop` restarts it"
        );
        return None;
    }
    tracing::info!(
        daemon_version = old_version,
        version = protocol::version(),
        "asking the older daemon to retire"
    );
    match protocol::send_control(paths, ControlRequest::Retire).await {
        Ok(response) if response.retiring => {
            tracing::info!(daemon_version = old_version, "the older daemon retired");
            Some(old_version)
        }
        Ok(response) => {
            tracing::warn!(
                daemon_version = old_version,
                reason = response.message,
                "the older daemon keeps running"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                daemon_version = old_version,
                error = format!("{error:#}"),
                "the older daemon keeps running"
            );
            None
        }
    }
}

async fn connect_or_start_daemon(
    paths: &Paths,
    hints: &ProjectHints,
) -> anyhow::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(paths.socket()).await {
        return Ok(stream);
    }

    let executable = daemon_executable(hints).await.context(
        "this plugbay binary is gone (upgraded?) and no `plugbay` as new as it is on PATH to start the daemon",
    )?;
    start_daemon(paths, &executable)?;
    let started_at = tokio::time::Instant::now();
    let deadline = started_at + DAEMON_START_TIMEOUT;
    let mut next_start = started_at + DAEMON_RESTART_INTERVAL;
    let mut delay = FIRST_CONNECT_POLL;
    loop {
        match UnixStream::connect(paths.socket()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(error).context("the daemon did not come up");
            }
            Err(_) => {
                if tokio::time::Instant::now() >= next_start {
                    start_daemon(paths, &executable)?;
                    next_start += DAEMON_RESTART_INTERVAL;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_CONNECT_POLL);
            }
        }
    }
}

/// Starts `plugbay daemon` detached in its own process group, so it outlives the agent that
/// spawned it, and reaps it if it exits while this shim still runs.
fn start_daemon(paths: &Paths, executable: &Path) -> anyhow::Result<()> {
    paths.ensure_state_dir()?;
    tracing::info!(executable = %executable.display(), "starting the plugbay daemon");
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("daemon")
        // The override stands for this process's version, not for the binary's.
        .env_remove(protocol::TEST_VERSION_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    spawn_and_reap(command).context("spawning the daemon")?;
    Ok(())
}

/// Spawns `command` and waits for it on a task of its own, so it never lingers as a zombie.
fn spawn_and_reap(mut command: tokio::process::Command) -> std::io::Result<u32> {
    let mut child = command.spawn()?;
    let pid = child.id().unwrap_or_default();
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => tracing::debug!(pid, %status, "a daemon this shim started exited"),
            Err(error) => {
                tracing::debug!(pid, %error, "could not wait for a daemon this shim started")
            }
        }
    });
    Ok(pid)
}

static STABLE_LINK: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Finds, while this binary still exists, the `plugbay` on PATH that links to it (what `setup`
/// writes into agents' configs, e.g. Homebrew's `/opt/homebrew/bin/plugbay`), which an upgrade
/// repoints at the new version.
fn remember_stable_link() {
    STABLE_LINK.get_or_init(|| {
        let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
        crate::clients::link_on_path(&executable, &std::env::var_os("PATH")?)
    });
}

/// The binary to run as the daemon. `current_exe` is the path this process was started from
/// (on macOS a link like `/opt/homebrew/bin/plugbay` stays a link, so it already runs an upgrade).
/// When an upgrade deleted that file (a Cellar path, or Linux, where it is always the resolved
/// file), the stable link found at startup, else the first safe `plugbay` on PATH. Those two must
/// be at least this version: an older one would start a daemon this shim retires again.
async fn daemon_executable(hints: &ProjectHints) -> Option<PathBuf> {
    if let Ok(executable) = std::env::current_exe()
        && executable.is_file()
    {
        return Some(executable);
    }
    let stable_link = STABLE_LINK.get().cloned().flatten();
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&search_path).map(|directory| directory.join("plugbay"));
    for candidate in stable_link.into_iter().chain(on_path) {
        if is_safe_to_run(&candidate, hints) && reports_at_least_our_version(&candidate).await {
            return Some(candidate);
        }
        tracing::debug!(candidate = %candidate.display(), "not starting the daemon from here");
    }
    None
}

/// A project can't plant the daemon: relative PATH entries (`.`, ``) resolve inside it, and so
/// can absolute ones under its directory. The cwd counts unless it is `/` or `$HOME`, which
/// plugbay never treats as a project either.
fn is_safe_to_run(candidate: &Path, hints: &ProjectHints) -> bool {
    use std::os::unix::fs::PermissionsExt;

    if !candidate.is_absolute() {
        return false;
    }
    let Ok(resolved_candidate) = candidate.canonicalize() else {
        return false;
    };
    let is_executable_file = std::fs::metadata(&resolved_candidate)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if !is_executable_file {
        return false;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = hints
        .cwd
        .clone()
        .filter(|cwd| cwd != Path::new("/") && Some(cwd) != home.as_ref());
    let project_dirs = [
        hints.explicit.clone(),
        hints.claude_project_dir.clone(),
        cwd,
    ];
    project_dirs.into_iter().flatten().all(|directory| {
        let directory = directory.canonicalize().unwrap_or(directory);
        !candidate.starts_with(&directory) && !resolved_candidate.starts_with(&directory)
    })
}

async fn reports_at_least_our_version(candidate: &Path) -> bool {
    let mut command = tokio::process::Command::new(candidate);
    command
        .arg("--version")
        .env_remove(protocol::TEST_VERSION_ENV)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(VERSION_CHECK_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => return false,
    };
    // `plugbay 0.3.0`
    let printed = String::from_utf8_lossy(&output.stdout);
    let Some(version) = printed.split_whitespace().last() else {
        return false;
    };
    matches!(protocol::skew(version), Skew::Same | Skew::PeerNewer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_plugbay(directory: &Path, version: &str) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join("plugbay");
        std::fs::write(&path, format!("#!/bin/sh\necho 'plugbay {version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn hints_at(project: &Path) -> ProjectHints {
        ProjectHints {
            explicit: Some(project.to_path_buf()),
            cwd: Some(project.to_path_buf()),
            ..ProjectHints::default()
        }
    }

    #[test]
    fn a_plugbay_inside_the_project_or_relative_is_never_run() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let project = root.join("repo");
        let planted = fake_plugbay(&project.join("bin"), "99.0.0");
        let installed = fake_plugbay(&root.join("usr/bin"), "99.0.0");
        let hints = hints_at(&project);

        assert!(!is_safe_to_run(&planted, &hints));
        assert!(!is_safe_to_run(Path::new("plugbay"), &hints));
        assert!(!is_safe_to_run(Path::new("./bin/plugbay"), &hints));
        assert!(is_safe_to_run(&installed, &hints));
    }

    #[test]
    fn a_link_into_the_project_is_never_run() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let project = root.join("repo");
        let planted = fake_plugbay(&project, "99.0.0");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let link = root.join("bin/plugbay");
        std::os::unix::fs::symlink(&planted, &link).unwrap();

        assert!(!is_safe_to_run(&link, &hints_at(&project)));
    }

    #[test]
    fn a_file_that_is_not_executable_is_never_run() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = fake_plugbay(&root.join("bin"), "99.0.0");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(!is_safe_to_run(&path, &hints_at(&root.join("repo"))));
    }

    #[tokio::test]
    async fn an_older_plugbay_never_starts_the_daemon() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let older = fake_plugbay(&root.join("old"), "0.0.1");
        let newer = fake_plugbay(&root.join("new"), "99.0.0");
        let broken = fake_plugbay(&root.join("broken"), "garbage");

        assert!(!reports_at_least_our_version(&older).await);
        assert!(!reports_at_least_our_version(&broken).await);
        assert!(reports_at_least_our_version(&newer).await);
    }

    #[tokio::test]
    async fn a_started_daemon_that_exits_is_reaped() {
        let pid = spawn_and_reap(tokio::process::Command::new("true")).unwrap();
        let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // A zombie still exists for `kill(pid, 0)`; a reaped process doesn't.
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(tokio::time::Instant::now() < deadline, "never reaped");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
