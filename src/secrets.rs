//! Secret references inside config values: `${op://vault/item/field}`, `${env:NAME}`,
//! `${keychain:service/account}`, `${fnox:NAME}`. Config files hold refs, never values.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};

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
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
