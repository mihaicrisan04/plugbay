//! Relays the agent's stdio to the daemon line by line, and when the daemon goes away (an
//! upgrade, `plugbay stop`, a crash) moves the session to a new one without the agent noticing
//! more than a `tools/list_changed` and errors for the requests that were in flight.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::Reconnects;
use super::replay::{self, Handshake, ListChanges, Replay, Route};
use crate::paths::Paths;
use crate::project::ProjectHints;

/// Lines buffered between each reader and the relay; readers wait when it is full.
const LINE_BUFFER: usize = 16;
const RECONNECT_DEADLINE: Duration = Duration::from_secs(30);
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
/// The daemon answers `initialize` only once upstreams are ready, which takes up to 8 s.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// What the agent may send while no daemon is connected, as much as a daemon session queues.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
const QUEUE_FULL_MESSAGE: &str =
    "plugbay is reconnecting to its daemon and can't queue more; retry";
const CLIENT_FLUSH_WAIT: Duration = Duration::from_secs(1);
/// A daemon that keeps closing the session (say it rejects the agent's `initialize`) would
/// otherwise be reconnected to forever.
const MAX_MOVES_PER_WINDOW: usize = 5;
const MOVES_WINDOW: Duration = Duration::from_secs(60);

pub async fn run(paths: &Paths, hints: ProjectHints, stream: UnixStream) {
    let (to_client, client_writer) = spawn_client_writer();
    let (from_client, _client_reader) = spawn_line_reader(tokio::io::stdin(), PartialLine::Keep);
    let mut relay = Relay {
        paths,
        hints,
        replay: Replay::default(),
        from_client,
        to_client,
        daemon: DaemonLink::new(stream),
        recent_moves: VecDeque::new(),
    };
    relay.relay().await;
    // Let the errors for lost requests reach the agent before the shim exits.
    drop(relay);
    if tokio::time::timeout(CLIENT_FLUSH_WAIT, client_writer)
        .await
        .is_err()
    {
        tracing::debug!("the agent did not read our last messages");
    }
}

struct Relay<'a> {
    paths: &'a Paths,
    hints: ProjectHints,
    replay: Replay,
    from_client: mpsc::Receiver<Vec<u8>>,
    to_client: mpsc::Sender<Vec<u8>>,
    daemon: DaemonLink,
    recent_moves: VecDeque<Instant>,
}

impl Relay<'_> {
    /// Runs until the agent leaves, or no daemon can be reached any more.
    async fn relay(&mut self) {
        loop {
            tokio::select! {
                line = self.from_client.recv() => {
                    let Some(line) = line else {
                        return tracing::info!("the agent hung up");
                    };
                    // A failed write means the daemon is gone; its reader sees that next.
                    self.send_to_daemon(line).await;
                }
                line = self.daemon.lines.recv() => {
                    let delivered = match line {
                        Some(line) => self.send_to_client(line).await,
                        None => self.move_to_new_daemon().await,
                    };
                    if !delivered {
                        return;
                    }
                }
            }
        }
    }

    async fn send_to_daemon(&mut self, line: Vec<u8>) -> bool {
        if self.replay.note_from_client(&line) == Route::Drop {
            tracing::debug!("dropped the agent's answer to a request the lost daemon sent");
            return true;
        }
        match self.daemon.send(&line).await {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, "writing to the daemon failed");
                false
            }
        }
    }

    /// Whether the agent is still there to receive it.
    async fn send_to_client(&mut self, line: Vec<u8>) -> bool {
        self.replay.note_from_daemon(&line);
        self.to_client.send(line).await.is_ok()
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
        let lost = self.replay.daemon_lost();
        if !lost.is_empty() {
            tracing::info!(
                count = lost.len(),
                "told the agent about requests the lost daemon will not finish"
            );
        }
        for line in lost {
            if self.to_client.send(line).await.is_err() {
                return false;
            }
        }

        let handshake = self.replay.handshake();
        let mut queued = Queued::default();
        let deadline = Instant::now() + RECONNECT_DEADLINE;
        let mut delay = FIRST_RETRY_DELAY;
        for attempt in 1.. {
            let connecting = async {
                tokio::time::sleep(delay).await;
                let connect = connect_and_replay(self.paths, &self.hints, &handshake);
                tokio::time::timeout_at(deadline, connect)
                    .await
                    .context("ran out of time")?
            };
            let Some(result) = queue_client_until(
                connecting,
                &mut self.from_client,
                &self.to_client,
                &mut queued,
            )
            .await
            else {
                return false;
            };
            match result {
                Ok(connected) => return self.resume(connected, queued).await,
                Err(error) => tracing::info!(
                    attempt,
                    error = format!("{error:#}"),
                    "could not reach a new daemon yet"
                ),
            }
            if Instant::now() >= deadline {
                break;
            }
            delay = (delay * 2).min(MAX_RETRY_DELAY);
        }
        tracing::error!(
            "no daemon came up within {}s; ending the session",
            RECONNECT_DEADLINE.as_secs()
        );
        false
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

    async fn resume(&mut self, connected: Connected, queued: Queued) -> bool {
        let Connected {
            link,
            early_lines,
            list_changes,
        } = connected;
        self.daemon = link;
        for line in early_lines {
            if !self.send_to_client(line).await {
                return false;
            }
        }
        for line in list_changes.notifications() {
            if self.to_client.send(line).await.is_err() {
                return false;
            }
        }
        let queued_count = queued.lines.len();
        for line in queued.lines {
            if !self.send_to_daemon(line).await {
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
    from_client: &mut mpsc::Receiver<Vec<u8>>,
    to_client: &mpsc::Sender<Vec<u8>>,
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
                if !queued.push(line, to_client).await {
                    return None;
                }
            }
            result = &mut connecting => return Some(result),
        }
    }
}

/// A daemon connection with its replayed handshake done.
struct Connected {
    link: DaemonLink,
    /// Daemon messages that came before the reply to the replayed `initialize`.
    early_lines: Vec<Vec<u8>>,
    list_changes: ListChanges,
}

async fn connect_and_replay(
    paths: &Paths,
    hints: &ProjectHints,
    handshake: &Handshake,
) -> anyhow::Result<Connected> {
    let stream = super::connect_as_session(paths, hints.clone(), Reconnects::Yes).await?;
    let mut link = DaemonLink::new(stream);
    tracing::info!("connected to a daemon");
    let mut early_lines = Vec::new();
    let mut list_changes = ListChanges::default();
    if let Some((line, id)) = &handshake.replayed_initialize {
        link.send(line).await.context("replaying initialize")?;
        let reply = async {
            while let Some(line) = link.lines.recv().await {
                match replay::handshake_reply(&line, id) {
                    Some(reply) => return Ok(reply),
                    None => early_lines.push(line),
                }
            }
            anyhow::bail!("the daemon hung up before answering initialize")
        };
        list_changes = tokio::time::timeout(HANDSHAKE_TIMEOUT, reply)
            .await
            .context("the daemon did not answer the replayed initialize in time")??
            .map_err(|error| {
                anyhow::anyhow!("the daemon refused the replayed initialize: {error}")
            })?;
        tracing::info!("replayed initialize");
    }
    for line in &handshake.then {
        link.send(line).await.context("replaying the handshake")?;
    }
    Ok(Connected {
        link,
        early_lines,
        list_changes,
    })
}

/// What the agent sent while no daemon was connected, for the next one.
#[derive(Default)]
struct Queued {
    lines: Vec<Vec<u8>>,
    bytes: usize,
}

impl Queued {
    /// Queues `line`, or when the queue is full answers it with an error (requests) or drops it.
    /// False when the agent is gone.
    async fn push(&mut self, line: Vec<u8>, to_client: &mpsc::Sender<Vec<u8>>) -> bool {
        if self.bytes + line.len() <= MAX_QUEUED_BYTES {
            self.bytes += line.len();
            self.lines.push(line);
            return true;
        }
        let Some(id) = replay::request_id(&line) else {
            tracing::warn!("dropped a message from the agent: too much queued while reconnecting");
            return true;
        };
        tracing::warn!("refused a request from the agent: too much queued while reconnecting");
        to_client
            .send(replay::retry_error(&id, QUEUE_FULL_MESSAGE))
            .await
            .is_ok()
    }
}

struct DaemonLink {
    lines: mpsc::Receiver<Vec<u8>>,
    writer: OwnedWriteHalf,
    reader: JoinHandle<()>,
}

impl DaemonLink {
    fn new(stream: UnixStream) -> Self {
        let (read_half, writer) = stream.into_split();
        let (lines, reader) = spawn_line_reader(read_half, PartialLine::Drop);
        Self {
            lines,
            writer,
            reader,
        }
    }

    async fn send(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(line).await
    }
}

impl Drop for DaemonLink {
    fn drop(&mut self) {
        self.reader.abort();
    }
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
    partial: PartialLine,
) -> (mpsc::Receiver<Vec<u8>>, JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (lines, received) = mpsc::channel(LINE_BUFFER);
    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) => break,
                Ok(_) if !line.ends_with(b"\n") && matches!(partial, PartialLine::Drop) => {
                    break tracing::debug!(bytes = line.len(), "dropped a message cut short");
                }
                Ok(_) => {
                    if lines.send(line).await.is_err() {
                        break;
                    }
                }
                Err(error) => break tracing::debug!(%error, "reading failed"),
            }
        }
    });
    (received, task)
}

fn spawn_client_writer() -> (mpsc::Sender<Vec<u8>>, JoinHandle<()>) {
    let (sender, mut lines) = mpsc::channel::<Vec<u8>>(LINE_BUFFER);
    let task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines.recv().await {
            let written = async {
                stdout.write_all(&line).await?;
                stdout.flush().await
            };
            if let Err(error) = written.await {
                tracing::debug!(%error, "the agent stopped reading");
                break;
            }
        }
    });
    (sender, task)
}
