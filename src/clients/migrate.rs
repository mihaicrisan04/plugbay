//! `plugbay migrate`: every MCP server Claude Code and Codex configure natively, moved into plugbay
//! in one go. Definitions go to the catalog in the global config (always trusted), pins go to the
//! directories that used each server (inferred from Claude Code's per-directory disable lists),
//! bearer tokens become credentials bound to the server's url, other secrets move to the keychain.
//! Then plugbay is registered with both clients and the native entries are removed.
//!
//! Every file touched is backed up first and listed in a journal, so `plugbay migrate --undo`
//! can roll the last run back.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};

use super::claude::{self, ClaudeProjectState};
use super::import::{
    ImportedServer, PlugbayEntry, existing_server_names, plan_import, print_secrets_in_args,
    print_skipped, store_without_overwriting,
};
use super::{
    ClaudeScope, Client, NativeServer, NativeSource, Paths, SetupState, ShimCommand, codex,
    find_on_path, read_optional,
};
use crate::config::{DEFAULT_ACCOUNT, LOCAL_FILE, PROJECT_FILE, ServerIdentity};
use crate::credentials::{self, StaticToken};
use crate::registry;
use crate::secrets::KEYCHAIN_SERVICE;

#[derive(Debug, clap::Args)]
pub struct MigrateOptions {
    /// Do it. Without this, only the plan is printed.
    #[arg(long)]
    apply: bool,
    /// Leave the native entries in Claude Code and Codex (agents then see both copies).
    #[arg(long, requires = "apply")]
    keep_native: bool,
    /// Roll back the last `migrate --apply`.
    #[arg(long, conflicts_with_all = ["apply", "keep_native"])]
    undo: bool,
}

/// One server to move: its definition, where it is pinned, and a bearer token to bind.
struct Migration {
    server: ImportedServer,
    placement: Placement,
    /// A literal `Authorization: Bearer ...` taken out of the headers, stored as a bound token.
    bearer_token: Option<String>,
    /// Where each native source lives, for removing it: `None` for user-wide entries.
    source_directories: Vec<Option<PathBuf>>,
}

/// Where a server is switched on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    /// On in every directory except these.
    Everywhere { off_in: Vec<PathBuf> },
    /// Off by default (a catalog entry), on in these directories.
    Only { on_in: Vec<PathBuf> },
}

pub async fn migrate(paths: &Paths, options: MigrateOptions) -> anyhow::Result<()> {
    if options.undo {
        return undo(paths).await;
    }

    let projects = claude::read_projects(&paths.home)?;
    let (migrations, skipped) = plan(paths, &projects)?;
    print_skipped(&skipped);
    if migrations.is_empty() {
        println!(
            "Nothing to migrate: Claude Code and Codex configure no MCP servers besides plugbay."
        );
        return Ok(());
    }
    let pin_files = pin_files(&migrations, &paths.home);
    print_plan(paths, &migrations, &pin_files);
    let servers: Vec<ImportedServer> = migrations
        .iter()
        .map(|migration| migration.server.clone())
        .collect();
    print_secrets_in_args(&servers);

    if !options.apply {
        println!(
            "\nRun `plugbay migrate --apply` to do it (every file is backed up; `plugbay migrate --undo` rolls it back)."
        );
        return Ok(());
    }

    let mut journal = Journal::start(paths)?;
    let result = apply(
        paths,
        migrations,
        &pin_files,
        &mut journal,
        options.keep_native,
    )
    .await;
    journal.save()?;
    result?;
    println!(
        "\nDone. Backups and the undo journal are in {}.",
        journal.directory.display()
    );
    println!("Next: `plugbay login` logs in to every account that still needs it.");
    crate::commands::notify_daemon_reload(paths).await;
    Ok(())
}

/// Reads every native config and decides where each server goes.
fn plan(
    paths: &Paths,
    projects: &[ClaudeProjectState],
) -> anyhow::Result<(Vec<Migration>, Vec<super::import::SkippedServer>)> {
    let user_wide = super::read_native_servers(&paths.home, None)?;
    let mut natives: Vec<(NativeServer, Option<PathBuf>)> =
        user_wide.into_iter().map(|native| (native, None)).collect();
    for project in projects {
        for native in &project.servers {
            if native.name != super::PLUGBAY_ENTRY {
                natives.push((native.clone(), Some(project.directory.clone())));
            }
        }
    }

    let global_config = read_optional(&paths.global_config())?.unwrap_or_default();
    let existing_names = existing_server_names(&global_config)?;
    let native_servers: Vec<NativeServer> =
        natives.iter().map(|(native, _)| native.clone()).collect();
    let import_plan = plan_import(&native_servers, registry::all(), &existing_names);

    let known_directories: Vec<PathBuf> = projects
        .iter()
        .map(|project| project.directory.clone())
        .collect();
    let mut migrations = Vec::new();
    for mut server in import_plan.servers {
        let source_directories = source_directories(&server, &natives);
        let placement = placement_for(
            &server,
            &source_directories,
            projects,
            &known_directories,
            &paths.home,
        );
        let bearer_token = take_bearer_token(&mut server);
        migrations.push(Migration {
            server,
            placement,
            bearer_token,
            source_directories,
        });
    }
    Ok((migrations, import_plan.skipped))
}

/// The directory each of `server`'s sources came from, in the same order as `server.sources`.
fn source_directories(
    server: &ImportedServer,
    natives: &[(NativeServer, Option<PathBuf>)],
) -> Vec<Option<PathBuf>> {
    let mut remaining: Vec<&(NativeServer, Option<PathBuf>)> = natives.iter().collect();
    let mut directories = Vec::new();
    for source in &server.sources {
        let position = remaining.iter().position(|(native, _)| {
            native.name == source.name
                && native.source == source.source
                && native.transport == source.transport
        });
        match position {
            Some(position) => directories.push(remaining.remove(position).1.clone()),
            None => directories.push(None),
        }
    }
    directories
}

fn placement_for(
    server: &ImportedServer,
    source_directories: &[Option<PathBuf>],
    projects: &[ClaudeProjectState],
    known_directories: &[PathBuf],
    home: &Path,
) -> Placement {
    let user_wide_names: Vec<&str> = server
        .sources
        .iter()
        .zip(source_directories)
        .filter(|(_, directory)| directory.is_none())
        .map(|(source, _)| source.name.as_str())
        .collect();

    if !user_wide_names.is_empty() {
        let disabled: BTreeSet<PathBuf> = projects
            .iter()
            .filter(|project| {
                project
                    .disabled_servers
                    .iter()
                    .any(|name| user_wide_names.contains(&name.as_str()))
            })
            .map(|project| project.directory.clone())
            .collect();
        return place(known_directories, &disabled, home);
    }

    let mut on_in = Vec::new();
    for (source, directory) in server.sources.iter().zip(source_directories) {
        let Some(directory) = directory else { continue };
        let Some(project) = projects
            .iter()
            .find(|project| &project.directory == directory)
        else {
            continue;
        };
        let declined = match source.source {
            NativeSource::Claude(ClaudeScope::Project) => {
                project.disabled_project_servers.contains(&source.name)
            }
            _ => project.disabled_servers.contains(&source.name),
        };
        if !declined && !on_in.contains(directory) {
            on_in.push(directory.clone());
        }
    }
    Placement::Only { on_in }
}

/// Picks the smaller set of pins for a user-wide server: on everywhere with `false` where it was
/// switched off, or a catalog entry with `true` where it was on. Ties keep it on everywhere,
/// so directories Claude Code hasn't seen yet still get it, as they did before.
fn place(known: &[PathBuf], disabled: &BTreeSet<PathBuf>, home: &Path) -> Placement {
    if disabled.is_empty() {
        return Placement::Everywhere { off_in: Vec::new() };
    }
    let enabled: BTreeSet<PathBuf> = known
        .iter()
        .filter(|directory| !disabled.contains(*directory))
        .cloned()
        .collect();
    let off_in = topmost_uniform(disabled, &enabled, home);
    let on_in = topmost_uniform(&enabled, disabled, home);
    if on_in.len() < off_in.len() {
        Placement::Only { on_in }
    } else {
        Placement::Everywhere { off_in }
    }
}

/// The highest directories (below `$HOME`) under which every known directory is in `wanted`,
/// so one pin there covers them: `~/dev/work` instead of each client repo under it.
fn topmost_uniform(
    wanted: &BTreeSet<PathBuf>,
    others: &BTreeSet<PathBuf>,
    home: &Path,
) -> Vec<PathBuf> {
    let mut tops: BTreeSet<PathBuf> = BTreeSet::new();
    for directory in wanted {
        let mut top = directory.clone();
        for ancestor in directory.ancestors().skip(1) {
            let below_home = ancestor.starts_with(home) && ancestor != home;
            let covers_other = others.iter().any(|other| other.starts_with(ancestor));
            if !below_home || covers_other {
                break;
            }
            top = ancestor.to_path_buf();
        }
        tops.insert(top);
    }
    let all_tops: Vec<PathBuf> = tops.iter().cloned().collect();
    tops.into_iter()
        .filter(|top| {
            !all_tops
                .iter()
                .any(|other| other != top && top.starts_with(other))
        })
        .collect()
}

/// `Authorization: Bearer <literal>` on a remote server becomes a token bound to its url, so the
/// definition holds no secret ref at all.
fn take_bearer_token(server: &mut ImportedServer) -> Option<String> {
    let PlugbayEntry::Remote { headers, .. } = &mut server.entry else {
        return None;
    };
    let (header_name, authorization) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(name, value)| (name.clone(), value.clone()))?;
    let index = server.secrets.iter().position(|secret| {
        authorization.eq_ignore_ascii_case(&format!("Bearer {}", secret.placeholder))
    })?;
    headers.remove(&header_name);
    let secret = server.secrets.remove(index);
    let registry_url_only =
        matches!(&server.entry, PlugbayEntry::Remote { url: None, headers } if headers.is_empty());
    if registry_url_only {
        server.entry = PlugbayEntry::Registry;
    }
    Some(secret.value)
}

/// Pin file → server key → on/off, from every migration's placement.
type PinFiles = BTreeMap<PathBuf, BTreeMap<String, bool>>;

fn pin_files(migrations: &[Migration], home: &Path) -> PinFiles {
    let mut files: PinFiles = BTreeMap::new();
    for migration in migrations {
        let (directories, value) = match &migration.placement {
            Placement::Everywhere { off_in } => (off_in, false),
            Placement::Only { on_in } => (on_in, true),
        };
        for directory in directories {
            files
                .entry(pin_file_for(directory, home))
                .or_default()
                .insert(migration.server.key.clone(), value);
        }
    }
    files
}

/// Inside a git checkout the pins are personal, so they go to `plugbay.local.toml` and stay
/// out of the team's repo; elsewhere (`~/dev/work`) a plain `plugbay.toml`.
fn pin_file_for(directory: &Path, home: &Path) -> PathBuf {
    let in_checkout = directory
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(home) && *ancestor != home)
        .any(|ancestor| ancestor.join(".git").exists());
    let file_name = if in_checkout {
        LOCAL_FILE
    } else {
        PROJECT_FILE
    };
    directory.join(file_name)
}

fn print_plan(paths: &Paths, migrations: &[Migration], pin_files: &PinFiles) {
    let home = &paths.home;
    println!("Definitions → {}", display(&paths.global_config(), home));
    for migration in migrations {
        let server = &migration.server;
        let reach = match &server.entry {
            PlugbayEntry::Registry => "from the registry".to_string(),
            PlugbayEntry::Remote { url: Some(url), .. } => url.clone(),
            PlugbayEntry::Remote { url: None, .. } => "registry url, custom headers".to_string(),
            PlugbayEntry::Stdio { command, args, .. } => {
                format!("runs {command} {}", args.join(" "))
            }
        };
        let scope = match &migration.placement {
            Placement::Everywhere { .. } => "on everywhere",
            Placement::Only { .. } => "catalog, pinned below",
        };
        println!("  {:<14} {reach}  ({scope})", server.key);
        if migration.bearer_token.is_some() {
            println!("  {:<14} bearer token → keychain, bound to this url", "");
        }
        for secret in &server.secrets {
            println!("  {:<14} secret → keychain ({})", "", secret.keychain_name);
        }
    }

    if !pin_files.is_empty() {
        println!("\nPins");
        for (file, pins) in pin_files {
            let described: Vec<String> = pins
                .iter()
                .map(|(key, on)| format!("{key} = {on}"))
                .collect();
            println!("  {:<44} {}", display(file, home), described.join(", "));
        }
    }

    println!("\nThen: register plugbay in Claude Code and Codex, and remove the native entries.");
}

async fn apply(
    paths: &Paths,
    migrations: Vec<Migration>,
    pin_files: &PinFiles,
    journal: &mut Journal,
    keep_native: bool,
) -> anyhow::Result<()> {
    let mut migrations = migrations;
    for migration in &mut migrations {
        store_secrets(&mut migration.server, journal).await?;
        if let Some(token) = &migration.bearer_token {
            store_bearer_token(&migration.server, token, journal).await?;
        }
    }

    let global_config = paths.global_config();
    journal.back_up(&global_config)?;
    let mut document = parse(
        &read_optional(&global_config)?.unwrap_or_default(),
        &global_config,
    )?;
    for migration in &migrations {
        add_definition(&mut document, migration)?;
    }
    write(&global_config, &document)?;
    println!(
        "Wrote {} definitions to {}",
        migrations.len(),
        display(&global_config, &paths.home)
    );

    for (file, pins) in pin_files {
        journal.back_up(file)?;
        let mut document = parse(&read_optional(file)?.unwrap_or_default(), file)?;
        let servers = servers_table(&mut document)?;
        for (key, on) in pins {
            servers.insert(key, toml_edit::value(*on));
        }
        write(file, &document)?;
        println!("Pinned in {}", display(file, &paths.home));
    }

    register_plugbay(paths, journal).await?;
    if !keep_native {
        remove_natives(paths, &migrations, journal).await?;
    }
    Ok(())
}

async fn store_secrets(server: &mut ImportedServer, journal: &mut Journal) -> anyhow::Result<()> {
    for secret in server.secrets.clone() {
        let name = secret.keychain_name.clone();
        let value = secret.value.clone();
        let (reference, created) = tokio::task::spawn_blocking(move || {
            let existed = crate::vault::read(KEYCHAIN_SERVICE, &name)?.is_some();
            let reference = store_without_overwriting(&name, &value)?;
            anyhow::Ok((reference, !existed))
        })
        .await
        .context("keychain task panicked")??;
        if created {
            journal.keychain_entries.push(reference.clone());
        }
        server
            .entry
            .replace_in_values(&secret.placeholder, &reference);
    }
    server.secrets.clear();
    Ok(())
}

async fn store_bearer_token(
    server: &ImportedServer,
    token: &str,
    journal: &mut Journal,
) -> anyhow::Result<()> {
    let Some(identity) = server.sources.iter().find_map(NativeServer::url_identity) else {
        return Ok(());
    };
    if credentials::load_token(&identity, DEFAULT_ACCOUNT)
        .await?
        .is_some()
    {
        println!(
            "{}: a token is already stored for {identity}; kept it",
            server.key
        );
        return Ok(());
    }
    let token = StaticToken::new(token, None)?;
    credentials::save_token(&identity, DEFAULT_ACCOUNT, &token).await?;
    journal.tokens.push(identity.0.clone());
    Ok(())
}

/// Registry servers on everywhere need only `name = true`; everything else gets its definition,
/// switched off when it is only pinned in some directories.
fn add_definition(document: &mut DocumentMut, migration: &Migration) -> anyhow::Result<()> {
    let everywhere = matches!(migration.placement, Placement::Everywhere { .. });
    let key = &migration.server.key;
    let servers = servers_table(document)?;
    match (&migration.server.entry, everywhere) {
        (PlugbayEntry::Registry, true) => {
            servers.insert(key, toml_edit::value(true));
        }
        (PlugbayEntry::Registry, false) => return Ok(()),
        (entry, everywhere) => {
            let mut item = entry.to_toml();
            if !everywhere && let Some(definition) = item.as_table_like_mut() {
                definition.insert("enabled", toml_edit::value(false));
                definition.fmt();
            }
            servers.insert(key, item);
        }
    }
    let separator = if servers.len() > 1 { "\n" } else { "" };
    if let Some(mut key) = servers.key_mut(key) {
        key.leaf_decor_mut()
            .set_prefix(format!("{separator}{}", migration.server.comment()));
    }
    Ok(())
}

/// Adds the `plugbay` entry to both clients, remembering whether Claude Code had one before.
async fn register_plugbay(paths: &Paths, journal: &mut Journal) -> anyhow::Result<()> {
    let shim = ShimCommand::current()?;
    let claude_had_entry = Client::ClaudeCode.is_installed(&paths.home)
        && !matches!(
            Client::ClaudeCode.setup_state(&paths.home, &shim)?,
            SetupState::Missing
        );
    journal.claude_entry_added = Client::ClaudeCode.is_installed(&paths.home) && !claude_had_entry;
    journal.back_up(&codex::config_file(&paths.home))?;
    super::setup(paths, &[], false).await
}

async fn remove_natives(
    paths: &Paths,
    migrations: &[Migration],
    journal: &mut Journal,
) -> anyhow::Result<()> {
    let claude_config = claude::config_file(&paths.home);
    if claude_config.exists() {
        let backup = journal.directory.join("claude.json");
        std::fs::copy(&claude_config, &backup)
            .with_context(|| format!("backing up {}", claude_config.display()))?;
        journal.claude_config_backup = Some(backup);
    }

    let mut codex_names = Vec::new();
    for migration in migrations {
        for (native, directory) in migration
            .server
            .sources
            .iter()
            .zip(&migration.source_directories)
        {
            match native.source {
                NativeSource::Codex => codex_names.push(native.name.as_str()),
                NativeSource::Claude(ClaudeScope::Project) => println!(
                    "Left `{}` in .mcp.json: that file is shared with your team. Decline it in Claude Code's /mcp if it shows up twice.",
                    native.name
                ),
                NativeSource::Claude(scope) => {
                    remove_from_claude(paths, native, scope, directory.as_deref(), journal).await
                }
            }
        }
    }
    if codex_names.is_empty() {
        return Ok(());
    }
    let file = codex::config_file(&paths.home);
    let contents = read_optional(&file)?.unwrap_or_default();
    let updated = codex::without_servers(&contents, &codex_names)?;
    super::write_atomically(&file, &updated)?;
    println!("Removed {} from Codex", codex_names.join(", "));
    Ok(())
}

async fn remove_from_claude(
    paths: &Paths,
    native: &NativeServer,
    scope: ClaudeScope,
    directory: Option<&Path>,
    journal: &mut Journal,
) {
    if find_on_path("claude").is_none() {
        println!(
            "`claude` CLI not found; remove `{}` by hand: {}",
            native.name,
            native.removal_hint(&paths.home)
        );
        return;
    }
    let working_dir = directory.unwrap_or(&paths.home);
    let arguments = claude::remove_command(scope, &native.name);
    match claude::run_cli(&arguments, working_dir).await {
        Ok(()) => {
            println!("Removed `{}` from {}", native.name, native.source);
            journal.claude_removed.push(RemovedClaudeServer {
                name: native.name.clone(),
                scope: scope.cli_name().to_string(),
                directory: directory.map(Path::to_path_buf),
            });
        }
        Err(error) => println!(
            "could not remove `{}` from {}: {error:#}",
            native.name, native.source
        ),
    }
}

/// What one `migrate --apply` changed, saved next to its backups.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Journal {
    #[serde(skip)]
    directory: PathBuf,
    /// Files written, with the backup of what was there (`None`: the file was created).
    #[serde(default)]
    files: Vec<BackedUpFile>,
    /// `${keychain:...}` refs this run created.
    #[serde(default)]
    keychain_entries: Vec<String>,
    /// Server identities that got a bound token (account `default`).
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default)]
    claude_removed: Vec<RemovedClaudeServer>,
    #[serde(default)]
    claude_entry_added: bool,
    /// A copy of Claude Code's config from before the run. Never restored wholesale: Claude Code
    /// rewrites it constantly, so it is only where removed entries can be copied back from.
    #[serde(default)]
    claude_config_backup: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BackedUpFile {
    path: PathBuf,
    backup: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemovedClaudeServer {
    name: String,
    scope: String,
    directory: Option<PathBuf>,
}

const JOURNAL_FILE: &str = "journal.toml";
const MIGRATION_PREFIX: &str = "migrate-";

impl Journal {
    fn start(paths: &Paths) -> anyhow::Result<Self> {
        paths.ensure_state_dir()?;
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("reading the clock")?
            .as_secs();
        let directory = paths
            .backups_dir()
            .join(format!("{MIGRATION_PREFIX}{seconds}"));
        crate::paths::create_private_dir(&directory)?;
        Ok(Self {
            directory,
            ..Self::default()
        })
    }

    /// Copies `file` into the journal directory once, before its first change.
    fn back_up(&mut self, file: &Path) -> anyhow::Result<()> {
        if self.files.iter().any(|backed_up| backed_up.path == file) {
            return Ok(());
        }
        let backup = if file.exists() {
            let backup = self
                .directory
                .join(format!("{}-{}", self.files.len(), file_name(file)));
            std::fs::copy(file, &backup)
                .with_context(|| format!("backing up {}", file.display()))?;
            Some(backup)
        } else {
            None
        };
        self.files.push(BackedUpFile {
            path: file.to_path_buf(),
            backup,
        });
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string(self).context("serialising the migration journal")?;
        let path = self.directory.join(JOURNAL_FILE);
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    fn load_latest(paths: &Paths) -> anyhow::Result<Self> {
        let backups = paths.backups_dir();
        let mut directories: Vec<PathBuf> = std::fs::read_dir(&backups)
            .with_context(|| format!("reading {}", backups.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(MIGRATION_PREFIX))
                    && path.join(JOURNAL_FILE).is_file()
            })
            .collect();
        directories.sort_by_key(|path| migration_time(path));
        let Some(directory) = directories.pop() else {
            bail!("no migration to undo");
        };
        let path = directory.join(JOURNAL_FILE);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut journal: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        journal.directory = directory;
        Ok(journal)
    }
}

fn migration_time(directory: &Path) -> u64 {
    directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(MIGRATION_PREFIX))
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(0)
}

/// Rolls back the last migration: files restored (created ones removed), keychain entries and
/// tokens it created deleted, Claude Code entries it removed added back from the backup.
async fn undo(paths: &Paths) -> anyhow::Result<()> {
    let journal = Journal::load_latest(paths)?;
    println!("Undoing the migration in {}", journal.directory.display());

    for file in &journal.files {
        match &file.backup {
            Some(backup) => {
                std::fs::copy(backup, &file.path)
                    .with_context(|| format!("restoring {}", file.path.display()))?;
                println!("Restored {}", display(&file.path, &paths.home));
            }
            None if file.path.exists() => {
                std::fs::remove_file(&file.path)
                    .with_context(|| format!("removing {}", file.path.display()))?;
                println!("Removed {}", display(&file.path, &paths.home));
            }
            None => {}
        }
    }

    for reference in &journal.keychain_entries {
        let Some(name) = keychain_entry_name(reference) else {
            continue;
        };
        let name = name.to_string();
        tokio::task::spawn_blocking(move || crate::vault::delete(KEYCHAIN_SERVICE, &name))
            .await
            .context("keychain task panicked")??;
    }
    for identity in &journal.tokens {
        credentials::forget_token(&ServerIdentity(identity.clone()), DEFAULT_ACCOUNT).await?;
    }

    if journal.claude_entry_added {
        let arguments = claude::remove_command(ClaudeScope::User, super::PLUGBAY_ENTRY);
        if let Err(error) = claude::run_cli(&arguments, &paths.home).await {
            println!("could not remove plugbay from Claude Code: {error:#}");
        }
    }
    let mut not_restored = Vec::new();
    for removed in &journal.claude_removed {
        if let Err(error) = restore_claude_server(&journal, removed, &paths.home).await {
            tracing::debug!(error = format!("{error:#}"), server = %removed.name, "not restored");
            not_restored.push(removed.name.as_str());
        }
    }
    if !not_restored.is_empty() {
        let backup = journal
            .claude_config_backup
            .as_ref()
            .map_or("the backup".to_string(), |backup| {
                backup.display().to_string()
            });
        println!(
            "\nNot re-added to Claude Code (they hold secrets, which plugbay won't pass on a command line): {}. \
             Copy them back from {backup} or re-add them with `claude mcp add`.",
            not_restored.join(", ")
        );
    }
    crate::commands::notify_daemon_reload(paths).await;
    Ok(())
}

/// Re-adds one removed Claude Code entry from the pre-migration copy, unless it holds a secret.
async fn restore_claude_server(
    journal: &Journal,
    removed: &RemovedClaudeServer,
    home: &Path,
) -> anyhow::Result<()> {
    let backup = journal
        .claude_config_backup
        .as_deref()
        .context("no copy of Claude Code's config was kept")?;
    let entry = claude::entry_in_backup(backup, &removed.name, removed.directory.as_deref())?
        .context("the entry is not in the copy")?;
    if holds_secret(&entry) {
        bail!("the entry holds a secret");
    }
    let scope = match removed.scope.as_str() {
        "user" => ClaudeScope::User,
        "local" => ClaudeScope::Local,
        other => bail!("unexpected scope {other}"),
    };
    let arguments = claude::add_json_command(scope, &removed.name, &entry);
    claude::run_cli(&arguments, removed.directory.as_deref().unwrap_or(home)).await?;
    println!(
        "Re-added `{}` to Claude Code ({} scope)",
        removed.name, removed.scope
    );
    Ok(())
}

/// True when an env or header value under a secret-looking name is a literal, not a `${...}` ref.
fn holds_secret(entry: &serde_json::Value) -> bool {
    ["env", "headers"].iter().any(|field| {
        entry
            .get(field)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|values| {
                values.iter().any(|(name, value)| {
                    let is_literal = value.as_str().is_some_and(|text| !text.contains("${"));
                    (name.eq_ignore_ascii_case("authorization")
                        || crate::secrets::looks_secret(name))
                        && is_literal
                })
            })
    })
}

/// `${keychain:plugbay/<name>}` → `<name>`.
fn keychain_entry_name(reference: &str) -> Option<&str> {
    reference
        .strip_prefix("${keychain:")?
        .strip_suffix('}')?
        .strip_prefix(KEYCHAIN_SERVICE)?
        .strip_prefix('/')
}

fn servers_table(document: &mut DocumentMut) -> anyhow::Result<&mut dyn toml_edit::TableLike> {
    document
        .entry("servers")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .context("`servers` must be a table")
}

fn parse(text: &str, file: &Path) -> anyhow::Result<DocumentMut> {
    text.parse()
        .with_context(|| format!("parsing {}", file.display()))
}

fn write(file: &Path, document: &DocumentMut) -> anyhow::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    super::write_atomically(file, &document.to_string())
}

fn file_name(file: &Path) -> String {
    file.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
}

fn display(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directories(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_server_on_everywhere_needs_no_pins() {
        let known = vec![PathBuf::from("/home/me/dev/work/a")];
        let placement = place(&known, &BTreeSet::new(), Path::new("/home/me"));
        assert_eq!(placement, Placement::Everywhere { off_in: Vec::new() });
    }

    #[test]
    fn a_server_off_in_one_tree_is_switched_off_at_its_top() {
        let known: Vec<PathBuf> = [
            "/home/me/dev/work/a",
            "/home/me/dev/work/b",
            "/home/me/dev/personal/x",
            "/home/me/dev/personal/y",
            "/home/me/dev/personal/z",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let disabled = directories(&["/home/me/dev/work/a", "/home/me/dev/work/b"]);

        let placement = place(&known, &disabled, Path::new("/home/me"));

        assert_eq!(
            placement,
            Placement::Everywhere {
                off_in: vec![PathBuf::from("/home/me/dev/work")]
            }
        );
    }

    #[test]
    fn a_server_on_in_one_tree_only_becomes_a_catalog_entry_pinned_there() {
        let known: Vec<PathBuf> = [
            "/home/me/dev/work/a",
            "/home/me/dev/work/b",
            "/home/me/dev/personal/x",
            "/home/me/dev/master",
            "/home/me/notes",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let disabled = directories(&[
            "/home/me/dev/personal/x",
            "/home/me/dev/master",
            "/home/me/notes",
        ]);

        let placement = place(&known, &disabled, Path::new("/home/me"));

        assert_eq!(
            placement,
            Placement::Only {
                on_in: vec![PathBuf::from("/home/me/dev/work")]
            }
        );
    }

    #[test]
    fn pins_never_climb_to_home() {
        let wanted = directories(&["/home/me/a", "/home/me/b"]);
        let tops = topmost_uniform(&wanted, &BTreeSet::new(), Path::new("/home/me"));
        assert_eq!(
            tops,
            vec![PathBuf::from("/home/me/a"), PathBuf::from("/home/me/b")]
        );
    }

    #[test]
    fn entries_with_literal_secrets_are_not_put_on_a_command_line() {
        let with_secret =
            serde_json::json!({ "command": "npx", "env": { "SLACK_TOKEN": "test-token" } });
        let with_ref =
            serde_json::json!({ "command": "npx", "env": { "SLACK_TOKEN": "${SLACK_TOKEN}" } });
        let remote = serde_json::json!({ "type": "http", "url": "https://mcp.linear.app/mcp" });
        assert!(holds_secret(&with_secret));
        assert!(!holds_secret(&with_ref));
        assert!(!holds_secret(&remote));
    }

    #[test]
    fn keychain_refs_give_back_their_entry_name() {
        assert_eq!(
            keychain_entry_name("${keychain:plugbay/slack-token-1a2b3c4d}"),
            Some("slack-token-1a2b3c4d")
        );
        assert_eq!(keychain_entry_name("${env:TOKEN}"), None);
    }

    #[test]
    fn a_literal_bearer_header_becomes_a_bound_token() {
        let native = NativeServer {
            name: "intranet".to_string(),
            source: NativeSource::Codex,
            transport: super::super::NativeTransport::Http {
                url: "https://intranet.example.test/api/mcp".to_string(),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer test-token".to_string(),
                )]),
            },
            notes: Vec::new(),
        };
        let plan = plan_import(&[native], registry::all(), &[]);
        let mut server = plan.servers.into_iter().next().unwrap();

        let token = take_bearer_token(&mut server);

        assert_eq!(token.as_deref(), Some("test-token"));
        assert!(server.secrets.is_empty());
        assert_eq!(
            server.entry,
            PlugbayEntry::Remote {
                url: Some("https://intranet.example.test/api/mcp".to_string()),
                headers: BTreeMap::new(),
            }
        );
    }
}
