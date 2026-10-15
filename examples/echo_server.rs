//! A tiny stdio MCP server for plugbay's end-to-end tests.
//! `whoami` reports `ECHO_ACCOUNT` and the process id, so tests can see which account and
//! which shared upstream process answered.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeResult,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};

struct EchoServer;

impl ServerHandler for EchoServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            tool("whoami", "Report the account and process id"),
            tool("echo", "Echo the `text` argument"),
            tool("delete_everything", "A tool tests filter out"),
            tool(
                "huge",
                "Answer with more data than plugbay accepts in one message",
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let text = match request.name.as_ref() {
            "whoami" => {
                let account = std::env::var("ECHO_ACCOUNT").unwrap_or_else(|_| "unset".to_string());
                format!("account={account} pid={}", std::process::id())
            }
            "echo" => request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("text"))
                .and_then(|text| text.as_str())
                .unwrap_or_default()
                .to_string(),
            "huge" => "x".repeat(20 * 1024 * 1024),
            other => format!("called {other}"),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

fn tool(name: &'static str, description: &'static str) -> Tool {
    let mut schema = JsonObject::new();
    schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    Tool::new(name, description, Arc::new(schema))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = EchoServer.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
