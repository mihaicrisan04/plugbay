//! Whether a config file was written on this machine or delivered from elsewhere (a git
//! clone or pull, a download), which decides if it may be trusted without asking.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

const QUARANTINE_ATTRIBUTE: &str = "com.apple.quarantine";

/// True when `path` was written here: not marked as a download, and either outside any git
/// repository, untracked, ignored, or carrying uncommitted changes. A file exactly as git
/// checked it out came from a commit, possibly someone else's, so it still asks.
/// Fails closed: when git can't answer, the file asks.
pub fn was_written_here(path: &Path) -> bool {
    if is_quarantined(path) {
        return false;
    }
    match git_state(path) {
        Ok(GitState::OutsideRepository | GitState::Untracked | GitState::LocallyChanged) => true,
        Ok(GitState::AsCommitted) => false,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = format!("{error:#}"), "could not ask git where a config file came from");
            false
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum GitState {
    OutsideRepository,
    /// Untracked or ignored: never part of a commit.
    Untracked,
    /// Tracked, with staged or unstaged edits made here.
    LocallyChanged,
    /// Tracked and identical to the checked-out commit.
    AsCommitted,
}

/// macOS marks files that browsers and archive tools download, and keeps the mark on extracted files.
fn is_quarantined(path: &Path) -> bool {
    let mut buffer = [0_u8; 512];
    match rustix::fs::getxattr(path, QUARANTINE_ATTRIBUTE, &mut buffer[..]) {
        Ok(_) => true,
        Err(error) => error == rustix::io::Errno::RANGE,
    }
}

fn git_state(path: &Path) -> anyhow::Result<GitState> {
    let directory = path.parent().context("the config file has no directory")?;
    let file_name = path.file_name().context("the config file has no name")?;

    let inside = git(directory)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("running git")?;
    if !inside.status.success() {
        let stderr = String::from_utf8_lossy(&inside.stderr);
        if stderr.contains("not a git repository") {
            return Ok(GitState::OutsideRepository);
        }
        bail!("git rev-parse failed: {}", stderr.trim());
    }

    let status = git(directory)
        .args(["status", "--porcelain=v1", "--ignored", "--"])
        .arg(file_name)
        .output()
        .context("running git status")?;
    if !status.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    Ok(parse_status(&String::from_utf8_lossy(&status.stdout)))
}

/// One `git status --porcelain` line for the file, or nothing when it matches the commit.
fn parse_status(output: &str) -> GitState {
    let line = output.lines().next().unwrap_or_default();
    if line.is_empty() {
        GitState::AsCommitted
    } else if line.starts_with("??") || line.starts_with("!!") {
        GitState::Untracked
    } else {
        GitState::LocallyChanged
    }
}

/// English messages (`LC_ALL=C`) so "not a git repository" can be recognised.
fn git(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn run_git(directory: &Path, args: &[&str]) {
        let status = git(directory)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.test"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn status_lines_map_to_states() {
        assert_eq!(parse_status(""), GitState::AsCommitted);
        assert_eq!(parse_status("?? plugbay.toml\n"), GitState::Untracked);
        assert_eq!(parse_status("!! plugbay.toml\n"), GitState::Untracked);
        assert_eq!(parse_status(" M plugbay.toml\n"), GitState::LocallyChanged);
        assert_eq!(parse_status("A  plugbay.toml\n"), GitState::LocallyChanged);
    }

    #[test]
    fn file_outside_git_was_written_here() {
        let directory = TempDir::new().unwrap();
        let file = directory.path().join("plugbay.toml");
        fs::write(&file, "").unwrap();
        assert!(was_written_here(&file));
    }

    #[test]
    fn committed_file_asks_until_edited_here() {
        let directory = TempDir::new().unwrap();
        let repository = directory.path();
        let file = repository.join("plugbay.toml");
        run_git(repository, &["init", "--quiet"]);
        fs::write(&file, "[servers]\n").unwrap();
        assert!(was_written_here(&file), "untracked");

        run_git(repository, &["add", "plugbay.toml"]);
        run_git(repository, &["commit", "--quiet", "-m", "config"]);
        assert!(!was_written_here(&file), "as committed");

        fs::write(&file, "[servers]\nlinear = true\n").unwrap();
        assert!(was_written_here(&file), "edited");
    }
}
