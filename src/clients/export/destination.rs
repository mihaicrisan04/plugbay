//! The file `export --write` merges into: what it already holds, the names it gives its own
//! secrets, and the merge itself.

use std::collections::BTreeSet;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use super::{Export, ExportFormat, Placeholder};

/// The file the export merges into: empty when printing, or when the file doesn't exist yet.
#[derive(Debug, Default)]
pub(super) struct Destination {
    document: Map<String, Value>,
    /// Replace entries of the same name instead of keeping them.
    pub force: bool,
}

/// What the export does with each server against the destination.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MergeReport {
    pub added: Vec<String>,
    pub replaced: Vec<String>,
    pub kept: Vec<String>,
}

impl Destination {
    /// `text` is the file's contents, `None` when there is none. Refuses what `merge` can't keep.
    pub fn parse(text: Option<&str>, format: ExportFormat, force: bool) -> anyhow::Result<Self> {
        let document = match text.map(str::trim) {
            None | Some("") => Map::new(),
            Some(text) => match serde_json::from_str(text)
                .context("parsing it as JSON (comments are not supported)")?
            {
                Value::Object(document) => document,
                _ => bail!("expected a JSON object at the top level"),
            },
        };
        let servers_key = format.servers_key();
        if document
            .get(servers_key)
            .is_some_and(|servers| !servers.is_object())
        {
            bail!("`{servers_key}` is not an object");
        }
        if format == ExportFormat::Vscode
            && document
                .get("inputs")
                .is_some_and(|inputs| !inputs.is_array())
        {
            bail!("`inputs` is not a list");
        }
        Ok(Self { document, force })
    }

    fn servers(&self, format: ExportFormat) -> Option<&Map<String, Value>> {
        self.document
            .get(format.servers_key())
            .and_then(Value::as_object)
    }

    pub fn has_server(&self, format: ExportFormat, name: &str) -> bool {
        self.servers(format)
            .is_some_and(|servers| servers.contains_key(name))
    }

    /// Names the file already gives secrets, which no new placeholder may take: every VS Code
    /// input id (VS Code may hold an answer under it), and the variables entries that stay read.
    /// Nothing proves a name holds the same secret, so none is reused.
    pub fn reserved_names(&self, format: ExportFormat, replaced: &[String]) -> BTreeSet<String> {
        let mut expansions = Vec::new();
        for (name, entry) in self.servers(format).into_iter().flatten() {
            if !replaced.contains(name) {
                collect_expansions(entry, &mut expansions);
            }
        }
        let mut reserved = BTreeSet::new();
        for expansion in &expansions {
            let name = match format {
                ExportFormat::Claude => expansion.split(":-").next(),
                ExportFormat::Vscode => expansion.strip_prefix("input:"),
            };
            reserved.extend(name.map(str::to_string));
        }
        if format == ExportFormat::Vscode {
            let inputs = self.document.get("inputs").and_then(Value::as_array);
            for input in inputs.into_iter().flatten() {
                if let Some(id) = input.get("id").and_then(Value::as_str) {
                    reserved.insert(id.to_string());
                }
            }
        }
        reserved
    }
}

/// The text inside every `${...}` in the strings of `value`.
fn collect_expansions(value: &Value, expansions: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            let mut rest = text.as_str();
            while let Some(start) = rest.find("${") {
                let after_open = &rest[start + 2..];
                let Some(end) = after_open.find('}') else {
                    break;
                };
                expansions.push(after_open[..end].to_string());
                rest = &after_open[end + 1..];
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_expansions(item, expansions);
            }
        }
        Value::Object(fields) => {
            for field in fields.values() {
                collect_expansions(field, expansions);
            }
        }
        _ => {}
    }
}

impl MergeReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.replaced.is_empty() && self.kept.is_empty()
    }

    pub fn changes_file(&self) -> bool {
        !self.added.is_empty() || !self.replaced.is_empty()
    }

    pub fn print(&self, file: &str) {
        if self.changes_file() {
            println!("Wrote {file}");
        } else {
            println!("{file} unchanged");
        }
        if !self.added.is_empty() {
            println!("  added {}", self.added.join(", "));
        }
        if !self.replaced.is_empty() {
            println!("  replaced {}", self.replaced.join(", "));
        }
        if !self.kept.is_empty() {
            println!(
                "  kept the existing {} (use --force to replace)",
                self.kept.join(", ")
            );
        }
    }
}

impl Export {
    /// The destination with the entries written in; everything else in it is left as it was.
    pub fn merge(&self, destination: Destination) -> anyhow::Result<String> {
        let mut document = destination.document;
        if self.format == ExportFormat::Vscode {
            self.add_inputs(&mut document);
        }
        let servers = document
            .entry(self.format.servers_key())
            .or_insert_with(|| Value::Object(Map::new()));
        // `Destination::parse` refused a file where this isn't an object.
        if let Value::Object(servers) = servers {
            for entry in &self.entries {
                servers.insert(entry.name.clone(), entry.entry.clone());
            }
        }

        let mut contents =
            serde_json::to_string_pretty(&Value::Object(document)).context("rendering the JSON")?;
        contents.push('\n');
        Ok(contents)
    }

    /// A password prompt for every input the entries use; their ids are all new to the file.
    fn add_inputs(&self, document: &mut Map<String, Value>) {
        let prompted: Vec<&Placeholder> = self
            .placeholders
            .iter()
            .filter(|placeholder| self.format.prompts_for(&placeholder.source))
            .collect();
        if prompted.is_empty() {
            return;
        }
        let inputs = document
            .entry("inputs")
            .or_insert_with(|| Value::Array(Vec::new()));
        // `Destination::parse` refused a file where this isn't a list.
        let Value::Array(inputs) = inputs else {
            return;
        };
        for placeholder in prompted {
            inputs.push(json!({
                "type": "promptString",
                "id": placeholder.name,
                "description": format!("{} for {}", placeholder.name, placeholder.servers.join(", ")),
                "password": true,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{DEFAULT_ACCOUNT, ResolvedConfig, ResolvedServer, ServerTransport};

    fn linear() -> ResolvedServer {
        ResolvedServer::test_http("linear", DEFAULT_ACCOUNT, "https://mcp.linear.app/mcp")
    }

    /// A local server reading `API_KEY` from the keychain.
    fn keyed(name: &str) -> ResolvedServer {
        let mut server = ResolvedServer::test_stdio(name, DEFAULT_ACCOUNT, name, &[]);
        if let ServerTransport::Stdio { env, .. } = &mut server.transport {
            env.insert(
                "API_KEY".to_string(),
                format!("${{keychain:plugbay/{name}}}"),
            );
        }
        server
    }

    /// The export merged into `existing`, and the merged file.
    fn merged(
        servers: Vec<ResolvedServer>,
        format: ExportFormat,
        existing: &str,
        force: bool,
    ) -> (Export, String) {
        let config = ResolvedConfig::test_with(Vec::new(), servers);
        let destination = Destination::parse(Some(existing), format, force).unwrap();
        let export = Export::build(&config, &BTreeMap::new(), format, &destination);
        let contents = export.merge(destination).unwrap();
        (export, contents)
    }

    const EXISTING_MCP_JSON: &str = r#"{
  "mcpServers": {
    "linear": { "type": "http", "url": "https://linear.example/mcp" },
    "mine": { "command": "mine" }
  },
  "custom": true
}"#;

    #[test]
    fn merge_keeps_same_named_entries_and_unrelated_keys() {
        let (export, contents) = merged(
            vec![linear(), keyed("tool")],
            ExportFormat::Claude,
            EXISTING_MCP_JSON,
            false,
        );
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            merged["mcpServers"]["linear"]["url"],
            "https://linear.example/mcp"
        );
        assert_eq!(merged["custom"], true);
        assert_eq!(
            export.report,
            MergeReport {
                added: vec!["tool".to_string()],
                replaced: Vec::new(),
                kept: vec!["linear".to_string()],
            }
        );
    }

    #[test]
    fn merge_keeps_the_order_of_existing_keys() {
        let (_, contents) = merged(
            vec![keyed("tool")],
            ExportFormat::Claude,
            EXISTING_MCP_JSON,
            false,
        );
        let positions: Vec<usize> = ["\"linear\"", "\"mine\"", "\"tool\"", "\"custom\""]
            .iter()
            .map(|key| contents.find(key).unwrap())
            .collect();
        assert!(positions.is_sorted(), "{contents}");
    }

    #[test]
    fn force_replaces_same_named_entries_in_place() {
        let (export, contents) = merged(
            vec![linear()],
            ExportFormat::Claude,
            EXISTING_MCP_JSON,
            true,
        );
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            merged["mcpServers"]["linear"]["url"],
            "https://mcp.linear.app/mcp"
        );
        assert_eq!(export.report.replaced, ["linear"]);
        assert!(
            contents.find("\"linear\"") < contents.find("\"mine\""),
            "{contents}"
        );
    }

    #[test]
    fn merge_never_binds_a_new_secret_to_an_existing_input() {
        let existing = r#"{
  "inputs": [{ "type": "promptString", "id": "api-key", "description": "mine" }],
  "servers": { "mine": { "command": "mine", "env": { "KEY": "${input:api-key}" } } }
}"#;
        let server = keyed("tool");
        let (_, contents) = merged(vec![server], ExportFormat::Vscode, existing, false);
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            merged["servers"]["tool"]["env"]["API_KEY"],
            "${input:tool-api-key}"
        );
        assert_eq!(merged["inputs"][0]["description"], "mine");
        assert_eq!(merged["inputs"][1]["id"], "tool-api-key");
    }

    #[test]
    fn merge_never_binds_a_new_secret_to_a_variable_a_kept_entry_reads() {
        let existing = r#"{ "mcpServers": { "mine": { "command": "mine", "env": { "API_KEY": "${API_KEY:-none}" } } } }"#;
        let server = keyed("tool");
        let (_, contents) = merged(vec![server], ExportFormat::Claude, existing, false);
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            merged["mcpServers"]["tool"]["env"]["API_KEY"],
            "${TOOL_API_KEY}"
        );
    }

    #[test]
    fn a_replaced_entry_frees_the_variables_it_read() {
        let existing = r#"{ "mcpServers": { "tool": { "command": "old", "env": { "API_KEY": "${API_KEY}" } } } }"#;
        let server = keyed("tool");
        let (_, contents) = merged(vec![server], ExportFormat::Claude, existing, true);
        let merged: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(merged["mcpServers"]["tool"]["env"]["API_KEY"], "${API_KEY}");
    }

    #[test]
    fn kept_entries_add_no_inputs() {
        let existing = r#"{ "servers": { "tool": { "type": "stdio", "command": "tool" } } }"#;
        let (export, contents) = merged(vec![keyed("tool")], ExportFormat::Vscode, existing, false);
        assert!(!export.report.changes_file());
        assert!(!contents.contains("inputs"), "{contents}");
    }

    #[test]
    fn merge_refuses_files_it_cannot_read_as_json() {
        let format = ExportFormat::Vscode;
        assert!(Destination::parse(Some("// comment\n{}"), format, false).is_err());
        assert!(Destination::parse(Some(r#"{ "servers": [] }"#), format, false).is_err());
        assert!(Destination::parse(Some(r#"{ "inputs": {} }"#), format, false).is_err());
    }
}
