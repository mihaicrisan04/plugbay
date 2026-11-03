//! Secret references inside config values: `${op://vault/item/field}`, `${env:NAME}`,
//! `${keychain:service/account}`, `${fnox:NAME}`. Config files hold refs, never values.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use url::Url;

/// Keychain service that `store_in_keychain` (and `plugbay import`) writes under.
pub const KEYCHAIN_SERVICE: &str = "plugbay";

/// `op` may wait for a biometric unlock, so this is generous; it only guards against a hung CLI.
const SECRET_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// One parsed `${...}` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// The full `op://vault/item/field` URI, passed to `op read` as is.
    OnePassword(String),
    Keychain {
        service: String,
        account: String,
    },
    Env(String),
    Fnox(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Reference(SecretRef),
}

/// True when `value` contains `${`. Deliberately loose: a malformed reference still
/// counts, so trust fingerprints cover it and resolving reports the mistake.
pub fn contains_secret_ref(value: &str) -> bool {
    value.contains("${")
}

/// Every reference in `value`, in order. Fails on unknown schemes and unterminated `${`.
pub fn parse_refs(value: &str) -> anyhow::Result<Vec<SecretRef>> {
    let mut references = Vec::new();
    for segment in parse(value)? {
        if let Segment::Reference(reference) = segment {
            references.push(reference);
        }
    }
    Ok(references)
}

/// Replaces every `${...}` reference in `value` with the secret it points to.
/// Literal text around references is kept (`"Bearer ${op://...}"`).
pub async fn resolve(value: &str) -> anyhow::Result<String> {
    let mut resolved = String::new();
    for segment in parse(value)? {
        match segment {
            Segment::Literal(text) => resolved.push_str(&text),
            Segment::Reference(reference) => {
                let secret = fetch(&reference)
                    .await
                    .with_context(|| format!("resolving ${{{}}}", reference.describe()))?;
                resolved.push_str(&secret);
            }
        }
    }
    Ok(resolved)
}

/// `value` with every `${...}` reference swapped for what `replace` returns, resolving nothing.
/// Fails like [`resolve`] on unknown schemes and unterminated `${`.
pub fn replace_refs(
    value: &str,
    mut replace: impl FnMut(&SecretRef) -> String,
) -> anyhow::Result<String> {
    let mut replaced = String::new();
    for segment in parse(value)? {
        match segment {
            Segment::Literal(text) => replaced.push_str(&text),
            Segment::Reference(reference) => replaced.push_str(&replace(&reference)),
        }
    }
    Ok(replaced)
}

pub async fn resolve_map(
    values: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for (name, value) in values {
        let secret = resolve(value)
            .await
            .with_context(|| format!("resolving `{name}`"))?;
        resolved.insert(name.clone(), secret);
    }
    Ok(resolved)
}

/// Stores `secret` in the OS keychain under plugbay's service and returns the `${keychain:...}` ref to it.
pub fn store_in_keychain(name: &str, secret: &str) -> anyhow::Result<String> {
    if name.is_empty() || name.contains('}') {
        bail!("invalid keychain entry name `{name}`");
    }
    crate::vault::write(KEYCHAIN_SERVICE, name, secret)?;
    Ok(format!("${{keychain:{KEYCHAIN_SERVICE}/{name}}}"))
}

/// True for names that conventionally hold secrets (`*TOKEN*`, `*KEY*`, `*SECRET*`, `*PASSWORD*`, `Authorization`, ...).
pub fn looks_secret(name: &str) -> bool {
    const SECRET_WORDS: [&str; 8] = [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
    ];
    let name = name.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|word| name.contains(word))
}

/// An env var or header whose value is a secret by its name, `Authorization` included.
pub fn is_secret_field(field: &str) -> bool {
    field.eq_ignore_ascii_case("authorization") || looks_secret(field)
}

/// `"Bearer abc"` → `("Bearer ", "abc")`, so the keychain holds the bare token.
pub fn split_bearer_scheme(value: &str) -> (&str, &str) {
    let scheme_length = "Bearer ".len();
    match value.get(..scheme_length) {
        Some(scheme) if scheme.eq_ignore_ascii_case("Bearer ") => value.split_at(scheme_length),
        _ => ("", value),
    }
}

/// True when an arg looks like it carries a secret inline: `--token=abc`, `Authorization:Bearer abc`,
/// or a secret-named flag followed by a value. Args using `${...}` expansion are fine.
pub fn has_secret_in_args(args: &[String]) -> bool {
    for (index, argument) in args.iter().enumerate() {
        if let Some((name, value)) = argument.split_once(['=', ':'])
            && looks_secret(name)
            && !value.is_empty()
            && !value.contains("${")
        {
            return true;
        }
        let is_secret_flag = argument.starts_with('-') && looks_secret(argument);
        if let Some(next) = args.get(index + 1)
            && is_secret_flag
            && !argument.contains('=')
            && !next.starts_with('-')
            && !next.contains("${")
        {
            return true;
        }
    }
    false
}

/// True when a URL in `text`, of any scheme, may carry a credential: userinfo, a query value or a
/// token-looking path segment, i.e. what [`crate::config::display_url`] masks.
pub fn has_secret_url(text: &str) -> bool {
    for candidate in url_candidates(text) {
        let after_scheme = &text[candidate.after_scheme..];
        if has_userinfo(after_scheme) {
            return true;
        }
        let may_hold_secret = match Url::parse(candidate.text) {
            Ok(url) => crate::config::url_may_hold_secret(&url),
            // `https://token@` has no host and `https://user:pass word@host` stops at the space:
            // neither parses, and any `@` after them may end a credential.
            Err(_) => after_scheme.contains('@'),
        };
        if may_hold_secret {
            return true;
        }
    }
    false
}

/// True when a `${...}` ref sits inside a `scheme://...` URL in `value`: the text around it may be
/// half a credential (`https://${env:USER}:password@host`). Fails on malformed refs.
pub fn has_ref_in_url(value: &str) -> anyhow::Result<bool> {
    const MARKER: &str = "\u{1}";
    let marked = replace_refs(value, |_| MARKER.to_string())?;
    let found = url_candidates(&marked)
        .iter()
        .any(|candidate| candidate.text.contains(MARKER));
    Ok(found)
}

/// One `scheme://...` run in a text, up to the next whitespace or quote.
struct UrlCandidate<'a> {
    text: &'a str,
    /// Where the text after `://` starts in the searched text.
    after_scheme: usize,
}

fn url_candidates(text: &str) -> Vec<UrlCandidate<'_>> {
    let is_scheme_character =
        |character: char| character.is_ascii_alphanumeric() || "+-.".contains(character);
    let mut candidates = Vec::new();
    let mut searched = 0;
    while let Some(offset) = text[searched..].find("://") {
        let separator = searched + offset;
        let scheme_start = text[..separator]
            .rfind(|character: char| !is_scheme_character(character))
            .map_or(0, |index| index + 1);
        let end = text[separator..]
            .find(|character: char| character.is_whitespace() || "\"'".contains(character))
            .map_or(text.len(), |index| separator + index);
        let scheme = &text[scheme_start..separator];
        if scheme.starts_with(|character: char| character.is_ascii_alphabetic()) {
            candidates.push(UrlCandidate {
                text: &text[scheme_start..end],
                after_scheme: separator + "://".len(),
            });
        }
        searched = separator + "://".len();
    }
    candidates
}

/// `user:pass@` right after `scheme://`. Read up to the `@` rather than the first space or quote,
/// which a password may hold; only the path, query or fragment ends the authority.
fn has_userinfo(after_scheme: &str) -> bool {
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    after_scheme[..authority_end]
        .find('@')
        .is_some_and(|at| at > 0)
}

/// Well-known token prefixes: GitHub, GitLab, OpenAI and Anthropic (`sk-`), Slack, npm, AWS keys.
const TOKEN_PREFIXES: [&str; 16] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "xapp-",
    "npm_",
    "AKIA",
];

/// How much must follow a token prefix, so a word like `sk-learn` stays a word.
const MIN_TOKEN_BODY: usize = 8;

/// A run of base64 or hex this long, with letters and digits, reads as a key (UUIDs included).
const MIN_OPAQUE_RUN: usize = 32;

/// True when `text` holds a literal shaped like a credential, whatever field it sits in: a
/// well-known token prefix (`ghp_`, `sk-`, `xoxb-`...), a JWT, or a long run of base64 or hex.
/// A heuristic for `export`, deliberately eager: a false positive only costs a placeholder.
pub fn has_token_literal(text: &str) -> bool {
    if words(text, "_-.").any(looks_like_token) {
        return true;
    }
    text.split_whitespace()
        .filter(|word| !is_path_url_or_digest(word))
        .flat_map(|word| words(word, "+/=_-"))
        .any(is_opaque_run)
}

/// A single word that starts with a well-known token prefix or is a JWT (`eyJ...`, three dotted parts).
pub fn looks_like_token(word: &str) -> bool {
    let has_token_prefix = TOKEN_PREFIXES.iter().any(|prefix| {
        word.strip_prefix(prefix)
            .is_some_and(|body| body.len() >= MIN_TOKEN_BODY)
    });
    let parts: Vec<&str> = word.split('.').collect();
    let is_jwt =
        parts.len() == 3 && word.starts_with("eyJ") && parts.iter().all(|part| !part.is_empty());
    has_token_prefix || is_jwt
}

/// Runs of ASCII letters, digits and `extra` characters in `text`.
fn words<'a>(text: &'a str, extra: &'a str) -> impl Iterator<Item = &'a str> {
    text.split(move |character: char| {
        !character.is_ascii_alphanumeric() && !extra.contains(character)
    })
    .filter(|word| !word.is_empty())
}

/// Paths and urls are long runs too, and an image digest (`sha256:...`) is public.
fn is_path_url_or_digest(word: &str) -> bool {
    word.contains("://")
        || word.starts_with(['/', '~', '.'])
        || word.contains("sha256:")
        || word.contains("sha512:")
}

fn is_opaque_run(run: &str) -> bool {
    run.len() >= MIN_OPAQUE_RUN
        && run.chars().any(|character| character.is_ascii_digit())
        && run.chars().any(|character| character.is_ascii_alphabetic())
}

/// A log-safe stand-in for a resolved secret value. Keeps a leading auth scheme
/// (`Bearer [redacted]`) because it helps debugging and reveals nothing.
pub fn redact(value: &str) -> String {
    const AUTH_SCHEMES: [&str; 3] = ["bearer", "basic", "token"];
    if value.is_empty() {
        return String::new();
    }
    if let Some((scheme, _)) = value.split_once(' ')
        && AUTH_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
    {
        return format!("{scheme} [redacted]");
    }
    "[redacted]".to_string()
}

impl SecretRef {
    /// The reference as written inside `${...}`. Refs are pointers, never secrets, so this is safe to show.
    pub fn describe(&self) -> String {
        match self {
            SecretRef::OnePassword(uri) => uri.clone(),
            SecretRef::Keychain { service, account } => format!("keychain:{service}/{account}"),
            SecretRef::Env(name) => format!("env:{name}"),
            SecretRef::Fnox(name) => format!("fnox:{name}"),
        }
    }
}

fn parse(value: &str) -> anyhow::Result<Vec<Segment>> {
    let mut segments = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            segments.push(Segment::Literal(rest[..start].to_string()));
        }
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find('}') else {
            bail!("unterminated `${{` in `{value}`: expected a closing `}}`");
        };
        segments.push(Segment::Reference(parse_reference(&after_open[..end])?));
        rest = &after_open[end + 1..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Literal(rest.to_string()));
    }
    Ok(segments)
}

fn parse_reference(reference: &str) -> anyhow::Result<SecretRef> {
    if reference.starts_with("op://") {
        if reference.len() == "op://".len() {
            bail!("empty 1Password reference `${{op://}}`");
        }
        return Ok(SecretRef::OnePassword(reference.to_string()));
    }
    if let Some(path) = reference.strip_prefix("keychain:") {
        let Some((service, account)) = path.split_once('/') else {
            bail!("keychain reference `{reference}` must look like `keychain:service/account`");
        };
        if service.is_empty() || account.is_empty() {
            bail!("keychain reference `{reference}` must look like `keychain:service/account`");
        }
        return Ok(SecretRef::Keychain {
            service: service.to_string(),
            account: account.to_string(),
        });
    }
    if let Some(name) = reference.strip_prefix("env:") {
        if name.is_empty() {
            bail!("empty environment variable name in `${{env:}}`");
        }
        return Ok(SecretRef::Env(name.to_string()));
    }
    if let Some(name) = reference.strip_prefix("fnox:") {
        if !is_valid_fnox_name(name) {
            bail!(
                "invalid fnox secret name `{name}`: use letters, digits, `_`, `-` and `.`, not starting with `-`"
            );
        }
        return Ok(SecretRef::Fnox(name.to_string()));
    }
    bail!(
        "unknown secret reference `${{{reference}}}`: expected `op://`, `keychain:`, `env:` or `fnox:`"
    )
}

/// The name becomes a CLI argument, so it must not look like a flag.
fn is_valid_fnox_name(name: &str) -> bool {
    let allowed = |character: char| character.is_ascii_alphanumeric() || "_-.".contains(character);
    !name.is_empty() && !name.starts_with('-') && name.chars().all(allowed)
}

async fn fetch(reference: &SecretRef) -> anyhow::Result<String> {
    match reference {
        SecretRef::OnePassword(uri) => {
            run_secret_command("op", &["read", uri], "the 1Password CLI `op`").await
        }
        SecretRef::Fnox(name) => run_secret_command("fnox", &["get", name], "`fnox`").await,
        SecretRef::Env(name) => {
            std::env::var(name).with_context(|| format!("environment variable `{name}` is not set"))
        }
        SecretRef::Keychain { service, account } => {
            let lookup = tokio::task::spawn_blocking({
                let service = service.clone();
                let account = account.clone();
                move || {
                    if crate::vault::owns(&service) {
                        crate::vault::read(&service, &account)
                    } else {
                        keychain::read(&service, &account)
                    }
                }
            });
            let secret = lookup.await.context("keychain lookup task failed")??;
            secret.with_context(|| {
                format!("no keychain entry for service `{service}`, account `{account}`")
            })
        }
    }
}

/// Runs a secret manager CLI without a shell and returns its stdout minus the trailing newline.
async fn run_secret_command(
    program: &str,
    args: &[&str],
    tool_description: &str,
) -> anyhow::Result<String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{tool_description} is not installed or not on PATH")
        }
        Err(error) => return Err(error).with_context(|| format!("starting {tool_description}")),
    };

    let output = tokio::time::timeout(SECRET_COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .with_context(|| {
            format!("{tool_description} did not answer within {SECRET_COMMAND_TIMEOUT:?}")
        })?
        .with_context(|| format!("running {tool_description}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{tool_description} failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{tool_description} printed a non-UTF-8 secret"))?;
    Ok(strip_trailing_newline(stdout))
}

fn strip_trailing_newline(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

/// Blocking OS keychain access. From async code, call these inside `spawn_blocking`.
/// Errors are formatted with `Display` only: keyring's `Debug` can include secret bytes.
pub(crate) mod keychain {
    use anyhow::anyhow;

    /// Without a keychain (Linux with no Secret Service running: CI, containers, servers) there
    /// is nothing stored, so reads find nothing and only writes fail.
    const NO_KEYCHAIN_HINT: &str = "no keychain is available; on Linux, start GNOME Keyring or KWallet (a Secret Service provider)";

    pub fn read(service: &str, account: &str) -> anyhow::Result<Option<String>> {
        let entry = match keyring::Entry::new(service, account) {
            Ok(entry) => entry,
            Err(keyring::Error::NoDefaultStore) => return Ok(None),
            Err(error) => return Err(open_error(service, account, error)),
        };
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry | keyring::Error::NoDefaultStore) => Ok(None),
            Err(error) => Err(anyhow!(
                "reading keychain entry {service}/{account}: {error}"
            )),
        }
    }

    pub fn write(service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        let entry = match keyring::Entry::new(service, account) {
            Ok(entry) => entry,
            Err(keyring::Error::NoDefaultStore) => return Err(anyhow!(NO_KEYCHAIN_HINT)),
            Err(error) => return Err(open_error(service, account, error)),
        };
        entry
            .set_password(secret)
            .map_err(|error| anyhow!("writing keychain entry {service}/{account}: {error}"))
    }

    /// Returns false when there was nothing to delete.
    pub fn delete(service: &str, account: &str) -> anyhow::Result<bool> {
        let entry = match keyring::Entry::new(service, account) {
            Ok(entry) => entry,
            Err(keyring::Error::NoDefaultStore) => return Ok(false),
            Err(error) => return Err(open_error(service, account, error)),
        };
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry | keyring::Error::NoDefaultStore) => Ok(false),
            Err(error) => Err(anyhow!(
                "deleting keychain entry {service}/{account}: {error}"
            )),
        }
    }

    fn open_error(service: &str, account: &str, error: keyring::Error) -> anyhow::Error {
        anyhow!("opening keychain entry {service}/{account}: {error}")
    }
}

/// A fake JWT for tests, split so secret scanners don't flag the source.
#[cfg(test)]
pub const FAKE_JWT: &str = concat!(
    "ey",
    "JhbGciOiJIUzI1NiJ9.",
    "ey",
    "JzdWIiOiJmYWtlIn0.fakeSignature"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_urls_of_any_scheme_are_detected() {
        assert!(has_secret_url("postgres://fake-user:fake-pass@db/app"));
        assert!(has_secret_url(
            "--url=https://fake-user@mcp.example.com/mcp"
        ));
        assert!(has_secret_url("https://mcp.example.com/mcp?api_key=fake"));
        assert!(has_secret_url("https://fake-token@"));
        assert!(has_secret_url("postgres://fake-user: @db/app"));
    }

    #[test]
    fn userinfo_with_spaces_quotes_or_slashes_still_counts() {
        assert!(has_secret_url(
            "postgres://fake-user:fake password@host/app"
        ));
        assert!(has_secret_url("https://fake-user:\"fake pass\"@host"));
        assert!(has_secret_url("postgres://fake-user:fake/pass@host/app"));
        assert!(has_secret_url("https://fake-user:fake pass/word@host/app"));
    }

    #[test]
    fn refs_inside_urls_are_found() {
        assert!(has_ref_in_url("https://${env:USER}:fake-url-password@host/app").unwrap());
        assert!(has_ref_in_url("https://fake-user:fake${env:SUFFIX}@host/app").unwrap());
        assert!(has_ref_in_url("https://mcp.example.com/${env:PATH_PART}").unwrap());
        assert!(!has_ref_in_url("Bearer ${env:TOKEN}").unwrap());
        assert!(!has_ref_in_url("${env:URL}").unwrap());
        assert!(has_ref_in_url("https://host/${nope:x}").is_err());
    }

    #[test]
    fn token_shaped_literals_are_detected() {
        let tokens = [
            "ghp_FAKE123456789012345678901234567890123456",
            "github_pat_FAKE1234567890",
            "sk-FAKE123456789012345678901234",
            "sk-ant-fake-test-token",
            "xoxb-fake-test-token",
            "Bearer xoxp-fake-test-token",
            "token=ghp_FAKE1234567890",
            FAKE_JWT,
            "0123456789abcdef0123456789abcdef",
            "ZmFrZXNlY3JldA==ZmFrZXNlY3JldA==ZmFrZXNlY3JldA==",
            "123e4567-e89b-12d3-a456-426614174000",
        ];
        for token in tokens {
            assert!(has_token_literal(token), "{token}");
        }
    }

    #[test]
    fn ordinary_values_are_not_tokens() {
        let values = [
            "debug",
            "sk-learn",
            "risk-assessment",
            "us-east-1",
            "https://api.example.com/v1/organizations/1234567/projects",
            "/Users/fake/dev/projects/plugbay-workspace-2026/config",
            "ghcr.io/acme/tool@sha256:0123456789abcdef0123456789abcdef0123456789abcdef",
            "version 1.2.3",
            "",
        ];
        for value in values {
            assert!(!has_token_literal(value), "{value}");
        }
    }

    #[test]
    fn plain_urls_and_text_are_not_secrets() {
        assert!(!has_secret_url("https://mcp.example.com/mcp"));
        assert!(!has_secret_url("postgres://db.internal:5432/app"));
        assert!(!has_secret_url("debug"));
    }

    #[test]
    fn inline_secret_args_are_detected() {
        let args = ["--api-key".to_string(), "test-key".to_string()];
        assert!(has_secret_in_args(&args));
        assert!(has_secret_in_args(&["--token=test-token".to_string()]));
        assert!(has_secret_in_args(&[
            "Authorization:Bearer test-token".to_string()
        ]));
    }

    #[test]
    fn args_with_expansion_or_plain_names_are_not_secrets() {
        let args = [
            "--header".to_string(),
            "Authorization:Bearer ${INTRANET_TOKEN}".to_string(),
            "keycloak-mcp".to_string(),
            "https://example.test/mcp".to_string(),
        ];
        assert!(!has_secret_in_args(&args));
    }

    fn literal(text: &str) -> Segment {
        Segment::Literal(text.to_string())
    }

    #[test]
    fn literal_text_around_a_reference_is_kept() {
        let segments = parse("Bearer ${op://Clients/posthog/key} tail").unwrap();
        assert_eq!(
            segments,
            vec![
                literal("Bearer "),
                Segment::Reference(SecretRef::OnePassword(
                    "op://Clients/posthog/key".to_string()
                )),
                literal(" tail"),
            ]
        );
    }

    #[test]
    fn value_without_references_is_one_literal() {
        assert_eq!(
            parse("plain $value").unwrap(),
            vec![literal("plain $value")]
        );
    }

    #[test]
    fn every_scheme_parses() {
        let references =
            parse_refs("${keychain:plugbay/linear}${env:HOME}${fnox:SENTRY_TOKEN}").unwrap();
        assert_eq!(
            references,
            vec![
                SecretRef::Keychain {
                    service: "plugbay".to_string(),
                    account: "linear".to_string()
                },
                SecretRef::Env("HOME".to_string()),
                SecretRef::Fnox("SENTRY_TOKEN".to_string()),
            ]
        );
    }

    #[test]
    fn unknown_scheme_is_an_error() {
        let error = parse("${cmd:cat /etc/passwd}").unwrap_err();
        assert!(error.to_string().contains("unknown secret reference"));
    }

    #[test]
    fn unterminated_reference_is_an_error() {
        let error = parse("Bearer ${op://vault/item").unwrap_err();
        assert!(error.to_string().contains("unterminated"));
    }

    #[test]
    fn keychain_reference_needs_service_and_account() {
        assert!(parse("${keychain:only-service}").is_err());
        assert!(parse("${keychain:/account}").is_err());
    }

    #[test]
    fn fnox_name_that_looks_like_a_flag_is_rejected() {
        assert!(parse("${fnox:--help}").is_err());
    }

    #[test]
    fn contains_secret_ref_counts_malformed_references() {
        assert!(contains_secret_ref("Bearer ${op://vault"));
        assert!(!contains_secret_ref("https://example.com/$path"));
    }

    #[tokio::test]
    async fn env_reference_resolves_with_surrounding_text() {
        let path = std::env::var("PATH").unwrap();
        let resolved = resolve("before ${env:PATH} after").await.unwrap();
        assert_eq!(resolved, format!("before {path} after"));
    }

    #[tokio::test]
    async fn missing_env_variable_is_an_error() {
        let error = resolve("${env:PLUGBAY_TEST_SURELY_UNSET_VARIABLE}")
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("is not set"));
    }

    #[test]
    fn replace_refs_swaps_references_without_resolving_them() {
        let replaced = replace_refs("Bearer ${env:PLUGBAY_NEVER_SET} ${fnox:B}", |reference| {
            format!("<{}>", reference.describe())
        })
        .unwrap();
        assert_eq!(replaced, "Bearer <env:PLUGBAY_NEVER_SET> <fnox:B>");
    }

    #[tokio::test]
    async fn resolve_map_keeps_keys() {
        let values = BTreeMap::from([("X-Static".to_string(), "value".to_string())]);
        assert_eq!(resolve_map(&values).await.unwrap(), values);
    }

    #[test]
    fn secret_looking_names_are_detected() {
        assert!(looks_secret("LINEAR_API_KEY"));
        assert!(looks_secret("Authorization"));
        assert!(looks_secret("x-auth-token"));
        assert!(!looks_secret("REGION"));
    }

    #[test]
    fn redact_keeps_only_the_auth_scheme() {
        assert_eq!(redact("Bearer test-token"), "Bearer [redacted]");
        assert_eq!(redact("test-token"), "[redacted]");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn trailing_newline_from_cli_output_is_stripped() {
        assert_eq!(strip_trailing_newline("value\r\n".to_string()), "value");
        assert_eq!(strip_trailing_newline("value".to_string()), "value");
    }
}
