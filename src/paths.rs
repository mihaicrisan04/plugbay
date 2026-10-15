use std::path::{Path, PathBuf};

use anyhow::Context;

/// Every location plugbay reads or writes outside a project.
///
/// `PLUGBAY_HOME` relocates everything under one directory (used by tests);
/// otherwise XDG locations are used on every platform, like mise.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> anyhow::Result<Self> {
        let home = std::env::home_dir().context("cannot determine the home directory")?;

        if let Some(plugbay_home) = std::env::var_os("PLUGBAY_HOME") {
            let root = PathBuf::from(plugbay_home);
            return Ok(Self {
                home,
                config_dir: root.join("config"),
                state_dir: root.join("state"),
            });
        }

        let config_root = xdg_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let state_root = xdg_dir("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        Ok(Self {
            config_dir: config_root.join("plugbay"),
            state_dir: state_root.join("plugbay"),
            home,
        })
    }

    #[cfg(test)]
    pub fn for_test(root: &Path) -> Self {
        Self {
            home: root.join("home"),
            config_dir: root.join("config"),
            state_dir: root.join("state"),
        }
    }

    pub fn global_config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("daemon.sock")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    pub fn trust_file(&self) -> PathBuf {
        self.state_dir.join("trust.toml")
    }

    /// Directories holding config files plugbay has seen (see `index.rs`).
    pub fn index_file(&self) -> PathBuf {
        self.state_dir.join("index.toml")
    }

    /// Who each login really is, as its server's whoami tool answered (see `identity.rs`).
    pub fn accounts_file(&self) -> PathBuf {
        self.state_dir.join("accounts.toml")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.state_dir.join("backups")
    }

    pub fn ensure_state_dir(&self) -> anyhow::Result<()> {
        create_private_dir(&self.state_dir)
    }
}

/// Creates `dir` readable only by the current user; the socket and trust file live there.
pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting permissions of {}", dir.display()))
}

fn xdg_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}
