//! The daemon going away under connected agents: `plugbay stop`, a crash, an upgrade. The shim
//! moves each session to a new daemon while the agent's client keeps running.

use super::*;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

const OLD_VERSION: &str = "0.0.1";
const MOVE_TIMEOUT: Duration = Duration::from_secs(20);

impl Sandbox {
    /// An agent session whose shim reports `version`, as one started before an upgrade.
    async fn connect_reporting(
        &self,
        project: &str,
        version: &str,
    ) -> (WatchingClient, watch::Receiver<usize>) {
        let mut command = self.shim_command(project);
        command.env("PLUGBAY_TEST_VERSION", version);
        watch_changes(command).await
    }

    /// A daemon started by hand that reports `version`, as one left over from before an upgrade.
    fn spawn_daemon_reporting(&self, version: &str) -> std::process::Child {
        self.command(&["daemon"], "client-b")
            .env("PLUGBAY_TEST_VERSION", version)
            .env("PATH", self.path_with_fake_tools())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// One raw control exchange, as a plugbay of `version` sends it.
    async fn control(&self, version: &str, request: &str) -> Value {
        let stream = UnixStream::connect(self.socket()).await.unwrap();
        control_on(stream, version, request).await
    }

    async fn status(&self) -> Value {
        self.control(env!("CARGO_PKG_VERSION"), "status").await
    }

    /// Polls the daemon's status until `ready` holds for it. A connection that never says hello
    /// would hold off retiring, so each poll's connection carries the status request.
    async fn wait_for_daemon(&self, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        poll_until(self, MOVE_TIMEOUT, async || {
            let status = match UnixStream::connect(self.socket()).await {
                Ok(stream) => control_on(stream, env!("CARGO_PKG_VERSION"), "status").await,
                Err(_) => Value::Null,
            };
            if ready(&status) {
                Ok(status)
            } else {
                Err(format!("never saw {what} (last status: {status})"))
            }
        })
        .await
    }
}

async fn control_on(mut stream: UnixStream, version: &str, request: &str) -> Value {
    let hello = json!({ "kind": "control", "version": version, "request": request });
    stream
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn wait_for_exit(sandbox: &Sandbox, daemon: &mut std::process::Child) {
    poll_until(sandbox, MOVE_TIMEOUT, async || match daemon.try_wait() {
        Ok(Some(_)) => Ok(()),
        _ => Err("the old daemon never exited".to_string()),
    })
    .await;
}

#[tokio::test]
async fn stopping_the_daemon_moves_open_sessions_to_a_new_one() {
    let sandbox = Sandbox::new();
    let (client, mut changes) = sandbox.connect_watching("client-b").await;
    let before = call_text(&client, "echo__whoami").await;
    let old_pid = sandbox.status().await["pid"].clone();

    changes.borrow_and_update();
    sandbox.plugbay(&["stop"], "client-b");
    wait_for_change(&mut changes, "the session never announced its new daemon").await;

    assert!(
        tool_names(&client)
            .await
            .contains(&"echo__whoami".to_string())
    );
    let after = call_text(&client, "echo__whoami").await;
    assert!(after.starts_with("account=client-b "), "{after}");
    assert_ne!(after, before, "a new daemon runs its own upstream");
    let status = sandbox.status().await;
    assert_ne!(status["pid"], old_pid);
    assert_eq!(status["sessions"], 1, "{status}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_request_in_flight_when_the_daemon_dies_fails_and_the_next_succeeds() {
    let sandbox = Sandbox::new();
    let (client, mut changes) = sandbox.connect_watching("client-b").await;
    call_text(&client, "echo__whoami").await;
    let pid = sandbox.status().await["pid"].to_string();

    changes.borrow_and_update();
    let started = sandbox.plugbay_home.join("slow-call-started");
    let slow_call = tokio::spawn({
        let peer = client.peer().clone();
        let arguments = json!({ "text": "slow", "delay_ms": 30_000, "started": started })
            .as_object()
            .cloned()
            .unwrap();
        async move {
            peer.call_tool(CallToolRequestParams::new("echo__echo").with_arguments(arguments))
                .await
        }
    });
    wait_for_file(&sandbox, &started).await;
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap();
    assert!(killed.success());

    let failed = tokio::time::timeout(MOVE_TIMEOUT, slow_call)
        .await
        .expect("the call in flight was never answered")
        .unwrap();
    let error = format!(
        "{:?}",
        failed.expect_err("the daemon died before answering")
    );
    assert!(error.contains("plugbay restarted"), "{error}");

    wait_for_change(&mut changes, "the session never announced its new daemon").await;
    let echoed = call_with(&client, "echo__echo", json!({ "text": "again" })).await;
    assert_eq!(echoed, "again");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_moved_session_keeps_the_servers_its_agent_switched_off() {
    let sandbox = Sandbox::new();
    let (client, mut changes) = sandbox.connect_watching("client-b").await;
    call_text(&client, "echo__whoami").await;
    let disabled = call_with(&client, "plugbay__disable", json!({ "server": "echo" })).await;
    assert!(disabled.contains("echo is off"), "{disabled}");
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);

    changes.borrow_and_update();
    sandbox.plugbay(&["stop"], "client-b");
    wait_for_change(&mut changes, "the session never announced its new daemon").await;

    assert_eq!(tool_names(&client).await, ["plugbay__status"]);
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("echo off"), "{status}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_newer_shim_retires_an_older_daemon_and_every_session_moves() {
    let sandbox = Sandbox::new();
    let mut old_daemon = sandbox.spawn_daemon_reporting(OLD_VERSION);
    sandbox
        .wait_for_daemon("the old daemon", |status| status["version"] == OLD_VERSION)
        .await;
    let (old_client, mut changes) = sandbox.connect_reporting("client-b", OLD_VERSION).await;
    assert!(
        call_text(&old_client, "echo__whoami")
            .await
            .starts_with("account=client-b ")
    );

    changes.borrow_and_update();
    let new_client = sandbox.connect("client-a").await;
    let whoami = call_text(&new_client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-a "), "{whoami}");
    wait_for_exit(&sandbox, &mut old_daemon).await;

    wait_for_change(&mut changes, "the old session never moved").await;
    let whoami = call_text(&old_client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-b "), "{whoami}");
    let status = sandbox.status().await;
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["sessions"], 2, "{status}");
    new_client.cancel().await.unwrap();
    old_client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_older_plugbay_never_retires_a_newer_daemon() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-b").await;
    let daemon = sandbox.status().await;

    let (older_client, _changes) = sandbox.connect_reporting("client-a", OLD_VERSION).await;
    assert!(
        call_text(&older_client, "echo__whoami")
            .await
            .starts_with("account=client-a ")
    );
    for version in [OLD_VERSION, env!("CARGO_PKG_VERSION")] {
        let refused = sandbox.control(version, "retire").await;
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .starts_with("not retiring"),
            "{refused}"
        );
        assert_eq!(refused["retiring"], false);
    }
    let status = sandbox.status().await;
    assert_eq!(status["pid"], daemon["pid"]);
    assert_eq!(status["sessions"], 2, "{status}");

    older_client.cancel().await.unwrap();
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_session_that_cannot_reconnect_keeps_the_daemon_from_retiring() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-b").await;
    let daemon = sandbox.status().await;

    // What a shim from before daemon upgrades sends: no `reconnects`.
    let mut old_shim = UnixStream::connect(sandbox.socket()).await.unwrap();
    let hello = json!({
        "kind": "session",
        "version": "0.2.1",
        "hints": { "explicit": null, "claude_project_dir": null, "root": null, "cwd": null },
    });
    old_shim
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .unwrap();
    sandbox
        .wait_for_daemon("the old shim's session", |status| status["sessions"] == 2)
        .await;

    let refused = sandbox.control("99.0.0", "retire").await;
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("can't reconnect"),
        "{refused}"
    );
    assert_eq!(sandbox.status().await["pid"], daemon["pid"]);

    drop(old_shim);
    sandbox
        .wait_for_daemon("the old shim leaving", |status| status["sessions"] == 1)
        .await;
    let retiring = sandbox.control("99.0.0", "retire").await;
    assert_eq!(retiring["retiring"], true, "{retiring}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn restart_hands_an_older_daemon_over_to_a_new_one() {
    let sandbox = Sandbox::new();
    let mut old_daemon = sandbox.spawn_daemon_reporting(OLD_VERSION);
    sandbox
        .wait_for_daemon("the old daemon", |status| status["version"] == OLD_VERSION)
        .await;

    let output = plugbay_output(&sandbox, &["restart"], "client-b");
    let stdout = text(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", text(&output.stderr));
    assert!(
        stdout.contains(&format!("plugbay {OLD_VERSION} and handed over")),
        "{stdout}{}\nexit: {:?}; daemon log:\n{}",
        text(&output.stderr),
        old_daemon.try_wait(),
        sandbox.daemon_log()
    );
    wait_for_exit(&sandbox, &mut old_daemon).await;
}
