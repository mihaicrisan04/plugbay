//! What resolved here and what state each server is in, built once and rendered by `ls`,
//! `doctor`, `plugbay__status` and the session instructions, so their wording can't drift.

use crate::config::{ResolvedConfig, ResolvedServer};
use crate::credentials::{self, StoredLogin};
use crate::identity::{self, ExpectationFailure, VerifiedAccounts};
use crate::paths::Paths;
use crate::trust::Trust;

/// A resolved config with what plugbay knows about each of its servers: the CLI's view.
pub struct Report<'a> {
    /// Location, config files, disabled servers, catalog and problems.
    pub config: &'a ResolvedConfig,
    /// One per `config.servers` entry, in the same order.
    pub servers: Vec<ServerReport<'a>>,
    /// Who each login turned out to be, when plugbay asked the server.
    pub accounts: VerifiedAccounts,
}

/// Stored facts about one server. Inside a session the live connection decides instead, as
/// `ServerState::from(&ServerStatus)`.
pub struct ServerReport<'a> {
    pub server: &'a ResolvedServer,
    pub trust: TrustState,
    pub login: LoginState,
    /// Who the server said the login is, when plugbay asked it.
    pub verified: Option<String>,
    /// Set when the verified login doesn't pass the account's `expect`, or can't be checked.
    pub expectation_failure: Option<ExpectationFailure>,
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

/// The one thing that stands between a server and an agent using it. `ServerReport::state` gives
/// it from stored facts; a session gives it from its live connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Nothing missing; not connected, since only a session connects.
    Ready,
    /// Inside a session only.
    Connected,
    /// Inside a session only.
    Connecting,
    NeedsTrust,
    NeedsLogin,
    LoginUnknown,
    WrongAccount {
        shown: String,
        expected: String,
    },
    /// The accounts record can't be read, so `expect` can't be checked.
    AccountUnknown {
        expected: String,
    },
    Failed(String),
}

/// Who reads the next step: a human runs commands, an agent asks the human to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Human,
    Agent,
}

impl<'a> Report<'a> {
    /// Trust grants on disk, stored logins from the keychain, verified accounts from the record.
    pub async fn build(paths: &Paths, config: &'a ResolvedConfig) -> Report<'a> {
        let trust = Trust::new(paths);
        let accounts = VerifiedAccounts::load(paths).await;
        let mut servers = Vec::new();
        for server in &config.servers {
            let trust = if server.requires_trust_from.is_empty() {
                TrustState::NotNeeded
            } else if trust.is_server_trusted(server, config) {
                TrustState::Trusted
            } else {
                TrustState::Untrusted
            };
            let login = login_state(server).await;
            servers.push(ServerReport::new(server, trust, login, &accounts));
        }
        Report {
            config,
            servers,
            accounts,
        }
    }

    /// Other labels of `server` logged in to the same account, as `name:account`.
    pub fn same_login_as(&self, server: &ResolvedServer) -> Vec<String> {
        same_login_labels(server, &self.accounts)
    }
}

impl<'a> ServerReport<'a> {
    pub fn new(
        server: &'a ResolvedServer,
        trust: TrustState,
        login: LoginState,
        accounts: &VerifiedAccounts,
    ) -> Self {
        Self {
            server,
            trust,
            login,
            verified: accounts
                .shown(&server.identity, &server.account)
                .map(str::to_string),
            expectation_failure: identity::expectation_failure(server, accounts),
        }
    }

    /// In the order a session meets them: trust before connecting, the login while connecting,
    /// the verified account once connected.
    pub fn state(&self) -> ServerState {
        if self.trust == TrustState::Untrusted {
            return ServerState::NeedsTrust;
        }
        match &self.login {
            LoginState::Missing => return ServerState::NeedsLogin,
            LoginState::Unknown(_) => return ServerState::LoginUnknown,
            LoginState::NotNeeded | LoginState::OAuth | LoginState::Token => {}
        }
        match &self.expectation_failure {
            Some(failure) => ServerState::from(failure),
            None => ServerState::Ready,
        }
    }

    /// `ls`'s STATUS column.
    pub fn status(&self) -> String {
        self.state()
            .status(self.server, self.verified.as_deref(), Audience::Human)
    }
}

impl From<&ExpectationFailure> for ServerState {
    fn from(failure: &ExpectationFailure) -> Self {
        match failure {
            ExpectationFailure::Mismatch { shown, expected } => Self::WrongAccount {
                shown: shown.clone(),
                expected: expected.clone(),
            },
            ExpectationFailure::Unreadable { expected } => Self::AccountUnknown {
                expected: expected.clone(),
            },
        }
    }
}

impl ServerState {
    /// Whether an agent gets the server's tools.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Ready | Self::Connected)
    }

    fn summary(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Connected => "connected",
            Self::Connecting => "connecting",
            Self::NeedsTrust => "needs trust",
            Self::NeedsLogin => "needs login",
            Self::LoginUnknown => "login status unknown",
            Self::WrongAccount { .. } => "wrong account",
            Self::AccountUnknown { .. } => "account unknown",
            Self::Failed(_) => "failed",
        }
    }

    /// What went wrong, for the states the summary alone doesn't explain.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::WrongAccount { shown, expected } => Some(format!(
                "logged in as {shown}, but this project expects {expected}"
            )),
            Self::AccountUnknown { expected } => Some(format!(
                "plugbay can't read its record of who this login is, so it can't check for {expected}"
            )),
            Self::Failed(error) => Some(error.clone()),
            Self::Ready
            | Self::Connected
            | Self::Connecting
            | Self::NeedsTrust
            | Self::NeedsLogin
            | Self::LoginUnknown => None,
        }
    }

    /// What gets the server working, worded for whoever reads it.
    pub fn next_step(&self, server: &ResolvedServer, audience: Audience) -> Option<String> {
        match self {
            Self::Connecting => {
                debug_assert_eq!(audience, Audience::Agent, "only a session connects");
                Some(
                    "its tools appear once it is up (clients that ignore tools/list_changed, like \
                     Codex, see them next session)"
                        .to_string(),
                )
            }
            Self::NeedsTrust => Some(trust_next_step(audience).to_string()),
            Self::NeedsLogin => Some(login_next_step(server, audience)),
            Self::LoginUnknown => Some(KEYCHAIN_NEXT_STEP.to_string()),
            Self::WrongAccount { expected, .. } => Some(format!(
                "{} and pick {expected}",
                relogin_next_step(server, audience)
            )),
            Self::AccountUnknown { .. } => Some(match audience {
                Audience::Human => ACCOUNTS_RECORD_NEXT_STEP.to_string(),
                Audience::Agent => format!("ask the human to {ACCOUNTS_RECORD_NEXT_STEP}"),
            }),
            Self::Ready | Self::Connected | Self::Failed(_) => None,
        }
    }

    /// The full status line: `ls`, `ls --all` and `plugbay__status`.
    pub fn status(
        &self,
        server: &ResolvedServer,
        verified: Option<&str>,
        audience: Audience,
    ) -> String {
        if self.is_usable()
            && let Some(shown) = verified
        {
            return format!("{} (verified: {shown})", self.summary());
        }
        self.summary_with(&[self.detail(), self.next_step(server, audience)])
    }

    /// Why an agent can't use the server and what to do, without the detail `status` adds;
    /// `None` when it is usable.
    pub fn unavailable_note(&self, server: &ResolvedServer) -> Option<String> {
        if self.is_usable() {
            return None;
        }
        Some(self.summary_with(&[self.next_step(server, Audience::Agent)]))
    }

    /// `name — status`, for an agent whose tool call or enable this state stopped.
    pub fn explain_to_agent(&self, server: &ResolvedServer) -> String {
        format!(
            "{} — {}",
            server.display_name(),
            self.status(server, None, Audience::Agent)
        )
    }

    /// `summary: a; b`, or just the summary when nothing explains it.
    fn summary_with(&self, explanation: &[Option<String>]) -> String {
        let explanation: Vec<&str> = explanation.iter().flatten().map(String::as_str).collect();
        if explanation.is_empty() {
            return self.summary().to_string();
        }
        format!("{}: {}", self.summary(), explanation.join("; "))
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

/// Logging in again picks the account; a plain `login` would keep the stored one.
pub fn relogin_next_step(server: &ResolvedServer, audience: Audience) -> String {
    let name = server.display_name();
    let command = format!("`plugbay logout {name} && plugbay login {name}`");
    match audience {
        Audience::Human => format!("run {command}"),
        Audience::Agent => format!("ask the human to run {command}"),
    }
}

pub const KEYCHAIN_NEXT_STEP: &str = "unlock the keychain, then try again";

const ACCOUNTS_RECORD_NEXT_STEP: &str =
    "delete `accounts.toml` in plugbay's state directory (it is rebuilt on the next connection)";

pub async fn login_state(server: &ResolvedServer) -> LoginState {
    if !server.uses_stored_login() {
        return LoginState::NotNeeded;
    }
    match credentials::stored_login(&server.identity, &server.account).await {
        Ok(StoredLogin::OAuth) => LoginState::OAuth,
        Ok(StoredLogin::Token { .. }) => LoginState::Token,
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

    const LINEAR: &str = "https://mcp.linear.app/mcp";

    fn linear() -> ResolvedServer {
        ResolvedServer::test_http("linear", "work", LINEAR)
    }

    fn expecting(expected: &str) -> ResolvedServer {
        let mut server = linear();
        server
            .account_metadata
            .insert(identity::EXPECT_KEY.to_string(), expected.to_string());
        server
    }

    fn report(server: &ResolvedServer, trust: TrustState, login: LoginState) -> ServerReport<'_> {
        ServerReport::new(server, trust, login, &VerifiedAccounts::default())
    }

    fn wrong_account() -> ServerState {
        ServerState::WrongAccount {
            shown: "Initech".to_string(),
            expected: "Personal".to_string(),
        }
    }

    #[test]
    fn nothing_missing_is_ready() {
        let server = linear();
        let report = report(&server, TrustState::NotNeeded, LoginState::OAuth);
        assert_eq!(report.state(), ServerState::Ready);
        assert_eq!(report.status(), "ready");
    }

    #[test]
    fn trust_comes_before_a_missing_login() {
        let server = linear();
        let report = report(&server, TrustState::Untrusted, LoginState::Missing);
        assert_eq!(report.state(), ServerState::NeedsTrust);
    }

    #[test]
    fn a_missing_login_comes_before_a_wrong_account() {
        let server = expecting("Personal");
        let accounts = VerifiedAccounts::test_with(&[(LINEAR, "work", "Initech")]);
        let report = ServerReport::new(
            &server,
            TrustState::NotNeeded,
            LoginState::Missing,
            &accounts,
        );
        assert_eq!(report.state(), ServerState::NeedsLogin);
    }

    #[test]
    fn an_unreadable_keychain_leaves_the_login_unknown() {
        let server = linear();
        let report = report(
            &server,
            TrustState::NotNeeded,
            LoginState::Unknown("locked".to_string()),
        );
        assert_eq!(report.state(), ServerState::LoginUnknown);
    }

    #[test]
    fn a_verified_login_that_fails_expect_is_the_wrong_account() {
        let server = expecting("Personal");
        let accounts = VerifiedAccounts::test_with(&[(LINEAR, "work", "Initech")]);
        let report =
            ServerReport::new(&server, TrustState::NotNeeded, LoginState::OAuth, &accounts);
        assert_eq!(report.state(), wrong_account());
        assert_eq!(
            report.status(),
            "wrong account: logged in as Initech, but this project expects Personal; \
             run `plugbay logout linear:work && plugbay login linear:work` and pick Personal"
        );
    }

    #[test]
    fn an_unreadable_accounts_record_is_not_a_wrong_account() {
        let server = expecting("Personal");
        let accounts = VerifiedAccounts::test_unreadable();
        let report =
            ServerReport::new(&server, TrustState::NotNeeded, LoginState::OAuth, &accounts);
        assert_eq!(
            report.state(),
            ServerState::AccountUnknown {
                expected: "Personal".to_string()
            }
        );
        assert_eq!(
            report.status(),
            "account unknown: plugbay can't read its record of who this login is, so it can't \
             check for Personal; delete `accounts.toml` in plugbay's state directory (it is \
             rebuilt on the next connection)"
        );
    }

    #[test]
    fn ready_shows_the_verified_login() {
        let server = linear();
        let accounts = VerifiedAccounts::test_with(&[(LINEAR, "work", "Initech")]);
        let report =
            ServerReport::new(&server, TrustState::NotNeeded, LoginState::OAuth, &accounts);
        assert_eq!(report.status(), "ready (verified: Initech)");
    }

    #[test]
    fn a_failure_shows_its_error_in_the_status_but_not_the_note() {
        let server = linear();
        let state = ServerState::Failed("connection refused".to_string());
        assert_eq!(
            state.status(&server, None, Audience::Agent),
            "failed: connection refused"
        );
        assert_eq!(state.unavailable_note(&server).as_deref(), Some("failed"));
    }

    #[test]
    fn a_session_says_connected_with_the_verified_login() {
        let server = linear();
        let state = ServerState::Connected;
        assert_eq!(
            state.status(&server, Some("Initech"), Audience::Agent),
            "connected (verified: Initech)"
        );
        assert_eq!(state.unavailable_note(&server), None);
    }

    #[test]
    fn the_note_for_a_wrong_account_says_what_to_do() {
        let server = linear();
        assert_eq!(
            wrong_account().unavailable_note(&server).as_deref(),
            Some(
                "wrong account: ask the human to run `plugbay logout linear:work && plugbay login linear:work` and pick Personal"
            )
        );
    }

    #[test]
    fn login_next_step_names_the_command_for_humans() {
        let server = linear();
        let report = report(&server, TrustState::NotNeeded, LoginState::Missing);
        assert_eq!(
            report.status(),
            "needs login: run `plugbay login linear:work` (add `--token` for an API token)"
        );
    }

    #[test]
    fn login_next_step_offers_the_login_tool_to_agents() {
        let server = linear();
        assert_eq!(
            ServerState::NeedsLogin.unavailable_note(&server).as_deref(),
            Some(
                "needs login: call linear__login, or ask the human to run `plugbay login linear:work` (add `--token` for an API token)"
            )
        );
    }

    #[test]
    fn trust_next_step_asks_agents_to_ask_the_human() {
        let server = linear();
        let state = ServerState::NeedsTrust;
        assert_eq!(
            state.status(&server, None, Audience::Human),
            "needs trust: review the config, then run `plugbay trust`"
        );
        assert_eq!(
            state.status(&server, None, Audience::Agent),
            "needs trust: ask the human to review the config and run `plugbay trust` in the project directory"
        );
    }

    #[test]
    fn an_agent_hears_which_server_stopped_it() {
        let server = linear();
        assert_eq!(
            ServerState::Failed("connection refused".to_string()).explain_to_agent(&server),
            "linear:work — failed: connection refused"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "only a session connects")]
    fn connecting_is_never_worded_for_humans() {
        ServerState::Connecting.next_step(&linear(), Audience::Human);
    }
}
