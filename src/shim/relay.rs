//! Relays the agent's stdio to the daemon line by line, and when the daemon goes away (an
//! upgrade, `plugbay stop`, a crash) moves the session to a new one without the agent noticing
//! more than a `tools/list_changed` and errors for the requests that were in flight.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rmcp::model::RequestId;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, WriteHalf,
};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::conversation::{self, Conversation, Handshake, ListChanges, Route};
use super::{
    FIRST_RETRY_DELAY, HANDSHAKE_TIMEOUT, MAX_RETRY_DELAY, RECONNECT_DEADLINE, Reconnects, Session,
};
use crate::config::SessionPin;
use crate::paths::Paths;
use crate::project::ProjectHints;

/// Lines buffered between each reader and the relay; readers also wait for the byte budget.
const LINE_BUFFER: usize = 16;
/// The daemon's own cap on one message. A longer line ends the side that sent it.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// What the agent may send while no daemon is connected, as much as a daemon session queues.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Bytes read and not yet written, per direction: reader channel, reconnect queue and the agent's
/// stdout writer. A reader waits for its share, so with the line it is still reading (at most
/// [`MAX_LINE_BYTES`]) a direction never holds more than this plus 16 MiB.
const BUFFERED_BYTES_PER_DIRECTION: usize = MAX_QUEUED_BYTES + MAX_LINE_BYTES + 1;
const QUEUE_FULL_MESSAGE: &str =
    "plugbay is reconnecting to its daemon and can't queue more; retry";
const CLIENT_FLUSH_WAIT: Duration = Duration::from_secs(1);
/// A daemon that keeps closing the session (say it rejects the agent's `initialize`) would
/// otherwise be reconnected to forever.
const MAX_MOVES_PER_WINDOW: usize = 5;
const MOVES_WINDOW: Duration = Duration::from_secs(60);

type Pins = BTreeMap<String, SessionPin>;

pub async fn run(paths: &Paths, hints: ProjectHints, stream: UnixStream) {
    let connect = async |pins| {
        let session = Session {
            hints: hints.clone(),
            reconnects: Reconnects::Yes,
            pins,
        };
        super::connect_as_session(paths, session).await
    };
    relay(tokio::io::stdin(), tokio::io::stdout(), stream, connect).await;
}

/// Runs until the agent leaves or no daemon can be reached any more. `connect` opens the next
/// daemon session, with the pins to carry over in its hello.
async fn relay<S>(
    agent_in: impl AsyncRead + Unpin + Send + 'static,
    agent_out: impl AsyncWrite + Unpin + Send + 'static,
    daemon: S,
    connect: impl AsyncFnMut(Pins) -> anyhow::Result<S>,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (to_client, client_writer) = ToClient::spawn(agent_out);
    let client_budget = Arc::new(Semaphore::new(BUFFERED_BYTES_PER_DIRECTION));
    let (from_client, client_reader) =
        spawn_line_reader(agent_in, client_budget, PartialLine::Keep);
    let mut relay = Relay {
        connect,
        conversation: Conversation::default(),
        from_client,
        daemon: DaemonLink::new(daemon, to_client.budget.clone()),
        to_client,
        recent_moves: VecDeque::new(),
    };
    relay.relay().await;
    // The daemon ends the session (and connections it is still making) on seeing the hangup.
    relay.daemon.hang_up().await;
    // Let the errors for lost requests reach the agent before the shim exits.
    drop(relay);
    client_reader.abort();
    if tokio::time::timeout(CLIENT_FLUSH_WAIT, client_writer)
        .await
        .is_err()
    {
        tracing::debug!("the agent did not read our last messages");
    }
}

struct Relay<S, F> {
    connect: F,
    conversation: Conversation,
    from_client: mpsc::Receiver<Line>,
    to_client: ToClient,
    daemon: DaemonLink<S>,
    recent_moves: VecDeque<Instant>,
}

impl<S, F> Relay<S, F>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    F: AsyncFnMut(Pins) -> anyhow::Result<S>,
{
    async fn relay(&mut self) {
        loop {
            tokio::select! {
                line = self.from_client.recv() => {
                    let Some(line) = line else {
                        tracing::info!("the agent hung up");
                        return;
                    };
                    self.send_to_daemon(line).await;
                }
                line = self.daemon.lines.recv() => {
                    let carry_on = match line {
                        Some(line) => self.send_to_client(line),
                        None => self.move_to_new_daemon().await,
                    };
                    if !carry_on {
                        return;
                    }
                }
            }
        }
    }

    /// A failed write means the daemon is gone: its reader sees that next, and the move answers
    /// the line's request.
    async fn send_to_daemon(&mut self, line: Line) {
        let route = self.conversation.client_sent(&line.bytes);
        let Some(line) = line.routed(route) else {
            return;
        };
        if let Err(error) = self.daemon.send(&line.bytes).await {
            tracing::debug!(%error, "writing to the daemon failed");
        }
    }

    /// Whether the agent is still there to receive it.
    fn send_to_client(&mut self, line: Line) -> bool {
        let route = self.conversation.daemon_sent(&line.bytes);
        match line.routed(route) {
            Some(line) => self.to_client.forward(line),
            None => true,
        }
    }

    /// Finds or starts a daemon and brings it to where the lost one was. False when the agent
    /// left meanwhile or no daemon came up in time, which ends the shim.
    async fn move_to_new_daemon(&mut self) -> bool {
        tracing::info!("lost the daemon connection; moving the session to a new daemon");
        if self.moved_too_often() {
            tracing::error!(
                "the daemon closed this session {MAX_MOVES_PER_WINDOW} times within {}s; ending it",
                MOVES_WINDOW.as_secs()
            );
            return false;
        }
        let lost = self.conversation.daemon_lost();
        if !lost.is_empty() {
            tracing::info!(
                count = lost.len(),
                "told the agent about requests the lost daemon will not finish"
            );
        }
        for line in lost {
            if !self.to_client.send_new(line).await {
                return false;
            }
        }

        let handshake = self.conversation.handshake();
        let pins = self.conversation.pins().clone();
        let budget = self.to_client.budget.clone();
        let connect = &mut self.connect;
        let reconnecting = async {
            let mut delay = FIRST_RETRY_DELAY;
            let mut attempt = 0;
            loop {
                attempt += 1;
                tokio::time::sleep(delay).await;
                let replayed =
                    connect_and_replay(&mut *connect, pins.clone(), &handshake, &budget).await;
                match replayed {
                    Ok(connected) => return connected,
                    Err(error) => tracing::info!(
                        attempt,
                        error = format!("{error:#}"),
                        "could not reach a new daemon yet"
                    ),
                }
                delay = (delay * 2).min(MAX_RETRY_DELAY);
            }
        };
        let mut queued = Queued::default();
        let reconnected = queue_client_until(
            tokio::time::timeout(RECONNECT_DEADLINE, reconnecting),
            &mut self.from_client,
            &mut self.conversation,
            &self.to_client,
            &mut queued,
        )
        .await;
        match reconnected {
            None => false,
            Some(Err(_elapsed)) => {
                tracing::error!(
                    "no daemon came up within {}s; ending the session",
                    RECONNECT_DEADLINE.as_secs()
                );
                false
            }
            Some(Ok(connected)) => self.resume(connected, queued).await,
        }
    }

    fn moved_too_often(&mut self) -> bool {
        let now = Instant::now();
        while let Some(&oldest) = self.recent_moves.front()
            && now.duration_since(oldest) > MOVES_WINDOW
        {
            self.recent_moves.pop_front();
        }
        self.recent_moves.push_back(now);
        self.recent_moves.len() > MAX_MOVES_PER_WINDOW
    }

    async fn resume(&mut self, connected: Connected<S>, queued: Queued) -> bool {
        let Connected {
            link,
            early_lines,
            list_changes,
        } = connected;
        self.daemon = link;
        for line in early_lines {
            if !self.send_to_client(line) {
                return false;
            }
        }
        for line in list_changes.notifications() {
            if !self.to_client.send_new(line).await {
                return false;
            }
        }
        let queued_count = queued.lines.len();
        for line in queued.lines {
            if let Err(error) = self.daemon.send(&line.bytes).await {
                // The queued requests are in flight already, so the next move answers the rest.
                tracing::info!(%error, "the new daemon left while plugbay sent what the agent queued");
                break;
            }
        }
        tracing::info!(queued = queued_count, "the session moved to the new daemon");
        true
    }
}

/// Runs `connecting` while queueing what the agent sends. `None` when the agent left.
async fn queue_client_until<T>(
    connecting: impl Future<Output = T>,
    from_client: &mut mpsc::Receiver<Line>,
    conversation: &mut Conversation,
    to_client: &ToClient,
    queued: &mut Queued,
) -> Option<T> {
    tokio::pin!(connecting);
    loop {
        tokio::select! {
            biased;
            line = from_client.recv() => {
                let Some(line) = line else {
                    tracing::info!("the agent hung up while plugbay was reconnecting");
                    return None;
                };
                if !queued.push(line, conversation, to_client).await {
                    return None;
                }
            }
            result = &mut connecting => return Some(result),
        }
    }
}

/// A daemon connection with its replayed handshake done.
struct Connected<S> {
    link: DaemonLink<S>,
    /// Daemon messages that came before the reply to the replayed `initialize`.
    early_lines: Vec<Line>,
    list_changes: ListChanges,
}

async fn connect_and_replay<S>(
    connect: &mut impl AsyncFnMut(Pins) -> anyhow::Result<S>,
    pins: Pins,
    handshake: &Handshake,
    budget: &Arc<Semaphore>,
) -> anyhow::Result<Connected<S>>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let stream = connect(pins).await?;
    let mut link = DaemonLink::new(stream, budget.clone());
    tracing::info!("connected to a daemon");
    let mut early_lines = Vec::new();
    let mut list_changes = ListChanges::default();
    if let Some((line, id)) = &handshake.replayed_initialize {
        link.send(line).await.context("replaying initialize")?;
        let answered = tokio::time::timeout(HANDSHAKE_TIMEOUT, link.reply_to(id, &mut early_lines))
            .await
            .context("the daemon did not answer the replayed initialize in time")?;
        let reply = answered.context("the daemon hung up before answering initialize")?;
        list_changes = conversation::initialize_reply(&reply.bytes).map_err(|refusal| {
            anyhow::anyhow!("the daemon refused the replayed initialize: {refusal}")
        })?;
        tracing::info!("replayed initialize");
    }
    for line in &handshake.after_reply {
        link.send(line).await.context("replaying the handshake")?;
    }
    Ok(Connected {
        link,
        early_lines,
        list_changes,
    })
}

/// A line read from one side, holding its share of that direction's byte budget until it is
/// written or dropped.
struct Line {
    bytes: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}

impl Line {
    /// The line to send on, if any.
    fn routed(mut self, route: Route) -> Option<Self> {
        match route {
            Route::Forward => Some(self),
            Route::Rewritten(bytes) => {
                self.bytes = bytes;
                Some(self)
            }
            Route::Drop => None,
        }
    }
}

/// What the agent sent while no daemon was connected, for the next one. Lines are routed as they
/// arrive, against what the lost daemon left, so its requests are in flight from then on.
#[derive(Default)]
struct Queued {
    lines: Vec<Line>,
    bytes: usize,
}

impl Queued {
    /// Queues `line`, or when the queue is full answers it with an error (requests) or drops it.
    /// False when the agent is gone.
    async fn push(
        &mut self,
        line: Line,
        conversation: &mut Conversation,
        to_client: &ToClient,
    ) -> bool {
        if self.bytes + line.bytes.len() > MAX_QUEUED_BYTES {
            let Some(id) = conversation::request_id(&line.bytes) else {
                tracing::warn!(
                    "dropped a message from the agent: too much queued while reconnecting"
                );
                return true;
            };
            tracing::warn!("refused a request from the agent: too much queued while reconnecting");
            return to_client
                .send_new(conversation::retry_error(id, QUEUE_FULL_MESSAGE))
                .await;
        }
        let route = conversation.client_sent(&line.bytes);
        if let Some(line) = line.routed(route) {
            self.bytes += line.bytes.len();
            self.lines.push(line);
        }
        true
    }
}

struct DaemonLink<S> {
    lines: mpsc::Receiver<Line>,
    writer: WriteHalf<S>,
    reader: JoinHandle<()>,
}

impl<S> DaemonLink<S>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    fn new(stream: S, budget: Arc<Semaphore>) -> Self {
        let (read_half, writer) = tokio::io::split(stream);
        let (lines, reader) = spawn_line_reader(read_half, budget, PartialLine::Drop);
        Self {
            lines,
            writer,
            reader,
        }
    }

    async fn send(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(line).await
    }

    async fn hang_up(&mut self) {
        if let Err(error) = self.writer.shutdown().await {
            tracing::debug!(%error, "the daemon connection was already closed");
        }
    }

    /// Waits for the response to `id`, keeping what comes before it. `None` when the daemon left.
    async fn reply_to(&mut self, id: &RequestId, early_lines: &mut Vec<Line>) -> Option<Line> {
        while let Some(line) = self.lines.recv().await {
            if conversation::response_id(&line.bytes).as_ref() == Some(id) {
                return Some(line);
            }
            early_lines.push(line);
        }
        None
    }
}

impl<S> Drop for DaemonLink<S> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// The agent's stdout, written by a task of its own, so an agent slow to read never holds up
/// what it sends. The daemon's reader waits for budget instead.
struct ToClient {
    lines: mpsc::UnboundedSender<Line>,
    budget: Arc<Semaphore>,
}

impl ToClient {
    fn spawn(mut writer: impl AsyncWrite + Unpin + Send + 'static) -> (Self, JoinHandle<()>) {
        let (sender, mut lines) = mpsc::unbounded_channel::<Line>();
        let task = tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                let written = async {
                    writer.write_all(&line.bytes).await?;
                    writer.flush().await
                };
                if let Err(error) = written.await {
                    tracing::debug!(%error, "the agent stopped reading");
                    break;
                }
            }
        });
        let to_client = Self {
            lines: sender,
            budget: Arc::new(Semaphore::new(BUFFERED_BYTES_PER_DIRECTION)),
        };
        (to_client, task)
    }

    /// False when the agent is gone.
    fn forward(&self, line: Line) -> bool {
        self.lines.send(line).is_ok()
    }

    /// Sends a message of the shim's own, within the same budget.
    async fn send_new(&self, bytes: Vec<u8>) -> bool {
        let Some(budget) = acquire(&self.budget, bytes.len()).await else {
            return false;
        };
        self.forward(Line {
            bytes,
            _budget: budget,
        })
    }
}

async fn acquire(budget: &Arc<Semaphore>, bytes: usize) -> Option<OwnedSemaphorePermit> {
    let bytes = u32::try_from(bytes).ok()?;
    budget.clone().acquire_many_owned(bytes).await.ok()
}

/// What to do with bytes after the last newline when the stream ends. A daemon's are a message
/// cut short, which would corrupt the next one; an agent's go on as they would have before.
#[derive(Clone, Copy)]
enum PartialLine {
    Keep,
    Drop,
}

fn spawn_line_reader<R>(
    reader: R,
    budget: Arc<Semaphore>,
    partial: PartialLine,
) -> (mpsc::Receiver<Line>, JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (lines, received) = mpsc::channel(LINE_BUFFER);
    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        while let Some(bytes) = read_line(&mut reader, partial).await {
            let Some(budget) = acquire(&budget, bytes.len()).await else {
                break;
            };
            let line = Line {
                bytes,
                _budget: budget,
            };
            if lines.send(line).await.is_err() {
                break;
            }
        }
    });
    (received, task)
}

/// The next line, or `None` at the end: EOF, a read error, a line over [`MAX_LINE_BYTES`], or for
/// [`PartialLine::Drop`] a last line cut short.
async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    partial: PartialLine,
) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let limit = MAX_LINE_BYTES as u64 + 1;
    match (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut line)
        .await
    {
        Ok(0) => None,
        Ok(_) if line.ends_with(b"\n") => Some(line),
        Ok(_) if line.len() > MAX_LINE_BYTES => {
            tracing::warn!(
                "a message is longer than plugbay's {} MiB cap; closing that side",
                MAX_LINE_BYTES / (1024 * 1024)
            );
            None
        }
        Ok(_) => match partial {
            PartialLine::Keep => Some(line),
            PartialLine::Drop => {
                tracing::debug!(bytes = line.len(), "dropped a message cut short");
                None
            }
        },
        Err(error) => {
            tracing::debug!(%error, "reading failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::DuplexStream;

    const PIPE_BYTES: usize = 64 * 1024;
    const TEST_TIMEOUT: Duration = Duration::from_secs(20);

    /// The far end of a duplex: the agent, or a daemon.
    struct Peer {
        stream: BufReader<DuplexStream>,
    }

    impl Peer {
        fn pair() -> (Self, DuplexStream) {
            let (ours, theirs) = tokio::io::duplex(PIPE_BYTES);
            (
                Self {
                    stream: BufReader::new(ours),
                },
                theirs,
            )
        }

        async fn send(&mut self, message: Value) {
            let line = format!("{message}\n");
            self.stream
                .get_mut()
                .write_all(line.as_bytes())
                .await
                .unwrap();
        }

        async fn receive(&mut self) -> Value {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(5), self.stream.read_line(&mut line))
                    .await
                    .expect("nothing arrived")
                    .unwrap();
            assert!(read > 0, "the other side hung up");
            serde_json::from_str(&line).unwrap()
        }

        async fn hang_up(&mut self) {
            self.stream.get_mut().shutdown().await.unwrap();
        }
    }

    /// Daemons the relay reaches on a move, and the pins each reconnect hello carried.
    struct Daemons {
        streams: mpsc::UnboundedSender<DuplexStream>,
        hellos: mpsc::UnboundedReceiver<Pins>,
    }

    impl Daemons {
        fn new() -> (Self, impl AsyncFnMut(Pins) -> anyhow::Result<DuplexStream>) {
            let (streams, mut waiting) = mpsc::unbounded_channel();
            let (said_hello, hellos) = mpsc::unbounded_channel();
            let connect = async move |pins| {
                said_hello.send(pins).unwrap();
                waiting.recv().await.context("no daemon left")
            };
            (Self { streams, hellos }, connect)
        }

        /// The next daemon, once the relay is asking for one.
        async fn next(&mut self) -> (Peer, Pins) {
            let pins = self.hellos.recv().await.unwrap();
            let (daemon, shim_end) = Peer::pair();
            self.streams.send(shim_end).unwrap();
            (daemon, pins)
        }
    }

    /// Runs the relay between an agent and a first daemon while `test` drives both.
    async fn with_relay(test: impl AsyncFnOnce(Peer, Peer, Daemons)) {
        let (agent, agent_end) = Peer::pair();
        let (daemon, daemon_end) = Peer::pair();
        let (daemons, connect) = Daemons::new();
        let (agent_in, agent_out) = tokio::io::split(agent_end);
        let both = async {
            tokio::join!(
                relay(agent_in, agent_out, daemon_end, connect),
                test(agent, daemon, daemons)
            )
        };
        tokio::time::timeout(TEST_TIMEOUT, both)
            .await
            .expect("the relay never ended");
    }

    fn request(id: u32, method: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {} })
    }

    fn initialize() -> Value {
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" },
            },
        })
    }

    fn initialize_result(id: &Value) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "protocolVersion": "2025-11-25",
                "serverInfo": { "name": "plugbay", "version": "0.3.0" },
                "capabilities": { "tools": { "listChanged": true } },
            },
        })
    }

    /// Long enough for a line sent now to reach the relay before a daemon is handed over.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn a_message_sent_during_a_move_reaches_the_new_daemon_after_the_handshake() {
        with_relay(
            async |mut agent: Peer, mut daemon: Peer, mut daemons: Daemons| {
                agent.send(initialize()).await;
                assert_eq!(daemon.receive().await["method"], "initialize");
                daemon.send(initialize_result(&json!(0))).await;
                assert_eq!(agent.receive().await["id"], 0);
                let initialized =
                    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
                agent.send(initialized.clone()).await;
                assert_eq!(daemon.receive().await, initialized);

                drop(daemon);
                let pins = daemons.hellos.recv().await.unwrap();
                assert!(pins.is_empty());
                agent.send(request(1, "tools/list")).await;
                settle().await;
                let (mut daemon, shim_end) = Peer::pair();
                daemons.streams.send(shim_end).unwrap();

                let replayed = daemon.receive().await;
                assert_eq!(replayed["method"], "initialize");
                assert_ne!(replayed["id"], 0);
                daemon.send(initialize_result(&replayed["id"])).await;
                assert_eq!(daemon.receive().await, initialized);
                assert_eq!(
                    daemon.receive().await["id"],
                    1,
                    "the queued request comes last"
                );
                assert_eq!(
                    agent.receive().await["method"],
                    "notifications/tools/list_changed",
                    "the replayed reply never reaches the agent"
                );
                daemon
                    .send(json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } }))
                    .await;
                assert_eq!(agent.receive().await["id"], 1);
                agent.hang_up().await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn a_late_answer_to_the_lost_daemons_prompt_never_reaches_the_next_one() {
        with_relay(async |mut agent: Peer, mut daemon: Peer, mut daemons: Daemons| {
            daemon.send(request(0, "elicitation/create")).await;
            let first = agent.receive().await;
            drop(daemon);
            let cancelled = agent.receive().await;
            assert_eq!(cancelled["method"], "notifications/cancelled");
            assert_eq!(cancelled["params"]["requestId"], first["id"]);

            let (mut daemon, _) = daemons.next().await;
            daemon.send(request(0, "elicitation/create")).await;
            let second = agent.receive().await;
            assert_ne!(second["id"], first["id"]);
            let answer = |id: &Value, text: &str| {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "action": "accept", "content": { "text": text } } })
            };
            agent.send(answer(&first["id"], "late")).await;
            agent.send(answer(&second["id"], "current")).await;
            let delivered = daemon.receive().await;
            assert_eq!(delivered["id"], 0);
            assert_eq!(delivered["result"]["content"]["text"], "current");
            agent.hang_up().await;
        })
        .await;
    }

    #[tokio::test]
    async fn the_sessions_pins_ride_the_reconnect_hello() {
        with_relay(
            async |mut agent: Peer, mut daemon: Peer, mut daemons: Daemons| {
                let disable = json!({
                    "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": { "name": "plugbay__disable", "arguments": { "server": "echo" } },
                });
                agent.send(disable).await;
                assert_eq!(daemon.receive().await["id"], 3);
                let done = json!({ "jsonrpc": "2.0", "id": 3, "result": { "content": [] } });
                daemon.send(done).await;
                assert_eq!(agent.receive().await["id"], 3);

                drop(daemon);
                let (_daemon, pins) = daemons.next().await;
                assert_eq!(pins, Pins::from([("echo".to_string(), SessionPin::Off)]));
                agent.hang_up().await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn a_full_queue_refuses_requests_with_an_error() {
        with_relay(async |mut agent: Peer, daemon: Peer, mut daemons: Daemons| {
            drop(daemon);
            daemons.hellos.recv().await.unwrap();
            let padded = |id: u32| {
                let padding = "x".repeat(MAX_QUEUED_BYTES / 2 + 1);
                json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": padding } })
            };
            agent.send(padded(1)).await;
            agent.send(padded(2)).await;
            let refused = agent.receive().await;
            assert_eq!(refused["id"], 2);
            assert_eq!(refused["error"]["message"], QUEUE_FULL_MESSAGE);

            let (mut daemon, shim_end) = Peer::pair();
            daemons.streams.send(shim_end).unwrap();
            assert_eq!(daemon.receive().await["id"], 1);
            agent.hang_up().await;
        })
        .await;
    }

    #[tokio::test]
    async fn requests_queued_for_a_daemon_that_left_get_an_error() {
        with_relay(
            async |mut agent: Peer, daemon: Peer, mut daemons: Daemons| {
                drop(daemon);
                daemons.hellos.recv().await.unwrap();
                agent.send(request(7, "tools/list")).await;
                settle().await;
                let (gone, shim_end) = Peer::pair();
                drop(gone);
                daemons.streams.send(shim_end).unwrap();

                let failed = agent.receive().await;
                assert_eq!(failed["id"], 7);
                assert!(
                    failed["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("plugbay restarted"),
                    "{failed}"
                );
                agent.hang_up().await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn a_daemon_that_keeps_closing_the_session_ends_it() {
        with_relay(async |_agent: Peer, daemon: Peer, mut daemons: Daemons| {
            drop(daemon);
            for _ in 0..MAX_MOVES_PER_WINDOW {
                let (daemon, _) = daemons.next().await;
                drop(daemon);
            }
            settle().await;
            assert!(
                daemons.hellos.try_recv().is_err(),
                "no attempt after the cutoff"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn an_agent_that_stops_reading_can_still_send() {
        with_relay(
            async |mut agent: Peer, mut daemon: Peer, _daemons: Daemons| {
                let notification = json!({
                    "jsonrpc": "2.0", "method": "notifications/message",
                    "params": { "level": "info", "data": "x".repeat(1024) },
                });
                for _ in 0..(4 * PIPE_BYTES / 1024) {
                    daemon.send(notification.clone()).await;
                }
                agent.send(request(1, "tools/list")).await;
                assert_eq!(daemon.receive().await["id"], 1);
                agent.hang_up().await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn a_line_over_the_cap_ends_its_reader() {
        let (mut writer, reader) = tokio::io::duplex(PIPE_BYTES);
        let budget = Arc::new(Semaphore::new(BUFFERED_BYTES_PER_DIRECTION));
        let (mut lines, _task) = spawn_line_reader(reader, budget, PartialLine::Keep);
        let flood = tokio::spawn(async move {
            let chunk = vec![b'x'; PIPE_BYTES];
            // Fails once the reader gave up.
            while writer.write_all(&chunk).await.is_ok() {}
        });
        let ended = tokio::time::timeout(TEST_TIMEOUT, lines.recv())
            .await
            .unwrap();
        assert!(ended.is_none(), "no line came out of the flood");
        flood.await.unwrap();
    }

    #[tokio::test]
    async fn a_reader_waits_until_the_bytes_before_it_are_written() {
        let (mut writer, reader) = tokio::io::duplex(PIPE_BYTES);
        let budget = Arc::new(Semaphore::new(10));
        let (mut lines, _task) = spawn_line_reader(reader, budget, PartialLine::Keep);
        writer.write_all(b"first\nsecond\n").await.unwrap();

        let first = lines.recv().await.unwrap();
        assert_eq!(first.bytes, b"first\n");
        settle().await;
        assert!(lines.try_recv().is_err(), "read past the budget");
        drop(first);
        assert_eq!(lines.recv().await.unwrap().bytes, b"second\n");
    }
}
