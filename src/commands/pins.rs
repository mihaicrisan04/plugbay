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
    /// Name to pin it by, e.g. `wa`.
    name: String,
    /// Streamable HTTP endpoint of a remote server.
    #[arg(long, conflicts_with = "command")]
    url: Option<String>,
    /// Environment for a stdio server, `NAME=value`; values may be secret refs like `${op://...}`.
    #[arg(long = "env", value_name = "NAME=VALUE", requires = "command")]
    env: Vec<String>,
    /// Command line of a stdio server, after `--`: `plugbay add teams -- npx -y msteams-mcp`.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

/// One `server[:account]` argument.
struct PinTarget {
    name: String,
    account: Option<String>,
}

pub async fn use_servers(
    paths: &Paths,
    cd: Option<PathBuf>,
    options: PinOptions,
) -> anyhow::Result<()> {
    let targets = parse_targets(&options.servers)?;
    let destination = Destination::choose(paths, cd, &options)?;
    let original = read_or_empty(&destination.file)?;
    let mut document = parse(&original, &destination.file)?;

    for target in &targets {
        pin(&mut document, target)?;
    }
    write(&destination.file, &document)?;

    let config = destination.resolve(paths)?;
    index::record(paths, &config).await;
    let missing: Vec<String> = targets
        .iter()
        .filter(|target| config.server(&target.name).is_empty())
        .map(|target| why_not_enabled(&config, &target.name))
        .collect();
    if !missing.is_empty() {
        restore(&destination.file, &original)?;
        bail!("nothing written:\n  {}", missing.join("\n  "));
    }

    for target in &targets {
        let enabled: Vec<String> = config
            .server(&target.name)
            .iter()
            .map(|server| server.display_name())
            .collect();
        println!(
            "Using {} in {}",
            enabled.join(", "),
            destination.display(&paths.home)
        );
    }
    notify_daemon_reload(paths).await;
    Ok(())
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
    let mut document = parse(&read_or_empty(&destination.file)?, &destination.file)?;

    for target in &targets {
        unpin(&mut document, &target.name)?;
    }
    write(&destination.file, &document)?;

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

    for target in &targets {
        let inherited = still_enabled
            .iter()
            .any(|enabled| enabled.name == target.name);
        let note = if inherited {
            " (inherited, so it is now switched off here)"
        } else {
            ""
        };
        println!(
            "Not using {} in {}{note}",
            target.name,
            destination.display(&paths.home)
        );
    }
    notify_daemon_reload(paths).await;
    Ok(())
}

pub async fn add(paths: &Paths, options: AddOptions) -> anyhow::Result<()> {
    if !config::is_valid_name(&options.name) {
        bail!("server names use lowercase letters, digits and `-` only");
    }
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
    if servers_table(&mut document)?.remove(name).is_none() {
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
    if !still_pinned.is_empty() {
        println!(
            "Still pinned in: {} (run `plugbay unuse {name}` there, or add it back)",
            still_pinned.join(", ")
        );
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

/// Switches `target` on. An entry that defines the server keeps its definition and only gets
/// its account (and `enabled`) changed; anything else becomes `name = "account"` or `name = true`.
fn pin(document: &mut DocumentMut, target: &PinTarget) -> anyhow::Result<()> {
    let servers = servers_table(document)?;
    if let Some(definition) = servers
        .get_mut(&target.name)
        .and_then(Item::as_table_like_mut)
    {
        definition.remove("enabled");
        if let Some(account) = &target.account {
            definition.remove("accounts");
            definition.insert("account", toml_edit::value(account.as_str()));
        }
        definition.fmt();
        return Ok(());
    }
    let value = match &target.account {
        Some(account) => toml_edit::value(account.as_str()),
        None => toml_edit::value(true),
    };
    servers.insert(&target.name, value);
    Ok(())
}

/// Removes `name`'s pin, but keeps a definition (url or command) by switching it off instead.
fn unpin(document: &mut DocumentMut, name: &str) -> anyhow::Result<()> {
    let servers = servers_table(document)?;
    let definition = servers
        .get_mut(name)
        .and_then(Item::as_table_like_mut)
        .filter(|entry| entry.contains_key("url") || entry.contains_key("command"));
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
            return format!(
                "plugbay doesn't know how to reach {name}: add it first with \
                 `plugbay add {name} --url <url>` or `plugbay add {name} -- <command>`"
            );
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

    fn target(name: &str, account: Option<&str>) -> PinTarget {
        PinTarget {
            name: name.to_string(),
            account: account.map(str::to_string),
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
            pin(document, &target("sentry", Some("acme"))).unwrap();
            pin(document, &target("vercel", None)).unwrap();
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
            pin(document, &target("linear", None)).unwrap()
        });
        assert_eq!(result, "[servers]\nlinear = true\n");
    }

    #[test]
    fn pin_switches_an_inline_definition_back_on() {
        let text = "[servers]\nwa = { url = \"http://127.0.0.1:7373/mcp\", enabled = false }\n";
        let result = edited(text, |document| {
            pin(document, &target("wa", Some("home"))).unwrap()
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
            pin(document, &target("teams", Some("work"))).unwrap()
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
        };
        assert_eq!(
            definition_table(&stdio).unwrap().to_string(),
            "command = \"npx\"\nargs = [\"-y\", \"msteams-mcp\"]\nenv = { REGION = \"eu\" }\nenabled = false\n"
        );
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
