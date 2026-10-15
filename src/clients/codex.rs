//! Codex: `[mcp_servers.<name>]` tables in `$CODEX_HOME/config.toml` (default `~/.codex`).
//! Edited with `toml_edit` so comments and formatting survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::{
    NativeServer, NativeSource, NativeTransport, PLUGBAY_ENTRY, ShimCommand, read_optional,
};

#[derive(Debug, Default, Deserialize)]
struct CodexConfig {
    #[serde(default)]
    mcp_servers: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct CodexServerEntry {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    bearer_token_env_var: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    /// Header name → name of the env var holding its value.
    #[serde(default)]
    env_http_headers: BTreeMap<String, String>,
    #[serde(default)]
    env_vars: Vec<String>,
    enabled: Option<bool>,
}

pub(crate) fn config_file(home: &Path) -> PathBuf {
    let codex_home = match std::env::var_os("CODEX_HOME") {
        Some(codex_home) => PathBuf::from(codex_home),
        None => home.join(".codex"),
    };
    codex_home.join("config.toml")
}

pub(crate) fn read_servers(home: &Path) -> anyhow::Result<Vec<NativeServer>> {
    let file = config_file(home);
    let Some(contents) = read_optional(&file)? else {
        return Ok(Vec::new());
    };
    parse_servers(&contents).with_context(|| format!("parsing {}", file.display()))
}

/// `contents` with `[mcp_servers.plugbay]` running `shim`. Other keys of an existing entry are kept.
pub(crate) fn with_plugbay_entry(contents: &str, shim: &ShimCommand) -> anyhow::Result<String> {
    let mut document = parse_document(contents)?;
    let mcp_servers = mcp_servers_table(&mut document)?;
    let entry = mcp_servers
        .entry(PLUGBAY_ENTRY)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .context("`mcp_servers.plugbay` in the Codex config is not a table")?;

    let mut args = Array::new();
    for argument in ShimCommand::args() {
        args.push(argument);
    }
    entry.insert("command", value(shim.command.as_str()));
    entry.insert("args", value(args));
    entry.remove("url");
    entry.remove("enabled");
    Ok(document.to_string())
}

/// `contents` without the `[mcp_servers.<name>]` tables in `names`.
pub(crate) fn without_servers(contents: &str, names: &[&str]) -> anyhow::Result<String> {
    let mut document = parse_document(contents)?;
    if let Some(mcp_servers) = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
    {
        for name in names {
            mcp_servers.remove(name);
        }
    }
    Ok(document.to_string())
}

fn parse_document(contents: &str) -> anyhow::Result<DocumentMut> {
    contents.parse().context("parsing the Codex config")
}

fn mcp_servers_table(document: &mut DocumentMut) -> anyhow::Result<&mut dyn toml_edit::TableLike> {
    document
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_like_mut()
        .context("`mcp_servers` in the Codex config is not a table")
}

fn parse_servers(contents: &str) -> anyhow::Result<Vec<NativeServer>> {
    let config: CodexConfig = toml::from_str(contents)?;
    let mut servers = Vec::new();
    for (name, value) in config.mcp_servers {
        match value.try_into::<CodexServerEntry>() {
            Ok(entry) => servers.extend(native_server(name, entry)),
            Err(error) => {
                tracing::warn!(server = %name, %error, "skipping unreadable Codex MCP entry")
            }
        }
    }
    Ok(servers)
}

fn native_server(name: String, entry: CodexServerEntry) -> Option<NativeServer> {
    if entry.enabled == Some(false) {
        tracing::debug!(server = %name, "ignoring disabled Codex MCP entry");
        return None;
    }
    let mut notes = Vec::new();
    if !entry.env_vars.is_empty() {
        notes.push(format!(
            "Codex forwarded env vars {}; add them to `env` as `${{env:NAME}}` refs if the server needs them",
            entry.env_vars.join(", ")
        ));
    }

    let transport = if let Some(command) = entry.command {
        NativeTransport::Stdio {
            command,
            args: entry.args,
            env: entry.env,
            cwd: entry.cwd,
        }
    } else if let Some(url) = entry.url {
        NativeTransport::Http {
            url,
            headers: http_headers(
                entry.http_headers,
                entry.env_http_headers,
                entry.bearer_token_env_var,
            ),
        }
    } else {
        tracing::warn!(server = %name, "skipping Codex MCP entry with neither `command` nor `url`");
        return None;
    };

    Some(NativeServer {
        name,
        source: NativeSource::Codex,
        transport,
        notes,
    })
}

/// Codex's three ways to set headers, as plugbay headers with `${env:...}` refs.
fn http_headers(
    literal: BTreeMap<String, String>,
    from_env: BTreeMap<String, String>,
    bearer_token_env_var: Option<String>,
) -> BTreeMap<String, String> {
    let mut headers = literal;
    for (header, variable) in from_env {
        headers.insert(header, format!("${{env:{variable}}}"));
    }
    if let Some(variable) = bearer_token_env_var {
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer ${{env:{variable}}}"),
        );
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::SetupState;

    const CODEX_TOML: &str = r#"
model = "gpt-5"   # keep me

[mcp_servers.slack]
command = "npx"
args = ["-y", "slack-mcp-server"]
env = { SLACK_MCP_ADD_MESSAGE_TOOL = "true" }

[mcp_servers.linear]
url = "https://mcp.linear.app/mcp"
bearer_token_env_var = "LINEAR_TOKEN"
env_http_headers = { "X-Team" = "LINEAR_TEAM" }

[mcp_servers.off]
command = "off"
enabled = false
"#;

    fn shim() -> ShimCommand {
        ShimCommand {
            command: "/opt/plugbay".to_string(),
        }
    }

    fn state_of(contents: &str) -> SetupState {
        SetupState::from_servers(&parse_servers(contents).unwrap(), &shim())
    }

    #[test]
    fn reads_stdio_and_http_servers() {
        let servers = parse_servers(CODEX_TOML).unwrap();
        let names: Vec<&str> = servers.iter().map(|server| server.name.as_str()).collect();
        assert_eq!(names, ["linear", "slack"]);
    }

    #[test]
    fn bearer_and_env_headers_become_env_refs() {
        let servers = parse_servers(CODEX_TOML).unwrap();
        let NativeTransport::Http { headers, .. } = &servers[0].transport else {
            panic!("expected http");
        };
        assert_eq!(headers["Authorization"], "Bearer ${env:LINEAR_TOKEN}");
        assert_eq!(headers["X-Team"], "${env:LINEAR_TEAM}");
    }

    #[test]
    fn disabled_plugbay_entry_is_enabled_by_setup() {
        let contents = "[mcp_servers.plugbay]\ncommand = \"/opt/plugbay\"\nargs = [\"connect\"]\nenabled = false\n";
        let updated = with_plugbay_entry(contents, &shim()).unwrap();
        assert_eq!(state_of(&updated), SetupState::UpToDate);
    }

    #[test]
    fn adding_plugbay_keeps_comments_and_other_servers() {
        let updated = with_plugbay_entry(CODEX_TOML, &shim()).unwrap();
        insta::assert_snapshot!(updated);
    }

    #[test]
    fn adding_plugbay_to_empty_config_writes_only_its_table() {
        let updated = with_plugbay_entry("", &shim()).unwrap();
        assert_eq!(
            updated,
            "[mcp_servers.plugbay]\ncommand = \"/opt/plugbay\"\nargs = [\"connect\"]\n"
        );
    }

    #[test]
    fn adding_plugbay_twice_changes_nothing() {
        let once = with_plugbay_entry(CODEX_TOML, &shim()).unwrap();
        let twice = with_plugbay_entry(&once, &shim()).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn added_entry_is_up_to_date() {
        let updated = with_plugbay_entry(CODEX_TOML, &shim()).unwrap();
        assert_eq!(state_of(&updated), SetupState::UpToDate);
    }

    #[test]
    fn entry_with_old_command_is_outdated() {
        let contents = "[mcp_servers.plugbay]\ncommand = \"/old/plugbay\"\nargs = [\"connect\"]\n";
        assert!(matches!(state_of(contents), SetupState::Outdated { .. }));
    }

    #[test]
    fn removing_servers_keeps_the_rest() {
        let updated = without_servers(CODEX_TOML, &["slack", "off"]).unwrap();
        insta::assert_snapshot!(updated);
    }
}
