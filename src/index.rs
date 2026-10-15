//! Every directory holding a plugbay config file that plugbay has seen, so commands can work
//! from anywhere: `ls --all`, `accounts` and `login` without a target look at all of them.
//!
//! Directories are recorded whenever config resolves (agent sessions and CLI commands), like
//! mise's tracked configs. Losing an entry is harmless: it comes back the next time it resolves.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigScope, ResolvedConfig};
use crate::paths::Paths;

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    #[serde(default)]
    directories: BTreeSet<PathBuf>,
}

/// Records the directories of `config`'s project files. Failures are logged, never fatal.
pub async fn record(paths: &Paths, config: &ResolvedConfig) {
    let directories = config_directories(config);
    if directories.is_empty() {
        return;
    }
    let paths = paths.clone();
    let recorded = tokio::task::spawn_blocking(move || add_directories(&paths, directories)).await;
    match recorded {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            error = format!("{error:#}"),
            "updating the config index failed"
        ),
        Err(error) => tracing::warn!(%error, "updating the config index failed"),
    }
}

/// Recorded directories that still hold a config file, sorted so parents come before children.
pub fn tracked_directories(paths: &Paths) -> anyhow::Result<Vec<PathBuf>> {
    let index = read(paths)?;
    let directories = index
        .directories
        .into_iter()
        .filter(|directory| holds_config_file(directory))
        .collect();
    Ok(directories)
}

fn config_directories(config: &ResolvedConfig) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for file in &config.files {
        if file.scope == ConfigScope::Global {
            continue;
        }
        if let Some(directory) = file.path.parent() {
            directories.insert(directory.to_path_buf());
        }
    }
    directories
}

fn add_directories(paths: &Paths, directories: BTreeSet<PathBuf>) -> anyhow::Result<()> {
    let mut index = read(paths)?;
    let before = index.directories.len();
    index.directories.extend(directories);
    if index.directories.len() == before {
        return Ok(());
    }
    write(paths, &index)
}

fn holds_config_file(directory: &Path) -> bool {
    directory.join(crate::config::PROJECT_FILE).is_file()
        || directory.join(crate::config::LOCAL_FILE).is_file()
}

fn read(paths: &Paths) -> anyhow::Result<IndexFile> {
    let path = paths.index_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexFile::default());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Written to a temporary file and renamed, so a concurrent reader never sees half a file.
fn write(paths: &Paths, index: &IndexFile) -> anyhow::Result<()> {
    paths.ensure_state_dir()?;
    let path = paths.index_file();
    let text = toml::to_string(index).context("serialising the config index")?;
    let temporary = path.with_extension(format!("toml.{}", std::process::id()));
    std::fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::config::ConfigFile;

    fn config_with_files(files: Vec<ConfigFile>) -> ResolvedConfig {
        ResolvedConfig::test_with(files, Vec::new())
    }

    #[tokio::test]
    async fn records_project_directories_but_not_the_global_config() {
        let temp = TempDir::new().unwrap();
        let paths = Paths::for_test(temp.path());
        let project = temp.path().join("work/app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("plugbay.toml"), "").unwrap();
        let global = paths.global_config();
        let config = config_with_files(vec![
            ConfigFile::test(global.to_str().unwrap(), ConfigScope::Global, None),
            ConfigFile::test(
                project.join("plugbay.toml").to_str().unwrap(),
                ConfigScope::Project,
                None,
            ),
        ]);

        record(&paths, &config).await;

        assert_eq!(tracked_directories(&paths).unwrap(), vec![project]);
    }

    #[tokio::test]
    async fn directories_whose_config_was_removed_are_skipped() {
        let temp = TempDir::new().unwrap();
        let paths = Paths::for_test(temp.path());
        let project = temp.path().join("gone");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("plugbay.local.toml"), "").unwrap();
        let config = config_with_files(vec![ConfigFile::test(
            project.join("plugbay.local.toml").to_str().unwrap(),
            ConfigScope::Local,
            None,
        )]);
        record(&paths, &config).await;

        std::fs::remove_file(project.join("plugbay.local.toml")).unwrap();

        assert!(tracked_directories(&paths).unwrap().is_empty());
    }
}
