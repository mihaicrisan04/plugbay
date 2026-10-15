//! The first line a client writes on the daemon socket. After a `Session` hello the
//! connection carries plain MCP JSON-RPC; after a `Control` hello the daemon writes one
//! `ControlResponse` line and closes.

use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::paths::Paths;
use crate::project::ProjectHints;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Hello {
    Session {
        version: String,
        hints: ProjectHints,
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
    /// (after `login` or `logout`), so the new ones apply right away.
    CredentialsChanged {
        identity: String,
        account: String,
    },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    pub version: String,
    pub sessions: usize,
    pub message: String,
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
            version: VERSION.to_string(),
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

pub async fn write_line(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &impl Serialize,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    Ok(())
}
