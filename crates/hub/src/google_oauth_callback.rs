//! HTTP handler for `/api/auth/google/callback`.
//!
//! Completes the hub-hosted Google OAuth web-redirect flow initiated by the
//! `google.setup(action='login')` tool. The handler:
//!
//! 1. Looks up the pending flow in the shared [`WebFlowStateStore`] by the
//!    `state` query param. The entry carries the initiating user's id, the
//!    chosen workspace, and the PKCE verifier.
//! 2. Reads the OAuth client credentials (client_id, client_secret,
//!    redirect_uri) from the global vault.
//! 3. Exchanges the authorization code for tokens with Google's token
//!    endpoint.
//! 4. Persists the tokens under `users/{uid}/skills/google/workspaces/{ws}/*`
//!    in the global vault, which `LayeredUserVault::get` resolves for the
//!    originating user on their next tool call.
//! 5. Renders a minimal HTML success page directing the user back to Slack.

use std::sync::{Arc, OnceLock};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tracing::{error, info};
use workforce_skills::adapters::google::{
    api::build_http_client,
    oauth::{exchange_web_code, persist_tokens_at_user_scope, OAuthClientCreds},
    web_flow::WebFlowStateStore,
};

use crate::gateway::GatewayState;
use crate::vault::Vault;

/// Process-wide shared store for in-flight Google OAuth web-redirect flows.
///
/// Lazily initialized on first access so both `GoogleSetupTool` (in
/// `skill_loader`) and the callback handler here use the same instance
/// without requiring explicit wiring at hub startup. Entries are ephemeral
/// (10-minute TTL) so losing process state on restart only invalidates
/// pending flows — the user re-runs `google.setup(action='login')`.
pub fn google_web_flow_store() -> Arc<WebFlowStateStore> {
    static STORE: OnceLock<Arc<WebFlowStateStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(WebFlowStateStore::new()))
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

pub async fn google_oauth_callback(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(vault) = state.vault.as_ref() else {
        return failure("Hub is not configured with a global vault");
    };
    let store = google_web_flow_store();

    if let Some(err) = params.error {
        let detail = params.error_description.unwrap_or_default();
        return failure(&format!("Google reported: {err} {detail}"));
    }

    let (Some(code), Some(state_token)) = (params.code, params.state) else {
        return failure("Callback was missing `code` or `state`");
    };

    let Some(entry) = store.take(&state_token) else {
        return failure(
            "This login link has already been used or has expired. \
             Ask the agent to start a fresh `google.setup(action='login')` flow.",
        );
    };

    match complete_exchange(vault, &code, &entry.verifier, &entry.uid, &entry.workspace).await {
        Ok(email) => {
            info!(
                uid = %entry.uid,
                workspace = %entry.workspace,
                email = ?email,
                "Google OAuth web-redirect flow completed"
            );
            success()
        }
        Err(e) => {
            error!(error = %e, "Google OAuth callback failed");
            failure(&format!("Token exchange failed: {e}"))
        }
    }
}

async fn complete_exchange(
    vault: &Arc<Vault>,
    code: &str,
    verifier: &str,
    uid: &str,
    workspace: &str,
) -> eyre::Result<Option<String>> {
    // Read OAuth client creds via a per-user-aware view of the global vault
    // so the resolution chain matches what the setup tool used at auth-URL
    // time. A `users/{uid}/skills/google/oauth_client/...` override that
    // bypassed the Phase 1 reserved-prefix policy would otherwise shadow
    // the org-wide bare path during setup but be invisible to the callback
    // — the original `unauthorized_client` mismatch.
    let store = crate::user_config::UserAwareVault::for_user(Arc::clone(vault), uid.to_string());
    let creds = OAuthClientCreds::from_store_or_bundled(&store)?;
    let redirect_uri = creds.redirect_uri.clone().ok_or_else(|| {
        eyre::eyre!("`skills/google/oauth_client/redirect_uri` is not configured")
    })?;

    info!(
        uid = %uid,
        workspace = %workspace,
        redirect_uri = %redirect_uri,
        "completing Google OAuth web-redirect exchange"
    );

    let http = build_http_client()?;
    let tokens = exchange_web_code(&http, &creds, code, verifier, &redirect_uri).await?;
    let email = tokens.email.clone();
    // `persist_tokens_at_user_scope` writes paths already prefixed with
    // `users/{uid}/...`, which `UserAwareVault` passes through unchanged,
    // keeping the `VaultActor::User(uid)` audit tag from inside `persist_at`.
    persist_tokens_at_user_scope(&store, uid, workspace, &tokens)?;
    Ok(email)
}

fn success() -> Response {
    Html(SUCCESS_HTML).into_response()
}

fn failure(detail: &str) -> Response {
    // `detail` can carry attacker-controlled OAuth error params, so escape it
    // before interpolating into the page.
    let body = format!(
        "{FAILURE_HTML_PREFIX}<p class=detail>{}</p>{FAILURE_HTML_SUFFIX}",
        crate::formatting::html_escape(detail)
    );
    (StatusCode::BAD_REQUEST, Html(body)).into_response()
}

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=utf-8>\
<title>Google connected</title>\
<style>body{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center;color:#222}\
h1{color:#1a7f37}</style></head><body><h1>✓ Authorization complete</h1>\
<p>You can close this tab and return to Slack.</p></body></html>";

const FAILURE_HTML_PREFIX: &str = "<!doctype html><html><head><meta charset=utf-8>\
<title>Google authorization failed</title>\
<style>body{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center;color:#222}\
h1{color:#c62828}.detail{color:#555;font-family:ui-monospace,monospace;text-align:left;\
background:#f6f6f6;padding:.75rem;border-radius:4px;white-space:pre-wrap}</style></head>\
<body><h1>✗ Authorization failed</h1>";

const FAILURE_HTML_SUFFIX: &str =
    "<p>You can close this tab and try <code>google.setup(action='login')</code> again from Slack.</p></body></html>";
