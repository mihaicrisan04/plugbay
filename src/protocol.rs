//! The first line a client writes on the daemon socket. After a `Session` hello the
//! connection carries plain MCP JSON-RPC; after a `Control` hello the daemon writes one
//! `ControlResponse` line and closes.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use rmcp::model::MetaObject;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config::SessionPin;
use crate::paths::Paths;
use crate::project::ProjectHints;
use crate::report::ServerState;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Debug builds only: the version a shim or daemon reports, so tests can stage an upgrade.
pub const TEST_VERSION_ENV: &str = "PLUGBAY_TEST_VERSION";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// The version this process speaks the daemon protocol as: [`VERSION`], or in debug builds
/// [`TEST_VERSION_ENV`] when set.
pub fn version() -> &'static str {
    #[cfg(debug_assertions)]
    {
        static OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if let Some(version) = OVERRIDE.get_or_init(|| std::env::var(TEST_VERSION_ENV).ok()) {
            return version;
        }
    }
    VERSION
}

/// How a peer's plugbay version compares with this process's [`version()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skew {
    Same,
    PeerOlder,
    PeerNewer,
    /// Different, and at least one doesn't parse: never older or newer, so nothing unexpected
    /// can retire a daemon.
    Unknown,
}

pub fn skew(peer: &str) -> Skew {
    compare(peer, version())
}

fn compare(peer: &str, ours: &str) -> Skew {
    if peer == ours {
        return Skew::Same;
    }
    match (semver::Version::parse(peer), semver::Version::parse(ours)) {
        (Ok(peer), Ok(ours)) => match peer.cmp(&ours) {
            std::cmp::Ordering::Less => Skew::PeerOlder,
            std::cmp::Ordering::Equal => Skew::Same,
            std::cmp::Ordering::Greater => Skew::PeerNewer,
        },
        _ => Skew::Unknown,
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hello {
    Session {
        version: String,
        hints: ProjectHints,
        /// The client finds a new daemon by itself when this one exits (shims since daemon
        /// upgrades). Older shims and CLI sessions can't, so a daemon serving one never retires.
        #[serde(default)]
        reconnects: bool,
        /// A moved session's `plugbay__enable` / `plugbay__disable` results on its last daemon,
        /// for the new one to apply before its first refresh.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pins: BTreeMap<String, SessionPin>,
    },
    Control {
        version: String,
        request: ControlRequest,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    /// Liveness and version check.
    Status,
    /// Re-resolve config, trust and credentials for every session (after `login`, `trust`).
    Reload,
    /// Drop every `allow_once` grant, then reload (after `plugbay trust --revoke`).
    ForgetSessionTrust,
    /// Close connections made with a server account's old credentials, then reload
    /// (after `login` or `logout`, and `plugbay restart server[:account]`), so the new ones
    /// apply right away.
    CredentialsChanged {
        identity: String,
        account: String,
    },
    /// Close every pooled upstream connection, then reload (`plugbay restart`). Agents stay
    /// connected and their sessions reconnect.
    ReconnectAll,
    Stop,
    /// A strictly newer plugbay asks this daemon to exit so a daemon of its version takes over.
    /// Refused unless every connected session reconnects on its own.
    Retire,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    pub version: String,
    pub sessions: usize,
    pub message: String,
    /// Missing from daemons that predate it.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Whether the daemon knows `retire`; daemons that predate it don't.
    #[serde(default)]
    pub can_retire: bool,
    /// Set when the daemon accepted `retire`: its socket is already gone.
    #[serde(default)]
    pub retiring: bool,
}

/// Sends one control request to a running daemon. Errors when none is listening.
pub async fn send_control(
    paths: &Paths,
    request: ControlRequest,
) -> anyhow::Result<ControlResponse> {
    let exchange = async {
        let mut stream = UnixStream::connect(paths.socket())
            .await
            .context("connecting to the daemon")?;
        let hello = Hello::Control {
            version: version().to_string(),
            request,
        };
        write_line(&mut stream, &hello).await?;

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await?;
        serde_json::from_str(&line).context("parsing the daemon's reply")
    };
    tokio::time::timeout(CONTROL_TIMEOUT, exchange)
        .await
        .context("the daemon did not answer in time")?
}

/// Whether `send_control` failed because no daemon listens, rather than one that answered badly.
pub fn daemon_is_not_running(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
    })
}

pub async fn write_line(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &impl Serialize,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    Ok(())
}

const STATUS_SERVERS_KEY: &str = "plugbay/servers";
const STATUS_PROBLEMS_KEY: &str = "plugbay/problems";

/// What `plugbay__status` reports, typed, in its result's `_meta` (agents read the same report as
/// its text), so `plugbay tools` and `call` never parse prose.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionStatus {
    pub servers: Vec<StatusServer>,
    pub problems: Vec<String>,
}

/// One server line of the status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusServer {
    pub name: String,
    /// `None` for a server the config disables.
    pub account: Option<String>,
    /// What its tool names start with: `echo`, or `echo-work` when it has several accounts.
    pub tool_prefix: String,
    /// `name:account`, as people type it.
    pub label: String,
    pub state: StatusState,
    /// The whole line as the text shows it, e.g. `echo:work (echo-server) — connected`.
    pub status_line: String,
}

/// [`ServerState`] without its details, plus servers the config disables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Ready,
    Connected,
    Connecting,
    NeedsTrust,
    NeedsLogin,
    LoginUnknown,
    WrongAccount,
    AccountUnknown,
    Failed,
    Disabled,
    /// A state from a newer daemon.
    #[serde(other)]
    Unknown,
}

impl StatusState {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready | Self::Connected)
    }
}

impl From<&ServerState> for StatusState {
    fn from(state: &ServerState) -> Self {
        match state {
            ServerState::Ready => Self::Ready,
            ServerState::Connected => Self::Connected,
            ServerState::Connecting => Self::Connecting,
            ServerState::NeedsTrust => Self::NeedsTrust,
            ServerState::NeedsLogin => Self::NeedsLogin,
            ServerState::LoginUnknown => Self::LoginUnknown,
            ServerState::WrongAccount { .. } => Self::WrongAccount,
            ServerState::AccountUnknown { .. } => Self::AccountUnknown,
            ServerState::Failed(_) => Self::Failed,
        }
    }
}

impl SessionStatus {
    pub fn into_meta(self) -> anyhow::Result<MetaObject> {
        let mut meta = MetaObject::new();
        meta.0.insert(
            STATUS_SERVERS_KEY.to_string(),
            serde_json::to_value(self.servers)?,
        );
        meta.0.insert(
            STATUS_PROBLEMS_KEY.to_string(),
            serde_json::to_value(self.problems)?,
        );
        Ok(meta)
    }

    pub fn from_meta(meta: Option<&MetaObject>) -> anyhow::Result<Self> {
        let field = |key: &str| meta.and_then(|meta| meta.0.get(key)).cloned();
        let (Some(servers), Some(problems)) =
            (field(STATUS_SERVERS_KEY), field(STATUS_PROBLEMS_KEY))
        else {
            anyhow::bail!(
                "the running daemon is older than this plugbay; run `plugbay stop` and try again"
            );
        };
        Ok(Self {
            servers: serde_json::from_value(servers).context("reading the daemon's server list")?,
            problems: serde_json::from_value(problems)
                .context("reading the daemon's config problems")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_changed_keeps_the_wire_format_older_plugbays_send() {
        let line = r#"{"kind":"control","version":"0.2.1","request":{"credentials_changed":{"identity":"https://mcp.linear.app/mcp","account":"work"}}}"#;
        let Hello::Control { request, .. } = serde_json::from_str(line).unwrap() else {
            panic!("not a control hello");
        };
        assert!(matches!(
            request,
            ControlRequest::CredentialsChanged { identity, account }
                if identity == "https://mcp.linear.app/mcp" && account == "work"
        ));
    }

    #[test]
    fn hellos_from_older_shims_cannot_reconnect() {
        let line = r#"{"kind":"session","version":"0.2.1","hints":{"explicit":null,"claude_project_dir":null,"root":null,"cwd":null}}"#;
        let hello: Hello = serde_json::from_str(line).unwrap();
        assert!(matches!(
            hello,
            Hello::Session {
                reconnects: false,
                ..
            }
        ));
    }

    #[test]
    fn replies_from_older_daemons_cannot_retire() {
        let line = r#"{"version":"0.2.1","sessions":1,"message":"running"}"#;
        let response: ControlResponse = serde_json::from_str(line).unwrap();
        assert!(!response.can_retire && !response.retiring && response.pid.is_none());
    }

    #[test]
    fn versions_compare_by_semver_and_unparseable_ones_are_unknown() {
        assert_eq!(compare("0.3.0", "0.2.1"), Skew::PeerNewer);
        assert_eq!(compare("0.3.0", "0.3.0-rc.1"), Skew::PeerNewer);
        assert_eq!(compare("0.3.0", "0.3.0"), Skew::Same);
        assert_eq!(compare("0.2.1", "0.3.0"), Skew::PeerOlder);
        assert_eq!(compare("garbage", "0.1.0"), Skew::Unknown);
        assert_eq!(compare("0.3.0", "garbage"), Skew::Unknown);
        assert_eq!(compare("garbage", "garbage"), Skew::Same);
    }

    #[test]
    fn session_pins_ride_the_hello_and_older_hellos_have_none() {
        let hello = Hello::Session {
            version: "0.3.0".to_string(),
            hints: ProjectHints::default(),
            reconnects: true,
            pins: BTreeMap::from([
                ("echo".to_string(), SessionPin::Off),
                (
                    "linear".to_string(),
                    SessionPin::On {
                        account: Some("work".to_string()),
                    },
                ),
            ]),
        };
        let line = serde_json::to_string(&hello).unwrap();
        let Hello::Session { pins, .. } = serde_json::from_str(&line).unwrap() else {
            panic!("not a session hello");
        };
        assert_eq!(pins.len(), 2, "{line}");
        assert_eq!(pins["echo"], SessionPin::Off);
    }

    #[test]
    fn session_status_survives_the_meta_round_trip() {
        let status = SessionStatus {
            servers: vec![StatusServer {
                name: "echo".to_string(),
                account: Some("work".to_string()),
                tool_prefix: "echo-work".to_string(),
                label: "echo:work".to_string(),
                state: StatusState::Connecting,
                status_line: "echo:work (echo) — connecting".to_string(),
            }],
            problems: vec!["odd".to_string()],
        };
        let meta = status.clone().into_meta().unwrap();
        assert_eq!(SessionStatus::from_meta(Some(&meta)).unwrap(), status);
    }

    #[test]
    fn a_status_without_meta_comes_from_an_older_daemon() {
        let error = SessionStatus::from_meta(None).unwrap_err().to_string();
        assert!(error.contains("plugbay stop"), "{error}");
    }

    #[test]
    fn states_from_a_newer_daemon_are_unknown() {
        let state: StatusState = serde_json::from_str(r#""sleeping""#).unwrap();
        assert_eq!(state, StatusState::Unknown);
    }
}
