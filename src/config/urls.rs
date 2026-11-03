//! Urls for people and for policy: masking secrets for display, and telling a local address apart.

use url::Url;

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

/// True when [`display_url`] would mask part of `url`: it may carry a key.
pub fn url_may_hold_secret(url: &Url) -> bool {
    display_url(url).contains(MASK)
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

/// Whether `url` points at this machine: `localhost` (or a name under it, with or without the
/// final DNS dot) or a loopback or unspecified address, IPv4-mapped IPv6 included.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => {
            let domain = domain
                .strip_suffix('.')
                .unwrap_or(domain)
                .to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(address)) => address.is_loopback() || address.is_unspecified(),
        Some(url::Host::Ipv6(address)) => match address.to_ipv4_mapped() {
            Some(mapped) => mapped.is_loopback() || mapped.is_unspecified(),
            None => address.is_loopback() || address.is_unspecified(),
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn loopback_urls_include_dotted_localhost_and_mapped_addresses() {
        let local = [
            "http://localhost:7373/mcp",
            "https://LOCALHOST./mcp",
            "https://app.localhost/mcp",
            "http://127.0.0.1/mcp",
            "http://127.1.2.3/mcp",
            "http://0.0.0.0/mcp",
            "http://[::1]/mcp",
            "http://[::]/mcp",
            "https://[::ffff:127.0.0.1]/mcp",
            "https://[::ffff:0.0.0.0]/mcp",
            "http://2130706433/mcp",
        ];
        for text in local {
            assert!(is_loopback(&Url::parse(text).unwrap()), "{text}");
        }
        let remote = [
            "https://mcp.linear.app/mcp",
            "https://localhost.example.com/mcp",
            "https://[::ffff:8.8.8.8]/mcp",
            "https://10.0.0.1/mcp",
        ];
        for text in remote {
            assert!(!is_loopback(&Url::parse(text).unwrap()), "{text}");
        }
    }
}
