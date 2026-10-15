//! Layering loaded files: deeper wins field by field, tables merge per key, lists replace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::file::{AccountChange, AccountSettings, LoadedFile, ServerEntry};
use super::{ConfigProblem, ConfigScope};

/// A value plus the file that set it.
#[derive(Debug, Clone)]
pub(super) struct Sourced<T> {
    pub(super) value: T,
    pub(super) source: PathBuf,
}

impl<T> Sourced<T> {
    fn new(value: T, source: &Path) -> Self {
        Self {
            value,
            source: source.to_path_buf(),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct MergedConfig {
    /// Nearest top-level `account = ...`.
    pub(super) tree_account: Option<Sourced<String>>,
    pub(super) servers: BTreeMap<String, MergedServer>,
    /// Keyed by (server name, account).
    pub(super) accounts: BTreeMap<(String, String), MergedAccount>,
    pub(super) trusted_paths: Vec<String>,
    pub(super) problems: Vec<ConfigProblem>,
}

#[derive(Debug, Default)]
pub(super) struct MergedServer {
    pub(super) enabled: Option<Sourced<bool>>,
    pub(super) accounts: Option<Sourced<Vec<String>>>,
    pub(super) url: Option<Sourced<String>>,
    pub(super) headers: BTreeMap<String, Sourced<String>>,
    pub(super) command: Option<Sourced<String>>,
    pub(super) args: Option<Sourced<Vec<String>>>,
    pub(super) env: BTreeMap<String, Sourced<String>>,
    pub(super) cwd: Option<Sourced<PathBuf>>,
    pub(super) tools: Option<Sourced<Vec<String>>>,
    pub(super) exclude_tools: Option<Sourced<Vec<String>>>,
    /// Header names that collided by case inside one file; they exclude the server.
    pub(super) header_problems: Vec<String>,
}

#[derive(Debug, Default)]
pub(super) struct MergedAccount {
    pub(super) url: Option<Sourced<String>>,
    pub(super) headers: BTreeMap<String, Sourced<String>>,
    pub(super) env: BTreeMap<String, Sourced<String>>,
    pub(super) metadata: BTreeMap<String, Sourced<String>>,
    pub(super) header_problems: Vec<String>,
}

impl MergedConfig {
    pub(super) fn apply_file(&mut self, file: &LoadedFile, home: &Path) {
        let path = &file.info.path;

        if let Some(trusted_paths) = &file.trusted_paths {
            if file.info.scope == ConfigScope::Global {
                for pattern in trusted_paths {
                    self.trusted_paths.push(expand_home_string(pattern, home));
                }
            } else {
                self.problems.push(ConfigProblem {
                    servers: Vec::new(),
                    message: format!(
                        "`trusted_paths` is only read from the global config; ignored in {}",
                        path.display()
                    ),
                });
            }
        }

        if let Some(account) = &file.account {
            self.tree_account = Some(Sourced::new(account.clone(), path));
        }

        for (name, entry) in &file.servers {
            let server = self.servers.entry(name.clone()).or_default();
            server.apply(entry, path, home);
        }

        for (server_name, server_accounts) in &file.accounts {
            for (account_name, settings) in server_accounts {
                let key = (server_name.clone(), account_name.clone());
                self.accounts.entry(key).or_default().apply(settings, path);
            }
        }
    }
}

impl MergedServer {
    fn apply(&mut self, entry: &ServerEntry, source: &Path, home: &Path) {
        self.enabled = Some(Sourced::new(entry.enabled, source));
        match &entry.account {
            AccountChange::Inherit => {}
            AccountChange::UseTreeDefault => self.accounts = None,
            AccountChange::Set(accounts) => {
                self.accounts = Some(Sourced::new(accounts.clone(), source))
            }
        }

        let settings = &entry.settings;
        if let Some(url) = &settings.url {
            self.forget_stdio_fields();
            self.url = Some(Sourced::new(url.clone(), source));
        }
        if let Some(command) = &settings.command {
            self.forget_http_fields();
            let command = expand_home_string(command, home);
            self.command = Some(Sourced::new(command, source));
        }
        if let Some(args) = &settings.args {
            self.args = Some(Sourced::new(args.clone(), source));
        }
        if let Some(cwd) = &settings.cwd {
            let cwd = resolve_cwd(cwd, source, home);
            self.cwd = Some(Sourced::new(cwd, source));
        }
        merge_per_key(&mut self.headers, &settings.headers, source);
        merge_per_key(&mut self.env, &settings.env, source);
        self.header_problems
            .extend(settings.header_problems.iter().cloned());
        if let Some(tools) = &settings.tools {
            self.tools = Some(Sourced::new(tools.clone(), source));
        }
        if let Some(exclude_tools) = &settings.exclude_tools {
            self.exclude_tools = Some(Sourced::new(exclude_tools.clone(), source));
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.enabled.as_ref().is_none_or(|enabled| enabled.value)
    }

    fn forget_stdio_fields(&mut self) {
        self.command = None;
        self.args = None;
        self.env.clear();
        self.cwd = None;
    }

    fn forget_http_fields(&mut self) {
        self.url = None;
        self.headers.clear();
    }
}

impl MergedAccount {
    fn apply(&mut self, settings: &AccountSettings, source: &Path) {
        if let Some(url) = &settings.url {
            self.url = Some(Sourced::new(url.clone(), source));
        }
        merge_per_key(&mut self.headers, &settings.headers, source);
        merge_per_key(&mut self.env, &settings.env, source);
        merge_per_key(&mut self.metadata, &settings.metadata, source);
        self.header_problems
            .extend(settings.header_problems.iter().cloned());
    }
}

fn merge_per_key(
    target: &mut BTreeMap<String, Sourced<String>>,
    values: &BTreeMap<String, String>,
    source: &Path,
) {
    for (key, value) in values {
        target.insert(key.clone(), Sourced::new(value.clone(), source));
    }
}

/// `~` expands to home; a relative `cwd` is relative to the file that set it.
fn resolve_cwd(cwd: &str, source: &Path, home: &Path) -> PathBuf {
    let expanded = expand_home(cwd, home);
    if expanded.is_absolute() {
        return expanded;
    }
    match source.parent() {
        Some(file_dir) => file_dir.join(expanded),
        None => expanded,
    }
}

fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}

fn expand_home_string(value: &str, home: &Path) -> String {
    expand_home(value, home).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::ServerTransport;
    use super::super::tests::{Fixture, names_and_accounts, only};

    #[test]
    fn false_disables_inherited_server_and_table_reenables_it() {
        let fixture = Fixture::new();
        fixture.write("a/plugbay.toml", "[servers]\nlinear = \"team\"\n");
        fixture.write("a/b/plugbay.toml", "[servers]\nlinear = false\n");
        fixture.write(
            "a/b/c/plugbay.toml",
            "[servers]\nlinear = { tools = [\"list_*\"] }\n",
        );

        assert!(fixture.resolve("a/b").servers.is_empty());
        let config = fixture.resolve("a/b/c");
        assert_eq!(names_and_accounts(&config), ["linear:team"]);
    }

    #[test]
    fn tree_account_applies_to_servers_without_one() {
        let fixture = Fixture::new();
        fixture.write(
            "work/plugbay.toml",
            "account = \"agency\"\n[servers]\nlinear = true\n",
        );
        let client = fixture.write("work/client/plugbay.toml", "account = \"client\"\n");

        let config = fixture.resolve("work/client");

        assert_eq!(names_and_accounts(&config), ["linear:client"]);
        assert_eq!(only(&config, "linear").provenance["account"], client);
    }

    #[test]
    fn explicit_account_beats_deeper_tree_account() {
        let fixture = Fixture::new();
        fixture.write("work/plugbay.toml", "[servers]\nlinear = \"initech\"\n");
        fixture.write("work/client/plugbay.toml", "account = \"client\"\n");

        let config = fixture.resolve("work/client");

        assert_eq!(names_and_accounts(&config), ["linear:initech"]);
    }

    #[test]
    fn bare_true_drops_inherited_account_for_tree_default() {
        let fixture = Fixture::new();
        fixture.write("work/plugbay.toml", "[servers]\nsentry = \"initech\"\n");
        fixture.write(
            "work/client/plugbay.toml",
            "account = \"client\"\n[servers]\nsentry = true\n",
        );

        let config = fixture.resolve("work/client");

        assert_eq!(names_and_accounts(&config), ["sentry:client"]);
    }

    #[test]
    fn account_metadata_merges_per_key() {
        let fixture = Fixture::new();
        fixture.write(
            "work/plugbay.toml",
            "[servers]\nposthog = \"a\"\n[accounts.posthog.a]\nregion = \"us\"\nteam = \"core\"\n",
        );
        fixture.write(
            "work/app/plugbay.toml",
            "[accounts.posthog.a]\nregion = \"eu\"\n",
        );

        let config = fixture.resolve("work/app");
        let metadata = &only(&config, "posthog").account_metadata;

        assert_eq!(metadata["region"], "eu");
        assert_eq!(metadata["team"], "core");
    }

    #[test]
    fn headers_merge_per_key_and_lists_replace() {
        let fixture = Fixture::new();
        fixture.write(
            "a/plugbay.toml",
            "[servers]\napi = { url = \"https://example.com/mcp\", headers = { A = \"1\", B = \"2\" }, tools = [\"x\", \"y\"] }\n",
        );
        let deeper = fixture.write(
            "a/b/plugbay.toml",
            "[servers]\napi = { headers = { B = \"3\" }, tools = [\"z\"] }\n",
        );

        let config = fixture.resolve("a/b");
        let api = only(&config, "api");

        let ServerTransport::Http { headers, .. } = &api.transport else {
            panic!("expected http");
        };
        assert_eq!(headers["a"], "1");
        assert_eq!(headers["b"], "3");
        assert_eq!(api.tool_filter.include, ["z"]);
        assert_eq!(api.provenance["headers.b"], deeper);
        assert_eq!(api.provenance["tools"], deeper);
    }

    #[test]
    fn setting_url_clears_inherited_command() {
        let fixture = Fixture::new();
        fixture.write(
            "a/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\", args = [\"--stdio\"], env = { DEBUG = \"1\" } }\n",
        );
        fixture.write(
            "a/b/plugbay.toml",
            "[servers]\ntool = { url = \"https://tool.example.com/mcp\" }\n",
        );

        let config = fixture.resolve("a/b");
        let tool = only(&config, "tool");

        assert!(!tool.is_stdio());
        assert!(!tool.provenance.contains_key("command"));
        assert!(tool.requires_trust_from.is_empty());
    }

    #[test]
    fn setting_command_clears_inherited_url() {
        let fixture = Fixture::new();
        fixture.write(
            "a/plugbay.toml",
            "[servers]\ntool = { url = \"https://tool.example.com/mcp\", headers = { A = \"1\" } }\n",
        );
        fixture.write(
            "a/b/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\" }\n",
        );

        let config = fixture.resolve("a/b");
        let tool = only(&config, "tool");

        assert!(tool.is_stdio());
        assert!(!tool.provenance.contains_key("url"));
        assert!(!tool.provenance.contains_key("headers.a"));
    }

    #[test]
    fn relative_cwd_and_home_command_are_expanded() {
        let fixture = Fixture::new();
        fixture.write(
            "app/plugbay.toml",
            "[servers]\ntool = { command = \"~/bin/tool\", cwd = \"scripts\" }\n",
        );

        let config = fixture.resolve("app");

        let ServerTransport::Stdio { command, cwd, .. } = &only(&config, "tool").transport else {
            panic!("expected stdio");
        };
        let expected_command = fixture.paths.home.join("bin/tool").display().to_string();
        assert_eq!(*command, expected_command);
        assert_eq!(
            cwd.as_deref(),
            Some(fixture.root.join("app/scripts").as_path())
        );
    }

    #[test]
    fn trusted_paths_only_come_from_global_config() {
        let fixture = Fixture::new();
        fixture.write_global("trusted_paths = [\"~/work/**\"]\n");
        fixture.write("app/plugbay.toml", "trusted_paths = [\"/**\"]\n");

        let config = fixture.resolve("app");

        let expected = fixture.paths.home.join("work/**").display().to_string();
        assert_eq!(config.trusted_paths, [expected]);
        assert_eq!(config.problems.len(), 1);
        assert!(config.problems[0].servers.is_empty());
    }

    #[test]
    fn account_env_merges_per_key() {
        let fixture = Fixture::new();
        fixture.write(
            "a/plugbay.toml",
            "[servers]\ntool = { command = \"t\", account = \"x\" }\n[accounts.tool.x]\nenv = { A = \"1\", B = \"2\" }\n",
        );
        fixture.write(
            "a/b/plugbay.toml",
            "[accounts.tool.x]\nenv = { B = \"3\" }\n",
        );

        let config = fixture.resolve("a/b");

        let ServerTransport::Stdio { env, .. } = &only(&config, "tool").transport else {
            panic!("expected stdio");
        };
        let expected = BTreeMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "3".to_string()),
        ]);
        assert_eq!(*env, expected);
    }
}
