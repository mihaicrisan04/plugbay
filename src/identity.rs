//! Who each login really is. When an upstream for a registry server connects, the daemon calls the
//! server's whoami tool (`registry::Whoami`) and records a short label per identity + account in
//! `accounts.toml`. Commands show it next to the account label, two labels that turn out to be the
//! same login get flagged, and `expect` in account metadata turns a mismatch into an off server.

mod answer;

pub use self::answer::one_safe_line;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};

use crate::config::{ResolvedServer, ServerIdentity};
use crate::paths::{Paths, state_file};
use crate::registry::{self, Whoami};
use crate::upstream::Upstream;

const RECHECK_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(10);
/// Account metadata key holding text the verified login must contain. A rule, not shown as metadata.
pub const EXPECT_KEY: &str = "expect";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedAccount {
    /// Sanitised label from the whoami answer, e.g. `Initech`. Also what duplicates are compared by.
    pub shown: String,
    /// Unix seconds.
    pub checked_at: u64,
}

impl VerifiedAccount {
    fn is_fresh(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) < RECHECK_AFTER.as_secs()
    }
}

type Records = BTreeMap<String, VerifiedAccount>;

/// Every recorded login, keyed `<identity>#<account>` like the keychain entries.
#[derive(Debug, Default, Clone)]
pub struct VerifiedAccounts {
    records: Records,
    /// The file exists but couldn't be read, so `expect` can't be checked and must not pass.
    unreadable: bool,
}

impl VerifiedAccounts {
    /// Blocking read; a missing file means nothing is recorded yet.
    pub fn read(paths: &Paths) -> anyhow::Result<Self> {
        let records = state_file::read_toml(&paths.accounts_file())?;
        Ok(Self {
            records,
            unreadable: false,
        })
    }

    /// An unreadable file is logged and shows nothing, but keeps servers with `expect` off.
    pub fn read_or_empty(paths: &Paths) -> Self {
        Self::read(paths).unwrap_or_else(|error| {
            tracing::warn!(
                error = format!("{error:#}"),
                "the verified accounts file is unreadable; servers with `expect` stay off until it is rewritten"
            );
            Self {
                records: Records::new(),
                unreadable: true,
            }
        })
    }

    pub async fn load(paths: &Paths) -> Self {
        let paths = paths.clone();
        match tokio::task::spawn_blocking(move || Self::read_or_empty(&paths)).await {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(%error, "reading the verified accounts file failed");
                Self::default()
            }
        }
    }

    pub fn get(&self, identity: &ServerIdentity, account: &str) -> Option<&VerifiedAccount> {
        self.records.get(&record_key(identity, account))
    }

    pub fn shown(&self, identity: &ServerIdentity, account: &str) -> Option<&str> {
        self.get(identity, account)
            .map(|record| record.shown.as_str())
    }

    /// Other account labels of the same server whose login turned out to be this one's.
    pub fn same_login_as(&self, identity: &ServerIdentity, account: &str) -> Vec<String> {
        let Some(shown) = self.shown(identity, account) else {
            return Vec::new();
        };
        let mut accounts = Vec::new();
        for (key, record) in &self.records {
            let Some((record_identity, record_account)) = key.rsplit_once('#') else {
                continue;
            };
            if record_identity == identity.0 && record_account != account && record.shown == shown {
                accounts.push(record_account.to_string());
            }
        }
        accounts
    }
}

#[cfg(test)]
impl VerifiedAccounts {
    /// `(identity, account, shown)` records, all checked at time 0.
    pub(crate) fn test_with(records: &[(&str, &str, &str)]) -> Self {
        let records = records
            .iter()
            .map(|(identity, account, shown)| {
                let record = VerifiedAccount {
                    shown: shown.to_string(),
                    checked_at: 0,
                };
                (format!("{identity}#{account}"), record)
            })
            .collect();
        Self {
            records,
            unreadable: false,
        }
    }
}

/// Matches the keychain entry name, so a record follows exactly one stored login.
fn record_key(identity: &ServerIdentity, account: &str) -> String {
    format!("{}#{account}", identity.0)
}

/// The text from `[accounts.<server>.<account>] expect = "..."`, if set.
pub fn expected_account(server: &ResolvedServer) -> Option<&str> {
    server
        .account_metadata
        .get(EXPECT_KEY)
        .map(|expected| expected.trim())
        .filter(|expected| !expected.is_empty())
}

/// Why `server` must stay off: its verified login doesn't contain what the project expects, or
/// the record of who it is can't be read. A login nobody verified yet passes, since there is
/// nothing to compare.
pub fn expectation_failure(server: &ResolvedServer, verified: &VerifiedAccounts) -> Option<String> {
    let expected = expected_account(server)?;
    let name = server.display_name();
    if verified.unreadable {
        return Some(format!(
            "plugbay can't read its record of who {name} is logged in as, so it can't check for \
             {expected}; ask the human to delete `accounts.toml` in plugbay's state directory, \
             it is rebuilt on the next connection"
        ));
    }
    let shown = verified.shown(&server.identity, &server.account)?;
    if shown.to_lowercase().contains(&expected.to_lowercase()) {
        return None;
    }
    Some(format!(
        "{name} is logged in as {shown}, but this project expects {expected}; \
         ask the human to run `plugbay logout {name} && plugbay login {name}`"
    ))
}

/// Checks who the login behind a newly connected upstream is, unless a recent answer is on record.
/// Runs in the background; `on_change` fires when the recorded answer changed.
pub fn spawn_check(
    paths: &Paths,
    upstream: Arc<Upstream>,
    on_change: impl FnOnce() + Send + 'static,
) {
    let Some(whoami) = registry::whoami_for(&upstream.key().identity) else {
        return;
    };
    tokio::spawn({
        let paths = paths.clone();
        async move {
            let key = upstream.key();
            let recorded = VerifiedAccounts::load(&paths).await;
            let previous = recorded.get(&key.identity, &key.account).cloned();
            if previous
                .as_ref()
                .is_some_and(|record| record.is_fresh(unix_now()))
            {
                return;
            }

            let checked = tokio::time::timeout(WHOAMI_TIMEOUT, check_upstream(whoami, &upstream));
            let shown = match checked.await {
                Ok(Ok(shown)) => shown,
                Ok(Err(error)) => {
                    tracing::info!(
                        identity = key.identity.masked(),
                        account = key.account,
                        error = format!("{error:#}"),
                        "could not verify the account"
                    );
                    return;
                }
                Err(_) => {
                    tracing::info!(
                        identity = key.identity.masked(),
                        account = key.account,
                        "verifying the account timed out"
                    );
                    return;
                }
            };
            tracing::info!(
                identity = key.identity.masked(),
                account = key.account,
                shown,
                "verified the account"
            );

            let changed = previous.is_none_or(|record| record.shown != shown);
            if let Err(error) = record(&paths, &key.identity, &key.account, shown).await {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "could not save the verified account"
                );
                return;
            }
            if changed {
                on_change();
            }
        }
    });
}

async fn check_upstream(whoami: &Whoami, upstream: &Upstream) -> anyhow::Result<String> {
    let tools = upstream.tools().await?;
    ask_whoami(whoami, &tools, |request| upstream.call_tool(request)).await
}

/// Calls the whoami tool through `call` and names the account from its answer.
async fn ask_whoami<Call, Answer>(
    whoami: &Whoami,
    tools: &[Tool],
    call: Call,
) -> anyhow::Result<String>
where
    Call: FnOnce(CallToolRequestParams) -> Answer,
    Answer: Future<Output = anyhow::Result<CallToolResult>>,
{
    let Some(tool) = tools.iter().find(|tool| tool.name == whoami.tool) else {
        bail!("the server has no `{}` tool", whoami.tool);
    };
    let read_only_hint = tool
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.read_only_hint);
    if read_only_hint == Some(false) {
        bail!(
            "`{}` says it changes things, so plugbay won't call it",
            whoami.tool
        );
    }

    let arguments: JsonObject = serde_json::from_str(whoami.arguments)
        .with_context(|| format!("arguments for `{}`", whoami.tool))?;
    let request = CallToolRequestParams::new(whoami.tool).with_arguments(arguments);
    let result = call(request).await?;
    if result.is_error == Some(true) {
        bail!("`{}` answered with an error", whoami.tool);
    }
    answer::shown_from(&result, whoami.show)
}

async fn record(
    paths: &Paths,
    identity: &ServerIdentity,
    account: &str,
    shown: String,
) -> anyhow::Result<()> {
    let key = record_key(identity, account);
    let record = VerifiedAccount {
        shown,
        checked_at: unix_now(),
    };
    update(paths, move |records| {
        records.insert(key, record);
        true
    })
    .await?;
    Ok(())
}

/// Drops what plugbay learned about a login, so the next login is verified afresh. Returns
/// whether anything was recorded.
pub async fn forget(
    paths: &Paths,
    identity: &ServerIdentity,
    account: &str,
) -> anyhow::Result<bool> {
    let key = record_key(identity, account);
    update(paths, move |records| records.remove(&key).is_some()).await
}

/// Read, change, and write back if `change` says something changed.
async fn update(
    paths: &Paths,
    change: impl FnOnce(&mut Records) -> bool + Send + 'static,
) -> anyhow::Result<bool> {
    let path = paths.accounts_file();
    tokio::task::spawn_blocking(move || state_file::update_toml(&path, change))
        .await
        .context("updating the verified accounts file")?
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs())
}

#[cfg(test)]
mod tests {
    use rmcp::model::{ContentBlock, ToolAnnotations};
    use tempfile::TempDir;

    use super::*;

    const LINEAR: &str = "https://mcp.linear.app/mcp";

    fn linear() -> ServerIdentity {
        ServerIdentity(LINEAR.to_string())
    }

    fn linear_server(account: &str, expect: Option<&str>) -> ResolvedServer {
        let mut server = ResolvedServer::test_http("linear", account, LINEAR);
        if let Some(expect) = expect {
            server
                .account_metadata
                .insert(EXPECT_KEY.to_string(), expect.to_string());
        }
        server
    }

    const WHOAMI: Whoami = Whoami {
        tool: "get_workspace",
        arguments: r#"{"scope":"me"}"#,
        show: &["/name"],
    };

    fn tool(name: &'static str) -> Tool {
        Tool::new(name, "test tool", Arc::new(JsonObject::new()))
    }

    #[tokio::test]
    async fn records_survive_a_round_trip_and_forget_removes_them() {
        let temp = TempDir::new().unwrap();
        let paths = Paths::for_test(temp.path());

        record(&paths, &linear(), "personal", "Mihai".to_string())
            .await
            .unwrap();
        record(&paths, &linear(), "initech", "Initech".to_string())
            .await
            .unwrap();

        let accounts = VerifiedAccounts::read(&paths).unwrap();
        assert_eq!(accounts.shown(&linear(), "personal"), Some("Mihai"));
        assert_eq!(accounts.shown(&linear(), "initech"), Some("Initech"));
        assert!(
            accounts
                .get(&linear(), "personal")
                .unwrap()
                .is_fresh(unix_now())
        );

        assert!(forget(&paths, &linear(), "personal").await.unwrap());
        assert!(!forget(&paths, &linear(), "personal").await.unwrap());
        let accounts = VerifiedAccounts::read(&paths).unwrap();
        assert_eq!(accounts.shown(&linear(), "personal"), None);
        assert_eq!(accounts.shown(&linear(), "initech"), Some("Initech"));
    }

    #[test]
    fn missing_file_means_nothing_recorded() {
        let temp = TempDir::new().unwrap();
        let accounts = VerifiedAccounts::read(&Paths::for_test(temp.path())).unwrap();
        assert!(accounts.records.is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_file_fails_expect_closed_until_a_check_rewrites_it() {
        let temp = TempDir::new().unwrap();
        let paths = Paths::for_test(temp.path());
        paths.ensure_state_dir().unwrap();
        std::fs::write(paths.accounts_file(), "[\"torn").unwrap();

        let accounts = VerifiedAccounts::load(&paths).await;
        let expecting = linear_server("personal", Some("mihai"));
        assert!(
            expectation_failure(&expecting, &accounts)
                .unwrap()
                .contains("can't check for mihai")
        );
        assert_eq!(
            expectation_failure(&linear_server("personal", None), &accounts),
            None
        );

        record(&paths, &linear(), "personal", "Mihai".to_string())
            .await
            .unwrap();
        let accounts = VerifiedAccounts::load(&paths).await;
        assert_eq!(expectation_failure(&expecting, &accounts), None);
    }

    #[test]
    fn records_older_than_a_day_are_stale() {
        let record = VerifiedAccount {
            shown: "Acme".to_string(),
            checked_at: 1_000,
        };
        assert!(record.is_fresh(1_000 + 60));
        assert!(!record.is_fresh(1_000 + RECHECK_AFTER.as_secs()));
    }

    #[test]
    fn two_labels_with_the_same_login_are_duplicates() {
        let accounts = VerifiedAccounts::test_with(&[
            (LINEAR, "personal", "Initech"),
            (LINEAR, "initech", "Initech"),
            (LINEAR, "client", "Client Co"),
            ("https://mcp.sentry.dev/mcp", "initech", "Initech"),
        ]);
        assert_eq!(
            accounts.same_login_as(&linear(), "personal"),
            vec!["initech"]
        );
        assert_eq!(
            accounts.same_login_as(&linear(), "initech"),
            vec!["personal"]
        );
        assert!(accounts.same_login_as(&linear(), "client").is_empty());
        assert!(accounts.same_login_as(&linear(), "unknown").is_empty());
    }

    #[test]
    fn expectation_matches_case_insensitively_as_a_substring() {
        let accounts = VerifiedAccounts::test_with(&[(LINEAR, "initech", "Initech")]);
        let server = linear_server("initech", Some("initech"));
        assert_eq!(expectation_failure(&server, &accounts), None);
    }

    #[test]
    fn mismatched_expectation_says_how_to_fix_it() {
        let accounts = VerifiedAccounts::test_with(&[(LINEAR, "personal", "Initech")]);
        let server = linear_server("personal", Some("mihaicrisan"));
        assert_eq!(
            expectation_failure(&server, &accounts).unwrap(),
            "linear:personal is logged in as Initech, but this project expects mihaicrisan; \
             ask the human to run `plugbay logout linear:personal && plugbay login linear:personal`"
        );
    }

    #[test]
    fn unverified_or_unconstrained_logins_pass() {
        let accounts = VerifiedAccounts::test_with(&[(LINEAR, "personal", "Initech")]);
        assert_eq!(
            expectation_failure(&linear_server("other", Some("x")), &accounts),
            None
        );
        assert_eq!(
            expectation_failure(&linear_server("personal", None), &accounts),
            None
        );
        assert_eq!(
            expectation_failure(&linear_server("personal", Some("  ")), &accounts),
            None
        );
    }

    #[tokio::test]
    async fn whoami_calls_the_registry_tool_with_its_arguments() {
        let tools = vec![tool("list_issues"), tool("get_workspace")];
        let shown = ask_whoami(&WHOAMI, &tools, |request| async move {
            assert_eq!(request.name, "get_workspace");
            assert_eq!(
                request.arguments,
                Some(serde_json::from_str(r#"{"scope":"me"}"#).unwrap())
            );
            Ok(CallToolResult::success(vec![ContentBlock::text(
                r#"{"id":"1","name":"Initech"}"#,
            )]))
        })
        .await
        .unwrap();
        assert_eq!(shown, "Initech");
    }

    #[tokio::test]
    async fn whoami_refuses_a_tool_marked_as_writing() {
        let writing =
            tool("get_workspace").with_annotations(ToolAnnotations::new().read_only(false));
        let result = ask_whoami(&WHOAMI, &[writing], |_| async {
            panic!("must not be called")
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("won't call it"));
    }

    #[tokio::test]
    async fn whoami_fails_without_the_tool_or_on_an_error_answer() {
        let missing = ask_whoami(&WHOAMI, &[tool("list_issues")], |_| async {
            panic!("must not be called")
        })
        .await;
        assert!(missing.is_err());

        let failed = ask_whoami(&WHOAMI, &[tool("get_workspace")], |_| async {
            Ok(CallToolResult::error(vec![ContentBlock::text("nope")]))
        })
        .await;
        assert!(failed.is_err());
    }
}
