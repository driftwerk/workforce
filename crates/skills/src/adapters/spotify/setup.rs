//! Agent-facing OAuth driver for the Spotify skill.
//!
//! Onboarding is two tool calls: `login` returns an authorization URL and
//! records pending state in the shared [`OAuthStateStore`]; the hub callback
//! exchanges the code and persists tokens; `login_poll` reports `authorized`
//! once they land.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::SecretStore;
use workforce_oauth_client::{OAuthClient, OAuthStateStore, STATE_TTL};

use super::client::build_http_client;
use super::provider::SpotifyProvider;
use super::token_store::{is_connected, ACCOUNT, ACCOUNT_PREFIX};
use super::{read_client_creds, OAUTH_CLIENT_PREFIX};

pub const SETUP_TOOL_NAME: &str = "spotify.setup";

/// Everything the token store persists, so a disconnect can name each key
/// rather than trusting the vault to enumerate them.
const TOKEN_SUFFIXES: &[&str] = &["refresh_token", "access_token", "expires_at"];

/// Shared by the `Tool` impl and the skill definition so they cannot drift.
pub(super) fn description() -> &'static str {
    "Connect and manage the Spotify account the spotify.* tools control. \
     \
     ADMIN PREREQUISITE (one-time): register an app at developer.spotify.com and populate \
     skills/spotify/oauth_client/{client_id,client_secret,redirect_uri} in the vault. The \
     redirect_uri must be https://<hub>/api/auth/spotify/callback and must be added to the \
     app's Redirect URIs *exactly*, character for character — a mismatch is the most common \
     failure and Spotify reports it only as INVALID_CLIENT. \
     \
     PER-USER SETUP — two steps: (1) call action='login'; relay the returned authorization_url \
     to the user verbatim and have them open it and approve. (2) call action='login_poll' until \
     status='authorized'. \
     \
     OTHER ACTIONS: status (is it connected), logout (remove the tokens). \
     \
     NOTE: controlling playback needs a Spotify Premium account; Spotify refuses it on free \
     accounts whatever the permissions say."
}

pub(super) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["login", "login_poll", "logout", "status"],
                "description": "What to do."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

pub struct SpotifySetupTool {
    store: Arc<dyn SecretStore>,
    state_store: Option<Arc<dyn OAuthStateStore>>,
}

impl SpotifySetupTool {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            state_store: None,
        }
    }

    /// The hub supplies the shared state store; without it there is no
    /// callback to come back to, so `login` is refused rather than handing out
    /// a URL that leads nowhere.
    pub fn with_state_store(mut self, store: Arc<dyn OAuthStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    fn action_login(&self) -> Result<Value> {
        let creds = read_client_creds(self.store.as_ref())?;
        let redirect_uri = creds.redirect_uri.clone().ok_or_else(|| {
            eyre!(
                "Spotify web login requires `{OAUTH_CLIENT_PREFIX}redirect_uri` (point it at \
                 https://<hub>/api/auth/spotify/callback and add the identical URL to the \
                 Spotify app's Redirect URIs)"
            )
        })?;
        let Some(state_store) = &self.state_store else {
            return Err(eyre!(
                "Spotify web login is only available on the hub (no callback is served here)"
            ));
        };
        let uid = workforce_agent::runner::CURRENT_USER_ID
            .try_with(Clone::clone)
            .map_err(|_| {
                eyre!("spotify.setup login requires an active user identity; this call has none")
            })?;

        let http = build_http_client()?;
        let oauth = OAuthClient::new(Arc::new(SpotifyProvider::new()), creds, http);
        let started = oauth.start_web_flow(&redirect_uri, &[]);
        state_store.insert(
            started.state.clone(),
            "spotify".to_string(),
            uid,
            ACCOUNT.to_string(),
            started.verifier,
        )?;

        Ok(json!({
            "status": "pending_authorization",
            "authorization_url": started.authorization_url,
            "expires_in_seconds": STATE_TTL.as_secs(),
            "next_step": format!(
                "Give the user this URL to open in their browser: {}. They sign in to Spotify \
                 and approve; the hub captures the result automatically. Then call \
                 spotify.setup(action='login_poll') — it returns 'authorized' once the tokens \
                 land.",
                started.authorization_url
            )
        }))
    }

    fn action_login_poll(&self) -> Value {
        if is_connected(self.store.as_ref()) {
            return json!({
                "status": "authorized",
                "message": "Spotify connected. The spotify.* tools can now control playback."
            });
        }
        json!({
            "status": "pending_authorization",
            "message": "Not connected yet. The user still has to open the authorization URL and \
                        approve; poll again once they have."
        })
    }

    fn action_logout(&self) -> Result<Value> {
        // The known paths by name, not whatever `list` reports. A store that
        // does not support prefix listing — or a layered vault that hides a
        // lower tier's keys from it — would otherwise report a successful
        // disconnect while leaving the refresh token in place, which is the one
        // secret that matters here.
        let mut removed = 0;
        for suffix in TOKEN_SUFFIXES {
            let key = format!("{ACCOUNT_PREFIX}{ACCOUNT}/{suffix}");
            if self.store.delete(&key).unwrap_or(false) {
                removed += 1;
            }
        }
        // Then anything else under the prefix, for stores that can enumerate.
        for key in self.store.list(Some(ACCOUNT_PREFIX)) {
            if self.store.delete(&key).unwrap_or(false) {
                removed += 1;
            }
        }
        Ok(json!({ "status": "disconnected", "secrets_removed": removed }))
    }

    fn action_status(&self) -> Value {
        let configured = read_client_creds(self.store.as_ref()).is_ok();
        json!({
            "app_configured": configured,
            "connected": is_connected(self.store.as_ref()),
            "next_step": if !configured {
                format!("An admin must set {OAUTH_CLIENT_PREFIX}client_id and client_secret first.")
            } else if !is_connected(self.store.as_ref()) {
                "Call spotify.setup(action='login').".to_string()
            } else {
                "Connected; nothing to do.".to_string()
            }
        })
    }
}

#[async_trait]
impl Tool for SpotifySetupTool {
    fn name(&self) -> &str {
        SETUP_TOOL_NAME
    }

    fn description(&self) -> &str {
        description()
    }

    fn input_schema(&self) -> Value {
        input_schema()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let result = match input["action"].as_str().unwrap_or("") {
            "login" => self.action_login(),
            "login_poll" => Ok(self.action_login_poll()),
            "logout" => self.action_logout(),
            "status" => Ok(self.action_status()),
            other => Err(eyre!(
                "unknown action '{other}'. Valid: login, login_poll, logout, status"
            )),
        };
        let duration = start.elapsed().as_millis() as u64;
        match result {
            Ok(value) => Ok(ToolResult::success(value, duration)),
            Err(e) => Ok(ToolResult::failure(format!("{e:#}"), duration)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::InMem;

    fn configured() -> Arc<InMem> {
        InMem::from(&[
            ("skills/spotify/oauth_client/client_id", "cid"),
            ("skills/spotify/oauth_client/client_secret", "secret"),
            (
                "skills/spotify/oauth_client/redirect_uri",
                "https://hub.example.com/api/auth/spotify/callback",
            ),
        ])
    }

    /// A URL that leads to a callback nobody serves is worse than a refusal —
    /// the person authorizes, nothing happens, and there is nothing to see.
    #[tokio::test]
    async fn login_is_refused_when_no_callback_is_served() {
        let tool = SpotifySetupTool::new(configured() as Arc<dyn SecretStore>);
        let result = tool.execute(json!({ "action": "login" })).await.unwrap();
        assert!(!result.success);
        assert!(
            result.error.unwrap().contains("only available on the hub"),
            "should say why"
        );
    }

    #[tokio::test]
    async fn status_says_what_to_do_next_at_each_stage() {
        let unconfigured = SpotifySetupTool::new(InMem::from(&[]) as Arc<dyn SecretStore>);
        let result = unconfigured
            .execute(json!({ "action": "status" }))
            .await
            .unwrap();
        assert_eq!(result.output["app_configured"], json!(false));
        assert!(result.output["next_step"]
            .as_str()
            .unwrap()
            .contains("client_id"));

        let ready = SpotifySetupTool::new(configured() as Arc<dyn SecretStore>);
        let result = ready.execute(json!({ "action": "status" })).await.unwrap();
        assert_eq!(result.output["app_configured"], json!(true));
        assert_eq!(result.output["connected"], json!(false));
        assert!(result.output["next_step"]
            .as_str()
            .unwrap()
            .contains("login"));
    }

    /// A vault that cannot enumerate by prefix must still be fully
    /// disconnected — otherwise "logged out" leaves the refresh token behind
    /// and the account stays reachable.
    #[tokio::test]
    async fn logout_removes_every_stored_token() {
        let store = InMem::from(&[
            ("skills/spotify/account/me/refresh_token", "rt"),
            ("skills/spotify/account/me/access_token", "at"),
        ]);
        let tool = SpotifySetupTool::new(Arc::clone(&store) as Arc<dyn SecretStore>);
        let result = tool.execute(json!({ "action": "logout" })).await.unwrap();
        assert!(result.success);
        assert!(
            !is_connected(store.as_ref()),
            "still connected after logout"
        );
    }

    #[tokio::test]
    async fn an_unknown_action_lists_the_valid_ones() {
        let tool = SpotifySetupTool::new(configured() as Arc<dyn SecretStore>);
        let result = tool.execute(json!({ "action": "sing" })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("login_poll"));
    }
}
