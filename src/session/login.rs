//! The `<server>__login` stub: starts an OAuth login and hands the URL to the human.

use rmcp::model::CallToolResult;
use rmcp::service::ElicitationMode;

use super::Session;
use super::tools::{error_result, text_result};
use crate::config::ResolvedServer;
use crate::{identity, oauth};

impl Session {
    pub(super) async fn start_login(
        &self,
        server: ResolvedServer,
        challenge: &str,
    ) -> CallToolResult {
        let login_key = (server.identity.clone(), server.account.clone());
        if let Some(url) = self.shared.logins_in_progress.lock().get(&login_key) {
            return text_result(format!(
                "A login for {} is already waiting. Open this URL to finish it: {url}",
                server.display_name()
            ));
        }

        let flow = match oauth::start_login(&server, Some(challenge)).await {
            Ok(flow) => flow,
            Err(error) => {
                return error_result(format!(
                    "could not start login for {}: {error:#}",
                    server.display_name()
                ));
            }
        };
        let url = flow.authorization_url.clone();
        self.shared
            .logins_in_progress
            .lock()
            .insert(login_key.clone(), url.clone());

        tokio::spawn({
            let shared = self.shared.clone();
            let display_name = server.display_name();
            async move {
                match flow.finish().await {
                    Ok(()) => {
                        tracing::info!(server = %display_name, "login completed");
                        let (identity, account) = &login_key;
                        if let Err(error) = identity::forget(&shared.paths, identity, account).await
                        {
                            tracing::warn!(server = %display_name, error = format!("{error:#}"), "could not forget the previous verified account");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(server = %display_name, error = format!("{error:#}"), "login failed")
                    }
                }
                shared.logins_in_progress.lock().remove(&login_key);
                shared.reload_all().await;
            }
        });
        self.show_login_url(&server, &url);

        text_result(format!(
            "Opened the browser to log in to {name}. If nothing opened, visit:\n{url}\n\n\
             Once the login completes its tools appear automatically (clients that ignore tool list \
             changes, like Codex, pick them up in the next session).",
            name = server.display_name()
        ))
    }

    /// Asks the client to open the URL when it supports URL elicitation, else opens the browser ourselves.
    fn show_login_url(&self, server: &ResolvedServer, url: &str) {
        let peer = self
            .peer
            .get()
            .filter(|peer| {
                peer.supported_elicitation_modes()
                    .contains(&ElicitationMode::Url)
            })
            .cloned();
        let Some((peer, parsed_url)) = peer.zip(url::Url::parse(url).ok()) else {
            if let Err(error) = open::that_detached(url) {
                tracing::warn!(%error, "could not open the browser");
            }
            return;
        };

        let message = format!("Log in to {} for plugbay", server.display_name());
        let elicitation_id = format!("plugbay-login-{}", server.display_name());
        tokio::spawn(async move {
            if let Err(error) = peer.elicit_url(message, parsed_url, elicitation_id).await {
                tracing::debug!(%error, "URL elicitation failed");
            }
        });
    }
}
