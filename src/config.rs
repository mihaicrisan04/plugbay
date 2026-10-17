//! Loading, merging and resolving `plugbay.toml` files. Schema: docs/config-schema.md.
//!
//! Three stages: [`file`] parses one file, [`merge`] layers files field by field,
//! [`build`] turns the merged layers into servers and reports problems.

mod build;
mod file;
mod merge;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use globset::{Glob, GlobSet, GlobSetBuilder};
use url::Url;

use crate::paths::Paths;
use crate::project::ProjectLocation;

use self::file::LoadedFile;
use self::merge::MergedConfig;

pub const PROJECT_FILE: &str = "plugbay.toml";
pub const LOCAL_FILE: &str = "plugbay.local.toml";
pub const DEFAULT_ACCOUNT: &str = "default";
/// Provenance source for a `url` filled in from the bundled registry.
pub const REGISTRY_SOURCE: &str = "<registry>";
/// Provenance source for servers an agent switched on or off with `plugbay__enable` / `plugbay__disable`.
pub const SESSION_SOURCE: &str = "<this session>";
/// The daemon exposes its own tools (`plugbay__status`) under this name.
pub const RESERVED_SERVER_NAME: &str = "plugbay";
/// Headers that would let config (or a stored token) change where a request goes (`host`) or
/// break HTTP framing. Where credentials go is decided by the url alone, which the trust rules cover.
pub const FORBIDDEN_HEADERS: [&str; 4] =
    ["host", "content-length", "transfer-encoding", "connection"];

/// Everything one directory resolves to. Built fresh on every config change; cheap to clone.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub location: ProjectLocation,
    /// Files that exist and were merged, in merge order (global first, deepest local last).
    pub files: Vec<ConfigFile>,
    /// Enabled servers, one per (server name, account).
    pub servers: Vec<ResolvedServer>,
    /// Inherited servers switched off with `name = false`, for `ls` and `why`.
    pub disabled: Vec<DisabledServer>,
    /// Servers the global config defines with `enabled = false`: known everywhere, on only where pinned.
    pub catalog: Vec<String>,
    /// Problems that excluded a server (e.g. two accounts on one server without opting in).
    pub problems: Vec<ConfigProblem>,
    /// `trusted_paths` globs from the global config, home-expanded.
    pub trusted_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub scope: ConfigScope,
    /// sha256 over the parts of this file that run code, pick where requests go, or resolve
    /// secrets (stdio command/args/env/cwd, `url`s, header values with `${...}` refs).
    /// `None` when no resolved server needs trust from this file, so remote-only files never
    /// ask for it. Formatting and other edits don't change it.
    pub executable_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    /// `~/.config/plugbay/config.toml`: always trusted.
    Global,
    /// A committed `plugbay.toml`.
    Project,
    /// A personal `plugbay.local.toml`.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServer {
    /// Config key, e.g. `linear`.
    pub name: String,
    pub account: String,
    /// Prefix for exposed tools: `name`, or `name-account` when the server opted into several accounts.
    pub tool_prefix: String,
    pub transport: ServerTransport,
    pub identity: ServerIdentity,
    pub tool_filter: ToolFilter,
    /// From `[accounts.<server>.<account>]`, e.g. `region = "eu"`, shown to the agent.
    pub account_metadata: BTreeMap<String, String>,
    /// Which file last set each field (`url`, `account`, `command`, `headers.Authorization`, ...).
    pub provenance: BTreeMap<String, PathBuf>,
    /// Non-global files that set an executable field of this server; each must be trusted.
    pub requires_trust_from: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerTransport {
    /// Streamable HTTP. Header values may contain `${...}` secret refs.
    Http {
        url: Url,
        headers: BTreeMap<String, String>,
    },
    /// A local process. Env values may contain `${...}` secret refs.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        /// Defaults to the session's working directory when unset.
        cwd: Option<PathBuf>,
    },
}

/// What credentials are bound to: a normalised URL for remote servers, a hash of
/// command + args for stdio ones. Renaming a server in config never changes it,
/// and pointing a name at another URL never inherits its token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerIdentity(pub String);

/// Glob filters on upstream tool names. The globs are compiled on first use.
#[derive(Clone, Default)]
pub struct ToolFilter {
    /// Globs; empty means every tool.
    pub include: Vec<String>,
    /// Globs applied after `include`.
    pub exclude: Vec<String>,
    compiled: OnceLock<CompiledToolFilter>,
}

#[derive(Clone)]
struct CompiledToolFilter {
    include: GlobSet,
    exclude: GlobSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledServer {
    pub name: String,
    pub disabled_by: PathBuf,
}

/// A server an agent switched on or off for its session only, layered over every config file.
/// Pins only choose servers and accounts, so they never need trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPin {
    /// On, with this account or the tree's default.
    On {
        account: Option<String>,
    },
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProblem {
    /// Server names the problem excluded; empty for file-level problems.
    pub servers: Vec<String>,
    pub message: String,
}

/// How the problem for a server with no `url`, no `command` and no registry entry starts.
const UNKNOWN_SERVER: &str = "unknown server";

impl ConfigProblem {
    /// The server has no definition anywhere, so `plugbay add` is the fix.
    pub fn is_unknown_server(&self) -> bool {
        self.message.starts_with(UNKNOWN_SERVER)
    }
}

/// Resolves the config for `location`: global config, then every `plugbay.toml` and
/// `plugbay.local.toml` from `/` down to `location.config_dir`, deeper winning field by field,
/// stopping at the deepest `root = true`.
///
/// Fails only for unreadable or unparsable files; semantic problems land in `problems`.
pub fn resolve(paths: &Paths, location: &ProjectLocation) -> anyhow::Result<ResolvedConfig> {
    resolve_with_pins(paths, location, &BTreeMap::new())
}

/// [`resolve`], then a session's own pins on top of every file.
pub fn resolve_with_pins(
    paths: &Paths,
    location: &ProjectLocation,
    session_pins: &BTreeMap<String, SessionPin>,
) -> anyhow::Result<ResolvedConfig> {
    let files = load_files(paths, location)?;

    let mut merged = MergedConfig::default();
    for file in &files {
        merged.apply_file(file, &paths.home);
    }
    if !session_pins.is_empty() {
        merged.apply_file(&file::session_pins_file(session_pins), &paths.home);
    }

    let built = build::build_servers(&merged, &paths.global_config());
    let mut problems = merged.problems;
    problems.extend(built.problems);

    let mut config_files: Vec<ConfigFile> = files.into_iter().map(|file| file.info).collect();
    clear_unneeded_fingerprints(&mut config_files, &built.servers);

    Ok(ResolvedConfig {
        location: location.clone(),
        files: config_files,
        servers: built.servers,
        disabled: built.disabled,
        catalog: built.catalog,
        problems,
        trusted_paths: merged.trusted_paths,
    })
}

/// Every path whose creation, change or removal could change what `location` resolves to,
/// known even when a file fails to parse so a fix is still noticed.
pub fn watched_paths(paths: &Paths, location: &ProjectLocation) -> Vec<PathBuf> {
    let mut watched = vec![paths.global_config()];
    for candidate in tree_candidates(location) {
        watched.push(candidate.path);
    }
    watched
}

/// Server names and accounts: lowercase letters, digits and `-` (tools are exposed as `server__tool`).
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

impl ResolvedConfig {
    pub fn server(&self, name: &str) -> Vec<&ResolvedServer> {
        self.servers
            .iter()
            .filter(|server| server.name == name)
            .collect()
    }
}

impl ResolvedServer {
    /// `name:account`, the form users type in `plugbay login`.
    pub fn display_name(&self) -> String {
        format!("{}:{}", self.name, self.account)
    }

    pub fn is_stdio(&self) -> bool {
        matches!(self.transport, ServerTransport::Stdio { .. })
    }

    /// Remote servers authenticate with what `plugbay login` stored (a token or an OAuth login)
    /// unless the config gives them a static `Authorization` header.
    pub fn uses_stored_login(&self) -> bool {
        match &self.transport {
            ServerTransport::Http { headers, .. } => !headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization")),
            ServerTransport::Stdio { .. } => false,
        }
    }
}

impl ServerTransport {
    /// One-line rendering for people: the url with secrets masked (see [`display_url`]), or the
    /// command line with spaced args quoted. Never use it to connect.
    pub fn target(&self) -> String {
        match self {
            ServerTransport::Http { url, .. } => display_url(url),
            ServerTransport::Stdio { command, args, .. } => {
                let mut words = vec![quote_if_spaced(command)];
                for arg in args {
                    words.push(quote_if_spaced(arg));
                }
                words.join(" ")
            }
        }
    }
}

const MASK: &str = "***";

/// `url` for status text, prompts and logs: userinfo, query values, the fragment and
/// token-looking path segments (`/mcp/sk-ak-…/sse`) are masked, since some servers take their
/// API key there. Scheme, host, port and the rest of the path stay readable.
pub fn display_url(url: &Url) -> String {
    let mut shown = format!("{}://", url.scheme());
    if !url.username().is_empty() || url.password().is_some() {
        shown.push_str(MASK);
        shown.push('@');
    }
    shown.push_str(url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        shown.push_str(&format!(":{port}"));
    }
    let segments: Vec<&str> = url.path().split('/').map(mask_if_token).collect();
    shown.push_str(&segments.join("/"));
    if let Some(query) = url.query() {
        let pairs: Vec<String> = query.split('&').map(mask_query_value).collect();
        shown.push('?');
        shown.push_str(&pairs.join("&"));
    }
    if url.fragment().is_some() {
        shown.push('#');
        shown.push_str(MASK);
    }
    shown
}

/// Replaces every occurrence of `url` in `text`, e.g. a transport error, with [`display_url`].
pub fn mask_url_in(text: &str, url: &Url) -> String {
    text.replace(url.as_str(), &display_url(url))
}

fn mask_query_value(pair: &str) -> String {
    match pair.split_once('=') {
        Some((key, value)) if !value.is_empty() => format!("{key}={MASK}"),
        _ => mask_if_token(pair).to_string(),
    }
}

/// Long runs of letters and digits look like keys; words, versions and short ids don't.
fn mask_if_token(segment: &str) -> &str {
    let token_characters = segment
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'));
    let has_digit = segment.chars().any(|character| character.is_ascii_digit());
    let has_letter = segment
        .chars()
        .any(|character| character.is_ascii_alphabetic());
    if segment.len() >= 20 && token_characters && has_digit && has_letter {
        MASK
    } else {
        segment
    }
}

fn quote_if_spaced(word: &str) -> String {
    if word.is_empty() || word.contains(char::is_whitespace) {
        format!("{word:?}")
    } else {
        word.to_string()
    }
}

impl ServerIdentity {
    /// A hash of command + args (NUL-separated), so env and cwd changes keep the identity.
    pub fn for_stdio(command: &str, args: &[String]) -> Self {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(command.as_bytes());
        for arg in args {
            hasher.update([0]);
            hasher.update(arg.as_bytes());
        }
        Self(format!("stdio:{}", hex::encode(hasher.finalize())))
    }

    /// The url without query or fragment and without a trailing slash (except the root path).
    /// `Url` already lowercases scheme and host and drops default ports.
    pub fn for_url(url: &Url) -> Self {
        let mut normalised = url.clone();
        normalised.set_query(None);
        normalised.set_fragment(None);
        let trimmed_path = normalised.path().trim_end_matches('/').to_string();
        if trimmed_path.is_empty() {
            normalised.set_path("/");
        } else {
            normalised.set_path(&trimmed_path);
        }
        Self(normalised.to_string())
    }
}

impl ServerIdentity {
    /// For logs: a url identity keeps its path, which may hold a key, so it is masked like
    /// [`display_url`]. Stdio identities are hashes already.
    pub fn masked(&self) -> String {
        match Url::parse(&self.0) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => display_url(&url),
            _ => self.0.clone(),
        }
    }
}

impl std::fmt::Display for ServerIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl ToolFilter {
    pub fn new(include: Vec<String>, exclude: Vec<String>) -> Self {
        Self {
            include,
            exclude,
            compiled: OnceLock::new(),
        }
    }

    pub fn allows(&self, tool_name: &str) -> bool {
        let compiled = self.compiled.get_or_init(|| CompiledToolFilter {
            include: compile_globs(&self.include),
            exclude: compile_globs(&self.exclude),
        });
        let included = self.include.is_empty() || compiled.include.is_match(tool_name);
        included && !compiled.exclude.is_match(tool_name)
    }
}

/// Invalid globs are reported as config problems during resolution; here they are skipped.
fn compile_globs(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

impl PartialEq for ToolFilter {
    fn eq(&self, other: &Self) -> bool {
        self.include == other.include && self.exclude == other.exclude
    }
}

impl Eq for ToolFilter {}

impl std::fmt::Debug for ToolFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolFilter")
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .finish()
    }
}

struct CandidateFile {
    path: PathBuf,
    scope: ConfigScope,
}

/// `plugbay.toml` and `plugbay.local.toml` in every directory from `/` down to the project dir.
fn tree_candidates(location: &ProjectLocation) -> Vec<CandidateFile> {
    let Some(config_dir) = &location.config_dir else {
        return Vec::new();
    };
    let mut directories: Vec<&Path> = config_dir.ancestors().collect();
    directories.reverse();

    let mut candidates = Vec::new();
    for directory in directories {
        candidates.push(CandidateFile {
            path: directory.join(PROJECT_FILE),
            scope: ConfigScope::Project,
        });
        candidates.push(CandidateFile {
            path: directory.join(LOCAL_FILE),
            scope: ConfigScope::Local,
        });
    }
    candidates
}

/// The global config plus tree files from the deepest `root = true` down, in merge order.
/// Tree files are read deepest first so files above a `root = true` are never parsed:
/// a broken parent can't break a project that walled itself off.
fn load_files(paths: &Paths, location: &ProjectLocation) -> anyhow::Result<Vec<LoadedFile>> {
    let mut tree_files = Vec::new();
    for candidate in tree_candidates(location).iter().rev() {
        let Some(file) = file::load(&candidate.path, candidate.scope)? else {
            continue;
        };
        let stops_inheritance = file.root;
        tree_files.push(file);
        if stops_inheritance {
            break;
        }
    }
    tree_files.reverse();

    let mut files = Vec::new();
    if let Some(global) = file::load(&paths.global_config(), ConfigScope::Global)? {
        files.push(global);
    }
    files.extend(tree_files);
    Ok(files)
}

/// A file's fingerprint only matters while a server needs trust from it. Clearing the rest keeps
/// files whose only fingerprinted part is a harmless `url` from ever asking for trust.
fn clear_unneeded_fingerprints(files: &mut [ConfigFile], servers: &[ResolvedServer]) {
    for file in files {
        let needed = servers
            .iter()
            .any(|server| server.requires_trust_from.contains(&file.path));
        if !needed {
            file.executable_fingerprint = None;
        }
    }
}

#[cfg(test)]
impl ResolvedServer {
    /// A remote server named `name` with no filters, provenance or trust needs; adjust fields as needed.
    pub(crate) fn test_http(name: &str, account: &str, url: &str) -> Self {
        let url = Url::parse(url).expect("test url");
        Self {
            name: name.to_string(),
            account: account.to_string(),
            tool_prefix: name.to_string(),
            identity: ServerIdentity::for_url(&url),
            transport: ServerTransport::Http {
                url,
                headers: BTreeMap::new(),
            },
            tool_filter: ToolFilter::default(),
            account_metadata: BTreeMap::new(),
            provenance: BTreeMap::new(),
            requires_trust_from: Vec::new(),
        }
    }

    /// A stdio server with no env or cwd; `requires_trust_from` is left empty.
    pub(crate) fn test_stdio(name: &str, account: &str, command: &str, args: &[&str]) -> Self {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        Self {
            name: name.to_string(),
            account: account.to_string(),
            tool_prefix: name.to_string(),
            identity: ServerIdentity::for_stdio(command, &args),
            transport: ServerTransport::Stdio {
                command: command.to_string(),
                args,
                env: BTreeMap::new(),
                cwd: None,
            },
            tool_filter: ToolFilter::default(),
            account_metadata: BTreeMap::new(),
            provenance: BTreeMap::new(),
            requires_trust_from: Vec::new(),
        }
    }
}

#[cfg(test)]
impl ResolvedConfig {
    /// Global-only location, nothing disabled, no problems or trusted paths.
    pub(crate) fn test_with(files: Vec<ConfigFile>, servers: Vec<ResolvedServer>) -> Self {
        Self {
            location: ProjectLocation::global_only(),
            files,
            servers,
            disabled: Vec::new(),
            catalog: Vec::new(),
            problems: Vec::new(),
            trusted_paths: Vec::new(),
        }
    }
}

#[cfg(test)]
impl ConfigFile {
    pub(crate) fn test(path: &str, scope: ConfigScope, fingerprint: Option<&str>) -> Self {
        Self {
            path: PathBuf::from(path),
            scope,
            executable_fingerprint: fingerprint.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    /// A temp tree with its own home and global config; shared by the stage modules' tests.
    pub(super) struct Fixture {
        _temp: TempDir,
        pub(super) paths: Paths,
        pub(super) root: PathBuf,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let paths = Paths::for_test(&root);
            fs::create_dir_all(&paths.home).unwrap();
            Self {
                _temp: temp,
                paths,
                root,
            }
        }

        pub(super) fn dir(&self, relative: &str) -> PathBuf {
            let dir = self.root.join(relative);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        pub(super) fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        pub(super) fn write_global(&self, contents: &str) -> PathBuf {
            let path = self.paths.global_config();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        pub(super) fn resolve(&self, relative_dir: &str) -> ResolvedConfig {
            self.try_resolve(relative_dir).unwrap()
        }

        pub(super) fn try_resolve(&self, relative_dir: &str) -> anyhow::Result<ResolvedConfig> {
            let location = ProjectLocation {
                config_dir: Some(self.dir(relative_dir)),
                working_dir: Some(self.dir(relative_dir)),
            };
            resolve(&self.paths, &location)
        }
    }

    pub(super) fn names_and_accounts(config: &ResolvedConfig) -> Vec<String> {
        config
            .servers
            .iter()
            .map(ResolvedServer::display_name)
            .collect()
    }

    pub(super) fn only<'config>(
        config: &'config ResolvedConfig,
        name: &str,
    ) -> &'config ResolvedServer {
        let servers = config.server(name);
        assert_eq!(servers.len(), 1, "expected one `{name}`, got {servers:?}");
        servers[0]
    }

    pub(super) fn url_of(server: &ResolvedServer) -> String {
        match &server.transport {
            ServerTransport::Http { url, .. } => url.to_string(),
            ServerTransport::Stdio { .. } => panic!("{} is stdio", server.name),
        }
    }

    #[test]
    fn no_project_uses_only_global_config() {
        let fixture = Fixture::new();
        fixture.write_global("[servers]\nlinear = \"initech\"\n");

        let config = resolve(&fixture.paths, &ProjectLocation::global_only()).unwrap();

        assert_eq!(names_and_accounts(&config), ["linear:initech"]);
        assert_eq!(
            watched_paths(&fixture.paths, &ProjectLocation::global_only()),
            [fixture.paths.global_config()]
        );
        assert_eq!(config.files.len(), 1);
        assert_eq!(config.files[0].scope, ConfigScope::Global);
    }

    #[test]
    fn missing_files_resolve_to_nothing() {
        let fixture = Fixture::new();
        let config = fixture.resolve("work/app");
        assert!(config.servers.is_empty());
        assert!(config.files.is_empty());
    }

    #[test]
    fn watched_paths_cover_every_candidate() {
        let fixture = Fixture::new();
        let config = fixture.resolve("work");
        let app = fixture.root.join("work");
        let watched = watched_paths(&fixture.paths, &config.location);

        assert_eq!(watched[0], fixture.paths.global_config());
        assert!(watched.contains(&PathBuf::from("/plugbay.toml")));
        assert!(watched.contains(&PathBuf::from("/plugbay.local.toml")));
        assert!(watched.contains(&app.join(PROJECT_FILE)));
        assert_eq!(watched.last(), Some(&app.join(LOCAL_FILE)));
    }

    #[test]
    fn deeper_file_overrides_account() {
        let fixture = Fixture::new();
        fixture.write("work/plugbay.toml", "[servers]\nlinear = \"initech\"\n");
        let deeper = fixture.write(
            "work/client/plugbay.toml",
            "[servers]\nlinear = \"client\"\n",
        );

        let config = fixture.resolve("work/client");

        assert_eq!(names_and_accounts(&config), ["linear:client"]);
        assert_eq!(only(&config, "linear").provenance["account"], deeper);
    }

    #[test]
    fn schema_example_resolves_like_the_docs_say() {
        let fixture = Fixture::new();
        fixture.write(
            "work/plugbay.toml",
            "[servers]\nlinear = \"initech\"\nsentry = \"initech\"\n",
        );
        let client = fixture.write(
            "work/client-a/plugbay.toml",
            "account = \"client-a\"\n[servers]\nsentry = true\nposthog = true\nlinear = false\n",
        );
        fixture.write(
            "work/client-a/app/plugbay.toml",
            "[servers]\nposthog = { exclude_tools = [\"delete_*\"] }\n",
        );

        let config = fixture.resolve("work/client-a/app");

        assert_eq!(
            names_and_accounts(&config),
            ["posthog:client-a", "sentry:client-a"]
        );
        assert_eq!(
            config.disabled,
            [DisabledServer {
                name: "linear".to_string(),
                disabled_by: client,
            }]
        );
        let posthog = only(&config, "posthog");
        assert_eq!(posthog.tool_filter.exclude, ["delete_*"]);
        assert_eq!(url_of(posthog), "https://mcp.posthog.com/mcp");
    }

    #[test]
    fn local_file_overrides_project_file_in_same_dir() {
        let fixture = Fixture::new();
        fixture.write("app/plugbay.toml", "[servers]\nlinear = \"team\"\n");
        let local = fixture.write("app/plugbay.local.toml", "[servers]\nlinear = \"me\"\n");

        let config = fixture.resolve("app");

        assert_eq!(names_and_accounts(&config), ["linear:me"]);
        assert_eq!(only(&config, "linear").provenance["account"], local);
        let scopes: Vec<ConfigScope> = config.files.iter().map(|file| file.scope).collect();
        assert_eq!(scopes, [ConfigScope::Project, ConfigScope::Local]);
    }

    #[test]
    fn root_true_drops_parent_files_but_keeps_global() {
        let fixture = Fixture::new();
        fixture.write_global("[servers]\nnotion = \"me\"\n");
        fixture.write(
            "work/plugbay.toml",
            "account = \"agency\"\n[servers]\nlinear = true\n",
        );
        fixture.write(
            "work/client/plugbay.toml",
            "root = true\n[servers]\nsentry = true\n",
        );

        let config = fixture.resolve("work/client/app");

        assert_eq!(names_and_accounts(&config), ["notion:me", "sentry:default"]);
        assert_eq!(config.files.len(), 2);
        let watched = watched_paths(&fixture.paths, &config.location);
        assert!(watched.contains(&fixture.root.join("work/plugbay.toml")));
    }

    #[test]
    fn root_true_never_reads_broken_parent_files() {
        let fixture = Fixture::new();
        fixture.write("work/plugbay.toml", "this is [not toml");
        fixture.write(
            "work/client/plugbay.toml",
            "root = true\n[servers]\nsentry = true\n",
        );

        let config = fixture.resolve("work/client");

        assert_eq!(names_and_accounts(&config), ["sentry:default"]);
    }

    #[test]
    fn root_true_in_local_file_drops_project_file_in_same_dir() {
        let fixture = Fixture::new();
        fixture.write("app/plugbay.toml", "[servers]\nlinear = true\n");
        fixture.write(
            "app/plugbay.local.toml",
            "root = true\n[servers]\nsentry = true\n",
        );

        let config = fixture.resolve("app");

        assert_eq!(names_and_accounts(&config), ["sentry:default"]);
    }

    #[test]
    fn broken_global_config_still_fails_under_root_true() {
        let fixture = Fixture::new();
        fixture.write_global("nope = [");
        fixture.write("app/plugbay.toml", "root = true\n");

        assert!(fixture.try_resolve("app").is_err());
    }

    #[test]
    fn remote_only_file_with_url_has_no_fingerprint() {
        let fixture = Fixture::new();
        fixture.write(
            "app/plugbay.toml",
            "[servers]\napi = { url = \"https://example.com/mcp\", headers = { X-Team = \"a\" } }\n",
        );

        let config = fixture.resolve("app");

        assert!(only(&config, "api").requires_trust_from.is_empty());
        assert_eq!(config.files[0].executable_fingerprint, None);
    }

    #[test]
    fn file_needing_trust_keeps_its_fingerprint() {
        let fixture = Fixture::new();
        fixture.write(
            "app/plugbay.toml",
            "[servers]\ntool = { command = \"tool-mcp\" }\n",
        );

        let config = fixture.resolve("app");

        assert!(config.files[0].executable_fingerprint.is_some());
    }

    #[test]
    fn tool_filter_without_globs_allows_everything() {
        assert!(ToolFilter::default().allows("anything"));
    }

    #[test]
    fn tool_filter_applies_include_then_exclude() {
        let filter = ToolFilter::new(
            vec!["list_*".to_string(), "get_issue".to_string()],
            vec!["list_secret*".to_string()],
        );
        assert!(filter.allows("list_issues"));
        assert!(filter.allows("get_issue"));
        assert!(!filter.allows("list_secrets"));
        assert!(!filter.allows("delete_issue"));
    }

    #[test]
    fn tool_filter_exclude_only() {
        let filter = ToolFilter::new(Vec::new(), vec!["delete_*".to_string()]);
        assert!(filter.allows("create_issue"));
        assert!(!filter.allows("delete_issue"));
    }

    #[test]
    fn tool_filter_equality_ignores_compiled_state() {
        let used = ToolFilter::new(vec!["a*".to_string()], Vec::new());
        assert!(used.allows("abc"));
        assert_eq!(used, ToolFilter::new(vec!["a*".to_string()], Vec::new()));
    }

    #[test]
    fn uses_stored_login_only_without_static_authorization_header() {
        let mut server = ResolvedServer::test_http("api", "default", "https://example.com/mcp");
        assert!(server.uses_stored_login());

        if let ServerTransport::Http { headers, .. } = &mut server.transport {
            headers.insert("authorization".to_string(), "Bearer ${env:X}".to_string());
        }
        assert!(!server.uses_stored_login());
        assert!(!ResolvedServer::test_stdio("tool", "default", "tool", &[]).uses_stored_login());
    }

    #[test]
    fn target_renders_url_or_quoted_command_line() {
        let remote = ResolvedServer::test_http("api", "default", "https://example.com/mcp");
        let local = ResolvedServer::test_stdio("tool", "default", "tool", &["serve", "my app"]);

        assert_eq!(remote.transport.target(), "https://example.com/mcp");
        assert_eq!(local.transport.target(), "tool serve \"my app\"");
    }

    #[test]
    fn display_url_masks_query_values_userinfo_and_token_segments() {
        let cases = [
            (
                "https://mcp.example.com/mcp?api_key=test-key-123&debug",
                "https://mcp.example.com/mcp?api_key=***&debug",
            ),
            (
                "https://user:test-password@mcp.example.com:8443/v1/mcp",
                "https://***@mcp.example.com:8443/v1/mcp",
            ),
            (
                "https://actions.example.com/mcp/sk-ak-0123456789abcdefghij/sse#frag",
                "https://actions.example.com/mcp/***/sse#***",
            ),
            (
                "http://[::1]:3000/servers/linear-workspace/mcp",
                "http://[::1]:3000/servers/linear-workspace/mcp",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                display_url(&Url::parse(input).unwrap()),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn url_identities_are_masked_for_logs_but_stdio_hashes_are_not() {
        let remote = ServerIdentity::for_url(
            &Url::parse("https://actions.example.com/mcp/sk-ak-0123456789abcdefghij").unwrap(),
        );
        assert_eq!(remote.masked(), "https://actions.example.com/mcp/***");
        let local = ServerIdentity::for_stdio("tool", &[]);
        assert_eq!(local.masked(), local.0);
    }

    #[test]
    fn identity_for_url_normalises() {
        let cases = [
            (
                "HTTPS://Mcp.Example.COM:443/mcp/?x=1#frag",
                "https://mcp.example.com/mcp",
            ),
            ("http://example.com:80", "http://example.com/"),
            (
                "https://example.com:8443/a/b/",
                "https://example.com:8443/a/b",
            ),
            ("https://example.com/", "https://example.com/"),
        ];
        for (input, expected) in cases {
            let url = Url::parse(input).unwrap();
            assert_eq!(ServerIdentity::for_url(&url).0, expected, "{input}");
        }
    }

    #[test]
    fn name_validation() {
        assert!(is_valid_name("client-a2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("Client"));
        assert!(!is_valid_name("my_server"));
    }
}
