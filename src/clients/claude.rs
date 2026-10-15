//! Claude Code: `~/.claude.json` (user and local scope), `.mcp.json` (project scope) and the `claude mcp` CLI.
//!
//! `~/.claude.json` is large and rewritten by every running session, so plugbay only reads it;
//! changes go through `claude mcp`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use super::{
    NativeServer, NativeSource, NativeTransport, PLUGBAY_ENTRY, SetupState, ShimCommand,
    read_optional,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeScope {
    /// Top-level `mcpServers` in `~/.claude.json`: every project.
    User,
    /// `projects["<dir>"].mcpServers` in `~/.claude.json`: one project, this machine.
    Local,
    /// `.mcp.json` in the project: committed.
    Project,
}

impl ClaudeScope {
    /// The value `claude mcp --scope` takes.
    pub fn cli_name(self) -> &'static str {
        match self {
            ClaudeScope::User => "user",
            ClaudeScope::Local => "local",
            ClaudeScope::Project => "project",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeConfig {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    projects: BTreeMap<String, ClaudeProject>,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeProject {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
    /// User- and local-scope servers switched off in this directory (`/mcp` → disable).
    #[serde(default, rename = "disabledMcpServers")]
    disabled_servers: Vec<String>,
    /// `.mcp.json` servers the human declined.
    #[serde(default, rename = "disabledMcpjsonServers")]
    disabled_project_servers: Vec<String>,
}

/// One directory Claude Code has run in, with the MCP choices made there.
#[derive(Debug)]
pub(crate) struct ClaudeProjectState {
    pub directory: PathBuf,
    /// Local-scope servers, then `.mcp.json` ones.
    pub servers: Vec<NativeServer>,
    pub disabled_servers: Vec<String>,
    pub disabled_project_servers: Vec<String>,
}

/// One `mcpServers` entry. Stdio entries may omit `type`.
#[derive(Debug, Deserialize)]
struct ClaudeServerEntry {
    #[serde(rename = "type")]
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(rename = "headersHelper")]
    headers_helper: Option<String>,
}

/// `~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json` when Claude Code is relocated.
pub(crate) fn config_file(home: &Path) -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(config_dir) => PathBuf::from(config_dir).join(".claude.json"),
        None => home.join(".claude.json"),
    }
}

pub(crate) fn read_user_servers(home: &Path) -> anyhow::Result<Vec<NativeServer>> {
    let Some(config) = read_config(&config_file(home))? else {
        return Ok(Vec::new());
    };
    Ok(native_servers(config.mcp_servers, ClaudeScope::User))
}

/// Local-scope servers of `project_dir` from `~/.claude.json`, then the ones in its `.mcp.json`.
pub(crate) fn read_project_servers(
    home: &Path,
    project_dir: &Path,
) -> anyhow::Result<Vec<NativeServer>> {
    let mut servers = Vec::new();
    if let Some(config) = read_config(&config_file(home))? {
        servers.extend(local_servers(config.projects, project_dir));
    }
    if let Some(config) = read_config(&project_dir.join(".mcp.json"))? {
        servers.extend(native_servers(config.mcp_servers, ClaudeScope::Project));
    }
    Ok(servers)
}

/// Every directory Claude Code knows that still exists, with its servers and disable lists.
pub(crate) fn read_projects(home: &Path) -> anyhow::Result<Vec<ClaudeProjectState>> {
    let Some(config) = read_config(&config_file(home))? else {
        return Ok(Vec::new());
    };
    let mut projects = Vec::new();
    for (key, project) in config.projects {
        let directory = PathBuf::from(key);
        if !directory.is_dir() {
            continue;
        }
        let mut servers = native_servers(project.mcp_servers, ClaudeScope::Local);
        if let Some(mcp_json) = read_config(&directory.join(".mcp.json"))? {
            servers.extend(native_servers(mcp_json.mcp_servers, ClaudeScope::Project));
        }
        projects.push(ClaudeProjectState {
            directory,
            servers,
            disabled_servers: project.disabled_servers,
            disabled_project_servers: project.disabled_project_servers,
        });
    }
    Ok(projects)
}

/// The `claude` invocations that bring the user-scope entry to `shim`; empty when nothing changes.
pub(crate) fn setup_commands(state: &SetupState, shim: &ShimCommand) -> Vec<Vec<String>> {
    match state {
        SetupState::UpToDate => Vec::new(),
        SetupState::Missing => vec![add_command(shim)],
        SetupState::Outdated { .. } => vec![
            remove_command(ClaudeScope::User, PLUGBAY_ENTRY),
            add_command(shim),
        ],
    }
}

pub(crate) fn remove_command(scope: ClaudeScope, name: &str) -> Vec<String> {
    ["mcp", "remove", "--scope", scope.cli_name(), name]
        .map(String::from)
        .to_vec()
}

/// Arguments joined for display, quoting the ones that hold spaces or JSON.
pub(crate) fn display_command(arguments: &[String]) -> String {
    let mut displayed = Vec::new();
    for argument in arguments {
        if argument.contains([' ', '"', '{']) {
            displayed.push(format!("'{argument}'"));
        } else {
            displayed.push(argument.clone());
        }
    }
    displayed.join(" ")
}

/// Runs `claude <arguments>` in `working_dir` and fails with its stderr when it exits non-zero.
pub(crate) async fn run_cli(arguments: &[String], working_dir: &Path) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("claude")
        .args(arguments)
        .current_dir(working_dir)
        .output()
        .await
        .context("running the `claude` CLI")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "`claude {}` failed: {}",
            display_command(arguments),
            stderr.trim()
        );
    }
    Ok(())
}

/// The entry `name` had in a copy of `~/.claude.json`: user scope, or local scope of `directory`.
pub(crate) fn entry_in_backup(
    backup: &Path,
    name: &str,
    directory: Option<&Path>,
) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(config) = read_config(backup)? else {
        return Ok(None);
    };
    let mut servers = match directory {
        None => config.mcp_servers,
        Some(directory) => {
            let mut projects = config.projects;
            let project = project_keys(directory)
                .into_iter()
                .find_map(|key| projects.remove(&key));
            match project {
                Some(project) => project.mcp_servers,
                None => return Ok(None),
            }
        }
    };
    Ok(servers.remove(name))
}

/// `claude mcp add-json --scope <scope> <name> <entry>`.
pub(crate) fn add_json_command(
    scope: ClaudeScope,
    name: &str,
    entry: &serde_json::Value,
) -> Vec<String> {
    let mut command: Vec<String> = ["mcp", "add-json", "--scope", scope.cli_name(), name]
        .map(String::from)
        .to_vec();
    command.push(entry.to_string());
    command
}

fn add_command(shim: &ShimCommand) -> Vec<String> {
    let entry = serde_json::json!({
        "type": "stdio",
        "command": shim.command,
        "args": ShimCommand::args(),
    });
    let mut command: Vec<String> = ["mcp", "add-json", "--scope", "user", PLUGBAY_ENTRY]
        .map(String::from)
        .to_vec();
    command.push(entry.to_string());
    command
}

fn read_config(path: &Path) -> anyhow::Result<Option<ClaudeConfig>> {
    let Some(contents) = read_optional(path)? else {
        return Ok(None);
    };
    let config =
        serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(config))
}

fn local_servers(
    mut projects: BTreeMap<String, ClaudeProject>,
    project_dir: &Path,
) -> Vec<NativeServer> {
    for project_key in project_keys(project_dir) {
        if let Some(project) = projects.remove(&project_key) {
            return native_servers(project.mcp_servers, ClaudeScope::Local);
        }
    }
    Vec::new()
}

/// Claude Code keys projects by the directory it started in; try the path as given, then
/// with symlinks resolved (`/var` vs `/private/var` on macOS).
fn project_keys(project_dir: &Path) -> Vec<String> {
    let mut keys = vec![project_dir.display().to_string()];
    if let Ok(canonical) = project_dir.canonicalize() {
        keys.push(canonical.display().to_string());
    }
    keys
}

fn native_servers(
    entries: BTreeMap<String, serde_json::Value>,
    scope: ClaudeScope,
) -> Vec<NativeServer> {
    let mut servers = Vec::new();
    for (name, value) in entries {
        match serde_json::from_value::<ClaudeServerEntry>(value) {
            Ok(entry) => servers.extend(native_server(name, entry, scope)),
            Err(error) => {
                tracing::warn!(server = %name, %error, "skipping unreadable Claude Code MCP entry");
            }
        }
    }
    servers
}

fn native_server(
    name: String,
    entry: ClaudeServerEntry,
    scope: ClaudeScope,
) -> Option<NativeServer> {
    let mut notes = Vec::new();
    let transport = if let Some(command) = entry.command {
        NativeTransport::Stdio {
            command,
            args: entry.args,
            env: env_refs_to_plugbay(entry.env),
            cwd: None,
        }
    } else if let Some(url) = entry.url {
        if entry.transport.as_deref() == Some("sse") {
            notes.push(
                "uses the SSE transport; plugbay speaks streamable HTTP, so check the URL (often `/mcp` instead of `/sse`)"
                    .to_string(),
            );
        }
        if entry.headers_helper.is_some() {
            notes.push(
                "`headersHelper` is not imported; add the header with a secret ref instead"
                    .to_string(),
            );
        }
        NativeTransport::Http {
            url,
            headers: env_refs_to_plugbay(entry.headers),
        }
    } else {
        tracing::warn!(server = %name, "skipping Claude Code MCP entry with neither `command` nor `url`");
        return None;
    };

    Some(NativeServer {
        name,
        source: NativeSource::Claude(scope),
        transport,
        notes,
    })
}

fn env_refs_to_plugbay(values: BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .into_iter()
        .map(|(key, value)| (key, env_ref_to_plugbay(&value)))
        .collect()
}

/// Claude Code expands `${VAR}` and `${VAR:-default}`; plugbay spells that `${env:VAR}`.
/// Anything else inside `${...}` is left alone.
fn env_ref_to_plugbay(value: &str) -> String {
    let mut converted = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let Some(length) = rest[start..].find('}') else {
            break;
        };
        let inner = &rest[start + 2..start + length];
        let variable = inner.split(":-").next().unwrap_or(inner);
        converted.push_str(&rest[..start]);
        if is_env_variable_name(variable) {
            converted.push_str(&format!("${{env:{variable}}}"));
        } else {
            converted.push_str(&rest[start..=start + length]);
        }
        rest = &rest[start + length + 1..];
    }
    converted.push_str(rest);
    converted
}

fn is_env_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_JSON: &str = r#"{
        "numStartups": 3,
        "mcpServers": {
            "linear-server": { "type": "http", "url": "https://mcp.linear.app/mcp" },
            "paper": { "type": "stdio", "command": "/bin/paper", "args": ["mcp"] },
            "legacy": { "command": "npx", "args": ["-y", "legacy"], "env": { "API_KEY": "${LEGACY_KEY}" } },
            "broken": { "type": "stdio", "args": 3 }
        },
        "projects": {
            "/work/app": {
                "allowedTools": [],
                "mcpServers": {
                    "posthog": {
                        "type": "sse",
                        "url": "https://mcp.posthog.com/sse",
                        "headersHelper": "get-token.sh"
                    }
                }
            },
            "/work/other": { "mcpServers": { "other": { "url": "https://other.example/mcp" } } }
        }
    }"#;

    fn parse(json: &str, project_dir: &str) -> Vec<NativeServer> {
        let config: ClaudeConfig = serde_json::from_str(json).unwrap();
        let mut servers = native_servers(config.mcp_servers, ClaudeScope::User);
        servers.extend(local_servers(config.projects, Path::new(project_dir)));
        servers
    }

    fn names(servers: &[NativeServer]) -> Vec<&str> {
        servers.iter().map(|server| server.name.as_str()).collect()
    }

    #[test]
    fn reads_user_servers_and_local_servers_of_this_project_only() {
        let servers = parse(CLAUDE_JSON, "/work/app");
        assert_eq!(
            names(&servers),
            ["legacy", "linear-server", "paper", "posthog"]
        );
    }

    #[test]
    fn local_servers_are_marked_local_scope() {
        let servers = parse(CLAUDE_JSON, "/work/app");
        let posthog = servers
            .iter()
            .find(|server| server.name == "posthog")
            .unwrap();
        assert_eq!(posthog.source, NativeSource::Claude(ClaudeScope::Local));
    }

    #[test]
    fn stdio_entries_without_type_are_read() {
        let servers = parse(CLAUDE_JSON, "/elsewhere");
        let legacy = servers
            .iter()
            .find(|server| server.name == "legacy")
            .unwrap();
        assert!(
            matches!(&legacy.transport, NativeTransport::Stdio { command, .. } if command == "npx")
        );
    }

    #[test]
    fn claude_env_expansion_becomes_plugbay_env_ref() {
        let servers = parse(CLAUDE_JSON, "/elsewhere");
        let legacy = servers
            .iter()
            .find(|server| server.name == "legacy")
            .unwrap();
        let NativeTransport::Stdio { env, .. } = &legacy.transport else {
            panic!("expected stdio");
        };
        assert_eq!(env["API_KEY"], "${env:LEGACY_KEY}");
    }

    #[test]
    fn sse_and_headers_helper_are_noted() {
        let servers = parse(CLAUDE_JSON, "/work/app");
        let posthog = servers
            .iter()
            .find(|server| server.name == "posthog")
            .unwrap();
        assert_eq!(posthog.notes.len(), 2);
    }

    #[test]
    fn env_refs_keep_surrounding_text_and_drop_defaults() {
        assert_eq!(
            env_ref_to_plugbay("Bearer ${TOKEN:-none} and ${op://vault/item} ${"),
            "Bearer ${env:TOKEN} and ${op://vault/item} ${"
        );
    }

    #[test]
    fn missing_entry_needs_one_add_command() {
        let shim = ShimCommand {
            command: "/opt/plugbay".to_string(),
        };
        let commands = setup_commands(&SetupState::Missing, &shim);
        assert_eq!(
            commands,
            vec![vec![
                "mcp".to_string(),
                "add-json".to_string(),
                "--scope".to_string(),
                "user".to_string(),
                "plugbay".to_string(),
                r#"{"args":["connect"],"command":"/opt/plugbay","type":"stdio"}"#.to_string(),
            ]]
        );
    }

    #[test]
    fn outdated_entry_is_removed_then_added() {
        let shim = ShimCommand {
            command: "/opt/plugbay".to_string(),
        };
        let outdated = SetupState::Outdated {
            command: "/old/plugbay connect".to_string(),
        };
        let commands = setup_commands(&outdated, &shim);
        assert_eq!(commands[0], remove_command(ClaudeScope::User, "plugbay"));
        assert_eq!(commands.len(), 2);
    }

    #[test]
    fn up_to_date_entry_needs_no_command() {
        let shim = ShimCommand {
            command: "/opt/plugbay".to_string(),
        };
        let json = r#"{ "mcpServers": { "plugbay": { "type": "stdio", "command": "/opt/plugbay", "args": ["connect"] } } }"#;
        let config: ClaudeConfig = serde_json::from_str(json).unwrap();
        let servers = native_servers(config.mcp_servers, ClaudeScope::User);
        let state = SetupState::from_servers(&servers, &shim);
        assert!(setup_commands(&state, &shim).is_empty());
    }
}
