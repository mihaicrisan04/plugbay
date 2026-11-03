//! The daemon going away under connected agents: `plugbay stop`, a crash, an upgrade. The shim
//! moves each session to a new daemon while the agent's client keeps running.

use super::*;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

const OLD_VERSION: &str = "0.0.1";
const MOVE_TIMEOUT: Duration = Duration::from_secs(20);

/// A client that counts `tools/list_changed` notifications.
struct CountingWatcher {
    changes: watch::Sender<usize>,
}

impl ClientHandler for CountingWatcher {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.send_modify(|count| *count += 1);
    }
}

type CountingClient = RunningService<RoleClient, CountingWatcher>;

impl Sandbox {
    /// An agent session whose shim reports `version` (the real one when `None`).
    async fn connect_counting(
        &self,
        project: &str,
        version: Option<&str>,
    ) -> (CountingClient, watch::Receiver<usize>) {
        let mut command = self.shim_command(project);
        if let Some(version) = version {
            command.env("PLUGBAY_TEST_VERSION", version);
        }
        let (changes, seen) = watch::channel(0);
        let client = CountingWatcher { changes }
            .serve(TokioChildProcess::new(command).unwrap())
            .await
            .unwrap();
        (client, seen)
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
        let mut stream = UnixStream::connect(self.plugbay_home.join("state/daemon.sock"))
            .await
            .unwrap();
        let hello = json!({ "kind": "control", "version": version, "request": request });
        stream
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn status(&self) -> Value {
        self.control(env!("CARGO_PKG_VERSION"), "status").await
    }

    /// Polls the daemon's status until `ready` holds for it.
    async fn wait_for_daemon(&self, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + MOVE_TIMEOUT;
        loop {
            let status =
                match UnixStream::connect(self.plugbay_home.join("state/daemon.sock")).await {
                    Ok(_) => self.status().await,
                    Err(_) => Value::Null,
                };
            if ready(&status) {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never saw {what} (last status: {status}); daemon log:\n{}",
                self.daemon_log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn wait_for_list_change(
    sandbox: &Sandbox,
    changes: &mut watch::Receiver<usize>,
    seen: usize,
) {
    let moved = tokio::time::timeout(MOVE_TIMEOUT, changes.wait_for(|count| *count > seen)).await;
    assert!(
        moved.is_ok(),
        "the session never announced its new daemon; daemon log:\n{}",
        sandbox.daemon_log()
    );
}

async fn wait_for_exit(sandbox: &Sandbox, daemon: &mut std::process::Child) {
    let deadline = tokio::time::Instant::now() + MOVE_TIMEOUT;
    while daemon.try_wait().unwrap().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old daemon never exited; daemon log:\n{}",
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn stopping_the_daemon_moves_open_sessions_to_a_new_one() {
    let sandbox = Sandbox::new();
    let (client, mut changes) = sandbox.connect_counting("client-b", None).await;
    let before = call_text(&client, "echo__whoami").await;
    let old_pid = sandbox.status().await["pid"].clone();

    let seen = *changes.borrow();
    sandbox.plugbay(&["stop"], "client-b");
    wait_for_list_change(&sandbox, &mut changes, seen).await;

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
    let (client, mut changes) = sandbox.connect_counting("client-b", None).await;
    call_text(&client, "echo__whoami").await;
    let pid = sandbox.status().await["pid"].to_string();

    let seen = *changes.borrow();
    let slow_call = tokio::spawn({
        let peer = client.peer().clone();
        async move {
            let arguments = json!({ "text": "slow", "delay_ms": 30_000 });
            let Value::Object(arguments) = arguments else {
                unreachable!()
            };
            peer.call_tool(CallToolRequestParams::new("echo__echo").with_arguments(arguments))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
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

    wait_for_list_change(&sandbox, &mut changes, seen).await;
    let echoed = call_with(&client, "echo__echo", json!({ "text": "again" })).await;
    assert_eq!(echoed, "again");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_newer_shim_retires_an_older_daemon_and_every_session_moves() {
    let sandbox = Sandbox::new();
    let mut old_daemon = sandbox.spawn_daemon_reporting(OLD_VERSION);
    sandbox
        .wait_for_daemon("the old daemon", |status| status["version"] == OLD_VERSION)
        .await;
    let (old_client, mut changes) = sandbox
        .connect_counting("client-b", Some(OLD_VERSION))
        .await;
    assert!(
        call_text(&old_client, "echo__whoami")
            .await
            .starts_with("account=client-b ")
    );

    let seen = *changes.borrow();
    let new_client = sandbox.connect("client-a").await;
    let whoami = call_text(&new_client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-a "), "{whoami}");
    wait_for_exit(&sandbox, &mut old_daemon).await;

    wait_for_list_change(&sandbox, &mut changes, seen).await;
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

    let (older_client, _changes) = sandbox
        .connect_counting("client-a", Some(OLD_VERSION))
        .await;
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
    let mut old_shim = UnixStream::connect(sandbox.plugbay_home.join("state/daemon.sock"))
        .await
        .unwrap();
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
    assert_eq!(retiring["message"], "retiring", "{retiring}");
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
