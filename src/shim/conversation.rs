//! What the shim knows about the MCP conversation between the agent and the daemon, so a session
//! survives the daemon going away: the client's `initialize` and `initialized` to replay on a new
//! daemon, the requests in flight each way, which a lost daemon will never finish, and the pins its
//! `plugbay__enable` / `plugbay__disable` calls left. MCP stdio is one JSON-RPC message per line;
//! lines that aren't one JSON object (MCP 2025-06-18 dropped batches) pass through untracked.
//!
//! Daemon requests (elicitation, roots) reach the client under ids of the shim's own, never reused
//! while it runs, and its answers go back under the daemon's id. An answer to a request a lost
//! daemon sent matches nothing and is dropped, so it can never answer a new daemon's request that
//! reuses the id (approving a trust prompt the human never saw).

use std::collections::{BTreeMap, HashMap};

use rmcp::model::{
    CallToolRequest, CallToolResult, CancelledNotification, ErrorData, InitializeRequest,
    InitializeResult, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    RequestId, ServerCapabilities, ServerJsonRpcMessage, ServerNotification,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::SessionPin;
use crate::session::tools::{DISABLE_TOOL, DisableArguments, ENABLE_TOOL, EnableArguments};

const RETRY_MESSAGE: &str = "plugbay restarted while this request was in flight; retry it";

#[derive(Debug, Default)]
pub struct Conversation {
    initialize: Option<Initialize>,
    initialized: Option<Vec<u8>>,
    /// Client requests the daemon hasn't answered, with the pin each sets if it succeeds.
    client_requests: HashMap<RequestId, Option<PinChange>>,
    /// Daemon requests the client hasn't answered: the id the client knows → the daemon's.
    daemon_requests: HashMap<RequestId, RequestId>,
    next_client_facing_id: i64,
    pins: BTreeMap<String, SessionPin>,
    replays: u64,
}

#[derive(Debug)]
struct Initialize {
    id: RequestId,
    request: InitializeRequest,
    line: Vec<u8>,
    answered: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PinChange {
    server: String,
    pin: SessionPin,
}

/// What becomes of a line on its way.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Forward,
    /// Forward this instead: the same message under another id.
    Rewritten(Vec<u8>),
    Drop,
}

/// How to bring a new daemon to where the lost one was.
#[derive(Debug, Default)]
pub struct Handshake {
    /// A copy of the client's answered `initialize` under an id of our own, whose reply the
    /// client must never see.
    pub replayed_initialize: Option<(Vec<u8>, RequestId)>,
    /// Sent once the replayed `initialize` is answered (or straight away without one).
    pub after_reply: Vec<Vec<u8>>,
}

/// The lists a daemon's `initialize` reply says it announces changes of.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ListChanges {
    pub tools: bool,
    pub prompts: bool,
    pub resources: bool,
}

impl Conversation {
    pub fn client_sent(&mut self, line: &[u8]) -> Route {
        match peek(line) {
            Some(Kind::Request { id, method }) => {
                let pin_change = match method.as_str() {
                    "initialize" => {
                        self.remember_initialize(line, &id);
                        None
                    }
                    "tools/call" => pin_change(line),
                    _ => None,
                };
                self.client_requests.insert(id, pin_change);
                Route::Forward
            }
            Some(Kind::Notification { method }) => {
                if method == "notifications/initialized" {
                    self.initialized = Some(line.to_vec());
                } else if method == "notifications/cancelled"
                    && let Some(cancelled) = parse_cancelled(line)
                    && let Some(id) = &cancelled.notification.params.request_id
                {
                    self.client_requests.remove(id);
                }
                Route::Forward
            }
            Some(Kind::Response { id }) => match self.daemon_requests.remove(&id) {
                Some(daemon_id) => with_id(line, daemon_id).map_or(Route::Drop, Route::Rewritten),
                None => {
                    tracing::debug!(?id, "dropped an answer to a request no daemon waits for");
                    Route::Drop
                }
            },
            None => Route::Forward,
        }
    }

    pub fn daemon_sent(&mut self, line: &[u8]) -> Route {
        match peek(line) {
            Some(Kind::Request { id, .. }) => {
                let client_facing = RequestId::Number(self.next_client_facing_id);
                self.next_client_facing_id += 1;
                let Some(rewritten) = with_id(line, client_facing.clone()) else {
                    return Route::Forward;
                };
                self.daemon_requests.insert(client_facing, id);
                Route::Rewritten(rewritten)
            }
            Some(Kind::Notification { method }) if method == "notifications/cancelled" => {
                self.daemon_cancelled(line)
            }
            Some(Kind::Notification { .. }) | None => Route::Forward,
            Some(Kind::Response { id }) => {
                if let Some(Some(change)) = self.client_requests.remove(&id)
                    && is_successful_tool_call(line)
                {
                    self.pins.insert(change.server, change.pin);
                }
                if let Some(initialize) = &mut self.initialize
                    && initialize.id == id
                {
                    initialize.answered = true;
                }
                Route::Forward
            }
        }
    }

    /// The daemon cancelling a request it sent: the client knows it by our id.
    fn daemon_cancelled(&mut self, line: &[u8]) -> Route {
        let Some(mut cancelled) = parse_cancelled(line) else {
            return Route::Drop;
        };
        let params = &mut cancelled.notification.params;
        let client_facing = self
            .daemon_requests
            .iter()
            .find(|(_, daemon_id)| params.request_id.as_ref() == Some(*daemon_id))
            .map(|(client_facing, _)| client_facing.clone());
        let Some(client_facing) = client_facing else {
            return Route::Drop;
        };
        self.daemon_requests.remove(&client_facing);
        params.request_id = Some(client_facing);
        Route::Rewritten(to_line(&cancelled))
    }

    /// The daemon is gone: what to tell the client about requests it will never finish. Client
    /// requests get an error, except an unanswered `initialize`, which the next daemon answers.
    /// Daemon requests are cancelled; the client's answers to them match nothing any more.
    pub fn daemon_lost(&mut self) -> Vec<Vec<u8>> {
        let pending_initialize = self
            .initialize
            .as_ref()
            .filter(|initialize| !initialize.answered)
            .map(|initialize| initialize.id.clone());
        let mut lines: Vec<Vec<u8>> = self
            .client_requests
            .extract_if(|id, _| Some(id) != pending_initialize.as_ref())
            .map(|(id, _)| retry_error(id, RETRY_MESSAGE))
            .collect();
        for (client_facing, _) in self.daemon_requests.drain() {
            lines.push(cancellation(client_facing));
        }
        lines
    }

    pub fn handshake(&mut self) -> Handshake {
        let mut handshake = Handshake::default();
        let Some(initialize) = &self.initialize else {
            return handshake;
        };
        if initialize.answered {
            self.replays += 1;
            let id = RequestId::String(format!("plugbay-replay-{}", self.replays).into());
            let replayed = JsonRpcRequest::new(id.clone(), initialize.request.clone());
            handshake.replayed_initialize = Some((to_line(&replayed), id));
        } else {
            handshake.after_reply.push(initialize.line.clone());
        }
        handshake.after_reply.extend(self.initialized.clone());
        handshake
    }

    /// What the session's successful `plugbay__enable` / `plugbay__disable` calls set.
    pub fn pins(&self) -> &BTreeMap<String, SessionPin> {
        &self.pins
    }

    /// The daemon parses `initialize` into these same types, so the replay loses nothing it sees.
    fn remember_initialize(&mut self, line: &[u8], id: &RequestId) {
        match serde_json::from_slice::<JsonRpcRequest<InitializeRequest>>(line) {
            Ok(parsed) => {
                self.initialize = Some(Initialize {
                    id: id.clone(),
                    request: parsed.request,
                    line: line.to_vec(),
                    answered: false,
                });
            }
            Err(error) => tracing::debug!(%error, "the agent's initialize does not parse"),
        }
    }
}

/// The routing fields of a line, read without building the rest of a (possibly large) message.
#[derive(Deserialize)]
struct Peek {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
}

enum Kind {
    Request { id: RequestId, method: String },
    Notification { method: String },
    Response { id: RequestId },
}

fn peek(line: &[u8]) -> Option<Kind> {
    let is_object = line.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{');
    if !is_object {
        return None;
    }
    let peeked: Peek = serde_json::from_slice(line).ok()?;
    match (peeked.id, peeked.method) {
        (Some(id), Some(method)) => Some(Kind::Request { id, method }),
        (None, Some(method)) => Some(Kind::Notification { method }),
        (Some(id), None) => Some(Kind::Response { id }),
        (None, None) => None,
    }
}

/// The id of a request on `line`, so a request that can't be delivered can still be answered.
pub fn request_id(line: &[u8]) -> Option<RequestId> {
    match peek(line)? {
        Kind::Request { id, .. } => Some(id),
        Kind::Notification { .. } | Kind::Response { .. } => None,
    }
}

/// The id of a response (or error) on `line`.
pub fn response_id(line: &[u8]) -> Option<RequestId> {
    match peek(line)? {
        Kind::Response { id } => Some(id),
        Kind::Request { .. } | Kind::Notification { .. } => None,
    }
}

/// Tool calls can carry large arguments; only plugbay's own two tools are read whole.
fn pin_change(line: &[u8]) -> Option<PinChange> {
    #[derive(Deserialize)]
    struct ToolName {
        params: ToolNameParams,
    }
    #[derive(Deserialize)]
    struct ToolNameParams {
        name: String,
    }
    let tool: ToolName = serde_json::from_slice(line).ok()?;
    if tool.params.name != ENABLE_TOOL && tool.params.name != DISABLE_TOOL {
        return None;
    }
    let call: JsonRpcRequest<CallToolRequest> = serde_json::from_slice(line).ok()?;
    let arguments = Value::Object(call.request.params.arguments.unwrap_or_default());
    if tool.params.name == ENABLE_TOOL {
        let enable: EnableArguments = serde_json::from_value(arguments).ok()?;
        Some(PinChange {
            server: enable.server,
            pin: SessionPin::On {
                account: enable.account,
            },
        })
    } else {
        let disable: DisableArguments = serde_json::from_value(arguments).ok()?;
        Some(PinChange {
            server: disable.server,
            pin: SessionPin::Off,
        })
    }
}

fn is_successful_tool_call(line: &[u8]) -> bool {
    serde_json::from_slice::<JsonRpcResponse<CallToolResult>>(line)
        .is_ok_and(|response| response.result.is_error != Some(true))
}

fn parse_cancelled(line: &[u8]) -> Option<JsonRpcNotification<CancelledNotification>> {
    serde_json::from_slice(line).ok()
}

/// What the daemon said to the replayed `initialize`: the lists to refetch, or its refusal.
pub fn initialize_reply(line: &[u8]) -> Result<ListChanges, String> {
    if let Ok(reply) = serde_json::from_slice::<JsonRpcResponse<InitializeResult>>(line) {
        return Ok(ListChanges::from(&reply.result.capabilities));
    }
    match serde_json::from_slice::<JsonRpcError>(line) {
        Ok(refusal) => Err(refusal.error.message.to_string()),
        Err(error) => Err(format!("the reply does not parse: {error}")),
    }
}

impl From<&ServerCapabilities> for ListChanges {
    fn from(capabilities: &ServerCapabilities) -> Self {
        Self {
            tools: capabilities
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed == Some(true)),
            prompts: capabilities
                .prompts
                .as_ref()
                .is_some_and(|prompts| prompts.list_changed == Some(true)),
            resources: capabilities
                .resources
                .as_ref()
                .is_some_and(|resources| resources.list_changed == Some(true)),
        }
    }
}

impl ListChanges {
    /// The notifications that make the client fetch every list again from the new daemon.
    pub fn notifications(&self) -> Vec<Vec<u8>> {
        let mut notifications = Vec::new();
        if self.tools {
            notifications.push(ServerNotification::ToolListChangedNotification(
                Default::default(),
            ));
        }
        if self.prompts {
            notifications.push(ServerNotification::PromptListChangedNotification(
                Default::default(),
            ));
        }
        if self.resources {
            notifications.push(ServerNotification::ResourceListChangedNotification(
                Default::default(),
            ));
        }
        notifications
            .into_iter()
            .map(|notification| to_line(&ServerJsonRpcMessage::notification(notification)))
            .collect()
    }
}

pub fn retry_error(id: RequestId, message: &'static str) -> Vec<u8> {
    let error = ErrorData::internal_error(message, None);
    to_line(&ServerJsonRpcMessage::error(error, Some(id)))
}

fn cancellation(id: RequestId) -> Vec<u8> {
    let params = rmcp::model::CancelledNotificationParam::new(
        Some(id),
        Some("plugbay restarted".to_string()),
    );
    let cancelled = ServerNotification::CancelledNotification(CancelledNotification::new(params));
    to_line(&ServerJsonRpcMessage::notification(cancelled))
}

/// `line` with its `id` replaced; the rest is kept as sent.
fn with_id(line: &[u8], id: RequestId) -> Option<Vec<u8>> {
    let mut message: Map<String, Value> = serde_json::from_slice(line).ok()?;
    message.insert("id".to_string(), serde_json::to_value(id).ok()?);
    Some(to_line(&message))
}

fn to_line(message: &impl Serialize) -> Vec<u8> {
    match serde_json::to_vec(message) {
        Ok(mut line) => {
            line.push(b'\n');
            line
        }
        Err(error) => {
            tracing::error!(%error, "could not encode a message for the agent");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

    fn parsed(line: &[u8]) -> Value {
        serde_json::from_slice(line).unwrap()
    }

    fn lines(lines: Vec<Vec<u8>>) -> Vec<Value> {
        lines.iter().map(|line| parsed(line)).collect()
    }

    fn rewritten(route: Route) -> Value {
        match route {
            Route::Rewritten(line) => parsed(&line),
            other => panic!("not rewritten: {other:?}"),
        }
    }

    fn initialized_session() -> Conversation {
        let mut conversation = Conversation::default();
        conversation.client_sent(INITIALIZE.as_bytes());
        conversation.daemon_sent(br#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        conversation.client_sent(INITIALIZED.as_bytes());
        conversation
    }

    fn tool_call(id: u32, tool: &str, arguments: Value) -> Vec<u8> {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        })
        .to_string()
        .into_bytes()
    }

    fn tool_result(id: u32, is_error: bool) -> Vec<u8> {
        json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "content": [{ "type": "text", "text": "done" }], "isError": is_error },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn requests_in_flight_get_a_retry_error_when_the_daemon_is_lost() {
        let mut conversation = initialized_session();
        conversation.client_sent(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#);
        conversation.client_sent(br#"{"jsonrpc":"2.0","id":"two","method":"tools/list"}"#);
        conversation.daemon_sent(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);

        let told = lines(conversation.daemon_lost());
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0]["id"], "two");
        assert_eq!(told[0]["error"]["code"], -32603);
        assert!(conversation.daemon_lost().is_empty(), "answered only once");
    }

    #[test]
    fn a_cancelled_request_is_no_longer_in_flight() {
        let mut conversation = initialized_session();
        conversation.client_sent(br#"{"jsonrpc":"2.0","id":5,"method":"tools/call"}"#);
        conversation.client_sent(
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}"#,
        );
        assert!(conversation.daemon_lost().is_empty());
    }

    #[test]
    fn the_replayed_initialize_uses_an_id_of_its_own_then_sends_initialized() {
        let mut conversation = initialized_session();
        let handshake = conversation.handshake();
        let (line, id) = handshake.replayed_initialize.unwrap();
        let message = parsed(&line);
        assert_eq!(message["method"], "initialize");
        assert_eq!(message["params"]["clientInfo"]["name"], "test");
        assert_eq!(message["params"]["protocolVersion"], "2025-11-25");
        assert_eq!(message["id"], json!(id));
        assert_ne!(message["id"], json!(0));
        assert_eq!(
            lines(handshake.after_reply),
            [serde_json::from_str::<Value>(INITIALIZED).unwrap()]
        );

        let (_, next_id) = conversation.handshake().replayed_initialize.unwrap();
        assert_ne!(next_id, id, "each replay gets a fresh id");
    }

    #[test]
    fn an_unanswered_initialize_is_resent_as_is_and_not_failed() {
        let mut conversation = Conversation::default();
        conversation.client_sent(INITIALIZE.as_bytes());
        assert!(conversation.daemon_lost().is_empty());

        let handshake = conversation.handshake();
        assert!(handshake.replayed_initialize.is_none());
        assert_eq!(handshake.after_reply, [INITIALIZE.as_bytes().to_vec()]);
    }

    #[test]
    fn nothing_to_replay_before_the_client_spoke() {
        let handshake = Conversation::default().handshake();
        assert!(handshake.replayed_initialize.is_none() && handshake.after_reply.is_empty());
    }

    #[test]
    fn a_late_answer_to_a_lost_daemons_prompt_never_answers_the_next_daemons() {
        let mut conversation = initialized_session();
        let prompt = br#"{"jsonrpc":"2.0","id":0,"method":"elicitation/create","params":{}}"#;
        let first = rewritten(conversation.daemon_sent(prompt));
        let told = lines(conversation.daemon_lost());
        assert_eq!(told[0]["method"], "notifications/cancelled");
        assert_eq!(told[0]["params"]["requestId"], first["id"]);

        // The new daemon's first prompt reuses id 0, for another command.
        let second = rewritten(conversation.daemon_sent(prompt));
        assert_ne!(second["id"], first["id"]);
        let answer = |id: &Value| {
            json!({ "jsonrpc": "2.0", "id": id, "result": { "action": "accept" } })
                .to_string()
                .into_bytes()
        };
        assert_eq!(conversation.client_sent(&answer(&first["id"])), Route::Drop);
        let delivered = rewritten(conversation.client_sent(&answer(&second["id"])));
        assert_eq!(delivered["id"], 0);
        assert_eq!(delivered["result"]["action"], "accept");
        assert_eq!(
            conversation.client_sent(&answer(&second["id"])),
            Route::Drop,
            "answered once"
        );
    }

    #[test]
    fn the_daemon_cancelling_its_prompt_reaches_the_client_under_our_id() {
        let mut conversation = initialized_session();
        let prompt = br#"{"jsonrpc":"2.0","id":4,"method":"roots/list"}"#;
        let sent = rewritten(conversation.daemon_sent(prompt));
        let cancelled = br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4,"reason":"timeout"}}"#;
        let told = rewritten(conversation.daemon_sent(cancelled));
        assert_eq!(told["params"]["requestId"], sent["id"]);
        assert_eq!(told["params"]["reason"], "timeout");
        assert!(conversation.daemon_lost().is_empty());
    }

    #[test]
    fn successful_enables_and_disables_become_the_sessions_pins() {
        let mut conversation = initialized_session();
        let enable = tool_call(
            1,
            ENABLE_TOOL,
            json!({ "server": "linear", "account": "work" }),
        );
        conversation.client_sent(&enable);
        conversation.daemon_sent(&tool_result(1, false));
        conversation.client_sent(&tool_call(2, DISABLE_TOOL, json!({ "server": "echo" })));
        conversation.daemon_sent(&tool_result(2, false));
        let refused = tool_call(3, ENABLE_TOOL, json!({ "server": "nope" }));
        conversation.client_sent(&refused);
        conversation.daemon_sent(&tool_result(3, true));
        conversation.client_sent(&tool_call(4, DISABLE_TOOL, json!({ "server": "lost" })));
        conversation.daemon_lost();
        conversation.client_sent(&tool_call(5, "echo__echo", json!({ "server": "other" })));
        conversation.daemon_sent(&tool_result(5, false));

        let expected = BTreeMap::from([
            ("echo".to_string(), SessionPin::Off),
            (
                "linear".to_string(),
                SessionPin::On {
                    account: Some("work".to_string()),
                },
            ),
        ]);
        assert_eq!(conversation.pins(), &expected);
    }

    #[test]
    fn arrays_pass_through_untracked() {
        let mut conversation = initialized_session();
        let batch = br#"[{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{}}]"#;
        assert_eq!(conversation.client_sent(batch), Route::Forward);
        assert_eq!(conversation.daemon_sent(br#"[5,"x"]"#), Route::Forward);
        assert!(conversation.daemon_lost().is_empty());
    }

    #[test]
    fn malformed_lines_pass_through_untracked() {
        let mut conversation = initialized_session();
        for line in [
            &b"not json"[..],
            b"{\"id\":",
            b"[1,2]",
            b"{}",
            b"{\"id\":1.5,\"method\":\"x\"}",
        ] {
            assert_eq!(conversation.client_sent(line), Route::Forward);
            assert_eq!(conversation.daemon_sent(line), Route::Forward);
        }
        assert!(conversation.daemon_lost().is_empty());
    }

    #[test]
    fn the_initialize_reply_says_which_lists_to_refetch() {
        let reply = br#"{"jsonrpc":"2.0","id":"plugbay-replay-1","result":{"protocolVersion":"2025-11-25","serverInfo":{"name":"plugbay","version":"0.3.0"},"capabilities":{"tools":{"listChanged":true},"prompts":{}}}}"#;
        let changes = initialize_reply(reply).unwrap();
        assert_eq!(
            changes,
            ListChanges {
                tools: true,
                ..ListChanges::default()
            }
        );
        assert_eq!(
            lines(changes.notifications()),
            [json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})]
        );
        let refused =
            br#"{"jsonrpc":"2.0","id":"plugbay-replay-1","error":{"code":1,"message":"no"}}"#;
        assert_eq!(initialize_reply(refused), Err("no".to_string()));
        assert!(initialize_reply(b"garbage").is_err());
    }

    #[test]
    fn ids_are_peeked_by_kind() {
        let request = br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#;
        assert_eq!(request_id(request), Some(RequestId::Number(3)));
        assert_eq!(response_id(request), None);
        assert_eq!(request_id(INITIALIZED.as_bytes()), None);
        assert_eq!(
            response_id(br#"{"jsonrpc":"2.0","id":"a","error":{"code":1,"message":"no"}}"#),
            Some(RequestId::String("a".into()))
        );
        assert_eq!(request_id(b"garbage"), None);
    }

    #[test]
    fn errors_are_internal_errors_for_the_request() {
        let error = parsed(&retry_error(RequestId::Number(9), RETRY_MESSAGE));
        assert_eq!(
            error,
            json!({ "jsonrpc": "2.0", "id": 9, "error": { "code": -32603, "message": RETRY_MESSAGE } })
        );
    }
}
