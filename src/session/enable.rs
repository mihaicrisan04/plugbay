//! `plugbay__enable` / `plugbay__disable`: servers an agent switches on or off for its session only.

use rmcp::model::{CallToolResult, JsonObject};

use super::prompt::{ask_human, supports_forms};
use super::tools::{
    DisableArguments, EnableArguments, STATUS_TOOL, TOOL_SEPARATOR, error_result, parse_arguments,
    text_result,
};
use super::{ActiveServer, ServerStatus, Session};
use crate::config::{self, ResolvedConfig, SessionPin};
use crate::registry;

impl Session {
    /// Switches a catalog or registry server on for this session only, keeping the pin only if it works.
    pub(super) async fn enable(&self, arguments: Option<JsonObject>) -> CallToolResult {
        let arguments: EnableArguments = match parse_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(message) => return error_result(message),
        };
        let account_is_valid = arguments
            .account
            .as_deref()
            .is_none_or(config::is_valid_name);
        if !config::is_valid_name(&arguments.server) || !account_is_valid {
            return error_result(
                "server names and accounts use lowercase letters, digits and `-` only".to_string(),
            );
        }
        if let Some(account) = &arguments.account
            && let Err(refusal) = self.approve_account(&arguments.server, account).await
        {
            return error_result(refusal);
        }

        let pin = SessionPin::On {
            account: arguments.account.clone(),
        };
        let previous = self.pins.lock().insert(arguments.server.clone(), pin);
        self.refresh_and_notify().await;

        match self.describe_enabled(&arguments.server) {
            Ok(message) => text_result(message),
            Err(message) => {
                self.restore_pin(&arguments.server, previous).await;
                error_result(message)
            }
        }
    }

    /// A project's config picks the account on purpose (client A's repo uses client A's
    /// workspace), so only the human may move this session to another one.
    async fn approve_account(&self, server: &str, account: &str) -> Result<(), String> {
        let bound = self.accounts_config_picks(server);
        if bound.is_empty() || bound.iter().any(|bound| bound == account) {
            return Ok(());
        }
        let current = bound.join(", ");
        let refusal = format!(
            "{server} uses {current} in this project's plugbay config, and switching it to {account} \
             needs the human's approval. Ask the human to run `plugbay use {server}:{account}` if \
             they want {account} here."
        );
        let Some(peer) = self.peer.get().filter(|peer| supports_forms(peer)) else {
            return Err(refusal);
        };
        let message =
            format!("plugbay: switch {server} from {current} to {account} for this session?");
        let keep = format!("Keep {current}");
        let choices = [
            ("switch", "Switch for this session"),
            ("keep", keep.as_str()),
        ];
        match ask_human(peer, message, &choices).await.as_deref() {
            Some("switch") => Ok(()),
            _ => Err(refusal),
        }
    }

    /// The accounts config files give `server` here when it is on, ignoring this session's own pin
    /// for it. Empty when no file names one (the server would use `default`).
    fn accounts_config_picks(&self, server: &str) -> Vec<String> {
        let mut pins = self.pins.lock().clone();
        pins.insert(server.to_string(), SessionPin::On { account: None });
        let config = match config::resolve_with_pins(&self.shared.paths, &self.location(), &pins) {
            Ok(config) => config,
            Err(error) => {
                tracing::debug!(error = format!("{error:#}"), "config did not resolve");
                return Vec::new();
            }
        };
        config
            .server(server)
            .into_iter()
            .filter(|resolved| resolved.provenance.contains_key("account"))
            .map(|resolved| resolved.account.clone())
            .collect()
    }

    pub(super) async fn disable(&self, arguments: Option<JsonObject>) -> CallToolResult {
        let arguments: DisableArguments = match parse_arguments(arguments) {
            Ok(arguments) => arguments,
            Err(message) => return error_result(message),
        };
        let is_on = self
            .state
            .lock()
            .servers
            .iter()
            .any(|active| active.server.name == arguments.server);
        if !is_on {
            return error_result(format!(
                "{} is not on in this session; call {STATUS_TOOL} to see what is",
                arguments.server
            ));
        }
        self.pins
            .lock()
            .insert(arguments.server.clone(), SessionPin::Off);
        self.refresh_and_notify().await;
        text_result(format!(
            "{} is off for the rest of this session (config files are unchanged).",
            arguments.server
        ))
    }

    async fn restore_pin(&self, server: &str, previous: Option<SessionPin>) {
        {
            let mut pins = self.pins.lock();
            match previous {
                Some(pin) => pins.insert(server.to_string(), pin),
                None => pins.remove(server),
            };
        }
        self.refresh_and_notify().await;
    }

    /// What enabling `name` led to, or why it can't stay on.
    fn describe_enabled(&self, name: &str) -> Result<String, String> {
        let state = self.state.lock();
        let config = match state.resolved.as_ref().map(|resolved| &resolved.outcome) {
            Some(Ok(config)) => config,
            Some(Err(error)) => return Err(format!("the config does not resolve: {error}")),
            None => return Err("plugbay has not resolved any config yet".to_string()),
        };
        let active: Vec<&ActiveServer> = state
            .servers
            .iter()
            .filter(|active| active.server.name == name)
            .collect();
        if active.is_empty() {
            return Err(why_not_enabled(config, name));
        }

        let mut lines = Vec::new();
        for active in active {
            let display_name = active.server.display_name();
            let prefix = &active.server.tool_prefix;
            match &active.status {
                ServerStatus::Connected(_) => lines.push(format!(
                    "{display_name} is on for this session; its tools are {prefix}{TOOL_SEPARATOR}*. \
                     Nothing is saved: to keep it for this project the human can run `plugbay use {display_name}`."
                )),
                ServerStatus::Connecting { .. } => lines.push(format!(
                    "{display_name} is on for this session but still connecting; its tools \
                     {prefix}{TOOL_SEPARATOR}* appear once it is up."
                )),
                ServerStatus::NeedsLogin { .. } => lines.push(format!(
                    "{display_name} is on but needs a login: call {prefix}{TOOL_SEPARATOR}login, \
                     or ask the human to run `plugbay login {display_name}`."
                )),
                ServerStatus::Untrusted => {
                    return Err(format!(
                        "{display_name} runs a command or reads secrets from config that is not trusted yet; ask the human to run `plugbay trust`"
                    ));
                }
                ServerStatus::Failed(error) => {
                    return Err(format!("{display_name} could not start: {error}"));
                }
                ServerStatus::WrongAccount(message) => return Err(message.clone()),
            }
        }
        Ok(lines.join("\n"))
    }
}

/// Servers plugbay knows how to reach that are off here: the catalog, ones switched off, the registry.
pub(super) fn servers_to_enable(config: &ResolvedConfig) -> Vec<String> {
    let mut names: Vec<String> = config.catalog.clone();
    names.extend(config.disabled.iter().map(|disabled| disabled.name.clone()));
    names.extend(registry::all().iter().map(|entry| entry.name.to_string()));
    names.retain(|name| config.server(name).is_empty());
    names.sort();
    names.dedup();
    names
}

fn why_not_enabled(config: &ResolvedConfig, name: &str) -> String {
    if let Some(problem) = config.problems.iter().find(|problem| {
        problem.servers.iter().any(|server| server == name) && !problem.is_unknown_server()
    }) {
        return format!("{name} can't be enabled: {}", problem.message);
    }
    format!(
        "plugbay doesn't know a server named {name}. Known: {}. The human can define one with \
         `plugbay add {name} --url <url>` or `plugbay add {name} -- <command>`.",
        servers_to_enable(config).join(", ")
    )
}
