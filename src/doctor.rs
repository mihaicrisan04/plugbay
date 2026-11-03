//! `plugbay doctor`: installation, daemon, config, trust, credentials and client setup checks.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::clients::{self, Client, NativeServer, SetupState, ShimCommand};
use crate::config::{ResolvedConfig, ResolvedServer, ServerTransport};
use crate::paths::Paths;
use crate::protocol::{self, ControlRequest, Skew};
use crate::report::{self, Audience, LoginState, Report, ServerReport, ServerState, TrustState};

const DAEMON_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Error,
}

/// One line of doctor output, with what to do about it when it isn't ok.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    status: Status,
    title: String,
    hint: Option<String>,
}

impl Check {
    fn ok(title: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            title: title.into(),
            hint: None,
        }
    }

    fn warn(title: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            title: title.into(),
            hint: Some(hint.into()),
        }
    }

    fn error(title: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            title: title.into(),
            hint: Some(hint.into()),
        }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.status {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Error => "error",
        };
        write!(formatter, "{label:<6}{}", self.title)?;
        if let Some(hint) = &self.hint {
            write!(formatter, "\n      → {hint}")?;
        }
        Ok(())
    }
}

pub async fn run(paths: &Paths, project: Option<PathBuf>) -> anyhow::Result<()> {
    let project_dir = match &project {
        Some(project) => project.clone(),
        None => std::env::current_dir()?,
    };

    let mut checks = vec![
        check_binary(),
        check_state_dir(&paths.state_dir),
        check_daemon(paths).await,
    ];
    checks.extend(check_client_setup(&paths.home));

    match crate::commands::resolve_here(paths, project).await {
        Ok(config) => {
            checks.extend(check_resolved_config(&config));
            checks.extend(check_servers(paths, &config).await);
            checks.extend(check_native_duplicates(&paths.home, &config));
        }
        Err(error) => checks.push(Check::error(
            format!(
                "config for {} does not resolve: {error:#}",
                project_dir.display()
            ),
            "fix the file named above, then run `plugbay doctor` again",
        )),
    }

    for check in &checks {
        println!("{check}");
    }
    let errors = checks
        .iter()
        .filter(|check| check.status == Status::Error)
        .count();
    if errors > 0 {
        anyhow::bail!("doctor found {errors} error(s)");
    }
    Ok(())
}

fn check_binary() -> Check {
    match ShimCommand::current() {
        Ok(shim) => Check::ok(format!(
            "plugbay {} at {}",
            protocol::version(),
            shim.command
        )),
        Err(error) => Check::error(
            format!("cannot locate the plugbay binary: {error:#}"),
            "reinstall plugbay",
        ),
    }
}

fn check_state_dir(state_dir: &Path) -> Check {
    use std::os::unix::fs::PermissionsExt;

    let metadata = match std::fs::metadata(state_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Check::ok(format!(
                "state dir {} not created yet (the daemon creates it on first start)",
                state_dir.display()
            ));
        }
        Err(error) => {
            return Check::error(
                format!("cannot read state dir {}: {error}", state_dir.display()),
                "check the permissions of its parent directory",
            );
        }
    };

    let mode = metadata.permissions().mode() & 0o777;
    if mode == 0o700 {
        Check::ok(format!("state dir {} (0700)", state_dir.display()))
    } else {
        Check::error(
            format!(
                "state dir {} is {mode:o}, other users can reach the daemon socket",
                state_dir.display()
            ),
            format!("chmod 700 {}", state_dir.display()),
        )
    }
}

async fn check_daemon(paths: &Paths) -> Check {
    let status = tokio::time::timeout(
        DAEMON_TIMEOUT,
        crate::protocol::send_control(paths, ControlRequest::Status),
    )
    .await;
    let response = match status {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => return Check::ok("daemon not running (starts on demand)"),
        Err(_) => {
            return Check::warn(
                "daemon did not answer within 2s",
                "run `plugbay stop`, or look at the daemon log",
            );
        }
    };

    let problem = format!(
        "daemon runs version {}, this binary is {}",
        response.version,
        protocol::version()
    );
    match protocol::skew(&response.version) {
        Skew::Same => Check::ok(format!(
            "daemon {} running, {} session(s)",
            response.version, response.sessions
        )),
        Skew::PeerNewer => Check::warn(
            problem,
            "an older plugbay may come first on PATH: check `which -a plugbay`",
        ),
        Skew::PeerOlder if response.can_retire => Check::warn(
            problem,
            "fixes itself: the next agent that connects moves every agent to the new daemon (`plugbay stop` does it now)",
        ),
        Skew::PeerOlder => Check::warn(
            problem,
            "run `plugbay stop`; agents started before the upgrade then need a restart",
        ),
        Skew::Unknown => Check::warn(
            problem,
            "the versions can't be compared, so neither replaces the other: run `plugbay stop` to start this version's daemon",
        ),
    }
}

fn check_resolved_config(config: &ResolvedConfig) -> Vec<Check> {
    let directory = match &config.location.config_dir {
        Some(directory) => directory.display().to_string(),
        None => "no project (global config only)".to_string(),
    };
    let mut checks = vec![Check::ok(format!(
        "{} server(s) resolved for {directory} from {} file(s)",
        config.servers.len(),
        config.files.len()
    ))];
    for problem in &config.problems {
        checks.push(Check::warn(
            problem.message.clone(),
            "run `plugbay why <server>` to see which files set it",
        ));
    }
    checks
}

async fn check_servers(paths: &Paths, config: &ResolvedConfig) -> Vec<Check> {
    let report = Report::build(paths, config).await;
    let mut checks = Vec::new();
    for server in &report.servers {
        checks.extend(check_server_trust(server));
        checks.extend(check_server_credentials(server));
        checks.extend(check_account_expectation(server));
        checks.extend(check_verified_account(
            server,
            &report.same_login_as(server.server),
        ));
    }
    checks.extend(check_op_cli(&config.servers));
    checks
}

fn check_server_trust(report: &ServerReport) -> Option<Check> {
    let name = describe(report.server);
    match report.trust {
        TrustState::NotNeeded => None,
        TrustState::Trusted => Some(Check::ok(format!("{name}: trusted"))),
        TrustState::Untrusted => Some(Check::warn(
            format!("{name}: not trusted, agents won't get it"),
            report::trust_next_step(Audience::Human),
        )),
    }
}

fn check_server_credentials(report: &ServerReport) -> Option<Check> {
    let name = describe(report.server);
    let check = match &report.login {
        LoginState::NotNeeded => return None,
        LoginState::OAuth => Check::ok(format!("{name}: logged in")),
        LoginState::Token => Check::ok(format!("{name}: token stored")),
        LoginState::Missing => Check::warn(
            format!("{name}: no stored login or token (fine if the server needs none)"),
            report::login_next_step(report.server, Audience::Human),
        ),
        LoginState::Unknown(error) => Check::warn(
            format!("{name}: cannot read the keychain: {error}"),
            report::KEYCHAIN_NEXT_STEP,
        ),
    };
    Some(check)
}

/// A login that fails `expect`, or an `expect` that can't be checked: agents won't get the server.
fn check_account_expectation(report: &ServerReport) -> Option<Check> {
    let state = ServerState::from(report.expectation_failure.as_ref()?);
    let problem = state.detail()?;
    let next_step = state.next_step(report.server, Audience::Human)?;
    Some(Check::error(
        format!(
            "{}: {problem}; agents won't get it",
            report.server.display_name()
        ),
        next_step,
    ))
}

/// Who a login that passes `expect` turned out to be, when the server was asked; a warning when
/// another label of this server is the same account (fine if intended, a mix-up if not).
fn check_verified_account(report: &ServerReport, same_login_as: &[String]) -> Option<Check> {
    if report.expectation_failure.is_some() {
        return None;
    }
    let name = report.server.display_name();
    let shown = report.verified.as_ref()?;
    if same_login_as.is_empty() {
        return Some(Check::ok(format!("{name}: verified as {shown}")));
    }
    Some(Check::warn(
        format!(
            "{name} sees the same account as {} ({shown}); if they are meant to be different accounts, one label is logged in to the wrong one",
            same_login_as.join(", ")
        ),
        format!(
            "if so, {} and pick the account this label is meant for",
            report::relogin_next_step(report.server, Audience::Human)
        ),
    ))
}

/// `linear:initech (https://mcp.linear.app/mcp)`
fn describe(server: &ResolvedServer) -> String {
    format!("{} ({})", server.display_name(), server.transport.target())
}

fn check_op_cli(servers: &[ResolvedServer]) -> Option<Check> {
    let users: Vec<String> = servers
        .iter()
        .filter(|server| uses_op_refs(server))
        .map(ResolvedServer::display_name)
        .collect();
    if users.is_empty() {
        return None;
    }
    let check = match clients::find_on_path("op") {
        Some(op) => Check::ok(format!("1Password CLI at {}", op.display())),
        None => Check::error(
            format!(
                "{} use `${{op://...}}` refs but the 1Password CLI `op` is not on PATH",
                users.join(", ")
            ),
            "install it (`brew install 1password-cli`) and sign in",
        ),
    };
    Some(check)
}

fn uses_op_refs(server: &ResolvedServer) -> bool {
    let values = match &server.transport {
        ServerTransport::Http { headers, .. } => headers,
        ServerTransport::Stdio { env, .. } => env,
    };
    values.values().any(|value| value.contains("${op://"))
}

fn check_client_setup(home: &Path) -> Vec<Check> {
    let Ok(shim) = ShimCommand::current() else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    for client in Client::ALL {
        if client.is_installed(home) {
            checks.push(setup_check(client, client.setup_state(home, &shim)));
        } else {
            checks.push(Check::ok(format!(
                "{} not found, skipped",
                client.display_name()
            )));
        }
    }
    checks
}

fn setup_check(client: Client, state: anyhow::Result<SetupState>) -> Check {
    let fix = format!("plugbay setup --client {}", client.cli_name());
    let client = client.display_name();
    match state {
        Ok(SetupState::UpToDate) => Check::ok(format!("{client}: `plugbay` entry set up")),
        Ok(SetupState::Missing) => Check::warn(
            format!("{client}: no `plugbay` MCP entry"),
            format!("run `{fix}`"),
        ),
        Ok(SetupState::Outdated { command }) => Check::warn(
            format!("{client}: `plugbay` entry runs `{command}`, not this binary"),
            format!("run `{fix}`"),
        ),
        Err(error) => Check::warn(
            format!("{client}: cannot read its config: {error:#}"),
            "check the file named above",
        ),
    }
}

fn check_native_duplicates(home: &Path, config: &ResolvedConfig) -> Vec<Check> {
    let project_dir = config.location.working_dir.as_deref();
    let natives = match clients::read_native_servers(home, project_dir) {
        Ok(natives) => natives,
        Err(error) => {
            return vec![Check::warn(
                format!("cannot read native MCP config: {error:#}"),
                "check the file named above",
            )];
        }
    };

    let duplicates = find_duplicates(&natives, &config.servers);
    if duplicates.is_empty() {
        return vec![Check::ok("no native MCP servers duplicate plugbay's")];
    }
    let mut checks = Vec::new();
    for (native, server) in duplicates {
        checks.push(Check::warn(
            format!(
                "`{}` from {} is also provided by plugbay as `{}`: the agent sees both",
                native.name,
                native.source,
                describe(server)
            ),
            format!(
                "{}, or run `plugbay import --apply --remove-native`",
                native.removal_hint(home)
            ),
        ));
    }
    checks
}

/// Native servers that plugbay also provides here, by name or by URL, with the plugbay server they shadow.
fn find_duplicates<'a>(
    natives: &'a [NativeServer],
    servers: &'a [ResolvedServer],
) -> Vec<(&'a NativeServer, &'a ResolvedServer)> {
    let mut duplicates = Vec::new();
    for native in natives {
        if let Some(server) = servers.iter().find(|server| is_same_server(native, server)) {
            duplicates.push((native, server));
        }
    }
    duplicates
}

fn is_same_server(native: &NativeServer, server: &ResolvedServer) -> bool {
    if clients::normalize_server_name(&native.name).as_deref() == Some(server.name.as_str()) {
        return true;
    }
    native.url_identity().as_ref() == Some(&server.identity)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::clients::{ClaudeScope, NativeSource, NativeTransport};
    use crate::config::{ServerIdentity, ToolFilter};
    use crate::identity::{self, VerifiedAccounts};

    fn resolved_http(name: &str, url: &str) -> ResolvedServer {
        let url: url::Url = url.parse().unwrap();
        ResolvedServer {
            name: name.to_string(),
            account: "default".to_string(),
            tool_prefix: name.to_string(),
            transport: ServerTransport::Http {
                url: url.clone(),
                headers: BTreeMap::new(),
            },
            identity: ServerIdentity::for_url(&url),
            tool_filter: ToolFilter::default(),
            account_metadata: BTreeMap::new(),
            provenance: BTreeMap::new(),
            requires_trust_from: Vec::new(),
        }
    }

    fn native_http(name: &str, url: &str) -> NativeServer {
        NativeServer {
            name: name.to_string(),
            source: NativeSource::Claude(ClaudeScope::User),
            transport: NativeTransport::Http {
                url: url.to_string(),
                headers: BTreeMap::new(),
            },
            notes: Vec::new(),
        }
    }

    #[test]
    fn native_server_with_same_url_is_a_duplicate() {
        let natives = [native_http("linear-server", "https://mcp.linear.app/mcp/")];
        let servers = [resolved_http("linear", "https://mcp.linear.app/mcp")];
        assert_eq!(find_duplicates(&natives, &servers).len(), 1);
    }

    #[test]
    fn native_server_with_same_normalised_name_is_a_duplicate() {
        let natives = [native_http("Sentry_Prod", "https://other.example/mcp")];
        let servers = [resolved_http("sentry-prod", "https://mcp.sentry.dev/mcp")];
        assert_eq!(find_duplicates(&natives, &servers).len(), 1);
    }

    #[test]
    fn unrelated_native_server_is_not_a_duplicate() {
        let natives = [native_http("paper", "https://paper.example/mcp")];
        let servers = [resolved_http("linear", "https://mcp.linear.app/mcp")];
        assert!(find_duplicates(&natives, &servers).is_empty());
    }

    #[test]
    fn op_refs_in_headers_are_detected() {
        let mut server = resolved_http("posthog", "https://mcp.posthog.com/mcp");
        if let ServerTransport::Http { headers, .. } = &mut server.transport {
            headers.insert(
                "Authorization".to_string(),
                "Bearer ${op://Clients/posthog/key}".to_string(),
            );
        }
        assert!(uses_op_refs(&server));
    }

    #[test]
    fn a_fresh_install_without_a_state_dir_is_fine() {
        let root = tempfile::TempDir::new().unwrap();
        let missing = root.path().join("state");
        assert_eq!(check_state_dir(&missing).status, Status::Ok);
    }

    #[test]
    fn group_writable_state_dir_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(check_state_dir(root.path()).status, Status::Error);
    }

    #[test]
    fn private_state_dir_is_ok() {
        let root = tempfile::TempDir::new().unwrap();
        crate::paths::create_private_dir(root.path()).unwrap();
        assert_eq!(check_state_dir(root.path()).status, Status::Ok);
    }

    const LINEAR: &str = "https://mcp.linear.app/mcp";

    fn oauth_report<'a>(
        server: &'a ResolvedServer,
        verified: &VerifiedAccounts,
    ) -> ServerReport<'a> {
        ServerReport::new(server, TrustState::NotNeeded, LoginState::OAuth, verified)
    }

    #[test]
    fn two_labels_with_one_login_warn() {
        let verified = VerifiedAccounts::test_with(&[
            (LINEAR, "default", "Initech"),
            (LINEAR, "initech", "Initech"),
        ]);
        let server = resolved_http("linear", "https://mcp.linear.app/mcp");
        let check = check_verified_account(
            &oauth_report(&server, &verified),
            &report::same_login_labels(&server, &verified),
        )
        .unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.title.contains("same account as linear:initech"),
            "{check}"
        );
        assert!(
            check
                .hint
                .unwrap()
                .contains("plugbay logout linear:default")
        );
    }

    #[test]
    fn unexpected_login_is_an_error() {
        let verified = VerifiedAccounts::test_with(&[(LINEAR, "default", "Initech")]);
        let mut server = resolved_http("linear", "https://mcp.linear.app/mcp");
        server
            .account_metadata
            .insert(identity::EXPECT_KEY.to_string(), "Personal".to_string());
        let report = oauth_report(&server, &verified);
        let check = check_account_expectation(&report).unwrap();
        assert_eq!(check.status, Status::Error);
        assert_eq!(
            check.title,
            "linear:default: logged in as Initech, but this project expects Personal; agents won't get it"
        );
        assert!(check_verified_account(&report, &[]).is_none());
    }

    #[test]
    fn unreadable_accounts_record_is_not_called_a_wrong_login() {
        let mut server = resolved_http("linear", "https://mcp.linear.app/mcp");
        server
            .account_metadata
            .insert(identity::EXPECT_KEY.to_string(), "Personal".to_string());
        let check =
            check_account_expectation(&oauth_report(&server, &VerifiedAccounts::test_unreadable()))
                .unwrap();
        assert!(check.title.contains("can't check for Personal"), "{check}");
        assert!(check.hint.unwrap().contains("accounts.toml"));
    }

    #[test]
    fn unverified_login_adds_no_check() {
        let server = resolved_http("linear", "https://mcp.linear.app/mcp");
        let report = oauth_report(&server, &VerifiedAccounts::default());
        assert!(check_verified_account(&report, &[]).is_none());
    }

    #[test]
    fn checks_print_status_title_and_hint() {
        let check = Check::warn(
            "linear:default: no stored login",
            "plugbay login linear:default",
        );
        insta::assert_snapshot!(check.to_string());
    }
}
