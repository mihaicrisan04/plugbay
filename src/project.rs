//! Which directory an agent session belongs to, and where its config lives.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a session's config is resolved and where its stdio servers run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectLocation {
    /// Directory the config walk ends at. For a linked git worktree this is the same
    /// relative path inside the main checkout, so worktrees stored elsewhere
    /// (e.g. `~/.t3/worktrees/...`) still inherit the project's config tree.
    /// `None` means no project: only the global config applies.
    pub config_dir: Option<PathBuf>,
    /// Working directory for stdio servers (the worktree itself).
    pub working_dir: Option<PathBuf>,
}

/// What the shim and the client told us, in priority order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectHints {
    /// `--project` flag.
    pub explicit: Option<PathBuf>,
    /// `CLAUDE_PROJECT_DIR`.
    pub claude_project_dir: Option<PathBuf>,
    /// First `file://` MCP root, when the client offers roots.
    pub root: Option<PathBuf>,
    /// The shim's working directory.
    pub cwd: Option<PathBuf>,
}

/// Picks the project directory: explicit > `CLAUDE_PROJECT_DIR` > MCP root > cwd.
/// A cwd of `/` or `$HOME` means "no project" (agents launched from a dock or a bare terminal).
pub fn detect(hints: &ProjectHints, home: &Path) -> ProjectLocation {
    match choose_project_dir(hints, home) {
        Some(project_dir) => location_for(project_dir),
        None => ProjectLocation::global_only(),
    }
}

/// True when the shim's cwd alone can't identify a project, so asking the client for roots is worth it.
pub fn cwd_is_ambiguous(cwd: Option<&Path>, home: &Path) -> bool {
    let Some(cwd) = cwd else {
        return true;
    };
    let cwd = canonical_or_original(cwd);
    cwd == Path::new("/") || cwd == canonical_or_original(home)
}

impl ProjectLocation {
    pub fn global_only() -> Self {
        Self {
            config_dir: None,
            working_dir: None,
        }
    }
}

fn choose_project_dir(hints: &ProjectHints, home: &Path) -> Option<PathBuf> {
    let base = hints.cwd.as_deref();
    let explicit_hints = [&hints.explicit, &hints.claude_project_dir, &hints.root];
    for hint in explicit_hints.into_iter().flatten() {
        if let Some(dir) = existing_dir(hint, base) {
            return Some(dir);
        }
    }

    let cwd = hints.cwd.as_deref()?;
    if cwd_is_ambiguous(Some(cwd), home) {
        return None;
    }
    existing_dir(cwd, None)
}

/// Canonical form of `path` when it is an existing directory. Relative hints resolve against `base`.
fn existing_dir(path: &Path, base: Option<&Path>) -> Option<PathBuf> {
    let path = match base {
        Some(base) if path.is_relative() => base.join(path),
        _ => path.to_path_buf(),
    };
    let canonical = path.canonicalize().ok()?;
    canonical.is_dir().then_some(canonical)
}

fn location_for(project_dir: PathBuf) -> ProjectLocation {
    let config_dir = main_checkout_equivalent(&project_dir).unwrap_or_else(|| project_dir.clone());
    ProjectLocation {
        config_dir: Some(config_dir),
        working_dir: Some(project_dir),
    }
}

/// For a directory inside a linked git worktree, the same relative path inside the main checkout.
/// `None` for normal checkouts, submodules, bare repos and directories outside git.
fn main_checkout_equivalent(dir: &Path) -> Option<PathBuf> {
    let worktree_root = dir
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())?;
    let main_checkout = main_checkout_of_linked_worktree(worktree_root)?;
    let relative = dir.strip_prefix(worktree_root).ok()?;
    Some(main_checkout.join(relative))
}

/// A linked worktree has a `.git` file pointing at `<main>/.git/worktrees/<name>`,
/// whose `commondir` file points back at `<main>/.git`.
fn main_checkout_of_linked_worktree(worktree_root: &Path) -> Option<PathBuf> {
    let dot_git = worktree_root.join(".git");
    if !dot_git.is_file() {
        return None;
    }

    let git_file = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = git_file
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let gitdir = worktree_root.join(gitdir);

    let commondir = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common_git_dir = gitdir.join(commondir.trim()).canonicalize().ok()?;

    // A common dir not named `.git` belongs to a bare repository: there is no main checkout.
    if common_git_dir.file_name()? != ".git" {
        return None;
    }
    common_git_dir.parent().map(Path::to_path_buf)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        home: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        Fixture {
            _temp: temp,
            root,
            home,
        }
    }

    fn make_dir(path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        path.to_path_buf()
    }

    /// Lays out `<root>/main` as a checkout and `<root>/elsewhere/feature` as its linked worktree.
    fn make_linked_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let main = make_dir(&root.join("main"));
        let worktree_git_dir = make_dir(&main.join(".git/worktrees/feature"));
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();

        let worktree = make_dir(&root.join("elsewhere/feature"));
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .unwrap();
        fs::write(
            worktree_git_dir.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        (main, worktree)
    }

    fn location(config_dir: &Path, working_dir: &Path) -> ProjectLocation {
        ProjectLocation {
            config_dir: Some(config_dir.to_path_buf()),
            working_dir: Some(working_dir.to_path_buf()),
        }
    }

    #[test]
    fn explicit_hint_wins_over_everything() {
        let fixture = fixture();
        let explicit = make_dir(&fixture.root.join("explicit"));
        let other = make_dir(&fixture.root.join("other"));
        let hints = ProjectHints {
            explicit: Some(explicit.clone()),
            claude_project_dir: Some(other.clone()),
            root: Some(other.clone()),
            cwd: Some(other),
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            location(&explicit, &explicit)
        );
    }

    #[test]
    fn claude_project_dir_wins_over_root_and_cwd() {
        let fixture = fixture();
        let claude_dir = make_dir(&fixture.root.join("claude"));
        let other = make_dir(&fixture.root.join("other"));
        let hints = ProjectHints {
            claude_project_dir: Some(claude_dir.clone()),
            root: Some(other.clone()),
            cwd: Some(other),
            ..ProjectHints::default()
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            location(&claude_dir, &claude_dir)
        );
    }

    #[test]
    fn mcp_root_wins_over_cwd() {
        let fixture = fixture();
        let root_dir = make_dir(&fixture.root.join("root"));
        let cwd = make_dir(&fixture.root.join("cwd"));
        let hints = ProjectHints {
            root: Some(root_dir.clone()),
            cwd: Some(cwd),
            ..ProjectHints::default()
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            location(&root_dir, &root_dir)
        );
    }

    #[test]
    fn missing_hint_paths_are_skipped() {
        let fixture = fixture();
        let cwd = make_dir(&fixture.root.join("cwd"));
        let hints = ProjectHints {
            explicit: Some(fixture.root.join("does-not-exist")),
            claude_project_dir: Some(fixture.root.join("also-missing")),
            cwd: Some(cwd.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(detect(&hints, &fixture.home), location(&cwd, &cwd));
    }

    #[test]
    fn relative_explicit_hint_resolves_against_cwd() {
        let fixture = fixture();
        let cwd = make_dir(&fixture.root.join("cwd"));
        let nested = make_dir(&cwd.join("nested"));
        let hints = ProjectHints {
            explicit: Some(PathBuf::from("nested")),
            cwd: Some(cwd),
            ..ProjectHints::default()
        };

        assert_eq!(detect(&hints, &fixture.home), location(&nested, &nested));
    }

    #[test]
    fn cwd_at_home_means_global_only() {
        let fixture = fixture();
        let hints = ProjectHints {
            cwd: Some(fixture.home.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            ProjectLocation::global_only()
        );
    }

    #[test]
    fn cwd_at_filesystem_root_means_global_only() {
        let fixture = fixture();
        let hints = ProjectHints {
            cwd: Some(PathBuf::from("/")),
            ..ProjectHints::default()
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            ProjectLocation::global_only()
        );
    }

    #[test]
    fn no_hints_means_global_only() {
        let fixture = fixture();
        assert_eq!(
            detect(&ProjectHints::default(), &fixture.home),
            ProjectLocation::global_only()
        );
    }

    #[test]
    fn explicit_home_is_honoured() {
        let fixture = fixture();
        let hints = ProjectHints {
            explicit: Some(fixture.home.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            location(&fixture.home, &fixture.home)
        );
    }

    #[test]
    fn cwd_is_ambiguous_for_missing_root_and_home() {
        let fixture = fixture();
        assert!(cwd_is_ambiguous(None, &fixture.home));
        assert!(cwd_is_ambiguous(Some(Path::new("/")), &fixture.home));
        assert!(cwd_is_ambiguous(Some(&fixture.home), &fixture.home));
    }

    #[test]
    fn cwd_is_not_ambiguous_inside_a_project() {
        let fixture = fixture();
        let project = make_dir(&fixture.home.join("work/app"));
        assert!(!cwd_is_ambiguous(Some(&project), &fixture.home));
    }

    #[test]
    fn home_comparison_sees_through_symlinks() {
        let fixture = fixture();
        let link = fixture.root.join("home-link");
        std::os::unix::fs::symlink(&fixture.home, &link).unwrap();
        assert!(cwd_is_ambiguous(Some(&link), &fixture.home));
    }

    #[test]
    fn linked_worktree_maps_config_dir_to_main_checkout() {
        let fixture = fixture();
        let (main, worktree) = make_linked_worktree(&fixture.root);
        let hints = ProjectHints {
            cwd: Some(worktree.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(detect(&hints, &fixture.home), location(&main, &worktree));
    }

    #[test]
    fn subdirectory_of_linked_worktree_keeps_its_relative_path() {
        let fixture = fixture();
        let (main, worktree) = make_linked_worktree(&fixture.root);
        let subdir = make_dir(&worktree.join("packages/api"));
        let hints = ProjectHints {
            cwd: Some(subdir.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(
            detect(&hints, &fixture.home),
            location(&main.join("packages/api"), &subdir)
        );
    }

    #[test]
    fn worktree_with_relative_gitdir_is_mapped() {
        let fixture = fixture();
        let main = make_dir(&fixture.root.join("main"));
        let worktree_git_dir = make_dir(&main.join(".git/worktrees/wt"));
        fs::write(worktree_git_dir.join("commondir"), "../..").unwrap();
        let worktree = make_dir(&fixture.root.join("wt"));
        fs::write(worktree.join(".git"), "gitdir: ../main/.git/worktrees/wt").unwrap();
        let hints = ProjectHints {
            cwd: Some(worktree.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(detect(&hints, &fixture.home), location(&main, &worktree));
    }

    #[test]
    fn normal_checkout_is_not_mapped() {
        let fixture = fixture();
        let checkout = make_dir(&fixture.root.join("repo"));
        make_dir(&checkout.join(".git"));
        let subdir = make_dir(&checkout.join("src"));
        let hints = ProjectHints {
            cwd: Some(subdir.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(detect(&hints, &fixture.home), location(&subdir, &subdir));
    }

    #[test]
    fn submodule_is_not_mapped() {
        let fixture = fixture();
        let parent = make_dir(&fixture.root.join("parent"));
        make_dir(&parent.join(".git/modules/child"));
        let child = make_dir(&parent.join("child"));
        fs::write(child.join(".git"), "gitdir: ../.git/modules/child").unwrap();
        let hints = ProjectHints {
            cwd: Some(child.clone()),
            ..ProjectHints::default()
        };

        assert_eq!(detect(&hints, &fixture.home), location(&child, &child));
    }
}
