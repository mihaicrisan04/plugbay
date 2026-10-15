//! `plugbay connect`: the stdio MCP server each agent launches. It forwards bytes to the daemon.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::net::UnixStream;

use crate::daemon;
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, Hello};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(paths: &Paths, project: Option<PathBuf>) -> anyhow::Result<()> {
    let hints = ProjectHints {
        explicit: project,
        claude_project_dir: std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
        root: None,
        cwd: std::env::current_dir().ok(),
    };

    let stream = match connect_or_start_daemon(paths).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "no daemon available, serving in-process"
            );
            return daemon::serve_in_process(paths, hints).await;
        }
    };

    forward(stream, hints).await;
    // stdin is read on a blocking thread that can't be cancelled; exit instead of waiting for it.
    std::process::exit(0)
}

async fn connect_or_start_daemon(paths: &Paths) -> anyhow::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(paths.socket()).await {
        return Ok(stream);
    }

    start_daemon(paths)?;
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    let mut delay = Duration::from_millis(20);
    loop {
        match UnixStream::connect(paths.socket()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(error).context("the daemon did not come up");
            }
            Err(_) => {
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
    let executable = std::env::current_exe().context("locating the plugbay binary")?;
    tracing::info!("starting the plugbay daemon");
    std::process::Command::new(executable)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawning the daemon")?;
    Ok(())
}

/// Copies the agent's stdio to the daemon and back until either side closes.
async fn forward(mut stream: UnixStream, hints: ProjectHints) {
    let hello = Hello::Session {
        version: protocol::VERSION.to_string(),
        hints,
    };
    if let Err(error) = protocol::write_line(&mut stream, &hello).await {
        tracing::error!(error = format!("{error:#}"), "could not greet the daemon");
        return;
    }

    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut to_daemon) => {
            if let Err(error) = result {
                tracing::debug!(%error, "agent side closed");
            }
        }
        result = tokio::io::copy(&mut from_daemon, &mut stdout) => {
            if let Err(error) = result {
                tracing::warn!(%error, "daemon connection closed");
            }
        }
    }
}
