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
use tokio::sync::Notify;

/// A client whose human answers plugbay's trust prompt with `allow_once`.
struct TrustingClient {
    prompts: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ClientHandler for TrustingClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_elicitation().build(),
            Implementation::new("trusting-test-client", "0.0.0"),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        self.prompts.lock().unwrap().push(format!("{request:?}"));
        Ok(ElicitResult::new(ElicitationAction::Accept)
            .with_content(serde_json::json!({ "decision": "allow_once" })))
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
}

impl Sandbox {
    /// Global config defines `echo` once (disabled); projects only pick the account,
    /// so they hold nothing executable and need no trust.
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
            .env("PLUGBAY_LOG", "debug");
        TokioChildProcess::new(command).unwrap()
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

fn write_project(dir: &Path, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    if !contents.is_empty() {
        std::fs::write(dir.join("plugbay.toml"), contents).unwrap();
    }
}

/// Writes `plugbay.toml` as a git clone would deliver it: committed and unchanged, so plugbay
/// can't tell it was written on this machine and asks for trust.
fn write_cloned_project(dir: &Path, contents: &str) {
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
    let project_config = format!(
        "[servers]\nlocal = {{ command = \"{}\" }}\n",
        echo_server_binary().display()
    );
    write_cloned_project(&sandbox.projects.join("untrusted"), &project_config);

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
async fn stdio_config_written_on_this_machine_needs_no_trust_prompt() {
    let sandbox = Sandbox::new();
    let project_config = format!(
        "[servers]\nlocal = {{ command = \"{}\" }}\n",
        echo_server_binary().display()
    );
    write_project(&sandbox.projects.join("mine"), &project_config);

    let client = sandbox.connect("mine").await;
    assert!(
        tool_names(&client)
            .await
            .contains(&"local__whoami".to_string()),
        "daemon log:\n{}",
        sandbox.daemon_log()
    );
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
    let project_config = format!(
        "[servers]\nlocal = {{ command = \"{}\" }}\n",
        echo_server_binary().display()
    );
    write_cloned_project(&sandbox.projects.join("prompted"), &project_config);

    let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let client = TrustingClient {
        prompts: prompts.clone(),
    }
    .serve(sandbox.transport("prompted"))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !tool_names(&client)
        .await
        .contains(&"local__whoami".to_string())
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "trusted server never appeared; daemon log:\n{}",
            sandbox.daemon_log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let prompts = prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].contains("echo_server"),
        "the prompt names the command: {prompts:?}"
    );

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
    let project_config = format!(
        "[servers]\nlocal = {{ command = \"{}\" }}\n",
        echo_server_binary().display()
    );
    write_cloned_project(&sandbox.projects.join("revoked"), &project_config);
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

#[tokio::test]
async fn migrate_merges_into_existing_pins_keeps_checkouts_clean_and_undoes_once() {
    let sandbox = MigrateSandbox::new();
    let repo = sandbox.root.join("elsewhere/repo");
    let committed = "[servers]\nposthog = { exclude_tools = [\"delete_*\"] }\n";
    write_cloned_project(&repo, committed);
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
