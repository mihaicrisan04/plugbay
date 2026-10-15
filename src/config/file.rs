//! One config file: its schema, the polymorphic `[servers]` values, and its executable fingerprint.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{ConfigFile, ConfigScope, SESSION_SOURCE, SessionPin};
use crate::secrets;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigFile {
    #[serde(default)]
    root: bool,
    account: Option<String>,
    #[serde(default)]
    servers: BTreeMap<String, toml::Value>,
    #[serde(default)]
    accounts: BTreeMap<String, BTreeMap<String, AccountSettings>>,
    trusted_paths: Option<Vec<String>>,
}

/// `[accounts.<server>.<account>]`: `url`, `headers` and `env` override the server's,
/// the rest is metadata.
#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct AccountSettings {
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) headers: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    #[serde(flatten)]
    pub(super) metadata: BTreeMap<String, String>,
    /// Filled by [`normalise_header_names`], reported when the server is built.
    #[serde(skip)]
    pub(super) header_problems: Vec<String>,
}

/// The inline-table form of a server entry; the short forms are converted into it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ServerSettings {
    enabled: Option<bool>,
    account: Option<String>,
    accounts: Option<Vec<String>>,
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) headers: BTreeMap<String, String>,
    pub(super) command: Option<String>,
    pub(super) args: Option<Vec<String>>,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    pub(super) cwd: Option<String>,
    pub(super) tools: Option<Vec<String>>,
    pub(super) exclude_tools: Option<Vec<String>>,
    /// Filled by [`normalise_header_names`], reported when the server is built.
    #[serde(skip)]
    pub(super) header_problems: Vec<String>,
}

/// One `[servers]` entry after its polymorphic value has been understood.
#[derive(Debug, Clone)]
pub(super) struct ServerEntry {
    pub(super) enabled: bool,
    pub(super) account: AccountChange,
    pub(super) settings: ServerSettings,
}

#[derive(Debug, Clone)]
pub(super) enum AccountChange {
    /// Inline tables without `account(s)` and `false`: keep whatever was inherited.
    Inherit,
    /// Bare `true`: forget an inherited account and use the tree's default.
    UseTreeDefault,
    /// One account, or the multi-account opt-in.
    Set(Vec<String>),
}

pub(super) struct LoadedFile {
    pub(super) info: ConfigFile,
    pub(super) root: bool,
    pub(super) account: Option<String>,
    pub(super) servers: BTreeMap<String, ServerEntry>,
    pub(super) accounts: BTreeMap<String, BTreeMap<String, AccountSettings>>,
    pub(super) trusted_paths: Option<Vec<String>>,
}

/// A config value that trust has to cover, borrowed from either a file or a merged server.
#[derive(Debug, Clone, Copy)]
pub(super) enum Field<'value> {
    Command(&'value str),
    Args(&'value [String]),
    Cwd(&'value str),
    Env(&'value str),
    Url(&'value str),
    Header(&'value str),
}

/// What a trust grant covers: everything that runs (command, args, env, cwd), where requests go
/// (`url`), and header values that resolve secrets. A plain header is harmless.
/// The one rule shared by file fingerprints and per-server trust.
pub(super) fn is_executable(field: Field<'_>) -> bool {
    match field {
        Field::Header(value) => secrets::contains_secret_ref(value),
        Field::Command(_) | Field::Args(_) | Field::Cwd(_) | Field::Env(_) | Field::Url(_) => true,
    }
}

impl Field<'_> {
    fn to_json(self) -> serde_json::Value {
        match self {
            Field::Args(args) => args.into(),
            Field::Command(value)
            | Field::Cwd(value)
            | Field::Env(value)
            | Field::Url(value)
            | Field::Header(value) => value.into(),
        }
    }
}

/// `Ok(None)` when the file does not exist.
pub(super) fn load(path: &Path, scope: ConfigScope) -> anyhow::Result<Option<LoadedFile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut raw: RawConfigFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    let mut servers = BTreeMap::new();
    for (name, value) in raw.servers {
        let mut entry = parse_server_entry(&name, value)
            .with_context(|| format!("parsing {}", path.display()))?;
        let settings = &mut entry.settings;
        settings.header_problems = normalise_header_names(&mut settings.headers, path);
        servers.insert(name, entry);
    }
    for server_accounts in raw.accounts.values_mut() {
        for settings in server_accounts.values_mut() {
            settings.header_problems = normalise_header_names(&mut settings.headers, path);
        }
    }

    let executable_fingerprint = executable_fingerprint(&servers, &raw.accounts);
    Ok(Some(LoadedFile {
        info: ConfigFile {
            path: path.to_path_buf(),
            scope,
            executable_fingerprint,
        },
        root: raw.root,
        account: raw.account,
        servers,
        accounts: raw.accounts,
        trusted_paths: raw.trusted_paths,
    }))
}

/// A session's pins as one more layer, merged after every real file. It sets no executable field,
/// so it never needs trust; its scope is `Local` only because it is personal like a local file.
pub(super) fn session_pins_file(pins: &BTreeMap<String, SessionPin>) -> LoadedFile {
    let mut servers = BTreeMap::new();
    for (name, pin) in pins {
        let entry = match pin {
            SessionPin::On {
                account: Some(account),
            } => ServerEntry {
                enabled: true,
                account: AccountChange::Set(vec![account.clone()]),
                settings: ServerSettings::default(),
            },
            SessionPin::On { account: None } => ServerEntry {
                enabled: true,
                account: AccountChange::Inherit,
                settings: ServerSettings::default(),
            },
            SessionPin::Off => ServerEntry {
                enabled: false,
                account: AccountChange::Inherit,
                settings: ServerSettings::default(),
            },
        };
        servers.insert(name.clone(), entry);
    }
    LoadedFile {
        info: ConfigFile {
            path: Path::new(SESSION_SOURCE).to_path_buf(),
            scope: ConfigScope::Local,
            executable_fingerprint: None,
        },
        root: false,
        account: None,
        servers,
        accounts: BTreeMap::new(),
        trusted_paths: None,
    }
}

/// Header names are case-insensitive, so they are lowercased before merging: otherwise an
/// inherited `authorization` and a deeper `Authorization` would both survive and the HTTP layer
/// would pick one by map order. Names that collide within one table are returned as problems.
fn normalise_header_names(headers: &mut BTreeMap<String, String>, path: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let mut normalised: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in std::mem::take(headers) {
        let lowercase = name.to_ascii_lowercase();
        if normalised.contains_key(&lowercase) {
            problems.push(format!(
                "header `{name}` is set twice with different casing in {}",
                path.display()
            ));
        }
        normalised.insert(lowercase, value);
    }
    *headers = normalised;
    problems
}

fn parse_server_entry(name: &str, value: toml::Value) -> anyhow::Result<ServerEntry> {
    match value {
        toml::Value::Boolean(true) => Ok(ServerEntry {
            enabled: true,
            account: AccountChange::UseTreeDefault,
            settings: ServerSettings::default(),
        }),
        toml::Value::Boolean(false) => Ok(ServerEntry {
            enabled: false,
            account: AccountChange::Inherit,
            settings: ServerSettings::default(),
        }),
        toml::Value::String(account) => Ok(ServerEntry {
            enabled: true,
            account: AccountChange::Set(vec![account]),
            settings: ServerSettings::default(),
        }),
        toml::Value::Array(_) => {
            let accounts: Vec<String> = value
                .try_into()
                .with_context(|| format!("servers.{name}: a list must hold account names"))?;
            Ok(ServerEntry {
                enabled: true,
                account: AccountChange::Set(accounts),
                settings: ServerSettings::default(),
            })
        }
        toml::Value::Table(_) => {
            let settings: ServerSettings = value
                .try_into()
                .with_context(|| format!("invalid servers.{name}"))?;
            parse_server_table(name, settings)
        }
        other => bail!(
            "servers.{name}: expected an account name, true/false, a list of accounts or a table, found a {}",
            other.type_str()
        ),
    }
}

fn parse_server_table(name: &str, mut settings: ServerSettings) -> anyhow::Result<ServerEntry> {
    if settings.url.is_some() && settings.command.is_some() {
        bail!("servers.{name}: set either `url` (remote) or `command` (stdio), not both");
    }
    let account = match (settings.account.take(), settings.accounts.take()) {
        (Some(_), Some(_)) => {
            bail!("servers.{name}: set either `account` or `accounts`, not both")
        }
        (Some(account), None) => AccountChange::Set(vec![account]),
        (None, Some(accounts)) => AccountChange::Set(accounts),
        (None, None) => AccountChange::Inherit,
    };
    Ok(ServerEntry {
        enabled: settings.enabled.take().unwrap_or(true),
        account,
        settings,
    })
}

/// Hashes a canonical JSON object of only the executable parts, keyed by their config path,
/// so reformatting, reordering or editing anything else leaves the fingerprint unchanged.
fn executable_fingerprint(
    servers: &BTreeMap<String, ServerEntry>,
    accounts: &BTreeMap<String, BTreeMap<String, AccountSettings>>,
) -> Option<String> {
    let mut parts = FingerprintParts::default();

    for (name, entry) in servers {
        let settings = &entry.settings;
        let prefix = format!("servers.{name}");
        if let Some(command) = &settings.command {
            parts.add(format!("{prefix}.command"), Field::Command(command));
        }
        if let Some(args) = &settings.args {
            parts.add(format!("{prefix}.args"), Field::Args(args));
        }
        if let Some(cwd) = &settings.cwd {
            parts.add(format!("{prefix}.cwd"), Field::Cwd(cwd));
        }
        if let Some(url) = &settings.url {
            parts.add(format!("{prefix}.url"), Field::Url(url));
        }
        for (key, value) in &settings.env {
            parts.add(format!("{prefix}.env.{key}"), Field::Env(value));
        }
        for (key, value) in &settings.headers {
            parts.add(format!("{prefix}.headers.{key}"), Field::Header(value));
        }
    }

    for (server, server_accounts) in accounts {
        for (account, settings) in server_accounts {
            let prefix = format!("accounts.{server}.{account}");
            if let Some(url) = &settings.url {
                parts.add(format!("{prefix}.url"), Field::Url(url));
            }
            for (key, value) in &settings.env {
                parts.add(format!("{prefix}.env.{key}"), Field::Env(value));
            }
            for (key, value) in &settings.headers {
                parts.add(format!("{prefix}.headers.{key}"), Field::Header(value));
            }
        }
    }

    parts.fingerprint()
}

#[derive(Default)]
struct FingerprintParts(BTreeMap<String, serde_json::Value>);

impl FingerprintParts {
    fn add(&mut self, key: String, field: Field<'_>) {
        if is_executable(field) {
            self.0.insert(key, field.to_json());
        }
    }

    fn fingerprint(self) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let canonical = serde_json::Value::Object(self.0.into_iter().collect()).to_string();
        Some(hex::encode(Sha256::digest(canonical.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    fn fingerprint_of(fixture: &Fixture, relative: &str, contents: &str) -> Option<String> {
        let path = fixture.write(relative, contents);
        load(&path, ConfigScope::Project)
            .unwrap()
            .unwrap()
            .info
            .executable_fingerprint
    }

    #[test]
    fn fingerprint_is_none_without_executable_parts() {
        let fixture = Fixture::new();
        let fingerprint = fingerprint_of(
            &fixture,
            "a/plugbay.toml",
            "account = \"x\"\n[servers]\nlinear = true\napi = { tools = [\"a\"], headers = { A = \"plain\" } }\n",
        );
        assert_eq!(fingerprint, None);
    }

    #[test]
    fn fingerprint_ignores_formatting_and_unrelated_edits() {
        let fixture = Fixture::new();
        let original = fingerprint_of(
            &fixture,
            "a/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\", args = [\"serve\"], env = { A = \"1\" } }\nlinear = true\n",
        );
        let reformatted = fingerprint_of(
            &fixture,
            "b/plugbay.toml",
            r#"
            # a comment
            account = "someone"

            [servers.tool]
            env.A = "1"
            args = [ "serve" ]
            command = "tool-mcp"
            tools = ["read_*"]

            [servers.sentry]
            account = "x"
            "#,
        );

        assert!(original.is_some());
        assert_eq!(original, reformatted);
    }

    #[test]
    fn fingerprint_changes_when_executable_parts_change() {
        let fixture = Fixture::new();
        let original = fingerprint_of(
            &fixture,
            "a/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\" }\n",
        );
        let new_args = fingerprint_of(
            &fixture,
            "b/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\", args = [\"--evil\"] }\n",
        );
        let new_account_env = fingerprint_of(
            &fixture,
            "c/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\" }\n[accounts.tool.x]\nenv = { MODE = \"x\" }\n",
        );

        assert_ne!(original, new_args);
        assert_ne!(original, new_account_env);
    }

    #[test]
    fn fingerprint_covers_server_and_account_urls() {
        let fixture = Fixture::new();
        let server_url = fingerprint_of(
            &fixture,
            "a/plugbay.toml",
            "[servers]\napi = { url = \"https://a.example.com/mcp\" }\n",
        );
        let other_server_url = fingerprint_of(
            &fixture,
            "b/plugbay.toml",
            "[servers]\napi = { url = \"https://b.example.com/mcp\" }\n",
        );
        let account_url = fingerprint_of(
            &fixture,
            "c/plugbay.toml",
            "[accounts.api.x]\nurl = \"https://c.example.com/mcp\"\n",
        );

        assert!(server_url.is_some());
        assert!(account_url.is_some());
        assert_ne!(server_url, other_server_url);
    }

    #[test]
    fn plain_headers_are_not_executable_but_secret_refs_are() {
        assert!(!is_executable(Field::Header("application/json")));
        assert!(is_executable(Field::Header("Bearer ${env:TOKEN}")));
        assert!(is_executable(Field::Url("https://example.com")));
        assert!(is_executable(Field::Env("plain")));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let fixture = Fixture::new();
        fixture.write("app/plugbay.toml", "[servrs]\nlinear = true\n");
        let error = format!("{:#}", fixture.try_resolve("app").unwrap_err());
        assert!(error.contains("servrs"), "{error}");
    }

    #[test]
    fn unknown_server_fields_are_rejected() {
        let fixture = Fixture::new();
        fixture.write(
            "app/plugbay.toml",
            "[servers]\nlinear = { acount = \"x\" }\n",
        );
        let error = format!("{:#}", fixture.try_resolve("app").unwrap_err());
        assert!(error.contains("acount"), "{error}");
        assert!(error.contains("servers.linear"), "{error}");
    }

    #[test]
    fn url_and_command_together_are_rejected() {
        let fixture = Fixture::new();
        fixture.write(
            "app/plugbay.toml",
            "[servers]\nx = { url = \"https://example.com\", command = \"x\" }\n",
        );
        assert!(fixture.try_resolve("app").is_err());
    }

    #[test]
    fn account_and_accounts_together_are_rejected() {
        let fixture = Fixture::new();
        fixture.write(
            "app/plugbay.toml",
            "[servers]\nlinear = { account = \"a\", accounts = [\"b\"] }\n",
        );
        assert!(fixture.try_resolve("app").is_err());
    }
}
