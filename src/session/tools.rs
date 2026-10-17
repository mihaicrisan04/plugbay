//! The tools a session exposes besides the upstream ones, and how upstream tools are renamed.

use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde::Deserialize;

use crate::config::ResolvedServer;

pub(super) const TOOL_SEPARATOR: &str = "__";
const MAX_TOOL_NAME_LENGTH: usize = 64;
pub(super) const STATUS_TOOL: &str = "plugbay__status";
pub(super) const ENABLE_TOOL: &str = "plugbay__enable";
pub(super) const DISABLE_TOOL: &str = "plugbay__disable";

/// Upstream tools the config's filters allow, renamed `<prefix>__<tool>`.
pub(super) fn exposed_tools(server: &ResolvedServer, upstream_tools: Vec<Tool>) -> Vec<Tool> {
    let mut exposed = Vec::new();
    for mut tool in upstream_tools {
        if !server.tool_filter.allows(&tool.name) {
            continue;
        }
        let name = format!("{}{TOOL_SEPARATOR}{}", server.tool_prefix, tool.name);
        if name.len() > MAX_TOOL_NAME_LENGTH {
            tracing::warn!(tool = %name, "skipping tool: name longer than {MAX_TOOL_NAME_LENGTH} characters");
            continue;
        }
        tool.name = name.into();
        exposed.push(tool);
    }
    exposed
}

pub(super) fn status_tool() -> Tool {
    Tool::new(
        STATUS_TOOL,
        "Show which MCP servers and accounts plugbay resolved for this project, and why any are missing.",
        Arc::new(empty_input_schema()),
    )
}

#[derive(Deserialize)]
pub(super) struct EnableArguments {
    pub(super) server: String,
    pub(super) account: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct DisableArguments {
    pub(super) server: String,
}

pub(super) fn parse_arguments<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, String> {
    let arguments = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

pub(super) fn enable_tool(available: &[String]) -> Tool {
    let description = format!(
        "Turn on an MCP server for this session only (nothing is saved); its tools appear right after. \
         Use only when the human asks for a server. Pass the account the human names; if a server has \
         several accounts and they did not say which, ask. Switching away from the account this \
         project's config sets asks the human first. Available: {}.",
        available.join(", ")
    );
    Tool::new(
        ENABLE_TOOL,
        description,
        Arc::new(object_schema(
            serde_json::json!({
                "server": { "type": "string", "description": "Server name, e.g. `linear`." },
                "account": { "type": "string", "description": "Account label, e.g. `initech`. Defaults to the project's account." }
            }),
            &["server"],
        )),
    )
}

pub(super) fn disable_tool() -> Tool {
    Tool::new(
        DISABLE_TOOL,
        "Turn off an MCP server for the rest of this session (nothing is saved).",
        Arc::new(object_schema(
            serde_json::json!({
                "server": { "type": "string", "description": "Server name, e.g. `linear`." }
            }),
            &["server"],
        )),
    )
}

pub(super) fn login_tool(server: &ResolvedServer) -> Tool {
    Tool::new(
        format!("{}{TOOL_SEPARATOR}login", server.tool_prefix),
        format!(
            "Log in to {} ({}) so its tools become available. Opens the browser for the human to approve.",
            server.display_name(),
            server.transport.target()
        ),
        Arc::new(empty_input_schema()),
    )
}

fn object_schema(properties: serde_json::Value, required: &[&str]) -> JsonObject {
    let mut schema = empty_input_schema();
    schema.insert("properties".to_string(), properties);
    schema.insert("required".to_string(), serde_json::json!(required));
    schema
}

fn empty_input_schema() -> JsonObject {
    let mut schema = JsonObject::new();
    schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    schema
}

pub(super) fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

pub(super) fn error_result(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}
