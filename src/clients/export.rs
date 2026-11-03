//! `plugbay export`: the servers an agent here gets, as Claude Code's `.mcp.json` or VS Code's
//! `.vscode/mcp.json`, for teammates without plugbay. No secret leaves as a value: secret refs,
//! stored token logins and values that may be secrets become placeholders each teammate fills in.

mod destination;
mod placeholders;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde_json::Value;

use self::destination::{Destination, MergeReport};
use self::placeholders::{
    Placeholder, Placeholders, SecretSource, environment_variable_name, header_placeholder_name,
};
use super::{Client, NativeTransport, claude, read_optional, write_atomically};
use crate::commands;
use crate::config::{self, DEFAULT_ACCOUNT, ResolvedConfig, ResolvedServer, ServerTransport};
use crate::credentials::{self, StoredLogin};
use crate::paths::{Paths, display_path};
use crate::secrets::{self, SecretRef};

const AUTHORIZATION: &str = "authorization";
const NO_LOGIN: StoredLogin = StoredLogin::None;

#[derive(clap::Args, Debug)]
pub struct ExportOptions {
    /// `claude` for Claude Code's `.mcp.json`, `vscode` for VS Code's `.vscode/mcp.json`.
    #[arg(long, value_enum)]
    format: ExportFormat,
    /// Write the file in this directory, merging into the servers already there.
    #[arg(long)]
    write: bool,
    /// With `--write`, replace entries of the same name instead of keeping them.
    #[arg(long, requires = "write")]
    force: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Claude,
    Vscode,
}

/// One server entry, ready for the file.
#[derive(Debug)]
struct ExportedEntry {
    name: String,
    entry: Value,
}

/// One directory's servers in the target format, before any file is touched.
#[derive(Debug)]
struct Export {
    format: ExportFormat,
    /// Only the entries to write: kept ones are in `report` alone.
    entries: Vec<ExportedEntry>,
    placeholders: Placeholders,
    /// What doesn't carry over, for stderr.
    notes: Vec<String>,
    report: MergeReport,
}

/// How one env or header value leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValuePlan {
    /// As written: nothing in it looks secret.
    Copy,
    /// Each secret ref swapped for a placeholder, the text around them kept.
    SwapRefs,
    /// One placeholder for the whole value: the text itself may be a secret.
    Replace,
}

pub async fn export(
    paths: &Paths,
    cd: Option<PathBuf>,
    options: ExportOptions,
) -> anyhow::Result<()> {
    let config = commands::resolve_here(paths, cd).await?;
    let logins = stored_logins(&config).await;

    if !options.write {
        let export = Export::build(&config, &logins, options.format, &Destination::default());
        print!("{}", export.merge(Destination::default())?);
        export.print_notes();
        return Ok(());
    }

    let Some(directory) = config
        .location
        .working_dir
        .as_ref()
        .or(config.location.config_dir.as_ref())
    else {
        bail!("no project here to write into; run it in a project directory or pass `-C <dir>`");
    };
    let file = directory.join(options.format.file_name());
    let existing = read_optional(&file)?;
    let destination = Destination::parse(existing.as_deref(), options.format, options.force)
        .with_context(|| format!("merging into {}", file.display()))?;
    let export = Export::build(&config, &logins, options.format, &destination);
    if export.report.is_empty() {
        println!("Nothing to write: no servers resolve here.");
        export.print_notes();
        return Ok(());
    }

    if export.report.changes_file() {
        let contents = export.merge(destination)?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        write_atomically(&file, &contents)?;
    }
    export.report.print(&display_path(&file, &paths.home));
    export.print_notes();
    Ok(())
}

/// What `plugbay login` stored for each remote server, keyed by entry name. A token's value never
/// gets here: `stored_login` keeps only its header.
async fn stored_logins(config: &ResolvedConfig) -> BTreeMap<String, StoredLogin> {
    let mut logins = BTreeMap::new();
    for server in &config.servers {
        if !server.uses_stored_login() {
            continue;
        }
        match credentials::stored_login(&server.identity, &server.account).await {
            Ok(login) => {
                logins.insert(server.tool_prefix.clone(), login);
            }
            Err(error) => tracing::warn!(
                server = %server.display_name(),
                error = %format!("{error:#}"),
                "could not read the stored login; exporting without one"
            ),
        }
    }
    logins
}

impl ExportFormat {
    /// Where the file goes, relative to the project directory.
    fn file_name(self) -> &'static str {
        match self {
            ExportFormat::Claude => claude::PROJECT_FILE,
            ExportFormat::Vscode => ".vscode/mcp.json",
        }
    }

    fn servers_key(self) -> &'static str {
        match self {
            ExportFormat::Claude => "mcpServers",
            ExportFormat::Vscode => "servers",
        }
    }

    fn client_name(self) -> &'static str {
        match self {
            ExportFormat::Claude => Client::ClaudeCode.display_name(),
            ExportFormat::Vscode => "VS Code",
        }
    }

    /// VS Code prompts for each secret plugbay keeps and stores the answer in its secret storage;
    /// an `${env:...}` ref stays an environment variable. Claude Code only reads the environment.
    fn prompts_for(self, source: &SecretSource) -> bool {
        self == ExportFormat::Vscode && source.environment_variable().is_none()
    }

    fn placeholder_text(self, placeholder: &Placeholder) -> String {
        let name = &placeholder.name;
        if self.prompts_for(&placeholder.source) {
            return format!("${{input:{name}}}");
        }
        match self {
            ExportFormat::Claude => format!("${{{name}}}"),
            ExportFormat::Vscode => format!("${{env:{name}}}"),
        }
    }

    /// Claude Code expands `${NAME}` only for shell-style names; VS Code's `${env:...}` takes any.
    fn can_expand(self, variable: &str) -> bool {
        match self {
            ExportFormat::Claude => claude::is_env_variable_name(variable),
            ExportFormat::Vscode => true,
        }
    }
}

impl Export {
    fn build(
        config: &ResolvedConfig,
        logins: &BTreeMap<String, StoredLogin>,
        format: ExportFormat,
        destination: &Destination,
    ) -> Self {
        let mut notes = Vec::new();
        for problem in &config.problems {
            notes.push(format!("not exported: {}", problem.message));
        }
        let mut report = MergeReport::default();
        let mut written = Vec::new();
        for server in &config.servers {
            let name = &server.tool_prefix;
            if let Some(reason) = skip_reason(server, format) {
                notes.push(format!("skipped {name}: {reason}"));
            } else if !destination.has_server(format, name) {
                report.added.push(name.clone());
                written.push(server);
            } else if destination.force {
                report.replaced.push(name.clone());
                written.push(server);
            } else {
                report.kept.push(name.clone());
            }
        }

        let reserved = destination.reserved_names(format, &report.replaced);
        let mut export = Export {
            format,
            entries: Vec::new(),
            placeholders: Placeholders::new(format, reserved),
            notes,
            report,
        };
        for server in &written {
            export.record_environment_refs(server);
        }
        for server in written {
            let login = logins.get(&server.tool_prefix).unwrap_or(&NO_LOGIN);
            let entry = export.entry(server, login);
            export.entries.push(ExportedEntry {
                name: server.tool_prefix.clone(),
                entry,
            });
            let server_notes = export.server_notes(server, login);
            export.notes.extend(server_notes);
        }
        export
    }

    /// Takes the variables `${env:...}` refs read before any name is made up, so none is reused.
    fn record_environment_refs(&mut self, server: &ResolvedServer) {
        let (_, values) = configured_values(&server.transport);
        for (field, value) in values {
            if value_plan(field, value) != ValuePlan::SwapRefs {
                continue;
            }
            let Ok(references) = secrets::parse_refs(value) else {
                continue;
            };
            for reference in references {
                if let SecretRef::Env(_) = reference {
                    self.placeholders
                        .add(&server.tool_prefix, SecretSource::Ref(reference), "");
                }
            }
        }
    }

    fn entry(&mut self, server: &ResolvedServer, login: &StoredLogin) -> Value {
        let name = server.tool_prefix.as_str();
        let transport = match &server.transport {
            ServerTransport::Http { url, headers } => {
                let mut exported = BTreeMap::new();
                for (header, value) in headers {
                    let preferred = header_placeholder_name(name, header);
                    let value = self.exported_value(name, header, value, &preferred);
                    exported.insert(header.clone(), value);
                }
                if let StoredLogin::Token { header } = login {
                    let source = SecretSource::StoredToken {
                        server: server.display_name(),
                    };
                    let preferred = header_placeholder_name(name, header);
                    let placeholder = self.placeholders.text_for(name, source, &preferred);
                    let value = if header == AUTHORIZATION {
                        format!("Bearer {placeholder}")
                    } else {
                        placeholder
                    };
                    exported.insert(header.clone(), value);
                }
                NativeTransport::Http {
                    url: url.to_string(),
                    headers: exported,
                }
            }
            ServerTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut exported = BTreeMap::new();
                for (variable, value) in env {
                    let preferred = environment_variable_name(variable);
                    let value = self.exported_value(name, variable, value, &preferred);
                    exported.insert(variable.clone(), value);
                }
                let cwd = match self.format {
                    ExportFormat::Claude => None,
                    ExportFormat::Vscode => cwd.as_ref().map(|cwd| cwd.display().to_string()),
                };
                NativeTransport::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: exported,
                    cwd,
                }
            }
        };
        transport.to_json()
    }

    /// `value` as the file gets it, see [`value_plan`]. New placeholders are named `preferred`.
    fn exported_value(
        &mut self,
        server: &str,
        field: &str,
        value: &str,
        preferred: &str,
    ) -> String {
        match value_plan(field, value) {
            ValuePlan::Copy => return value.to_string(),
            ValuePlan::SwapRefs => {
                let swapped = secrets::replace_refs(value, |reference| {
                    let source = SecretSource::Ref(reference.clone());
                    self.placeholders.text_for(server, source, preferred)
                });
                // `skip_reason` turned malformed refs away; if one slips by, replace it all.
                if let Ok(swapped) = swapped {
                    return swapped;
                }
            }
            ValuePlan::Replace => {}
        }
        let (scheme, _) = secrets::split_bearer_scheme(value);
        let source = SecretSource::Literal {
            server: server.to_string(),
            field: field.to_string(),
        };
        let placeholder = self.placeholders.text_for(server, source, preferred);
        format!("{scheme}{placeholder}")
    }

    fn server_notes(&self, server: &ResolvedServer, login: &StoredLogin) -> Vec<String> {
        let name = &server.tool_prefix;
        let client = self.format.client_name();
        let mut notes = Vec::new();
        if server.account != DEFAULT_ACCOUNT {
            notes.push(format!(
                "{name} uses account `{}` here; the file names no account, so each teammate signs in to the one they need",
                server.account
            ));
        }
        if *login == StoredLogin::OAuth {
            notes.push(format!(
                "{name} signs in with OAuth here; {client} asks each teammate to sign in"
            ));
        }
        if !server.tool_filter.include.is_empty() || !server.tool_filter.exclude.is_empty() {
            notes.push(format!(
                "{name} hides some tools here (`tools` / `exclude_tools`); the file exposes all of them"
            ));
        }
        if let ServerTransport::Stdio { cwd: Some(cwd), .. } = &server.transport
            && self.format == ExportFormat::Claude
        {
            notes.push(format!(
                "{name} runs in {} here; {} has no `cwd`, so it starts in the project directory",
                cwd.display(),
                claude::PROJECT_FILE
            ));
        }
        notes
    }

    fn print_notes(&self) {
        for note in &self.notes {
            eprintln!("note: {note}");
        }
        let (prompted, from_environment): (Vec<&Placeholder>, Vec<&Placeholder>) = self
            .placeholders
            .iter()
            .partition(|placeholder| self.format.prompts_for(&placeholder.source));
        if !prompted.is_empty() {
            eprintln!(
                "\nVS Code asks each teammate for these and keeps them in its secret storage:"
            );
            print_placeholders(&prompted);
        }
        if !from_environment.is_empty() {
            eprintln!(
                "\nEach teammate sets these environment variables before starting {}:",
                self.format.client_name()
            );
            print_placeholders(&from_environment);
        }
    }
}

fn print_placeholders(placeholders: &[&Placeholder]) {
    let width = placeholders
        .iter()
        .map(|placeholder| placeholder.name.len())
        .max()
        .unwrap_or(0);
    for placeholder in placeholders {
        eprintln!(
            "  {:width$}  {} (here: {})",
            placeholder.name,
            placeholder.servers.join(", "),
            placeholder.source.describe()
        );
    }
}

/// Why `server` can't be exported safely, if it can't. Never quotes a value: it may be a secret.
fn skip_reason(server: &ResolvedServer, format: ExportFormat) -> Option<String> {
    match &server.transport {
        ServerTransport::Http { url, .. } if config::url_may_hold_secret(url) => {
            return Some(format!(
                "its url may hold a key ({}); add it to the file by hand",
                config::display_url(url)
            ));
        }
        ServerTransport::Stdio { args, .. } if secrets::has_secret_in_args(args) => {
            return Some(
                "its args seem to hold a secret; move it to `env` with a secret ref".to_string(),
            );
        }
        ServerTransport::Stdio { args, .. }
            if args
                .iter()
                .any(|argument| secrets::has_secret_url(argument)) =>
        {
            return Some(
                "its args hold a url that may carry a credential; move it to `env` with a secret ref"
                    .to_string(),
            );
        }
        _ => {}
    }

    let (kind, values) = configured_values(&server.transport);
    for (field, value) in values {
        let Ok(references) = secrets::parse_refs(value) else {
            return Some(format!(
                "`{kind}.{field}` has a malformed secret ref; fix the `${{...}}` in the config"
            ));
        };
        if value_plan(field, value) != ValuePlan::SwapRefs {
            continue;
        }
        for reference in references {
            if let SecretRef::Env(variable) = reference
                && !format.can_expand(&variable)
            {
                return Some(format!(
                    "`{kind}.{field}` reads `${{env:{variable}}}`, which {} can't expand; rename the variable to letters, digits and `_`",
                    format.client_name()
                ));
            }
        }
    }
    None
}

/// The values that may hold secret refs: a remote server's headers or a local one's env.
fn configured_values(transport: &ServerTransport) -> (&'static str, &BTreeMap<String, String>) {
    match transport {
        ServerTransport::Http { headers, .. } => ("headers", headers),
        ServerTransport::Stdio { env, .. } => ("env", env),
    }
}

/// In a secret-named field only a `Bearer ` scheme may sit next to a ref (`key-${env:SUFFIX}`
/// would leak half the key), and nowhere may a literal URL carry a credential.
fn value_plan(field: &str, value: &str) -> ValuePlan {
    let secret_field = secrets::is_secret_field(field);
    if !secrets::contains_secret_ref(value) {
        let may_be_secret = secret_field || secrets::has_secret_url(value);
        return if value.is_empty() || !may_be_secret {
            ValuePlan::Copy
        } else {
            ValuePlan::Replace
        };
    }
    let (_, after_scheme) = secrets::split_bearer_scheme(value);
    let Ok(literal) = text_around_refs(after_scheme) else {
        return ValuePlan::Replace;
    };
    let literal_next_to_secret = secret_field && !literal.trim().is_empty();
    if literal_next_to_secret || secrets::has_secret_url(&literal) {
        ValuePlan::Replace
    } else {
        ValuePlan::SwapRefs
    }
}

/// `value` with each ref swapped for a space, so the text on either side never reads as one word.
fn text_around_refs(value: &str) -> anyhow::Result<String> {
    secrets::replace_refs(value, |_| " ".to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::{ConfigProblem, ToolFilter};

    fn with_env(mut server: ResolvedServer, values: &[(&str, &str)]) -> ResolvedServer {
        if let ServerTransport::Stdio { env, .. } = &mut server.transport {
            for (name, value) in values {
                env.insert(name.to_string(), value.to_string());
            }
        }
        server
    }

    fn with_headers(mut server: ResolvedServer, values: &[(&str, &str)]) -> ResolvedServer {
        if let ServerTransport::Http { headers, .. } = &mut server.transport {
            for (name, value) in values {
                headers.insert(name.to_string(), value.to_string());
            }
        }
        server
    }

    fn stdio(name: &str, env: &[(&str, &str)]) -> ResolvedServer {
        with_env(
            ResolvedServer::test_stdio(name, DEFAULT_ACCOUNT, name, &[]),
            env,
        )
    }

    fn tool() -> ResolvedServer {
        with_env(
            ResolvedServer::test_stdio("tool", DEFAULT_ACCOUNT, "npx", &["-y", "tool"]),
            &[
                ("TOOL_API_KEY", "${keychain:plugbay/tool-key}"),
                ("GITHUB_TOKEN", "${env:GITHUB_TOKEN}"),
                ("LOG_LEVEL", "debug"),
            ],
        )
    }

    fn linear() -> ResolvedServer {
        ResolvedServer::test_http("linear", DEFAULT_ACCOUNT, "https://mcp.linear.app/mcp")
    }

    fn build(servers: Vec<ResolvedServer>, format: ExportFormat) -> Export {
        build_with_logins(servers, &[], format)
    }

    fn build_with_logins(
        servers: Vec<ResolvedServer>,
        logins: &[(&str, StoredLogin)],
        format: ExportFormat,
    ) -> Export {
        let config = ResolvedConfig::test_with(Vec::new(), servers);
        let logins = logins
            .iter()
            .map(|(name, login)| (name.to_string(), login.clone()))
            .collect();
        Export::build(&config, &logins, format, &Destination::default())
    }

    fn printed(export: &Export) -> Value {
        let contents = export.merge(Destination::default()).unwrap();
        serde_json::from_str(&contents).unwrap()
    }

    fn printed_text(export: &Export) -> String {
        export.merge(Destination::default()).unwrap()
    }

    #[test]
    fn claude_export_turns_secret_refs_into_env_placeholders() {
        let export = build(vec![tool(), linear()], ExportFormat::Claude);
        assert_eq!(
            printed(&export),
            json!({
                "mcpServers": {
                    "tool": {
                        "type": "stdio",
                        "command": "npx",
                        "args": ["-y", "tool"],
                        "env": {
                            "GITHUB_TOKEN": "${GITHUB_TOKEN}",
                            "LOG_LEVEL": "debug",
                            "TOOL_API_KEY": "${TOOL_API_KEY}",
                        },
                    },
                    "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
                }
            })
        );
    }

    #[test]
    fn vscode_export_prompts_for_stored_secrets_and_reads_env_refs() {
        let export = build(vec![tool()], ExportFormat::Vscode);
        assert_eq!(
            printed(&export),
            json!({
                "inputs": [{
                    "type": "promptString",
                    "id": "tool-api-key",
                    "description": "tool-api-key for tool",
                    "password": true,
                }],
                "servers": {
                    "tool": {
                        "type": "stdio",
                        "command": "npx",
                        "args": ["-y", "tool"],
                        "env": {
                            "GITHUB_TOKEN": "${env:GITHUB_TOKEN}",
                            "LOG_LEVEL": "debug",
                            "TOOL_API_KEY": "${input:tool-api-key}",
                        },
                    },
                }
            })
        );
    }

    #[test]
    fn vscode_keeps_inputs_before_servers_in_a_new_file() {
        let contents = printed_text(&build(vec![tool()], ExportFormat::Vscode));
        assert!(contents.find("\"inputs\"") < contents.find("\"servers\""));
    }

    #[test]
    fn stored_bearer_token_becomes_an_authorization_placeholder() {
        let token = StoredLogin::Token {
            header: AUTHORIZATION.to_string(),
        };
        let export = build_with_logins(vec![linear()], &[("linear", token)], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["linear"]["headers"],
            json!({ "authorization": "Bearer ${LINEAR_TOKEN}" })
        );
    }

    #[test]
    fn stored_token_in_a_custom_header_is_sent_bare() {
        let token = StoredLogin::Token {
            header: "x-api-key".to_string(),
        };
        let export = build_with_logins(vec![linear()], &[("linear", token)], ExportFormat::Vscode);
        assert_eq!(
            printed(&export)["servers"]["linear"]["headers"],
            json!({ "x-api-key": "${input:linear-x-api-key}" })
        );
    }

    #[test]
    fn oauth_login_gets_no_header_and_a_note() {
        let export = build_with_logins(
            vec![linear()],
            &[("linear", StoredLogin::OAuth)],
            ExportFormat::Claude,
        );
        assert_eq!(
            printed(&export)["mcpServers"]["linear"].get("headers"),
            None
        );
        assert!(
            export.notes[0].contains("signs in with OAuth"),
            "{:?}",
            export.notes
        );
    }

    #[test]
    fn configured_header_refs_keep_their_scheme() {
        let server = with_headers(
            linear(),
            &[
                ("authorization", "Bearer ${op://work/linear/key}"),
                ("x-team", "platform"),
            ],
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["linear"]["headers"],
            json!({ "authorization": "Bearer ${LINEAR_TOKEN}", "x-team": "platform" })
        );
    }

    #[test]
    fn literal_secret_values_never_reach_the_file() {
        let export = build(
            vec![stdio("tool", &[("API_KEY", "literal-test-token")])],
            ExportFormat::Claude,
        );
        let contents = printed_text(&export);
        assert!(!contents.contains("literal-test-token"), "{contents}");
        assert!(contents.contains("${API_KEY}"), "{contents}");
    }

    #[test]
    fn literal_text_next_to_a_ref_in_a_secret_field_becomes_one_placeholder() {
        let server = stdio("tool", &[("API_KEY", "fake-private-${env:SUFFIX}")]);
        let export = build(vec![server], ExportFormat::Claude);
        let contents = printed_text(&export);
        assert!(!contents.contains("fake-private"), "{contents}");
        assert_eq!(
            printed(&export)["mcpServers"]["tool"]["env"]["API_KEY"],
            "${API_KEY}"
        );
        assert_eq!(export.placeholders.iter().count(), 1);
    }

    #[test]
    fn malformed_refs_skip_the_server_without_quoting_the_value() {
        let server = stdio("tool", &[("API_KEY", "fake-private-${env:SUFFIX")]);
        let export = build(vec![server], ExportFormat::Claude);
        assert!(export.entries.is_empty());
        let notes = export.notes.join("\n");
        assert!(
            notes.contains("`env.API_KEY` has a malformed secret ref"),
            "{notes}"
        );
        assert!(!notes.contains("fake-private"), "{notes}");
    }

    #[test]
    fn credential_urls_in_args_skip_the_server() {
        let server = ResolvedServer::test_stdio(
            "tool",
            DEFAULT_ACCOUNT,
            "npx",
            &["--url", "https://fake-user:fake-pass@mcp.example.com/mcp"],
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert!(export.entries.is_empty());
        let notes = export.notes.join("\n");
        assert!(notes.contains("may carry a credential"), "{notes}");
        assert!(!notes.contains("fake-pass"), "{notes}");
    }

    #[test]
    fn credential_urls_in_env_values_become_placeholders() {
        let server = stdio(
            "db",
            &[
                (
                    "DATABASE_URL",
                    "postgres://fake-user:fake-pass@db.internal/app",
                ),
                ("LOG_URL", "https://logs.example.com/ingest"),
            ],
        );
        let export = build(vec![server], ExportFormat::Claude);
        let contents = printed_text(&export);
        assert!(!contents.contains("fake-pass"), "{contents}");
        let env = &printed(&export)["mcpServers"]["db"]["env"];
        assert_eq!(env["DATABASE_URL"], "${DATABASE_URL}");
        assert_eq!(env["LOG_URL"], "https://logs.example.com/ingest");
    }

    #[test]
    fn one_secret_used_twice_shares_a_placeholder() {
        let other = stdio("other", &[("KEY", "${keychain:plugbay/tool-key}")]);
        let export = build(vec![tool(), other], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["other"]["env"]["KEY"],
            "${TOOL_API_KEY}"
        );
        assert_eq!(export.placeholders.iter().count(), 2);
    }

    #[test]
    fn different_secrets_under_one_name_get_the_server_prefix() {
        let other = stdio("other", &[("TOOL_API_KEY", "${op://work/other/key}")]);
        let export = build(vec![tool(), other], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["other"]["env"]["TOOL_API_KEY"],
            "${OTHER_TOOL_API_KEY}"
        );
    }

    #[test]
    fn several_refs_in_one_value_are_numbered() {
        let server = stdio(
            "db",
            &[(
                "DB_URL",
                "postgres://${env:DB_USER}:${fnox:DB_PASSWORD}@${fnox:DB_HOST}/app",
            )],
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["db"]["env"]["DB_URL"],
            "postgres://${DB_USER}:${DB_URL}@${DB_URL_2}/app"
        );
    }

    #[test]
    fn input_ids_stay_distinct_when_names_differ_only_in_underscores() {
        let server = stdio(
            "tool",
            &[
                ("KEY", "${keychain:plugbay/a}"),
                ("_KEY", "${keychain:plugbay/b}"),
            ],
        );
        let printed = printed(&build(vec![server], ExportFormat::Vscode));
        assert_eq!(printed["servers"]["tool"]["env"]["KEY"], "${input:key}");
        assert_eq!(printed["servers"]["tool"]["env"]["_KEY"], "${input:key-2}");
        assert_eq!(printed["inputs"][1]["id"], "key-2");
    }

    #[test]
    fn env_refs_keep_their_exact_name() {
        let server = stdio("tool", &[("TOKEN", "${env:my_token}")]);
        let claude = printed(&build(vec![server.clone()], ExportFormat::Claude));
        assert_eq!(claude["mcpServers"]["tool"]["env"]["TOKEN"], "${my_token}");
        let vscode = printed(&build(vec![server], ExportFormat::Vscode));
        assert_eq!(vscode["servers"]["tool"]["env"]["TOKEN"], "${env:my_token}");
    }

    #[test]
    fn env_ref_names_are_taken_before_any_name_is_made_up() {
        let first = stdio("first", &[("GITHUB_TOKEN", "${keychain:plugbay/github}")]);
        let second = stdio("second", &[("TOKEN", "${env:GITHUB_TOKEN}")]);
        let printed = printed(&build(vec![first, second], ExportFormat::Claude));
        assert_eq!(
            printed["mcpServers"]["first"]["env"]["GITHUB_TOKEN"],
            "${FIRST_GITHUB_TOKEN}"
        );
        assert_eq!(
            printed["mcpServers"]["second"]["env"]["TOKEN"],
            "${GITHUB_TOKEN}"
        );
    }

    #[test]
    fn env_names_claude_code_cannot_expand_skip_the_server() {
        let server = stdio("tool", &[("TOKEN", "${env:my-token}")]);
        let claude = build(vec![server.clone()], ExportFormat::Claude);
        assert!(claude.entries.is_empty());
        assert!(
            claude.notes[0].contains("can't expand"),
            "{:?}",
            claude.notes
        );
        let vscode = printed(&build(vec![server], ExportFormat::Vscode));
        assert_eq!(vscode["servers"]["tool"]["env"]["TOKEN"], "${env:my-token}");
    }

    #[test]
    fn non_default_account_and_tool_filters_are_noted() {
        let mut server =
            ResolvedServer::test_http("linear", "client-a", "https://mcp.linear.app/mcp");
        server.tool_filter = ToolFilter::new(Vec::new(), vec!["delete_*".to_string()]);
        let export = build(vec![server], ExportFormat::Claude);
        assert!(
            export.notes[0].contains("account `client-a`"),
            "{:?}",
            export.notes
        );
        assert!(
            export.notes[1].contains("hides some tools"),
            "{:?}",
            export.notes
        );
    }

    #[test]
    fn url_that_may_hold_a_key_is_skipped() {
        let server = ResolvedServer::test_http(
            "keyed",
            DEFAULT_ACCOUNT,
            "https://mcp.example.com/mcp?api_key=test-token",
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert!(export.entries.is_empty());
        assert!(
            !export.notes[0].contains("test-token"),
            "{:?}",
            export.notes
        );
    }

    #[test]
    fn secret_in_args_is_skipped() {
        let server =
            ResolvedServer::test_stdio("tool", DEFAULT_ACCOUNT, "npx", &["--api-key=test-token"]);
        let export = build(vec![server], ExportFormat::Claude);
        assert!(export.entries.is_empty());
        assert!(
            export.notes[0].starts_with("skipped tool"),
            "{:?}",
            export.notes
        );
    }

    #[test]
    fn a_skipped_server_leaves_no_placeholders_behind() {
        let server = stdio(
            "broken",
            &[("A_KEY", "${keychain:plugbay/a}"), ("B_KEY", "${nope:b}")],
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert!(export.entries.is_empty());
        assert_eq!(export.placeholders.iter().count(), 0);
    }

    #[test]
    fn config_problems_are_noted() {
        let mut config = ResolvedConfig::test_with(Vec::new(), Vec::new());
        config.problems.push(ConfigProblem {
            servers: vec!["linear".to_string()],
            message: "linear is bound to two accounts".to_string(),
        });
        let export = Export::build(
            &config,
            &BTreeMap::new(),
            ExportFormat::Claude,
            &Destination::default(),
        );
        assert_eq!(
            export.notes,
            ["not exported: linear is bound to two accounts"]
        );
    }

    #[test]
    fn claude_drops_cwd_with_a_note_and_vscode_keeps_it() {
        let mut server = ResolvedServer::test_stdio("tool", DEFAULT_ACCOUNT, "npx", &[]);
        if let ServerTransport::Stdio { cwd, .. } = &mut server.transport {
            *cwd = Some(PathBuf::from("/srv/tool"));
        }
        let claude = build(vec![server.clone()], ExportFormat::Claude);
        assert_eq!(printed(&claude)["mcpServers"]["tool"].get("cwd"), None);
        assert!(claude.notes[0].contains("no `cwd`"), "{:?}", claude.notes);
        let vscode = build(vec![server], ExportFormat::Vscode);
        assert_eq!(printed(&vscode)["servers"]["tool"]["cwd"], "/srv/tool");
    }
}
