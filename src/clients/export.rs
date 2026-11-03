//! `plugbay export`: the servers an agent here gets, as Claude Code's `.mcp.json` or VS Code's
//! `.vscode/mcp.json`, for teammates without plugbay. No secret leaves as a value: secret refs and
//! stored token logins become the format's placeholders, which each teammate fills in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use url::Url;

use super::import::{has_secret_in_args, is_secret_field, split_bearer_scheme};
use super::{read_optional, write_atomically};
use crate::commands::{self, display_path};
use crate::config::{self, DEFAULT_ACCOUNT, ResolvedConfig, ResolvedServer, ServerTransport};
use crate::credentials;
use crate::paths::Paths;
use crate::secrets::{self, SecretRef};

const AUTHORIZATION: &str = "authorization";

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

/// What a remote server signs in with here from `plugbay login`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredAuth {
    None,
    OAuth,
    /// A static token, sent in this lowercase header.
    Token {
        header: String,
    },
}

/// One server entry, ready for the file.
#[derive(Debug)]
struct ExportedEntry {
    name: String,
    entry: Value,
}

/// A value the teammate provides in place of a secret plugbay resolves here.
#[derive(Debug, Clone)]
struct Placeholder {
    /// `SCREAMING_SNAKE`: the environment variable for Claude Code; VS Code inputs use it kebab-cased.
    name: String,
    /// Where plugbay finds the value here (`keychain:plugbay/x`, a stored token), so one secret
    /// used twice gets one placeholder. Refs are pointers, never values, so this is safe to print.
    source: String,
    /// Came from `${env:...}`: the teammate's own environment variable in every format.
    from_environment: bool,
    servers: Vec<String>,
}

/// One directory's servers in the target format, before any file is touched.
#[derive(Debug)]
struct Export {
    format: ExportFormat,
    entries: Vec<ExportedEntry>,
    placeholders: Vec<Placeholder>,
    /// What doesn't carry over, for stderr.
    notes: Vec<String>,
}

/// What a merge did with each exported entry.
#[derive(Debug, Default, PartialEq, Eq)]
struct MergeReport {
    added: Vec<String>,
    replaced: Vec<String>,
    kept: Vec<String>,
}

pub async fn export(
    paths: &Paths,
    cd: Option<PathBuf>,
    options: ExportOptions,
) -> anyhow::Result<()> {
    let config = commands::resolve_here(paths, cd).await?;
    let logins = stored_logins(&config).await;
    let export = Export::build(&config, &logins, options.format);

    if !options.write {
        let (contents, _) = export.merge(None, false)?;
        print!("{contents}");
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
    if export.entries.is_empty() {
        println!("Nothing to write: no servers resolve here.");
        export.print_notes();
        return Ok(());
    }

    let file = directory.join(options.format.file_name());
    let existing = read_optional(&file)?;
    let (contents, report) = export
        .merge(existing.as_deref(), options.force)
        .with_context(|| format!("merging into {}", file.display()))?;
    if report.changes_file() {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        write_atomically(&file, &contents)?;
    }
    report.print(&display_path(&file, &paths.home));
    export.print_notes();
    Ok(())
}

/// How each remote server signs in with what `plugbay login` stored, keyed by entry name. Only
/// the kind of login and a token's header are kept; the token itself is dropped right away.
async fn stored_logins(config: &ResolvedConfig) -> BTreeMap<String, StoredAuth> {
    let mut logins = BTreeMap::new();
    for server in &config.servers {
        if !server.uses_stored_login() {
            continue;
        }
        match stored_auth(server).await {
            Ok(auth) => {
                logins.insert(server.tool_prefix.clone(), auth);
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

async fn stored_auth(server: &ResolvedServer) -> anyhow::Result<StoredAuth> {
    if let Some(token) = credentials::load_token(&server.identity, &server.account).await? {
        return Ok(StoredAuth::Token {
            header: token.header.clone(),
        });
    }
    if credentials::has_stored_credentials(&server.identity, &server.account).await? {
        return Ok(StoredAuth::OAuth);
    }
    Ok(StoredAuth::None)
}

impl ExportFormat {
    /// Where the file goes, relative to the project directory.
    fn file_name(self) -> &'static str {
        match self {
            ExportFormat::Claude => ".mcp.json",
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
            ExportFormat::Claude => "Claude Code",
            ExportFormat::Vscode => "VS Code",
        }
    }

    /// Claude Code expands `${VAR}` from the environment; VS Code reads `${env:VAR}` and prompts
    /// for `${input:id}`, keeping the answer in its secret storage.
    fn placeholder_text(self, placeholder: &Placeholder) -> String {
        match self {
            ExportFormat::Claude => format!("${{{}}}", placeholder.name),
            ExportFormat::Vscode if placeholder.from_environment => {
                format!("${{env:{}}}", placeholder.name)
            }
            ExportFormat::Vscode => format!("${{input:{}}}", input_id(&placeholder.name)),
        }
    }
}

impl Export {
    fn build(
        config: &ResolvedConfig,
        logins: &BTreeMap<String, StoredAuth>,
        format: ExportFormat,
    ) -> Self {
        let mut export = Export {
            format,
            entries: Vec::new(),
            placeholders: Vec::new(),
            notes: Vec::new(),
        };
        for problem in &config.problems {
            export
                .notes
                .push(format!("not exported: {}", problem.message));
        }
        for server in &config.servers {
            let auth = logins
                .get(&server.tool_prefix)
                .cloned()
                .unwrap_or(StoredAuth::None);
            let placeholders_before = export.placeholders.clone();
            match export.entry(server, &auth) {
                Ok(entry) => {
                    export.entries.push(ExportedEntry {
                        name: server.tool_prefix.clone(),
                        entry,
                    });
                    export.notes.extend(export.server_notes(server, &auth));
                }
                Err(reason) => {
                    export.placeholders = placeholders_before;
                    export
                        .notes
                        .push(format!("skipped {}: {reason:#}", server.tool_prefix));
                }
            }
        }
        export
    }

    fn entry(&mut self, server: &ResolvedServer, auth: &StoredAuth) -> anyhow::Result<Value> {
        let name = server.tool_prefix.as_str();
        match &server.transport {
            ServerTransport::Http { url, headers } => {
                if config::url_may_hold_secret(url) {
                    bail!(
                        "its url may hold a key ({}); add it to the file by hand",
                        config::display_url(url)
                    );
                }
                let mut exported = Map::new();
                for (header, value) in headers {
                    let preferred = header_placeholder_name(name, header);
                    let value = self.without_secrets(name, header, value, &preferred)?;
                    exported.insert(header.clone(), value.into());
                }
                if let StoredAuth::Token { header } = auth {
                    let source =
                        format!("the token from `plugbay login {}`", server.display_name());
                    let preferred = header_placeholder_name(name, header);
                    let placeholder = self.placeholder(name, &source, &preferred, false);
                    let value = if header == AUTHORIZATION {
                        format!("Bearer {placeholder}")
                    } else {
                        placeholder
                    };
                    exported.insert(header.clone(), value.into());
                }
                Ok(http_entry(url, exported))
            }
            ServerTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                if has_secret_in_args(args) {
                    bail!("its args seem to hold a secret; move it to `env` with a secret ref");
                }
                let mut exported = Map::new();
                for (variable, value) in env {
                    let preferred = environment_variable_name(variable);
                    let value = self.without_secrets(name, variable, value, &preferred)?;
                    exported.insert(variable.clone(), value.into());
                }
                let cwd = match self.format {
                    ExportFormat::Claude => None,
                    ExportFormat::Vscode => cwd.as_deref(),
                };
                Ok(stdio_entry(command, args, exported, cwd))
            }
        }
    }

    /// `value` with each secret ref swapped for a placeholder named `preferred` (numbered after the
    /// first). A literal value in a secret-looking `field` becomes a placeholder as a whole.
    fn without_secrets(
        &mut self,
        server: &str,
        field: &str,
        value: &str,
        preferred: &str,
    ) -> anyhow::Result<String> {
        if !secrets::contains_secret_ref(value) {
            if value.is_empty() || !is_secret_field(field) {
                return Ok(value.to_string());
            }
            let (scheme, _) = split_bearer_scheme(value);
            let source = format!("the literal `{field}` value of {server}");
            let placeholder = self.placeholder(server, &source, preferred, false);
            return Ok(format!("{scheme}{placeholder}"));
        }

        let mut count = 0;
        secrets::replace_refs(value, |reference| {
            count += 1;
            let source = reference.describe();
            match reference {
                SecretRef::Env(variable) => {
                    self.placeholder(server, &source, &environment_variable_name(variable), true)
                }
                _ if count == 1 => self.placeholder(server, &source, preferred, false),
                _ => self.placeholder(server, &source, &format!("{preferred}_{count}"), false),
            }
        })
        .with_context(|| format!("reading the secret refs in `{field}`"))
    }

    /// The placeholder text for `source`, reusing the one already made for it.
    fn placeholder(
        &mut self,
        server: &str,
        source: &str,
        preferred: &str,
        from_environment: bool,
    ) -> String {
        let index = match self
            .placeholders
            .iter()
            .position(|placeholder| placeholder.source == source)
        {
            Some(index) => index,
            None => {
                let name = self.unused_name(server, preferred);
                self.placeholders.push(Placeholder {
                    name,
                    source: source.to_string(),
                    from_environment,
                    servers: Vec::new(),
                });
                self.placeholders.len() - 1
            }
        };
        let placeholder = &mut self.placeholders[index];
        if !placeholder.servers.iter().any(|name| name == server) {
            placeholder.servers.push(server.to_string());
        }
        self.format.placeholder_text(placeholder)
    }

    /// `preferred`, else `SERVER_preferred`, else that numbered: two secrets never share a name.
    fn unused_name(&self, server: &str, preferred: &str) -> String {
        let is_taken = |candidate: &str| {
            self.placeholders
                .iter()
                .any(|placeholder| placeholder.name == candidate)
        };
        let prefixed = environment_variable_name(&format!("{server}_{preferred}"));
        for candidate in [preferred, prefixed.as_str()] {
            if !is_taken(candidate) {
                return candidate.to_string();
            }
        }
        let mut number = 2;
        loop {
            let candidate = format!("{prefixed}_{number}");
            if !is_taken(&candidate) {
                return candidate;
            }
            number += 1;
        }
    }

    fn server_notes(&self, server: &ResolvedServer, auth: &StoredAuth) -> Vec<String> {
        let name = &server.tool_prefix;
        let client = self.format.client_name();
        let mut notes = Vec::new();
        if server.account != DEFAULT_ACCOUNT {
            notes.push(format!(
                "{name} uses account `{}` here; the file names no account, so each teammate signs in to the one they need",
                server.account
            ));
        }
        if *auth == StoredAuth::OAuth {
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
                "{name} runs in {} here; .mcp.json has no `cwd`, so it starts in the project directory",
                cwd.display()
            ));
        }
        notes
    }

    /// `existing` (a missing file when `None`) with the exported entries merged in: entries of the
    /// same name are kept unless `force`, everything else in the file is left as it was.
    fn merge(&self, existing: Option<&str>, force: bool) -> anyhow::Result<(String, MergeReport)> {
        let mut document = match existing.map(str::trim) {
            None | Some("") => Map::new(),
            Some(text) => match serde_json::from_str(text)
                .context("parsing it as JSON (comments are not supported)")?
            {
                Value::Object(document) => document,
                _ => bail!("expected a JSON object at the top level"),
            },
        };

        let servers_key = self.format.servers_key();
        let existing_names: Vec<String> = match document.get(servers_key) {
            None => Vec::new(),
            Some(Value::Object(servers)) => servers.keys().cloned().collect(),
            Some(_) => bail!("`{servers_key}` is not an object"),
        };
        let mut report = MergeReport::default();
        let mut written = Vec::new();
        for entry in &self.entries {
            if !existing_names.contains(&entry.name) {
                report.added.push(entry.name.clone());
                written.push(entry);
            } else if force {
                report.replaced.push(entry.name.clone());
                written.push(entry);
            } else {
                report.kept.push(entry.name.clone());
            }
        }

        if self.format == ExportFormat::Vscode {
            self.add_inputs(&mut document, &written)?;
        }
        let servers = document
            .entry(servers_key)
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(servers) = servers else {
            bail!("`{servers_key}` is not an object");
        };
        for entry in written {
            servers.insert(entry.name.clone(), entry.entry.clone());
        }

        let mut contents =
            serde_json::to_string_pretty(&Value::Object(document)).context("rendering the JSON")?;
        contents.push('\n');
        Ok((contents, report))
    }

    /// A password prompt for every input the written entries use, unless one with that id exists.
    fn add_inputs(
        &self,
        document: &mut Map<String, Value>,
        written: &[&ExportedEntry],
    ) -> anyhow::Result<()> {
        let needed: Vec<&Placeholder> = self
            .placeholders
            .iter()
            .filter(|placeholder| !placeholder.from_environment)
            .filter(|placeholder| {
                written
                    .iter()
                    .any(|entry| placeholder.servers.contains(&entry.name))
            })
            .collect();
        if needed.is_empty() {
            return Ok(());
        }

        let inputs = document
            .entry("inputs")
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(inputs) = inputs else {
            bail!("`inputs` is not a list");
        };
        for placeholder in needed {
            let id = input_id(&placeholder.name);
            let exists = inputs
                .iter()
                .any(|input| input.get("id").and_then(Value::as_str) == Some(id.as_str()));
            if !exists {
                inputs.push(json!({
                    "type": "promptString",
                    "id": id,
                    "description": format!("{} for {}", placeholder.name, placeholder.servers.join(", ")),
                    "password": true,
                }));
            }
        }
        Ok(())
    }

    fn print_notes(&self) {
        for note in &self.notes {
            eprintln!("note: {note}");
        }
        let (from_environment, prompted): (Vec<&Placeholder>, Vec<&Placeholder>) =
            self.placeholders.iter().partition(|placeholder| {
                placeholder.from_environment || self.format == ExportFormat::Claude
            });
        if !prompted.is_empty() {
            eprintln!(
                "\nVS Code asks each teammate for these and keeps them in its secret storage:"
            );
            print_placeholders(&prompted, |placeholder| input_id(&placeholder.name));
        }
        if !from_environment.is_empty() {
            eprintln!(
                "\nEach teammate sets these environment variables before starting {}:",
                self.format.client_name()
            );
            print_placeholders(&from_environment, |placeholder| placeholder.name.clone());
        }
    }
}

fn print_placeholders(placeholders: &[&Placeholder], label: impl Fn(&Placeholder) -> String) {
    let width = placeholders
        .iter()
        .map(|placeholder| label(placeholder).len())
        .max()
        .unwrap_or(0);
    for placeholder in placeholders {
        eprintln!(
            "  {:width$}  {} (here: {})",
            label(placeholder),
            placeholder.servers.join(", "),
            placeholder.source
        );
    }
}

impl MergeReport {
    fn changes_file(&self) -> bool {
        !self.added.is_empty() || !self.replaced.is_empty()
    }

    fn print(&self, file: &str) {
        if self.changes_file() {
            println!("Wrote {file}");
        } else {
            println!("{file} unchanged");
        }
        if !self.added.is_empty() {
            println!("  added {}", self.added.join(", "));
        }
        if !self.replaced.is_empty() {
            println!("  replaced {}", self.replaced.join(", "));
        }
        if !self.kept.is_empty() {
            println!(
                "  kept the existing {} (use --force to replace)",
                self.kept.join(", ")
            );
        }
    }
}

fn http_entry(url: &Url, headers: Map<String, Value>) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_string(), "http".into());
    entry.insert("url".to_string(), url.as_str().into());
    if !headers.is_empty() {
        entry.insert("headers".to_string(), Value::Object(headers));
    }
    Value::Object(entry)
}

fn stdio_entry(
    command: &str,
    args: &[String],
    env: Map<String, Value>,
    cwd: Option<&Path>,
) -> Value {
    let mut entry = Map::new();
    entry.insert("type".to_string(), "stdio".into());
    entry.insert("command".to_string(), command.into());
    if !args.is_empty() {
        entry.insert("args".to_string(), args.into());
    }
    if !env.is_empty() {
        entry.insert("env".to_string(), Value::Object(env));
    }
    if let Some(cwd) = cwd {
        entry.insert("cwd".to_string(), cwd.display().to_string().into());
    }
    Value::Object(entry)
}

/// `SERVER_TOKEN` for `Authorization`, `SERVER_HEADER_NAME` for any other header.
fn header_placeholder_name(server: &str, header: &str) -> String {
    if header.eq_ignore_ascii_case(AUTHORIZATION) {
        environment_variable_name(&format!("{server}_token"))
    } else {
        environment_variable_name(&format!("{server}_{header}"))
    }
}

/// Uppercase letters, digits and `_`, not starting with a digit: a name every shell and both
/// clients expand.
fn environment_variable_name(text: &str) -> String {
    let mut name: String = text
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.starts_with(|character: char| character.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// `LINEAR_API_KEY` → `linear-api-key`.
fn input_id(name: &str) -> String {
    name.trim_start_matches('_')
        .to_ascii_lowercase()
        .replace('_', "-")
}

#[cfg(test)]
mod tests {
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
        logins: &[(&str, StoredAuth)],
        format: ExportFormat,
    ) -> Export {
        let config = ResolvedConfig::test_with(Vec::new(), servers);
        let logins = logins
            .iter()
            .map(|(name, auth)| (name.to_string(), auth.clone()))
            .collect();
        Export::build(&config, &logins, format)
    }

    fn printed(export: &Export) -> Value {
        let (contents, _) = export.merge(None, false).unwrap();
        serde_json::from_str(&contents).unwrap()
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
                    "description": "TOOL_API_KEY for tool",
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
        let (contents, _) = build(vec![tool()], ExportFormat::Vscode)
            .merge(None, false)
            .unwrap();
        assert!(contents.find("\"inputs\"") < contents.find("\"servers\""));
    }

    #[test]
    fn stored_bearer_token_becomes_an_authorization_placeholder() {
        let token = StoredAuth::Token {
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
        let token = StoredAuth::Token {
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
            &[("linear", StoredAuth::OAuth)],
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
        let server = with_env(
            ResolvedServer::test_stdio("tool", DEFAULT_ACCOUNT, "npx", &[]),
            &[("API_KEY", "literal-test-token")],
        );
        let export = build(vec![server], ExportFormat::Claude);
        let (contents, _) = export.merge(None, false).unwrap();
        assert!(!contents.contains("literal-test-token"), "{contents}");
        assert!(contents.contains("${API_KEY}"), "{contents}");
    }

    #[test]
    fn one_secret_used_twice_shares_a_placeholder() {
        let other = with_env(
            ResolvedServer::test_stdio("other", DEFAULT_ACCOUNT, "other", &[]),
            &[("KEY", "${keychain:plugbay/tool-key}")],
        );
        let export = build(vec![tool(), other], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["other"]["env"]["KEY"],
            "${TOOL_API_KEY}"
        );
        assert_eq!(export.placeholders.len(), 2);
    }

    #[test]
    fn different_secrets_under_one_name_get_the_server_prefix() {
        let other = with_env(
            ResolvedServer::test_stdio("other", DEFAULT_ACCOUNT, "other", &[]),
            &[("TOOL_API_KEY", "${op://work/other/key}")],
        );
        let export = build(vec![tool(), other], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["other"]["env"]["TOOL_API_KEY"],
            "${OTHER_TOOL_API_KEY}"
        );
    }

    #[test]
    fn several_refs_in_one_value_are_numbered() {
        let server = with_env(
            ResolvedServer::test_stdio("db", DEFAULT_ACCOUNT, "db", &[]),
            &[(
                "DB_URL",
                "postgres://${env:DB_USER}:${fnox:DB_PASSWORD}@${fnox:DB_HOST}/app",
            )],
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert_eq!(
            printed(&export)["mcpServers"]["db"]["env"]["DB_URL"],
            "postgres://${DB_USER}:${DB_URL_2}@${DB_URL_3}/app"
        );
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
        let server = with_env(
            ResolvedServer::test_stdio("broken", DEFAULT_ACCOUNT, "broken", &[]),
            &[("A_KEY", "${keychain:plugbay/a}"), ("B_KEY", "${nope:b}")],
        );
        let export = build(vec![server], ExportFormat::Claude);
        assert!(export.entries.is_empty());
        assert!(export.placeholders.is_empty());
    }

    #[test]
    fn config_problems_are_noted() {
        let mut config = ResolvedConfig::test_with(Vec::new(), Vec::new());
        config.problems.push(ConfigProblem {
            servers: vec!["linear".to_string()],
            message: "linear is bound to two accounts".to_string(),
        });
        let export = Export::build(&config, &BTreeMap::new(), ExportFormat::Claude);
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

    const EXISTING_MCP_JSON: &str = r#"{
  "mcpServers": {
    "linear": { "type": "http", "url": "https://linear.example/mcp" },
    "mine": { "command": "mine" }
  },
  "custom": true
}"#;

    #[test]
    fn merge_keeps_same_named_entries_and_unrelated_keys() {
        let export = build(vec![linear(), tool()], ExportFormat::Claude);
        let (contents, report) = export.merge(Some(EXISTING_MCP_JSON), false).unwrap();
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            merged["mcpServers"]["linear"]["url"],
            "https://linear.example/mcp"
        );
        assert_eq!(merged["custom"], true);
        assert_eq!(
            report,
            MergeReport {
                added: vec!["tool".to_string()],
                replaced: Vec::new(),
                kept: vec!["linear".to_string()],
            }
        );
    }

    #[test]
    fn merge_keeps_the_order_of_existing_keys() {
        let export = build(vec![tool()], ExportFormat::Claude);
        let (contents, _) = export.merge(Some(EXISTING_MCP_JSON), false).unwrap();
        let positions: Vec<usize> = ["\"linear\"", "\"mine\"", "\"tool\"", "\"custom\""]
            .iter()
            .map(|key| contents.find(key).unwrap())
            .collect();
        assert!(positions.is_sorted(), "{contents}");
    }

    #[test]
    fn force_replaces_same_named_entries_in_place() {
        let export = build(vec![linear()], ExportFormat::Claude);
        let (contents, report) = export.merge(Some(EXISTING_MCP_JSON), true).unwrap();
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            merged["mcpServers"]["linear"]["url"],
            "https://mcp.linear.app/mcp"
        );
        assert_eq!(report.replaced, ["linear"]);
        assert!(
            contents.find("\"linear\"") < contents.find("\"mine\""),
            "{contents}"
        );
    }

    #[test]
    fn merge_does_not_add_an_input_that_exists() {
        let existing = r#"{ "inputs": [{ "type": "promptString", "id": "tool-api-key", "description": "mine" }] }"#;
        let export = build(vec![tool()], ExportFormat::Vscode);
        let (contents, _) = export.merge(Some(existing), false).unwrap();
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(merged["inputs"].as_array().unwrap().len(), 1);
        assert_eq!(merged["inputs"][0]["description"], "mine");
    }

    #[test]
    fn kept_entries_add_no_inputs() {
        let existing = r#"{ "servers": { "tool": { "type": "stdio", "command": "tool" } } }"#;
        let export = build(vec![tool()], ExportFormat::Vscode);
        let (contents, report) = export.merge(Some(existing), false).unwrap();
        assert!(!report.changes_file());
        assert!(!contents.contains("inputs"), "{contents}");
    }

    #[test]
    fn merge_refuses_files_it_cannot_read_as_json() {
        let export = build(vec![linear()], ExportFormat::Vscode);
        assert!(export.merge(Some("// comment\n{}"), false).is_err());
        assert!(export.merge(Some(r#"{ "servers": [] }"#), false).is_err());
    }

    #[test]
    fn names_become_environment_variables_and_input_ids() {
        assert_eq!(
            environment_variable_name("my-server_x-api-key"),
            "MY_SERVER_X_API_KEY"
        );
        assert_eq!(environment_variable_name("1password"), "_1PASSWORD");
        assert_eq!(input_id("_1PASSWORD_TOKEN"), "1password-token");
    }
}
