//! Text for the agent: the server instructions sent at initialize and the `plugbay__status` report.

use super::tools::{ENABLE_TOOL, STATUS_TOOL, TOOL_SEPARATOR};
use super::{ActiveServer, Resolved};
use crate::config::{DEFAULT_ACCOUNT, ResolvedConfig, ResolvedServer};
use crate::identity::{self, VerifiedAccounts};
use crate::protocol::{SessionStatus, StatusServer, StatusState};
use crate::registry;
use crate::report::{Audience, ServerState};

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
    let mut working = Vec::new();
    let mut unavailable = Vec::new();
    for active in servers {
        let server = &active.server;
        match ServerState::from(&active.status).unavailable_note(server) {
            None => working.push(account_line(server, verified_login(server, verified))),
            Some(note) => unavailable.push(format!("{} ({note})", server_label(server))),
        }
    }
    if config.servers.is_empty() {
        lines.push("No servers are configured here.".to_string());
    } else if !working.is_empty() {
        lines.push("Servers and accounts in use (use the right one for this project):".to_string());
        lines.extend(working);
    }
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

fn verified_login<'a>(server: &ResolvedServer, verified: &'a VerifiedAccounts) -> Option<&'a str> {
    verified.shown(&server.identity, &server.account)
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

fn account_line(server: &ResolvedServer, verified: Option<&str>) -> String {
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
    if let Some(shown) = verified {
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

fn problem_summary(config: &ResolvedConfig) -> String {
    config
        .problems
        .iter()
        .map(|problem| problem.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The `plugbay__status` report: text for the agent, and the same servers and problems typed
/// for `plugbay tools` and `call`.
pub(super) struct StatusReport {
    pub text: String,
    pub status: SessionStatus,
}

impl StatusReport {
    pub fn not_resolved_yet() -> Self {
        Self {
            text: "plugbay has not resolved any config yet".to_string(),
            status: SessionStatus::default(),
        }
    }
}

pub(super) fn status_report(
    resolved: &Resolved,
    servers: &[ActiveServer],
    verified: &VerifiedAccounts,
) -> StatusReport {
    let mut lines = Vec::new();
    let mut status = SessionStatus::default();
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
            status.problems.push(error.to_string());
            return StatusReport {
                text: lines.join("\n"),
                status,
            };
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
        let server = &active.server;
        let state = ServerState::from(&active.status);
        let status_line = format!(
            "{} ({}) — {}",
            server.display_name(),
            server.transport.target(),
            state.status(server, verified_login(server, verified), Audience::Agent)
        );
        lines.push(format!("  {status_line}"));
        status.servers.push(StatusServer {
            name: server.name.clone(),
            account: Some(server.account.clone()),
            tool_prefix: server.tool_prefix.clone(),
            label: server.display_name(),
            state: StatusState::from(&state),
            status_line,
        });
    }
    for disabled in &config.disabled {
        let status_line = format!(
            "{} — disabled by {}",
            disabled.name,
            disabled.disabled_by.display()
        );
        lines.push(format!("  {status_line}"));
        status.servers.push(StatusServer {
            name: disabled.name.clone(),
            account: None,
            tool_prefix: disabled.name.clone(),
            label: disabled.name.clone(),
            state: StatusState::Disabled,
            status_line,
        });
    }
    for problem in &config.problems {
        lines.push(format!("problem: {}", problem.message));
        status.problems.push(problem.message.clone());
    }
    StatusReport {
        text: lines.join("\n"),
        status,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{ServerIdentity, ServerTransport, ToolFilter};
    use crate::identity::ExpectationFailure;
    use crate::project::ProjectLocation;
    use crate::session::ServerStatus;

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
            account_line(&server, verified_login(&server, &verified)),
            "- Linear → initech (team WP) (verified: Initech)"
        );
    }

    #[test]
    fn account_line_without_a_verified_login_is_unchanged() {
        let server = linear("personal", &[("expect", "mihaicrisan")]);
        assert_eq!(account_line(&server, None), "- Linear → personal");
    }

    #[test]
    fn local_command_with_only_the_default_account_shows_no_label() {
        let unnamed = local_tool("default");
        let named = local_tool("work");
        assert_eq!(account_line(&unnamed, None), "- paper");
        assert_eq!(account_line(&named, None), "- paper → work");
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
                "Not available right now: sentry:acme (failed); paper (needs trust: ask the human to review the config and run `plugbay trust` in the project directory)"
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
            ServerStatus::ExpectFailed(ExpectationFailure::Mismatch {
                shown: "Initech".to_string(),
                expected: "mihaicrisan".to_string(),
            }),
        )];
        let text = instructions(&resolved, &servers, &VerifiedAccounts::default());
        assert!(
            text.contains(
                "linear:personal (wrong account: ask the human to run `plugbay logout linear:personal && plugbay login linear:personal` and pick mihaicrisan)"
            ),
            "{text}"
        );
    }
}
