//! HTTP handler for `/api/auth/spotify/callback`.
//!
//! Completes the hub-hosted Spotify OAuth web-redirect flow started by the
//! `spotify.setup(action='login')` tool. The handler:
//!
//! 1. Looks up the pending flow in the shared [`OAuthStateStore`] by `state`.
//! 2. Reads the OAuth client credentials through a per-user view of the vault,
//!    so the resolution chain matches what the setup tool used at auth-URL time.
//! 3. Exchanges the authorization code for tokens.
//! 4. Persists them under `users/{uid}/skills/spotify/account/me/*`, which
//!    `LayeredUserVault` resolves for that user on their next tool call.
//! 5. Renders a minimal HTML page.

use std::sync::{Arc, OnceLock};

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tracing::{error, info};
use workforce_common::settings::VaultActor;
use workforce_oauth_client::{InMemoryOAuthStateStore, OAuthClient, OAuthStateStore, StoredTokens};
use workforce_skills::adapters::spotify::client::build_http_client;
use workforce_skills::adapters::spotify::provider::SpotifyProvider;
use workforce_skills::adapters::spotify::read_client_creds;
use workforce_skills::adapters::spotify::token_store::{ACCOUNT, ACCOUNT_PREFIX};

use crate::gateway::GatewayState;
use crate::user_config::UserAwareVault;
use crate::vault::Vault;

/// Process-wide store for in-flight Spotify OAuth flows, shared by the
/// `spotify.setup` tool and this callback. Entries are ephemeral, so a restart
/// only invalidates logins that were mid-flight.
pub fn spotify_state_store() -> Arc<dyn OAuthStateStore> {
    static STORE: OnceLock<Arc<dyn OAuthStateStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(InMemoryOAuthStateStore::new()))
        .clone()
}

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

pub async fn spotify_oauth_callback(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(vault) = state.vault.as_ref() else {
        return failure("Hub is not configured with a global vault");
    };

    if let Some(err) = params.error {
        // `access_denied` is the person changing their mind, which is not a
        // fault worth a stack trace.
        return failure(&format!("Spotify reported: {err}"));
    }

    let (Some(code), Some(state_token)) = (params.code, params.state) else {
        return failure("Callback was missing `code` or `state`");
    };

    let entry = match spotify_state_store().take(&state_token) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return failure(
                "This login link has already been used or has expired. Ask the agent to start a \
                 fresh spotify.setup(action='login') flow.",
            );
        }
        Err(e) => {
            error!(error = %e, "Spotify OAuth state lookup failed");
            return failure("Could not verify the login state");
        }
    };

    match complete_exchange(vault, &code, &entry.uid).await {
        Ok(()) => {
            info!(uid = %entry.uid, "Spotify OAuth flow completed");
            success()
        }
        Err(e) => {
            error!(error = %e, "Spotify OAuth callback failed");
            failure(&format!("Token exchange failed: {e}"))
        }
    }
}

async fn complete_exchange(vault: &Arc<Vault>, code: &str, uid: &str) -> eyre::Result<()> {
    let store = UserAwareVault::for_user(Arc::clone(vault), uid.to_string());
    let creds = read_client_creds(&store)?;
    let redirect_uri = creds.redirect_uri.clone().ok_or_else(|| {
        eyre::eyre!("`skills/spotify/oauth_client/redirect_uri` is not configured")
    })?;

    let oauth = OAuthClient::new(
        Arc::new(SpotifyProvider::new()),
        creds,
        build_http_client()?,
    );
    let tokens = oauth.exchange_web_code(code, "", &redirect_uri).await?;
    persist_tokens(vault, uid, &tokens)
}

/// Persist tokens at the user-scoped vault paths, tagged with the user actor
/// for audit. The refresh token is written first so a crash mid-write never
/// strands a newer access token on an older refresh.
fn persist_tokens(vault: &Arc<Vault>, uid: &str, tokens: &StoredTokens) -> eyre::Result<()> {
    let base = format!("users/{uid}/{ACCOUNT_PREFIX}{ACCOUNT}");
    let actor = VaultActor::User(uid);
    vault.set_with_actor(
        &format!("{base}/refresh_token"),
        &tokens.refresh_token,
        actor,
    )?;
    vault.set_with_actor(&format!("{base}/access_token"), &tokens.access_token, actor)?;
    vault.set_with_actor(
        &format!("{base}/expires_at"),
        &tokens.expires_at.to_string(),
        actor,
    )?;
    if !tokens.scopes.is_empty() {
        vault.set_with_actor(&format!("{base}/scopes"), &tokens.scopes, actor)?;
    }
    Ok(())
}

fn success() -> Response {
    Html(SUCCESS_HTML).into_response()
}

fn failure(detail: &str) -> Response {
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>Spotify</title>\
         <body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto\">\
         <h1>Spotify could not be connected</h1><p>{}</p>\
         <p>You can close this tab and try again.</p></body>",
        html_escape(detail)
    ))
    .into_response()
}

/// The detail can contain text Spotify sent us, so it is escaped rather than
/// trusted into the page.
fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const SUCCESS_HTML: &str = "<!doctype html><meta charset=utf-8><title>Spotify</title>\
     <body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto\">\
     <h1>Spotify connected</h1>\
     <p>You can close this tab and go back to the conversation.</p></body>";

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure page repeats text Spotify sent, so it must not be able to
    /// carry markup into the page.
    #[test]
    fn error_detail_cannot_inject_markup() {
        let escaped = html_escape("<script>alert(1)</script>");
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(escaped.contains("&lt;script&gt;"), "{escaped}");
    }

    #[test]
    fn tokens_land_under_the_users_own_namespace() {
        // The layered vault resolves `users/{uid}/…` for that user only, so
        // this prefix is what keeps two people's accounts apart.
        let base = format!("users/{}/{ACCOUNT_PREFIX}{ACCOUNT}", "u-123");
        assert_eq!(base, "users/u-123/skills/spotify/account/me");
    }
}
