//! Native MCP config of Claude Code and Codex: `setup`, `import`, and what `doctor` inspects.

mod claude;
mod codex;
mod import;
mod migrate;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use url::Url;

use crate::config::ServerIdentity;
use crate::paths::{self, Paths};

pub(crate) use claude::ClaudeScope;
pub use import::{ImportOptions, import};
pub use migrate::{MigrateOptions, migrate};

/// The name of the one MCP entry `setup` registers in every client.
pub(crate) const PLUGBAY_ENTRY: &str = "plugbay";

/// An MCP server as a client configures it natively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeServer {
    /// The name in the client's config, before plugbay normalisation.
    pub name: String,
    pub source: NativeSource,
    pub transport: NativeTransport,
    /// What plugbay can't carry over (`headersHelper`, SSE, disabled entries), shown on import.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSource {
    Claude(ClaudeScope),
    Codex,
}

/// `Debug` shows env and header names only: their values may be secrets.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum NativeTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// Whether a client already has the `plugbay` entry, and whether it runs this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetupState {
    Missing,
    UpToDate,
    /// The entry exists but runs something else, e.g. an older install location.
    Outdated {
        command: String,
    },
}

/// The command clients run to reach plugbay: this binary with `connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShimCommand {
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Client {
    ClaudeCode,
    Codex,
}

impl Client {
    pub const ALL: [Client; 2] = [Client::ClaudeCode, Client::Codex];

    pub fn display_name(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::Codex => "Codex",
        }
    }

    /// The name `--client` takes, which is also the client's CLI.
    pub fn cli_name(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude",
            Client::Codex => "codex",
        }
    }

    pub fn config_file(self, home: &Path) -> PathBuf {
        match self {
            Client::ClaudeCode => claude::config_file(home),
            Client::Codex => codex::config_file(home),
        }
    }

    /// Its CLI is on `PATH`, or it has left config behind (desktop apps ship their own CLI).
    pub fn is_installed(self, home: &Path) -> bool {
        find_on_path(self.cli_name()).is_some() || self.config_file(home).exists()
    }

    pub fn setup_state(self, home: &Path, shim: &ShimCommand) -> anyhow::Result<SetupState> {
        let servers = match self {
            Client::ClaudeCode => claude::read_user_servers(home)?,
            Client::Codex => codex::read_servers(home)?,
        };
        Ok(SetupState::from_servers(&servers, shim))
    }

    fn from_name(name: &str) -> anyhow::Result<Self> {
        match name.to_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Client::ClaudeCode),
            "codex" => Ok(Client::Codex),
            other => anyhow::bail!("unknown client `{other}`: expected `claude` or `codex`"),
        }
    }
}

impl SetupState {
    /// The state of the `plugbay` entry among a client's servers.
    pub fn from_servers(servers: &[NativeServer], shim: &ShimCommand) -> Self {
        let Some(entry) = servers.iter().find(|server| server.name == PLUGBAY_ENTRY) else {
            return SetupState::Missing;
        };
        match &entry.transport {
            NativeTransport::Stdio { command, args, .. }
                if *command == shim.command && *args == ShimCommand::args() =>
            {
                SetupState::UpToDate
            }
            NativeTransport::Stdio { command, args, .. } => SetupState::Outdated {
                command: format!("{command} {}", args.join(" "))
                    .trim_end()
                    .to_string(),
            },
            NativeTransport::Http { url, .. } => SetupState::Outdated {
                command: url.clone(),
            },
        }
    }
}

impl ShimCommand {
    /// The path agents should run. A package manager's link on `PATH` (Homebrew's
    /// `/opt/homebrew/bin/plugbay`) survives upgrades; the versioned file it points at does not.
    pub fn current() -> anyhow::Result<Self> {
        let executable = std::env::current_exe().context("locating the plugbay binary")?;
        let executable = executable
            .canonicalize()
            .with_context(|| format!("resolving {}", executable.display()))?;
        let search_path = std::env::var_os("PATH").unwrap_or_default();
        let command = link_on_path(&executable, &search_path).unwrap_or(executable);
        Ok(Self {
            command: command.display().to_string(),
        })
    }

    pub fn args() -> Vec<String> {
        vec!["connect".to_string()]
    }
}

/// The first `plugbay` on `search_path` that resolves to `executable`, as written on the path.
fn link_on_path(executable: &Path, search_path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .map(|directory| directory.join("plugbay"))
        .find(|candidate| {
            candidate
                .canonicalize()
                .is_ok_and(|resolved| resolved == executable)
        })
}

impl NativeServer {
    /// The identity plugbay would key credentials by, for remote servers with a valid URL.
    pub fn url_identity(&self) -> Option<ServerIdentity> {
        let NativeTransport::Http { url, .. } = &self.transport else {
            return None;
        };
        let url = Url::parse(url).ok()?;
        Some(ServerIdentity::for_url(&url))
    }

    /// The shell command that removes this server from its client, for hints.
    pub fn removal_hint(&self, home: &Path) -> String {
        match self.source {
            NativeSource::Claude(scope) => {
                format!(
                    "claude mcp remove --scope {} {}",
                    scope.cli_name(),
                    self.name
                )
            }
            NativeSource::Codex => format!(
                "remove [mcp_servers.{}] from {}",
                self.name,
                codex::config_file(home).display()
            ),
        }
    }
}

impl fmt::Debug for NativeTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &env.keys().collect::<Vec<_>>())
                .field("cwd", cwd)
                .finish(),
            NativeTransport::Http { url, headers } => formatter
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &headers.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

impl fmt::Display for NativeSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeSource::Claude(ClaudeScope::User) => {
                formatter.write_str("Claude Code (user scope)")
            }
            NativeSource::Claude(ClaudeScope::Local) => {
                formatter.write_str("Claude Code (local scope)")
            }
            NativeSource::Claude(ClaudeScope::Project) => {
                formatter.write_str("Claude Code (.mcp.json)")
            }
            NativeSource::Codex => formatter.write_str("Codex"),
        }
    }
}

/// Every MCP server Claude Code and Codex configure natively for a session in `project_dir`
/// (`None`: no project, so only user-wide servers), without plugbay's own entry.
pub(crate) fn read_native_servers(
    home: &Path,
    project_dir: Option<&Path>,
) -> anyhow::Result<Vec<NativeServer>> {
    let mut servers = claude::read_user_servers(home)?;
    if let Some(project_dir) = project_dir {
        servers.extend(claude::read_project_servers(home, project_dir)?);
    }
    servers.extend(codex::read_servers(home)?);
    servers.retain(|server| server.name != PLUGBAY_ENTRY);
    Ok(servers)
}

/// Turns a native server name into a plugbay one: lowercase letters, digits and `-`.
/// `None` when nothing usable is left.
pub(crate) fn normalize_server_name(name: &str) -> Option<String> {
    let mut normalized = String::new();
    for character in name.to_lowercase().chars() {
        let mapped = match character {
            'a'..='z' | '0'..='9' => character,
            '-' | '_' | ' ' | '.' => '-',
            _ => continue,
        };
        if mapped == '-' && (normalized.is_empty() || normalized.ends_with('-')) {
            continue;
        }
        normalized.push(mapped);
    }
    let normalized = normalized.trim_end_matches('-').to_string();
    (!normalized.is_empty()).then_some(normalized)
}

/// The first executable named `program` on `PATH`.
pub(crate) fn find_on_path(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path_variable = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_variable) {
        let candidate = directory.join(program);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

/// Reads a file, treating a missing one as `None`.
pub(crate) fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes `contents` to `file`, first copying the current file (if any) to the backups dir.
/// Returns the backup path.
fn write_with_backup(
    paths: &Paths,
    file: &Path,
    contents: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let backup = if file.exists() {
        Some(backup(paths, file)?)
    } else {
        None
    };
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_atomically(file, contents)?;
    Ok(backup)
}

/// Writes a temp file next to `file` and renames it over, so a client starting meanwhile
/// never reads half a config. Symlinks are followed, so a config kept in dotfiles stays linked.
fn write_atomically(file: &Path, contents: &str) -> anyhow::Result<()> {
    let target = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let file_name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?;
    let temporary = target.with_file_name(format!(
        ".{}.plugbay-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    std::fs::write(&temporary, contents)
        .with_context(|| format!("writing {}", temporary.display()))?;
    if let Ok(metadata) = std::fs::metadata(&target) {
        std::fs::set_permissions(&temporary, metadata.permissions())
            .with_context(|| format!("copying permissions of {}", target.display()))?;
    }
    if let Err(error) = std::fs::rename(&temporary, &target) {
        if let Err(cleanup_error) = std::fs::remove_file(&temporary) {
            tracing::warn!(error = %cleanup_error, path = %temporary.display(), "could not remove temp file");
        }
        return Err(error).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

fn backup(paths: &Paths, file: &Path) -> anyhow::Result<PathBuf> {
    paths.ensure_state_dir()?;
    let backups_dir = paths.backups_dir();
    paths::create_private_dir(&backups_dir)?;

    let file_name = file
        .file_name()
        .with_context(|| format!("{} has no file name", file.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs();
    let backup = unused_path(
        &backups_dir,
        &format!("{}.{timestamp}", file_name.to_string_lossy()),
    );

    std::fs::copy(file, &backup)
        .with_context(|| format!("backing up {} to {}", file.display(), backup.display()))?;
    Ok(backup)
}

/// `directory/name`, or `directory/name-1`, `-2`, ... when taken: Claude Code, Codex and plugbay
/// configs can all be backed up within the same second.
fn unused_path(directory: &Path, name: &str) -> PathBuf {
    let mut candidate = directory.join(name);
    let mut counter = 1;
    while candidate.exists() {
        candidate = directory.join(format!("{name}-{counter}"));
        counter += 1;
    }
    candidate
}

/// `plugbay setup`: registers the `plugbay` entry in Claude Code and Codex.
pub async fn setup(paths: &Paths, clients: &[String], dry_run: bool) -> anyhow::Result<()> {
    let selected = parse_clients(clients)?;
    let shim = ShimCommand::current()?;
    println!("plugbay entry: {} connect", shim.command);

    if selected.contains(&Client::ClaudeCode) {
        setup_claude(paths, &shim, dry_run).await?;
    }
    if selected.contains(&Client::Codex) {
        setup_codex(paths, &shim, dry_run)?;
    }
    println!("T3 Code: nothing to do, it runs Claude Code and Codex with their own MCP config");
    Ok(())
}

fn parse_clients(names: &[String]) -> anyhow::Result<Vec<Client>> {
    if names.is_empty() {
        return Ok(Client::ALL.to_vec());
    }
    let mut clients = Vec::new();
    for name in names {
        clients.push(Client::from_name(name)?);
    }
    Ok(clients)
}

async fn setup_claude(paths: &Paths, shim: &ShimCommand, dry_run: bool) -> anyhow::Result<()> {
    if !Client::ClaudeCode.is_installed(&paths.home) {
        println!("Claude Code: not installed, skipped");
        return Ok(());
    }
    if find_on_path("claude").is_none() {
        println!(
            "Claude Code: `claude` CLI not found on PATH; install it, then run `plugbay setup --client claude`"
        );
        return Ok(());
    }

    let state = Client::ClaudeCode.setup_state(&paths.home, shim)?;
    let commands = claude::setup_commands(&state, shim);
    if commands.is_empty() {
        println!("Claude Code: already set up (user scope)");
        return Ok(());
    }

    for arguments in &commands {
        if dry_run {
            println!(
                "Claude Code: would run `claude {}`",
                claude::display_command(arguments)
            );
        } else {
            claude::run_cli(arguments, &paths.home).await?;
        }
    }
    if !dry_run {
        println!("Claude Code: registered `plugbay` in user scope");
    }
    Ok(())
}

fn setup_codex(paths: &Paths, shim: &ShimCommand, dry_run: bool) -> anyhow::Result<()> {
    if !Client::Codex.is_installed(&paths.home) {
        println!("Codex: not installed, skipped");
        return Ok(());
    }

    let file = Client::Codex.config_file(&paths.home);
    if Client::Codex.setup_state(&paths.home, shim)? == SetupState::UpToDate {
        println!("Codex: already set up in {}", file.display());
        return Ok(());
    }

    if dry_run {
        println!(
            "Codex: would set [mcp_servers.plugbay] command = {:?}, args = [\"connect\"] in {}",
            shim.command,
            file.display()
        );
        return Ok(());
    }

    let contents = read_optional(&file)?.unwrap_or_default();
    let updated = codex::with_plugbay_entry(&contents, shim)?;
    let backup = write_with_backup(paths, &file, &updated)?;
    println!(
        "Codex: registered [mcp_servers.plugbay] in {}",
        file.display()
    );
    if let Some(backup) = backup {
        println!("       backup: {}", backup.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_prefers_the_link_on_path_over_the_versioned_binary() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cellar = root.join("Cellar/plugbay/0.2.0/bin");
        let linked = root.join("bin");
        std::fs::create_dir_all(&cellar).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(cellar.join("plugbay"), "").unwrap();
        std::os::unix::fs::symlink(cellar.join("plugbay"), linked.join("plugbay")).unwrap();
        let search_path = std::env::join_paths([root.join("elsewhere"), linked.clone()]).unwrap();

        assert_eq!(
            link_on_path(&cellar.join("plugbay"), &search_path),
            Some(linked.join("plugbay"))
        );
        assert_eq!(link_on_path(&root.join("other"), &search_path), None);
    }

    #[test]
    fn server_names_become_lowercase_with_dashes() {
        assert_eq!(
            normalize_server_name("My_Server Name").as_deref(),
            Some("my-server-name")
        );
    }

    #[test]
    fn server_names_collapse_and_trim_separators() {
        assert_eq!(
            normalize_server_name("__claude.ai  Figma!__").as_deref(),
            Some("claude-ai-figma")
        );
    }

    #[test]
    fn server_names_without_usable_characters_are_rejected() {
        assert_eq!(normalize_server_name("!!!"), None);
    }

    fn shim() -> ShimCommand {
        ShimCommand {
            command: "/usr/local/bin/plugbay".to_string(),
        }
    }

    fn plugbay_entry(command: &str) -> NativeServer {
        NativeServer {
            name: PLUGBAY_ENTRY.to_string(),
            source: NativeSource::Codex,
            transport: NativeTransport::Stdio {
                command: command.to_string(),
                args: ShimCommand::args(),
                env: BTreeMap::new(),
                cwd: None,
            },
            notes: Vec::new(),
        }
    }

    #[test]
    fn entry_running_this_binary_is_up_to_date() {
        let servers = [plugbay_entry("/usr/local/bin/plugbay")];
        assert_eq!(
            SetupState::from_servers(&servers, &shim()),
            SetupState::UpToDate
        );
    }

    #[test]
    fn entry_running_another_binary_is_outdated() {
        let servers = [plugbay_entry("/old/plugbay")];
        assert_eq!(
            SetupState::from_servers(&servers, &shim()),
            SetupState::Outdated {
                command: "/old/plugbay connect".to_string()
            }
        );
    }

    #[test]
    fn no_entry_is_missing() {
        assert_eq!(SetupState::from_servers(&[], &shim()), SetupState::Missing);
    }

    #[test]
    fn atomic_write_keeps_symlinks_and_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::TempDir::new().unwrap();
        let real = root.path().join("real.toml");
        let link = root.path().join("link.toml");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomically(&link, "new").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn unknown_client_is_an_error() {
        assert!(parse_clients(&["cursor".to_string()]).is_err());
    }

    #[test]
    fn backup_copies_into_backups_dir() {
        let root = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_test(root.path());
        let file = root.path().join("config.toml");
        std::fs::write(&file, "old").unwrap();

        let backup = write_with_backup(&paths, &file, "new").unwrap().unwrap();

        assert!(backup.starts_with(paths.backups_dir()));
        assert_private(&paths.state_dir);
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(file).unwrap(), "new");
    }

    fn assert_private(directory: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(directory).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn backups_of_same_named_files_do_not_overwrite_each_other() {
        let root = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_test(root.path());
        let codex_config = root.path().join("codex.toml");
        let plugbay_config = root.path().join("plugbay.toml");
        std::fs::write(&codex_config, "codex").unwrap();
        std::fs::write(&plugbay_config, "plugbay").unwrap();

        let first = backup(&paths, &codex_config).unwrap();
        std::fs::rename(&plugbay_config, &codex_config).unwrap();
        let second = backup(&paths, &codex_config).unwrap();

        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "codex");
    }
}
