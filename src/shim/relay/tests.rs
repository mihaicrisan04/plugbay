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
        let read = tokio::time::timeout(Duration::from_secs(5), self.stream.read_line(&mut line))
            .await
            .expect("nothing arrived")
            .unwrap();
        assert!(read > 0, "the other side hung up");
        serde_json::from_str(&line).unwrap()
    }

    async fn send_raw(&mut self, line: &[u8]) {
        self.stream.get_mut().write_all(line).await.unwrap();
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
            let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
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

/// A notification line of exactly `bytes` bytes.
fn notification_of(bytes: usize) -> Vec<u8> {
    let line = |data: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/message","params":{{"level":"info","data":"{data}"}}}}"#
        ) + "\n"
    };
    let padding = bytes - line("").len();
    line(&"x".repeat(padding)).into_bytes()
}

/// Sends the agent, which never reads, all its stdout budget can hold but `free` bytes.
async fn fill_stdout_budget(daemon: &mut Peer, free: usize) {
    let total = BUFFERED_BYTES_PER_DIRECTION - free;
    for size in [total / 2, total - total / 2] {
        daemon.send_raw(&notification_of(size)).await;
    }
    settle().await;
}

/// Runs `test`, which leaves the relay waiting for room on a full stdout, then closes the
/// agent's stdin and checks the relay ends soon after. The agent keeps stdout open and never
/// reads it, and the daemon `test` returns stays connected, so neither frees the budget.
async fn ends_soon_after_the_agent_hangs_up(
    test: impl AsyncFnOnce(&mut Peer, Peer, &mut Daemons) -> Option<Peer>,
) {
    let kept = std::sync::Mutex::new(Vec::new());
    let hung_up = std::sync::OnceLock::new();
    with_relay(
        async |mut agent: Peer, daemon: Peer, mut daemons: Daemons| {
            let connected = test(&mut agent, daemon, &mut daemons).await;
            settle().await;
            agent.hang_up().await;
            hung_up.set(Instant::now()).unwrap();
            kept.lock().unwrap().push((agent, daemons, connected));
        },
    )
    .await;
    let ended_after = hung_up.get().unwrap().elapsed();
    assert!(
        ended_after < Duration::from_secs(5),
        "the relay ended {ended_after:?} after the agent hung up"
    );
}

#[tokio::test]
async fn an_agent_that_stops_reading_and_leaves_during_a_move_ends_the_shim() {
    ends_soon_after_the_agent_hangs_up(async |agent: &mut Peer, mut daemon, _| {
        agent.send(request(1, "tools/list")).await;
        assert_eq!(daemon.receive().await["id"], 1);
        fill_stdout_budget(&mut daemon, 0).await;
        drop(daemon);
        None
    })
    .await;
}

#[tokio::test]
async fn an_agent_that_stops_reading_and_leaves_with_a_full_queue_ends_the_shim() {
    ends_soon_after_the_agent_hangs_up(async |agent: &mut Peer, mut daemon, daemons| {
        fill_stdout_budget(&mut daemon, 0).await;
        drop(daemon);
        daemons.hellos.recv().await.unwrap();
        let padded = |id: u32| {
            let padding = "x".repeat(MAX_QUEUED_BYTES / 2 + 1);
            json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": padding } })
        };
        agent.send(padded(1)).await;
        agent.send(padded(2)).await;
        None
    })
    .await;
}

#[tokio::test]
async fn an_agent_that_stops_reading_and_leaves_before_list_changed_ends_the_shim() {
    ends_soon_after_the_agent_hangs_up(async |agent: &mut Peer, mut daemon, daemons| {
        agent.send(initialize()).await;
        assert_eq!(daemon.receive().await["method"], "initialize");
        daemon.send(initialize_result(&json!(0))).await;
        assert_eq!(agent.receive().await["id"], 0);
        let filler = notification_of(1000);
        fill_stdout_budget(&mut daemon, filler.len()).await;
        drop(daemon);

        let (mut daemon, _) = daemons.next().await;
        let replayed = daemon.receive().await;
        // The reply takes the room the filler waits for, and gives it back once swallowed.
        let mut reply_then_filler = format!("{}\n", initialize_result(&replayed["id"]));
        reply_then_filler.push_str(std::str::from_utf8(&filler).unwrap());
        daemon.send_raw(reply_then_filler.as_bytes()).await;
        Some(daemon)
    })
    .await;
}

#[tokio::test]
async fn a_line_over_the_cap_ends_its_reader() {
    let (mut writer, reader) = tokio::io::duplex(PIPE_BYTES);
    let budget = Arc::new(Semaphore::new(BUFFERED_BYTES_PER_DIRECTION));
    let mut lines = spawn_line_reader(reader, budget, PartialLine::Keep).lines;
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
    let mut lines = spawn_line_reader(reader, budget, PartialLine::Keep).lines;
    writer.write_all(b"first\nsecond\n").await.unwrap();

    let first = lines.recv().await.unwrap();
    assert_eq!(first.bytes, b"first\n");
    settle().await;
    assert!(lines.try_recv().is_err(), "read past the budget");
    drop(first);
    assert_eq!(lines.recv().await.unwrap().bytes, b"second\n");
}
