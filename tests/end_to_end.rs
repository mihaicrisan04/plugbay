//! Drives the real binary the way an agent does: `plugbay connect` over stdio, daemon auto-started,
//! a fake stdio upstream (`examples/echo_server.rs`) per account.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult, ElicitationAction,
    Implementation,
};
use rmcp::service::RunningService;
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

/// A client whose human answers every plugbay prompt with the same choice, e.g. `allow_once`.
#[derive(Clone)]
struct AnsweringClient {
    first_answer: &'static str,
    later_answer: &'static str,
    prompts: Arc<std::sync::Mutex<Vec<String>>>,
    changed: Arc<Notify>,
}

impl AnsweringClient {
    fn new(answer: &'static str) -> Self {
        Self::first_then(answer, answer)
    }

    /// Answers the first prompt with `first_answer` and every later one with `later_answer`.
    fn first_then(first_answer: &'static str, later_answer: &'static str) -> Self {
        Self {
            first_answer,
            later_answer,
            prompts: Arc::default(),
            changed: Arc::default(),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl ClientHandler for AnsweringClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_elicitation().build(),
            Implementation::new("answering-test-client", "0.0.0"),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        let answer = {
            let mut prompts = self.prompts.lock().unwrap();
            prompts.push(format!("{request:?}"));
            if prompts.len() == 1 {
                self.first_answer
            } else {
                self.later_answer
            }
        };
        Ok(ElicitResult::new(ElicitationAction::Accept)
            .with_content(serde_json::json!({ "decision": answer })))
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changed.notify_one();
    }
}

/// A client that records `tools/list_changed` notifications.
struct ChangeWatcher {
    changed: Arc<Notify>,
}

impl ClientHandler for ChangeWatcher {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changed.notify_one();
    }
}

struct Sandbox {
    _temp: TempDir,
    plugbay_home: PathBuf,
    projects: PathBuf,
    /// Passed to the daemon as `PLUGBAY_READY_DEADLINE_MS`; the daemon's 8 s otherwise.
    ready_deadline: Option<Duration>,
}

/// Stands in for `fnox get <NAME>`: `SLOW_TOKEN` answers after 3 s, `FAILS_SLOWLY` fails after 2 s.
const FAKE_FNOX: &str = r#"#!/bin/sh
case "$2" in
  SLOW_TOKEN) touch "$(dirname "$0")/slow-token-asked"; sleep 3; echo test-token ;;
  FAILS_SLOWLY) sleep 2; echo "no such secret" >&2; exit 1 ;;
  *) echo test-token ;;
esac
"#;

impl Sandbox {
    /// A sandbox whose daemon waits at most `ready_deadline` for upstreams, so slow-server
    /// tests don't sit through the full 8 s.
    fn with_ready_deadline(ready_deadline: Duration) -> Self {
        let mut sandbox = Self::new();
        sandbox.ready_deadline = Some(ready_deadline);
        sandbox
    }

    /// Global config defines `echo` once (disabled); projects only pick the account,
    /// so they hold nothing executable and need no trust. A fake `fnox` is on the daemon's PATH.
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let plugbay_home = root.join("plugbay-home");
        let projects = root.join("work");
        std::fs::create_dir_all(plugbay_home.join("config")).unwrap();

        let global_config = format!(
            r#"
[servers.echo]
command = "{echo}"
enabled = false

[accounts.echo.client-a]
env = {{ ECHO_ACCOUNT = "client-a" }}

[accounts.echo.client-b]
env = {{ ECHO_ACCOUNT = "client-b" }}
"#,
            echo = echo_server_binary().display()
        );
        std::fs::write(plugbay_home.join("config/config.toml"), global_config).unwrap();
        install_fake_fnox(&plugbay_home.join("bin"));

        write_project(
            &projects.join("client-a"),
            "[servers]\necho = { account = \"client-a\", exclude_tools = [\"delete_*\"] }\n",
        );
        write_project(
            &projects.join("client-b"),
            "[servers]\necho = \"client-b\"\n",
        );
        write_project(&projects.join("client-a/app"), "");
        write_project(&projects.join("quiet"), "[servers]\necho = false\n");

        Self {
            _temp: temp,
            plugbay_home,
            projects,
            ready_deadline: None,
        }
    }

    async fn connect(&self, project: &str) -> RunningService<RoleClient, ()> {
        ().serve(self.transport(project)).await.unwrap()
    }

    async fn connect_watching(
        &self,
        project: &str,
    ) -> (RunningService<RoleClient, ChangeWatcher>, Arc<Notify>) {
        let changed = Arc::new(Notify::new());
        let watcher = ChangeWatcher {
            changed: changed.clone(),
        };
        (
            watcher.serve(self.transport(project)).await.unwrap(),
            changed,
        )
    }

    fn transport(&self, project: &str) -> TokioChildProcess {
        TokioChildProcess::new(self.shim_command(project)).unwrap()
    }

    /// `plugbay connect` for `project`, the way an agent launches it.
    fn shim_command(&self, project: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_plugbay"));
        command
            .arg("connect")
            .arg("--project")
            .arg(self.projects.join(project))
            .env("PLUGBAY_HOME", &self.plugbay_home)
            .env(
                "PLUGBAY_INSECURE_TEST_VAULT",
                self.plugbay_home.join("vault.json"),
            )
            .env_remove("CLAUDE_PROJECT_DIR")
            .env("PLUGBAY_LOG", "debug")
            .env("PATH", self.path_with_fake_tools());
        if let Some(deadline) = self.ready_deadline {
            command.env(
                "PLUGBAY_READY_DEADLINE_MS",
                deadline.as_millis().to_string(),
            );
        }
        command
    }

    fn path_with_fake_tools(&self) -> String {
        format!(
            "{}:{}",
            self.plugbay_home.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn append_to_global_config(&self, tables: &str) {
        let path = self.plugbay_home.join("config/config.toml");
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push_str(tables);
        std::fs::write(path, contents).unwrap();
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.plugbay_home.join("state/daemon.log")).unwrap_or_default()
    }

    /// Adds top-level keys, which must come before the first table header.
    fn prepend_to_global_config(&self, top_level_keys: &str) {
        let path = self.plugbay_home.join("config/config.toml");
        let existing = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("{top_level_keys}{existing}")).unwrap();
    }

    fn plugbay(&self, args: &[&str], project: &str) -> String {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_plugbay"))
            .args(args)
            .current_dir(self.projects.join(project))
            .env("PLUGBAY_HOME", &self.plugbay_home)
            .env(
                "PLUGBAY_INSECURE_TEST_VAULT",
                self.plugbay_home.join("vault.json"),
            )
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _stop_output = std::process::Command::new(env!("CARGO_BIN_EXE_plugbay"))
            .arg("stop")
            .env("PLUGBAY_HOME", &self.plugbay_home)
            .output();
    }
}

fn install_fake_fnox(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(bin).unwrap();
    let fnox = bin.join("fnox");
    std::fs::write(&fnox, FAKE_FNOX).unwrap();
    std::fs::set_permissions(&fnox, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_project(dir: &Path, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    if !contents.is_empty() {
        std::fs::write(dir.join("plugbay.toml"), contents).unwrap();
    }
}

/// A project server that runs a local command, which needs the human's trust.
fn local_echo_config(extra_args: &str) -> String {
    format!(
        "[servers]\nlocal = {{ command = \"{}\", args = [{extra_args}] }}\n",
        echo_server_binary().display()
    )
}

/// Polls the tool list until `tool` shows up, failing with the daemon log after 10 s.
async fn wait_for_tool<H: ClientHandler>(
    sandbox: &Sandbox,
    client: &RunningService<RoleClient, H>,
    tool: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !tool_names(client).await.contains(&tool.to_string()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{tool} never appeared; daemon log:\n{}",
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Builds the fake upstream once per test run: `cargo test --test end_to_end` alone doesn't
/// rebuild examples, so the binary could otherwise be stale.
fn echo_server_binary() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "--quiet", "--example", "echo_server"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .unwrap();
            assert!(status.success(), "building examples/echo_server failed");
            let plugbay = PathBuf::from(env!("CARGO_BIN_EXE_plugbay"));
            plugbay
                .parent()
                .unwrap()
                .join("examples")
                .join("echo_server")
        })
        .clone()
}

/// Tool names, minus `plugbay__enable` / `plugbay__disable`, which every session always has.
async fn tool_names<H: ClientHandler>(client: &RunningService<RoleClient, H>) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .filter(|name| name != "plugbay__enable" && name != "plugbay__disable")
        .collect();
    names.sort();
    names
}

async fn call_text<H: ClientHandler>(client: &RunningService<RoleClient, H>, tool: &str) -> String {
    call_with(client, tool, serde_json::json!({})).await
}

async fn call_with<H: ClientHandler>(
    client: &RunningService<RoleClient, H>,
    tool: &str,
    arguments: serde_json::Value,
) -> String {
    let serde_json::Value::Object(arguments) = arguments else {
        panic!("tool arguments must be an object");
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .unwrap();
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|text| text.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn each_project_gets_its_own_account_and_tools() {
    let sandbox = Sandbox::new();

    let client_a = sandbox.connect("client-a").await;
    assert_eq!(
        tool_names(&client_a).await,
        [
            "echo__echo",
            "echo__huge",
            "echo__whoami",
            "plugbay__status"
        ],
        "delete_* is filtered out for client-a"
    );
    let whoami_a = call_text(&client_a, "echo__whoami").await;
    assert!(whoami_a.starts_with("account=client-a "), "{whoami_a}");

    let client_b = sandbox.connect("client-b").await;
    let whoami_b = call_text(&client_b, "echo__whoami").await;
    assert!(whoami_b.starts_with("account=client-b "), "{whoami_b}");
    assert!(
        tool_names(&client_b)
            .await
            .contains(&"echo__delete_everything".to_string())
    );

    let quiet = sandbox.connect("quiet").await;
    assert_eq!(tool_names(&quiet).await, ["plugbay__status"]);

    let instructions = client_a
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(instructions.contains("client-a"), "{instructions}");

    for client in [client_a, client_b, quiet] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test]
async fn sessions_with_the_same_account_share_one_upstream_process() {
    let sandbox = Sandbox::new();

    let first = sandbox.connect("client-a").await;
    let nested = sandbox.connect("client-a/app").await;
    let first_pid = call_text(&first, "echo__whoami").await;
    let nested_pid = call_text(&nested, "echo__whoami").await;

    // The nested dir inherits client-a but runs with its own working dir, so it gets its own process;
    // two sessions in the same dir share one.
    let second = sandbox.connect("client-a").await;
    let second_pid = call_text(&second, "echo__whoami").await;
    assert_eq!(first_pid, second_pid);
    assert!(nested_pid.starts_with("account=client-a "), "{nested_pid}");

    for client in [first, nested, second] {
        client.cancel().await.unwrap();
    }
}

#[tokio::test]
async fn status_tool_and_ls_explain_the_resolution() {
    let sandbox = Sandbox::new();

    let client = sandbox.connect("client-a").await;
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("echo:client-a"), "{status}");
    assert!(status.contains("connected"), "{status}");
    client.cancel().await.unwrap();

    let ls = sandbox.plugbay(&["ls"], "client-a");
    assert!(ls.contains("echo"), "{ls}");
    assert!(ls.contains("client-a"), "{ls}");
}

#[tokio::test]
async fn editing_config_updates_tools_live() {
    let sandbox = Sandbox::new();
    let (client, changed) = sandbox.connect_watching("client-b").await;
    assert!(
        tool_names(&client)
            .await
            .contains(&"echo__whoami".to_string())
    );

    write_project(
        &sandbox.projects.join("client-b"),
        "[servers]\necho = false\n",
    );
    tokio::time::timeout(Duration::from_secs(10), changed.notified())
        .await
        .expect("plugbay should send tools/list_changed after a config edit");
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_servers_from_untrusted_project_config_stay_off() {
    let sandbox = Sandbox::new();
    let project_config = local_echo_config("");
    write_project(&sandbox.projects.join("untrusted"), &project_config);

    let client = sandbox.connect("untrusted").await;
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("not trusted"), "{status}");
    client.cancel().await.unwrap();

    sandbox.prepend_to_global_config(&format!(
        "trusted_paths = [\"{}/**\"]\n",
        sandbox.projects.display()
    ));
    let trusted_client = sandbox.connect("untrusted").await;
    assert!(
        tool_names(&trusted_client)
            .await
            .contains(&"local__whoami".to_string())
    );
    trusted_client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_config_written_on_this_machine_still_needs_trust() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("mine"), "");
    std::fs::write(
        sandbox.projects.join("mine/plugbay.local.toml"),
        local_echo_config(""),
    )
    .unwrap();

    let client = sandbox.connect("mine").await;
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("not trusted"), "{status}");
    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(instructions.contains("plugbay trust"), "{instructions}");
    assert!(!instructions.contains("in use"), "{instructions}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn remote_server_that_needs_oauth_shows_a_login_tool() {
    let sandbox = Sandbox::new();
    let url = spawn_unauthorized_http_server().await;
    write_project(
        &sandbox.projects.join("remote"),
        &format!("[servers]\nacme = {{ url = \"{url}\", account = \"team\" }}\n"),
    );

    let client = sandbox.connect("remote").await;
    assert_eq!(
        tool_names(&client).await,
        ["acme__login", "plugbay__status"]
    );
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("plugbay login acme:team"), "{status}");
    client.cancel().await.unwrap();
}

/// Answers every request with 401 and a bearer challenge, like an OAuth-protected MCP server.
async fn spawn_unauthorized_http_server() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = vec![0_u8; 16 * 1024];
                let _bytes_read = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\n\
                     WWW-Authenticate: Bearer resource_metadata=\"http://{address}/.well-known/oauth-protected-resource\"\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _write_result = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/mcp")
}

#[tokio::test]
async fn trust_prompt_in_the_agent_unlocks_stdio_servers() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("prompted"), &local_echo_config(""));

    let human = AnsweringClient::new("allow_once");
    let client = human
        .clone()
        .serve(sandbox.transport("prompted"))
        .await
        .unwrap();

    wait_for_tool(&sandbox, &client, "local__whoami").await;
    let prompts = human.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].contains("echo_server"),
        "the prompt names the command: {prompts:?}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn editing_a_trusted_command_mid_session_asks_again() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("edited"), &local_echo_config(""));
    let human = AnsweringClient::new("allow_once");
    let client = human
        .clone()
        .serve(sandbox.transport("edited"))
        .await
        .unwrap();
    wait_for_tool(&sandbox, &client, "local__whoami").await;

    write_project(
        &sandbox.projects.join("edited"),
        &local_echo_config("\"--changed\""),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while human.prompts().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edited command was never put to the human; daemon log:\n{}",
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        human.prompts()[1].contains("--changed"),
        "{:?}",
        human.prompts()
    );
    wait_for_tool(&sandbox, &client, "local__whoami").await;

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_crashed_upstream_is_restarted_on_the_next_call() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-b").await;

    let before = call_text(&client, "echo__whoami").await;
    let pid = before.rsplit("pid=").next().unwrap().trim().to_string();
    let kill_status = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap();
    assert!(kill_status.success());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let after = call_text(&client, "echo__whoami").await;
        if after.starts_with("account=client-b ") && after != before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "upstream never came back (last answer: {after}); daemon log:\n{}",
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stopping_the_daemon_stops_its_upstreams() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-b").await;
    let whoami = call_text(&client, "echo__whoami").await;
    let pid = whoami.rsplit("pid=").next().unwrap().trim().to_string();

    sandbox.plugbay(&["stop"], "client-b");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while process_is_alive(&pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "upstream {pid} outlived the daemon"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(client);
}

#[tokio::test]
async fn editing_a_tool_filter_notifies_the_client() {
    let sandbox = Sandbox::new();
    let (client, changed) = sandbox.connect_watching("client-b").await;
    assert!(
        tool_names(&client)
            .await
            .contains(&"echo__delete_everything".to_string())
    );

    write_project(
        &sandbox.projects.join("client-b"),
        "[servers]\necho = { account = \"client-b\", exclude_tools = [\"delete_*\"] }\n",
    );
    tokio::time::timeout(Duration::from_secs(10), changed.notified())
        .await
        .expect("a filter edit should send tools/list_changed");
    assert!(
        !tool_names(&client)
            .await
            .contains(&"echo__delete_everything".to_string())
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn losing_trust_turns_off_a_connected_stdio_server() {
    let sandbox = Sandbox::new();
    let project_config = local_echo_config("");
    write_project(&sandbox.projects.join("revoked"), &project_config);
    let global_without_trust =
        std::fs::read_to_string(sandbox.plugbay_home.join("config/config.toml")).unwrap();
    sandbox.prepend_to_global_config(&format!(
        "trusted_paths = [\"{}/**\"]\n",
        sandbox.projects.display()
    ));

    let (client, changed) = sandbox.connect_watching("revoked").await;
    assert!(
        tool_names(&client)
            .await
            .contains(&"local__whoami".to_string())
    );

    std::fs::write(
        sandbox.plugbay_home.join("config/config.toml"),
        global_without_trust,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), changed.notified())
        .await
        .expect("dropping trust should send tools/list_changed");
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);
    client.cancel().await.unwrap();
}

fn process_is_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test]
async fn fixing_a_broken_config_restores_the_session() {
    let sandbox = Sandbox::new();
    write_project(
        &sandbox.projects.join("client-b"),
        "[servers\necho = \"client-b\"\n",
    );
    let (client, changed) = sandbox.connect_watching("client-b").await;
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);

    write_project(
        &sandbox.projects.join("client-b"),
        "[servers]\necho = \"client-b\"\n",
    );
    tokio::time::timeout(Duration::from_secs(10), changed.notified())
        .await
        .expect("fixing the file should send tools/list_changed");
    assert!(
        tool_names(&client)
            .await
            .contains(&"echo__whoami".to_string())
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stopping_the_daemon_also_stops_processes_a_launcher_started() {
    let sandbox = Sandbox::new();
    sandbox.append_to_global_config(&format!(
        "\n[servers.launched]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"{}; true\"]\n",
        echo_server_binary().display()
    ));
    let client = sandbox.connect("client-b").await;
    let whoami = call_text(&client, "launched__whoami").await;
    let grandchild_pid = whoami.rsplit("pid=").next().unwrap().trim().to_string();

    sandbox.plugbay(&["stop"], "client-b");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while process_is_alive(&grandchild_pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the launcher's child {grandchild_pid} outlived the daemon"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(client);
}

#[tokio::test]
async fn an_oversized_message_fails_that_call_without_taking_down_the_daemon() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-b").await;

    let huge = client
        .call_tool(CallToolRequestParams::new("echo__huge".to_string()))
        .await
        .unwrap();
    assert_eq!(
        huge.is_error,
        Some(true),
        "{:?}",
        huge.content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.len())
    );

    let whoami = call_text(&client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-b "), "{whoami}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn agent_enables_a_catalog_server_for_its_session_only() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("fresh"), "");
    let (client, changed) = sandbox.connect_watching("fresh").await;
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);

    let enabled = call_with(
        &client,
        "plugbay__enable",
        serde_json::json!({ "server": "echo", "account": "client-a" }),
    )
    .await;
    assert!(enabled.contains("echo:client-a is on"), "{enabled}");
    tokio::time::timeout(Duration::from_secs(10), changed.notified())
        .await
        .expect("enabling a server should send tools/list_changed");
    assert!(
        tool_names(&client)
            .await
            .contains(&"echo__whoami".to_string())
    );
    let whoami = call_text(&client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-a "), "{whoami}");

    let other_session = sandbox.connect("fresh").await;
    assert_eq!(tool_names(&other_session).await, ["plugbay__status"]);
    other_session.cancel().await.unwrap();

    let disabled = call_with(
        &client,
        "plugbay__disable",
        serde_json::json!({ "server": "echo" }),
    )
    .await;
    assert!(disabled.contains("echo is off"), "{disabled}");
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);
    assert!(!sandbox.projects.join("fresh/plugbay.toml").exists());

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn enabling_an_unknown_server_explains_what_exists() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("fresh"), "");
    let client = sandbox.connect("fresh").await;

    let answer = call_with(
        &client,
        "plugbay__enable",
        serde_json::json!({ "server": "nope" }),
    )
    .await;
    assert!(
        answer.contains("doesn't know a server named nope"),
        "{answer}"
    );
    assert!(answer.contains("echo"), "{answer}");
    assert_eq!(tool_names(&client).await, ["plugbay__status"]);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn use_and_unuse_write_pins_that_ls_and_accounts_see() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("fresh"), "");
    let pins_file = sandbox.projects.join("fresh/plugbay.toml");

    sandbox.plugbay(&["use", "echo:client-b"], "fresh");
    assert_eq!(
        std::fs::read_to_string(&pins_file).unwrap(),
        "[servers]\necho = \"client-b\"\n"
    );
    let ls = sandbox.plugbay(&["ls"], "fresh");
    assert!(ls.contains("echo:client-b"), "{ls}");

    let everywhere = sandbox.plugbay(&["ls", "--all"], "client-a");
    assert!(everywhere.contains("work/fresh"), "{everywhere}");
    assert!(everywhere.contains("work/client-a"), "{everywhere}");
    let accounts = sandbox.plugbay(&["accounts"], "fresh");
    assert!(accounts.contains("client-b"), "{accounts}");

    sandbox.plugbay(&["unuse", "echo"], "fresh");
    assert_eq!(std::fs::read_to_string(&pins_file).unwrap(), "[servers]\n");

    sandbox.plugbay(&["use", "nope"], "fresh");
    assert_eq!(std::fs::read_to_string(&pins_file).unwrap(), "[servers]\n");
}

/// Runs the binary like `Sandbox::plugbay`, keeping stderr and the exit status too.
fn plugbay_output(sandbox: &Sandbox, args: &[&str], project: &str) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_plugbay"))
        .args(args)
        .current_dir(sandbox.projects.join(project))
        .env("PLUGBAY_HOME", &sandbox.plugbay_home)
        .env(
            "PLUGBAY_INSECURE_TEST_VAULT",
            sandbox.plugbay_home.join("vault.json"),
        )
        .env_remove("PLUGBAY_LOG")
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn use_with_two_accounts_writes_the_list_form_and_says_what_it_replaced() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("fresh"), "");
    let pins_file = sandbox.projects.join("fresh/plugbay.toml");

    let both = sandbox.plugbay(&["use", "echo:client-a", "echo:client-b"], "fresh");
    assert_eq!(
        std::fs::read_to_string(&pins_file).unwrap(),
        "[servers]\necho = [\"client-a\", \"client-b\"]\n"
    );
    assert_eq!(both.matches("Using echo:client-a in").count(), 1, "{both}");
    assert_eq!(both.matches("Using echo:client-b in").count(), 1, "{both}");

    let one = sandbox.plugbay(&["use", "echo:client-b"], "fresh");
    assert!(one.contains("(was client-a, client-b)"), "{one}");
}

#[tokio::test]
async fn unuse_keeps_a_pins_settings_and_knows_when_there_is_nothing_to_do() {
    let sandbox = Sandbox::new();
    let pins_file = sandbox.projects.join("client-a/plugbay.toml");

    sandbox.plugbay(&["unuse", "echo"], "client-a");
    let switched_off = std::fs::read_to_string(&pins_file).unwrap();
    assert_eq!(
        switched_off,
        "[servers]\necho = { account = \"client-a\", exclude_tools = [\"delete_*\"], enabled = false }\n"
    );

    let again = sandbox.plugbay(&["unuse", "echo"], "client-a");
    assert!(again.contains("echo is already off here"), "{again}");
    assert_eq!(std::fs::read_to_string(&pins_file).unwrap(), switched_off);

    sandbox.plugbay(&["use", "echo"], "client-a");
    assert_eq!(
        std::fs::read_to_string(&pins_file).unwrap(),
        "[servers]\necho = { account = \"client-a\", exclude_tools = [\"delete_*\"] }\n"
    );

    let unknown = plugbay_output(&sandbox, &["unuse", "notthere"], "client-a");
    assert!(!unknown.status.success());
    assert!(
        text(&unknown.stderr).contains("doesn't know how to reach notthere"),
        "{}",
        text(&unknown.stderr)
    );
}

#[tokio::test]
async fn typos_and_unknown_servers_point_at_what_exists() {
    let sandbox = Sandbox::new();
    write_project(&sandbox.projects.join("fresh"), "");

    let typo = plugbay_output(&sandbox, &["use", "sentri"], "fresh");
    assert!(!typo.status.success());
    assert!(
        text(&typo.stderr).contains("did you mean `sentry`?"),
        "{}",
        text(&typo.stderr)
    );

    let available = sandbox.plugbay(&["ls", "--available"], "fresh");
    assert!(
        available.contains("https://mcp.linear.app/mcp"),
        "{available}"
    );
    assert!(available.contains("echo"), "{available}");

    let empty = sandbox.plugbay(&["ls"], "fresh");
    assert!(empty.contains("plugbay ls --available"), "{empty}");
}

#[tokio::test]
async fn add_refuses_shadowing_plain_http_and_plaintext_secrets() {
    let sandbox = Sandbox::new();
    let global_config = sandbox.plugbay_home.join("config/config.toml");
    let before = std::fs::read_to_string(&global_config).unwrap();

    let refused = [
        &["add", "linear", "--url", "https://evil.example.com/mcp"][..],
        &[
            "add",
            "intranet",
            "--url",
            "http://intranet.example.com/mcp",
        ][..],
        &[
            "add",
            "tool",
            "--env",
            "API_KEY=test-token",
            "--",
            "npx",
            "tool",
        ][..],
    ];
    for args in refused {
        let output = plugbay_output(&sandbox, args, "client-a");
        assert!(!output.status.success(), "{args:?} was accepted");
        assert!(
            !text(&output.stderr).contains("test-token"),
            "{}",
            text(&output.stderr)
        );
    }
    assert_eq!(std::fs::read_to_string(&global_config).unwrap(), before);

    let local = plugbay_output(
        &sandbox,
        &["add", "local", "--url", "http://127.0.0.1:7373/mcp"],
        "client-a",
    );
    assert!(local.status.success(), "{}", text(&local.stderr));
}

#[tokio::test]
async fn piped_cli_output_has_no_colour_codes_or_info_lines() {
    let sandbox = Sandbox::new();
    let plain = plugbay_output(&sandbox, &["ls"], "client-a");
    let debug = plugbay_output(&sandbox, &["--log-level", "debug", "ls"], "client-a");

    for output in [&plain, &debug] {
        let stdout = text(&output.stdout);
        let stderr = text(&output.stderr);
        assert!(!stdout.contains('\u{1b}'), "{stdout}");
        assert!(!stderr.contains('\u{1b}'), "{stderr}");
    }
    let plain_stderr = text(&plain.stderr);
    assert!(!plain_stderr.contains("INFO"), "{plain_stderr}");
}

/// A machine with Claude Code config but no real `claude`: a fake CLI on `PATH` records calls.
struct MigrateSandbox {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl MigrateSandbox {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let bin = root.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        std::fs::write(&claude, "#!/bin/sh\necho \"$*\" >> \"$FAKE_CLAUDE_LOG\"\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temp: temp,
            root,
            home,
        }
    }

    fn plugbay(&self, args: &[&str]) -> std::process::Output {
        let search_path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        std::process::Command::new(env!("CARGO_BIN_EXE_plugbay"))
            .args(args)
            .current_dir(&self.home)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", search_path)
            .env("PLUGBAY_HOME", self.root.join("plugbay-home"))
            .env(
                "PLUGBAY_INSECURE_TEST_VAULT",
                self.root.join("plugbay-home/vault.json"),
            )
            .env("FAKE_CLAUDE_LOG", self.claude_log_path())
            .output()
            .unwrap()
    }

    fn claude_log_path(&self) -> PathBuf {
        self.root.join("claude.log")
    }

    fn claude_calls(&self) -> String {
        std::fs::read_to_string(self.claude_log_path()).unwrap_or_default()
    }
}

/// Writes `plugbay.toml` and commits it in a new git repository, like a teammate's checkout.
fn write_committed_project(dir: &Path, contents: &str) {
    write_project(dir, contents);
    for args in [
        &["init", "--quiet"][..],
        &["add", "plugbay.toml"][..],
        &["commit", "--quiet", "-m", "add plugbay config"][..],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.test"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
}

#[tokio::test]
async fn migrate_merges_into_existing_pins_keeps_checkouts_clean_and_undoes_once() {
    let sandbox = MigrateSandbox::new();
    let repo = sandbox.root.join("elsewhere/repo");
    let committed = "[servers]\nposthog = { exclude_tools = [\"delete_*\"] }\n";
    write_committed_project(&repo, committed);
    let plain = sandbox.home.join("work/plain");
    let plain_pins = "[servers]\nposthog = { exclude_tools = [\"delete_*\"] } # careful\n";
    write_project(&plain, plain_pins);
    let other = sandbox.home.join("work/other");
    std::fs::create_dir_all(&other).unwrap();

    let claude_config = serde_json::json!({
        "mcpServers": {
            "github": { "type": "http", "url": "https://api.githubcopilot.com/mcp/" },
            "posthog": { "type": "http", "url": "https://mcp.posthog.com/mcp" },
        },
        "projects": {
            repo.display().to_string(): { "disabledMcpServers": ["github"] },
            plain.display().to_string(): { "disabledMcpServers": ["posthog"] },
            other.display().to_string(): {},
        },
    });
    std::fs::write(sandbox.home.join(".claude.json"), claude_config.to_string()).unwrap();

    let plan = text(&sandbox.plugbay(&["migrate"]).stdout);
    assert!(
        plan.contains("posthog: keeps exclude_tools, sets enabled = false"),
        "{plan}"
    );

    let applied = sandbox.plugbay(&["migrate", "--apply"]);
    assert!(applied.status.success(), "{}", text(&applied.stderr));
    assert_eq!(
        std::fs::read_to_string(plain.join("plugbay.toml")).unwrap(),
        "[servers]\nposthog = { exclude_tools = [\"delete_*\"], enabled = false } # careful\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("plugbay.toml")).unwrap(),
        committed
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("plugbay.local.toml")).unwrap(),
        "[servers]\ngithub = false\n"
    );
    let repo_status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(text(&repo_status.stdout), "?? plugbay.local.toml\n");

    let undone = sandbox.plugbay(&["migrate", "--undo"]);
    assert!(undone.status.success(), "{}", text(&undone.stderr));
    assert_eq!(
        std::fs::read_to_string(plain.join("plugbay.toml")).unwrap(),
        plain_pins
    );
    assert!(!repo.join("plugbay.local.toml").exists());

    let calls_after_undo = sandbox.claude_calls();
    assert_eq!(
        calls_after_undo
            .matches("add-json --scope user github")
            .count(),
        1,
        "{calls_after_undo}"
    );
    std::fs::write(plain.join("plugbay.toml"), "[servers]\nlinear = true\n").unwrap();

    let second = sandbox.plugbay(&["migrate", "--undo"]);
    assert!(!second.status.success());
    assert!(
        text(&second.stderr).contains("already undone"),
        "{}",
        text(&second.stderr)
    );
    assert_eq!(sandbox.claude_calls(), calls_after_undo);
    assert_eq!(
        std::fs::read_to_string(plain.join("plugbay.toml")).unwrap(),
        "[servers]\nlinear = true\n"
    );
}

#[tokio::test]
async fn enabling_another_account_than_the_project_sets_needs_the_human() {
    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-a").await;

    let same_account = call_with(
        &client,
        "plugbay__enable",
        serde_json::json!({ "server": "echo", "account": "client-a" }),
    )
    .await;
    assert!(
        same_account.contains("echo:client-a is on"),
        "{same_account}"
    );

    let refused = call_with(
        &client,
        "plugbay__enable",
        serde_json::json!({ "server": "echo", "account": "client-b" }),
    )
    .await;
    assert!(refused.contains("plugbay use echo:client-b"), "{refused}");
    let whoami = call_text(&client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-a "), "{whoami}");
    client.cancel().await.unwrap();

    let human = AnsweringClient::new("switch");
    let approving = human
        .clone()
        .serve(sandbox.transport("client-a"))
        .await
        .unwrap();
    let switched = call_with(
        &approving,
        "plugbay__enable",
        serde_json::json!({ "server": "echo", "account": "client-b" }),
    )
    .await;
    assert!(switched.contains("echo:client-b is on"), "{switched}");
    let prompts = human.prompts();
    assert!(
        prompts
            .iter()
            .any(|prompt| prompt.contains("switch echo from client-a to client-b")),
        "{prompts:?}"
    );
    let whoami = call_text(&approving, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-b "), "{whoami}");
    approving.cancel().await.unwrap();
}

#[tokio::test]
async fn conflicting_account_pins_still_need_the_human_to_switch() {
    let sandbox = Sandbox::new();
    sandbox.append_to_global_config(&format!(
        "\n[servers.echo-copy]\ncommand = \"{}\"\nenabled = false\n",
        echo_server_binary().display()
    ));
    write_project(
        &sandbox.projects.join("conflict"),
        "[servers]\necho = \"client-a\"\necho-copy = \"client-b\"\n",
    );
    let client = sandbox.connect("conflict").await;

    let refused = call_with(
        &client,
        "plugbay__enable",
        serde_json::json!({ "server": "echo", "account": "client-b" }),
    )
    .await;
    assert!(refused.contains("needs the human's approval"), "{refused}");
    assert!(refused.contains("plugbay use echo:client-b"), "{refused}");
    let tools = tool_names(&client).await;
    assert!(!tools.contains(&"echo__whoami".to_string()), "{tools:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_slow_upstream_does_not_hold_up_the_others() {
    let sandbox = Sandbox::with_ready_deadline(Duration::from_secs(2));
    sandbox.append_to_global_config(&format!(
        "\n[servers.slow]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 4; exec {}\"]\n",
        echo_server_binary().display()
    ));
    let (client, changed) = sandbox.connect_watching("client-b").await;

    let tools = tool_names(&client).await;
    assert!(tools.contains(&"echo__whoami".to_string()), "{tools:?}");
    assert!(!tools.contains(&"slow__whoami".to_string()), "{tools:?}");

    let started = tokio::time::Instant::now();
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("slow:default"), "{status}");
    assert!(status.contains("connecting"), "{status}");
    let whoami = call_text(&client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-b "), "{whoami}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "status and calls to connected servers must not wait for the slow one"
    );

    tokio::time::timeout(Duration::from_secs(20), changed.notified())
        .await
        .expect("the slow server's arrival should send tools/list_changed");
    wait_for_tool(&sandbox, &client, "slow__whoami").await;
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn initialize_and_the_first_listing_share_one_startup_budget() {
    let sandbox = Sandbox::with_ready_deadline(Duration::from_secs(3));
    let echo = echo_server_binary();
    sandbox.append_to_global_config(&format!(
        "\n[servers.hangs]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30; exec {echo}\"]\nenabled = false\n\
         \n[servers.sluggish]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 1; exec {echo}\"]\nenv = {{ ECHO_LIST_DELAY_MS = \"30000\" }}\nenabled = false\n",
        echo = echo.display()
    ));
    write_project(
        &sandbox.projects.join("slow-start"),
        "[servers]\necho = \"client-b\"\nhangs = true\nsluggish = true\n",
    );
    let warm_up = sandbox.connect("quiet").await;
    warm_up.cancel().await.unwrap();

    let started = tokio::time::Instant::now();
    let client = sandbox.connect("slow-start").await;
    let tools = tool_names(&client).await;
    let elapsed = started.elapsed();

    assert!(tools.contains(&"echo__whoami".to_string()), "{tools:?}");
    assert!(!tools.contains(&"hangs__whoami".to_string()), "{tools:?}");
    assert!(
        !tools.contains(&"sluggish__whoami".to_string()),
        "{tools:?}"
    );
    // `hangs` spends the whole 3 s budget in initialize; the first listing must not start another.
    assert!(elapsed < Duration::from_millis(4200), "took {elapsed:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_client_that_leaves_during_initialize_starts_nothing() {
    let sandbox = Sandbox::new();
    let marker = sandbox.plugbay_home.join("local-started");
    sandbox.append_to_global_config(&format!(
        "\n[servers.local]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"touch {marker}; exec {echo}\"]\nenv = {{ TOKEN = \"${{fnox:SLOW_TOKEN}}\" }}\n",
        marker = marker.display(),
        echo = echo_server_binary().display()
    ));
    let mut shim = sandbox
        .shim_command("quiet")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = shim.stdin.take().unwrap();
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "leaves-early", "version": "0" }
        }
    });
    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .unwrap();
    wait_for_file(&sandbox, &sandbox.plugbay_home.join("bin/slow-token-asked")).await;

    drop(stdin);
    shim.wait().await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        !marker.exists(),
        "the command ran after its client left during initialize; daemon log:\n{}",
        sandbox.daemon_log()
    );
}

async fn wait_for_file(sandbox: &Sandbox, path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} never appeared; daemon log:\n{}",
            path.display(),
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A project server whose env waits on the fake `fnox` (3 s) and that creates `marker` if it
/// ever starts.
fn slow_secret_config(marker: &Path) -> String {
    format!(
        "[servers]\nlocal = {{ command = \"/bin/sh\", args = [\"-c\", \"touch {marker}; exec {echo}\"], env = {{ TOKEN = \"${{fnox:SLOW_TOKEN}}\" }} }}\n",
        marker = marker.display(),
        echo = echo_server_binary().display()
    )
}

async fn wait_for_status<H: ClientHandler>(
    sandbox: &Sandbox,
    client: &RunningService<RoleClient, H>,
    needle: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = call_text(client, "plugbay__status").await;
        if status.contains(needle) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "status never said {needle}: {status}\ndaemon log:\n{}",
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_trust_stops_a_connection_still_resolving_secrets() {
    let sandbox = Sandbox::new();
    let marker = sandbox.plugbay_home.join("local-started");
    write_project(
        &sandbox.projects.join("slow-secret"),
        &slow_secret_config(&marker),
    );
    let human = AnsweringClient::first_then("allow_always", "deny");
    let client = human
        .clone()
        .serve(sandbox.transport("slow-secret"))
        .await
        .unwrap();
    wait_for_status(&sandbox, &client, "— connecting").await;

    let revoked =
        tokio::task::block_in_place(|| sandbox.plugbay(&["trust", "--revoke"], "slow-secret"));
    assert!(revoked.contains("Revoked trust"), "{revoked}");
    // The revoke's reload waits for the attempt to settle, so the secret has resolved by now.
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        !marker.exists(),
        "the command ran after its trust was revoked; daemon log:\n{}",
        sandbox.daemon_log()
    );
    wait_for_status(&sandbox, &client, "not trusted").await;
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_client_that_leaves_stops_its_pending_connections() {
    let sandbox = Sandbox::new();
    let marker = sandbox.plugbay_home.join("local-started");
    write_project(
        &sandbox.projects.join("slow-secret"),
        &slow_secret_config(&marker),
    );
    let human = AnsweringClient::new("allow_once");
    let client = human
        .clone()
        .serve(sandbox.transport("slow-secret"))
        .await
        .unwrap();
    wait_for_status(&sandbox, &client, "— connecting").await;

    client.cancel().await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        !marker.exists(),
        "the command ran after its client left; daemon log:\n{}",
        sandbox.daemon_log()
    );
}

#[tokio::test]
async fn a_failed_enable_does_not_undo_a_newer_one() {
    let sandbox = Sandbox::new();
    sandbox.append_to_global_config(
        "\n[accounts.echo.bad]\nenv = { ECHO_ACCOUNT = \"${fnox:FAILS_SLOWLY}\" }\n",
    );
    write_project(&sandbox.projects.join("open"), "");
    let client = sandbox.connect("open").await;

    let enable_bad = call_with(
        &client,
        "plugbay__enable",
        serde_json::json!({ "server": "echo", "account": "bad" }),
    );
    let enable_good = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        call_with(
            &client,
            "plugbay__enable",
            serde_json::json!({ "server": "echo", "account": "client-a" }),
        )
        .await
    };
    let (bad, good) = tokio::join!(enable_bad, enable_good);

    assert!(bad.contains("could not start"), "{bad}");
    assert!(good.contains("echo:client-a is on"), "{good}");
    let status = call_text(&client, "plugbay__status").await;
    assert!(status.contains("echo:client-a on"), "{status}");
    let whoami = call_text(&client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-a "), "{whoami}");
    client.cancel().await.unwrap();
}
