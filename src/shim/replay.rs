//! What the shim remembers about the MCP conversation between the agent and the daemon, so a
//! session survives the daemon going away: the client's `initialize` and `initialized` to replay
//! on a new daemon, and the requests in flight each way, which a lost daemon will never finish.
//! MCP stdio is one JSON-RPC message (or batch) per line; lines that don't parse are passed
//! through untouched and untracked.

use std::collections::HashSet;

use rmcp::model::RequestId;
use serde::Deserialize;
use serde_json::{Map, Value, json};

const RETRY_MESSAGE: &str = "plugbay restarted while this request was in flight; retry it";
const INTERNAL_ERROR: i64 = -32603;

#[derive(Debug, Default)]
pub struct Replay {
    initialize: Option<Initialize>,
    initialized: Option<Vec<u8>>,
    /// Client requests the daemon hasn't answered, by id.
    client_requests: HashSet<RequestId>,
    /// Daemon requests (elicitation, roots) the client hasn't answered.
    daemon_requests: HashSet<RequestId>,
    /// Daemon requests a lost daemon sent; the client's late answers are dropped.
    abandoned_daemon_requests: HashSet<RequestId>,
    replays: u64,
}

#[derive(Debug)]
struct Initialize {
    message: Map<String, Value>,
    line: Vec<u8>,
    id: RequestId,
    answered: bool,
}

/// Whether a client line goes on to the daemon.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Forward,
    Drop,
}

/// How to bring a new daemon to where the lost one was.
#[derive(Debug, Default)]
pub struct Handshake {
    /// A copy of the client's answered `initialize` under an id of our own, whose reply the
    /// client must never see.
    pub replayed_initialize: Option<(Vec<u8>, RequestId)>,
    /// Sent once the replayed `initialize` is answered (or straight away without one).
    pub then: Vec<Vec<u8>>,
}

/// The lists a daemon's `initialize` reply says it announces changes of.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ListChanges {
    pub tools: bool,
    pub prompts: bool,
    pub resources: bool,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
}

enum Message {
    Request { id: RequestId, method: String },
    Notification { method: String },
    Response { id: RequestId },
}

impl Envelope {
    fn into_message(self) -> Option<Message> {
        match (self.id, self.method) {
            (Some(id), Some(method)) => Some(Message::Request { id, method }),
            (None, Some(method)) => Some(Message::Notification { method }),
            (Some(id), None) => Some(Message::Response { id }),
            (None, None) => None,
        }
    }
}

impl Replay {
    pub fn note_from_client(&mut self, line: &[u8]) -> Route {
        let mut route = Route::Forward;
        for message in parse(line) {
            match message {
                Message::Request { id, method } => {
                    if method == "initialize" {
                        self.remember_initialize(line, &id);
                    }
                    self.client_requests.insert(id);
                }
                Message::Notification { method } if method == "notifications/initialized" => {
                    self.initialized = Some(line.to_vec());
                }
                Message::Notification { method } if method == "notifications/cancelled" => {
                    if let Some(id) = cancelled_request(line) {
                        self.client_requests.remove(&id);
                    }
                }
                Message::Notification { .. } => {}
                Message::Response { id } => {
                    self.daemon_requests.remove(&id);
                    if self.abandoned_daemon_requests.remove(&id) {
                        route = Route::Drop;
                    }
                }
            }
        }
        route
    }

    pub fn note_from_daemon(&mut self, line: &[u8]) {
        for message in parse(line) {
            match message {
                Message::Request { id, .. } => {
                    self.abandoned_daemon_requests.remove(&id);
                    self.daemon_requests.insert(id);
                }
                Message::Notification { method } if method == "notifications/cancelled" => {
                    if let Some(id) = cancelled_request(line) {
                        self.daemon_requests.remove(&id);
                    }
                }
                Message::Notification { .. } => {}
                Message::Response { id } => {
                    self.client_requests.remove(&id);
                    if let Some(initialize) = &mut self.initialize
                        && initialize.id == id
                    {
                        initialize.answered = true;
                    }
                }
            }
        }
    }

    /// The daemon is gone: what to tell the client about requests it will never finish. Client
    /// requests get an error, except an unanswered `initialize`, which the next daemon answers.
    /// Daemon requests are cancelled, and the client's answers to them are dropped.
    pub fn daemon_lost(&mut self) -> Vec<Vec<u8>> {
        let pending_initialize = self
            .initialize
            .as_ref()
            .filter(|initialize| !initialize.answered)
            .map(|initialize| initialize.id.clone());
        let mut lines = Vec::new();
        for id in std::mem::take(&mut self.client_requests) {
            if Some(&id) == pending_initialize.as_ref() {
                self.client_requests.insert(id);
            } else {
                lines.push(retry_error(&id, RETRY_MESSAGE));
            }
        }
        for id in std::mem::take(&mut self.daemon_requests) {
            lines.push(to_line(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": id, "reason": "plugbay restarted" },
            })));
            self.abandoned_daemon_requests.insert(id);
        }
        lines
    }

    pub fn handshake(&mut self) -> Handshake {
        let Some(initialize) = &self.initialize else {
            return Handshake::default();
        };
        let mut handshake = Handshake::default();
        if initialize.answered {
            self.replays += 1;
            let id = RequestId::String(format!("plugbay-replay-{}", self.replays).into());
            let mut message = initialize.message.clone();
            message.insert("id".to_string(), json!(id));
            handshake.replayed_initialize = Some((to_line(&Value::Object(message)), id));
        } else {
            handshake.then.push(initialize.line.clone());
        }
        handshake.then.extend(self.initialized.clone());
        handshake
    }

    fn remember_initialize(&mut self, line: &[u8], id: &RequestId) {
        let Ok(Value::Object(message)) = serde_json::from_slice(line) else {
            return;
        };
        self.initialize = Some(Initialize {
            message,
            line: line.to_vec(),
            id: id.clone(),
            answered: false,
        });
    }
}

/// The id of a request on `line`, so a request that can't be delivered can still be answered.
pub fn request_id(line: &[u8]) -> Option<RequestId> {
    match serde_json::from_slice::<Envelope>(line)
        .ok()?
        .into_message()?
    {
        Message::Request { id, .. } => Some(id),
        _ => None,
    }
}

pub fn retry_error(id: &RequestId, message: &str) -> Vec<u8> {
    to_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": INTERNAL_ERROR, "message": message },
    }))
}

/// Matches the daemon's reply to a replayed `initialize`: `None` for any other line.
pub fn handshake_reply(line: &[u8], id: &RequestId) -> Option<Result<ListChanges, String>> {
    #[derive(Deserialize)]
    struct Reply {
        id: Option<RequestId>,
        result: Option<Value>,
        error: Option<Value>,
    }
    let reply: Reply = serde_json::from_slice(line).ok()?;
    if reply.id.as_ref() != Some(id) {
        return None;
    }
    if let Some(error) = reply.error {
        return Some(Err(error.to_string()));
    }
    let capabilities = reply
        .result
        .as_ref()
        .and_then(|result| result.get("capabilities"));
    let announces = |list: &str| {
        capabilities
            .and_then(|capabilities| capabilities.get(list))
            .and_then(|list| list.get("listChanged"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Some(Ok(ListChanges {
        tools: announces("tools"),
        prompts: announces("prompts"),
        resources: announces("resources"),
    }))
}

impl ListChanges {
    /// The notifications that make the client fetch every list again from the new daemon.
    pub fn notifications(&self) -> Vec<Vec<u8>> {
        [
            (self.tools, "notifications/tools/list_changed"),
            (self.prompts, "notifications/prompts/list_changed"),
            (self.resources, "notifications/resources/list_changed"),
        ]
        .into_iter()
        .filter(|(announced, _)| *announced)
        .map(|(_, method)| to_line(&json!({ "jsonrpc": "2.0", "method": method })))
        .collect()
    }
}

/// The messages on a line: one, several for a batch, none when it isn't JSON-RPC.
fn parse(line: &[u8]) -> Vec<Message> {
    let is_batch = line.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'[');
    let envelopes = if is_batch {
        serde_json::from_slice::<Vec<Envelope>>(line).unwrap_or_default()
    } else {
        serde_json::from_slice::<Envelope>(line)
            .map(|envelope| vec![envelope])
            .unwrap_or_default()
    };
    envelopes
        .into_iter()
        .filter_map(Envelope::into_message)
        .collect()
}

fn cancelled_request(line: &[u8]) -> Option<RequestId> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        request_id: RequestId,
    }
    #[derive(Deserialize)]
    struct Cancelled {
        params: Params,
    }
    let cancelled: Cancelled = serde_json::from_slice(line).ok()?;
    Some(cancelled.params.request_id)
}

fn to_line(message: &Value) -> Vec<u8> {
    let mut line = message.to_string().into_bytes();
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
    const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

    fn lines(lines: Vec<Vec<u8>>) -> Vec<Value> {
        lines
            .iter()
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    fn initialized_session() -> Replay {
        let mut replay = Replay::default();
        replay.note_from_client(INITIALIZE.as_bytes());
        replay.note_from_daemon(br#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        replay.note_from_client(INITIALIZED.as_bytes());
        replay
    }

    #[test]
    fn requests_in_flight_get_a_retry_error_when_the_daemon_is_lost() {
        let mut replay = initialized_session();
        replay.note_from_client(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#);
        replay.note_from_client(br#"{"jsonrpc":"2.0","id":"two","method":"tools/list"}"#);
        replay.note_from_daemon(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);

        let told = lines(replay.daemon_lost());
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0]["id"], "two");
        assert_eq!(told[0]["error"]["code"], INTERNAL_ERROR);
        assert!(replay.daemon_lost().is_empty(), "answered only once");
    }

    #[test]
    fn a_cancelled_request_is_no_longer_in_flight() {
        let mut replay = initialized_session();
        replay.note_from_client(br#"{"jsonrpc":"2.0","id":5,"method":"tools/call"}"#);
        replay.note_from_client(
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}}"#,
        );
        assert!(replay.daemon_lost().is_empty());
    }

    #[test]
    fn the_replayed_initialize_uses_an_id_of_its_own_then_sends_initialized() {
        let mut replay = initialized_session();
        let handshake = replay.handshake();
        let (line, id) = handshake.replayed_initialize.unwrap();
        let message: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(message["method"], "initialize");
        assert_eq!(message["params"]["clientInfo"]["name"], "test");
        assert_eq!(message["id"], json!(id));
        assert_ne!(message["id"], json!(0));
        assert_eq!(
            lines(handshake.then),
            [serde_json::from_str::<Value>(INITIALIZED).unwrap()]
        );

        let (_, next_id) = replay.handshake().replayed_initialize.unwrap();
        assert_ne!(next_id, id, "each replay gets a fresh id");
    }

    #[test]
    fn an_unanswered_initialize_is_resent_as_is_and_not_failed() {
        let mut replay = Replay::default();
        replay.note_from_client(INITIALIZE.as_bytes());
        assert!(replay.daemon_lost().is_empty());

        let handshake = replay.handshake();
        assert!(handshake.replayed_initialize.is_none());
        assert_eq!(handshake.then, [INITIALIZE.as_bytes().to_vec()]);
    }

    #[test]
    fn nothing_to_replay_before_the_client_spoke() {
        let handshake = Replay::default().handshake();
        assert!(handshake.replayed_initialize.is_none() && handshake.then.is_empty());
    }

    #[test]
    fn daemon_requests_are_cancelled_and_late_answers_dropped() {
        let mut replay = initialized_session();
        replay.note_from_daemon(br#"{"jsonrpc":"2.0","id":0,"method":"elicitation/create"}"#);

        let told = lines(replay.daemon_lost());
        assert_eq!(told[0]["method"], "notifications/cancelled");
        assert_eq!(told[0]["params"]["requestId"], 0);
        let late_answer = br#"{"jsonrpc":"2.0","id":0,"result":{"action":"accept"}}"#;
        assert_eq!(replay.note_from_client(late_answer), Route::Drop);
        assert_eq!(
            replay.note_from_client(late_answer),
            Route::Forward,
            "dropped once"
        );
    }

    #[test]
    fn a_new_daemon_reusing_an_abandoned_id_gets_its_answer() {
        let mut replay = initialized_session();
        replay.note_from_daemon(br#"{"jsonrpc":"2.0","id":0,"method":"roots/list"}"#);
        replay.daemon_lost();
        replay.note_from_daemon(br#"{"jsonrpc":"2.0","id":0,"method":"roots/list"}"#);
        assert_eq!(
            replay.note_from_client(br#"{"jsonrpc":"2.0","id":0,"result":{"roots":[]}}"#),
            Route::Forward
        );
    }

    #[test]
    fn batches_track_every_message() {
        let mut replay = initialized_session();
        replay.note_from_client(
            br#"[{"jsonrpc":"2.0","id":7,"method":"ping"},{"jsonrpc":"2.0","id":8,"method":"ping"}]"#,
        );
        replay.note_from_daemon(br#" [{"jsonrpc":"2.0","id":7,"result":{}}]"#);
        let told = lines(replay.daemon_lost());
        assert_eq!(told.len(), 1);
        assert_eq!(told[0]["id"], 8);
    }

    #[test]
    fn malformed_lines_pass_through_untracked() {
        let mut replay = initialized_session();
        for line in [
            &b"not json"[..],
            b"{\"id\":",
            b"[1,2]",
            b"{}",
            b"{\"id\":1.5,\"method\":\"x\"}",
        ] {
            assert_eq!(replay.note_from_client(line), Route::Forward);
            replay.note_from_daemon(line);
        }
        assert!(replay.daemon_lost().is_empty());
    }

    #[test]
    fn the_handshake_reply_says_which_lists_to_refetch() {
        let id = RequestId::String("plugbay-replay-1".into());
        let reply = br#"{"jsonrpc":"2.0","id":"plugbay-replay-1","result":{"capabilities":{"tools":{"listChanged":true},"prompts":{}}}}"#;
        let changes = handshake_reply(reply, &id).unwrap().unwrap();
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
        assert!(handshake_reply(br#"{"jsonrpc":"2.0","id":0,"result":{}}"#, &id).is_none());
        let refused =
            br#"{"jsonrpc":"2.0","id":"plugbay-replay-1","error":{"code":1,"message":"no"}}"#;
        assert!(handshake_reply(refused, &id).unwrap().is_err());
    }

    #[test]
    fn request_id_finds_requests_only() {
        assert_eq!(
            request_id(br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#),
            Some(RequestId::Number(3))
        );
        assert_eq!(request_id(INITIALIZED.as_bytes()), None);
        assert_eq!(request_id(b"garbage"), None);
    }
}
