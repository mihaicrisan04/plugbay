//! `plugbay login`, `plugbay logout` and `plugbay trust`.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};

use crate::commands::inspect::display_path;
use crate::commands::{
    notify_credentials_changed, notify_daemon_reload, resolve_everywhere, resolve_here,
};
use crate::config::{
    self, ConfigFile, ConfigScope, ResolvedConfig, ResolvedServer, ServerTransport, SessionPin,
};
use crate::credentials::{StaticToken, StoredLogin};
use crate::paths::Paths;
use crate::project::ProjectLocation;
use crate::protocol::{ControlRequest, send_control};
use crate::secrets;
use crate::trust::Trust;
use crate::{credentials, identity, oauth};

/// Where `plugbay login` reads a static API token from, instead of starting an OAuth login.
#[derive(clap::Args, Debug, Default)]
#[group(id = "token_source", multiple = false)]
pub struct TokenOptions {
    /// Store an API token instead of logging in with OAuth. Typed at a hidden prompt, or piped on stdin.
    #[arg(long)]
    pub token: bool,
    /// Store the API token held in this environment variable.
    #[arg(long, value_name = "VARIABLE")]
    pub from_env: Option<String>,
    /// Send the token in this header instead of `Authorization: Bearer <token>`.
    #[arg(long, value_name = "NAME", requires = "token_source")]
    pub header: Option<String>,
}

pub async fn login(
    paths: &Paths,
    target: Option<&str>,
    cd: Option<PathBuf>,
    token_options: TokenOptions,
) -> anyhow::Result<()> {
    let Some(target) = target else {
        if token_options.token || token_options.from_env.is_some() {
            bail!("name the server to store a token for: `plugbay login <server> --token`");
        }
        return login_to_everything_missing(paths, cd).await;
    };
    let server = find_server(paths, cd, target).await?;
    match read_token(&token_options, &server).await? {
        Some(token) => {
            login_with_token(paths, &server, &token, token_options.header.as_deref()).await
        }
        None => login_with_oauth(paths, &server).await,
    }
}

/// `target` as this directory resolves it; otherwise the one match among every directory
/// plugbay has seen; otherwise the catalog or the registry.
async fn find_server(
    paths: &Paths,
    cd: Option<PathBuf>,
    target: &str,
) -> anyhow::Result<ResolvedServer> {
    let config = resolve_here(paths, cd).await?;
    let (name, account) = split_target(target);
    if !config.server(name).is_empty() {
        return select_server(&config, target).cloned();
    }

    let mut matches: Vec<(ResolvedServer, String)> = Vec::new();
    for other in resolve_everywhere(paths)? {
        for server in other.server(name) {
            if account.is_some_and(|account| account != server.account) {
                continue;
            }
            let already_found = matches.iter().any(|(found, _)| {
                found.identity == server.identity && found.account == server.account
            });
            if !already_found {
                let used_in = match &other.location.config_dir {
                    Some(directory) => display_path(directory, &paths.home),
                    None => "everywhere".to_string(),
                };
                matches.push((server.clone(), used_in));
            }
        }
    }

    match matches.as_slice() {
        [(server, _)] => Ok(server.clone()),
        [] => from_catalog(paths, name, account).with_context(|| {
            format!("no server named {name} is used here or in any directory plugbay has seen")
        }),
        several => {
            let choices: Vec<String> = several
                .iter()
                .map(|(server, used_in)| format!("{} ({used_in})", server.display_name()))
                .collect();
            bail!(
                "{name} is used with several accounts; choose one of: {}",
                choices.join(", ")
            )
        }
    }
}

/// A server nobody pins yet, defined in the catalog or the registry.
fn from_catalog(
    paths: &Paths,
    name: &str,
    account: Option<&str>,
) -> anyhow::Result<ResolvedServer> {
    let pins = [(
        name.to_string(),
        SessionPin::On {
            account: account.map(str::to_string),
        },
    )]
    .into();
    let config = config::resolve_with_pins(paths, &ProjectLocation::global_only(), &pins)?;
    let Some(server) = config.server(name).into_iter().next() else {
        bail!("it is not in the catalog or the registry either");
    };
    Ok(server.clone())
}

/// Every remote account in use anywhere that has no stored login yet, one after the other.
async fn login_to_everything_missing(paths: &Paths, cd: Option<PathBuf>) -> anyhow::Result<()> {
    resolve_here(paths, cd).await?;
    let mut missing: Vec<ResolvedServer> = Vec::new();
    for config in resolve_everywhere(paths)? {
        for server in config.servers {
            if !server.uses_stored_login() {
                continue;
            }
            let already_listed = missing.iter().any(|listed| {
                listed.identity == server.identity && listed.account == server.account
            });
            if already_listed {
                continue;
            }
            if credentials::stored_login(&server.identity, &server.account).await?
                == StoredLogin::None
            {
                missing.push(server);
            }
        }
    }

    if missing.is_empty() {
        println!("Every server account plugbay knows about is logged in.");
        return Ok(());
    }
    let names: Vec<String> = missing.iter().map(ResolvedServer::display_name).collect();
    if !std::io::stdin().is_terminal() {
        bail!(
            "these need a login: {}; run `plugbay login` in a terminal",
            names.join(", ")
        );
    }

    println!("These need a login: {}\n", names.join(", "));
    for server in &missing {
        let question = format!(
            "Log in to {} ({})? [Y/n] ",
            server.display_name(),
            server.transport.target()
        );
        if !confirm_default_yes(&question).await? {
            continue;
        }
        if let Err(error) = login_with_oauth(paths, server).await {
            eprintln!("{}: {error:#}\n", server.display_name());
        }
    }
    Ok(())
}

async fn login_with_oauth(paths: &Paths, server: &ResolvedServer) -> anyhow::Result<()> {
    if !server.is_stdio() && !server.uses_stored_login() {
        bail!(
            "{} authenticates with its configured Authorization header; there is nothing to log in to",
            server.display_name()
        );
    }

    let flow = match oauth::start_login(server, None).await {
        Ok(flow) => flow,
        Err(error) if std::io::stdin().is_terminal() && !server.is_stdio() => {
            return offer_token_instead(paths, server, error).await;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "could not start an OAuth login for {name}; if it takes an API token instead, run `plugbay login {name} --token`",
                    name = server.display_name()
                )
            });
        }
    };
    println!("Log in to {} in your browser:", server.display_name());
    println!("  {}", flow.authorization_url);
    if let Err(error) = open::that_detached(&flow.authorization_url) {
        tracing::warn!(%error, "could not open the browser; open the URL above yourself");
    }

    flow.finish().await?;
    identity::forget(paths, &server.identity, &server.account).await?;
    println!("Logged in to {}.", server.display_name());
    notify_credentials_changed(paths, server).await;
    Ok(())
}

/// Servers without OAuth usually take an API token, so ask for one rather than failing.
async fn offer_token_instead(
    paths: &Paths,
    server: &ResolvedServer,
    oauth_error: anyhow::Error,
) -> anyhow::Result<()> {
    println!(
        "{} has no OAuth login ({oauth_error:#}).",
        server.display_name()
    );
    let prompt = "Paste its API token to store instead (input hidden, empty to cancel): ";
    let token = prompt_hidden(prompt.to_string()).await?;
    if token.trim().is_empty() {
        bail!("no token given; nothing stored");
    }
    login_with_token(paths, server, &token, None).await
}

/// The token is bound to the server's url + account, so no config file ever names or holds it.
async fn login_with_token(
    paths: &Paths,
    server: &ResolvedServer,
    token: &str,
    header: Option<&str>,
) -> anyhow::Result<()> {
    if server.is_stdio() {
        bail!(
            "{} is a stdio server; pass it secrets through `env` in its config instead",
            server.display_name()
        );
    }
    if !server.uses_stored_login() {
        bail!(
            "{} already sends an Authorization header from config; remove it there to use a stored token",
            server.display_name()
        );
    }

    let token = StaticToken::new(token, header)?;
    credentials::save_token(&server.identity, &server.account, &token).await?;
    identity::forget(paths, &server.identity, &server.account).await?;
    println!(
        "Stored a token for {} (sent only to {}).",
        server.display_name(),
        server.identity.masked()
    );
    notify_credentials_changed(paths, server).await;
    Ok(())
}

/// The token to store, or `None` for an OAuth login.
async fn read_token(
    options: &TokenOptions,
    server: &ResolvedServer,
) -> anyhow::Result<Option<String>> {
    if let Some(variable) = &options.from_env {
        let token = std::env::var(variable)
            .with_context(|| format!("the environment variable {variable} is not set"))?;
        return Ok(Some(token));
    }
    if !options.token {
        return Ok(None);
    }

    let prompt = format!("Token for {} (input hidden): ", server.display_name());
    Ok(Some(prompt_hidden(prompt).await?))
}

/// A hidden prompt on a terminal; piped input is read whole.
async fn prompt_hidden(prompt: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        if std::io::stdin().is_terminal() {
            rpassword::prompt_password(prompt)
        } else {
            std::io::read_to_string(std::io::stdin())
        }
    })
    .await
    .context("reading the token failed")?
    .context("reading the token")
}

pub async fn logout(paths: &Paths, target: &str, cd: Option<PathBuf>) -> anyhow::Result<()> {
    let server = &find_server(paths, cd, target).await?;

    identity::forget(paths, &server.identity, &server.account).await?;
    if credentials::forget(&server.identity, &server.account).await? {
        println!("Logged out of {}.", server.display_name());
        notify_credentials_changed(paths, server).await;
    } else {
        println!("No stored credentials for {}.", server.display_name());
    }
    Ok(())
}

pub async fn trust(paths: &Paths, cd: Option<PathBuf>, revoke: bool) -> anyhow::Result<()> {
    let config = resolve_here(paths, cd).await?;
    let mut trust = Trust::new(paths);
    if revoke {
        return revoke_trust(paths, &config, &mut trust).await;
    }
    let untrusted = trust.untrusted_files(&config);
    if untrusted.is_empty() {
        println!("Nothing to trust here: every config file is already trusted or runs nothing.");
        return Ok(());
    }

    println!("These config files run commands or read secrets on your machine:\n");
    for file in &untrusted {
        println!("{}", file.path.display());
        for line in describe_executable_parts(file, &config) {
            println!("  {line}");
        }
        println!();
    }

    if !std::io::stdin().is_terminal() {
        bail!(
            "trusting config needs your confirmation: run `plugbay trust` yourself in a terminal"
        );
    }
    if !confirm("Trust these files? [y/N] ").await? {
        println!("Nothing trusted.");
        return Ok(());
    }

    for file in untrusted {
        trust.grant_always(file)?;
        println!("Trusted {}", file.path.display());
    }
    notify_daemon_reload(paths).await;
    Ok(())
}

async fn revoke_trust(
    paths: &Paths,
    config: &ResolvedConfig,
    trust: &mut Trust,
) -> anyhow::Result<()> {
    let mut revoked_any = false;
    for file in &config.files {
        if file.scope == ConfigScope::Global {
            continue;
        }
        if trust.revoke(&file.path)? {
            println!("Revoked trust for {}", file.path.display());
            revoked_any = true;
        }
    }
    if !revoked_any {
        println!("No config file here had a saved trust grant.");
    }

    // `allow_once` grants live in the daemon, so they are revoked there.
    match send_control(paths, ControlRequest::ForgetSessionTrust).await {
        Ok(_response) => println!("The running daemon forgot its session trust grants."),
        Err(error) => tracing::debug!(%error, "no daemon to forget session trust"),
    }
    notify_daemon_reload(paths).await;
    Ok(())
}

/// Finds the server `target` (`server` or `server:account`) names in `config`.
/// Without an account, the server must be enabled with exactly one.
fn select_server<'config>(
    config: &'config ResolvedConfig,
    target: &str,
) -> anyhow::Result<&'config ResolvedServer> {
    let (name, account) = split_target(target);

    let candidates = config.server(name);
    if candidates.is_empty() {
        if config.disabled.iter().any(|disabled| disabled.name == name) {
            bail!(
                "{name} is disabled here; run `plugbay why {name}` to see which file turned it off"
            );
        }
        bail!(
            "no server named {name} is enabled here; run `plugbay ls` to see the servers for this directory"
        );
    }

    let choices = candidates
        .iter()
        .map(|server| server.display_name())
        .collect::<Vec<_>>()
        .join(", ");
    let Some(account) = account else {
        let [only] = candidates.as_slice() else {
            bail!("{name} has several accounts here; choose one of: {choices}");
        };
        return Ok(only);
    };
    let Some(server) = candidates
        .into_iter()
        .find(|server| server.account == account)
    else {
        bail!("{name} has no account {account} here; choose one of: {choices}");
    };
    Ok(server)
}

/// `server:account` → (`server`, `Some(account)`); `server` → (`server`, `None`).
fn split_target(target: &str) -> (&str, Option<&str>) {
    match target.split_once(':') {
        Some((name, account)) => (name, Some(account)),
        None => (target, None),
    }
}

/// Human-readable lines for what trusting `file` would allow: the commands it runs and
/// the secret refs it resolves, taken from every server that needs trust from it.
fn describe_executable_parts(file: &ConfigFile, config: &ResolvedConfig) -> Vec<String> {
    let mut lines = Vec::new();
    for server in &config.servers {
        if !server.requires_trust_from.contains(&file.path) {
            continue;
        }
        let display_name = server.display_name();
        let target = server.transport.target();
        match &server.transport {
            ServerTransport::Stdio { env, cwd, .. } => {
                lines.push(format!("{display_name} runs: {target}"));
                if let Some(cwd) = cwd {
                    lines.push(format!("  in {}", cwd.display()));
                }
                for (name, value) in env {
                    if secrets::contains_secret_ref(value) {
                        lines.push(format!("  env {name} = {value}"));
                    }
                }
            }
            ServerTransport::Http { headers, .. } => {
                for (name, value) in headers {
                    if secrets::contains_secret_ref(value) {
                        lines.push(format!(
                            "{display_name} sends header {name} = {value} to {target}"
                        ));
                    }
                }
            }
        }
    }
    lines
}

async fn confirm(prompt: &str) -> anyhow::Result<bool> {
    let answer = ask(prompt).await?;
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

async fn confirm_default_yes(prompt: &str) -> anyhow::Result<bool> {
    let answer = ask(prompt).await?;
    Ok(matches!(answer.as_str(), "" | "y" | "yes"))
}

/// The trimmed, lowercased line typed after `prompt`.
async fn ask(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().context("writing the prompt")?;
    let answer = tokio::task::spawn_blocking(|| {
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).map(|_| answer)
    })
    .await
    .context("reading the answer failed")?
    .context("reading the answer")?;
    Ok(answer.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use url::Url;

    use super::*;
    use crate::config::{DisabledServer, ServerIdentity, ToolFilter};
    use crate::project::ProjectLocation;

    fn http_server(name: &str, account: &str, headers: BTreeMap<String, String>) -> ResolvedServer {
        ResolvedServer {
            name: name.to_string(),
            account: account.to_string(),
            tool_prefix: name.to_string(),
            transport: ServerTransport::Http {
                url: Url::parse("https://mcp.example.test/mcp").unwrap(),
                headers,
            },
            identity: ServerIdentity("https://mcp.example.test/mcp".to_string()),
            tool_filter: ToolFilter::default(),
            account_metadata: BTreeMap::new(),
            provenance: BTreeMap::new(),
            requires_trust_from: Vec::new(),
        }
    }

    fn config_with(servers: Vec<ResolvedServer>) -> ResolvedConfig {
        ResolvedConfig {
            location: ProjectLocation::global_only(),
            files: Vec::new(),
            servers,
            disabled: Vec::new(),
            catalog: Vec::new(),
            problems: Vec::new(),
            trusted_paths: Vec::new(),
        }
    }

    #[test]
    fn single_account_is_picked_without_naming_it() {
        let config = config_with(vec![http_server("linear", "initech", BTreeMap::new())]);
        assert_eq!(select_server(&config, "linear").unwrap().account, "initech");
    }

    #[test]
    fn several_accounts_need_an_explicit_choice() {
        let config = config_with(vec![
            http_server("posthog", "client-a", BTreeMap::new()),
            http_server("posthog", "client-b", BTreeMap::new()),
        ]);
        let error = select_server(&config, "posthog").unwrap_err().to_string();
        assert!(error.contains("posthog:client-a, posthog:client-b"));
        assert_eq!(
            select_server(&config, "posthog:client-b").unwrap().account,
            "client-b"
        );
    }

    #[test]
    fn unknown_account_lists_the_choices() {
        let config = config_with(vec![http_server("linear", "initech", BTreeMap::new())]);
        let error = select_server(&config, "linear:other")
            .unwrap_err()
            .to_string();
        assert!(error.contains("choose one of: linear:initech"));
    }

    #[test]
    fn disabled_server_explains_why() {
        let mut config = config_with(Vec::new());
        config.disabled.push(DisabledServer {
            name: "linear".to_string(),
            disabled_by: PathBuf::from("/work/client/plugbay.toml"),
        });
        let error = select_server(&config, "linear").unwrap_err().to_string();
        assert!(error.contains("plugbay why linear"));
    }

    #[test]
    fn executable_parts_show_commands_and_secret_refs() {
        let file = ConfigFile {
            path: PathBuf::from("/work/app/plugbay.toml"),
            scope: ConfigScope::Project,
            executable_fingerprint: Some("abc".to_string()),
        };
        let mut stdio = http_server("local", "default", BTreeMap::new());
        stdio.transport = ServerTransport::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "some-server".to_string()],
            env: BTreeMap::from([
                ("API_KEY".to_string(), "${op://vault/item/key}".to_string()),
                ("REGION".to_string(), "eu".to_string()),
            ]),
            cwd: None,
        };
        stdio.requires_trust_from = vec![file.path.clone()];
        let mut remote = http_server(
            "posthog",
            "client-a",
            BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer ${env:POSTHOG_KEY}".to_string(),
            )]),
        );
        remote.requires_trust_from = vec![file.path.clone()];
        let unrelated = http_server("linear", "initech", BTreeMap::new());
        let config = config_with(vec![stdio, remote, unrelated]);

        assert_eq!(
            describe_executable_parts(&file, &config),
            vec![
                "local:default runs: npx -y some-server".to_string(),
                "  env API_KEY = ${op://vault/item/key}".to_string(),
                "posthog:client-a sends header Authorization = Bearer ${env:POSTHOG_KEY} to https://mcp.example.test/mcp".to_string(),
            ]
        );
    }
}
