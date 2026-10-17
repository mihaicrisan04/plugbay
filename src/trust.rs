//! Trust for config that executes code or resolves secrets, pinned to a fingerprint of those parts.
//! Only the human grants it (`plugbay trust`, the in-agent prompt, `trusted_paths`): an agent can
//! write files too, so where a file came from proves nothing.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::config::{ConfigFile, ConfigScope, ResolvedConfig, ResolvedServer};
use crate::paths::{Paths, state_file};

const TRUST_FILE_HEADER: &str =
    "# Managed by `plugbay trust`: config file path = fingerprint of its executable parts.\n";

/// Persistent grants (`trust.toml` in the state dir) plus grants for the daemon's lifetime (`allow_once`).
pub struct Trust {
    trust_file: PathBuf,
    session_grants: HashSet<(PathBuf, String)>,
}

/// The on-disk shape of `trust.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    files: BTreeMap<String, String>,
}

impl Trust {
    pub fn new(paths: &Paths) -> Self {
        Self {
            trust_file: paths.trust_file(),
            session_grants: HashSet::new(),
        }
    }

    /// Global files are always trusted; others when under a global `trusted_paths` glob, or when
    /// a grant matches their current fingerprint. Reads `trust.toml` on every call so grants
    /// made by `plugbay trust` apply without restarting the daemon.
    #[cfg(test)]
    pub fn is_file_trusted(&self, file: &ConfigFile, trusted_paths: &[String]) -> bool {
        self.check(trusted_paths).is_file_trusted(file)
    }

    /// True when every file the server needs trust from is trusted.
    pub fn is_server_trusted(&self, server: &ResolvedServer, config: &ResolvedConfig) -> bool {
        self.check(&config.trusted_paths)
            .is_server_trusted(server, config)
    }

    /// Files some resolved server needs trust from that are not trusted yet.
    /// Files no server needs trust from (plain remote config) are never listed.
    pub fn untrusted_files<'config>(
        &self,
        config: &'config ResolvedConfig,
    ) -> Vec<&'config ConfigFile> {
        let check = self.check(&config.trusted_paths);
        config
            .files
            .iter()
            .filter(|file| some_server_needs_trust_from(config, &file.path))
            .filter(|file| !check.is_file_trusted(file))
            .collect()
    }

    pub fn grant_for_session(&mut self, file: &ConfigFile) {
        let Some(fingerprint) = &file.executable_fingerprint else {
            tracing::warn!(path = %file.path.display(), "config file has no fingerprint to trust");
            return;
        };
        self.session_grants
            .insert((file.path.clone(), fingerprint.clone()));
    }

    pub fn grant_always(&mut self, file: &ConfigFile) -> anyhow::Result<()> {
        let Some(fingerprint) = &file.executable_fingerprint else {
            anyhow::bail!("{} has no fingerprint to pin trust to", file.path.display());
        };
        self.change(|trust_file| {
            trust_file
                .files
                .insert(path_key(&file.path), fingerprint.clone());
        })
    }

    /// Removes persistent and session grants for `file`. Returns false when it had none.
    pub fn revoke(&mut self, file: &Path) -> anyhow::Result<bool> {
        let session_grants_before = self.session_grants.len();
        self.session_grants
            .retain(|(granted_path, _fingerprint)| granted_path != file);
        let revoked_session_grant = self.session_grants.len() != session_grants_before;

        let mut revoked_persistent_grant = false;
        self.change(|trust_file| {
            revoked_persistent_grant = trust_file.files.remove(&path_key(file)).is_some();
        })?;
        Ok(revoked_session_grant || revoked_persistent_grant)
    }

    /// Loads grants and globs once, for evaluating many files or servers.
    fn check(&self, trusted_paths: &[String]) -> TrustCheck<'_> {
        TrustCheck {
            session_grants: &self.session_grants,
            persistent_grants: self.load_grants_or_none(),
            trusted_paths: build_trusted_paths(trusted_paths),
        }
    }

    /// Grants for checks, which must answer yes or no: an unreadable trust file trusts nothing.
    fn load_grants_or_none(&self) -> BTreeMap<String, String> {
        match self.load() {
            Ok(trust_file) => trust_file.files,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "ignoring the trust file");
                BTreeMap::new()
            }
        }
    }

    fn load(&self) -> anyhow::Result<TrustFile> {
        let contents = match std::fs::read_to_string(&self.trust_file) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TrustFile::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading {}", self.trust_file.display()));
            }
        };
        toml::from_str(&contents).with_context(|| format!("parsing {}", self.trust_file.display()))
    }

    /// Read-modify-write under the state-file lock, so the daemon and CLI commands never lose
    /// each other's grants or revocations.
    fn change(&self, change: impl FnOnce(&mut TrustFile)) -> anyhow::Result<()> {
        let _lock = state_file::lock(&state_file::lock_path_for(&self.trust_file))?;
        let mut trust_file = self.load()?;
        change(&mut trust_file);
        let contents = format!(
            "{TRUST_FILE_HEADER}{}",
            toml::to_string(&trust_file).context("serialising trust grants")?
        );
        state_file::write_atomic(&self.trust_file, &contents)
    }
}

/// A snapshot of every grant, so one check reads `trust.toml` and builds the globs once.
struct TrustCheck<'trust> {
    session_grants: &'trust HashSet<(PathBuf, String)>,
    persistent_grants: BTreeMap<String, String>,
    trusted_paths: GlobSet,
}

impl TrustCheck<'_> {
    /// Fails closed: a server that needs trust from a file that was not loaded, or from a
    /// file without a fingerprint to pin a grant to, stays untrusted.
    fn is_server_trusted(&self, server: &ResolvedServer, config: &ResolvedConfig) -> bool {
        for path in &server.requires_trust_from {
            let Some(file) = config.files.iter().find(|file| &file.path == path) else {
                tracing::warn!(
                    path = %path.display(),
                    server = %server.display_name(),
                    "server needs trust from a file that was not loaded"
                );
                return false;
            };
            if !self.is_file_trusted(file) {
                return false;
            }
        }
        true
    }

    fn is_file_trusted(&self, file: &ConfigFile) -> bool {
        if file.scope == ConfigScope::Global {
            return true;
        }
        if is_under_trusted_path(&file.path, &self.trusted_paths) {
            return true;
        }
        let Some(fingerprint) = &file.executable_fingerprint else {
            return false;
        };
        let session_grant = (file.path.clone(), fingerprint.clone());
        if self.session_grants.contains(&session_grant) {
            return true;
        }
        self.persistent_grants.get(&path_key(&file.path)) == Some(fingerprint)
    }
}

fn some_server_needs_trust_from(config: &ResolvedConfig, path: &Path) -> bool {
    config.servers.iter().any(|server| {
        server
            .requires_trust_from
            .iter()
            .any(|required| required == path)
    })
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Invalid globs are skipped with a warning rather than failing every trust check.
fn build_trusted_paths(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(error) => tracing::warn!(pattern, %error, "ignoring invalid trusted_paths glob"),
        }
    }
    builder.build().unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring trusted_paths");
        GlobSet::empty()
    })
}

/// A file is under a trusted path when the glob matches the file itself or any directory
/// above it, so both `~/work` and `~/work/**` trust everything in `~/work`.
fn is_under_trusted_path(file: &Path, trusted_paths: &GlobSet) -> bool {
    file.ancestors().any(|path| trusted_paths.is_match(path))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;
    use crate::config::{ServerIdentity, ServerTransport, ToolFilter};
    use crate::project::ProjectLocation;

    fn project_file(path: &str, fingerprint: Option<&str>) -> ConfigFile {
        ConfigFile {
            path: PathBuf::from(path),
            scope: ConfigScope::Project,
            executable_fingerprint: fingerprint.map(str::to_string),
        }
    }

    fn config_with(files: Vec<ConfigFile>, trusted_paths: Vec<String>) -> ResolvedConfig {
        ResolvedConfig {
            location: ProjectLocation::global_only(),
            files,
            servers: Vec::new(),
            disabled: Vec::new(),
            catalog: Vec::new(),
            problems: Vec::new(),
            trusted_paths,
        }
    }

    fn stdio_server(requires_trust_from: Vec<PathBuf>) -> ResolvedServer {
        ResolvedServer {
            name: "local".to_string(),
            account: "default".to_string(),
            tool_prefix: "local".to_string(),
            transport: ServerTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["some-server".to_string()],
                env: BTreeMap::new(),
                cwd: None,
            },
            identity: ServerIdentity("stdio:test".to_string()),
            tool_filter: ToolFilter::default(),
            account_metadata: BTreeMap::new(),
            provenance: BTreeMap::new(),
            requires_trust_from,
        }
    }

    fn trust_in(directory: &TempDir) -> Trust {
        Trust::new(&Paths::for_test(directory.path()))
    }

    #[test]
    fn global_file_is_always_trusted() {
        let directory = TempDir::new().unwrap();
        let file = ConfigFile {
            path: PathBuf::from("/config/config.toml"),
            scope: ConfigScope::Global,
            executable_fingerprint: Some("abc".to_string()),
        };
        assert!(trust_in(&directory).is_file_trusted(&file, &[]));
    }

    #[test]
    fn server_needing_trust_from_a_file_without_fingerprint_is_untrusted() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", None);
        let config = config_with(vec![file.clone()], Vec::new());
        let server = stdio_server(vec![file.path.clone()]);
        assert!(!trust_in(&directory).is_server_trusted(&server, &config));
    }

    #[test]
    fn granting_a_file_without_fingerprint_is_an_error() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", None);
        assert!(trust_in(&directory).grant_always(&file).is_err());
    }

    #[test]
    fn file_with_executable_parts_starts_untrusted() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", Some("abc"));
        assert!(!trust_in(&directory).is_file_trusted(&file, &[]));
    }

    #[test]
    fn trusted_paths_glob_trusts_nested_files() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/home/me/work/client/app/plugbay.toml", Some("abc"));
        let trust = trust_in(&directory);
        assert!(trust.is_file_trusted(&file, &["/home/me/work/**".to_string()]));
        assert!(trust.is_file_trusted(&file, &["/home/me/work".to_string()]));
        assert!(!trust.is_file_trusted(&file, &["/home/me/personal/**".to_string()]));
    }

    #[test]
    fn persistent_grant_survives_a_new_trust_instance() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", Some("abc"));
        trust_in(&directory).grant_always(&file).unwrap();
        assert!(trust_in(&directory).is_file_trusted(&file, &[]));
    }

    #[test]
    fn changed_fingerprint_needs_trust_again() {
        let directory = TempDir::new().unwrap();
        trust_in(&directory)
            .grant_always(&project_file("/work/app/plugbay.toml", Some("abc")))
            .unwrap();
        let edited = project_file("/work/app/plugbay.toml", Some("def"));
        assert!(!trust_in(&directory).is_file_trusted(&edited, &[]));
    }

    #[test]
    fn trust_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        let mut trust = trust_in(&directory);
        trust
            .grant_always(&project_file("/work/app/plugbay.toml", Some("abc")))
            .unwrap();
        let mode = std::fs::metadata(&trust.trust_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn session_grant_is_not_persisted() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", Some("abc"));
        let mut trust = trust_in(&directory);
        trust.grant_for_session(&file);
        assert!(trust.is_file_trusted(&file, &[]));
        assert!(!trust_in(&directory).is_file_trusted(&file, &[]));
    }

    #[test]
    fn revoke_removes_both_kinds_of_grant() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", Some("abc"));
        let mut trust = trust_in(&directory);
        trust.grant_for_session(&file);
        trust.grant_always(&file).unwrap();

        assert!(trust.revoke(&file.path).unwrap());
        assert!(!trust.is_file_trusted(&file, &[]));
        assert!(!trust.revoke(&file.path).unwrap());
    }

    #[test]
    fn server_is_trusted_only_when_every_contributing_file_is() {
        let directory = TempDir::new().unwrap();
        let parent = project_file("/work/plugbay.toml", Some("abc"));
        let child = project_file("/work/app/plugbay.toml", Some("def"));
        let config = config_with(vec![parent.clone(), child.clone()], Vec::new());
        let server = stdio_server(vec![parent.path.clone(), child.path.clone()]);

        let mut trust = trust_in(&directory);
        trust.grant_for_session(&parent);
        assert!(!trust.is_server_trusted(&server, &config));
        trust.grant_for_session(&child);
        assert!(trust.is_server_trusted(&server, &config));
    }

    #[test]
    fn server_needing_trust_from_an_unloaded_file_is_untrusted() {
        let directory = TempDir::new().unwrap();
        let config = config_with(Vec::new(), Vec::new());
        let server = stdio_server(vec![PathBuf::from("/work/plugbay.toml")]);
        assert!(!trust_in(&directory).is_server_trusted(&server, &config));
    }

    #[test]
    fn untrusted_files_lists_only_files_a_server_needs_trust_from() {
        let directory = TempDir::new().unwrap();
        let remote_only = project_file("/work/plugbay.toml", Some("url-fingerprint"));
        let executable = project_file("/work/app/plugbay.toml", Some("abc"));
        let mut config = config_with(vec![remote_only, executable.clone()], Vec::new());
        config
            .servers
            .push(stdio_server(vec![executable.path.clone()]));
        assert_eq!(
            trust_in(&directory).untrusted_files(&config),
            vec![&executable]
        );
    }

    #[test]
    fn a_new_file_with_executable_parts_is_not_trusted_however_it_got_here() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("plugbay.local.toml");
        std::fs::write(&path, "").unwrap();
        let file = project_file(path.to_str().unwrap(), Some("abc"));
        let mut config = config_with(vec![file.clone()], Vec::new());
        config.servers.push(stdio_server(vec![file.path.clone()]));

        let trust = trust_in(&directory);
        assert!(!trust.is_file_trusted(&file, &[]));
        assert_eq!(trust.untrusted_files(&config), vec![&file]);
    }

    #[test]
    fn trusting_again_after_a_revoke_works() {
        let directory = TempDir::new().unwrap();
        let file = project_file("/work/app/plugbay.toml", Some("abc"));
        let mut trust = trust_in(&directory);
        trust.grant_always(&file).unwrap();
        trust.revoke(&file.path).unwrap();
        assert!(!trust.is_file_trusted(&file, &[]));

        trust.grant_always(&file).unwrap();
        assert!(trust.is_file_trusted(&file, &[]));
    }

    #[test]
    fn unreadable_trust_file_trusts_nothing() {
        let directory = TempDir::new().unwrap();
        let trust = trust_in(&directory);
        std::fs::create_dir_all(trust.trust_file.parent().unwrap()).unwrap();
        std::fs::write(&trust.trust_file, "not = [valid").unwrap();
        let file = project_file("/work/app/plugbay.toml", Some("abc"));
        assert!(!trust.is_file_trusted(&file, &[]));
    }
}
