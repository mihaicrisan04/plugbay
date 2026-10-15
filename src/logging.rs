use std::fs::OpenOptions;
use std::sync::Mutex;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

use crate::paths::Paths;

/// Logs to stderr. MCP clients capture a stdio server's stderr, so this is where shim logs land.
pub fn init_stderr(filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter(filter))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

/// Logs to `daemon.log` in the state dir, and to stderr when it is a terminal (`plugbay daemon` run by hand).
pub fn init_daemon(paths: &Paths, filter: &str) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    paths.ensure_state_dir()?;
    let log_path = paths.daemon_log();
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(Mutex::new(log_file));
    let terminal_layer = std::io::stderr()
        .is_terminal()
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));

    tracing_subscriber::registry()
        .with(env_filter(filter))
        .with(file_layer)
        .with(terminal_layer)
        .init();
    Ok(())
}

fn env_filter(filter: &str) -> EnvFilter {
    EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"))
}
