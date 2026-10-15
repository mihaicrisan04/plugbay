//! `plugbay import`: native Claude Code and Codex servers → plugbay config entries.
//!
//! Secret-looking env and header values never land in config: `--apply` moves them to the
//! keychain and writes a `${keychain:...}` ref; the preview shows a placeholder instead.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};
use url::Url;

use super::{
    ClaudeScope, NativeServer, NativeSource, NativeTransport, Paths, claude, codex, find_on_path,
    normalize_server_name, read_native_servers, read_optional, write_with_backup,
};
use crate::config::{DEFAULT_ACCOUNT, LOCAL_FILE, PROJECT_FILE, ServerIdentity};
use crate::registry::{self, RegistryEntry};
use crate::secrets::{self, KEYCHAIN_SERVICE};

#[derive(Debug, clap::Args)]
pub struct ImportOptions {
    /// Write the result instead of printing it. Secrets move to the keychain.
    #[arg(long)]
    pub apply: bool,
    /// Write into `./plugbay.toml` (and `./plugbay.local.toml` for entries holding secrets)
    /// instead of the global config.
    #[arg(long)]
    pub here: bool,
    /// After writing, remove the imported servers from the native configs (backups are kept).
    #[arg(long, requires = "apply")]
    pub remove_native: bool,
}

/// A native server turned into a plugbay entry, not written yet.
#[derive(Debug, Clone)]
pub(super) struct ImportedServer {
    pub(super) key: String,
    pub(super) entry: PlugbayEntry,
    pub(super) secrets: Vec<PendingSecret>,
    /// The native entry, plus identical ones from other sources (removed together by `--remove-native`).
    pub(super) sources: Vec<NativeServer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlugbayEntry {
    /// `linear = "default"`: a registry server with nothing to override.
    Registry,
    /// A remote server; `url` is `None` when the registry provides it.
    Remote {
        url: Option<String>,
        headers: BTreeMap<String, String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
}

/// A secret pulled out of an env or header value; the entry holds `placeholder` until it is stored.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct PendingSecret {
    pub(super) keychain_name: String,
    pub(super) placeholder: String,
    pub(super) value: String,
}

#[derive(Debug)]
pub(super) struct SkippedServer {
    pub(super) name: String,
    pub(super) source: NativeSource,
    pub(super) reason: String,
}

#[derive(Debug, Default)]
pub(super) struct ImportPlan {
    pub(super) servers: Vec<ImportedServer>,
    pub(super) skipped: Vec<SkippedServer>,
}

/// Where imported entries go. With `--here`, entries holding keychain refs go to the personal
/// `plugbay.local.toml`, because those refs only resolve on this machine.
struct ImportTargets {
    shared: PathBuf,
    with_secrets: PathBuf,
}

pub async fn import(paths: &Paths, options: ImportOptions) -> anyhow::Result<()> {
    let project_dir = std::env::current_dir().context("reading the current directory")?;
    let targets = ImportTargets::new(paths, &project_dir, options.here);
    // Held until the end of the import, so planning sees what a concurrent import wrote.
    let _import_lock = if options.apply {
        Some(lock_imports(paths).await?)
    } else {
        None
    };

    let natives = read_native_servers(&paths.home, Some(&project_dir))?;
    let mut existing_names = Vec::new();
    for file in targets.files() {
        let existing = read_optional(file)?.unwrap_or_default();
        existing_names.extend(existing_server_names(&existing)?);
    }
    let mut plan = plan_import(&natives, registry::all(), &existing_names);

    print_skipped(&plan.skipped);
    if plan.servers.is_empty() {
        println!("Nothing to import.");
        return Ok(());
    }
    print_secrets_in_args(&plan.servers);

    if !options.apply {
        for file in targets.files() {
            let servers = targets.servers_for(file, &plan.servers);
            if !servers.is_empty() {
                println!("Would add to {}:\n", file.display());
                println!("{}", merge_into("", &servers)?);
            }
        }
        println!("Run `plugbay import --apply` to write it (secrets move to the keychain).");
        print_trust_note(&plan.servers, options.here);
        return Ok(());
    }

    store_secrets(&mut plan.servers).await?;
    for file in targets.files() {
        let servers = targets.servers_for(file, &plan.servers);
        if !servers.is_empty() {
            write_servers(paths, file, &servers)?;
        }
    }
    print_trust_note(&plan.servers, options.here);

    if options.remove_native {
        remove_native(paths, &plan.servers, &project_dir).await;
    }
    crate::commands::notify_daemon_reload(paths).await;
    Ok(())
}

/// Serialises `import --apply` across processes: keychain names are picked read-then-write and
/// config files are read, merged and rewritten. The lock is released when the file drops.
async fn lock_imports(paths: &Paths) -> anyhow::Result<File> {
    paths.ensure_state_dir()?;
    let lock_path = paths.state_dir.join("import.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;

    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(TryLockError::WouldBlock) => {
            println!("Waiting for another `plugbay import` to finish...");
        }
        Err(TryLockError::Error(error)) => {
            return Err(error).with_context(|| format!("locking {}", lock_path.display()));
        }
    }
    tokio::task::spawn_blocking(move || file.lock().map(|()| file))
        .await
        .context("lock task panicked")?
        .with_context(|| format!("locking {}", lock_path.display()))
}

impl ImportTargets {
    fn new(paths: &Paths, project_dir: &Path, here: bool) -> Self {
        if here {
            Self {
                shared: project_dir.join(PROJECT_FILE),
                with_secrets: project_dir.join(LOCAL_FILE),
            }
        } else {
            Self {
                shared: paths.global_config(),
                with_secrets: paths.global_config(),
            }
        }
    }

    fn files(&self) -> Vec<&Path> {
        if self.shared == self.with_secrets {
            vec![&self.shared]
        } else {
            vec![&self.shared, &self.with_secrets]
        }
    }

    fn file_for(&self, server: &ImportedServer) -> &Path {
        if server.secrets.is_empty() {
            &self.shared
        } else {
            &self.with_secrets
        }
    }

    fn servers_for<'a>(
        &self,
        file: &Path,
        servers: &'a [ImportedServer],
    ) -> Vec<&'a ImportedServer> {
        servers
            .iter()
            .filter(|server| self.file_for(server) == file)
            .collect()
    }
}

fn write_servers(paths: &Paths, file: &Path, servers: &[&ImportedServer]) -> anyhow::Result<()> {
    let existing = read_optional(file)?.unwrap_or_default();
    let merged = merge_into(&existing, servers)?;
    let backup = write_with_backup(paths, file, &merged)?;
    println!(
        "Imported {} server(s) into {}",
        servers.len(),
        file.display()
    );
    if let Some(backup) = backup {
        println!("  backup: {}", backup.display());
    }
    Ok(())
}

/// Converts `natives` into new entries. Names in `existing_names`, unconvertible names and
/// conflicting duplicates are skipped.
pub(super) fn plan_import(
    natives: &[NativeServer],
    registry: &[RegistryEntry],
    existing_names: &[String],
) -> ImportPlan {
    let mut plan = ImportPlan::default();

    for native in natives {
        let Some((key, entry, pending)) = convert(native, registry) else {
            plan.skipped.push(SkippedServer {
                name: native.name.clone(),
                source: native.source,
                reason: "the name has no letters or digits plugbay can use".to_string(),
            });
            continue;
        };

        if existing_names.contains(&key) {
            plan.skipped.push(SkippedServer {
                name: native.name.clone(),
                source: native.source,
                reason: format!("`{key}` is already configured"),
            });
            continue;
        }

        if let Some(planned) = plan.servers.iter_mut().find(|planned| planned.key == key) {
            if planned.entry == entry && planned.secrets == pending {
                planned.sources.push(native.clone());
            } else {
                plan.skipped.push(SkippedServer {
                    name: native.name.clone(),
                    source: native.source,
                    reason: format!(
                        "differs from `{key}` imported from {}; add it by hand",
                        planned.sources[0].source
                    ),
                });
            }
            continue;
        }

        plan.servers.push(ImportedServer {
            key,
            entry,
            secrets: pending,
            sources: vec![native.clone()],
        });
    }
    plan
}

/// The plugbay key, entry and secrets for one native server. `None` when the name can't be normalised.
fn convert(
    native: &NativeServer,
    registry: &[RegistryEntry],
) -> Option<(String, PlugbayEntry, Vec<PendingSecret>)> {
    let identity = secret_identity(native);
    match &native.transport {
        NativeTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let key = normalize_server_name(&native.name)?;
            let (env, pending) = move_secrets_out(&key, &identity, env);
            let entry = PlugbayEntry::Stdio {
                command: command.clone(),
                args: args.clone(),
                env,
                cwd: cwd.clone(),
            };
            Some((key, entry, pending))
        }
        NativeTransport::Http { url, headers } => {
            let registry_entry = registry_entry_for(native, registry);
            let key = match registry_entry {
                Some(registry_entry) => registry_entry.name.to_string(),
                None => normalize_server_name(&native.name)?,
            };
            let (headers, pending) = move_secrets_out(&key, &identity, headers);
            let entry = match (registry_entry, headers.is_empty()) {
                (Some(_), true) => PlugbayEntry::Registry,
                (Some(_), false) => PlugbayEntry::Remote { url: None, headers },
                (None, _) => PlugbayEntry::Remote {
                    url: Some(url.clone()),
                    headers,
                },
            };
            Some((key, entry, pending))
        }
    }
}

/// The registry entry whose URL has the same credential identity as `native`'s.
fn registry_entry_for<'registry>(
    native: &NativeServer,
    registry: &'registry [RegistryEntry],
) -> Option<&'registry RegistryEntry> {
    let identity = native.url_identity()?;
    for entry in registry {
        let Ok(url) = Url::parse(entry.url) else {
            continue;
        };
        if ServerIdentity::for_url(&url) == identity {
            return Some(entry);
        }
    }
    None
}

/// What keychain names derive from: the credential identity for remote servers, the command
/// line for stdio ones. Two servers that share a name but not an endpoint get different entries.
fn secret_identity(native: &NativeServer) -> String {
    if let Some(identity) = native.url_identity() {
        return identity.0;
    }
    match &native.transport {
        NativeTransport::Stdio { command, args, .. } => ServerIdentity::for_stdio(command, args).0,
        NativeTransport::Http { url, .. } => url.clone(),
    }
}

/// Replaces secret-looking values with keychain placeholders and returns the secrets separately.
/// Values that already hold a secret ref (`${env:...}`, `${op://...}`) are kept as they are.
fn move_secrets_out(
    server_key: &str,
    identity: &str,
    values: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<PendingSecret>) {
    let mut kept = BTreeMap::new();
    let mut pending = Vec::new();
    for (field, value) in values {
        if !is_secret_field(field) || secrets::contains_secret_ref(value) {
            kept.insert(field.clone(), value.clone());
            continue;
        }
        let keychain_name = keychain_name(server_key, identity, field);
        let placeholder = format!("${{keychain:{KEYCHAIN_SERVICE}/{keychain_name}}}");
        let (scheme, secret) = split_bearer_scheme(value);
        kept.insert(field.clone(), format!("{scheme}{placeholder}"));
        pending.push(PendingSecret {
            keychain_name,
            placeholder,
            value: secret.to_string(),
        });
    }
    (kept, pending)
}

/// `<server>-<field>-<8 hex>`: readable in Keychain Access, unique per identity + account + field.
fn keychain_name(server_key: &str, identity: &str, field: &str) -> String {
    let fingerprint = Sha256::digest(format!("{identity}|{DEFAULT_ACCOUNT}|{field}"));
    let short_fingerprint = &hex::encode(fingerprint)[..8];
    let field_name = normalize_server_name(field).unwrap_or_default();
    format!("{server_key}-{field_name}-{short_fingerprint}")
}

fn is_secret_field(field: &str) -> bool {
    field.eq_ignore_ascii_case("authorization") || secrets::looks_secret(field)
}

/// `"Bearer abc"` → `("Bearer ", "abc")`, so the keychain holds the bare token.
fn split_bearer_scheme(value: &str) -> (&str, &str) {
    let scheme_length = "Bearer ".len();
    match value.get(..scheme_length) {
        Some(scheme) if scheme.eq_ignore_ascii_case("Bearer ") => value.split_at(scheme_length),
        _ => ("", value),
    }
}

/// True when an arg looks like it carries a secret inline: `--token=abc`, `Authorization:Bearer abc`,
/// or a secret-named flag followed by a value. Args using `${...}` expansion are fine.
fn has_secret_in_args(args: &[String]) -> bool {
    for (index, argument) in args.iter().enumerate() {
        if let Some((name, value)) = argument.split_once(['=', ':'])
            && secrets::looks_secret(name)
            && !value.is_empty()
            && !value.contains("${")
        {
            return true;
        }
        let is_secret_flag = argument.starts_with('-') && secrets::looks_secret(argument);
        if let Some(next) = args.get(index + 1)
            && is_secret_flag
            && !argument.contains('=')
            && !next.starts_with('-')
            && !next.contains("${")
        {
            return true;
        }
    }
    false
}

async fn store_secrets(servers: &mut [ImportedServer]) -> anyhow::Result<()> {
    for server in servers {
        for secret in &server.secrets {
            let reference = tokio::task::spawn_blocking({
                let name = secret.keychain_name.clone();
                let value = secret.value.clone();
                move || store_without_overwriting(&name, &value)
            })
            .await
            .context("keychain task panicked")?
            .with_context(|| {
                format!("storing secret `{}` in the keychain", secret.keychain_name)
            })?;
            server
                .entry
                .replace_in_values(&secret.placeholder, &reference);
        }
    }
    Ok(())
}

/// Stores `value` as `name`, or as `name-2`, `name-3`, ... when that entry already holds a
/// different secret. Returns the `${keychain:...}` ref. Blocking.
pub(super) fn store_without_overwriting(name: &str, value: &str) -> anyhow::Result<String> {
    let mut candidate = name.to_string();
    let mut attempt = 1;
    loop {
        match crate::vault::read(KEYCHAIN_SERVICE, &candidate)? {
            Some(existing) if existing != value => {
                attempt += 1;
                candidate = format!("{name}-{attempt}");
            }
            _ => return secrets::store_in_keychain(&candidate, value),
        }
    }
}

impl PlugbayEntry {
    pub(super) fn replace_in_values(&mut self, from: &str, to: &str) {
        let Some(values) = self.secret_values_mut() else {
            return;
        };
        for value in values.values_mut() {
            *value = value.replace(from, to);
        }
    }

    /// Header or env values: the only places plugbay resolves secret refs.
    fn secret_values_mut(&mut self) -> Option<&mut BTreeMap<String, String>> {
        match self {
            PlugbayEntry::Registry => None,
            PlugbayEntry::Remote { headers, .. } => Some(headers),
            PlugbayEntry::Stdio { env, .. } => Some(env),
        }
    }

    fn secret_values(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            PlugbayEntry::Registry => None,
            PlugbayEntry::Remote { headers, .. } => Some(headers),
            PlugbayEntry::Stdio { env, .. } => Some(env),
        }
    }

    fn has_secret_refs(&self) -> bool {
        let Some(values) = self.secret_values() else {
            return false;
        };
        values
            .values()
            .any(|value| secrets::contains_secret_ref(value))
    }

    pub(super) fn to_toml(&self) -> Item {
        match self {
            PlugbayEntry::Registry => toml_edit::value(DEFAULT_ACCOUNT),
            PlugbayEntry::Remote { url, headers } => {
                let mut table = InlineTable::new();
                if let Some(url) = url {
                    table.insert("url", url.as_str().into());
                }
                if !headers.is_empty() {
                    table.insert("headers", inline_table(headers));
                }
                toml_edit::value(table)
            }
            PlugbayEntry::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut table = InlineTable::new();
                table.insert("command", command.as_str().into());
                if !args.is_empty() {
                    table.insert("args", Value::Array(args.iter().collect::<Array>()));
                }
                if !env.is_empty() {
                    table.insert("env", inline_table(env));
                }
                if let Some(cwd) = cwd {
                    table.insert("cwd", cwd.as_str().into());
                }
                toml_edit::value(table)
            }
        }
    }
}

fn inline_table(values: &BTreeMap<String, String>) -> Value {
    let mut table = InlineTable::new();
    for (key, value) in values {
        table.insert(key, value.as_str().into());
    }
    Value::InlineTable(table)
}

pub(super) fn existing_server_names(existing: &str) -> anyhow::Result<Vec<String>> {
    let document: DocumentMut = existing.parse().context("parsing the plugbay config")?;
    let Some(servers) = document.get("servers").and_then(Item::as_table_like) else {
        return Ok(Vec::new());
    };
    Ok(servers.iter().map(|(name, _)| name.to_string()).collect())
}

/// `existing` with `servers` added under `[servers]`, each preceded by a comment naming its source.
fn merge_into(existing: &str, servers: &[&ImportedServer]) -> anyhow::Result<String> {
    let mut document: DocumentMut = existing.parse().context("parsing the plugbay config")?;
    let table = document
        .entry("servers")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .context("`servers` in the plugbay config is not a table")?;

    for server in servers {
        let separator = if table.is_empty() { "" } else { "\n" };
        table.insert(&server.key, server.entry.to_toml());
        if let Some(mut key) = table.key_mut(&server.key) {
            key.leaf_decor_mut()
                .set_prefix(format!("{separator}{}", server.comment()));
        }
    }
    Ok(document.to_string())
}

impl ImportedServer {
    pub(super) fn comment(&self) -> String {
        let mut comment = String::new();
        for source in &self.sources {
            comment.push_str(&format!("# from {} `{}`\n", source.source, source.name));
        }
        for note in self.sources.iter().flat_map(|source| &source.notes) {
            comment.push_str(&format!("# note: {note}\n"));
        }
        comment
    }

    /// Stdio commands and secret refs only take effect in trusted files.
    fn needs_trust(&self) -> bool {
        matches!(self.entry, PlugbayEntry::Stdio { .. }) || self.entry.has_secret_refs()
    }
}

pub(super) fn print_skipped(skipped: &[SkippedServer]) {
    for server in skipped {
        println!(
            "skipped `{}` from {}: {}",
            server.name, server.source, server.reason
        );
    }
    if !skipped.is_empty() {
        println!();
    }
}

fn print_trust_note(servers: &[ImportedServer], here: bool) {
    let names: Vec<&str> = servers
        .iter()
        .filter(|server| server.needs_trust())
        .map(|server| server.key.as_str())
        .collect();
    if names.is_empty() {
        return;
    }
    if here {
        println!(
            "\nServers that run code or use secret refs ({}) only reach agents after you run `plugbay trust` here.",
            names.join(", ")
        );
        println!(
            "Entries with keychain refs go to {LOCAL_FILE}: the refs only resolve on this machine."
        );
    } else {
        println!(
            "\nServers that run code or use secret refs ({}) go into the global config, which is trusted automatically.",
            names.join(", ")
        );
    }
}

/// Names only: the args themselves may hold the secret.
pub(super) fn print_secrets_in_args(servers: &[ImportedServer]) {
    for server in servers {
        let PlugbayEntry::Stdio { args, .. } = &server.entry else {
            continue;
        };
        if has_secret_in_args(args) {
            println!(
                "warning: `{}` seems to pass a secret in its args, which were copied as they are; move it to `env` with a secret ref",
                server.key
            );
        }
    }
}

/// Removes imported servers from their native configs. Failures are reported, not fatal:
/// the plugbay config is already written by now.
pub(super) async fn remove_native(paths: &Paths, servers: &[ImportedServer], project_dir: &Path) {
    let natives: Vec<&NativeServer> = servers.iter().flat_map(|server| &server.sources).collect();

    let mut codex_names = Vec::new();
    for native in &natives {
        match native.source {
            NativeSource::Claude(scope) => {
                remove_from_claude(paths, native, scope, project_dir).await
            }
            NativeSource::Codex => codex_names.push(native.name.as_str()),
        }
    }
    if !codex_names.is_empty()
        && let Err(error) = remove_from_codex(paths, &codex_names)
    {
        println!(
            "could not remove {} from Codex: {error:#}",
            codex_names.join(", ")
        );
    }
}

async fn remove_from_claude(
    paths: &Paths,
    native: &NativeServer,
    scope: ClaudeScope,
    project_dir: &Path,
) {
    if find_on_path("claude").is_none() {
        println!(
            "`claude` CLI not found; remove `{}` by hand: {}",
            native.name,
            native.removal_hint(&paths.home)
        );
        return;
    }
    let arguments = claude::remove_command(scope, &native.name);
    match claude::run_cli(&arguments, project_dir).await {
        Ok(()) => println!("removed `{}` from {}", native.name, native.source),
        Err(error) => println!(
            "could not remove `{}` from {}: {error:#}",
            native.name, native.source
        ),
    }
}

fn remove_from_codex(paths: &Paths, names: &[&str]) -> anyhow::Result<()> {
    let file = codex::config_file(&paths.home);
    let contents = read_optional(&file)?.unwrap_or_default();
    let updated = codex::without_servers(&contents, names)?;
    let backup: Option<PathBuf> = write_with_backup(paths, &file, &updated)?;
    println!(
        "removed {} from Codex ({})",
        names.join(", "),
        file.display()
    );
    if let Some(backup) = backup {
        println!("  backup: {}", backup.display());
    }
    Ok(())
}

impl fmt::Debug for PendingSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingSecret")
            .field("keychain_name", &self.keychain_name)
            .field("placeholder", &self.placeholder)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &[RegistryEntry] = &[RegistryEntry {
        name: "linear",
        title: "Linear",
        url: "https://mcp.linear.app/mcp",
        whoami: None,
    }];

    fn http(name: &str, url: &str, headers: &[(&str, &str)], source: NativeSource) -> NativeServer {
        NativeServer {
            name: name.to_string(),
            source,
            transport: NativeTransport::Http {
                url: url.to_string(),
                headers: headers
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            },
            notes: Vec::new(),
        }
    }

    fn stdio(name: &str, env: &[(&str, &str)]) -> NativeServer {
        NativeServer {
            name: name.to_string(),
            source: NativeSource::Codex,
            transport: NativeTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "some-server".to_string()],
                env: env
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
                cwd: None,
            },
            notes: vec!["Codex forwarded env vars HOME".to_string()],
        }
    }

    fn user_scope() -> NativeSource {
        NativeSource::Claude(ClaudeScope::User)
    }

    #[test]
    fn registry_url_becomes_short_form() {
        let natives = [http(
            "linear-server",
            "https://mcp.linear.app/mcp/",
            &[],
            user_scope(),
        )];
        let plan = plan_import(&natives, REGISTRY, &[]);
        assert_eq!(plan.servers[0].key, "linear");
        assert_eq!(plan.servers[0].entry, PlugbayEntry::Registry);
    }

    #[test]
    fn authorization_header_moves_to_keychain_placeholder() {
        let natives = [http(
            "Posthog_EU",
            "https://mcp-eu.posthog.com/mcp",
            &[("Authorization", "Bearer test-token")],
            user_scope(),
        )];
        let plan = plan_import(&natives, REGISTRY, &[]);
        let server = &plan.servers[0];
        assert_eq!(server.key, "posthog-eu");
        assert_eq!(server.secrets[0].value, "test-token");
        let PlugbayEntry::Remote { headers, .. } = &server.entry else {
            panic!("expected remote");
        };
        let expected = format!("Bearer {}", server.secrets[0].placeholder);
        assert_eq!(headers["Authorization"], expected);
        assert!(
            server.secrets[0]
                .keychain_name
                .starts_with("posthog-eu-authorization-")
        );
    }

    #[tokio::test]
    async fn import_lock_is_exclusive_until_dropped() {
        let root = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_test(root.path());
        let held = lock_imports(&paths).await.unwrap();

        let other = File::open(paths.state_dir.join("import.lock")).unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
        drop(held);
        assert!(other.try_lock().is_ok());
    }

    #[test]
    fn same_named_servers_on_other_endpoints_get_other_keychain_names() {
        let first = [http(
            "acme",
            "https://one.example/mcp",
            &[("Authorization", "Bearer a")],
            user_scope(),
        )];
        let second = [http(
            "acme",
            "https://two.example/mcp",
            &[("Authorization", "Bearer b")],
            user_scope(),
        )];
        let first_plan = plan_import(&first, REGISTRY, &[]);
        let second_plan = plan_import(&second, REGISTRY, &[]);
        assert_ne!(
            first_plan.servers[0].secrets[0].keychain_name,
            second_plan.servers[0].secrets[0].keychain_name
        );
    }

    #[test]
    fn inline_secret_args_are_detected() {
        let args = ["--api-key".to_string(), "test-key".to_string()];
        assert!(has_secret_in_args(&args));
        assert!(has_secret_in_args(&["--token=test-token".to_string()]));
        assert!(has_secret_in_args(&[
            "Authorization:Bearer test-token".to_string()
        ]));
    }

    #[test]
    fn args_with_expansion_or_plain_names_are_not_secrets() {
        let args = [
            "--header".to_string(),
            "Authorization:Bearer ${INTRANET_TOKEN}".to_string(),
            "keycloak-mcp".to_string(),
            "https://example.test/mcp".to_string(),
        ];
        assert!(!has_secret_in_args(&args));
    }

    #[test]
    fn here_sends_entries_with_secrets_to_the_local_file() {
        let root = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_test(root.path());
        let targets = ImportTargets::new(&paths, root.path(), true);
        let natives = [
            http(
                "linear-server",
                "https://mcp.linear.app/mcp",
                &[],
                user_scope(),
            ),
            stdio("tool", &[("API_TOKEN", "test-token")]),
        ];
        let plan = plan_import(&natives, REGISTRY, &[]);

        let local = targets.servers_for(&root.path().join(LOCAL_FILE), &plan.servers);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].key, "tool");
    }

    #[test]
    fn env_values_that_are_already_refs_stay() {
        let natives = [stdio("tool", &[("API_TOKEN", "${env:TOOL_TOKEN}")])];
        let plan = plan_import(&natives, REGISTRY, &[]);
        assert!(plan.servers[0].secrets.is_empty());
    }

    #[test]
    fn servers_already_in_the_config_are_skipped() {
        let natives = [http(
            "linear-server",
            "https://mcp.linear.app/mcp",
            &[],
            user_scope(),
        )];
        let plan = plan_import(&natives, REGISTRY, &["linear".to_string()]);
        assert!(plan.servers.is_empty());
        assert_eq!(plan.skipped[0].reason, "`linear` is already configured");
    }

    #[test]
    fn identical_servers_from_two_clients_import_once() {
        let natives = [
            http("linear", "https://mcp.linear.app/mcp", &[], user_scope()),
            http(
                "linear",
                "https://mcp.linear.app/mcp",
                &[],
                NativeSource::Codex,
            ),
        ];
        let plan = plan_import(&natives, REGISTRY, &[]);
        assert_eq!(plan.servers.len(), 1);
        assert_eq!(plan.servers[0].sources.len(), 2);
    }

    #[test]
    fn conflicting_servers_with_one_name_keep_the_first() {
        let natives = [
            http("sentry", "https://mcp.sentry.dev/mcp", &[], user_scope()),
            http(
                "sentry",
                "https://sentry.example/mcp",
                &[],
                NativeSource::Codex,
            ),
        ];
        let plan = plan_import(&natives, REGISTRY, &[]);
        assert_eq!(plan.servers.len(), 1);
        assert_eq!(plan.skipped.len(), 1);
    }

    #[test]
    fn stored_secret_ref_replaces_placeholder() {
        let natives = [stdio("tool", &[("API_TOKEN", "test-token")])];
        let mut plan = plan_import(&natives, REGISTRY, &[]);
        let server = &mut plan.servers[0];
        let placeholder = server.secrets[0].placeholder.clone();
        server
            .entry
            .replace_in_values(&placeholder, "${keychain:plugbay/stored}");
        let PlugbayEntry::Stdio { env, .. } = &server.entry else {
            panic!("expected stdio");
        };
        assert_eq!(env["API_TOKEN"], "${keychain:plugbay/stored}");
    }

    #[test]
    fn preview_renders_servers_with_source_comments() {
        let natives = [
            http(
                "linear-server",
                "https://mcp.linear.app/mcp",
                &[],
                user_scope(),
            ),
            http(
                "Posthog",
                "https://mcp.posthog.com/mcp",
                &[("X-Api-Key", "test-token")],
                user_scope(),
            ),
            stdio("tool", &[("DEBUG", "1")]),
        ];
        let plan = plan_import(&natives, REGISTRY, &[]);
        let servers: Vec<&ImportedServer> = plan.servers.iter().collect();
        insta::assert_snapshot!(merge_into("", &servers).unwrap());
    }

    #[test]
    fn merge_keeps_existing_servers_and_comments() {
        let existing = "account = \"initech\" # tree default\n\n[servers]\nsentry = true\n";
        let natives = [http(
            "linear-server",
            "https://mcp.linear.app/mcp",
            &[],
            user_scope(),
        )];
        let existing_names = existing_server_names(existing).unwrap();
        let plan = plan_import(&natives, REGISTRY, &existing_names);
        let servers: Vec<&ImportedServer> = plan.servers.iter().collect();
        insta::assert_snapshot!(merge_into(existing, &servers).unwrap());
    }

    #[test]
    fn debug_output_redacts_secret_values() {
        let natives = [stdio("tool", &[("API_TOKEN", "test-token")])];
        let plan = plan_import(&natives, REGISTRY, &[]);
        assert!(!format!("{plan:?}").contains("test-token"));
    }
}
