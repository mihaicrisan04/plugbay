//! `plugbay tools`, `call` and `restart`: what an agent started here sees and does, from the
//! command line. `tools` and `call` open a real session on the daemon, the way the shim does.

use std::io::{IsTerminal, Read};
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
use crate::commands::credentials_changed;
use crate::identity::{one_safe_line, safe_for_terminal};
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::protocol::{self, ControlRequest, SessionStatus, StatusServer, StatusState};
use crate::session::{self, STATUS_TOOL, TOOL_SEPARATOR};
use crate::shim;

const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DESCRIPTION_CHARS: usize = 120;

pub async fn tools(paths: &Paths, cd: Option<PathBuf>, server: Option<&str>) -> anyhow::Result<()> {
    let session = AgentSession::open(paths, cd).await?;
    let listed = session.status_and_tools(server).await;
    session.close().await;
    let (mut status, mut tools) = listed?;

    if let Some(server) = server {
        status.servers.retain(|line| is_named(line, server));
        // The server's own prefixes, every account's, and `server` itself for `plugbay__*`.
        let mut prefixes = vec![server];
        prefixes.extend(status.servers.iter().map(|line| line.tool_prefix.as_str()));
        tools.retain(|tool| {
            tool_prefix(&tool.name).is_some_and(|prefix| prefixes.contains(&prefix))
        });
        if tools.is_empty() && status.servers.is_empty() {
            bail!(
                "no server `{server}` in this session; run `plugbay tools` to see every server, or `plugbay use {server}` to pin it here"
            );
        }
    }
    print!("{}", tool_table(&tools));

    let (disabled, unavailable): (Vec<&StatusServer>, Vec<&StatusServer>) = status
        .servers
        .iter()
        .filter(|line| !line.state.is_usable())
        .partition(|line| line.state == StatusState::Disabled);
    print_section(
        "not available:",
        unavailable.iter().map(|line| line.status_line.as_str()),
    );
    print_section(
        "disabled by config:",
        disabled.iter().map(|line| line.status_line.as_str()),
    );
    print_section(
        "config problems (`plugbay doctor` explains them):",
        status.problems.iter().map(String::as_str),
    );
    Ok(())
}

pub async fn call(
    paths: &Paths,
    cd: Option<PathBuf>,
    tool: &str,
    arguments: Option<&str>,
) -> anyhow::Result<()> {
    let Some(prefix) = tool_prefix(tool) else {
        bail!(
            "tools are named <server>__<tool>, e.g. `echo__whoami`; run `plugbay tools` to list them"
        );
    };
    let arguments = parse_arguments(arguments)?;

    let session = AgentSession::open(paths, cd).await?;
    let result = session.call_once_connected(prefix, tool, arguments).await;
    session.close().await;
    let result = result?;

    let output = render_result(&result);
    if result.is_error == Some(true) {
        eprint!("{}", for_stream(output, std::io::stderr().is_terminal()));
        bail!("{tool} returned an error");
    }
    print!("{}", for_stream(output, std::io::stdout().is_terminal()));
    Ok(())
}

pub async fn restart(
    paths: &Paths,
    cd: Option<PathBuf>,
    target: Option<&str>,
) -> anyhow::Result<()> {
    let request = match target {
        Some(target) => credentials_changed(&find_server(paths, cd, target).await?),
        None => ControlRequest::ReconnectAll,
    };
    if let shim::Handover::Retired { old_version } = shim::retire_older_daemon(paths).await {
        println!(
            "the daemon ran plugbay {old_version} and handed over; agents reconnect to a new daemon, which connects every server afresh"
        );
        return Ok(());
    }
    match protocol::send_control(paths, request).await {
        Ok(response) => {
            match response.sessions {
                0 => println!("{}", response.message),
                sessions => println!(
                    "{}; {sessions} connected agent session(s) reconnect on their own",
                    response.message
                ),
            }
            Ok(())
        }
        Err(error) if protocol::daemon_is_not_running(&error) => {
            println!("the daemon is not running, so there is nothing to restart");
            Ok(())
        }
        Err(error) => Err(error).context(
            "the daemon did not answer; if it is older than this plugbay, run `plugbay stop` (agents started before the upgrade then need a restart)",
        ),
    }
}

/// `tools <server>` covers every account of `server`, or one account by its tool prefix.
fn is_named(line: &StatusServer, wanted: &str) -> bool {
    line.name == wanted || line.tool_prefix == wanted
}

fn tool_prefix(tool_name: &str) -> Option<&str> {
    tool_name
        .split_once(TOOL_SEPARATOR)
        .map(|(prefix, _)| prefix)
}

/// A blank line, `heading`, then the lines indented; nothing when there are none. Status lines
/// can quote an upstream's error, so they are made safe for the terminal too.
fn print_section<'a>(heading: &str, lines: impl IntoIterator<Item = &'a str>) {
    let mut lines = lines.into_iter().peekable();
    if lines.peek().is_none() {
        return;
    }
    println!("\n{heading}");
    for line in lines {
        println!("  {}", one_safe_line(line, usize::MAX));
    }
}

/// A tool's output as it sent it when redirected; on a terminal, without the control sequences
/// that could rewrite the screen or the window title.
fn for_stream(output: String, is_terminal: bool) -> String {
    if is_terminal {
        safe_for_terminal(&output)
    } else {
        output
    }
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

    async fn status_and_tools(
        &self,
        server: Option<&str>,
    ) -> anyhow::Result<(SessionStatus, Vec<Tool>)> {
        let status = self
            .wait_while_connecting(|line| server.is_none_or(|server| is_named(line, server)))
            .await?;
        Ok((status, self.list_tools().await?))
    }

    async fn call_once_connected(
        &self,
        prefix: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> anyhow::Result<CallToolResult> {
        self.wait_while_connecting(|line| line.tool_prefix == prefix)
            .await?;
        self.call(tool, arguments).await
    }

    /// `initialize` already waited the ready deadline for upstreams; the servers `wanted` picks
    /// that are still connecting then get one more, so the output is complete.
    async fn wait_while_connecting(
        &self,
        wanted: impl Fn(&StatusServer) -> bool,
    ) -> anyhow::Result<SessionStatus> {
        let deadline = tokio::time::Instant::now() + session::ready_deadline();
        loop {
            let status = self.status().await?;
            let waiting = status
                .servers
                .iter()
                .any(|line| line.state == StatusState::Connecting && wanted(line));
            if !waiting || tokio::time::Instant::now() >= deadline {
                return Ok(status);
            }
            tokio::time::sleep(STATUS_POLL_INTERVAL).await;
        }
    }

    async fn status(&self) -> anyhow::Result<SessionStatus> {
        let result = self.call(STATUS_TOOL, JsonObject::new()).await?;
        SessionStatus::from_meta(result.meta.as_ref())
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

    async fn close(self) {
        if let Err(error) = self.service.cancel().await {
            tracing::debug!(%error, "closing the session");
        }
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
                one_safe_line(&tool.name, usize::MAX),
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

/// The tool's own output as it sent it: text blocks verbatim, other blocks described, then the
/// structured content once, as pretty JSON (text blocks that only repeat it are left out).
fn render_result(result: &CallToolResult) -> String {
    let structured = result.structured_content.as_ref();
    let mut parts: Vec<String> = result
        .content
        .iter()
        .filter(|block| !repeats_structured(block, structured))
        .map(describe_block)
        .collect();
    if let Some(structured) = structured {
        parts.push(pretty_json(structured));
    }
    let mut output = parts.join("\n");
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn repeats_structured(block: &ContentBlock, structured: Option<&Value>) -> bool {
    let (Some(structured), ContentBlock::Text(text)) = (structured, block) else {
        return false;
    };
    serde_json::from_str::<Value>(&text.text).is_ok_and(|value| &value == structured)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

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
    fn long_tool_names_are_not_cut() {
        let name = format!("echo__{}", "x".repeat(MAX_DESCRIPTION_CHARS));
        assert_eq!(
            tool_table(&[tool(&name, "Long")]),
            format!("{name}  Long\n")
        );
    }

    #[test]
    fn structured_content_is_shown_once_after_the_other_blocks() {
        let structured = CallToolResult::structured(serde_json::json!({ "count": 2 }));
        assert_eq!(render_result(&structured), "{\n  \"count\": 2\n}\n");

        let mut separate = CallToolResult::success(vec![
            ContentBlock::text(r#"{"count": 2}"#),
            ContentBlock::text("two items"),
        ]);
        separate.structured_content = Some(serde_json::json!({ "count": 2 }));
        assert_eq!(
            render_result(&separate),
            "two items\n{\n  \"count\": 2\n}\n"
        );
    }

    #[test]
    fn tool_output_loses_escape_sequences_only_on_a_terminal() {
        let output = "\u{1b}]0;owned\u{7}\u{1b}[2Jline\n\tnext\n".to_string();
        assert_eq!(for_stream(output.clone(), false), output);
        assert_eq!(for_stream(output, true), "]0;owned[2Jline\n\tnext\n");
    }
}
