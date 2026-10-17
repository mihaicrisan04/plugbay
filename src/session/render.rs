//! Text for the agent: the server instructions sent at initialize and the `plugbay__status` report.

use super::tools::{ENABLE_TOOL, STATUS_TOOL, TOOL_SEPARATOR};
use super::{ActiveServer, Resolved, ServerStatus};
use crate::config::{DEFAULT_ACCOUNT, ResolvedConfig, ResolvedServer};
use crate::identity::{self, VerifiedAccounts};
use crate::registry;

pub(super) fn not_resolved_yet() -> String {
    format!("plugbay routes MCP servers for this project. Call {STATUS_TOOL} to see which.")
}

/// Server instructions: the agent (and the human reading along) see which account each working
/// server uses, and which servers are configured but not usable right now.
pub(super) fn instructions(
    resolved: &Resolved,
    servers: &[ActiveServer],
    verified: &VerifiedAccounts,
) -> String {
    let config = match &resolved.outcome {
        Ok(config) => config,
        Err(error) => {
            return format!(
                "plugbay could not read the MCP config for this project: {error}. Ask the human to run `plugbay doctor`."
            );
        }
    };
    let routes = match &resolved.location.config_dir {
        Some(dir) => format!("plugbay routes MCP servers for {}.", dir.display()),
        None => {
            "plugbay routes MCP servers from your global config (no project directory).".to_string()
        }
    };

    let mut lines = vec![format!("{routes} Tools are named <server>__<tool>.")];
    let working: Vec<String> = servers
        .iter()
        .filter(|active| matches!(active.status, ServerStatus::Connected(_)))
        .map(|active| account_line(&active.server, verified))
        .collect();
    if config.servers.is_empty() {
        lines.push("No servers are configured here.".to_string());
    } else if !working.is_empty() {
        lines.push("Servers and accounts in use (use the right one for this project):".to_string());
        lines.extend(working);
    }
    let unavailable: Vec<String> = servers.iter().filter_map(unavailable_note).collect();
    if !unavailable.is_empty() {
        lines.push(format!(
            "Not available right now: {}. Call {STATUS_TOOL} for details.",
            unavailable.join("; ")
        ));
    }
    if !config.problems.is_empty() {
        lines.push(format!("Config problems: {}", problem_summary(config)));
    }
    lines.push(format!(
        "If a server or tool seems missing, call {STATUS_TOOL}. \
         To turn on another server for this session when the human asks, call {ENABLE_TOOL}."
    ));
    lines.join("\n")
}

/// A local command nobody gave accounts to has nothing worth naming.
fn has_account_worth_showing(server: &ResolvedServer) -> bool {
    !(server.is_stdio() && server.account == DEFAULT_ACCOUNT)
}

/// `name:account`, or just `name` for a local command without accounts.
fn server_label(server: &ResolvedServer) -> String {
    if has_account_worth_showing(server) {
        server.display_name()
    } else {
        server.name.clone()
    }
}

fn account_line(server: &ResolvedServer, verified: &VerifiedAccounts) -> String {
    let title = registry::lookup(&server.name).map_or(server.name.as_str(), |entry| entry.title);
    let mut line = format!("- {title}");
    if has_account_worth_showing(server) {
        line.push_str(&format!(" → {}", server.account));
    }
    let details: Vec<String> = server
        .account_metadata
        .iter()
        .filter(|(key, _)| key.as_str() != identity::EXPECT_KEY)
        .map(|(key, value)| format!("{key} {value}"))
        .collect();
    if !details.is_empty() {
        line.push_str(&format!(" ({})", details.join(", ")));
    }
    if let Some(shown) = verified.shown(&server.identity, &server.account) {
        line.push_str(&format!(" (verified: {shown})"));
    }
    if server.tool_prefix != server.name {
        line.push_str(&format!(
            ", tools prefixed {}{TOOL_SEPARATOR}",
            server.tool_prefix
        ));
    }
    line
}

/// One short clause per configured server that exposes none of its own tools.
fn unavailable_note(active: &ActiveServer) -> Option<String> {
    let label = server_label(&active.server);
    let reason = match &active.status {
        ServerStatus::Connected(_) => return None,
        ServerStatus::Connecting { .. } => {
            "still connecting, its tools appear when it is up".to_string()
        }
        ServerStatus::NeedsLogin { .. } => format!(
            "needs a login, call {}{TOOL_SEPARATOR}login",
            active.server.tool_prefix
        ),
        ServerStatus::Untrusted => {
            "its config is not trusted yet, ask the human to run `plugbay trust`".to_string()
        }
        ServerStatus::Failed(_) => "failed to start".to_string(),
        ServerStatus::WrongAccount(_) => "logged in as the wrong account".to_string(),
    };
    Some(format!("{label} ({reason})"))
}

fn problem_summary(config: &ResolvedConfig) -> String {
    config
        .problems
        .iter()
        .map(|problem| problem.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

pub(super) fn status_text(
    resolved: &Resolved,
    servers: &[ActiveServer],
    verified: &VerifiedAccounts,
) -> String {
    let mut lines = Vec::new();
    match &resolved.location.config_dir {
        Some(dir) => lines.push(format!("project: {}", dir.display())),
        None => lines.push("project: none (global config only)".to_string()),
    }
    if let Some(working_dir) = &resolved.location.working_dir
        && Some(working_dir) != resolved.location.config_dir.as_ref()
    {
        lines.push(format!("working dir: {}", working_dir.display()));
    }

    let config = match &resolved.outcome {
        Ok(config) => config,
        Err(error) => {
            lines.push(format!("config error: {error}"));
            return lines.join("\n");
        }
    };
    lines.push("config files:".to_string());
    lines.extend(
        config
            .files
            .iter()
            .map(|file| format!("  {}", file.path.display())),
    );

    lines.push("servers:".to_string());
    if servers.is_empty() {
        lines.push("  (none)".to_string());
    }
    for active in servers {
        lines.push(format!(
            "  {} ({}) — {}",
            active.server.display_name(),
            active.server.transport.target(),
            server_status(active, verified)
        ));
    }
    for disabled in &config.disabled {
        lines.push(format!(
            "  {} — disabled by {}",
            disabled.name,
            disabled.disabled_by.display()
        ));
    }
    for problem in &config.problems {
        lines.push(format!("problem: {}", problem.message));
    }
    lines.join("\n")
}

fn server_status(active: &ActiveServer, verified: &VerifiedAccounts) -> String {
    let server = &active.server;
    match &active.status {
        ServerStatus::Connected(_) => match verified.shown(&server.identity, &server.account) {
            Some(shown) => format!("connected (verified: {shown})"),
            None => "connected".to_string(),
        },
        ServerStatus::Connecting { .. } => "connecting: its tools appear once it is up (clients \
             that ignore tools/list_changed, like Codex, see them next session)"
            .to_string(),
        ServerStatus::NeedsLogin { .. } => format!(
            "needs login: call {}__login or run `plugbay login {}` (add `--token` for an API token)",
            server.tool_prefix,
            server.display_name()
        ),
        ServerStatus::Untrusted => {
            "off: runs a command or reads secrets from config that is not trusted, run `plugbay trust`"
                .to_string()
        }
        ServerStatus::Failed(error) => format!("failed: {error}"),
        ServerStatus::WrongAccount(message) => format!("off: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{ServerIdentity, ServerTransport, ToolFilter};
    use crate::project::ProjectLocation;

    const LINEAR: &str = "https://mcp.linear.app/mcp";

    fn linear(account: &str, metadata: &[(&str, &str)]) -> ResolvedServer {
        let mut server = ResolvedServer::test_http("linear", account, LINEAR);
        for (key, value) in metadata {
            server
                .account_metadata
                .insert(key.to_string(), value.to_string());
        }
        server
    }

    fn local_tool(account: &str) -> ResolvedServer {
        ResolvedServer {
            name: "paper".to_string(),
            account: account.to_string(),
            tool_prefix: "paper".to_string(),
            transport: ServerTransport::Stdio {
                command: "paper-mcp".to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
            },
            identity: ServerIdentity("stdio:paper".to_string()),
            tool_filter: ToolFilter::default(),
            account_metadata: BTreeMap::new(),
            provenance: BTreeMap::new(),
            requires_trust_from: Vec::new(),
        }
    }

    fn resolved_with(servers: &[&ResolvedServer], config_dir: Option<&str>) -> Resolved {
        let location = ProjectLocation {
            config_dir: config_dir.map(Into::into),
            working_dir: config_dir.map(Into::into),
        };
        Resolved {
            location: location.clone(),
            stamps: Vec::new(),
            outcome: Ok(ResolvedConfig {
                location,
                files: Vec::new(),
                servers: servers.iter().map(|server| (*server).clone()).collect(),
                disabled: Vec::new(),
                catalog: Vec::new(),
                problems: Vec::new(),
                trusted_paths: Vec::new(),
            }),
        }
    }

    fn with_status(server: &ResolvedServer, status: ServerStatus) -> ActiveServer {
        ActiveServer {
            server: server.clone(),
            status,
        }
    }

    #[test]
    fn account_line_shows_metadata_and_the_verified_login_but_not_expect() {
        let server = linear("initech", &[("team", "WP"), ("expect", "Initech")]);
        let verified = VerifiedAccounts::test_with(&[(LINEAR, "initech", "Initech")]);
        assert_eq!(
            account_line(&server, &verified),
            "- Linear → initech (team WP) (verified: Initech)"
        );
    }

    #[test]
    fn account_line_without_a_verified_login_is_unchanged() {
        let server = linear("personal", &[("expect", "mihaicrisan")]);
        assert_eq!(
            account_line(&server, &VerifiedAccounts::default()),
            "- Linear → personal"
        );
    }

    #[test]
    fn local_command_with_only_the_default_account_shows_no_label() {
        let verified = VerifiedAccounts::default();
        assert_eq!(account_line(&local_tool("default"), &verified), "- paper");
        assert_eq!(
            account_line(&local_tool("work"), &verified),
            "- paper → work"
        );
    }

    #[test]
    fn instructions_list_only_servers_whose_tools_are_exposed() {
        let linear = linear("initech", &[]);
        let sentry = ResolvedServer::test_http("sentry", "acme", "https://mcp.sentry.dev/mcp");
        let paper = local_tool("default");
        let resolved = resolved_with(&[&linear, &sentry, &paper], Some("/work/app"));
        let servers = [
            with_status(
                &sentry,
                ServerStatus::Failed("connection refused".to_string()),
            ),
            with_status(&paper, ServerStatus::Untrusted),
        ];

        let text = instructions(&resolved, &servers, &VerifiedAccounts::default());

        assert!(!text.contains("Sentry →"), "{text}");
        assert!(!text.contains("in use"), "{text}");
        assert!(
            text.contains(
                "Not available right now: sentry:acme (failed to start); paper (its config is not trusted yet"
            ),
            "{text}"
        );
    }

    #[test]
    fn instructions_without_a_project_name_the_global_config() {
        let resolved = resolved_with(&[], None);
        let text = instructions(&resolved, &[], &VerifiedAccounts::default());
        assert!(
            text.starts_with(
                "plugbay routes MCP servers from your global config (no project directory)."
            ),
            "{text}"
        );
        assert!(text.contains("No servers are configured here."), "{text}");
    }

    #[test]
    fn wrong_account_is_listed_as_unavailable() {
        let server = linear("personal", &[("expect", "mihaicrisan")]);
        let resolved = resolved_with(&[&server], Some("/work/app"));
        let servers = [with_status(
            &server,
            ServerStatus::WrongAccount("logged in as Initech".to_string()),
        )];
        let text = instructions(&resolved, &servers, &VerifiedAccounts::default());
        assert!(
            text.contains("linear:personal (logged in as the wrong account)"),
            "{text}"
        );
    }
}
