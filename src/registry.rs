//! Built-in short names for well-known remote MCP servers, like mise's tool registry.
//! Only remote servers live here: a registry entry never runs code on the machine.

use crate::config::ServerIdentity;

pub struct RegistryEntry {
    pub name: &'static str,
    pub title: &'static str,
    pub url: &'static str,
    /// How to ask the server which account a login belongs to (see `identity.rs`).
    pub whoami: Option<Whoami>,
}

/// A read-only tool that names the workspace or user behind a login. Only the registry defines
/// these: committed config must never choose which tool plugbay calls on its own.
pub struct Whoami {
    pub tool: &'static str,
    /// JSON object text sent as the tool's arguments.
    pub arguments: &'static str,
    /// JSON pointers into the answer, tried in order; the first that names something wins.
    pub show: &'static [&'static str],
}

/// Official remote endpoints only; anything unsure stays out and users set `url` themselves.
const ENTRIES: &[RegistryEntry] = &[
    RegistryEntry {
        name: "atlassian",
        title: "Atlassian",
        url: "https://mcp.atlassian.com/v1/mcp",
        whoami: Some(Whoami {
            tool: "getAccessibleAtlassianResources",
            arguments: "{}",
            show: &[],
        }),
    },
    RegistryEntry {
        name: "context7",
        title: "Context7",
        url: "https://mcp.context7.com/mcp",
        whoami: None,
    },
    RegistryEntry {
        name: "github",
        title: "GitHub",
        url: "https://api.githubcopilot.com/mcp/",
        whoami: Some(Whoami {
            tool: "get_me",
            arguments: "{}",
            show: &["/login"],
        }),
    },
    RegistryEntry {
        name: "linear",
        title: "Linear",
        url: "https://mcp.linear.app/mcp",
        whoami: Some(Whoami {
            tool: "get_workspace",
            arguments: "{}",
            show: &["/name"],
        }),
    },
    RegistryEntry {
        name: "notion",
        title: "Notion",
        url: "https://mcp.notion.com/mcp",
        whoami: None,
    },
    RegistryEntry {
        name: "posthog",
        title: "PostHog",
        url: "https://mcp.posthog.com/mcp",
        whoami: None,
    },
    RegistryEntry {
        name: "sentry",
        title: "Sentry",
        url: "https://mcp.sentry.dev/mcp",
        whoami: Some(Whoami {
            tool: "find_organizations",
            arguments: "{}",
            show: &["/organizations"],
        }),
    },
    RegistryEntry {
        name: "stripe",
        title: "Stripe",
        url: "https://mcp.stripe.com",
        whoami: Some(Whoami {
            tool: "get_stripe_account_info",
            arguments: "{}",
            show: &[
                "/display_name",
                "/business_profile/name",
                "/settings/dashboard/display_name",
            ],
        }),
    },
    RegistryEntry {
        name: "supabase",
        title: "Supabase",
        url: "https://mcp.supabase.com/mcp",
        whoami: Some(Whoami {
            tool: "list_organizations",
            arguments: "{}",
            show: &[],
        }),
    },
    RegistryEntry {
        name: "vercel",
        title: "Vercel",
        url: "https://mcp.vercel.com",
        whoami: Some(Whoami {
            tool: "list_teams",
            arguments: "{}",
            show: &["/result/teams", "/teams"],
        }),
    },
];

pub fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    ENTRIES.iter().find(|entry| entry.name == name)
}

pub fn all() -> &'static [RegistryEntry] {
    ENTRIES
}

/// Matched by identity, not name: a config that points `linear` at another url gets no whoami.
pub fn whoami_for(identity: &ServerIdentity) -> Option<&'static Whoami> {
    ENTRIES.iter().find_map(|entry| {
        let whoami = entry.whoami.as_ref()?;
        let url = url::Url::parse(entry.url).ok()?;
        (ServerIdentity::for_url(&url) == *identity).then_some(whoami)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_known_server() {
        let entry = lookup("linear").map(|entry| entry.url);
        assert_eq!(entry, Some("https://mcp.linear.app/mcp"));
    }

    #[test]
    fn lookup_misses_unknown_server() {
        assert!(lookup("not-a-server").is_none());
    }

    #[test]
    fn every_entry_is_a_valid_https_url_with_a_valid_name() {
        for entry in all() {
            let url = url::Url::parse(entry.url).unwrap();
            assert_eq!(url.scheme(), "https", "{}", entry.name);
            assert!(crate::config::is_valid_name(entry.name), "{}", entry.name);
        }
    }

    #[test]
    fn whoami_is_found_by_identity_only() {
        let linear =
            ServerIdentity::for_url(&url::Url::parse("https://mcp.linear.app/mcp/").unwrap());
        assert_eq!(
            whoami_for(&linear).map(|whoami| whoami.tool),
            Some("get_workspace")
        );
        let elsewhere = ServerIdentity("https://linear.example.test/mcp".to_string());
        assert!(whoami_for(&elsewhere).is_none());
    }

    #[test]
    fn whoami_arguments_are_json_objects_and_pointers_are_valid() {
        for whoami in all().iter().filter_map(|entry| entry.whoami.as_ref()) {
            let arguments: serde_json::Value = serde_json::from_str(whoami.arguments).unwrap();
            assert!(arguments.is_object(), "{}", whoami.tool);
            for pointer in whoami.show {
                assert!(pointer.starts_with('/'), "{} {pointer}", whoami.tool);
            }
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = all().iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all().len());
    }
}
