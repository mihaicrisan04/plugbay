//! User-facing CLI commands that are not part of the agent runtime.

mod access;
mod inspect;
mod pins;

use std::path::PathBuf;

use crate::config::{self, ResolvedConfig};
use crate::index;
use crate::paths::Paths;
use crate::project::{self, ProjectHints, ProjectLocation};
use crate::protocol::ControlRequest;

pub use access::{TokenOptions, login, logout, trust};
pub use inspect::{accounts, ls, ls_all, why};
pub use pins::{AddOptions, PinOptions, add, remove, unuse_servers, use_servers};

/// Resolves the config the way an agent started in this directory would see it, and remembers
/// the directory for commands that look everywhere.
pub async fn resolve_here(paths: &Paths, cd: Option<PathBuf>) -> anyhow::Result<ResolvedConfig> {
    let config = config::resolve(paths, &location_here(paths, cd))?;
    index::record(paths, &config).await;
    Ok(config)
}

pub fn location_here(paths: &Paths, cd: Option<PathBuf>) -> ProjectLocation {
    let hints = ProjectHints {
        explicit: cd,
        cwd: std::env::current_dir().ok(),
        ..ProjectHints::default()
    };
    project::detect(&hints, &paths.home)
}

/// The global config on its own, then every directory plugbay has seen, parents first.
/// A directory whose config fails to parse is reported and skipped.
pub fn resolve_everywhere(paths: &Paths) -> anyhow::Result<Vec<ResolvedConfig>> {
    let mut configs = vec![config::resolve(paths, &ProjectLocation::global_only())?];
    for directory in index::tracked_directories(paths)? {
        let location = project::detect(
            &ProjectHints {
                explicit: Some(directory.clone()),
                ..ProjectHints::default()
            },
            &paths.home,
        );
        match config::resolve(paths, &location) {
            Ok(config) => configs.push(config),
            Err(error) => eprintln!("skipping {}: {error:#}", directory.display()),
        }
    }
    Ok(configs)
}

pub async fn stop(paths: &Paths) -> anyhow::Result<()> {
    match crate::protocol::send_control(paths, ControlRequest::Stop).await {
        Ok(response) => println!("{}", response.message),
        Err(_) => println!("daemon is not running"),
    }
    Ok(())
}

/// Tells a running daemon that `server`'s credentials changed, so it drops connections made with
/// the old ones. Silent when none is running.
pub async fn notify_credentials_changed(paths: &Paths, server: &config::ResolvedServer) {
    let request = ControlRequest::CredentialsChanged {
        identity: server.identity.0.clone(),
        account: server.account.clone(),
    };
    if let Err(error) = crate::protocol::send_control(paths, request).await {
        tracing::debug!(%error, "no daemon to tell about new credentials");
    }
}

/// Tells a running daemon to re-read config, trust and credentials. Silent when none is running.
pub async fn notify_daemon_reload(paths: &Paths) {
    if let Err(error) = crate::protocol::send_control(paths, ControlRequest::Reload).await {
        tracing::debug!(%error, "no daemon to reload");
    }
}
