//! `plugbay tools`, `call` and `restart`: what an agent started here sees and does, from the
//! command line. `tools` and `call` open a real session on the daemon, the way the shim does.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock,
    Implementation, JsonObject, ResourceContents, Tool,
};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

use crate::commands::access::find_server;
use crate::commands::server_account;
use crate::identity::one_safe_line;
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, ControlRequest};
use crate::session::{self, STATUS_TOOL, TOOL_SEPARATOR};
use crate::shim;

const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DESCRIPTION_CHARS: usize = 120;

pub async fn tools(paths: &Paths, cd: Option<PathBuf>, server: Option<&str>) -> anyhow::Result<()> {
    let session = AgentSession::open(paths, cd).await?;
    let status = session.wait_while_connecting(server).await?;
    let mut tools = session.list_tools().await?;
    session.close().await;

    if let Some(server) = server {
        let prefix = format!("{server}{TOOL_SEPARATOR}");
        tools.retain(|tool| tool.name.starts_with(&prefix));
        if tools.is_empty() && !status.servers.iter().any(|line| line.name() == server) {
            bail!(
                "no server `{server}` in this session; run `plugbay tools` to see every server, or `plugbay use {server}` to pin it here"
            );
        }
    }
    print!("{}", tool_table(&tools));

    let unavailable: Vec<&ServerLine> = status
        .servers
        .iter()
        .filter(|line| !line.is_connected())
        .filter(|line| server.is_none_or(|server| line.name() == server))
        .collect();
    if !unavailable.is_empty() {
        println!("\nnot available:");
        for line in unavailable {
            println!("  {}", line.text);
        }
    }
    if !status.problems.is_empty() {
        println!("\nconfig problems (`plugbay doctor` explains them):");
        for problem in &status.problems {
            println!("  {problem}");
        }
    }
    Ok(())
}

pub async fn call(
    paths: &Paths,
    cd: Option<PathBuf>,
    tool: &str,
    arguments: Option<&str>,
) -> anyhow::Result<()> {
    let Some((server, _)) = tool.split_once(TOOL_SEPARATOR) else {
        bail!(
            "tools are named <server>__<tool>, e.g. `echo__whoami`; run `plugbay tools` to list them"
        );
    };
    let arguments = parse_arguments(arguments)?;

    let session = AgentSession::open(paths, cd).await?;
    session.wait_while_connecting(Some(server)).await?;
    let result = session.call(tool, arguments).await;
    session.close().await;
    let result = result?;

    let output = render_result(&result);
    if result.is_error == Some(true) {
        eprint!("{output}");
        bail!("{tool} returned an error");
    }
    print!("{output}");
    Ok(())
}

pub async fn restart(
    paths: &Paths,
    cd: Option<PathBuf>,
    target: Option<&str>,
) -> anyhow::Result<()> {
    let only = match target {
        Some(target) => Some(server_account(&find_server(paths, cd, target).await?)),
        None => None,
    };
    match protocol::send_control(paths, ControlRequest::Reconnect { only }).await {
        Ok(response) => {
            println!(
                "{}; {} connected agent session(s) reconnect on their own",
                response.message, response.sessions
            );
            Ok(())
        }
        Err(error) if daemon_is_not_running(&error) => {
            println!("the daemon is not running, so there is nothing to restart");
            Ok(())
        }
        Err(error) => Err(error).context(
            "the daemon did not answer; if it is older than this plugbay, run `plugbay stop` and agents start a new one",
        ),
    }
}

fn daemon_is_not_running(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
    })
}

/// Arguments for `call`: a JSON object given inline, `-` for stdin, or nothing for `{}`.
/// Checked before connecting, so a typo doesn't start the daemon or any upstream.
fn parse_arguments(arguments: Option<&str>) -> anyhow::Result<JsonObject> {
    let text = match arguments {
        None => return Ok(JsonObject::new()),
        Some("-") => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading the arguments from stdin")?;
            text
        }
        Some(text) => text.to_string(),
    };
    match serde_json::from_str(&text) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => bail!(r#"the arguments must be a JSON object, like '{{"text": "hi"}}'"#),
        Err(error) => bail!(
            r#"the arguments are not valid JSON ({error}); pass an object like '{{"text": "hi"}}', or `-` to read it from stdin"#
        ),
    }
}

/// A session on the daemon, opened with the same hello an agent's shim sends. It offers no
/// elicitation, so trust and account prompts never reach it: such servers stay off, as in a
/// client without prompts.
struct AgentSession {
    service: RunningService<RoleClient, ClientConfig>,
}

impl AgentSession {
    async fn open(paths: &Paths, cd: Option<PathBuf>) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().ok();
        let hints = ProjectHints {
            explicit: cd.or_else(|| cwd.clone()),
            cwd,
            ..ProjectHints::default()
        };
        let stream = shim::open_session(paths, hints).await.context(
            "could not reach the plugbay daemon; run `plugbay daemon --log-level debug` to see why it does not start",
        )?;
        let client = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("plugbay-cli", protocol::VERSION),
        );
        let service = client
            .serve(stream.into_split())
            .await
            .context("the MCP handshake with the daemon failed")?;
        Ok(Self { service })
    }

    /// `initialize` already waited the ready deadline for upstreams; servers still connecting
    /// then get one more, so the output is complete. Only `server` is waited for when given.
    async fn wait_while_connecting(&self, server: Option<&str>) -> anyhow::Result<StatusReport> {
        let deadline = tokio::time::Instant::now() + session::ready_deadline();
        loop {
            let status = StatusReport::parse(&self.call_text(STATUS_TOOL).await?);
            let waiting = status.servers.iter().any(|line| {
                line.is_connecting() && server.is_none_or(|server| line.name() == server)
            });
            if !waiting || tokio::time::Instant::now() >= deadline {
                return Ok(status);
            }
            tokio::time::sleep(STATUS_POLL_INTERVAL).await;
        }
    }

    async fn list_tools(&self) -> anyhow::Result<Vec<Tool>> {
        self.service
            .list_all_tools()
            .await
            .context("listing tools from the daemon")
    }

    async fn call(&self, tool: &str, arguments: JsonObject) -> anyhow::Result<CallToolResult> {
        let request = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        self.service
            .call_tool(request)
            .await
            .with_context(|| format!("calling {tool}"))
    }

    async fn call_text(&self, tool: &str) -> anyhow::Result<String> {
        let result = self.call(tool, JsonObject::new()).await?;
        Ok(text_blocks(&result).join("\n"))
    }

    async fn close(self) {
        if let Err(error) = self.service.cancel().await {
            tracing::debug!(%error, "closing the session");
        }
    }
}

/// One server line of `plugbay__status`, e.g. `echo:client-a (echo-server) — connected`.
#[derive(Debug, PartialEq)]
struct ServerLine {
    label: String,
    state: String,
    text: String,
}

impl ServerLine {
    fn parse(entry: &str) -> Option<Self> {
        let (server, state) = entry.split_once(" — ")?;
        let label = server.split_whitespace().next().unwrap_or(server);
        Some(Self {
            label: label.to_string(),
            state: state.to_string(),
            text: entry.to_string(),
        })
    }

    /// The server name, without the account: what tool names start with.
    fn name(&self) -> &str {
        self.label.split(':').next().unwrap_or(&self.label)
    }

    fn is_connected(&self) -> bool {
        self.state.starts_with("connected")
    }

    fn is_connecting(&self) -> bool {
        self.state.starts_with("connecting")
    }
}

/// What `plugbay__status` says, as far as these commands need it: the lines under `servers:`
/// (which already say why each one is off) and the config problems.
#[derive(Debug, Default, PartialEq)]
struct StatusReport {
    servers: Vec<ServerLine>,
    problems: Vec<String>,
}

impl StatusReport {
    fn parse(status: &str) -> Self {
        let mut report = Self::default();
        let mut in_servers = false;
        for line in status.lines() {
            if line == "servers:" {
                in_servers = true;
                continue;
            }
            if let Some(problem) = line
                .strip_prefix("problem: ")
                .or_else(|| line.strip_prefix("config error: "))
            {
                report.problems.push(problem.to_string());
            }
            let Some(entry) = line.strip_prefix("  ").filter(|_| in_servers) else {
                in_servers = false;
                continue;
            };
            if let Some(server) = ServerLine::parse(entry) {
                report.servers.push(server);
            }
        }
        report
    }
}

/// One row per tool, names aligned, with the first line of its description.
fn tool_table(tools: &[Tool]) -> String {
    let mut rows: Vec<(String, String)> = tools
        .iter()
        .map(|tool| {
            let description = tool.description.as_deref().unwrap_or_default();
            let first_line = description.lines().next().unwrap_or_default();
            (
                one_safe_line(&tool.name, MAX_DESCRIPTION_CHARS),
                one_safe_line(first_line, MAX_DESCRIPTION_CHARS),
            )
        })
        .collect();
    rows.sort();
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let mut table = String::new();
    for (name, description) in rows {
        table.push_str(format!("{name:<width$}  {description}").trim_end());
        table.push('\n');
    }
    table
}

/// The tool's own output as it sent it: text blocks verbatim, other blocks described, and the
/// structured content as pretty JSON (in place of a text block that carries the same JSON).
fn render_result(result: &CallToolResult) -> String {
    let structured = result.structured_content.as_ref();
    let mut structured_shown = false;
    let mut parts = Vec::new();
    for block in &result.content {
        if let (Some(structured), ContentBlock::Text(text)) = (structured, block)
            && serde_json::from_str::<Value>(&text.text).ok().as_ref() == Some(structured)
        {
            parts.push(pretty_json(structured));
            structured_shown = true;
        } else {
            parts.push(describe_block(block));
        }
    }
    if let Some(structured) = structured
        && !structured_shown
    {
        parts.push(pretty_json(structured));
    }
    let mut output = parts.join("\n");
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Content kinds newer than this plugbay, shown as the JSON they came in.
fn json_of(content: &impl serde::Serialize) -> String {
    serde_json::to_string(content).unwrap_or_else(|error| format!("[unreadable content: {error}]"))
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn describe_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => {
            format!(
                "[image, {}, {} base64 bytes]",
                image.mime_type,
                image.data.len()
            )
        }
        ContentBlock::Audio(audio) => {
            format!(
                "[audio, {}, {} base64 bytes]",
                audio.mime_type,
                audio.data.len()
            )
        }
        ContentBlock::Resource(embedded) => match &embedded.resource {
            ResourceContents::TextResourceContents { uri, text, .. } => {
                format!("[resource {uri}]\n{text}")
            }
            ResourceContents::BlobResourceContents { uri, blob, .. } => {
                format!("[resource {uri}, {} base64 bytes]", blob.len())
            }
            other => json_of(other),
        },
        ContentBlock::ResourceLink(link) => format!("[link {}]", link.uri),
        other => json_of(other),
    }
}

fn text_blocks(result: &CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|text| text.text.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const STATUS: &str = "project: /work/app
config files:
  /work/app/plugbay.toml
servers:
  echo:client-a (/bin/echo-server) — connected
  linear:initech (https://mcp.linear.app/mcp) — needs login: call linear__login or run `plugbay login linear:initech` (add `--token` for an API token)
  paper (paper-mcp) — connecting: its tools appear once it is up
  sentry — disabled by /work/plugbay.toml
problem: something odd";

    #[test]
    fn status_server_lines_carry_name_and_state() {
        let lines = StatusReport::parse(STATUS).servers;
        let summary: Vec<(&str, bool, bool)> = lines
            .iter()
            .map(|line| (line.name(), line.is_connected(), line.is_connecting()))
            .collect();
        assert_eq!(
            summary,
            [
                ("echo", true, false),
                ("linear", false, false),
                ("paper", false, true),
                ("sentry", false, false),
            ]
        );
        assert!(lines[1].text.starts_with("linear:initech (https://"));
    }

    #[test]
    fn config_file_lines_are_not_servers() {
        let status = "config files:\n  /odd — name/plugbay.toml\nservers:\n  (none)";
        assert_eq!(StatusReport::parse(status), StatusReport::default());
    }

    #[test]
    fn status_problems_are_collected() {
        let report = StatusReport::parse(STATUS);
        assert_eq!(report.problems, ["something odd"]);
        let broken = StatusReport::parse("project: /work\nconfig error: bad toml");
        assert_eq!(broken.problems, ["bad toml"]);
    }

    #[test]
    fn arguments_must_be_a_json_object() {
        assert!(parse_arguments(None).unwrap().is_empty());
        assert_eq!(
            parse_arguments(Some(r#"{"text": "hi"}"#)).unwrap()["text"],
            "hi"
        );
        let not_object = parse_arguments(Some("[1]")).unwrap_err().to_string();
        assert!(not_object.contains("must be a JSON object"), "{not_object}");
        let broken = parse_arguments(Some("{text: hi}")).unwrap_err().to_string();
        assert!(broken.contains("not valid JSON"), "{broken}");
    }

    fn tool(name: &str, description: &str) -> Tool {
        Tool::new(
            name.to_string(),
            description.to_string(),
            Arc::new(JsonObject::new()),
        )
    }

    #[test]
    fn tool_table_aligns_names_and_keeps_the_first_description_line() {
        let table = tool_table(&[
            tool("plugbay__status", "Show what resolved."),
            tool("echo__echo", "Echo text\nSecond line"),
            tool("echo__evil", "Bell\u{7}\u{202E} here"),
        ]);
        assert_eq!(
            table,
            "echo__echo       Echo text\n\
             echo__evil       Bell here\n\
             plugbay__status  Show what resolved.\n"
        );
    }

    #[test]
    fn structured_content_is_shown_once() {
        let structured = CallToolResult::structured(serde_json::json!({ "count": 2 }));
        assert_eq!(render_result(&structured), "{\n  \"count\": 2\n}\n");

        let mut separate = CallToolResult::success(vec![ContentBlock::text("two items")]);
        separate.structured_content = Some(serde_json::json!({ "count": 2 }));
        assert_eq!(
            render_result(&separate),
            "two items\n{\n  \"count\": 2\n}\n"
        );
    }
}
