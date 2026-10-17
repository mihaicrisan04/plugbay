//! Every secret plugbay stores itself (OAuth logins, `login --token` tokens, `${keychain:plugbay/...}`
//! refs) lives in ONE keychain item, as a JSON map.
//!
//! macOS asks the user before an app it doesn't recognise reads a keychain item, once per item.
//! An unsigned binary is a new app after every upgrade, so one item per secret meant one prompt per
//! secret per upgrade; one item means one "Always Allow". Entries written by older versions as
//! their own items are copied in the first time they are read.
//!
//! Blocking: call from `spawn_blocking`. Reads are cached per process; every write re-reads the
//! item under a file lock first, so the daemon and CLI commands never lose each other's changes.

use std::collections::BTreeMap;

use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, state_file};
use crate::secrets::keychain;

const VAULT_SERVICE: &str = "plugbay-vault";
/// Tests point this at a file so they never touch (or prompt for) the real keychain.
const TEST_FILE_VARIABLE: &str = "PLUGBAY_INSECURE_TEST_VAULT";
const VAULT_ACCOUNT: &str = "secrets";
/// Keychain services whose entries belong in the vault; anything else is the user's own item.
const OWNED_SERVICES: [&str; 3] = ["plugbay", "plugbay-oauth", "plugbay-token"];

static CACHE: Mutex<Option<Contents>> = Mutex::new(None);

#[derive(Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Contents {
    /// `<service>/<name>` → secret.
    entries: BTreeMap<String, String>,
}

/// Names only: the values are secrets.
impl std::fmt::Debug for Contents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.entries.keys()).finish()
    }
}

/// True for the keychain services plugbay owns, which the vault holds.
pub fn owns(service: &str) -> bool {
    OWNED_SERVICES.contains(&service)
}

pub fn read(service: &str, name: &str) -> anyhow::Result<Option<String>> {
    let key = entry_key(service, name);
    if let Some(secret) = cached()?.entries.get(&key) {
        return Ok(Some(secret.clone()));
    }
    if test_file().is_some() {
        return Ok(None);
    }
    let Some(legacy) = keychain::read(service, name)? else {
        return Ok(None);
    };
    move_into_vault(service, name, &legacy);
    Ok(Some(legacy))
}

pub fn write(service: &str, name: &str, secret: &str) -> anyhow::Result<()> {
    update(|contents| {
        contents
            .entries
            .insert(entry_key(service, name), secret.to_string());
    })
}

/// Returns false when there was nothing to delete, in the vault or as an old separate item.
pub fn delete(service: &str, name: &str) -> anyhow::Result<bool> {
    let mut removed = false;
    update(|contents| removed = contents.entries.remove(&entry_key(service, name)).is_some())?;
    if test_file().is_some() {
        return Ok(removed);
    }
    let removed_legacy = keychain::delete(service, name)?;
    Ok(removed || removed_legacy)
}

/// Makes the next read go to the keychain, e.g. after another process stored a login.
pub fn forget_cached() {
    *CACHE.lock() = None;
}

fn entry_key(service: &str, name: &str) -> String {
    format!("{service}/{name}")
}

fn cached() -> anyhow::Result<Contents> {
    if let Some(contents) = CACHE.lock().as_ref() {
        return Ok(contents.clone());
    }
    let contents = load()?;
    *CACHE.lock() = Some(contents.clone());
    Ok(contents)
}

fn load() -> anyhow::Result<Contents> {
    let json = match test_file() {
        Some(file) => match std::fs::read_to_string(&file) {
            Ok(json) => Some(json),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
        },
        None => keychain::read(VAULT_SERVICE, VAULT_ACCOUNT)?,
    };
    let Some(json) = json else {
        return Ok(Contents::default());
    };
    serde_json::from_str(&json).context("plugbay's keychain item is unreadable")
}

fn save(json: &str) -> anyhow::Result<()> {
    match test_file() {
        Some(file) => state_file::write_atomic(&file, json),
        None => keychain::write(VAULT_SERVICE, VAULT_ACCOUNT, json),
    }
}

fn test_file() -> Option<std::path::PathBuf> {
    std::env::var_os(TEST_FILE_VARIABLE).map(std::path::PathBuf::from)
}

/// Read-modify-write of the whole item under an exclusive lock across processes.
fn update(change: impl FnOnce(&mut Contents)) -> anyhow::Result<()> {
    let _lock = lock()?;
    let mut contents = load()?;
    let before = contents.clone();
    change(&mut contents);
    if contents != before {
        let json = serde_json::to_string(&contents).context("serialising the vault")?;
        save(&json)?;
    }
    *CACHE.lock() = Some(contents);
    Ok(())
}

/// Copies an old separate item in. The old item stays: deleting it could prompt once more, and
/// it is never read again once the vault has the entry. A failed copy is only logged.
fn move_into_vault(service: &str, name: &str, secret: &str) {
    match write(service, name, secret) {
        Ok(()) => tracing::info!(service, name, "copied a keychain item into plugbay's vault"),
        Err(error) => {
            tracing::warn!(
                service,
                name,
                error = format!("{error:#}"),
                "could not copy a keychain item into the vault"
            )
        }
    }
}

fn lock() -> anyhow::Result<std::fs::File> {
    state_file::lock(&Paths::from_env()?.vault_lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plugbays_own_services_go_in_the_vault() {
        assert!(owns("plugbay"));
        assert!(owns("plugbay-oauth"));
        assert!(owns("plugbay-token"));
        assert!(!owns("github"));
        assert!(!owns(VAULT_SERVICE));
    }

    #[test]
    fn entries_are_keyed_by_service_and_name() {
        let mut contents = Contents::default();
        contents.entries.insert(
            entry_key("plugbay-token", "https://mcp.example.test/mcp#work"),
            "test-token".to_string(),
        );
        let json = serde_json::to_string(&contents).unwrap();
        assert_eq!(
            json,
            r#"{"entries":{"plugbay-token/https://mcp.example.test/mcp#work":"test-token"}}"#
        );
        assert_eq!(serde_json::from_str::<Contents>(&json).unwrap(), contents);
    }
}
