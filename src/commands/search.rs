//! `plugbay search` and `plugbay add --from`: servers from the official MCP Registry. Only a
//! remote url is ever written; a package is turned into a command for the user to review and
//! run, because a registry entry must never run code on this machine.

use anyhow::bail;

use super::pins::AddOptions;
use crate::config::{self, ServerIdentity};
use crate::identity::one_safe_line;
use crate::registry::official::package::{self, PackageCommand, shell_word};
use crate::registry::official::{self, Offer, Package, RegistryServer, Remote};
use crate::registry::{self, RegistryEntry};

const MAX_SHOWN: usize = 15;
const MAX_TITLE_CHARS: usize = 60;
const MAX_DESCRIPTION_CHARS: usize = 100;
const MAX_URL_CHARS: usize = 120;

pub async fn search(words: &[String]) -> anyhow::Result<()> {
    let query = words.join(" ");
    let results = official::search(&query).await?;
    print!(
        "{}",
        format_results(&query, &results.servers, results.more_in_registry)
    );
    Ok(())
}

fn format_results(query: &str, servers: &[RegistryServer], more_in_registry: bool) -> String {
    let mut output = String::new();
    if let Some(entry) = registry::lookup(&query.trim().to_lowercase()) {
        output.push_str(&format!(
            "{name} is built in ({url}): `plugbay use {name}` works without adding anything.\n\n",
            name = entry.name,
            url = entry.url
        ));
    }
    if servers.is_empty() {
        output.push_str(&format!(
            "No server in the MCP Registry matches `{}`.\n",
            one_safe_line(query, MAX_TITLE_CHARS)
        ));
        return output;
    }
    for (position, server) in servers.iter().take(MAX_SHOWN).enumerate() {
        if position > 0 {
            output.push('\n');
        }
        output.push_str(&server.name);
        if let Some(title) = &server.title {
            output.push_str(&format!("  {}", one_safe_line(title, MAX_TITLE_CHARS)));
        }
        if let Some(entry) = built_in_entry(server) {
            output.push_str(&format!("  (built in: `plugbay use {}`)", entry.name));
        }
        output.push('\n');
        if let Some(description) = &server.description {
            output.push_str(&format!(
                "    {}\n",
                one_safe_line(description, MAX_DESCRIPTION_CHARS)
            ));
        }
        output.push_str(&format!("    {}\n", what_it_is(server)));
    }
    let hidden = servers.len().saturating_sub(MAX_SHOWN);
    if hidden > 0 || more_in_registry {
        let count = if more_in_registry {
            "More matches".to_string()
        } else {
            format!("{hidden} more")
        };
        output.push_str(&format!(
            "\n{count} in the registry; add words to narrow the search.\n"
        ));
    }
    output
        .push_str("\nAdd one to your catalog with `plugbay add <name> --from <registry-name>`.\n");
    output
}

/// `remote https://…`, `npm @scope/pkg`, or why plugbay can't use it.
fn what_it_is(server: &RegistryServer) -> String {
    match server.offer() {
        Offer::Remotes(remotes) => {
            let urls: Vec<String> = remotes
                .iter()
                .map(|remote| one_safe_line(&remote.url, MAX_URL_CHARS))
                .collect();
            let needs_token = remotes
                .iter()
                .any(|remote| !remote.required_headers().is_empty());
            let token_note = if needs_token { " (needs a token)" } else { "" };
            format!("remote: {}{token_note}", urls.join(", "))
        }
        Offer::RefusedRemote(error) => format!(
            "remote, but {}",
            one_safe_line(&format!("{error:#}"), MAX_URL_CHARS)
        ),
        Offer::Packages(packages) => {
            let packages: Vec<String> = packages
                .iter()
                .map(|package| {
                    format!(
                        "{} {}",
                        package.registry_type,
                        one_safe_line(&package.identifier, MAX_URL_CHARS)
                    )
                })
                .collect();
            format!("runs locally: {}", packages.join(", "))
        }
        Offer::SseOnly => "remote over SSE only, which plugbay doesn't support".to_string(),
        Offer::Nothing => "lists nothing to connect to".to_string(),
    }
}

/// The built-in server a registry entry's remote already points at, so `plugbay use` works as is.
fn built_in_entry(server: &RegistryServer) -> Option<&'static RegistryEntry> {
    let Offer::Remotes(remotes) = server.offer() else {
        return None;
    };
    remotes
        .iter()
        .find_map(|remote| built_in_for_url(&remote.url))
}

fn built_in_for_url(text: &str) -> Option<&'static RegistryEntry> {
    let url = url::Url::parse(text).ok()?;
    registry::entry_for_identity(&ServerIdentity::for_url(&url))
}

/// `plugbay add com.acme/tracker` means `plugbay add tracker --from com.acme/tracker`.
pub fn take_registry_name(options: &mut AddOptions) -> anyhow::Result<()> {
    let is_bare_name =
        options.from.is_none() && options.url.is_none() && options.command.is_empty();
    if !is_bare_name || !options.name.contains('/') {
        return Ok(());
    }
    let Some(name) = name_from_registry_name(&options.name) else {
        bail!(
            "pick a name for it: `plugbay add <name> --from {}`",
            options.name
        );
    };
    options.from = Some(std::mem::replace(&mut options.name, name));
    Ok(())
}

/// Where `plugbay add --from` got a catalog entry, and what the user still has to do for it.
pub struct RegistrySource {
    pub registry_name: String,
    url: String,
    required_headers: Vec<String>,
}

/// For `plugbay add --from`: looks the registry server up and sets `options.url` to the remote
/// to write, or explains why nothing can be written (for a package, with the command to add it
/// by hand). `None` when the options don't come from the registry.
pub async fn registry_source(options: &mut AddOptions) -> anyhow::Result<Option<RegistrySource>> {
    let Some(registry_name) = &options.from else {
        if options.remote.is_some() {
            bail!("`--remote` picks one of a registry server's remotes; use it with `--from`");
        }
        return Ok(None);
    };
    let server = official::lookup(registry_name).await?;
    let source = choose_remote(
        &server,
        &options.name,
        options.remote.as_deref(),
        options.force,
    )?;
    options.url = Some(source.url.clone());
    Ok(Some(source))
}

fn choose_remote(
    server: &RegistryServer,
    name: &str,
    chosen_url: Option<&str>,
    force: bool,
) -> anyhow::Result<RegistrySource> {
    let remotes = match server.offer() {
        Offer::Remotes(remotes) => remotes,
        Offer::RefusedRemote(error) => bail!(
            "nothing written: plugbay won't connect to {}: {error:#}",
            server.name
        ),
        Offer::Packages(packages) => {
            bail!("nothing written: {}", package_steps(server, packages, name))
        }
        Offer::SseOnly => bail!(
            "nothing written: {} only offers a remote over SSE, which plugbay doesn't support. {}",
            server.name,
            add_by_hand(name)
        ),
        Offer::Nothing => bail!(
            "nothing written: {} lists no remote or package. {}",
            server.name,
            add_by_hand(name)
        ),
    };
    let remote = pick_remote(server, &remotes, chosen_url)?;
    if let Some(entry) = built_in_for_url(&remote.url)
        && !force
    {
        bail!(
            "nothing written: {} is built in as {built_in}, so `plugbay use {built_in}` works \
             without adding it (pass `--force` to add it under another name anyway)",
            server.name,
            built_in = entry.name
        );
    }
    Ok(RegistrySource {
        registry_name: server.name.clone(),
        url: remote.url.clone(),
        required_headers: remote
            .required_headers()
            .into_iter()
            .map(str::to_string)
            .collect(),
    })
}

fn pick_remote<'a>(
    server: &RegistryServer,
    remotes: &[&'a Remote],
    chosen_url: Option<&str>,
) -> anyhow::Result<&'a Remote> {
    // Full and shell-quoted, so a pasted choice is exactly the remote and never runs anything.
    let choices = || {
        remotes
            .iter()
            .map(|remote| format!("  --remote {}", shell_word(&remote.url)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match (chosen_url, remotes) {
        (Some(url), _) => match remotes.iter().find(|remote| remote.url == url) {
            Some(remote) => Ok(remote),
            None => bail!(
                "nothing written: `{url}` is not one of {}'s https remotes; pick one of:\n{}",
                server.name,
                choices()
            ),
        },
        (None, [only]) => Ok(only),
        (None, _) => bail!(
            "nothing written: {} has {} remotes; pick one with `--remote`:\n{}",
            server.name,
            remotes.len(),
            choices()
        ),
    }
}

fn add_by_hand(name: &str) -> String {
    format!(
        "If you know how to run it, add it with `plugbay add {name} --url <url>` or \
         `plugbay add {name} -- <command>`"
    )
}

/// The `plugbay add` command for each package plugbay knows how to run, to review and run by hand.
fn package_steps(server: &RegistryServer, packages: &[Package], requested_name: &str) -> String {
    let name = match registry::lookup(requested_name) {
        Some(_) => format!("{requested_name}-local"),
        None => requested_name.to_string(),
    };
    let commands: Vec<PackageCommand> = packages
        .iter()
        .filter_map(|package| package::package_command(&name, package))
        .collect();
    if commands.is_empty() {
        return format!(
            "{} only ships packages plugbay can't suggest a command for. {}",
            server.name,
            add_by_hand(requested_name)
        );
    }
    let mut message = format!(
        "{} runs as a local package, and plugbay never runs code from a registry entry on its \
         own. Check the package, then add it yourself:\n",
        server.name
    );
    for command in &commands {
        message.push_str(&format!("\n  {}\n", command.command_line()));
        if !command.optional_env.is_empty() {
            message.push_str(&format!(
                "    (optional env: {})\n",
                command.optional_env.join(", ")
            ));
        }
    }
    let mut notes = Vec::new();
    if name != requested_name {
        notes.push(format!(
            "{requested_name} is built in, so the command adds it as {name}."
        ));
    }
    if commands.iter().any(|command| command.has_placeholders) {
        notes.push("Replace each `<...>` first.".to_string());
    }
    if commands.iter().any(|command| command.has_secret_refs) {
        notes.push(format!(
            "Store each secret before using it, e.g. on macOS \
             `security add-generic-password -s {name} -a <NAME> -w`, or point the ref at \
             `${{op://vault/item/field}}` or `${{env:NAME}}` instead."
        ));
    }
    if !notes.is_empty() {
        message.push('\n');
        message.push_str(&notes.join(" "));
    }
    message.trim_end().to_string()
}

impl RegistrySource {
    /// The `login` step for a remote that declared required headers, if it has any.
    pub fn next_steps(&self, name: &str) -> Option<String> {
        match self.required_headers.as_slice() {
            [] => None,
            [header] if header.eq_ignore_ascii_case("authorization") => Some(format!(
                "It needs a token: store it with `plugbay login {name} --token`."
            )),
            [header] => Some(format!(
                "It needs a token in the `{header}` header: store it with \
                 `plugbay login {name} --token --header {}`.",
                shell_word(header)
            )),
            several => Some(format!(
                "It needs these headers: {}. `plugbay login {name} --token --header <NAME>` stores \
                 one of them; give the others as `${{keychain:...}}` refs under \
                 `[servers.{name}.headers]` in your global config.",
                several.join(", ")
            )),
        }
    }
}

/// `plugbay add <registry-name>` without `--from`: the catalog name to use,
/// `io.github.acme/tracker-mcp` → `tracker`.
fn name_from_registry_name(registry_name: &str) -> Option<String> {
    let (_, server) = registry_name.split_once('/')?;
    let mut name = String::new();
    for character in server.to_ascii_lowercase().chars() {
        let character = if character.is_ascii_alphanumeric() {
            character
        } else {
            '-'
        };
        if !(character == '-' && (name.is_empty() || name.ends_with('-'))) {
            name.push(character);
        }
    }
    let name = name.trim_end_matches('-');
    let shortened = ["-mcp-server", "-server", "-mcp"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .or_else(|| name.strip_prefix("mcp-"))
        .filter(|shortened| !shortened.is_empty())
        .unwrap_or(name);
    config::is_valid_name(shortened).then(|| shortened.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::official::tests::{fixture_search, fixture_server};

    #[test]
    fn results_show_what_each_server_is_and_which_are_built_in() {
        insta::assert_snapshot!(format_results("tracker", &fixture_search("tracker"), false));
    }

    #[test]
    fn no_results_say_so() {
        assert_eq!(
            format_results("nothing", &[], false),
            "No server in the MCP Registry matches `nothing`.\n"
        );
    }

    #[test]
    fn searching_a_built_in_name_says_it_needs_no_adding() {
        let output = format_results("Linear", &[], false);
        assert!(
            output.starts_with(
                "linear is built in (https://mcp.linear.app/mcp): `plugbay use linear`"
            ),
            "{output}"
        );
    }

    #[test]
    fn a_registry_with_more_pages_says_to_narrow_the_search() {
        let output = format_results("tracker", &fixture_search("tracker"), true);
        assert!(
            output.contains("More matches in the registry; add words"),
            "{output}"
        );
    }

    #[test]
    fn a_single_streamable_remote_is_chosen_and_its_token_header_explained() {
        let server = fixture_server("com.acme/tracker");
        let source = choose_remote(&server, "acme", None, false).unwrap();
        assert_eq!(source.url, "https://mcp.acme.example/mcp");
        assert_eq!(source.registry_name, "com.acme/tracker");
        assert_eq!(
            source.next_steps("acme").as_deref(),
            Some("It needs a token: store it with `plugbay login acme --token`.")
        );
    }

    #[test]
    fn several_remotes_need_a_choice() {
        let server = fixture_server("app.linear/linear");
        let error = choose_remote(&server, "lin", None, false)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("--remote https://mcp.linear.app/mcp/readonly"),
            "{error}"
        );

        let chosen = choose_remote(
            &server,
            "linear-readonly",
            Some("https://mcp.linear.app/mcp/readonly"),
            false,
        )
        .unwrap();
        assert_eq!(chosen.url, "https://mcp.linear.app/mcp/readonly");
        assert!(chosen.next_steps("linear-readonly").is_none());

        let unknown = choose_remote(&server, "lin", Some("https://evil.example/mcp"), false);
        assert!(unknown.is_err());
    }

    #[test]
    fn only_https_remotes_count_when_a_server_also_lists_plain_http() {
        let server = fixture_server("net.mixed/tracker");
        let chosen = choose_remote(&server, "mixed", None, false).unwrap();
        assert_eq!(chosen.url, "https://mcp.mixed.example/mcp");
        assert_eq!(what_it_is(&server), "remote: https://mcp.mixed.example/mcp");

        let error = choose_remote(
            &server,
            "mixed",
            Some("http://mcp.mixed.example/mcp"),
            false,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            error.ends_with("pick one of:\n  --remote https://mcp.mixed.example/mcp"),
            "{error}"
        );
    }

    #[test]
    fn remote_choices_are_printed_whole_and_shell_quoted() {
        let server: RegistryServer = serde_json::from_str(
            r#"{"name": "com.evil/tracker", "remotes": [
                {"type": "streamable-http", "url": "https://example.com/$(printf INJECTED)"},
                {"type": "streamable-http", "url": "https://example.com/mcp"}
            ]}"#,
        )
        .unwrap();
        let error = choose_remote(&server, "evil", None, false)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("\n  --remote 'https://example.com/$(printf INJECTED)'\n"),
            "{error}"
        );
    }

    #[test]
    fn a_remote_that_is_built_in_points_to_use() {
        let server = fixture_server("app.linear/linear");
        let error = choose_remote(&server, "lin", Some("https://mcp.linear.app/mcp"), false)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("`plugbay use linear`"), "{error}");
        assert!(choose_remote(&server, "lin", Some("https://mcp.linear.app/mcp"), true).is_ok());
    }

    #[test]
    fn a_plain_http_remote_is_refused() {
        let server = fixture_server("dev.tricky/tracker");
        let error = choose_remote(&server, "tricky", None, false)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("not https"), "{error}");
    }

    #[test]
    fn sse_only_servers_are_unsupported() {
        let server = fixture_server("dev.oldco/legacy");
        let error = choose_remote(&server, "legacy", None, false)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("SSE, which plugbay doesn't support"),
            "{error}"
        );
    }

    #[test]
    fn an_npm_package_becomes_a_command_to_review_with_env_refs() {
        let server = fixture_server("io.github.acme-dev/tracker-mcp");
        let error = choose_remote(&server, "tracker", None, false)
            .err()
            .unwrap()
            .to_string();
        insta::assert_snapshot!(error);
    }

    #[test]
    fn a_package_suggested_under_a_built_in_name_gets_another_name() {
        let server = fixture_server("io.github.acme-dev/tracker-mcp");
        let message = package_steps(&server, &server.packages, "linear");
        assert!(
            message.contains("  plugbay add linear-local --env"),
            "{message}"
        );
        assert!(message.contains("linear is built in"), "{message}");
    }

    #[test]
    fn registry_names_shorten_to_catalog_names() {
        assert_eq!(
            name_from_registry_name("io.github.acme/tracker-mcp").as_deref(),
            Some("tracker")
        );
        assert_eq!(
            name_from_registry_name("io.github.github/github-mcp-server").as_deref(),
            Some("github")
        );
        assert_eq!(
            name_from_registry_name("com.acme/Acme_Tracker").as_deref(),
            Some("acme-tracker")
        );
        assert_eq!(
            name_from_registry_name("com.acme/mcp").as_deref(),
            Some("mcp")
        );
        assert_eq!(name_from_registry_name("no-slash"), None);
    }
}
