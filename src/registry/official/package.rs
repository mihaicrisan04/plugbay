//! Turns a registry package into the `plugbay add` command a user can review and run. Nothing
//! here runs anything: the command is only printed.

use super::{Argument, EnvironmentVariable, Package};

/// `plugbay add <name> [--env ...] -- <command>` for one package, as words.
#[derive(Debug)]
pub struct PackageCommand {
    pub words: Vec<String>,
    /// Environment variables the package reads but doesn't need, by name.
    pub optional_env: Vec<String>,
    /// Some word holds a `<...>` the user must replace.
    pub has_placeholders: bool,
    /// Some `--env` value is a `${keychain:...}` ref the user must store first.
    pub has_secret_refs: bool,
}

/// The program that runs a package, chosen by its registry type. The registry's own
/// `runtimeHint` is ignored: plugbay picks the runtime, not the entry.
enum Runtime {
    Npx,
    Uvx,
    Docker,
}

impl Runtime {
    fn for_registry_type(registry_type: &str) -> Option<Self> {
        match registry_type {
            "npm" => Some(Self::Npx),
            "pypi" => Some(Self::Uvx),
            "oci" => Some(Self::Docker),
            _ => None,
        }
    }
}

/// The command for a stdio package, or `None` when plugbay doesn't know how to run that kind of
/// package (or its env names aren't plain names).
pub fn package_command(name: &str, package: &Package) -> Option<PackageCommand> {
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
    let runtime = Runtime::for_registry_type(&package.registry_type)?;
    let (required_env, optional_env): (Vec<&EnvironmentVariable>, Vec<&EnvironmentVariable>) =
        package
            .environment_variables
            .iter()
            .partition(|variable| variable.is_required);

    let mut command = PackageCommand {
        words: vec!["plugbay".into(), "add".into(), name.into()],
        optional_env: optional_env
            .iter()
            .map(|variable| variable.name.clone())
            .collect(),
        has_placeholders: false,
        has_secret_refs: false,
    };
    for variable in &required_env {
        let value = command.env_value(name, variable);
        command.words.push("--env".into());
        command.words.push(format!("{}={value}", variable.name));
    }
    command.words.push("--".into());
    command.push_run_command(&runtime, package, &required_env);
    Some(command)
}

impl PackageCommand {
    /// The command as one line to paste into a POSIX shell.
    pub fn command_line(&self) -> String {
        self.words
            .iter()
            .map(|word| shell_word(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A secret becomes a keychain ref, anything else its default or a placeholder.
    fn env_value(&mut self, name: &str, variable: &EnvironmentVariable) -> String {
        if variable.is_secret {
            self.has_secret_refs = true;
            return format!("${{keychain:{name}/{}}}", variable.name);
        }
        match &variable.default_value {
            Some(default) => default.clone(),
            None => {
                self.has_placeholders = true;
                "<value>".to_string()
            }
        }
    }

    fn push_run_command(
        &mut self,
        runtime: &Runtime,
        package: &Package,
        env: &[&EnvironmentVariable],
    ) {
        let identifier = package.identifier.as_str();
        let version = package.version.as_deref();
        let runtime_words = self.argument_words(&package.runtime_arguments);
        match runtime {
            Runtime::Npx => {
                self.words.push("npx".into());
                let confirms_install = runtime_words
                    .iter()
                    .any(|word| word == "-y" || word == "--yes");
                if !confirms_install {
                    self.words.push("-y".into());
                }
                self.words.extend(runtime_words);
                self.words.push(match version {
                    Some(version) => format!("{identifier}@{version}"),
                    None => identifier.to_string(),
                });
            }
            Runtime::Uvx => {
                self.words.push("uvx".into());
                self.words.extend(runtime_words);
                self.words.push(match version {
                    Some(version) => format!("{identifier}=={version}"),
                    None => identifier.to_string(),
                });
            }
            Runtime::Docker => {
                self.words
                    .extend(["docker", "run", "-i", "--rm"].map(String::from));
                self.words.extend(runtime_words);
                for variable in env {
                    self.words.push("-e".into());
                    self.words.push(variable.name.clone());
                }
                self.words.push(image_reference(identifier, version));
            }
        }
        let package_words = self.argument_words(&package.package_arguments);
        self.words.extend(package_words);
    }

    /// Fixed and required arguments as words; optional ones without a value are left out.
    fn argument_words(&mut self, arguments: &[Argument]) -> Vec<String> {
        let mut words = Vec::new();
        for argument in arguments {
            let given = argument.value.clone().or(argument.default_value.clone());
            if given.is_none() && !argument.is_required {
                continue;
            }
            let value = match given {
                Some(value) => value,
                None => {
                    self.has_placeholders = true;
                    placeholder(argument)
                }
            };
            if argument.kind == "named"
                && let Some(name) = &argument.name
            {
                words.push(name.clone());
            }
            words.push(value);
        }
        words
    }
}

fn placeholder(argument: &Argument) -> String {
    let hint = argument
        .value_hint
        .as_deref()
        .or(argument.name.as_deref())
        .unwrap_or("value");
    format!("<{}>", hint.trim_start_matches('-'))
}

/// `image:version`, unless the identifier already pins a tag or digest.
fn image_reference(identifier: &str, version: Option<&str>) -> String {
    let has_tag = identifier
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains(':'))
        || identifier.contains('@');
    match version {
        Some(version) if !has_tag => format!("{identifier}:{version}"),
        _ => identifier.to_string(),
    }
}

fn is_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Quotes a word for a POSIX shell unless it is plainly safe, so a printed command means what it
/// shows even when the registry put shell syntax in a package name, argument or url.
pub fn shell_word(word: &str) -> String {
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
    use crate::registry::official::tests::fixture_server;

    fn command_lines(server_name: &str, name: &str) -> Vec<String> {
        fixture_server(server_name)
            .packages
            .iter()
            .filter_map(|package| package_command(name, package))
            .map(|command| command.command_line())
            .collect()
    }

    #[test]
    fn pypi_and_oci_packages_get_commands_and_bundles_are_skipped() {
        assert_eq!(
            command_lines("org.pyco/finder", "finder"),
            [
                "plugbay add finder -- uvx pyco-finder==1.2.0",
                "plugbay add finder --env 'FINDER_TOKEN=${keychain:finder/FINDER_TOKEN}' -- docker run -i --rm -e FINDER_TOKEN ghcr.io/pyco/finder:1.2.0",
            ]
        );
    }

    #[test]
    fn placeholders_secret_refs_and_optional_env_are_reported_as_flags() {
        let server = fixture_server("io.github.acme-dev/tracker-mcp");
        let command = package_command("tracker", &server.packages[0]).unwrap();
        assert!(command.has_placeholders);
        assert!(command.has_secret_refs);
        assert_eq!(command.optional_env, ["TRACKER_DEBUG"]);

        let finder = fixture_server("org.pyco/finder");
        let plain = package_command("finder", &finder.packages[0]).unwrap();
        assert!(!plain.has_placeholders && !plain.has_secret_refs);
    }

    #[test]
    fn npx_always_confirms_the_install_once() {
        let package: Package = serde_json::from_str(
            r#"{"registryType": "npm", "identifier": "pkg", "runtimeArguments": [{"type": "named", "name": "--node-options", "value": "--max-old-space-size=4096"}]}"#,
        )
        .unwrap();
        let command = package_command("pkg", &package).unwrap().command_line();
        assert_eq!(
            command,
            "plugbay add pkg -- npx -y --node-options --max-old-space-size=4096 pkg"
        );

        let already: Package = serde_json::from_str(
            r#"{"registryType": "npm", "identifier": "pkg", "runtimeArguments": [{"type": "positional", "value": "--yes"}]}"#,
        )
        .unwrap();
        let command = package_command("pkg", &already).unwrap().command_line();
        assert_eq!(command, "plugbay add pkg -- npx --yes pkg");
    }

    #[test]
    fn shell_syntax_from_the_registry_is_quoted() {
        assert_eq!(shell_word("@acme/tracker@1.0.0"), "@acme/tracker@1.0.0");
        assert_eq!(shell_word("$(touch pwned)"), "'$(touch pwned)'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("<workspace>"), "'<workspace>'");
    }
}
