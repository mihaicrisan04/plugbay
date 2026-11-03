//! Waiting for what a test expects: a tool, a file, a status line, a `tools/list_changed`.

use super::*;

/// A client that counts `tools/list_changed` notifications.
pub struct ChangeWatcher {
    changes: watch::Sender<usize>,
}

impl ClientHandler for ChangeWatcher {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.send_modify(|count| *count += 1);
    }
}

pub type WatchingClient = RunningService<RoleClient, ChangeWatcher>;

pub async fn watch_changes(
    command: tokio::process::Command,
) -> (WatchingClient, watch::Receiver<usize>) {
    let (changes, seen) = watch::channel(0);
    let transport = TokioChildProcess::new(command).unwrap();
    (
        ChangeWatcher { changes }.serve(transport).await.unwrap(),
        seen,
    )
}

/// Polls the tool list until `tool` shows up, failing with the daemon log after 10 s.
pub async fn wait_for_tool<H: ClientHandler>(
    sandbox: &Sandbox,
    client: &RunningService<RoleClient, H>,
    tool: &str,
) {
    wait_for_tool_within(sandbox, client, tool, Duration::from_secs(10)).await;
}

pub async fn wait_for_tool_within<H: ClientHandler>(
    sandbox: &Sandbox,
    client: &RunningService<RoleClient, H>,
    tool: &str,
    timeout: Duration,
) {
    poll_until(sandbox, timeout, async || {
        let found = tool_names(client).await.contains(&tool.to_string());
        found.then_some(()).ok_or(format!("{tool} never appeared"))
    })
    .await;
}

/// Polls `check` until it gives a value, failing after `timeout` with what it last saw.
pub async fn poll_until<T>(
    sandbox: &Sandbox,
    timeout: Duration,
    mut check: impl AsyncFnMut() -> Result<T, String>,
) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match check().await {
            Ok(value) => return value,
            Err(last_seen) => assert!(
                tokio::time::Instant::now() < deadline,
                "{last_seen}; daemon log:\n{}",
                sandbox.daemon_log()
            ),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn wait_for_file(sandbox: &Sandbox, path: &Path) {
    poll_until(sandbox, Duration::from_secs(10), async || {
        let exists = path.exists();
        exists
            .then_some(())
            .ok_or(format!("{} never appeared", path.display()))
    })
    .await;
}

pub async fn wait_for_status<H: ClientHandler>(
    sandbox: &Sandbox,
    client: &RunningService<RoleClient, H>,
    needle: &str,
) {
    poll_until(sandbox, Duration::from_secs(10), async || {
        let status = call_text(client, "plugbay__status").await;
        let found = status.contains(needle);
        found
            .then_some(())
            .ok_or(format!("status never said {needle}: {status}"))
    })
    .await;
}

/// Waits for a `tools/list_changed` the client hasn't seen yet (`borrow_and_update` marks the
/// ones before an action as seen), failing with `why`.
pub async fn wait_for_change(changed: &mut watch::Receiver<usize>, why: &str) {
    let arrived = tokio::time::timeout(Duration::from_secs(20), changed.changed()).await;
    assert!(matches!(arrived, Ok(Ok(()))), "{why}");
}
