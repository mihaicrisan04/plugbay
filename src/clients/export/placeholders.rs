//! The placeholders `export` writes in place of secrets: one per secret, never two secrets under
//! one name, and never a name the destination file already gives a secret of its own.

use std::collections::BTreeSet;

use super::{AUTHORIZATION, ExportFormat};
use crate::secrets::SecretRef;

/// Where plugbay finds a secret here. Refs are pointers, never values, so this is safe to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SecretSource {
    Ref(SecretRef),
    /// The token `plugbay login --token` stored for `server` (`name:account`).
    StoredToken {
        server: String,
    },
    /// A config value exported as a whole, because the text itself may be a secret.
    Literal {
        server: String,
        field: String,
    },
}

/// A value the teammate provides in place of a secret plugbay resolves here.
#[derive(Debug, Clone)]
pub(super) struct Placeholder {
    /// What the file refers to: an environment variable, or a VS Code input id.
    pub name: String,
    pub source: SecretSource,
    pub servers: Vec<String>,
}

#[derive(Debug)]
pub(super) struct Placeholders {
    format: ExportFormat,
    all: Vec<Placeholder>,
    /// Names the destination file already uses for secrets of its own.
    reserved: BTreeSet<String>,
}

impl SecretSource {
    pub fn describe(&self) -> String {
        match self {
            SecretSource::Ref(reference) => reference.describe(),
            SecretSource::StoredToken { server } => {
                format!("the token from `plugbay login {server}`")
            }
            SecretSource::Literal { server, field } => format!("the `{field}` value of {server}"),
        }
    }

    /// An `${env:NAME}` ref already names the teammate's variable, so it keeps that name.
    pub fn environment_variable(&self) -> Option<&str> {
        match self {
            SecretSource::Ref(SecretRef::Env(variable)) => Some(variable),
            _ => None,
        }
    }
}

impl Placeholders {
    pub fn new(format: ExportFormat, reserved: BTreeSet<String>) -> Self {
        Self {
            format,
            all: Vec::new(),
            reserved,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Placeholder> {
        self.all.iter()
    }

    /// The placeholder text for `source` in `server`, reusing the placeholder made for it before.
    pub fn text_for(&mut self, server: &str, source: SecretSource, preferred: &str) -> String {
        let index = self.add(server, source, preferred);
        self.format.placeholder_text(&self.all[index])
    }

    /// Records that `server` uses `source` and returns its index. A new placeholder is named
    /// `preferred` if that's free; an env ref always keeps its variable's name.
    pub fn add(&mut self, server: &str, source: SecretSource, preferred: &str) -> usize {
        let index = match self
            .all
            .iter()
            .position(|placeholder| placeholder.source == source)
        {
            Some(index) => index,
            None => {
                let name = match source.environment_variable() {
                    Some(variable) => variable.to_string(),
                    None => self.unused_name(server, &source, preferred),
                };
                self.all.push(Placeholder {
                    name,
                    source,
                    servers: Vec::new(),
                });
                self.all.len() - 1
            }
        };
        let placeholder = &mut self.all[index];
        if !placeholder.servers.iter().any(|name| name == server) {
            placeholder.servers.push(server.to_string());
        }
        index
    }

    /// `preferred` as the format spells it; numbered (`preferred_2`) when this server already uses
    /// it, prefixed with the server's name first when another secret owns it.
    fn unused_name(&self, server: &str, source: &SecretSource, preferred: &str) -> String {
        let prompted = self.format.prompts_for(source);
        let spelled = |candidate: &str| {
            if prompted {
                input_id(candidate)
            } else {
                candidate.to_string()
            }
        };
        let base = if self.is_taken_by_another(server, &spelled(preferred), prompted) {
            with_server_prefix(server, preferred)
        } else {
            preferred.to_string()
        };
        let mut name = spelled(&base);
        let mut number = 2;
        while self.is_taken(&name, prompted) {
            name = spelled(&format!("{base}_{number}"));
            number += 1;
        }
        name
    }

    fn is_taken(&self, name: &str, prompted: bool) -> bool {
        self.reserved.contains(name) || self.named(name, prompted).is_some()
    }

    fn is_taken_by_another(&self, server: &str, name: &str, prompted: bool) -> bool {
        if self.reserved.contains(name) {
            return true;
        }
        self.named(name, prompted)
            .is_some_and(|placeholder| !placeholder.servers.iter().any(|other| other == server))
    }

    /// The placeholder called `name` among the inputs (`prompted`) or the environment variables.
    fn named(&self, name: &str, prompted: bool) -> Option<&Placeholder> {
        self.all.iter().find(|placeholder| {
            placeholder.name == name && self.format.prompts_for(&placeholder.source) == prompted
        })
    }
}

/// `SERVER_TOKEN` for `Authorization`, `SERVER_HEADER_NAME` for any other header.
pub(super) fn header_placeholder_name(server: &str, header: &str) -> String {
    if header.eq_ignore_ascii_case(AUTHORIZATION) {
        environment_variable_name(&format!("{server}_token"))
    } else {
        environment_variable_name(&format!("{server}_{header}"))
    }
}

/// Uppercase letters, digits and `_`, not starting with a digit: a name every shell and both
/// clients expand.
pub(super) fn environment_variable_name(text: &str) -> String {
    let mut name: String = text
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.starts_with(|character: char| character.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// `SERVER_preferred`, unless `preferred` already starts with the server's name.
fn with_server_prefix(server: &str, preferred: &str) -> String {
    let prefix = environment_variable_name(&format!("{server}_"));
    if preferred.starts_with(&prefix) {
        preferred.to_string()
    } else {
        format!("{prefix}{preferred}")
    }
}

/// `LINEAR_API_KEY` → `linear-api-key`. Never empty.
fn input_id(name: &str) -> String {
    let id = name
        .trim_start_matches('_')
        .to_ascii_lowercase()
        .replace('_', "-");
    if id.is_empty() {
        "secret".to_string()
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keychain(account: &str) -> SecretSource {
        SecretSource::Ref(SecretRef::Keychain {
            service: "plugbay".to_string(),
            account: account.to_string(),
        })
    }

    #[test]
    fn names_become_environment_variables_and_input_ids() {
        assert_eq!(
            environment_variable_name("my-server_x-api-key"),
            "MY_SERVER_X_API_KEY"
        );
        assert_eq!(environment_variable_name("1password"), "_1PASSWORD");
        assert_eq!(input_id("_1PASSWORD_TOKEN"), "1password-token");
        assert_eq!(input_id("_"), "secret");
    }

    #[test]
    fn a_name_taken_by_the_same_server_is_numbered() {
        let mut placeholders = Placeholders::new(ExportFormat::Claude, BTreeSet::new());
        assert_eq!(
            placeholders.text_for("db", keychain("a"), "DB_URL"),
            "${DB_URL}"
        );
        assert_eq!(
            placeholders.text_for("db", keychain("b"), "DB_URL"),
            "${DB_URL_2}"
        );
    }

    #[test]
    fn a_reserved_name_is_never_given_out() {
        let reserved = BTreeSet::from(["api-key".to_string(), "tool-api-key".to_string()]);
        let mut placeholders = Placeholders::new(ExportFormat::Vscode, reserved);
        assert_eq!(
            placeholders.text_for("tool", keychain("a"), "API_KEY"),
            "${input:tool-api-key-2}"
        );
    }

    #[test]
    fn inputs_and_environment_variables_are_separate_names_in_vscode() {
        let mut placeholders = Placeholders::new(ExportFormat::Vscode, BTreeSet::new());
        let environment = SecretSource::Ref(SecretRef::Env("API_KEY".to_string()));
        assert_eq!(
            placeholders.text_for("tool", environment, ""),
            "${env:API_KEY}"
        );
        assert_eq!(
            placeholders.text_for("tool", keychain("a"), "API_KEY"),
            "${input:api-key}"
        );
    }
}
