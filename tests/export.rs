//! `plugbay export` never prints or writes a secret it holds or would treat as one. Every value
//! here is fake and starts with `fake-` (or `FAKE`), so one search proves none got out.

use std::path::Path;
use std::process::Output;

use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new(config: &str) -> Self {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::fs::create_dir_all(root.path().join("project")).unwrap();
        let project = Self { root };
        std::fs::write(
            project.dir().join("plugbay.toml"),
            format!("root = true\n{config}"),
        )
        .unwrap();
        project
    }

    fn dir(&self) -> std::path::PathBuf {
        self.root.path().join("project")
    }

    fn export(&self, format: &str, extra: &[&str]) -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_plugbay"))
            .args(["export", "--format", format])
            .args(extra)
            .current_dir(self.dir())
            .env("PLUGBAY_HOME", self.root.path().join("home"))
            .env(
                "PLUGBAY_INSECURE_TEST_VAULT",
                self.root.path().join("home/vault.json"),
            )
            .env("SUFFIX", "fake-suffix-from-the-environment")
            .env_remove("PLUGBAY_LOG")
            .output()
            .unwrap()
    }
}

fn everything_shown(output: &Output, written: Option<&Path>) -> String {
    let mut shown = String::from_utf8_lossy(&output.stdout).into_owned();
    shown.push_str(&String::from_utf8_lossy(&output.stderr));
    if let Some(file) = written {
        shown.push_str(&std::fs::read_to_string(file).unwrap_or_default());
    }
    shown
}

fn assert_nothing_fake_leaves(config: &str) {
    for format in ["claude", "vscode"] {
        let project = Project::new(config);
        let printed = project.export(format, &[]);
        let shown = everything_shown(&printed, None);
        assert!(
            !shown.contains("fake-") && !shown.contains("FAKE"),
            "{shown}"
        );

        let written = project.export(format, &["--write"]);
        let file = match format {
            "claude" => project.dir().join(".mcp.json"),
            _ => project.dir().join(".vscode/mcp.json"),
        };
        let shown = everything_shown(&written, Some(&file));
        assert!(
            !shown.contains("fake-") && !shown.contains("FAKE"),
            "{shown}"
        );
    }
}

#[test]
fn credentials_next_to_refs_inside_urls_never_leave() {
    assert_nothing_fake_leaves(
        r#"
[servers.demo]
command = "echo"
env = { DATABASE_URL = "https://${env:USER}:fake-url-password@host/app" }

[servers.remote]
url = "https://mcp.example.test/mcp"
headers = { x-target = "https://fake-user:fake-url-password${env:SUFFIX}@host/app" }
"#,
    );
}

#[test]
fn passwords_with_spaces_never_leave() {
    assert_nothing_fake_leaves(
        r#"
[servers.demo]
command = "echo"
env = { DATABASE_URL = "postgres://fake-user:fake password@host/app" }
"#,
    );
}

#[test]
fn token_shaped_literals_under_innocent_names_never_leave() {
    assert_nothing_fake_leaves(
        r#"
[servers.demo]
command = "echo"

[servers.demo.env]
X = "ghp_FAKE123456789012345678901234567890123456"
Y = "sk-FAKE123456789012345678901234"
Z = "xoxb-fake-test-token"
"#,
    );
}

#[test]
fn config_problems_are_named_but_never_quoted() {
    let project = Project::new(
        r#"
[servers.demo]
url = "https://fake-user:fake-url-password@"

[servers.filtered]
command = "echo"
tools = ["fake-sensitive-["]
"#,
    );
    let output = project.export("claude", &[]);
    let shown = everything_shown(&output, None);
    assert!(!shown.contains("fake-"), "{shown}");
    assert!(shown.contains("plugbay why demo"), "{shown}");
    assert!(shown.contains("plugbay why filtered"), "{shown}");
}

#[test]
fn malformed_config_is_reported_without_its_text() {
    let project = Project::new(
        "[servers.demo]\ncommand = \"echo\"\n[servers.demo.env]\nAPI_KEY = \"fake-private-token\" trailing\n",
    );
    let output = project.export("vscode", &[]);
    assert!(!output.status.success());
    let shown = everything_shown(&output, None);
    assert!(!shown.contains("fake-"), "{shown}");
    assert!(shown.contains("plugbay.toml"), "{shown}");
    assert!(shown.contains("plugbay ls"), "{shown}");
}
