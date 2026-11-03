//! `plugbay search` and `plugbay add --from`: servers from the official MCP Registry. Only a
//! remote url is ever written; a package is turned into a command for the user to review and
//! run, because a registry entry must never run code on this machine.

use anyhow::bail;

use crate::config::{self, ServerIdentity};
use crate::identity::one_safe_line;
use crate::registry::official::{self, Argument, Package, RegistryServer, Remote};
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
    let streamable = server.streamable_remotes();
    let usable: Vec<&&Remote> = streamable
        .iter()
        .filter(|remote| official::https_url(&remote.url).is_ok())
        .collect();
    if !usable.is_empty() {
        let urls: Vec<String> = usable
            .iter()
            .map(|remote| one_safe_line(&remote.url, MAX_URL_CHARS))
            .collect();
        let needs_token = usable
            .iter()
            .any(|remote| !remote.required_headers().is_empty());
        let token_note = if needs_token { " (needs a token)" } else { "" };
        return format!("remote: {}{token_note}", urls.join(", "));
    }
    if let Some(remote) = streamable.first()
        && let Err(error) = official::https_url(&remote.url)
    {
        return format!(
            "remote, but {}",
            one_safe_line(&format!("{error:#}"), MAX_URL_CHARS)
        );
    }
    if !server.packages.is_empty() {
        let packages: Vec<String> = server
            .packages
            .iter()
            .map(|package| {
                format!(
                    "{} {}",
                    package.registry_type,
                    one_safe_line(&package.identifier, MAX_URL_CHARS)
                )
            })
            .collect();
        return format!("runs locally: {}", packages.join(", "));
    }
    if !server.remotes.is_empty() {
        return "remote over SSE only, which plugbay doesn't support".to_string();
    }
    "lists nothing to connect to".to_string()
}

/// The built-in server a registry entry's remote already points at, so `plugbay use` works as is.
fn built_in_entry(server: &RegistryServer) -> Option<&'static RegistryEntry> {
    server
        .streamable_remotes()
        .iter()
        .find_map(|remote| built_in_for_url(&remote.url))
}

fn built_in_for_url(text: &str) -> Option<&'static RegistryEntry> {
    let identity = ServerIdentity::for_url(&url::Url::parse(text).ok()?);
    registry::all().iter().find(|entry| {
        url::Url::parse(entry.url).is_ok_and(|url| ServerIdentity::for_url(&url) == identity)
    })
}

/// The remote `plugbay add --from` writes, and what the user still has to do for it.
pub struct ChosenRemote {
    pub url: String,
    required_headers: Vec<String>,
}

/// Looks `registry_name` up and picks the remote to write as catalog entry `name`, or explains
/// why nothing can be written (and, for a package, prints the command to add it by hand).
pub async fn remote_from_registry(
    name: &str,
    registry_name: &str,
    chosen_url: Option<&str>,
    force: bool,
) -> anyhow::Result<ChosenRemote> {
    let server = official::lookup(registry_name).await?;
    choose_remote(&server, name, chosen_url, force)
}

fn choose_remote(
    server: &RegistryServer,
    name: &str,
    chosen_url: Option<&str>,
    force: bool,
) -> anyhow::Result<ChosenRemote> {
    let streamable = server.streamable_remotes();
    if streamable.is_empty() {
        bail!("nothing written: {}", why_no_remote(server, name));
    }
    let remote = pick_remote(server, &streamable, chosen_url)?;
    if let Err(error) = official::https_url(&remote.url) {
        bail!(
            "nothing written: plugbay won't connect to {}: {error:#}",
            server.name
        );
    }
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
    Ok(ChosenRemote {
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
    streamable: &[&'a Remote],
    chosen_url: Option<&str>,
) -> anyhow::Result<&'a Remote> {
    let listed = || {
        streamable
            .iter()
            .map(|remote| format!("  --remote {}", one_safe_line(&remote.url, MAX_URL_CHARS)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match (chosen_url, streamable) {
        (Some(url), _) => match streamable.iter().find(|remote| remote.url == url) {
            Some(remote) => Ok(remote),
            None => bail!(
                "nothing written: `{url}` is not one of {}'s remotes; pick one of:\n{}",
                server.name,
                listed()
            ),
        },
        (None, [only]) => Ok(only),
        (None, _) => bail!(
            "nothing written: {} has {} remotes; pick one with `--remote`:\n{}",
            server.name,
            streamable.len(),
            listed()
        ),
    }
}

fn why_no_remote(server: &RegistryServer, requested_name: &str) -> String {
    let name = match registry::lookup(requested_name) {
        Some(_) => format!("{requested_name}-local"),
        None => requested_name.to_string(),
    };
    let commands: Vec<String> = server
        .packages
        .iter()
        .filter_map(|package| package_command(&name, package))
        .collect();
    let sse_only = !server.remotes.is_empty();
    if commands.is_empty() {
        let reason = if sse_only {
            "only offers a remote over SSE, which plugbay doesn't support"
        } else if server.packages.is_empty() {
            "lists no remote or package"
        } else {
            "only ships packages plugbay can't suggest a command for"
        };
        return format!(
            "{} {reason}. If you know how to run it, add it with \
             `plugbay add {requested_name} --url <url>` or `plugbay add {requested_name} -- <command>`",
            server.name
        );
    }
    let mut message = format!(
        "{} runs as a local package, and plugbay never runs code from a registry entry on its \
         own. Check the package, then add it yourself:\n",
        server.name
    );
    for command in &commands {
        message.push_str(&format!("\n  {command}\n"));
    }
    let mut notes = Vec::new();
    if name != requested_name {
        notes.push(format!(
            "{requested_name} is built in, so the command adds it as {name}."
        ));
    }
    if commands.iter().any(|command| command.contains('<')) {
        notes.push("Replace each `<...>` first.".to_string());
    }
    if commands
        .iter()
        .any(|command| command.contains("${keychain:"))
    {
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

/// The `plugbay add <name> [--env ...] -- <command>` line for a stdio package, or `None` when
/// plugbay doesn't know how to run that kind of package.
fn package_command(name: &str, package: &Package) -> Option<String> {
    let is_stdio = package
        .transport
        .kind
        .as_deref()
        .is_none_or(|kind| kind == "stdio");
    let has_valid_env_names = package
        .environment_variables
        .iter()
        .all(|variable| is_env_name(&variable.name));
    if !is_stdio || !has_valid_env_names {
        return None;
    }
    let required_env: Vec<&official::EnvironmentVariable> = package
        .environment_variables
        .iter()
        .filter(|variable| variable.is_required)
        .collect();

    let mut words: Vec<String> = vec!["plugbay".into(), "add".into(), name.into()];
    for variable in &required_env {
        let value = match (&variable.default_value, variable.is_secret) {
            (_, true) => format!("${{keychain:{name}/{}}}", variable.name),
            (Some(default), false) => default.clone(),
            (None, false) => "<value>".to_string(),
        };
        words.push("--env".into());
        words.push(format!("{}={value}", variable.name));
    }
    words.push("--".into());
    words.extend(run_command(package, &required_env)?);

    let mut command = words
        .iter()
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ");
    let optional: Vec<&str> = package
        .environment_variables
        .iter()
        .filter(|variable| !variable.is_required)
        .map(|variable| variable.name.as_str())
        .collect();
    if !optional.is_empty() {
        command.push_str(&format!("\n    (optional env: {})", optional.join(", ")));
    }
    Some(command)
}

/// The command line that runs `package`, with the runtime chosen by its registry type. The
/// registry's own `runtimeHint` is ignored: plugbay picks the runtime, not the entry.
fn run_command(package: &Package, env: &[&official::EnvironmentVariable]) -> Option<Vec<String>> {
    let identifier = package.identifier.as_str();
    let version = package.version.as_deref();
    let mut words: Vec<String> = Vec::new();
    match package.registry_type.as_str() {
        "npm" => {
            words.push("npx".into());
            if package.runtime_arguments.is_empty() {
                words.push("-y".into());
            }
            words.extend(argument_words(&package.runtime_arguments));
            words.push(match version {
                Some(version) => format!("{identifier}@{version}"),
                None => identifier.to_string(),
            });
        }
        "pypi" => {
            words.push("uvx".into());
            words.extend(argument_words(&package.runtime_arguments));
            words.push(match version {
                Some(version) => format!("{identifier}=={version}"),
                None => identifier.to_string(),
            });
        }
        "oci" => {
            words.extend(["docker", "run", "-i", "--rm"].map(String::from));
            words.extend(argument_words(&package.runtime_arguments));
            for variable in env {
                words.push("-e".into());
                words.push(variable.name.clone());
            }
            let image_has_tag = identifier
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains(':'))
                || identifier.contains('@');
            words.push(match version {
                Some(version) if !image_has_tag => format!("{identifier}:{version}"),
                _ => identifier.to_string(),
            });
        }
        _ => return None,
    }
    words.extend(argument_words(&package.package_arguments));
    Some(words)
}

/// Fixed and required arguments as words; optional ones without a value are left out.
fn argument_words(arguments: &[Argument]) -> Vec<String> {
    let mut words = Vec::new();
    for argument in arguments {
        let given = argument.value.clone().or(argument.default_value.clone());
        if given.is_none() && !argument.is_required {
            continue;
        }
        let value = given.unwrap_or_else(|| {
            let hint = argument
                .value_hint
                .as_deref()
                .or(argument.name.as_deref())
                .unwrap_or("value");
            format!("<{}>", hint.trim_start_matches('-'))
        });
        if argument.kind == "named"
            && let Some(name) = &argument.name
        {
            words.push(name.clone());
        }
        words.push(value);
    }
    words
}

/// The `login` step for a remote that declared required headers, if it has any.
pub fn token_steps(name: &str, remote: &ChosenRemote) -> Option<String> {
    let headers = &remote.required_headers;
    match headers.as_slice() {
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

/// `plugbay add <registry-name>` without `--from`: the catalog name to use,
/// `io.github.acme/tracker-mcp` → `tracker`.
pub fn name_from_registry_name(registry_name: &str) -> Option<String> {
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

fn is_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Quotes a word for a POSIX shell unless it is plainly safe, so a printed command means what it
/// shows even when the registry put shell syntax in a package name or argument.
fn shell_word(word: &str) -> String {
    let is_plain = !word.is_empty()
        && word.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '_' | '-'
                )
        });
    if is_plain {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
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
        let remote = choose_remote(&server, "acme", None, false).unwrap();
        assert_eq!(remote.url, "https://mcp.acme.example/mcp");
        assert_eq!(
            token_steps("acme", &remote).as_deref(),
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
        assert!(token_steps("linear-readonly", &chosen).is_none());

        let unknown = choose_remote(&server, "lin", Some("https://evil.example/mcp"), false);
        assert!(unknown.is_err());
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
        let message = why_no_remote(&server, "linear");
        assert!(
            message.contains("  plugbay add linear-local --env"),
            "{message}"
        );
        assert!(message.contains("linear is built in"), "{message}");
    }

    #[test]
    fn pypi_and_oci_packages_get_commands_and_bundles_are_skipped() {
        let server = fixture_server("org.pyco/finder");
        let commands: Vec<String> = server
            .packages
            .iter()
            .filter_map(|package| package_command("finder", package))
            .collect();
        assert_eq!(
            commands,
            [
                "plugbay add finder -- uvx pyco-finder==1.2.0",
                "plugbay add finder --env 'FINDER_TOKEN=${keychain:finder/FINDER_TOKEN}' -- docker run -i --rm -e FINDER_TOKEN ghcr.io/pyco/finder:1.2.0",
            ]
        );
    }

    #[test]
    fn shell_syntax_from_the_registry_is_quoted() {
        assert_eq!(shell_word("@acme/tracker@1.0.0"), "@acme/tracker@1.0.0");
        assert_eq!(shell_word("$(touch pwned)"), "'$(touch pwned)'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("<workspace>"), "'<workspace>'");
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
