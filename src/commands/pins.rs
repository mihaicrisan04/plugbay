//! `plugbay use` / `unuse` (pins in a directory's config) and `add` / `remove` (the catalog of
//! server definitions in the global config). Edits keep the file's comments and layout.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::commands::inspect::display_path;
use crate::commands::{location_here, notify_daemon_reload, resolve_everywhere};
use crate::config::{self, LOCAL_FILE, PROJECT_FILE, ResolvedConfig};
use crate::index;
use crate::paths::Paths;
use crate::project::ProjectLocation;
use crate::{registry, secrets};

#[derive(clap::Args, Debug)]
pub struct PinOptions {
    /// `server` or `server:account`.
    #[arg(required = true, value_name = "SERVER")]
    servers: Vec<String>,
    /// Write to the global config, so every directory gets it.
    #[arg(short = 'g', long, conflicts_with = "local")]
    global: bool,
    /// Write to `plugbay.local.toml` (personal, not committed) instead of `plugbay.toml`.
    #[arg(long)]
    local: bool,
}

#[derive(clap::Args, Debug)]
pub struct AddOptions {
    /// Name to pin it by, e.g. `huggingface` or `playwright`.
    name: String,
    /// Streamable HTTP endpoint of a remote server: `--url https://huggingface.co/mcp`.
    #[arg(long, conflicts_with = "command")]
    url: Option<String>,
    /// Environment for a stdio server, `NAME=value`. Secrets must be refs like
    /// `${keychain:service/account}` or `${op://vault/item/field}`, never the value itself.
    #[arg(long = "env", value_name = "NAME=VALUE", requires = "command")]
    env: Vec<String>,
    /// Replace a built-in server of the same name everywhere it is pinned.
    #[arg(long)]
    force: bool,
    /// Command line of a stdio server, after `--`: `plugbay add playwright -- npx -y @playwright/mcp`.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

/// One `server[:account]` argument.
struct PinTarget {
    name: String,
    account: Option<String>,
}

/// What `use` writes for one server: no account (the tree's default), one, or several.
#[derive(Debug, PartialEq, Eq)]
struct ServerPin {
    name: String,
    accounts: Vec<String>,
}

pub async fn use_servers(
    paths: &Paths,
    cd: Option<PathBuf>,
    options: PinOptions,
) -> anyhow::Result<()> {
    let pins = group_by_server(&parse_targets(&options.servers)?)?;
    let destination = Destination::choose(paths, cd, &options)?;
    let before = destination.resolve(paths)?;
    let original = read_or_empty(&destination.file)?;
    let mut document = parse(&original, &destination.file)?;

    for server_pin in &pins {
        pin(&mut document, server_pin)?;
    }
    write(&destination.file, &document)?;

    let config = destination.resolve(paths)?;
    index::record(paths, &config).await;
    let missing: Vec<String> = pins
        .iter()
        .filter(|server_pin| config.server(&server_pin.name).is_empty())
        .map(|server_pin| why_not_enabled(&config, &server_pin.name))
        .collect();
    if !missing.is_empty() {
        restore(&destination.file, &original)?;
        bail!("nothing written:\n  {}", missing.join("\n  "));
    }

    for server_pin in &pins {
        let mut note = replaced_accounts_note(&before, &config, &server_pin.name);
        for server in config.server(&server_pin.name) {
            println!(
                "Using {} in {}{note}",
                server.display_name(),
                destination.display(&paths.home)
            );
            note.clear();
        }
    }
    notify_daemon_reload(paths).await;
    Ok(())
}

/// What `unuse` did, or found, for one server.
enum Unpinned {
    Removed,
    AlreadyOff,
}

pub async fn unuse_servers(
    paths: &Paths,
    cd: Option<PathBuf>,
    options: PinOptions,
) -> anyhow::Result<()> {
    let targets = parse_targets(&options.servers)?;
    if let Some(target) = targets.iter().find(|target| target.account.is_some()) {
        bail!(
            "`unuse` takes server names only; to switch {}'s account, run `plugbay use {}:<account>`",
            target.name,
            target.name
        );
    }
    let destination = Destination::choose(paths, cd, &options)?;
    let before = destination.resolve(paths)?;
    let mut document = parse(&read_or_empty(&destination.file)?, &destination.file)?;

    let mut outcomes = Vec::new();
    for target in &targets {
        let name = target.name.as_str();
        let is_on = !before.server(name).is_empty() || has_problem_for(&before, name);
        if is_on {
            unpin(&mut document, name)?;
            outcomes.push(Unpinned::Removed);
        } else if is_known(&before, &document, name) {
            outcomes.push(Unpinned::AlreadyOff);
        } else {
            bail!("nothing written: {}", unknown_server(&before, name));
        }
    }
    let any_removed = outcomes
        .iter()
        .any(|outcome| matches!(outcome, Unpinned::Removed));
    if any_removed {
        write(&destination.file, &document)?;
    }

    // A server inherited from a parent directory is still on; switch it off here explicitly.
    let config = destination.resolve(paths)?;
    let still_enabled: Vec<&PinTarget> = targets
        .iter()
        .filter(|target| !config.server(&target.name).is_empty())
        .collect();
    if !still_enabled.is_empty() {
        for target in &still_enabled {
            servers_table(&mut document)?.insert(&target.name, toml_edit::value(false));
        }
        write(&destination.file, &document)?;
    }

    for (target, outcome) in targets.iter().zip(&outcomes) {
        let file = destination.display(&paths.home);
        if let Unpinned::AlreadyOff = outcome {
            println!("{} is already off here; {file} is unchanged", target.name);
            continue;
        }
        let inherited = still_enabled
            .iter()
            .any(|enabled| enabled.name == target.name);
        let note = if inherited {
            " (inherited, so it is now switched off here)"
        } else {
            ""
        };
        println!("Not using {} in {file}{note}", target.name);
    }
    if any_removed {
        notify_daemon_reload(paths).await;
    }
    Ok(())
}

pub async fn add(paths: &Paths, options: AddOptions) -> anyhow::Result<()> {
    if !config::is_valid_name(&options.name) {
        bail!("server names use lowercase letters, digits and `-` only");
    }
    check_add_options(&options, &paths.global_config())?;
    let file = paths.global_config();
    let original = read_or_empty(&file)?;
    let mut document = parse(&original, &file)?;
    if servers_table(&mut document)?.contains_key(&options.name) {
        bail!(
            "{} is already in {}; edit it there, or `plugbay remove {}` first",
            options.name,
            file.display(),
            options.name
        );
    }

    let definition = definition_table(&options)?;
    servers_table(&mut document)?.insert(&options.name, Item::Table(definition));
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write(&file, &document)?;

    if let Err(problem) = check_definition(paths, &options.name) {
        restore(&file, &original)?;
        bail!("nothing written: {problem}");
    }
    println!(
        "Added {name} to the catalog in {file}. Pin it with `plugbay use {name}` (or `{name}:<account>`).",
        name = options.name,
        file = file.display()
    );
    Ok(())
}

pub async fn remove(paths: &Paths, name: &str) -> anyhow::Result<()> {
    let file = paths.global_config();
    let mut document = parse(&read_or_empty(&file)?, &file)?;
    let built_in = registry::lookup(name);
    if servers_table(&mut document)?.remove(name).is_none() {
        if built_in.is_some() {
            bail!(
                "{name} is built in, not in your catalog, so there is nothing to remove. \
                 To stop using it, run `plugbay unuse {name}` (add `-g` for every directory)"
            );
        }
        bail!("{name} is not in the catalog ({})", file.display());
    }
    write(&file, &document)?;
    println!("Removed {name} from the catalog.");

    let still_pinned: Vec<String> = resolve_everywhere(paths)?
        .iter()
        .filter(|config| !config.server(name).is_empty() || has_problem_for(config, name))
        .filter_map(|config| config.location.config_dir.as_deref())
        .map(|directory| display_path(directory, &paths.home))
        .collect();
    match built_in {
        Some(entry) if still_pinned.is_empty() => println!(
            "The built-in {name} ({}) applies again wherever it is pinned.",
            entry.url
        ),
        Some(entry) => println!(
            "The built-in {name} ({}) takes over where it is still pinned: {} \
             (run `plugbay unuse {name}` there to stop using it)",
            entry.url,
            still_pinned.join(", ")
        ),
        None if !still_pinned.is_empty() => println!(
            "Still pinned in: {} (run `plugbay unuse {name}` there, or add it back)",
            still_pinned.join(", ")
        ),
        None => {}
    }
    notify_daemon_reload(paths).await;
    Ok(())
}

/// Which file a pin goes to, and the directory whose resolution it changes.
struct Destination {
    file: PathBuf,
    location: ProjectLocation,
}

impl Destination {
    fn choose(paths: &Paths, cd: Option<PathBuf>, options: &PinOptions) -> anyhow::Result<Self> {
        if options.global {
            return Ok(Self {
                file: paths.global_config(),
                location: ProjectLocation::global_only(),
            });
        }
        let location = location_here(paths, cd);
        let Some(directory) = location.config_dir.clone() else {
            bail!(
                "this is not a project directory (it is `/` or your home); \
                 run it inside a project, pass `-C <dir>`, or use `-g` for every directory"
            );
        };
        let file_name = if options.local {
            LOCAL_FILE
        } else {
            PROJECT_FILE
        };
        Ok(Self {
            file: directory.join(file_name),
            location,
        })
    }

    fn resolve(&self, paths: &Paths) -> anyhow::Result<ResolvedConfig> {
        config::resolve(paths, &self.location)
    }

    fn display(&self, home: &Path) -> String {
        display_path(&self.file, home)
    }
}

fn parse_targets(arguments: &[String]) -> anyhow::Result<Vec<PinTarget>> {
    let mut targets = Vec::new();
    for argument in arguments {
        let (name, account) = match argument.split_once(':') {
            Some((name, account)) => (name, Some(account.to_string())),
            None => (argument.as_str(), None),
        };
        let account_is_valid = account.as_deref().is_none_or(config::is_valid_name);
        if !config::is_valid_name(name) || !account_is_valid {
            bail!(
                "`{argument}`: server names and accounts use lowercase letters, digits and `-` only"
            );
        }
        targets.push(PinTarget {
            name: name.to_string(),
            account,
        });
    }
    Ok(targets)
}

/// `sentry:a sentry:b linear` → `sentry` with accounts a and b, `linear` with none.
fn group_by_server(targets: &[PinTarget]) -> anyhow::Result<Vec<ServerPin>> {
    let mut pins: Vec<ServerPin> = Vec::new();
    for target in targets {
        let Some(existing) = pins.iter_mut().find(|pin| pin.name == target.name) else {
            pins.push(ServerPin {
                name: target.name.clone(),
                accounts: target.account.iter().cloned().collect(),
            });
            continue;
        };
        let Some(account) = &target.account else {
            bail!(
                "{name} is given more than once; to use several of its accounts, name each one: \
                 `plugbay use {name}:<account> {name}:<other>`",
                name = target.name
            );
        };
        if existing.accounts.is_empty() {
            bail!(
                "{name} is given with and without an account; name every account to use several",
                name = target.name
            );
        }
        if !existing.accounts.contains(account) {
            existing.accounts.push(account.clone());
        }
    }
    Ok(pins)
}

/// Switches `server_pin` on. An entry that is a table keeps its settings (definition, tool
/// filters) and only gets its account and `enabled` changed; anything else becomes
/// `name = true`, `name = "account"` or `name = ["a", "b"]`.
fn pin(document: &mut DocumentMut, server_pin: &ServerPin) -> anyhow::Result<()> {
    let servers = servers_table(document)?;
    if let Some(definition) = servers
        .get_mut(&server_pin.name)
        .and_then(Item::as_table_like_mut)
    {
        definition.remove("enabled");
        match server_pin.accounts.as_slice() {
            [] => {}
            [account] => {
                definition.remove("accounts");
                definition.insert("account", toml_edit::value(account.as_str()));
            }
            accounts => {
                definition.remove("account");
                definition.insert("accounts", toml_edit::value(account_list(accounts)));
            }
        }
        definition.fmt();
        return Ok(());
    }
    let value = match server_pin.accounts.as_slice() {
        [] => toml_edit::value(true),
        [account] => toml_edit::value(account.as_str()),
        accounts => toml_edit::value(account_list(accounts)),
    };
    servers.insert(&server_pin.name, value);
    Ok(())
}

fn account_list(accounts: &[String]) -> toml_edit::Array {
    accounts.iter().map(String::as_str).collect()
}

/// ` (was acme)` when `use` changed which accounts of `name` resolve here, else empty.
fn replaced_accounts_note(before: &ResolvedConfig, after: &ResolvedConfig, name: &str) -> String {
    let accounts = |config: &ResolvedConfig| -> Vec<String> {
        let mut accounts: Vec<String> = config
            .server(name)
            .iter()
            .map(|server| server.account.clone())
            .collect();
        accounts.sort();
        accounts
    };
    let previous = accounts(before);
    if previous.is_empty() || previous == accounts(after) {
        return String::new();
    }
    format!(" (was {})", previous.join(", "))
}

/// Settings that only say "on, with this account": an entry holding nothing else is a plain pin.
const PIN_ONLY_KEYS: [&str; 3] = ["enabled", "account", "accounts"];

/// Removes `name`'s pin, but keeps a table with other settings (a definition, tool filters) by
/// switching it off instead, so `use` can bring it back as it was.
fn unpin(document: &mut DocumentMut, name: &str) -> anyhow::Result<()> {
    let servers = servers_table(document)?;
    let definition = servers
        .get_mut(name)
        .and_then(Item::as_table_like_mut)
        .filter(|entry| entry.iter().any(|(key, _)| !PIN_ONLY_KEYS.contains(&key)));
    match definition {
        Some(definition) => {
            definition.insert("enabled", toml_edit::value(false));
            definition.fmt();
        }
        None => {
            servers.remove(name);
        }
    }
    Ok(())
}

/// The `[servers]` table, created when missing.
fn servers_table(document: &mut DocumentMut) -> anyhow::Result<&mut dyn TableLike> {
    document
        .entry("servers")
        .or_insert_with(|| {
            // Implicit, so a file holding only `[servers.<name>]` tables gets no empty `[servers]`.
            let mut servers = Table::new();
            servers.set_implicit(true);
            Item::Table(servers)
        })
        .as_table_like_mut()
        .context("`servers` must be a table")
}

/// `[servers.<name>]` for `plugbay add`: a url or a command line, off until pinned.
fn definition_table(options: &AddOptions) -> anyhow::Result<Table> {
    let mut table = Table::new();
    match (&options.url, options.command.split_first()) {
        (Some(url), None) => {
            table.insert("url", toml_edit::value(url.as_str()));
        }
        (None, Some((command, args))) => {
            table.insert("command", toml_edit::value(command.as_str()));
            if !args.is_empty() {
                let args: toml_edit::Array = args.iter().map(String::as_str).collect();
                table.insert("args", toml_edit::value(args));
            }
            if !options.env.is_empty() {
                table.insert("env", toml_edit::value(env_table(&options.env)?));
            }
        }
        _ => bail!("give the server a `--url`, or its command line after `--`"),
    }
    table.insert("enabled", toml_edit::value(false));
    Ok(table)
}

fn env_table(assignments: &[String]) -> anyhow::Result<toml_edit::InlineTable> {
    let mut env = toml_edit::InlineTable::new();
    for assignment in assignments {
        let Some((name, value)) = assignment.split_once('=') else {
            bail!("`--env {assignment}`: expected NAME=value");
        };
        env.insert(name, Value::from(value));
    }
    Ok(env)
}

/// Resolves the new catalog entry as if a session pinned it, so a bad url or secret ref is caught now.
fn check_definition(paths: &Paths, name: &str) -> Result<(), String> {
    let pins = [(name.to_string(), config::SessionPin::On { account: None })].into();
    let config = config::resolve_with_pins(paths, &ProjectLocation::global_only(), &pins)
        .map_err(|error| format!("{error:#}"))?;
    if config.server(name).is_empty() {
        return Err(why_not_enabled(&config, name));
    }
    Ok(())
}

fn why_not_enabled(config: &ResolvedConfig, name: &str) -> String {
    if let Some(problem) = config
        .problems
        .iter()
        .find(|problem| problem.servers.iter().any(|server| server == name))
    {
        if problem.is_unknown_server() {
            return unknown_server(config, name);
        }
        return format!("{name}: {}", problem.message);
    }
    if let Some(disabled) = config.disabled.iter().find(|server| server.name == name) {
        return format!(
            "{name} is switched off by {}",
            disabled.disabled_by.display()
        );
    }
    format!("{name} did not resolve; run `plugbay why {name}`")
}

fn unknown_server(config: &ResolvedConfig, name: &str) -> String {
    let did_you_mean = match closest_known_name(config, name) {
        Some(closest) => format!("did you mean `{closest}`? Otherwise "),
        None => String::new(),
    };
    format!(
        "plugbay doesn't know how to reach {name}: {did_you_mean}add it first with \
         `plugbay add {name} --url <url>` or `plugbay add {name} -- <command>` \
         (`plugbay ls --available` lists what it knows)"
    )
}

/// Every server name plugbay could pin here: built-in, catalog, and anything config mentions.
fn known_names(config: &ResolvedConfig) -> Vec<&str> {
    let mut names: Vec<&str> = registry::all().iter().map(|entry| entry.name).collect();
    names.extend(config.catalog.iter().map(String::as_str));
    names.extend(config.servers.iter().map(|server| server.name.as_str()));
    names.extend(config.disabled.iter().map(|server| server.name.as_str()));
    names
}

/// The known name a typo most likely meant: `sentri` → `sentry`.
pub(super) fn closest_known_name<'a>(config: &'a ResolvedConfig, name: &str) -> Option<&'a str> {
    const SIMILAR_ENOUGH: f64 = 0.8;
    let mut best: Option<(&str, f64)> = None;
    for candidate in known_names(config) {
        let similarity = strsim::jaro_winkler(name, candidate);
        let better = best.is_none_or(|(_, best_similarity)| similarity > best_similarity);
        if similarity >= SIMILAR_ENOUGH && candidate != name && better {
            best = Some((candidate, similarity));
        }
    }
    best.map(|(candidate, _)| candidate)
}

/// Known but off here: built in, in the catalog, switched off, or mentioned in this file.
fn is_known(config: &ResolvedConfig, document: &DocumentMut, name: &str) -> bool {
    let in_this_file = document
        .get("servers")
        .and_then(Item::as_table_like)
        .is_some_and(|servers| servers.contains_key(name));
    in_this_file || known_names(config).contains(&name)
}

/// Refuses catalog entries that would quietly redirect pins, send traffic unencrypted, or
/// write a secret into the config file.
fn check_add_options(options: &AddOptions, global_config: &Path) -> anyhow::Result<()> {
    let name = &options.name;
    if let Some(entry) = registry::lookup(name)
        && !options.force
    {
        bail!(
            "{name} is built in ({}). A catalog entry named {name} would replace it in every \
             directory: existing `{name} = ...` pins would connect to your definition instead. \
             Pick another name (e.g. `{name}-self-hosted`), or pass `--force` if that is what you want",
            entry.url
        );
    }
    if let Some(url) = &options.url {
        check_url(url)?;
    }
    for assignment in &options.env {
        let Some((variable, value)) = assignment.split_once('=') else {
            continue;
        };
        if secrets::looks_secret(variable)
            && !value.is_empty()
            && !secrets::contains_secret_ref(value)
        {
            bail!(
                "`--env {variable}=...` would write a secret in plain text to {config}. \
                 Store it in your keychain (macOS: `security add-generic-password -s {name} -a {variable} -w`) \
                 and pass a reference instead: `--env '{variable}=${{keychain:{name}/{variable}}}'`. \
                 `${{op://vault/item/field}}`, `${{env:{variable}}}` and `${{fnox:{variable}}}` work too. \
                 If the value is not secret, add it to {config} by hand",
                config = global_config.display()
            );
        }
    }
    Ok(())
}

/// `https`, or plain `http` to this machine only: anything else would send tokens in the clear.
fn check_url(text: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(text).with_context(|| format!("`{text}` is not a valid url"))?;
    let is_loopback = match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback => Ok(()),
        "http" => bail!(
            "`{text}` is plain http to another machine, so logins and tokens would travel \
             unencrypted; use its https url (plain http is fine for localhost / 127.0.0.1)"
        ),
        scheme => {
            bail!("`{text}`: plugbay connects over https (or http to localhost), not `{scheme}`")
        }
    }
}

fn has_problem_for(config: &ResolvedConfig, name: &str) -> bool {
    config
        .problems
        .iter()
        .any(|problem| problem.servers.iter().any(|server| server == name))
}

fn read_or_empty(file: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", file.display())),
    }
}

fn parse(text: &str, file: &Path) -> anyhow::Result<DocumentMut> {
    text.parse()
        .with_context(|| format!("parsing {}", file.display()))
}

fn write(file: &Path, document: &DocumentMut) -> anyhow::Result<()> {
    std::fs::write(file, document.to_string())
        .with_context(|| format!("writing {}", file.display()))
}

/// Puts back what was there before, deleting a file this command created.
fn restore(file: &Path, original: &str) -> anyhow::Result<()> {
    if original.is_empty() {
        return std::fs::remove_file(file).with_context(|| format!("removing {}", file.display()));
    }
    std::fs::write(file, original).with_context(|| format!("restoring {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_pin(name: &str, accounts: &[&str]) -> ServerPin {
        ServerPin {
            name: name.to_string(),
            accounts: accounts.iter().map(|account| account.to_string()).collect(),
        }
    }

    fn add_options(name: &str, url: Option<&str>, env: &[&str]) -> AddOptions {
        AddOptions {
            name: name.to_string(),
            url: url.map(str::to_string),
            env: env
                .iter()
                .map(|assignment| assignment.to_string())
                .collect(),
            command: if url.is_none() {
                vec!["npx".to_string()]
            } else {
                Vec::new()
            },
            force: false,
        }
    }

    fn edited(text: &str, edit: impl FnOnce(&mut DocumentMut)) -> String {
        let mut document: DocumentMut = text.parse().unwrap();
        edit(&mut document);
        document.to_string()
    }

    #[test]
    fn pin_adds_a_short_entry_and_keeps_comments() {
        let text = "# work\naccount = \"initech\"\n\n[servers]\nlinear = true # tracker\n";
        let result = edited(text, |document| {
            pin(document, &server_pin("sentry", &["acme"])).unwrap();
            pin(document, &server_pin("vercel", &[])).unwrap();
        });
        assert_eq!(
            result,
            "# work\naccount = \"initech\"\n\n[servers]\nlinear = true # tracker\nsentry = \"acme\"\nvercel = true\n"
        );
    }

    #[test]
    fn a_definition_in_a_new_file_gets_no_empty_servers_header() {
        let options = AddOptions {
            name: "wa".to_string(),
            url: Some("http://127.0.0.1:7373/mcp".to_string()),
            env: Vec::new(),
            command: Vec::new(),
            force: false,
        };
        let result = edited("", |document| {
            let definition = definition_table(&options).unwrap();
            servers_table(document)
                .unwrap()
                .insert("wa", Item::Table(definition));
        });
        assert_eq!(
            result,
            "[servers.wa]\nurl = \"http://127.0.0.1:7373/mcp\"\nenabled = false\n"
        );
    }

    #[test]
    fn pin_creates_the_servers_table() {
        let result = edited("", |document| {
            pin(document, &server_pin("linear", &[])).unwrap()
        });
        assert_eq!(result, "[servers]\nlinear = true\n");
    }

    #[test]
    fn pin_switches_an_inline_definition_back_on() {
        let text = "[servers]\nwa = { url = \"http://127.0.0.1:7373/mcp\", enabled = false }\n";
        let result = edited(text, |document| {
            pin(document, &server_pin("wa", &["home"])).unwrap()
        });
        assert_eq!(
            result,
            "[servers]\nwa = { url = \"http://127.0.0.1:7373/mcp\", account = \"home\" }\n"
        );
    }

    #[test]
    fn pin_keeps_a_definition_and_sets_its_account() {
        let text = "[servers.teams]\ncommand = \"npx\"\nenabled = false\n";
        let result = edited(text, |document| {
            pin(document, &server_pin("teams", &["work"])).unwrap()
        });
        assert_eq!(
            result,
            "[servers.teams]\ncommand = \"npx\"\naccount = \"work\"\n"
        );
    }

    #[test]
    fn unpin_removes_short_entries_but_switches_definitions_off() {
        let text = "[servers]\nlinear = true\nwa = { url = \"http://127.0.0.1:7373/mcp\" }\n";
        let result = edited(text, |document| {
            unpin(document, "linear").unwrap();
            unpin(document, "wa").unwrap();
        });
        assert_eq!(
            result,
            "[servers]\nwa = { url = \"http://127.0.0.1:7373/mcp\", enabled = false }\n"
        );
    }

    #[test]
    fn definitions_hold_a_url_or_a_command_line_and_start_off() {
        let remote = AddOptions {
            name: "wa".to_string(),
            url: Some("http://127.0.0.1:7373/mcp".to_string()),
            env: Vec::new(),
            command: Vec::new(),
            force: false,
        };
        assert_eq!(
            definition_table(&remote).unwrap().to_string(),
            "url = \"http://127.0.0.1:7373/mcp\"\nenabled = false\n"
        );

        let stdio = AddOptions {
            name: "teams".to_string(),
            url: None,
            env: vec!["REGION=eu".to_string()],
            command: vec![
                "npx".to_string(),
                "-y".to_string(),
                "msteams-mcp".to_string(),
            ],
            force: false,
        };
        assert_eq!(
            definition_table(&stdio).unwrap().to_string(),
            "command = \"npx\"\nargs = [\"-y\", \"msteams-mcp\"]\nenv = { REGION = \"eu\" }\nenabled = false\n"
        );
    }

    #[test]
    fn two_accounts_of_one_server_become_the_list_form() {
        let targets = parse_targets(&[
            "sentry:a".to_string(),
            "linear".to_string(),
            "sentry:b".to_string(),
            "sentry:a".to_string(),
        ])
        .unwrap();
        let pins = group_by_server(&targets).unwrap();
        assert_eq!(
            pins,
            [server_pin("sentry", &["a", "b"]), server_pin("linear", &[])]
        );

        let result = edited("", |document| pin(document, &pins[0]).unwrap());
        assert_eq!(result, "[servers]\nsentry = [\"a\", \"b\"]\n");
    }

    #[test]
    fn several_accounts_replace_a_tables_single_account() {
        let text = "[servers]\nsentry = { account = \"a\", exclude_tools = [\"delete_*\"] }\n";
        let result = edited(text, |document| {
            pin(document, &server_pin("sentry", &["a", "b"])).unwrap()
        });
        assert_eq!(
            result,
            "[servers]\nsentry = { exclude_tools = [\"delete_*\"], accounts = [\"a\", \"b\"] }\n"
        );
    }

    #[test]
    fn a_server_with_and_without_an_account_is_refused() {
        let targets = parse_targets(&["sentry".to_string(), "sentry:b".to_string()]).unwrap();
        assert!(group_by_server(&targets).is_err());
    }

    #[test]
    fn unpin_keeps_a_table_with_tool_filters_switched_off() {
        let text = "[servers]\nposthog = { account = \"a\", exclude_tools = [\"delete_*\"] }\nsentry = { account = \"a\" }\n";
        let result = edited(text, |document| {
            unpin(document, "posthog").unwrap();
            unpin(document, "sentry").unwrap();
        });
        assert_eq!(
            result,
            "[servers]\nposthog = { account = \"a\", exclude_tools = [\"delete_*\"], enabled = false }\n"
        );

        let back_on = edited(&result, |document| {
            pin(document, &server_pin("posthog", &[])).unwrap()
        });
        assert_eq!(
            back_on,
            "[servers]\nposthog = { account = \"a\", exclude_tools = [\"delete_*\"] }\n"
        );
    }

    #[test]
    fn typos_suggest_the_closest_known_server() {
        let config = ResolvedConfig::test_with(Vec::new(), Vec::new());
        assert_eq!(closest_known_name(&config, "sentri"), Some("sentry"));
        assert_eq!(closest_known_name(&config, "zzz"), None);
        assert!(unknown_server(&config, "sentri").contains("did you mean `sentry`?"));
    }

    #[test]
    fn add_refuses_to_shadow_a_built_in_server_without_force() {
        let global = Path::new("/home/me/.config/plugbay/config.toml");
        let mut options = add_options("linear", Some("https://evil.example.com/mcp"), &[]);
        let error = check_add_options(&options, global).unwrap_err().to_string();
        assert!(error.contains("--force"), "{error}");

        options.force = true;
        assert!(check_add_options(&options, global).is_ok());
    }

    #[test]
    fn add_allows_plain_http_only_to_this_machine() {
        assert!(check_url("https://huggingface.co/mcp").is_ok());
        assert!(check_url("http://127.0.0.1:7373/mcp").is_ok());
        assert!(check_url("http://localhost:7373/mcp").is_ok());
        assert!(check_url("http://[::1]:7373/mcp").is_ok());
        assert!(check_url("http://mcp.example.com/mcp").is_err());
        assert!(check_url("ftp://127.0.0.1/mcp").is_err());
    }

    #[test]
    fn add_refuses_plain_text_secrets_in_env() {
        let global = Path::new("/home/me/.config/plugbay/config.toml");
        let plain = add_options("teams", None, &["API_KEY=test-token"]);
        let error = check_add_options(&plain, global).unwrap_err().to_string();
        assert!(error.contains("${keychain:teams/API_KEY}"), "{error}");
        assert!(!error.contains("test-token"), "{error}");

        let reference = add_options("teams", None, &["API_KEY=${op://vault/teams/key}"]);
        assert!(check_add_options(&reference, global).is_ok());
        let not_secret = add_options("teams", None, &["REGION=eu"]);
        assert!(check_add_options(&not_secret, global).is_ok());
    }

    #[test]
    fn targets_parse_names_and_accounts() {
        let targets = parse_targets(&["linear:initech".to_string(), "sentry".to_string()]).unwrap();
        assert_eq!(targets[0].name, "linear");
        assert_eq!(targets[0].account.as_deref(), Some("initech"));
        assert_eq!(targets[1].account, None);
        assert!(parse_targets(&["Linear".to_string()]).is_err());
    }
}
