//! What resolved here and what state each server is in, built once and rendered by `ls`,
//! `doctor`, `plugbay__status` and the session instructions, so their wording can't drift.

use crate::config::{ResolvedConfig, ResolvedServer};
use crate::credentials::{self, StoredLogin};
use crate::identity::{self, VerifiedAccounts};
use crate::paths::Paths;
use crate::trust::Trust;

/// A resolved config with what plugbay knows about each of its servers.
pub struct Report<'a> {
    /// Location, config files, disabled servers, catalog and problems.
    pub config: &'a ResolvedConfig,
    /// One per `config.servers` entry, in the same order.
    pub servers: Vec<ServerReport<'a>>,
}

pub struct ServerReport<'a> {
    pub server: &'a ResolvedServer,
    pub trust: TrustState,
    /// `None` inside a session, where connecting tells instead of a keychain read.
    pub login: Option<LoginState>,
    /// Who the server said the login is, when plugbay asked it.
    pub verified: Option<String>,
    /// Other labels of this server logged in to the same account, as `name:account`.
    pub same_login_as: Vec<String>,
    /// Set when the verified login doesn't match the account's `expect`.
    pub wrong_account: Option<String>,
    /// Inside a session only: how far connecting got.
    pub connection: Option<Connection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// Nothing in it runs code or reads secrets from a project file.
    NotNeeded,
    Trusted,
    Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    /// Stdio servers and remote servers with static auth headers.
    NotNeeded,
    OAuth,
    Token,
    Missing,
    /// The keychain could not be read.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Connecting,
    Connected,
    /// The server answered with an auth challenge.
    NeedsLogin,
    Failed(String),
}

/// The one thing that stands between a server and an agent using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Ready,
    Connecting,
    NeedsTrust,
    NeedsLogin,
    LoginUnknown,
    WrongAccount(String),
    Failed(String),
}

/// Who reads the next step: a human runs commands, an agent asks the human to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Human,
    Agent,
}

impl<'a> Report<'a> {
    /// The CLI's view: trust grants on disk and stored logins from the keychain.
    pub async fn build(paths: &Paths, config: &'a ResolvedConfig) -> Report<'a> {
        let trust = Trust::new(paths);
        let verified = VerifiedAccounts::load(paths).await;
        let mut servers = Vec::new();
        for server in &config.servers {
            let trust = if server.requires_trust_from.is_empty() {
                TrustState::NotNeeded
            } else if trust.is_server_trusted(server, config) {
                TrustState::Trusted
            } else {
                TrustState::Untrusted
            };
            servers.push(ServerReport {
                trust,
                login: Some(login_state(server).await),
                wrong_account: identity::expectation_failure(server, &verified),
                ..ServerReport::new(server, &verified)
            });
        }
        Report { config, servers }
    }
}

impl<'a> ServerReport<'a> {
    /// Nothing checked yet beyond who the login is.
    pub fn new(server: &'a ResolvedServer, verified: &VerifiedAccounts) -> Self {
        Self {
            server,
            trust: TrustState::NotNeeded,
            login: None,
            verified: verified
                .shown(&server.identity, &server.account)
                .map(str::to_string),
            same_login_as: same_login_labels(server, verified),
            wrong_account: None,
            connection: None,
        }
    }

    /// In the order a session meets them: trust before connecting, the login while connecting,
    /// the verified account once connected.
    pub fn state(&self) -> ServerState {
        if self.trust == TrustState::Untrusted {
            return ServerState::NeedsTrust;
        }
        match &self.connection {
            Some(Connection::Connecting) => return ServerState::Connecting,
            Some(Connection::NeedsLogin) => return ServerState::NeedsLogin,
            Some(Connection::Failed(error)) => return ServerState::Failed(error.clone()),
            Some(Connection::Connected) | None => {}
        }
        match &self.login {
            Some(LoginState::Missing) => return ServerState::NeedsLogin,
            Some(LoginState::Unknown(_)) => return ServerState::LoginUnknown,
            Some(LoginState::NotNeeded | LoginState::OAuth | LoginState::Token) | None => {}
        }
        match &self.wrong_account {
            Some(message) => ServerState::WrongAccount(message.clone()),
            None => ServerState::Ready,
        }
    }

    /// The full status line: `ls`'s STATUS column and `plugbay__status`.
    pub fn status(&self, audience: Audience) -> String {
        let state = self.state();
        let explanation = match &state {
            ServerState::Ready => return self.ready_label(),
            ServerState::WrongAccount(message) => Some(message.clone()),
            ServerState::Failed(error) => Some(error.clone()),
            ServerState::Connecting
            | ServerState::NeedsTrust
            | ServerState::NeedsLogin
            | ServerState::LoginUnknown => state.next_step(self.server, audience),
        };
        match explanation {
            Some(explanation) => format!("{}: {explanation}", state.summary()),
            None => state.summary().to_string(),
        }
    }

    /// Why an agent can't use the server and what to do, without the details `status` adds;
    /// `None` when it is ready.
    pub fn unavailable_note(&self) -> Option<String> {
        let state = self.state();
        if state == ServerState::Ready {
            return None;
        }
        match state.next_step(self.server, Audience::Agent) {
            Some(next_step) => Some(format!("{}: {next_step}", state.summary())),
            None => Some(state.summary().to_string()),
        }
    }

    fn ready_label(&self) -> String {
        let ready = match self.connection {
            Some(_) => "connected",
            None => "ready",
        };
        match &self.verified {
            Some(shown) => format!("{ready} (verified: {shown})"),
            None => ready.to_string(),
        }
    }
}

impl ServerState {
    fn summary(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Connecting => "connecting",
            Self::NeedsTrust => "needs trust",
            Self::NeedsLogin => "needs login",
            Self::LoginUnknown => "login status unknown",
            Self::WrongAccount(_) => "wrong account",
            Self::Failed(_) => "failed",
        }
    }

    /// What gets the server working, worded for whoever reads it. A wrong account's message
    /// and a failure's error carry their own.
    fn next_step(&self, server: &ResolvedServer, audience: Audience) -> Option<String> {
        match self {
            Self::Connecting => Some(
                "its tools appear once it is up (clients that ignore tools/list_changed, like \
                 Codex, see them next session)"
                    .to_string(),
            ),
            Self::NeedsTrust => Some(trust_next_step(audience).to_string()),
            Self::NeedsLogin => Some(login_next_step(server, audience)),
            Self::LoginUnknown => Some(KEYCHAIN_NEXT_STEP.to_string()),
            Self::Ready | Self::WrongAccount(_) | Self::Failed(_) => None,
        }
    }
}

pub fn trust_next_step(audience: Audience) -> &'static str {
    match audience {
        Audience::Human => "review the config, then run `plugbay trust`",
        Audience::Agent => {
            "ask the human to review the config and run `plugbay trust` in the project directory"
        }
    }
}

pub fn login_next_step(server: &ResolvedServer, audience: Audience) -> String {
    let name = server.display_name();
    match audience {
        Audience::Human => format!("run `plugbay login {name}` (add `--token` for an API token)"),
        Audience::Agent => format!(
            "call {}__login, or ask the human to run `plugbay login {name}` (add `--token` for an API token)",
            server.tool_prefix
        ),
    }
}

pub const KEYCHAIN_NEXT_STEP: &str = "unlock the keychain, then try again";

pub async fn login_state(server: &ResolvedServer) -> LoginState {
    if !server.uses_stored_login() {
        return LoginState::NotNeeded;
    }
    match credentials::stored_login(&server.identity, &server.account).await {
        Ok(StoredLogin::OAuth) => LoginState::OAuth,
        Ok(StoredLogin::Token) => LoginState::Token,
        Ok(StoredLogin::None) => LoginState::Missing,
        Err(error) => {
            tracing::warn!(%error, server = %server.display_name(), "could not read stored credentials");
            LoginState::Unknown(format!("{error:#}"))
        }
    }
}

/// Other labels of `server` whose verified login is the same account, as `name:account`.
pub fn same_login_labels(server: &ResolvedServer, verified: &VerifiedAccounts) -> Vec<String> {
    verified
        .same_login_as(&server.identity, &server.account)
        .iter()
        .map(|account| format!("{}:{account}", server.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear() -> ResolvedServer {
        ResolvedServer::test_http("linear", "work", "https://mcp.linear.app/mcp")
    }

    fn report(server: &ResolvedServer) -> ServerReport<'_> {
        ServerReport::new(server, &VerifiedAccounts::default())
    }

    #[test]
    fn nothing_missing_is_ready() {
        let server = linear();
        let report = ServerReport {
            login: Some(LoginState::OAuth),
            ..report(&server)
        };
        assert_eq!(report.state(), ServerState::Ready);
        assert_eq!(report.status(Audience::Human), "ready");
    }

    #[test]
    fn trust_comes_before_a_missing_login() {
        let server = linear();
        let report = ServerReport {
            trust: TrustState::Untrusted,
            login: Some(LoginState::Missing),
            ..report(&server)
        };
        assert_eq!(report.state(), ServerState::NeedsTrust);
    }

    #[test]
    fn a_missing_login_comes_before_a_wrong_account() {
        let server = linear();
        let report = ServerReport {
            login: Some(LoginState::Missing),
            wrong_account: Some("linear:work is logged in as Initech".to_string()),
            ..report(&server)
        };
        assert_eq!(report.state(), ServerState::NeedsLogin);
    }

    #[test]
    fn an_unreadable_keychain_leaves_the_login_unknown() {
        let server = linear();
        let report = ServerReport {
            login: Some(LoginState::Unknown("locked".to_string())),
            ..report(&server)
        };
        assert_eq!(report.state(), ServerState::LoginUnknown);
    }

    #[test]
    fn wrong_account_turns_the_status_off() {
        let server = linear();
        let report = ServerReport {
            login: Some(LoginState::OAuth),
            wrong_account: Some("linear:work is logged in as Initech".to_string()),
            ..report(&server)
        };
        assert_eq!(
            report.status(Audience::Human),
            "wrong account: linear:work is logged in as Initech"
        );
    }

    #[test]
    fn a_connection_attempt_decides_over_stored_facts() {
        let server = linear();
        let report = ServerReport {
            connection: Some(Connection::Failed("connection refused".to_string())),
            wrong_account: Some("linear:work is logged in as Initech".to_string()),
            ..report(&server)
        };
        assert_eq!(report.status(Audience::Agent), "failed: connection refused");
        assert_eq!(report.unavailable_note().as_deref(), Some("failed"));
    }

    #[test]
    fn a_session_says_connected_with_the_verified_login() {
        let server = linear();
        let report = ServerReport {
            verified: Some("Initech".to_string()),
            connection: Some(Connection::Connected),
            ..report(&server)
        };
        assert_eq!(
            report.status(Audience::Agent),
            "connected (verified: Initech)"
        );
        assert_eq!(report.unavailable_note(), None);
    }

    #[test]
    fn login_next_step_names_the_command_for_humans() {
        let server = linear();
        let report = ServerReport {
            login: Some(LoginState::Missing),
            ..report(&server)
        };
        assert_eq!(
            report.status(Audience::Human),
            "needs login: run `plugbay login linear:work` (add `--token` for an API token)"
        );
    }

    #[test]
    fn login_next_step_offers_the_login_tool_to_agents() {
        let server = linear();
        let report = ServerReport {
            connection: Some(Connection::NeedsLogin),
            ..report(&server)
        };
        assert_eq!(
            report.unavailable_note().as_deref(),
            Some(
                "needs login: call linear__login, or ask the human to run `plugbay login linear:work` (add `--token` for an API token)"
            )
        );
    }

    #[test]
    fn trust_next_step_asks_agents_to_ask_the_human() {
        let server = linear();
        let report = ServerReport {
            trust: TrustState::Untrusted,
            ..report(&server)
        };
        assert_eq!(
            report.status(Audience::Human),
            "needs trust: review the config, then run `plugbay trust`"
        );
        assert_eq!(
            report.status(Audience::Agent),
            "needs trust: ask the human to review the config and run `plugbay trust` in the project directory"
        );
    }
}
