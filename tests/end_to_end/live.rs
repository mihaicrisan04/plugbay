//! `plugbay tools`, `call`, `restart` and `logs`: the commands that talk to a live daemon.

use super::*;

#[tokio::test]
async fn cli_tools_lists_what_an_agent_here_sees() {
    let sandbox = Sandbox::new();
    let mut config = local_echo_config("\"--as-local\"");
    config.push_str("echo = \"client-a\"\n");
    write_project(&sandbox.projects.join("mixed"), &config);

    let output = plugbay_output(&sandbox, &["tools"], "mixed");
    let stdout = text(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", text(&output.stderr));
    let rows: Vec<&str> = stdout.lines().take_while(|line| !line.is_empty()).collect();
    assert!(
        rows.iter()
            .any(|row| row.starts_with("echo__echo ") && row.ends_with("Echo the `text` argument")),
        "{stdout}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("plugbay__status ")),
        "{stdout}"
    );
    assert!(!stdout.contains("local__"), "{stdout}");
    assert!(stdout.contains("\nnot available:\n  local"), "{stdout}");
    assert!(stdout.contains("plugbay trust"), "{stdout}");

    let only_echo = text(&plugbay_output(&sandbox, &["tools", "echo"], "mixed").stdout);
    assert!(
        only_echo.lines().all(|line| line.starts_with("echo__")),
        "{only_echo}"
    );

    let unknown = plugbay_output(&sandbox, &["tools", "nope"], "mixed");
    assert!(!unknown.status.success());
    assert!(
        text(&unknown.stderr).contains("plugbay use nope"),
        "{}",
        text(&unknown.stderr)
    );
}

#[tokio::test]
async fn cli_tools_puts_servers_the_config_disables_under_their_own_heading() {
    let sandbox = Sandbox::new();
    sandbox.append_to_global_config(&format!(
        "\n[servers.always]\ncommand = \"{}\"\n",
        echo_server_binary().display()
    ));
    write_project(
        &sandbox.projects.join("without-always"),
        "[servers]\nalways = false\n",
    );

    let output = plugbay_output(&sandbox, &["tools"], "without-always");
    let stdout = text(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", text(&output.stderr));
    assert!(!stdout.contains("always__"), "{stdout}");
    assert!(!stdout.contains("not available"), "{stdout}");
    assert!(
        stdout.contains("\ndisabled by config:\n  always — disabled by "),
        "{stdout}"
    );
}

#[tokio::test]
async fn cli_tools_and_call_find_every_account_of_a_server() {
    let sandbox = Sandbox::with_ready_deadline(Duration::from_secs(1));
    sandbox.append_to_global_config(&format!(
        "\n[servers.slow]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 1.5; exec {}\"]\nenabled = false\n\
         \n[accounts.slow.a]\nenv = {{ ECHO_ACCOUNT = \"a\" }}\n\
         \n[accounts.slow.b]\nenv = {{ ECHO_ACCOUNT = \"b\" }}\n",
        echo_server_binary().display()
    ));
    write_project(
        &sandbox.projects.join("both"),
        "[servers]\necho = [\"client-a\", \"client-b\"]\nslow = [\"a\", \"b\"]\n",
    );

    // `slow-a` is still connecting when initialize gives up; `call` waits for it.
    let called = plugbay_output(&sandbox, &["call", "slow-a__whoami"], "both");
    assert!(
        called.status.success(),
        "{}\n{}",
        text(&called.stderr),
        sandbox.daemon_log()
    );
    assert!(text(&called.stdout).starts_with("account=a "));

    let echo = text(&plugbay_output(&sandbox, &["tools", "echo"], "both").stdout);
    assert!(echo.contains("echo-client-a__whoami"), "{echo}");
    assert!(echo.contains("echo-client-b__whoami"), "{echo}");
    assert!(!echo.contains("slow-"), "{echo}");

    let one_account = text(&plugbay_output(&sandbox, &["tools", "echo-client-b"], "both").stdout);
    assert!(
        one_account.contains("echo-client-b__whoami"),
        "{one_account}"
    );
    assert!(!one_account.contains("echo-client-a__"), "{one_account}");
}

#[tokio::test]
async fn cli_call_prints_tool_output_and_fails_on_tool_errors() {
    let sandbox = Sandbox::new();

    let invalid = plugbay_output(&sandbox, &["call", "echo__echo", "{text"], "client-b");
    assert!(!invalid.status.success());
    assert!(
        text(&invalid.stderr).contains("not valid JSON"),
        "{}",
        text(&invalid.stderr)
    );
    assert!(
        !sandbox.plugbay_home.join("state/daemon.sock").exists(),
        "invalid arguments must not start the daemon"
    );

    let echoed = plugbay_output(
        &sandbox,
        &["call", "echo__echo", r#"{"text": "hi"}"#],
        "client-b",
    );
    assert!(echoed.status.success(), "{}", text(&echoed.stderr));
    assert_eq!(text(&echoed.stdout), "hi\n");

    let failed = plugbay_output(
        &sandbox,
        &[
            "call",
            "echo__echo",
            r#"{"text": "it broke", "fail": true}"#,
        ],
        "client-b",
    );
    assert!(!failed.status.success());
    assert!(
        text(&failed.stderr).contains("it broke"),
        "{}",
        text(&failed.stderr)
    );

    let mut from_stdin = sandbox
        .command(&["call", "echo__echo", "-"], "client-b")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = from_stdin.stdin.take().unwrap();
        stdin.write_all(br#"{"text": "piped"}"#).unwrap();
    }
    let piped = from_stdin.wait_with_output().unwrap();
    assert_eq!(text(&piped.stdout), "piped\n");
}

#[tokio::test]
async fn cli_call_passes_escape_sequences_through_when_redirected() {
    let sandbox = Sandbox::new();
    let output = plugbay_output(
        &sandbox,
        &["call", "echo__echo", r#"{"text": "\u001b[31mred"}"#],
        "client-b",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(output.stdout, b"\x1b[31mred\n");
}

#[tokio::test]
async fn restart_reconnects_upstreams_while_agents_stay_connected() {
    let sandbox = Sandbox::new();
    let nothing = sandbox.plugbay(&["restart"], "client-b");
    assert!(nothing.contains("nothing to restart"), "{nothing}");

    let client = sandbox.connect("client-b").await;
    let before = call_text(&client, "echo__whoami").await;
    let old_pid = before.rsplit("pid=").next().unwrap().trim().to_string();

    let restarted = sandbox.plugbay(&["restart", "echo"], "client-b");
    assert!(
        restarted.starts_with("closed 1 connection(s)"),
        "{restarted}"
    );
    let after = call_text(&client, "echo__whoami").await;
    assert!(after.starts_with("account=client-b "), "{after}");
    assert_ne!(after, before, "the upstream was not reconnected");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while process_is_alive(&old_pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "old upstream {old_pid} kept running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let everything = sandbox.plugbay(&["restart"], "client-b");
    assert!(
        everything.starts_with("closed 1 connection(s)"),
        "{everything}"
    );
    let again = call_text(&client, "echo__whoami").await;
    assert_ne!(again, after);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn restart_answers_promptly_while_an_upstream_is_still_connecting() {
    let sandbox = Sandbox::with_ready_deadline(Duration::from_secs(1));
    sandbox.append_to_global_config(&format!(
        "\n[servers.slow]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 9; exec {}\"]\n",
        echo_server_binary().display()
    ));
    let client = sandbox.connect("client-b").await;
    let status = call_text(&client, "plugbay__status").await;
    let slow = status
        .lines()
        .find(|line| line.starts_with("  slow:default (/bin/sh"))
        .unwrap_or_default();
    assert!(slow.contains(" — connecting"), "{status}");

    let started = std::time::Instant::now();
    let output = plugbay_output(&sandbox, &["restart"], "client-b");
    let elapsed = started.elapsed();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).starts_with("reconnecting"),
        "{}",
        text(&output.stdout)
    );
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");

    let whoami = call_text(&client, "echo__whoami").await;
    assert!(whoami.starts_with("account=client-b "), "{whoami}");

    // The reconnect finishes after the answer: the slow server comes back for the same agent.
    wait_for_tool_within(&sandbox, &client, "slow__whoami", Duration::from_secs(25)).await;
    let slow_whoami = call_text(&client, "slow__whoami").await;
    assert!(slow_whoami.starts_with("account="), "{slow_whoami}");
    client.cancel().await.unwrap();
}

/// The request `login` / `logout` have always sent: the daemon keeps answering it, whichever
/// plugbay version sends it.
#[tokio::test]
async fn credentials_changed_in_the_old_wire_format_is_answered() {
    use std::io::{BufRead, Write};

    let sandbox = Sandbox::new();
    let client = sandbox.connect("client-b").await;

    let mut socket =
        std::os::unix::net::UnixStream::connect(sandbox.plugbay_home.join("state/daemon.sock"))
            .unwrap();
    socket
        .write_all(
            br#"{"kind":"control","version":"0.2.1","request":{"credentials_changed":{"identity":"stdio:unknown","account":"client-b"}}}
"#,
        )
        .unwrap();
    let mut answer = String::new();
    std::io::BufReader::new(socket)
        .read_line(&mut answer)
        .unwrap();
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["message"], "closed 0 connection(s) and reloaded");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn login_says_so_when_the_running_daemon_hangs_up() {
    use std::io::{BufRead, Write};

    let sandbox = Sandbox::new();
    write_project(
        &sandbox.projects.join("remote"),
        "[servers]\nacme = { url = \"https://mcp.example.com/mcp\", account = \"team\" }\n",
    );
    let state = sandbox.plugbay_home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(state.join("daemon.sock")).unwrap();
    let older_daemon = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut hello = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut hello)
            .unwrap();
    });

    let mut login = sandbox
        .command(&["login", "acme", "--token"], "remote")
        .env_remove("PLUGBAY_LOG")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    login
        .stdin
        .take()
        .unwrap()
        .write_all(b"test-token\n")
        .unwrap();
    let output = login.wait_with_output().unwrap();
    older_daemon.join().unwrap();

    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("the running daemon is older; run `plugbay stop`"),
        "{}",
        text(&output.stderr)
    );
}

#[tokio::test]
async fn logs_shows_the_end_of_the_daemon_log() {
    let sandbox = Sandbox::new();
    let missing = plugbay_output(&sandbox, &["logs"], "client-b");
    assert!(!missing.status.success());
    assert!(
        text(&missing.stderr).contains("state/daemon.log"),
        "{}",
        text(&missing.stderr)
    );

    let log_path = sandbox.plugbay_home.join("state/daemon.log");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    let lines: String = (1..=100).map(|line| format!("line {line}\n")).collect();
    std::fs::write(&log_path, lines).unwrap();

    let default = text(&plugbay_output(&sandbox, &["logs"], "client-b").stdout);
    assert_eq!(default.lines().count(), 50);
    let last_two = text(&plugbay_output(&sandbox, &["logs", "-n", "2"], "client-b").stdout);
    assert_eq!(last_two, "line 99\nline 100\n");

    let mut follower =
        tokio::process::Command::from(sandbox.command(&["logs", "-f", "-n", "1"], "client-b"))
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
    let mut stdout = tokio::io::BufReader::new(follower.stdout.take().unwrap());
    let mut first = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut first)
        .await
        .unwrap();
    assert_eq!(first, "line 100\n");

    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .unwrap();
    std::io::Write::write_all(&mut log, b"line 101\n").unwrap();
    let mut next = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut next),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(next, "line 101\n");
}

#[tokio::test]
async fn cli_tools_waits_one_more_ready_deadline_for_servers_still_connecting() {
    let sandbox = Sandbox::new();
    let echo = echo_server_binary();
    sandbox.append_to_global_config(&format!(
        "\n[servers.late]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 1.5; exec {echo}\"]\n\
         \n[servers.hangs]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30; exec {echo}\"]\n",
        echo = echo.display()
    ));

    let started = std::time::Instant::now();
    let output = sandbox
        .command(&["tools"], "client-b")
        .env("PLUGBAY_READY_DEADLINE_MS", "1000")
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    let stdout = text(&output.stdout);

    assert!(
        stdout.contains("late__whoami"),
        "{stdout}\n{}",
        sandbox.daemon_log()
    );
    assert!(stdout.contains("echo__whoami"), "{stdout}");
    let hangs = stdout
        .lines()
        .find(|line| line.starts_with("  hangs:default (/bin/sh"))
        .unwrap_or_default();
    assert!(hangs.contains(" — connecting"), "{stdout}");
    // Two 1 s deadlines plus startup; far from `hangs`' 30 s or the default 8 s deadline.
    assert!(elapsed < Duration::from_secs(7), "took {elapsed:?}");
}
