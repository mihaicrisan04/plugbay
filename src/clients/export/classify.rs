//! Which config values `export` may copy. Env and header values, the url and args are classified;
//! the command, `cwd` and header names are copied as is. Spotting a secret written as a literal is
//! a heuristic, so it leans towards a placeholder: a false positive costs nothing but a prompt.

use std::collections::BTreeMap;

use super::ExportFormat;
use crate::config::{self, ResolvedServer, ServerTransport};
use crate::secrets::{self, SecretRef};

/// How one env or header value leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ValuePlan {
    /// As written: nothing in it looks secret.
    Copy,
    /// Each secret ref swapped for a placeholder, the text around them kept.
    SwapRefs,
    /// One placeholder for the whole value: the text itself may be a secret.
    Replace,
}

/// Why `server` can't be exported safely, if it can't. Never quotes a value: it may be a secret.
pub(super) fn skip_reason(server: &ResolvedServer, format: ExportFormat) -> Option<String> {
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
        ServerTransport::Stdio { args, .. }
            if args
                .iter()
                .any(|argument| secrets::has_token_literal(argument)) =>
        {
            return Some(
                "its args hold a value shaped like a token; move it to `env` with a secret ref"
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
pub(super) fn configured_values(
    transport: &ServerTransport,
) -> (&'static str, &BTreeMap<String, String>) {
    match transport {
        ServerTransport::Http { headers, .. } => ("headers", headers),
        ServerTransport::Stdio { env, .. } => ("env", env),
    }
}

/// In a secret-named field only a `Bearer ` scheme may sit next to a ref (`key-${env:SUFFIX}`
/// would leak half the key). A ref inside a URL takes the whole value with it, since the text
/// around it may be half a credential.
pub(super) fn value_plan(field: &str, value: &str) -> ValuePlan {
    let secret_field = secrets::is_secret_field(field);
    if !secrets::contains_secret_ref(value) {
        let may_be_secret = secret_field || may_hold_secret(value);
        return if value.is_empty() || !may_be_secret {
            ValuePlan::Copy
        } else {
            ValuePlan::Replace
        };
    }
    let (_, after_scheme) = secrets::split_bearer_scheme(value);
    let (Ok(literal), Ok(ref_in_url)) = (
        text_around_refs(after_scheme),
        secrets::has_ref_in_url(value),
    ) else {
        return ValuePlan::Replace;
    };
    let literal_next_to_secret = secret_field && !literal.trim().is_empty();
    if literal_next_to_secret || ref_in_url || may_hold_secret(&literal) {
        ValuePlan::Replace
    } else {
        ValuePlan::SwapRefs
    }
}

/// A literal that is a secret whatever its field is called: a bearer credential, a URL carrying
/// one, or a token-shaped word.
fn may_hold_secret(text: &str) -> bool {
    let (scheme, _) = secrets::split_bearer_scheme(text);
    !scheme.is_empty() || secrets::has_secret_url(text) || secrets::has_token_literal(text)
}

/// `value` with each ref swapped for a space, so the text on either side never reads as one word.
fn text_around_refs(value: &str) -> anyhow::Result<String> {
    secrets::replace_refs(value, |_| " ".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_ACCOUNT;

    fn stdio_reason(args: &[&str]) -> Option<String> {
        let server = ResolvedServer::test_stdio("tool", DEFAULT_ACCOUNT, "npx", args);
        skip_reason(&server, ExportFormat::Claude)
    }

    #[test]
    fn secrets_in_args_skip_the_server_without_quoting_them() {
        let cases = [
            (&["--api-key=fake-test-token"][..], "seem to hold a secret"),
            (
                &["--url", "https://fake-user:fake-pass@mcp.example.com/mcp"][..],
                "may carry a credential",
            ),
            (&["tool", "xoxb-fake-test-token"][..], "shaped like a token"),
        ];
        for (args, expected) in cases {
            let reason = stdio_reason(args).unwrap_or_default();
            assert!(reason.contains(expected), "{args:?}: {reason}");
            assert!(!reason.contains("fake-"), "{reason}");
        }
        assert_eq!(stdio_reason(&["-y", "tool@1.2.3", "--port", "3000"]), None);
    }

    #[test]
    fn url_that_may_hold_a_key_is_skipped_masked() {
        let server = ResolvedServer::test_http(
            "keyed",
            DEFAULT_ACCOUNT,
            "https://mcp.example.com/mcp?api_key=fake-test-token",
        );
        let reason = skip_reason(&server, ExportFormat::Claude).unwrap_or_default();
        assert!(reason.contains("api_key=***"), "{reason}");
        assert!(!reason.contains("fake-"), "{reason}");
    }

    #[test]
    fn plain_values_are_copied() {
        assert_eq!(value_plan("LOG_LEVEL", "debug"), ValuePlan::Copy);
        assert_eq!(
            value_plan("LOG_URL", "https://logs.example.com/ingest"),
            ValuePlan::Copy
        );
        assert_eq!(value_plan("API_KEY", ""), ValuePlan::Copy);
    }

    #[test]
    fn refs_alone_or_after_a_bearer_scheme_are_swapped() {
        assert_eq!(
            value_plan("authorization", "Bearer ${op://work/linear/key}"),
            ValuePlan::SwapRefs
        );
        assert_eq!(
            value_plan("GITHUB_TOKEN", "${env:GITHUB_TOKEN}"),
            ValuePlan::SwapRefs
        );
        assert_eq!(value_plan("REGION", "eu-${env:ZONE}"), ValuePlan::SwapRefs);
    }

    #[test]
    fn refs_inside_a_credential_url_replace_the_whole_value() {
        let values = [
            "https://${env:USER}:fake-url-password@host/app",
            "https://fake-user:fake-url-password${env:SUFFIX}@host/app",
            "postgres://${env:DB_USER}:${fnox:DB_PASSWORD}@${fnox:DB_HOST}/app",
            "https://mcp.example.com/${env:PATH_PART}",
        ];
        for value in values {
            assert_eq!(
                value_plan("DATABASE_URL", value),
                ValuePlan::Replace,
                "{value}"
            );
            assert_eq!(value_plan("x-target", value), ValuePlan::Replace, "{value}");
        }
    }

    #[test]
    fn credential_urls_with_spaces_in_the_password_are_replaced() {
        assert_eq!(
            value_plan(
                "DATABASE_URL",
                "postgres://fake-user:fake password@host/app"
            ),
            ValuePlan::Replace
        );
    }

    #[test]
    fn token_shaped_literals_under_innocent_names_are_replaced() {
        let values = [
            "ghp_FAKE123456789012345678901234567890123456",
            "github_pat_FAKE1234567890",
            "sk-FAKE123456789012345678901234",
            "xoxb-fake-test-token",
            crate::secrets::FAKE_JWT,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "ZmFrZXNlY3JldA==ZmFrZXNlY3JldA==ZmFrZXNlY3JldA==ZmFrZXNlY3JldA==",
            "Bearer fake-literal",
        ];
        for value in values {
            assert_eq!(value_plan("X", value), ValuePlan::Replace, "{value}");
        }
        assert_eq!(
            value_plan("X", "ghp_FAKE1234567890 ${env:SUFFIX}"),
            ValuePlan::Replace
        );
    }

    #[test]
    fn malformed_refs_replace_the_whole_value() {
        assert_eq!(
            value_plan("X", "fake-private-${env:SUFFIX"),
            ValuePlan::Replace
        );
    }
}
