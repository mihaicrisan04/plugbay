//! The official MCP Registry (registry.modelcontextprotocol.io): search it and look servers up.
//! Its answers are hostile input: text is made safe while it is parsed, and a remote url is only
//! used once `https_url` accepts it.

pub mod package;

use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer};
use url::Url;

use crate::config;
use crate::identity::one_safe_line;

const SERVERS_URL: &str = "https://registry.modelcontextprotocol.io/v0.1/servers";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// The largest page the registry serves.
const PAGE_SIZE: &str = "100";
const MAX_QUERY_CHARS: usize = 100;
const MAX_NAME_CHARS: usize = 200;
/// Long enough for any real url or description; longer text is cut (and a cut url stops parsing).
const MAX_TEXT_CHARS: usize = 2048;
const MAX_STATUS_CHARS: usize = 40;
const STREAMABLE_HTTP: &str = "streamable-http";

#[derive(Debug, Deserialize)]
pub struct RegistryServer {
    #[serde(deserialize_with = "safe_text")]
    pub name: String,
    #[serde(default, deserialize_with = "optional_safe_text")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "optional_safe_text")]
    pub description: Option<String>,
    #[serde(default)]
    pub remotes: Vec<Remote>,
    #[serde(default)]
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Remote {
    #[serde(rename = "type", deserialize_with = "safe_text")]
    pub transport: String,
    #[serde(deserialize_with = "safe_text")]
    pub url: String,
    #[serde(default)]
    pub headers: Vec<Header>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    #[serde(deserialize_with = "safe_text")]
    pub name: String,
    #[serde(default)]
    pub is_required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    #[serde(deserialize_with = "safe_text")]
    pub registry_type: String,
    #[serde(deserialize_with = "safe_text")]
    pub identifier: String,
    #[serde(default, deserialize_with = "optional_safe_text")]
    pub version: Option<String>,
    #[serde(default)]
    pub transport: PackageTransport,
    #[serde(default)]
    pub runtime_arguments: Vec<Argument>,
    #[serde(default)]
    pub package_arguments: Vec<Argument>,
    #[serde(default)]
    pub environment_variables: Vec<EnvironmentVariable>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PackageTransport {
    #[serde(rename = "type", default, deserialize_with = "optional_safe_text")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Argument {
    #[serde(rename = "type", deserialize_with = "safe_text")]
    pub kind: String,
    #[serde(default, deserialize_with = "optional_safe_text")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "optional_safe_text")]
    pub value: Option<String>,
    #[serde(rename = "default", default, deserialize_with = "optional_safe_text")]
    pub default_value: Option<String>,
    #[serde(default, deserialize_with = "optional_safe_text")]
    pub value_hint: Option<String>,
    #[serde(default)]
    pub is_required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentVariable {
    #[serde(deserialize_with = "safe_text")]
    pub name: String,
    #[serde(rename = "default", default, deserialize_with = "optional_safe_text")]
    pub default_value: Option<String>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub is_secret: bool,
}

/// What a registry server offers plugbay, decided once so `search` and `add` agree.
#[derive(Debug)]
pub enum Offer<'a> {
    /// Streamable HTTP remotes plugbay may connect to (`https_url` accepts them).
    Remotes(Vec<&'a Remote>),
    /// Streamable HTTP remotes only, and none plugbay may connect to: why the first one isn't.
    RefusedRemote(anyhow::Error),
    /// Packages to run locally, which plugbay only ever prints a command for.
    Packages(&'a [Package]),
    /// Remotes over the old SSE transport only.
    SseOnly,
    Nothing,
}

impl RegistryServer {
    pub fn offer(&self) -> Offer<'_> {
        let mut usable = Vec::new();
        let mut refusal = None;
        let streamable = self
            .remotes
            .iter()
            .filter(|remote| remote.transport == STREAMABLE_HTTP);
        for remote in streamable {
            match https_url(&remote.url) {
                Ok(_) => usable.push(remote),
                Err(error) => {
                    refusal.get_or_insert(error);
                }
            }
        }
        if !usable.is_empty() {
            return Offer::Remotes(usable);
        }
        if let Some(error) = refusal {
            return Offer::RefusedRemote(error);
        }
        if !self.packages.is_empty() {
            return Offer::Packages(&self.packages);
        }
        if !self.remotes.is_empty() {
            return Offer::SseOnly;
        }
        Offer::Nothing
    }
}

impl Remote {
    pub fn required_headers(&self) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|header| header.is_required)
            .map(|header| header.name.as_str())
            .collect()
    }
}

/// One element of the registry's `servers` list: the server and the registry's own metadata.
#[derive(Deserialize)]
struct Entry {
    server: RegistryServer,
    #[serde(rename = "_meta", default)]
    meta: EntryMeta,
}

#[derive(Default, Deserialize)]
struct EntryMeta {
    #[serde(rename = "io.modelcontextprotocol.registry/official")]
    official: Option<Official>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Official {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    is_latest: bool,
}

impl Entry {
    /// Entries without registry metadata count as current: the request already asked for
    /// the latest versions, this only guards against a registry that ignores that.
    fn is_latest_active(&self) -> bool {
        match &self.meta.official {
            Some(official) => official.is_latest && is_active(official.status.as_deref()),
            None => true,
        }
    }
}

fn is_active(status: Option<&str>) -> bool {
    status.is_none_or(|status| status == "active")
}

pub struct SearchResults {
    /// Latest active servers, the best matches first.
    pub servers: Vec<RegistryServer>,
    /// The registry has matches beyond the one page plugbay reads.
    pub more_in_registry: bool,
}

/// One page of the registry's `servers` listing.
struct Page {
    entries: Vec<Entry>,
    has_more: bool,
}

pub async fn search(query: &str) -> anyhow::Result<SearchResults> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        bail!("give a search of 1 to {MAX_QUERY_CHARS} characters");
    }
    let mut url = Url::parse(SERVERS_URL).context("parsing the MCP Registry url")?;
    url.query_pairs_mut()
        .append_pair("search", query)
        .append_pair("version", "latest")
        .append_pair("limit", PAGE_SIZE);
    let Some(body) = fetch(url).await? else {
        bail!("the MCP Registry has no server list at {SERVERS_URL}");
    };
    let page = parse_listing(&body)?;
    let mut servers = latest_active(page.entries);
    rank(&mut servers, query);
    Ok(SearchResults {
        servers,
        more_in_registry: page.has_more,
    })
}

/// The latest version of the server named `name`, e.g. `app.linear/linear`.
pub async fn lookup(name: &str) -> anyhow::Result<RegistryServer> {
    if !is_registry_name(name) {
        bail!(
            "`{}` is not an MCP Registry name; they look like `app.linear/linear` (`plugbay search` lists them)",
            one_safe_line(name, MAX_NAME_CHARS)
        );
    }
    let mut url = Url::parse(SERVERS_URL).context("parsing the MCP Registry url")?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("the MCP Registry url has no path"))?
        .push(name)
        .push("versions")
        .push("latest");
    let Some(body) = fetch(url).await? else {
        bail!("the MCP Registry has no server named {name}; `plugbay search <words>` finds names");
    };
    parse_lookup(&body, name)
}

fn parse_lookup(body: &[u8], name: &str) -> anyhow::Result<RegistryServer> {
    let entry: Entry = serde_json::from_slice(body).context("reading the MCP Registry's answer")?;
    if entry.server.name != name {
        bail!("the MCP Registry answered with another server than {name}");
    }
    if let Some(official) = &entry.meta.official
        && let Some(status) = official.status.as_deref()
        && !is_active(Some(status))
    {
        bail!(
            "{name} is marked `{}` in the MCP Registry, so plugbay won't add it",
            one_safe_line(status, MAX_STATUS_CHARS)
        );
    }
    Ok(entry.server)
}

/// The body of a successful GET, `None` for 404. Redirects are not followed and the body is
/// capped, so a misbehaving registry can't send plugbay elsewhere or fill memory.
async fn fetch(url: Url) -> anyhow::Result<Option<Vec<u8>>> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("plugbay/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the HTTP client")?;
    tracing::debug!(%url, "asking the MCP Registry");
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("asking the MCP Registry (registry.modelcontextprotocol.io)")?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("the MCP Registry answered {status}; try again later");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("reading the MCP Registry's answer")?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            bail!(
                "the MCP Registry's answer is larger than the {MAX_RESPONSE_BYTES} bytes plugbay reads"
            );
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Every well-formed entry in a `servers` listing; malformed ones are skipped, not fatal.
fn parse_listing(body: &[u8]) -> anyhow::Result<Page> {
    #[derive(Deserialize)]
    struct Listing {
        #[serde(default)]
        servers: Vec<serde_json::Value>,
        #[serde(default)]
        metadata: ListingMetadata,
    }
    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ListingMetadata {
        next_cursor: Option<String>,
    }
    let listing: Listing =
        serde_json::from_slice(body).context("reading the MCP Registry's answer")?;
    let mut entries = Vec::new();
    for value in listing.servers {
        match serde_json::from_value::<Entry>(value) {
            Ok(entry) if is_registry_name(&entry.server.name) => entries.push(entry),
            Ok(entry) => {
                tracing::debug!(name = %entry.server.name, "skipping a badly named registry entry")
            }
            Err(error) => tracing::debug!(%error, "skipping a malformed registry entry"),
        }
    }
    Ok(Page {
        entries,
        has_more: listing.metadata.next_cursor.is_some(),
    })
}

/// One server per name, latest active version only.
fn latest_active(entries: Vec<Entry>) -> Vec<RegistryServer> {
    let mut servers: Vec<RegistryServer> = Vec::new();
    for entry in entries {
        let seen = servers
            .iter()
            .any(|server| server.name == entry.server.name);
        if entry.is_latest_active() && !seen {
            servers.push(entry.server);
        }
    }
    servers
}

/// Servers whose namespace belongs to the query (`app.linear` for "linear") first, then vendor
/// namespaces before open ones anyone can publish under, then an exact server name.
fn rank(servers: &mut [RegistryServer], query: &str) {
    let query = query.to_lowercase();
    servers.sort_by_cached_key(|server| {
        let name = server.name.to_lowercase();
        let (namespace, server_name) = name.split_once('/').unwrap_or((&name, ""));
        (
            !namespace_belongs_to(namespace, &query),
            is_open_namespace(namespace),
            server_name != query,
            name.clone(),
        )
    });
}

/// Whether the query names who publishes under a namespace: `linear` owns `app.linear`,
/// `acme` owns `io.github.acme`.
fn namespace_belongs_to(namespace: &str, query: &str) -> bool {
    match namespace.strip_prefix("io.github.") {
        Some(account) => account == query,
        None => namespace.split('.').skip(1).any(|label| label == query),
    }
}

/// Namespaces anyone can publish under (a GitHub account, a hosting service), unlike a vendor's
/// own reverse-DNS domain, which the registry verifies.
fn is_open_namespace(namespace: &str) -> bool {
    namespace.starts_with("io.github.") || namespace == "ai.smithery"
}

/// `namespace/server`, as the registry allows them: reverse DNS (or `io.github.<user>`), a slash,
/// then letters, digits, `.`, `_` and `-`.
pub fn is_registry_name(name: &str) -> bool {
    let Some((namespace, server)) = name.split_once('/') else {
        return false;
    };
    let namespace_is_valid = namespace
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'));
    let server_is_valid = server
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'));
    name.len() <= MAX_NAME_CHARS
        && namespace.contains('.')
        && !server.is_empty()
        && namespace_is_valid
        && server_is_valid
}

/// A registry remote's url, if plugbay may connect to it: https, a real host, no userinfo and no
/// `{placeholders}` left to fill in.
pub fn https_url(text: &str) -> anyhow::Result<Url> {
    if text.contains('{') {
        bail!("its url `{text}` is a template with values to fill in");
    }
    let url = Url::parse(text).with_context(|| format!("its url `{text}` is not a valid url"))?;
    if url.scheme() != "https" {
        bail!("its url `{text}` is not https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("its url holds a user name or password");
    }
    if url.host().is_none() || config::is_loopback(&url) {
        bail!("its url `{text}` points at this machine");
    }
    Ok(url)
}

fn safe_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    Ok(one_safe_line(&text, MAX_TEXT_CHARS))
}

fn optional_safe_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let text = Option::<String>::deserialize(deserializer)?;
    Ok(text
        .map(|text| one_safe_line(&text, MAX_TEXT_CHARS))
        .filter(|text| !text.is_empty()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SEARCH_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/registry/search.json"
    ));

    pub(crate) fn fixture_servers() -> Vec<RegistryServer> {
        latest_active(parse_listing(SEARCH_FIXTURE.as_bytes()).unwrap().entries)
    }

    pub(crate) fn fixture_search(query: &str) -> Vec<RegistryServer> {
        let mut servers = fixture_servers();
        rank(&mut servers, query);
        servers
    }

    pub(crate) fn fixture_server(name: &str) -> RegistryServer {
        fixture_servers()
            .into_iter()
            .find(|server| server.name == name)
            .unwrap()
    }

    fn names(servers: &[RegistryServer]) -> Vec<&str> {
        servers.iter().map(|server| server.name.as_str()).collect()
    }

    #[test]
    fn listing_keeps_one_latest_active_entry_per_server() {
        let servers = fixture_servers();
        assert_eq!(
            names(&servers),
            [
                "io.github.acme-dev/tracker-mcp",
                "app.linear/linear",
                "com.acme/tracker",
                "dev.oldco/legacy",
                "dev.tricky/tracker",
                "org.pyco/finder",
                "net.mixed/tracker",
            ]
        );
        let linear = fixture_server("app.linear/linear");
        assert_eq!(linear.remotes.len(), 2, "the latest version wins");
    }

    #[test]
    fn a_next_cursor_means_more_matches() {
        let page = parse_listing(br#"{"servers": [], "metadata": {"nextCursor": "x:1.0.0"}}"#);
        assert!(page.unwrap().has_more);
        assert!(!parse_listing(SEARCH_FIXTURE.as_bytes()).unwrap().has_more);
    }

    #[test]
    fn a_listing_that_is_not_json_is_an_error() {
        assert!(parse_listing(b"<html>maintenance</html>").is_err());
    }

    #[test]
    fn displayed_text_loses_control_and_direction_characters() {
        let tricky = fixture_server("dev.tricky/tracker");
        assert_eq!(tricky.title.as_deref(), Some("Tricky rekcart"));
        assert_eq!(
            tricky.description.as_deref(),
            Some("Line one line two with hidden marks.")
        );
    }

    #[test]
    fn the_namespace_owner_ranks_first_then_vendors_before_open_namespaces() {
        let mut servers = fixture_servers();
        rank(&mut servers, "tracker");
        assert_eq!(
            &names(&servers)[..3],
            [
                "com.acme/tracker",
                "dev.tricky/tracker",
                "net.mixed/tracker"
            ]
        );
        assert_eq!(
            names(&servers).last(),
            Some(&"io.github.acme-dev/tracker-mcp")
        );

        rank(&mut servers, "Acme");
        assert_eq!(names(&servers)[0], "com.acme/tracker");
        rank(&mut servers, "acme-dev");
        assert_eq!(names(&servers)[0], "io.github.acme-dev/tracker-mcp");
    }

    #[test]
    fn lookup_answers_must_name_the_server_asked_for_and_be_active() {
        let linear = r#"{"server": {"name": "app.linear/linear"}, "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}}}"#;
        assert!(parse_lookup(linear.as_bytes(), "app.linear/linear").is_ok());
        assert!(parse_lookup(linear.as_bytes(), "com.acme/tracker").is_err());

        let deprecated = linear.replace("active", "deprecated");
        let error = parse_lookup(deprecated.as_bytes(), "app.linear/linear").unwrap_err();
        assert!(error.to_string().contains("deprecated"), "{error}");
    }

    #[test]
    fn a_hostile_status_is_refused_and_shown_as_one_short_line() {
        let status = format!("active\\n\\u001b[2Jdeprecated{}", "x".repeat(500));
        let entry = format!(
            r#"{{"server": {{"name": "app.linear/linear"}}, "_meta": {{"io.modelcontextprotocol.registry/official": {{"status": "{status}", "isLatest": true}}}}}}"#
        );
        let error = parse_lookup(entry.as_bytes(), "app.linear/linear")
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains('\n') && !error.contains('\u{1b}'),
            "{error}"
        );
        assert!(error.len() < 200, "{error}");
    }

    fn offer_of(name: &str) -> String {
        match fixture_server(name).offer() {
            Offer::Remotes(remotes) => {
                let urls: Vec<&str> = remotes.iter().map(|remote| remote.url.as_str()).collect();
                format!("remotes {}", urls.join(" "))
            }
            Offer::RefusedRemote(error) => format!("refused: {error:#}"),
            Offer::Packages(packages) => format!("{} packages", packages.len()),
            Offer::SseOnly => "sse only".to_string(),
            Offer::Nothing => "nothing".to_string(),
        }
    }

    #[test]
    fn offers_keep_only_usable_streamable_remotes() {
        assert_eq!(
            offer_of("com.acme/tracker"),
            "remotes https://mcp.acme.example/mcp"
        );
        assert_eq!(
            offer_of("net.mixed/tracker"),
            "remotes https://mcp.mixed.example/mcp"
        );
        assert_eq!(
            offer_of("dev.tricky/tracker"),
            "refused: its url `http://tricky.example/mcp` is not https"
        );
        assert_eq!(offer_of("org.pyco/finder"), "3 packages");
        assert_eq!(offer_of("dev.oldco/legacy"), "sse only");
    }

    #[test]
    fn registry_names_are_namespace_slash_server() {
        assert!(is_registry_name("app.linear/linear"));
        assert!(is_registry_name("io.github.acme-dev/tracker_mcp.v2"));
        assert!(!is_registry_name("linear"));
        assert!(!is_registry_name("linear/linear"));
        assert!(!is_registry_name("app.linear/"));
        assert!(!is_registry_name("app.linear/lin ear"));
        assert!(!is_registry_name("app.linear/a/b"));
    }

    #[test]
    fn only_plain_https_urls_to_other_machines_are_accepted() {
        assert!(https_url("https://mcp.linear.app/mcp").is_ok());
        assert!(https_url("http://mcp.linear.app/mcp").is_err());
        assert!(https_url("https://{tenant}.example.com/mcp").is_err());
        assert!(https_url("https://mcp.example.com/{tenant}/mcp").is_err());
        assert!(https_url("https://user:secret@mcp.example.com/mcp").is_err());
        assert!(https_url("https://localhost/mcp").is_err());
        assert!(https_url("https://127.0.0.1/mcp").is_err());
        assert!(https_url("https://[::1]/mcp").is_err());
        assert!(https_url("https://localhost./mcp").is_err());
        assert!(https_url("https://[::ffff:127.0.0.1]/mcp").is_err());
        assert!(https_url("https://0.0.0.0/mcp").is_err());
        assert!(https_url("file:///etc/passwd").is_err());
    }
}
