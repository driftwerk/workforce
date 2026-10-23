//! HTTP handler for `/api/auth/freeagent/callback`.
//!
//! Completes the hub-hosted FreeAgent OAuth web-redirect flow started by the
//! `freeagent.setup(action='login')` tool. The handler:
//!
//! 1. Looks up the pending flow in the shared [`OAuthStateStore`] by `state`.
//!    The entry carries the initiating user's id, the chosen workspace, and the
//!    PKCE verifier (unused — FreeAgent doesn't use PKCE).
//! 2. Reads the OAuth client credentials via a per-user-aware view of the vault
//!    so the resolution chain matches what the setup tool used at auth-URL time.
//! 3. Exchanges the authorization code for tokens.
//! 4. Persists tokens under `users/{uid}/skills/freeagent/workspaces/{ws}/*`
//!    (and the company name for display), which `LayeredUserVault` resolves for
//!    the originating user on their next tool call.
//! 5. Renders a minimal HTML page.

use std::sync::{Arc, OnceLock};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, info};
use workforce_common::settings::VaultActor;
use workforce_oauth_client::{InMemoryOAuthStateStore, OAuthClient, OAuthStateStore, StoredTokens};
use workforce_skills::adapters::freeagent::client::build_http_client;
use workforce_skills::adapters::freeagent::provider::FreeAgentProvider;
use workforce_skills::adapters::freeagent::token_store::WORKSPACE_PREFIX;
use workforce_skills::adapters::freeagent::{read_client_creds, read_sandbox};

use crate::gateway::GatewayState;
use crate::user_config::UserAwareVault;
use crate::vault::Vault;

/// Process-wide store for in-flight FreeAgent OAuth flows, shared by the
/// `freeagent.setup` tool (in `skill_loader`) and this callback. Entries are
/// ephemeral (10-minute TTL) so a restart only invalidates pending logins.
pub fn freeagent_state_store() -> Arc<dyn OAuthStateStore> {
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
    #[serde(default)]
    pub error_description: Option<String>,
}

pub async fn freeagent_oauth_callback(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(vault) = state.vault.as_ref() else {
        return failure("Hub is not configured with a global vault");
    };

    if let Some(err) = params.error {
        let detail = params.error_description.unwrap_or_default();
        return failure(&format!("FreeAgent reported: {err} {detail}"));
    }

    let (Some(code), Some(state_token)) = (params.code, params.state) else {
        return failure("Callback was missing `code` or `state`");
    };

    let entry = match freeagent_state_store().take(&state_token) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return failure(
                "This login link has already been used or has expired. Ask the agent to start a \
                 fresh freeagent.setup(action='login') flow.",
            );
        }
        Err(e) => {
            error!(error = %e, "FreeAgent OAuth state lookup failed");
            return failure("Could not verify the login state");
        }
    };

    match complete_exchange(vault, &code, &entry.uid, &entry.workspace).await {
        Ok(company) => {
            info!(uid = %entry.uid, workspace = %entry.workspace, company = ?company, "FreeAgent OAuth flow completed");
            success()
        }
        Err(e) => {
            error!(error = %e, "FreeAgent OAuth callback failed");
            failure(&format!("Token exchange failed: {e}"))
        }
    }
}

async fn complete_exchange(
    vault: &Arc<Vault>,
    code: &str,
    uid: &str,
    workspace: &str,
) -> eyre::Result<Option<String>> {
    let store = UserAwareVault::for_user(Arc::clone(vault), uid.to_string());
    let creds = read_client_creds(&store)?;
    let redirect_uri = creds.redirect_uri.clone().ok_or_else(|| {
        eyre::eyre!("`skills/freeagent/oauth_client/redirect_uri` is not configured")
    })?;
    let sandbox = read_sandbox(&store);

    let http = build_http_client()?;
    let provider = Arc::new(FreeAgentProvider::new(sandbox));
    let api_base = provider.api_base();
    let oauth = OAuthClient::new(provider, creds, http.clone());
    let tokens = oauth.exchange_web_code(code, "", &redirect_uri).await?;

    let company = fetch_company_name(&http, api_base, &tokens.access_token).await;
    persist_tokens(vault, uid, workspace, &tokens, company.as_deref())?;
    Ok(company)
}

/// Persist tokens at the user-scoped vault paths, tagged with the user actor
/// for audit (mirrors the Google callback). Writes the refresh token first so
/// a crash mid-write never strands a newer access token on an older refresh.
fn persist_tokens(
    vault: &Arc<Vault>,
    uid: &str,
    workspace: &str,
    tokens: &StoredTokens,
    company: Option<&str>,
) -> eyre::Result<()> {
    let base = format!("users/{uid}/{WORKSPACE_PREFIX}{workspace}");
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
    if let Some(name) = company {
        vault.set_with_actor(&format!("{base}/company"), name, actor)?;
    }
    Ok(())
}

/// Best-effort company-name lookup for display on the status/list tools.
async fn fetch_company_name(
    http: &reqwest::Client,
    api_base: &str,
    access_token: &str,
) -> Option<String> {
    let resp = http
        .get(format!("{api_base}/company"))
        .bearer_auth(access_token)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    body["company"]["name"].as_str().map(str::to_string)
}

fn success() -> Response {
    Html(SUCCESS_HTML).into_response()
}

fn failure(detail: &str) -> Response {
    // `detail` can carry attacker-controlled OAuth `error`/`error_description`
    // query params (the error branch fires before any state lookup), so escape
    // it before interpolating into the page.
    let body = format!(
        "{FAILURE_HTML_PREFIX}<p class=detail>{}</p>{FAILURE_HTML_SUFFIX}",
        crate::formatting::html_escape(detail)
    );
    (StatusCode::BAD_REQUEST, Html(body)).into_response()
}

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=utf-8>\
<title>FreeAgent connected</title>\
<style>body{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center;color:#222}\
h1{color:#1a7f37}</style></head><body><h1>✓ Authorization complete</h1>\
<p>Your FreeAgent company is connected. You can close this tab and return to the chat.</p></body></html>";

const FAILURE_HTML_PREFIX: &str = "<!doctype html><html><head><meta charset=utf-8>\
<title>FreeAgent authorization failed</title>\
<style>body{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center;color:#222}\
h1{color:#c62828}.detail{color:#555;font-family:ui-monospace,monospace;text-align:left;\
background:#f6f6f6;padding:.75rem;border-radius:4px;white-space:pre-wrap}</style></head>\
<body><h1>✗ Authorization failed</h1>";

const FAILURE_HTML_SUFFIX: &str =
    "<p>You can close this tab and try <code>freeagent.setup(action='login')</code> again.</p></body></html>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_store_is_a_shared_singleton() {
        // The setup tool and the callback must observe the same store, so
        // `freeagent_state_store()` returns one process-wide instance.
        let a = freeagent_state_store();
        a.insert(
            "state-xyz".into(),
            "freeagent".into(),
            "alice".into(),
            "acme".into(),
            String::new(),
        )
        .unwrap();
        let entry = freeagent_state_store()
            .take("state-xyz")
            .unwrap()
            .expect("entry present via a second handle");
        assert_eq!(entry.uid, "alice");
        assert_eq!(entry.workspace, "acme");
        // Single-use: a second take returns None.
        assert!(freeagent_state_store().take("state-xyz").unwrap().is_none());
    }

    #[tokio::test]
    async fn failure_page_escapes_reflected_detail() {
        // Attacker-controlled OAuth error params must not reach the page raw.
        let resp = failure("<img src=x onerror=alert(1)>");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(!html.contains("<img src=x"), "unescaped payload in: {html}");
        assert!(
            html.contains("&lt;img src=x"),
            "expected escaped payload in: {html}"
        );
    }
}
