//! `plugbay ls`, `ls --all`, `accounts` and `why`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::commands::pins::closest_known_name;
use crate::commands::{resolve_everywhere, resolve_here};
use crate::config::{
    self, ConfigScope, ResolvedConfig, ResolvedServer, ServerTransport, SessionPin,
};
use crate::credentials::{self, StoredLogin};
use crate::identity::{self, VerifiedAccounts};
use crate::paths::Paths;
use crate::project::ProjectLocation;
use crate::trust::Trust;
use crate::{registry, secrets};

/// `available`: list every server that could be pinned (built in and catalog) instead.
pub async fn ls(paths: &Paths, project: Option<PathBuf>, available: bool) -> anyhow::Result<()> {
    let config = resolve_here(paths, project).await?;
    if available {
        let catalog = catalog_definitions(paths, &config)?;
        print!("{}", format_available(&config, &catalog, &paths.home));
        return Ok(());
    }
    let statuses = server_statuses(paths, &config).await;
    print!("{}", format_ls(&config, &statuses, &paths.home));
    fail_on_problems(config.problems.len())
}

/// Each catalog entry with where it connects, resolved as if a session switched it on.
fn catalog_definitions(
    paths: &Paths,
    config: &ResolvedConfig,
) -> anyhow::Result<Vec<(String, String)>> {
    let pins: BTreeMap<String, SessionPin> = config
        .catalog
        .iter()
        .map(|name| (name.clone(), SessionPin::On { account: None }))
        .collect();
    let catalog = config::resolve_with_pins(paths, &ProjectLocation::global_only(), &pins)?;
    let mut definitions = Vec::new();
    for name in &config.catalog {
        let target = match catalog.server(name).first() {
            Some(server) => server.transport.target(),
            None => format!("does not resolve: run `plugbay why {name}`"),
        };
        definitions.push((name.clone(), target));
    }
    Ok(definitions)
}

fn format_available(config: &ResolvedConfig, catalog: &[(String, String)], home: &Path) -> String {
    let in_use_here = |name: &str| {
        if config.server(name).is_empty() {
            String::new()
        } else {
            "in use here".to_string()
        }
    };
    let mut output = String::from("Built in\n");
    let mut rows = Vec::new();
    for entry in registry::all() {
        let replaced = catalog.iter().any(|(name, _)| name == entry.name);
        let note = if replaced {
            "replaced by your catalog".to_string()
        } else {
            in_use_here(entry.name)
        };
        rows.push(vec![
            entry.name.to_string(),
            entry.title.to_string(),
            entry.url.to_string(),
            note,
        ]);
    }
    output.push_str(&render_rows(&rows));

    let global_config = config
        .files
        .iter()
        .find(|file| file.scope == ConfigScope::Global)
        .map_or("the global config".to_string(), |file| {
            display_path(&file.path, home)
        });
    output.push_str(&format!("\nYour catalog ({global_config})\n"));
    if catalog.is_empty() {
        output.push_str(
            "  empty: `plugbay add <name> --url <url>` or `plugbay add <name> -- <command>` adds one\n",
        );
    } else {
        let rows: Vec<Vec<String>> = catalog
            .iter()
            .map(|(name, target)| vec![name.clone(), target.clone(), in_use_here(name)])
            .collect();
        output.push_str(&render_rows(&rows));
    }
    output
        .push_str("\nPin one here with `plugbay use <name>` or `plugbay use <name>:<account>`.\n");
    output
}

/// Every directory plugbay has seen, each with what resolves there.
pub async fn ls_all(paths: &Paths, cd: Option<PathBuf>) -> anyhow::Result<()> {
    resolve_here(paths, cd).await?;
    let configs = resolve_everywhere(paths)?;
    let mut output = String::new();
    for config in &configs {
        let heading = match &config.location.config_dir {
            Some(directory) => display_path(directory, &paths.home),
            None => "everywhere (global config)".to_string(),
        };
        let statuses = server_statuses(paths, config).await;
        output.push_str(&format!("{heading}\n"));
        output.push_str(&format_server_summary(config, &statuses));
        output.push('\n');
    }
    if configs.len() == 1 {
        output.push_str(
            "No project directories yet: plugbay learns them as agents and commands run in them.\n",
        );
    }
    print!("{output}");
    Ok(())
}

/// One row per server account in use anywhere: login state and the directories that pin it.
pub async fn accounts(paths: &Paths, cd: Option<PathBuf>) -> anyhow::Result<()> {
    resolve_here(paths, cd).await?;
    let configs = resolve_everywhere(paths)?;

    let mut usages: BTreeMap<(String, String, String), AccountUsage> = BTreeMap::new();
    for config in &configs {
        for server in &config.servers {
            let key = (
                server.name.clone(),
                server.account.clone(),
                server.identity.to_string(),
            );
            let usage = usages.entry(key).or_insert_with(|| AccountUsage {
                server: server.clone(),
                directories: Vec::new(),
            });
            let directory = config.location.config_dir.clone();
            usage.directories.push(directory);
        }
    }
    if usages.is_empty() {
        println!("No server accounts in use yet. Pin one with `plugbay use <server>:<account>`.");
        return Ok(());
    }

    let verified = VerifiedAccounts::load(paths).await;
    let mut rows = vec![vec![
        "SERVER".to_string(),
        "ACCOUNT".to_string(),
        "LOGIN".to_string(),
        "VERIFIED AS".to_string(),
        "USED IN".to_string(),
    ]];
    for usage in usages.values() {
        rows.push(vec![
            usage.server.name.clone(),
            usage.server.account.clone(),
            login_label(login_status(&usage.server).await).to_string(),
            verified_label(&usage.server, &verified),
            usage.used_in(&paths.home),
        ]);
    }
    print!("{}", render_rows(&rows));
    Ok(())
}

struct AccountUsage {
    server: ResolvedServer,
    /// `None` stands for the global config: every directory.
    directories: Vec<Option<PathBuf>>,
}

impl AccountUsage {
    /// The topmost directories only: a subdirectory that merely inherits the pin adds nothing.
    fn used_in(&self, home: &Path) -> String {
        if self.directories.contains(&None) {
            return "everywhere".to_string();
        }
        let directories: Vec<&PathBuf> = self.directories.iter().flatten().collect();
        let topmost: Vec<String> = directories
            .iter()
            .filter(|directory| {
                !directories
                    .iter()
                    .any(|other| other != *directory && directory.starts_with(other))
            })
            .map(|directory| display_path(directory, home))
            .collect();
        topmost.join(", ")
    }
}

/// What the server says the login is, flagged when another label of the server is the same login.
fn verified_label(server: &ResolvedServer, verified: &VerifiedAccounts) -> String {
    let Some(shown) = verified.shown(&server.identity, &server.account) else {
        return "—".to_string();
    };
    let same_login = verified.same_login_as(&server.identity, &server.account);
    if same_login.is_empty() {
        return shown.to_string();
    }
    let labels: Vec<String> = same_login
        .iter()
        .map(|account| format!("{}:{account}", server.name))
        .collect();
    format!("{shown} (same login as {})", labels.join(", "))
}

fn login_label(status: LoginStatus) -> &'static str {
    match status {
        LoginStatus::NotNeeded => "not needed",
        LoginStatus::LoggedIn => "logged in",
        LoginStatus::LoggedOut => "missing",
        LoginStatus::Unknown => "unknown",
    }
}

pub async fn why(paths: &Paths, server: &str, project: Option<PathBuf>) -> anyhow::Result<()> {
    let config = resolve_here(paths, project).await?;
    print!("{}", format_why(&config, server, &paths.home)?);
    let problem_count = config
        .problems
        .iter()
        .filter(|problem| problem.servers.iter().any(|name| name == server))
        .count();
    fail_on_problems(problem_count)
}

/// Problems are printed with everything else; failing afterwards lets scripts and agents notice.
fn fail_on_problems(problem_count: usize) -> anyhow::Result<()> {
    match problem_count {
        0 => Ok(()),
        1 => bail!("1 config problem, listed above"),
        count => bail!("{count} config problems, listed above"),
    }
}

/// What `ls` checks beyond the config itself; one per `ResolvedConfig::servers` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerStatus {
    needs_trust: bool,
    login: LoginStatus,
    /// Who the server said the login is, when plugbay asked it.
    verified: Option<String>,
    /// Set when the verified login doesn't match the account's `expect`.
    wrong_account: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginStatus {
    /// Stdio servers and remote servers with static auth headers.
    NotNeeded,
    LoggedIn,
    LoggedOut,
    /// The keychain could not be read.
    Unknown,
}

async fn server_statuses(paths: &Paths, config: &ResolvedConfig) -> Vec<ServerStatus> {
    let trust = Trust::new(paths);
    let verified = VerifiedAccounts::load(paths).await;
    let mut statuses = Vec::new();
    for server in &config.servers {
        let needs_trust =
            !server.requires_trust_from.is_empty() && !trust.is_server_trusted(server, config);
        statuses.push(ServerStatus {
            needs_trust,
            login: login_status(server).await,
            verified: verified
                .shown(&server.identity, &server.account)
                .map(str::to_string),
            wrong_account: identity::expectation_failure(server, &verified),
        });
    }
    statuses
}

async fn login_status(server: &ResolvedServer) -> LoginStatus {
    if !server.uses_stored_login() {
        return LoginStatus::NotNeeded;
    }
    match credentials::stored_login(&server.identity, &server.account).await {
        Ok(StoredLogin::OAuth | StoredLogin::Token) => LoginStatus::LoggedIn,
        Ok(StoredLogin::None) => LoginStatus::LoggedOut,
        Err(error) => {
            tracing::warn!(%error, server = %server.display_name(), "could not read stored credentials");
            LoginStatus::Unknown
        }
    }
}

/// `ls --all`'s compact form: name and status per server, then problems.
fn format_server_summary(config: &ResolvedConfig, statuses: &[ServerStatus]) -> String {
    let mut rows = Vec::new();
    for (server, status) in config.servers.iter().zip(statuses) {
        rows.push(vec![server.display_name(), status_label(server, status)]);
    }
    let mut output = if rows.is_empty() {
        "  no servers\n".to_string()
    } else {
        render_rows(&rows)
    };
    for problem in &config.problems {
        output.push_str(&format!("  problem: {}\n", problem.message));
    }
    output
}

fn format_ls(config: &ResolvedConfig, statuses: &[ServerStatus], home: &Path) -> String {
    let mut output = String::new();
    output.push_str(&format_location(config, home));

    output.push_str("\nConfig files (merge order)\n");
    if config.files.is_empty() {
        output.push_str("  none found\n");
    } else {
        let rows: Vec<Vec<String>> = config
            .files
            .iter()
            .map(|file| {
                vec![
                    scope_label(file.scope).to_string(),
                    display_path(&file.path, home),
                ]
            })
            .collect();
        output.push_str(&render_rows(&rows));
    }

    output.push_str("\nServers\n");
    if config.servers.is_empty() {
        output.push_str(
            "  none yet: `plugbay ls --available` lists the servers you can pin with `plugbay use`\n",
        );
    } else {
        let mut rows = vec![vec![
            "SERVER".to_string(),
            "TRANSPORT".to_string(),
            "TARGET".to_string(),
            "TOOLS".to_string(),
            "STATUS".to_string(),
        ]];
        for (server, status) in config.servers.iter().zip(statuses) {
            rows.push(vec![
                server.display_name(),
                transport_label(&server.transport).to_string(),
                server.transport.target(),
                tools_label(server),
                status_label(server, status),
            ]);
        }
        output.push_str(&render_rows(&rows));
    }

    if !config.disabled.is_empty() {
        output.push_str("\nDisabled\n");
        let rows: Vec<Vec<String>> = config
            .disabled
            .iter()
            .map(|disabled| {
                vec![
                    disabled.name.clone(),
                    format!("by {}", display_path(&disabled.disabled_by, home)),
                ]
            })
            .collect();
        output.push_str(&render_rows(&rows));
    }

    if !config.catalog.is_empty() {
        output.push_str("\nAvailable from the catalog (`plugbay use <name>`)\n");
        output.push_str(&format!("  {}\n", config.catalog.join(", ")));
    }

    if !config.problems.is_empty() {
        output.push_str("\nProblems\n");
        for problem in &config.problems {
            output.push_str(&format!("  - {}\n", problem.message));
        }
    }
    output
}

fn format_location(config: &ResolvedConfig, home: &Path) -> String {
    let Some(config_dir) = &config.location.config_dir else {
        return "Project      none (only the global config applies)\n".to_string();
    };
    let mut output = format!("Project      {}\n", display_path(config_dir, home));
    if let Some(working_dir) = &config.location.working_dir
        && working_dir != config_dir
    {
        output.push_str(&format!(
            "Working dir  {}\n",
            display_path(working_dir, home)
        ));
    }
    output
}

fn scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Global => "global",
        ConfigScope::Project => "project",
        ConfigScope::Local => "local",
    }
}

fn transport_label(transport: &ServerTransport) -> &'static str {
    match transport {
        ServerTransport::Http { .. } => "http",
        ServerTransport::Stdio { .. } => "stdio",
    }
}

fn tools_label(server: &ResolvedServer) -> String {
    let filter = &server.tool_filter;
    if filter.include.is_empty() && filter.exclude.is_empty() {
        format!("{}__*", server.tool_prefix)
    } else {
        format!("{}__* (filtered)", server.tool_prefix)
    }
}

fn status_label(server: &ResolvedServer, status: &ServerStatus) -> String {
    let mut notes = Vec::new();
    if status.needs_trust {
        notes.push("needs trust: run `plugbay trust`".to_string());
    }
    match status.login {
        LoginStatus::LoggedOut => notes.push(format!(
            "needs login: run `plugbay login {}` (add `--token` for an API token)",
            server.display_name()
        )),
        LoginStatus::Unknown => notes.push("login status unknown".to_string()),
        LoginStatus::NotNeeded | LoginStatus::LoggedIn => {}
    }
    if let Some(wrong_account) = &status.wrong_account {
        notes.push(format!("off: {wrong_account}"));
    }
    if !notes.is_empty() {
        return notes.join("; ");
    }
    match &status.verified {
        Some(shown) => format!("ready · {shown}"),
        None => "ready".to_string(),
    }
}

fn format_why(config: &ResolvedConfig, name: &str, home: &Path) -> anyhow::Result<String> {
    let mut output = String::new();

    for server in config.server(name) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format_server_fields(server, home));
    }
    for disabled in config
        .disabled
        .iter()
        .filter(|disabled| disabled.name == name)
    {
        output.push_str(&format!(
            "{name} is disabled by {}\n",
            display_path(&disabled.disabled_by, home)
        ));
    }
    for problem in &config.problems {
        if problem.servers.iter().any(|server| server == name) {
            output.push_str(&format!("{name} is left out: {}\n", problem.message));
        }
    }

    if output.is_empty() {
        if let Some(closest) = closest_known_name(config, name) {
            bail!("no server named `{name}` is configured here; did you mean `{closest}`?");
        }
        bail!("no server named `{name}` is configured here; `plugbay ls` lists them");
    }
    Ok(output)
}

fn format_server_fields(server: &ResolvedServer, home: &Path) -> String {
    let mut output = format!("{}\n", server.display_name());
    output.push_str(&format!("  identity  {}\n", server.identity));
    output.push_str(&format!("  tools     {}__*\n\n", server.tool_prefix));

    let mut rows = vec![vec![
        "FIELD".to_string(),
        "VALUE".to_string(),
        "SET BY".to_string(),
    ]];
    let mut fields: Vec<(&str, String)> = server
        .provenance
        .iter()
        .map(|(field, source)| (field.as_str(), display_path(source, home)))
        .collect();
    if !server.provenance.contains_key("account") {
        fields.push(("account", "built-in default".to_string()));
    }
    fields.sort_by_key(|(field, _)| (field_rank(field), *field));
    for (field, source) in fields {
        rows.push(vec![field.to_string(), field_value(server, field), source]);
    }
    output.push_str(&render_rows(&rows));

    if !server.requires_trust_from.is_empty() {
        output.push_str("\n  needs trust from\n");
        for path in &server.requires_trust_from {
            output.push_str(&format!("    {}\n", display_path(path, home)));
        }
    }
    output
}

/// Orders `why` rows the way a server is read: on/off, who, where, how, then filters.
fn field_rank(field: &str) -> u8 {
    let group = field.split('.').next().unwrap_or(field);
    match group {
        "enabled" => 0,
        "account" => 1,
        "url" => 2,
        "headers" => 3,
        "command" => 4,
        "args" => 5,
        "env" => 6,
        "cwd" => 7,
        "tools" => 8,
        "exclude_tools" => 9,
        "metadata" => 10,
        _ => 11,
    }
}

fn field_value(server: &ResolvedServer, field: &str) -> String {
    if let Some(name) = field.strip_prefix("metadata.") {
        return keyed_value(name, server.account_metadata.get(name));
    }

    match (&server.transport, field) {
        (_, "enabled") => "true".to_string(),
        (_, "account") => server.account.clone(),
        (_, "tools") => format!("{:?}", server.tool_filter.include),
        (_, "exclude_tools") => format!("{:?}", server.tool_filter.exclude),
        (ServerTransport::Http { url, .. }, "url") => url.to_string(),
        (ServerTransport::Http { headers, .. }, _) => match field.strip_prefix("headers.") {
            Some(name) => keyed_value(name, headers.get(name)),
            None => String::new(),
        },
        (ServerTransport::Stdio { command, .. }, "command") => command.clone(),
        (ServerTransport::Stdio { args, .. }, "args") => format!("{args:?}"),
        (ServerTransport::Stdio { cwd, .. }, "cwd") => cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string())
            .unwrap_or_default(),
        (ServerTransport::Stdio { env, .. }, _) => match field.strip_prefix("env.") {
            Some(name) => keyed_value(name, env.get(name)),
            None => String::new(),
        },
    }
}

/// Secret refs are pointers and safe to show; a literal value under a secret-looking name is not.
fn keyed_value(name: &str, value: Option<&String>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if !secrets::contains_secret_ref(value) && secrets::looks_secret(name) {
        return secrets::redact(value);
    }
    value.clone()
}

pub(super) fn display_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Left-aligned columns separated by two spaces, indented by two.
fn render_rows(rows: &[Vec<String>]) -> String {
    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; column_count];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut output = String::new();
    for row in rows {
        let padded_cells: Vec<String> = widths
            .iter()
            .zip(row)
            .map(|(width, cell)| format!("{cell:<width$}"))
            .collect();
        let line = format!("  {}", padded_cells.join("  "));
        output.push_str(line.trim_end());
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{ConfigFile, ConfigProblem, DisabledServer, REGISTRY_SOURCE, ToolFilter};
    use crate::project::ProjectLocation;

    const HOME: &str = "/home/me";
    const GLOBAL_FILE: &str = "/home/me/.config/plugbay/config.toml";
    const CLIENT_FILE: &str = "/home/me/work/client-a/plugbay.toml";
    const APP_FILE: &str = "/home/me/work/client-a/app/plugbay.toml";
    const LOCAL_FILE: &str = "/home/me/work/client-a/app/plugbay.local.toml";

    fn sourced(fields: &[(&str, &str)]) -> BTreeMap<String, PathBuf> {
        fields
            .iter()
            .map(|(field, file)| (field.to_string(), PathBuf::from(file)))
            .collect()
    }

    fn posthog() -> ResolvedServer {
        let mut server =
            ResolvedServer::test_http("posthog", "client-a", "https://mcp-eu.posthog.com/mcp");
        if let ServerTransport::Http { headers, .. } = &mut server.transport {
            headers.insert(
                "authorization".to_string(),
                "Bearer ${op://c/posthog/key}".to_string(),
            );
            headers.insert("x-api-key".to_string(), "literal-test-token".to_string());
        }
        server.tool_filter = ToolFilter::new(Vec::new(), vec!["delete_*".to_string()]);
        server.account_metadata = BTreeMap::from([("region".to_string(), "eu".to_string())]);
        server.provenance = sourced(&[
            ("enabled", CLIENT_FILE),
            ("account", CLIENT_FILE),
            ("url", CLIENT_FILE),
            ("headers.authorization", CLIENT_FILE),
            ("headers.x-api-key", CLIENT_FILE),
            ("exclude_tools", APP_FILE),
            ("metadata.region", CLIENT_FILE),
        ]);
        server.requires_trust_from = vec![PathBuf::from(CLIENT_FILE)];
        server
    }

    fn linear() -> ResolvedServer {
        let mut server =
            ResolvedServer::test_http("linear", "default", "https://mcp.linear.app/mcp");
        server.provenance = sourced(&[("enabled", GLOBAL_FILE), ("url", REGISTRY_SOURCE)]);
        server
    }

    fn tool() -> ResolvedServer {
        let mut server = ResolvedServer::test_stdio(
            "tool",
            "default",
            "/home/me/bin/tool",
            &["serve", "--name", "my app"],
        );
        if let ServerTransport::Stdio { env, cwd, .. } = &mut server.transport {
            env.insert("MODE".to_string(), "dev".to_string());
            *cwd = Some(PathBuf::from("/home/me/work/client-a/app/scripts"));
        }
        server.identity = crate::config::ServerIdentity("stdio:abc123".to_string());
        server.provenance = sourced(&[
            ("enabled", LOCAL_FILE),
            ("command", LOCAL_FILE),
            ("args", LOCAL_FILE),
            ("env.MODE", LOCAL_FILE),
            ("cwd", LOCAL_FILE),
        ]);
        server.requires_trust_from = vec![PathBuf::from(LOCAL_FILE)];
        server
    }

    fn sample_config() -> ResolvedConfig {
        let files = vec![
            ConfigFile::test(GLOBAL_FILE, ConfigScope::Global, None),
            ConfigFile::test("/home/me/work/plugbay.toml", ConfigScope::Project, None),
            ConfigFile::test(CLIENT_FILE, ConfigScope::Project, Some("abc")),
            ConfigFile::test(APP_FILE, ConfigScope::Project, None),
            ConfigFile::test(LOCAL_FILE, ConfigScope::Local, Some("def")),
        ];
        let mut config = ResolvedConfig::test_with(files, vec![linear(), posthog(), tool()]);
        config.location = ProjectLocation {
            config_dir: Some(PathBuf::from("/home/me/work/client-a/app")),
            working_dir: Some(PathBuf::from("/home/me/.t3/worktrees/app/feature")),
        };
        config.disabled = vec![DisabledServer {
            name: "notion".to_string(),
            disabled_by: PathBuf::from(CLIENT_FILE),
        }];
        config.problems = vec![ConfigProblem {
            servers: vec!["mystery".to_string()],
            message: "unknown server `mystery`: it is not in the bundled registry, so set `url` or `command`".to_string(),
        }];
        config
    }

    fn sample_statuses() -> Vec<ServerStatus> {
        vec![
            ServerStatus {
                needs_trust: false,
                login: LoginStatus::LoggedOut,
                verified: None,
                wrong_account: None,
            },
            ServerStatus {
                needs_trust: false,
                login: LoginStatus::NotNeeded,
                verified: Some("Client A EU".to_string()),
                wrong_account: None,
            },
            ServerStatus {
                needs_trust: true,
                login: LoginStatus::NotNeeded,
                verified: None,
                wrong_account: None,
            },
        ]
    }

    #[test]
    fn ls_shows_servers_files_disabled_and_problems() {
        let output = format_ls(&sample_config(), &sample_statuses(), Path::new(HOME));
        insta::assert_snapshot!(output);
    }

    #[test]
    fn ls_without_project_says_only_global_applies() {
        let config = ResolvedConfig::test_with(Vec::new(), Vec::new());
        insta::assert_snapshot!(format_ls(&config, &[], Path::new(HOME)));
    }

    #[test]
    fn available_lists_built_in_and_catalog_servers() {
        let mut config = sample_config();
        config.catalog = vec!["playwright".to_string(), "linear".to_string()];
        let catalog = vec![
            (
                "playwright".to_string(),
                "npx -y @playwright/mcp".to_string(),
            ),
            (
                "linear".to_string(),
                "https://linear.example.test/mcp".to_string(),
            ),
        ];
        insta::assert_snapshot!(format_available(&config, &catalog, Path::new(HOME)));
    }

    #[test]
    fn available_explains_an_empty_catalog() {
        let config = ResolvedConfig::test_with(Vec::new(), Vec::new());
        let output = format_available(&config, &[], Path::new(HOME));
        assert!(output.contains("Your catalog (the global config)\n  empty: `plugbay add"));
    }

    #[test]
    fn why_suggests_the_server_a_typo_meant() {
        let error = format_why(&sample_config(), "posthgo", Path::new(HOME))
            .unwrap_err()
            .to_string();
        assert!(error.ends_with("did you mean `posthog`?"), "{error}");
    }

    #[test]
    fn why_shows_each_field_and_the_file_that_set_it() {
        let output = format_why(&sample_config(), "posthog", Path::new(HOME)).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn why_shows_stdio_fields_and_default_account() {
        let output = format_why(&sample_config(), "tool", Path::new(HOME)).unwrap();
        insta::assert_snapshot!(output);
    }

    #[test]
    fn why_redacts_literal_secret_values_but_shows_refs() {
        let output = format_why(&sample_config(), "posthog", Path::new(HOME)).unwrap();
        assert!(!output.contains("literal-test-token"));
        assert!(output.contains("${op://c/posthog/key}"));
    }

    #[test]
    fn why_explains_disabled_server() {
        let output = format_why(&sample_config(), "notion", Path::new(HOME)).unwrap();
        assert_eq!(
            output,
            "notion is disabled by ~/work/client-a/plugbay.toml\n"
        );
    }

    #[test]
    fn why_explains_excluded_server() {
        let output = format_why(&sample_config(), "mystery", Path::new(HOME)).unwrap();
        assert!(output.starts_with("mystery is left out: unknown server"));
    }

    #[test]
    fn why_fails_for_unknown_server() {
        assert!(format_why(&sample_config(), "nothing", Path::new(HOME)).is_err());
    }

    #[test]
    fn wrong_account_turns_the_status_off() {
        let status = ServerStatus {
            needs_trust: false,
            login: LoginStatus::LoggedIn,
            verified: Some("Initech".to_string()),
            wrong_account: Some("linear:default is logged in as Initech".to_string()),
        };
        assert_eq!(
            status_label(&linear(), &status),
            "off: linear:default is logged in as Initech"
        );
    }

    #[test]
    fn verified_label_flags_labels_sharing_one_login() {
        let verified = VerifiedAccounts::test_with(&[
            ("https://mcp.linear.app/mcp", "default", "Initech"),
            ("https://mcp.linear.app/mcp", "initech", "Initech"),
        ]);

        assert_eq!(
            verified_label(&linear(), &verified),
            "Initech (same login as linear:initech)"
        );
        assert_eq!(verified_label(&posthog(), &verified), "—");
    }

    #[test]
    fn problems_make_the_command_fail() {
        assert!(fail_on_problems(0).is_ok());
        let error = fail_on_problems(2).unwrap_err().to_string();
        assert_eq!(error, "2 config problems, listed above");
    }
}
