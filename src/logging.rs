use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::sync::Mutex;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use crate::paths::Paths;

/// Logs to stderr. MCP clients capture a stdio server's stderr, so this is where shim logs land;
/// colours only when a person is watching, never in a pipe or a client's log file.
pub fn init_stderr(filter: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter(filter))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();
}

/// Logs to `daemon.log` in the state dir, and to stderr when it is a terminal (`plugbay daemon` run by hand).
pub fn init_daemon(paths: &Paths, filter: &str) -> anyhow::Result<()> {
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

/// A bare level (`debug`) applies to plugbay only; dependencies stay at `warn`, since rmcp at
/// debug logs every tool response. A full filter (`debug,rmcp=debug`) is used as written.
fn env_filter(filter: &str) -> EnvFilter {
    let directives = match filter.parse::<LevelFilter>() {
        Ok(level) => format!("{},plugbay={level}", level.min(LevelFilter::WARN)),
        Err(_) => filter.to_string(),
    };
    EnvFilter::try_new(&directives).unwrap_or_else(|_| EnvFilter::new("warn,plugbay=info"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_level_only_raises_plugbays_own_logs() {
        assert_eq!(env_filter("debug").to_string(), "plugbay=debug,warn");
        assert_eq!(env_filter("error").to_string(), "plugbay=error,error");
    }

    #[test]
    fn a_full_filter_is_used_as_written() {
        assert_eq!(
            env_filter("debug,rmcp=trace").to_string(),
            "rmcp=trace,debug"
        );
    }
}
