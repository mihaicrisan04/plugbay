//! Credentials in plugbay's keychain vault, keyed by server identity + account (never by config
//! name): OAuth logins under `plugbay-oauth`, static tokens from `plugbay login --token` under
//! `plugbay-token`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use anyhow::{Context, bail};
use rmcp::transport::auth::{
    AuthError, CredentialRefreshGuard, CredentialStore, StoredCredentials,
};
use serde::{Deserialize, Serialize};

use crate::config::{FORBIDDEN_HEADERS, ServerIdentity};
use crate::vault;

const OAUTH_KEYCHAIN_SERVICE: &str = "plugbay-oauth";
const TOKEN_KEYCHAIN_SERVICE: &str = "plugbay-token";
const AUTHORIZATION_HEADER: &str = "authorization";

/// One lock per keychain entry so two refreshes in this process never race and
/// burn a rotating refresh token.
static REFRESH_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// rmcp credential store backed by one keychain entry.
pub struct KeychainCredentialStore {
    entry_name: String,
}

impl KeychainCredentialStore {
    pub fn new(identity: &ServerIdentity, account: &str) -> Self {
        Self {
            entry_name: entry_name(identity, account),
        }
    }

    async fn read_entry(&self) -> anyhow::Result<Option<String>> {
        read_keychain(OAUTH_KEYCHAIN_SERVICE, self.entry_name.clone()).await
    }

    async fn write_entry(&self, value: String) -> anyhow::Result<()> {
        write_keychain(OAUTH_KEYCHAIN_SERVICE, self.entry_name.clone(), value).await
    }

    async fn delete_entry(&self) -> anyhow::Result<bool> {
        delete_keychain(OAUTH_KEYCHAIN_SERVICE, self.entry_name.clone()).await
    }

    fn refresh_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = REFRESH_LOCKS.lock();
        locks.entry(self.entry_name.clone()).or_default().clone()
    }
}

#[async_trait::async_trait]
impl CredentialStore for KeychainCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let Some(json) = self.read_entry().await.map_err(store_error)? else {
            return Ok(None);
        };
        let credentials = serde_json::from_str(&json).map_err(|error| {
            AuthError::CredentialStoreError(format!(
                "stored credentials for {} are unreadable ({error}); log in again",
                self.entry_name
            ))
        })?;
        Ok(Some(credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials).map_err(|error| {
            AuthError::CredentialStoreError(format!("serialising credentials: {error}"))
        })?;
        self.write_entry(json).await.map_err(store_error)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.delete_entry().await.map_err(store_error)?;
        Ok(())
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let guard = self.refresh_lock().lock_owned().await;
        Ok(Some(CredentialRefreshGuard::new(guard)))
    }
}

fn store_error(error: anyhow::Error) -> AuthError {
    AuthError::CredentialStoreError(format!("{error:#}"))
}

/// A static credential from `plugbay login --token`, sent as one header to the server it was stored for.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticToken {
    /// Lowercase header name: `authorization` unless `--header` chose another.
    pub header: String,
    /// The whole header value, e.g. `Bearer <token>`.
    pub value: String,
}

impl StaticToken {
    /// `Authorization: Bearer <token>`, or the bare token under `header` when one is given.
    pub fn new(token: &str, header: Option<&str>) -> anyhow::Result<Self> {
        let token = token.trim();
        if token.is_empty() {
            bail!("the token is empty");
        }
        if token.chars().any(char::is_control) {
            bail!("the token contains a line break or other control character");
        }

        let header = header.unwrap_or(AUTHORIZATION_HEADER).to_ascii_lowercase();
        reqwest::header::HeaderName::try_from(header.as_str())
            .with_context(|| format!("`{header}` is not a valid header name"))?;
        if FORBIDDEN_HEADERS.contains(&header.as_str()) {
            bail!("`{header}` cannot carry a token");
        }

        let value = if header == AUTHORIZATION_HEADER {
            format!("Bearer {token}")
        } else {
            token.to_string()
        };
        Ok(Self { header, value })
    }
}

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StaticToken")
            .field("header", &self.header)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// What `plugbay login` stored for one server identity + account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredLogin {
    None,
    OAuth,
    Token,
}

pub async fn save_token(
    identity: &ServerIdentity,
    account: &str,
    token: &StaticToken,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(token).context("serialising the token")?;
    write_keychain(TOKEN_KEYCHAIN_SERVICE, entry_name(identity, account), json).await
}

pub async fn load_token(
    identity: &ServerIdentity,
    account: &str,
) -> anyhow::Result<Option<StaticToken>> {
    let entry_name = entry_name(identity, account);
    let Some(json) = read_keychain(TOKEN_KEYCHAIN_SERVICE, entry_name.clone()).await? else {
        return Ok(None);
    };
    let token = serde_json::from_str(&json).with_context(|| {
        format!(
            "the stored token for {entry_name} is unreadable; run `plugbay login --token` again"
        )
    })?;
    Ok(Some(token))
}

/// A stored token wins over an OAuth login, matching the order upstream connections use them in.
pub async fn stored_login(identity: &ServerIdentity, account: &str) -> anyhow::Result<StoredLogin> {
    if load_token(identity, account).await?.is_some() {
        return Ok(StoredLogin::Token);
    }
    if has_stored_credentials(identity, account).await? {
        return Ok(StoredLogin::OAuth);
    }
    Ok(StoredLogin::None)
}

/// True when an OAuth token (possibly expired but refreshable) is stored for this identity + account.
pub async fn has_stored_credentials(
    identity: &ServerIdentity,
    account: &str,
) -> anyhow::Result<bool> {
    let store = KeychainCredentialStore::new(identity, account);
    let credentials = store.load().await?;
    Ok(credentials.is_some_and(|credentials| credentials.token_response.is_some()))
}

/// Deletes only the stored token, leaving an OAuth login alone. Returns false when there was none.
pub async fn forget_token(identity: &ServerIdentity, account: &str) -> anyhow::Result<bool> {
    delete_keychain(TOKEN_KEYCHAIN_SERVICE, entry_name(identity, account)).await
}

/// Deletes the stored OAuth login and token. Returns false when there was neither.
pub async fn forget(identity: &ServerIdentity, account: &str) -> anyhow::Result<bool> {
    let forgot_oauth = KeychainCredentialStore::new(identity, account)
        .delete_entry()
        .await?;
    let forgot_token =
        delete_keychain(TOKEN_KEYCHAIN_SERVICE, entry_name(identity, account)).await?;
    Ok(forgot_oauth || forgot_token)
}

fn entry_name(identity: &ServerIdentity, account: &str) -> String {
    format!("{}#{account}", identity.0)
}

async fn read_keychain(
    service: &'static str,
    entry_name: String,
) -> anyhow::Result<Option<String>> {
    tokio::task::spawn_blocking(move || vault::read(service, &entry_name))
        .await
        .context("keychain task failed")?
}

async fn write_keychain(
    service: &'static str,
    entry_name: String,
    value: String,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || vault::write(service, &entry_name, &value))
        .await
        .context("keychain task failed")?
}

async fn delete_keychain(service: &'static str, entry_name: String) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || vault::delete(service, &entry_name))
        .await
        .context("keychain task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_name_binds_identity_and_account() {
        let identity = ServerIdentity("https://mcp.linear.app/mcp".to_string());
        let store = KeychainCredentialStore::new(&identity, "initech");
        assert_eq!(store.entry_name, "https://mcp.linear.app/mcp#initech");
    }

    #[tokio::test]
    async fn refresh_guard_is_shared_per_entry() {
        let identity = ServerIdentity("https://example.test/mcp".to_string());
        let first = KeychainCredentialStore::new(&identity, "a");
        let same_entry = KeychainCredentialStore::new(&identity, "a");
        let other_entry = KeychainCredentialStore::new(&identity, "b");

        let _guard = first.acquire_refresh_guard().await.unwrap();
        assert!(same_entry.refresh_lock().try_lock().is_err());
        assert!(other_entry.refresh_lock().try_lock().is_ok());
    }

    #[test]
    fn token_defaults_to_bearer_authorization() {
        let token = StaticToken::new("  test-token\n", None).unwrap();
        assert_eq!(token.header, "authorization");
        assert_eq!(token.value, "Bearer test-token");
    }

    #[test]
    fn token_under_custom_header_is_sent_bare() {
        let token = StaticToken::new("test-token", Some("X-API-Key")).unwrap();
        assert_eq!(token.header, "x-api-key");
        assert_eq!(token.value, "test-token");
    }

    #[test]
    fn token_rejects_empty_values_line_breaks_and_framing_headers() {
        assert!(StaticToken::new("   ", None).is_err());
        assert!(StaticToken::new("test\ntoken", None).is_err());
        assert!(StaticToken::new("test-token", Some("Host")).is_err());
        assert!(StaticToken::new("test-token", Some("bad header")).is_err());
    }

    #[test]
    fn token_debug_hides_the_value() {
        let token = StaticToken::new("test-token", None).unwrap();
        assert!(!format!("{token:?}").contains("test-token"));
    }
}
