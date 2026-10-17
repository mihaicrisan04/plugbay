//! Questions plugbay puts to the human through the agent's client (MCP elicitation): trusting
//! config that runs commands or reads secrets, and switching a server to another account.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use rmcp::model::{
    ElicitRequestParams, ElicitationAction, ElicitationSchema, EnumSchema,
    PrimitiveSchemaDefinition,
};
use rmcp::service::ElicitationMode;
use rmcp::{Peer, RoleServer};

use super::{ServerStatus, Session, Shared};
use crate::config::ConfigFile;

const ANSWER_FIELD: &str = "decision";

/// The untrusted files a prompt was about, with the fingerprints of their executable parts.
pub(super) type TrustPromptKey = Vec<(PathBuf, Option<String>)>;

/// Config waiting for the human's trust decision.
struct PendingTrust {
    config_dir: PathBuf,
    files: Vec<ConfigFile>,
    /// One line per server that would run: `name:account: command args`.
    commands: Vec<String>,
}

impl PendingTrust {
    fn key(&self) -> TrustPromptKey {
        self.files
            .iter()
            .map(|file| (file.path.clone(), file.executable_fingerprint.clone()))
            .collect()
    }
}

impl Session {
    /// Asks once per version of untrusted config, so editing a command mid-session asks again
    /// rather than quietly keeping the old tools. Without elicitation the instructions and
    /// `plugbay__status` point the agent to `plugbay trust`.
    pub(super) fn offer_trust_prompt(&self) {
        if !self.initialized.load(Ordering::SeqCst) {
            return;
        }
        let Some(peer) = self.peer.get().filter(|peer| supports_forms(peer)).cloned() else {
            return;
        };
        let Some(pending) = self.pending_trust() else {
            return;
        };
        if !self.shared.trust_prompted.lock().insert(pending.key()) {
            return;
        }
        let shared = self.shared.clone();
        tokio::spawn(async move {
            if ask_for_trust(&shared, &peer, pending).await {
                shared.reload_all().await;
            }
        });
    }

    fn pending_trust(&self) -> Option<PendingTrust> {
        let state = self.state.lock();
        let resolved = state.resolved.as_ref()?;
        let config = resolved.outcome.as_ref().ok()?;
        let config_dir = resolved.location.config_dir.clone()?;

        let files: Vec<ConfigFile> = self
            .shared
            .trust
            .lock()
            .untrusted_files(config)
            .into_iter()
            .cloned()
            .collect();
        if files.is_empty() {
            return None;
        }
        let commands = state
            .servers
            .iter()
            .filter(|active| matches!(active.status, ServerStatus::Untrusted))
            .map(|active| {
                format!(
                    "  {}: {}",
                    active.server.display_name(),
                    active.server.transport.target()
                )
            })
            .collect();
        Some(PendingTrust {
            config_dir,
            files,
            commands,
        })
    }
}

/// Returns true when the human granted anything.
async fn ask_for_trust(shared: &Shared, peer: &Peer<RoleServer>, pending: PendingTrust) -> bool {
    let message = format!(
        "plugbay: the config for {} runs local commands or reads secrets:\n{}\n\nAllow them?",
        pending.config_dir.display(),
        pending.commands.join("\n")
    );
    let choices = [
        ("allow_once", "Allow for this session"),
        ("allow_always", "Always allow in this folder"),
        ("deny", "Deny"),
    ];
    let decision = ask_human(peer, message, &choices).await;

    let mut trust = shared.trust.lock();
    match decision.as_deref() {
        Some("allow_once") => {
            for file in &pending.files {
                trust.grant_for_session(file);
            }
        }
        Some("allow_always") => {
            for file in &pending.files {
                if let Err(error) = trust.grant_always(file) {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "could not save trust; allowing for this session"
                    );
                    trust.grant_for_session(file);
                }
            }
        }
        _ => return false,
    }
    true
}

pub(super) fn supports_forms(peer: &Peer<RoleServer>) -> bool {
    peer.supported_elicitation_modes()
        .contains(&ElicitationMode::Form)
}

/// A single-enum form, so clients that map elicitations onto approval UIs (T3's Codex) show one
/// button per choice. Returns the chosen value; `None` when declined, cancelled or unanswerable.
pub(super) async fn ask_human(
    peer: &Peer<RoleServer>,
    message: String,
    choices: &[(&str, &str)],
) -> Option<String> {
    let values = choices.iter().map(|(value, _)| value.to_string()).collect();
    let titles = choices.iter().map(|(_, title)| title.to_string()).collect();
    let choice_schema = EnumSchema::builder(values)
        .enum_titles(titles)
        .map_err(|error| tracing::warn!(%error, "building the prompt"))
        .ok()?
        .build();
    let schema = ElicitationSchema::new(BTreeMap::from([(
        ANSWER_FIELD.to_string(),
        PrimitiveSchemaDefinition::Enum(choice_schema),
    )]))
    .with_required(vec![ANSWER_FIELD.to_string()]);

    let request = ElicitRequestParams::FormElicitationParams {
        meta: None,
        message,
        requested_schema: schema,
    };
    let response = match peer.create_elicitation(request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::info!(%error, "prompt failed");
            return None;
        }
    };
    if response.action != ElicitationAction::Accept {
        return None;
    }
    let answer = response.content?.get(ANSWER_FIELD)?.as_str()?.to_string();
    let offered = choices.iter().any(|(value, _)| *value == answer);
    offered.then_some(answer)
}
