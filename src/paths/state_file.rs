//! Small TOML files in the state dir (`index.toml`, `accounts.toml`, `trust.toml`) that the daemon
//! and CLI commands both change. Every change locks a sibling `.lock` file across processes and
//! threads, re-reads, and replaces the file atomically, so nobody loses anyone else's update.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Makes temporary names unique between threads of one process; the pid covers other processes.
static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Holds an exclusive lock on `lock_path` until dropped. `File::lock` is per open file, so it
/// also serialises threads of the same process.
pub fn lock(lock_path: &Path) -> anyhow::Result<File> {
    if let Some(directory) = lock_path.parent() {
        super::create_private_dir(directory)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(file)
}

/// The parsed file, or `T::default()` when it doesn't exist. A file that doesn't parse is an
/// error: callers decide whether that is fatal or something to warn about.
pub fn read_toml<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Locks, reads, applies `change` and writes back when it returns true. A file that doesn't
/// parse is logged and replaced, so one bad write can't block every later update. Blocking.
pub fn update_toml<T: Serialize + DeserializeOwned + Default>(
    path: &Path,
    change: impl FnOnce(&mut T) -> bool,
) -> anyhow::Result<bool> {
    let _lock = lock(&lock_path_for(path))?;
    let mut contents = match read_toml(path) {
        Ok(contents) => contents,
        Err(error) if path.exists() => {
            tracing::warn!(
                error = format!("{error:#}"),
                "replacing an unreadable state file"
            );
            T::default()
        }
        Err(error) => return Err(error),
    };
    if !change(&mut contents) {
        return Ok(false);
    }
    let text =
        toml::to_string(&contents).with_context(|| format!("serialising {}", path.display()))?;
    write_atomic(path, &text)?;
    Ok(true)
}

/// Writes a private (0600) temporary file next to `path`, syncs it and renames it over `path`,
/// so readers see the old file or the new one, never half of one.
pub fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(directory) = path.parent() {
        super::create_private_dir(directory)?;
    }
    let temporary_path = temporary_path_for(path);
    let replaced = write_private(&temporary_path, contents).and_then(|()| {
        std::fs::rename(&temporary_path, path)
            .with_context(|| format!("replacing {}", path.display()))
    });
    if replaced.is_err() {
        remove_leftover(&temporary_path);
    }
    replaced
}

fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

fn remove_leftover(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "could not remove a temporary file")
        }
    }
}

/// `accounts.toml` → `accounts.lock`.
pub fn lock_path_for(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

fn temporary_path_for(path: &Path) -> PathBuf {
    let counter = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{counter}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct Numbers {
        #[serde(default)]
        numbers: BTreeSet<u32>,
    }

    #[test]
    fn concurrent_updates_never_lose_each_other() {
        let temp = TempDir::new().unwrap();
        let path = Arc::new(temp.path().join("state/numbers.toml"));
        let writers: Vec<_> = (0..16)
            .map(|number| {
                let path = path.clone();
                std::thread::spawn(move || {
                    update_toml(&path, |file: &mut Numbers| file.numbers.insert(number)).unwrap()
                })
            })
            .collect();
        for writer in writers {
            assert!(writer.join().unwrap());
        }

        let file: Numbers = read_toml(&path).unwrap();
        assert_eq!(file.numbers, (0..16).collect());
    }

    #[test]
    fn an_unreadable_file_is_an_error_for_readers_and_replaced_by_updates() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("numbers.toml");
        std::fs::write(&path, "numbers = [1, 2").unwrap();
        assert!(read_toml::<Numbers>(&path).is_err());

        update_toml(&path, |file: &mut Numbers| file.numbers.insert(7)).unwrap();

        let file: Numbers = read_toml(&path).unwrap();
        assert_eq!(file.numbers, BTreeSet::from([7]));
    }

    #[test]
    fn an_unchanged_update_leaves_no_file_behind() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("numbers.toml");
        assert!(!update_toml(&path, |_: &mut Numbers| false).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn atomic_writes_are_private_and_leave_no_temporary_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("trust.toml");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["trust.toml"]);
    }
}
