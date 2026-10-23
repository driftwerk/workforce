//! Google OAuth 2.0: authorization-code + PKCE (loopback) and device-flow
//! (headless). Hand-rolled to avoid `oauth2` crate's reqwest 0.11 pin.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use eyre::{bail, eyre, Result, WrapErr};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};
use workforce_common::settings::{SecretStore, VaultActor};

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";

/// Google's [`workforce_oauth_client::OAuthProvider`] impl. Used by
/// the migrated [`super::token_store::TokenStore`] so refresh-token
/// rotation is handled by the shared library code. `WEB_SCOPES` is
/// the default; the device flow supplies `DEVICE_SCOPES` explicitly
/// at start-flow time.
struct GoogleOAuthProvider;

#[async_trait::async_trait]
impl workforce_oauth_client::OAuthProvider for GoogleOAuthProvider {
    fn name(&self) -> &str {
        "google"
    }
    fn authorization_endpoint(&self) -> &str {
        AUTH_URL
    }
    fn token_endpoint(&self) -> &str {
        TOKEN_URL
    }
    fn default_scopes(&self) -> &[&str] {
        WEB_SCOPES
    }
    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        vec![("access_type", "offline"), ("prompt", "consent")]
    }
    async fn fetch_userinfo(
        &self,
        http: &reqwest::Client,
        access_token: &str,
    ) -> eyre::Result<Option<String>> {
        Ok(fetch_email(http, access_token).await)
    }
}

pub fn google_provider() -> std::sync::Arc<dyn workforce_oauth_client::OAuthProvider> {
    std::sync::Arc::new(GoogleOAuthProvider)
}

/// Scopes for the agent's device flow.
///
/// Google's "TVs and Limited Input devices" OAuth client type (required by the
/// device flow) rejects the broad `https://mail.google.com/` scope, so Gmail
/// uses `gmail.modify` + `gmail.settings.basic` instead. `gmail.modify` covers
/// every tool in `services/gmail.rs` except permanent delete (tools exposed
/// there use `trash` instead).
///
/// Changing this list forces existing users to re-consent.
pub const DEVICE_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.settings.basic",
    "https://www.googleapis.com/auth/calendar",
    "https://www.googleapis.com/auth/drive",
    "https://www.googleapis.com/auth/spreadsheets",
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/tasks",
    "https://www.googleapis.com/auth/contacts",
    "https://www.googleapis.com/auth/directory.readonly",
    "https://www.googleapis.com/auth/chat.messages",
    "https://www.googleapis.com/auth/chat.spaces",
    "https://www.googleapis.com/auth/presentations",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// Scopes for the hub-hosted web-redirect flow.
///
/// Web application OAuth clients permit the broad `https://mail.google.com/`
/// scope (full IMAP/SMTP + permanent delete + all settings), so the web flow
/// replaces `gmail.modify` + `gmail.settings.basic` with that single broader
/// scope. Non-Gmail entries match `DEVICE_SCOPES`.
///
/// Do NOT "just narrow" this back to `gmail.modify` — the whole point of
/// `WEB_SCOPES` existing is to diverge from `DEVICE_SCOPES` and unlock the
/// Gmail surface that the TV-client type bans (perma-delete via
/// `users.messages.delete`, `gmail.settings.sharing` for forwarding
/// creation, future omnibus-scope features). Narrowing it silently breaks
/// those tools under web flow too and erases the reason this constant
/// exists.
pub const WEB_SCOPES: &[&str] = &[
    "https://mail.google.com/",
    "https://www.googleapis.com/auth/calendar",
    "https://www.googleapis.com/auth/drive",
    "https://www.googleapis.com/auth/spreadsheets",
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/tasks",
    "https://www.googleapis.com/auth/contacts",
    "https://www.googleapis.com/auth/directory.readonly",
    "https://www.googleapis.com/auth/chat.messages",
    "https://www.googleapis.com/auth/chat.spaces",
    "https://www.googleapis.com/auth/presentations",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

#[derive(Debug, Clone)]
pub struct OAuthClientCreds {
    pub client_id: String,
    pub client_secret: String,
    /// When `Some`, the hub-hosted web-redirect flow is active — agents emit an
    /// authorization URL with this redirect, and `/api/auth/google/callback`
    /// on the hub completes the exchange. When `None`, the device flow is
    /// used (suitable for unattended / headless deployments).
    pub redirect_uri: Option<String>,
}

impl OAuthClientCreds {
    pub fn from_store_or_bundled(store: &dyn SecretStore) -> Result<Self> {
        if let (Some(id), Some(secret)) = (
            store.get("skills/google/oauth_client/client_id"),
            store.get("skills/google/oauth_client/client_secret"),
        ) {
            debug!("Using BYO OAuth client from vault");
            return Ok(Self {
                client_id: id,
                client_secret: secret,
                redirect_uri: store.get("skills/google/oauth_client/redirect_uri"),
            });
        }

        match (
            option_env!("WORKFORCE_GOOGLE_CLIENT_ID"),
            option_env!("WORKFORCE_GOOGLE_CLIENT_SECRET"),
        ) {
            (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
                debug!("Using bundled workforce OAuth client");
                Ok(Self {
                    client_id: id.to_string(),
                    client_secret: secret.to_string(),
                    redirect_uri: None,
                })
            }
            _ => Err(eyre!(
                "No Google OAuth client configured. This is a one-time hub admin step.\n\
                 \n\
                 In Google Cloud Console (https://console.cloud.google.com):\n\
                 \n\
                 1. Enable APIs at APIs & Services → Library:\n\
                    Gmail, Google Calendar, Google Drive, Google Docs, Google Sheets,\n\
                    Google Slides, Tasks, People (Contacts), Google Chat.\n\
                 \n\
                 2. Configure the OAuth consent screen at APIs & Services → OAuth\n\
                    consent screen.\n\
                    • User type 'Internal' if all hub users are in the same Google\n\
                      Workspace org — no Google verification needed.\n\
                    • User type 'External' otherwise. While in 'Testing' status,\n\
                      ONLY accounts on the 'Test users' list can authorize. If a\n\
                      non-test-user attempts to log in they hit\n\
                      'unauthorized_client' at token exchange. Add every hub user\n\
                      as a Test user, or publish the app (sensitive scopes like\n\
                      mail.google.com require Google's verification process for\n\
                      published External apps).\n\
                 \n\
                 3. Create an OAuth 2.0 client at APIs & Services → Credentials →\n\
                    Create Credentials → OAuth client ID. Pick ONE:\n\
                    • 'TVs and Limited Input devices' — works from any deployment,\n\
                      no callback URL needed. Narrower Gmail scope (no permanent\n\
                      delete, no forwarding-address creation).\n\
                    • 'Web application' — adds the broad mail.google.com/ scope\n\
                      (full Gmail incl. permanent delete and forwarding). Requires\n\
                      the hub to be reachable on a public HTTPS URL. Set the\n\
                      Authorized redirect URI to:\n\
                      https://<your-hub-host>/api/auth/google/callback\n\
                 \n\
                 4. Store the credentials in the hub's GLOBAL vault. These are\n\
                    organization-wide, shared by every user. They MUST be set\n\
                    from the hub host — the agent-callable `vault.set` tool\n\
                    writes to a per-agent vault that the OAuth callback handler\n\
                    cannot reach, so setting them through an agent silently\n\
                    creates a shadowing trap.\n\
                    \n\
                    From the hub host CLI (recommended):\n\
                      workforce vault set skills/google/oauth_client/client_id     '<client_id>'\n\
                      workforce vault set skills/google/oauth_client/client_secret '<client_secret>'\n\
                    Web application client only:\n\
                      workforce vault set skills/google/oauth_client/redirect_uri \\\n\
                        'https://<your-hub-host>/api/auth/google/callback'\n\
                    \n\
                    Or via REST with the hub owner token:\n\
                      curl -X POST -H 'Authorization: Bearer $WF_TOKEN' \\\n\
                        -d '{{\"value\":\"<client_id>\"}}' \\\n\
                        https://<your-hub-host>/api/vault/skills/google/oauth_client/client_id\n\
                 \n\
                 5. Call google.setup(action='login') again. Each end-user then\n\
                    completes their own login (device or web flow); only the\n\
                    OAuth client itself is shared.\n\
                 \n\
                 (Alternatively: build workforce with WORKFORCE_GOOGLE_CLIENT_ID and\n\
                 WORKFORCE_GOOGLE_CLIENT_SECRET env vars set at compile time for a\n\
                 bundled client.)"
            )),
        }
    }
}

/// Options for `run_login`.
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// Workspace name under which tokens are persisted
    /// (`skills/google/workspaces/<name>/...`).
    pub workspace: String,
    /// When true, skip browser launch and use the device flow instead.
    /// Useful for headless containers.
    pub no_browser: bool,
    /// Optional BYO client credential override (takes precedence over vault
    /// and bundled).
    pub override_client: Option<OAuthClientCreds>,
}

#[derive(Debug, Clone)]
pub struct StoredTokens {
    pub email: Option<String>,
    pub refresh_token: String,
    pub access_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub scopes: String,
}

fn persist_at(
    store: &dyn SecretStore,
    base: &str,
    tokens: &StoredTokens,
    actor: VaultActor<'_>,
) -> Result<()> {
    if let Some(ref email) = tokens.email {
        store
            .set_with_actor(&format!("{base}/email"), email, actor)
            .wrap_err("failed to persist email")?;
    }
    store
        .set_with_actor(
            &format!("{base}/refresh_token"),
            &tokens.refresh_token,
            actor,
        )
        .wrap_err("failed to persist refresh_token")?;
    store
        .set_with_actor(&format!("{base}/access_token"), &tokens.access_token, actor)
        .wrap_err("failed to persist access_token")?;
    store
        .set_with_actor(
            &format!("{base}/expires_at"),
            &tokens.expires_at.to_string(),
            actor,
        )
        .wrap_err("failed to persist expires_at")?;
    store
        .set_with_actor(&format!("{base}/scopes"), &tokens.scopes, actor)
        .wrap_err("failed to persist scopes")
}

pub fn persist_tokens(
    store: &dyn SecretStore,
    workspace: &str,
    tokens: &StoredTokens,
) -> Result<()> {
    let uid = workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty());
    let actor = match uid.as_deref() {
        Some(u) => VaultActor::User(u),
        None => VaultActor::System("google_token_persist"),
    };
    persist_at(
        store,
        &format!("skills/google/workspaces/{workspace}"),
        tokens,
        actor,
    )
}

fn gen_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn url_encode(v: &str) -> String {
    url::form_urlencoded::byte_serialize(v.as_bytes()).collect()
}

/// Build the Google authorize URL shared by loopback and web-redirect flows.
/// PKCE is always used even when a client_secret is sent, as defence in depth.
fn build_authorize_url(
    creds: &OAuthClientCreds,
    redirect_uri: &str,
    scope: &str,
    challenge: &str,
    state: &str,
) -> String {
    format!(
        "{AUTH_URL}?response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect}\
         &scope={scope}\
         &code_challenge={challenge}\
         &code_challenge_method=S256\
         &access_type=offline\
         &prompt=consent\
         &state={state}",
        client_id = url_encode(&creds.client_id),
        redirect = url_encode(redirect_uri),
        scope = url_encode(scope),
        challenge = url_encode(challenge),
        state = url_encode(state),
    )
}

/// Build `StoredTokens` from a parsed Google token response.
///
/// Shared by loopback, web-redirect, and device polling. `fallback_scopes` is
/// only used when Google's response omits the `scope` field (rare — it
/// normally echoes the granted scopes back).
async fn finalize_tokens(
    http: &reqwest::Client,
    resp: GoogleTokenResponse,
    fallback_scopes: &[&str],
) -> Result<StoredTokens> {
    let refresh_token = resp.refresh_token.ok_or_else(|| {
        eyre!(
            "Google did not return a refresh_token — the account may have \
             previously consented without access_type=offline. Revoke the app at \
             https://myaccount.google.com/permissions and retry."
        )
    })?;
    let expires_at = now_unix() + resp.expires_in.unwrap_or(3600).max(0) as u64;
    let email = fetch_email(http, &resp.access_token).await;
    Ok(StoredTokens {
        email,
        refresh_token,
        access_token: resp.access_token,
        expires_at,
        scopes: resp.scope.unwrap_or_else(|| fallback_scopes.join(" ")),
    })
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleTokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn exchange_code(
    http: &reqwest::Client,
    creds: &OAuthClientCreds,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> impl std::future::Future<Output = Result<GoogleTokenResponse>> + Send {
    let params: HashMap<&str, String> = HashMap::from([
        ("client_id", creds.client_id.clone()),
        ("client_secret", creds.client_secret.clone()),
        ("code", code.to_string()),
        ("code_verifier", code_verifier.to_string()),
        ("grant_type", "authorization_code".to_string()),
        ("redirect_uri", redirect_uri.to_string()),
    ]);
    let http = http.clone();
    async move {
        let resp = http
            .post(TOKEN_URL)
            .form(&params)
            .send()
            .await
            .wrap_err("token exchange request failed")?;
        let status = resp.status();
        let body = resp.text().await.wrap_err("reading token body")?;
        if !status.is_success() {
            let err: GoogleTokenError = serde_json::from_str(&body).unwrap_or(GoogleTokenError {
                error: format!("http {}", status.as_u16()),
                error_description: Some(body.clone()),
            });
            bail!(
                "token exchange failed: {} {}",
                err.error,
                err.error_description.unwrap_or_default()
            );
        }
        serde_json::from_str::<GoogleTokenResponse>(&body)
            .wrap_err_with(|| format!("parsing token response: {body}"))
    }
}

/// Exchange a refresh token for a new access token.
///
/// Exposed to `token_store.rs`. Returns the parsed response — caller owns
/// the decision to persist.
pub async fn refresh_access_token(
    http: &reqwest::Client,
    creds: &OAuthClientCreds,
    refresh_token: &str,
) -> Result<(String, u64)> {
    let params: HashMap<&str, String> = HashMap::from([
        ("client_id", creds.client_id.clone()),
        ("client_secret", creds.client_secret.clone()),
        ("refresh_token", refresh_token.to_string()),
        ("grant_type", "refresh_token".to_string()),
    ]);
    let resp = http
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .wrap_err("refresh request failed")?;
    let status = resp.status();
    let body = resp.text().await.wrap_err("reading refresh body")?;
    if !status.is_success() {
        let err: GoogleTokenError = serde_json::from_str(&body).unwrap_or(GoogleTokenError {
            error: format!("http {}", status.as_u16()),
            error_description: Some(body.clone()),
        });
        bail!(
            "token refresh failed: {} {}",
            err.error,
            err.error_description.unwrap_or_default()
        );
    }
    let parsed: GoogleTokenResponse = serde_json::from_str(&body)
        .wrap_err_with(|| format!("parsing refresh response: {body}"))?;
    let expires_at = now_unix() + parsed.expires_in.unwrap_or(3600).max(0) as u64;
    Ok((parsed.access_token, expires_at))
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    email: Option<String>,
}

async fn fetch_email(http: &reqwest::Client, access_token: &str) -> Option<String> {
    let resp = http
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let info: UserInfo = resp.json().await.ok()?;
    info.email
}

// ─── Loopback authorization-code flow ────────────────────────────────────────

/// Run the full authorization-code-with-PKCE flow via a local loopback
/// redirect and return the resulting `StoredTokens` (already persisted).
///
/// This function:
/// 1. Binds `127.0.0.1:0` and reads the assigned port.
/// 2. Builds the Google authorize URL with PKCE and the loopback redirect.
/// 3. Opens the user's browser (unless `opts.no_browser`).
/// 4. Spawns a small axum server that captures `code` on `/callback`.
/// 5. Exchanges the code + verifier for tokens.
/// 6. Persists everything under `skills/google/workspaces/<name>/`.
pub async fn run_login(store: Arc<dyn SecretStore>, opts: LoginOptions) -> Result<StoredTokens> {
    let creds = match opts.override_client.clone() {
        Some(c) => c,
        None => OAuthClientCreds::from_store_or_bundled(store.as_ref())?,
    };

    if opts.no_browser {
        return run_device_login(store, opts.workspace, creds).await;
    }

    // Try loopback first; fall back to device flow on bind failure.
    match run_loopback_login(&store, &opts.workspace, &creds).await {
        Ok(tokens) => Ok(tokens),
        Err(e) => {
            warn!(error = %e, "loopback login failed, falling back to device flow");
            run_device_login(store, opts.workspace, creds).await
        }
    }
}

async fn run_loopback_login(
    store: &Arc<dyn SecretStore>,
    workspace: &str,
    creds: &OAuthClientCreds,
) -> Result<StoredTokens> {
    use axum::{extract::Query, response::Html, routing::get, Router};
    use tokio::sync::oneshot;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .wrap_err("failed to bind loopback listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    debug!(%redirect_uri, "loopback redirect bound");

    let verifier = gen_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    let state = gen_code_verifier(); // reuse generator for opaque state token

    let auth_url = build_authorize_url(
        creds,
        &redirect_uri,
        &DEVICE_SCOPES.join(" "),
        &challenge,
        &state,
    );

    println!(
        "\n🔐 Opening your browser to authorize workforce for Google…\n\
         If the browser doesn't open, paste this URL manually:\n  {auth_url}\n"
    );

    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx: Arc<std::sync::Mutex<Option<oneshot::Sender<CallbackResult>>>> =
        Arc::new(std::sync::Mutex::new(Some(tx)));
    let expected_state = Arc::new(state.clone());

    #[derive(Debug, Deserialize)]
    struct CallbackParams {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        state: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }

    let handler_tx = tx.clone();
    let handler_state = expected_state.clone();
    let app = Router::new().route(
        "/callback",
        get(move |Query(p): Query<CallbackParams>| {
            let tx = handler_tx.clone();
            let expected_state = handler_state.clone();
            async move {
                let payload = if let Some(err) = p.error {
                    CallbackResult::Error(err)
                } else if p.state.as_deref() != Some(expected_state.as_str()) {
                    CallbackResult::Error("state mismatch — possible CSRF attempt".into())
                } else if let Some(code) = p.code {
                    CallbackResult::Code(code)
                } else {
                    CallbackResult::Error("callback missing `code`".into())
                };

                let html = match &payload {
                    CallbackResult::Code(_) => SUCCESS_HTML,
                    CallbackResult::Error(msg) => {
                        warn!(%msg, "oauth callback error");
                        FAILURE_HTML
                    }
                };

                if let Some(sender) = tx.lock().ok().and_then(|mut guard| guard.take()) {
                    let _ = sender.send(payload);
                }
                Html(html)
            }
        }),
    );

    // Spawn the server so it can shut down as soon as the callback fires.
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Launch the browser. Ignore failure — user can still paste the URL.
    if let Err(e) = open::that(&auth_url) {
        warn!(error = %e, "open::that failed; rely on manual paste");
    }

    // Wait up to 5 minutes for the callback.
    let result = tokio::time::timeout(Duration::from_secs(300), rx)
        .await
        .map_err(|_| eyre!("timed out waiting for oauth callback after 5 minutes"))?
        .map_err(|_| eyre!("callback receiver dropped"))?;

    // Shut down the server.
    server.abort();

    let code = match result {
        CallbackResult::Code(c) => c,
        CallbackResult::Error(msg) => bail!("oauth callback reported error: {msg}"),
    };

    let http = reqwest::Client::builder()
        .build()
        .wrap_err("building http client")?;
    let resp = exchange_code(&http, creds, &code, &verifier, &redirect_uri).await?;
    let stored = finalize_tokens(&http, resp, DEVICE_SCOPES).await?;
    persist_tokens(store.as_ref(), workspace, &stored)?;
    info!(
        workspace = %workspace,
        email = ?stored.email,
        "Google OAuth loopback login complete"
    );
    Ok(stored)
}

enum CallbackResult {
    Code(String),
    Error(String),
}

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=utf-8><title>Workforce Google login</title>\
<style>body{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center;color:#222}\
h1{color:#1a7f37}</style></head><body><h1>✓ You're signed in</h1>\
<p>Workforce has received the authorization. You can close this tab and return to the terminal.</p></body></html>";

const FAILURE_HTML: &str = "<!doctype html><html><head><meta charset=utf-8><title>Workforce Google login</title>\
<style>body{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center;color:#222}\
h1{color:#c62828}</style></head><body><h1>✗ Authorization failed</h1>\
<p>Check the terminal for details. You can close this tab.</p></body></html>";

// ─── Hub-hosted web-redirect flow — see WEB_SCOPES. ─────────────────────────

/// One-shot values generated at web-flow initiation.
///
/// The caller stashes `state` + `verifier` (keyed by `state`) in a short-TTL
/// store, emits `authorization_url` to the user, and on callback looks up the
/// verifier by `state` to feed into [`exchange_web_code`].
#[derive(Debug, Clone)]
pub struct WebFlowStart {
    pub authorization_url: String,
    pub state: String,
    pub verifier: String,
}

/// Build the authorization URL + PKCE material for the web-redirect flow.
pub fn start_web_flow(creds: &OAuthClientCreds, redirect_uri: &str) -> WebFlowStart {
    let verifier = gen_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    let state = gen_code_verifier();
    let authorization_url = build_authorize_url(
        creds,
        redirect_uri,
        &WEB_SCOPES.join(" "),
        &challenge,
        &state,
    );
    WebFlowStart {
        authorization_url,
        state,
        verifier,
    }
}

/// Exchange the `code` returned on the hub callback for `StoredTokens`.
///
/// Does not persist — the callback handler chooses where to write, since the
/// user identity is carried in the state store rather than a task-local.
pub async fn exchange_web_code(
    http: &reqwest::Client,
    creds: &OAuthClientCreds,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<StoredTokens> {
    let resp = exchange_code(http, creds, code, verifier, redirect_uri).await?;
    finalize_tokens(http, resp, WEB_SCOPES).await
}

/// Persist tokens at a per-user prefix in the global vault.
///
/// Writes to `users/{uid}/skills/google/workspaces/{workspace}/*` instead of
/// the bare `skills/google/workspaces/{workspace}/*`. Used by the hub
/// callback, which has no `CURRENT_USER_ID` task-local but does know the uid
/// from the state-store entry captured at flow initiation.
pub fn persist_tokens_at_user_scope(
    global_vault: &dyn SecretStore,
    uid: &str,
    workspace: &str,
    tokens: &StoredTokens,
) -> Result<()> {
    persist_at(
        global_vault,
        &format!("users/{uid}/skills/google/workspaces/{workspace}"),
        tokens,
        VaultActor::User(uid),
    )
}

// ─── Device authorization grant ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    interval: Option<i64>,
}

/// Pending device-authorization state. The agent flow splits login into
/// `login` → `login_poll`, so this state is persisted to the vault (under
/// `skills/google/workspaces/<name>/pending/*`) and cleared on any
/// terminal outcome. `device_code` is the opaque secret Google expects at
/// the token endpoint; `user_code` + `verification_url` are what the user
/// sees at google.com/device.
#[derive(Debug, Clone)]
pub struct DevicePending {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    /// Minimum seconds between polls (RFC 8628 §3.5). Grows on `slow_down`.
    pub interval_secs: u64,
    /// Unix seconds at which the device_code expires.
    pub expires_at_unix: u64,
}

#[derive(Debug, Clone)]
pub enum DevicePollOutcome {
    /// Tokens already persisted under the workspace.
    Authorized(StoredTokens),
    Pending,
    Expired,
    Denied,
    SlowDown {
        new_interval_secs: u64,
    },
}

/// Request a new device code. Non-blocking; does not persist.
pub async fn request_device_code(
    http: &reqwest::Client,
    creds: &OAuthClientCreds,
) -> Result<DevicePending> {
    let scope = DEVICE_SCOPES.join(" ");
    let dev_params: HashMap<&str, String> =
        HashMap::from([("client_id", creds.client_id.clone()), ("scope", scope)]);
    let dev_resp = http
        .post(DEVICE_URL)
        .form(&dev_params)
        .send()
        .await
        .wrap_err("device_code request failed")?;
    if !dev_resp.status().is_success() {
        let body = dev_resp.text().await.unwrap_or_default();
        bail!("device_code endpoint returned error: {body}");
    }
    let dev: DeviceCodeResponse = dev_resp
        .json()
        .await
        .wrap_err("parsing device_code response")?;

    let interval_secs = dev.interval.unwrap_or(5).max(1) as u64;
    let ttl_secs = dev.expires_in.unwrap_or(1800).max(60) as u64;
    Ok(DevicePending {
        device_code: dev.device_code,
        user_code: dev.user_code,
        verification_url: dev.verification_url,
        interval_secs,
        expires_at_unix: now_unix() + ttl_secs,
    })
}

/// Run one token-endpoint poll. Does not sleep; the caller controls cadence.
/// On `Authorized`, tokens are already persisted under `workspace`.
pub async fn poll_device_token_once(
    http: &reqwest::Client,
    store: &dyn SecretStore,
    workspace: &str,
    creds: &OAuthClientCreds,
    pending: &DevicePending,
) -> Result<DevicePollOutcome> {
    if now_unix() >= pending.expires_at_unix {
        return Ok(DevicePollOutcome::Expired);
    }

    let params: HashMap<&str, String> = HashMap::from([
        ("client_id", creds.client_id.clone()),
        ("client_secret", creds.client_secret.clone()),
        ("device_code", pending.device_code.clone()),
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        ),
    ]);
    let resp = http
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .wrap_err("device token poll failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status.is_success() {
        let parsed: GoogleTokenResponse = serde_json::from_str(&body)
            .wrap_err_with(|| format!("parsing device token response: {body}"))?;
        let stored = finalize_tokens(http, parsed, DEVICE_SCOPES).await?;
        persist_tokens(store, workspace, &stored)?;
        info!(
            workspace = %workspace,
            email = ?stored.email,
            "Google OAuth device login complete"
        );
        return Ok(DevicePollOutcome::Authorized(stored));
    }

    let err: GoogleTokenError = serde_json::from_str(&body).unwrap_or(GoogleTokenError {
        error: format!("http {}", status.as_u16()),
        error_description: Some(body.clone()),
    });
    match err.error.as_str() {
        "authorization_pending" => Ok(DevicePollOutcome::Pending),
        "slow_down" => Ok(DevicePollOutcome::SlowDown {
            new_interval_secs: pending.interval_secs + 5,
        }),
        "access_denied" => Ok(DevicePollOutcome::Denied),
        "expired_token" => Ok(DevicePollOutcome::Expired),
        other => bail!(
            "device token poll error: {other} {}",
            err.error_description.unwrap_or_default()
        ),
    }
}

const PENDING_KEYS: [&str; 5] = [
    "device_code",
    "user_code",
    "verification_url",
    "interval_secs",
    "expires_at",
];

fn pending_path(workspace: &str, key: &str) -> String {
    format!("skills/google/workspaces/{workspace}/pending/{key}")
}

pub fn save_pending_device(
    store: &dyn SecretStore,
    workspace: &str,
    pending: &DevicePending,
) -> Result<()> {
    store
        .set(
            &pending_path(workspace, "device_code"),
            &pending.device_code,
        )
        .wrap_err("failed to persist pending device_code")?;
    store
        .set(&pending_path(workspace, "user_code"), &pending.user_code)
        .wrap_err("failed to persist pending user_code")?;
    store
        .set(
            &pending_path(workspace, "verification_url"),
            &pending.verification_url,
        )
        .wrap_err("failed to persist pending verification_url")?;
    store
        .set(
            &pending_path(workspace, "interval_secs"),
            &pending.interval_secs.to_string(),
        )
        .wrap_err("failed to persist pending interval_secs")?;
    store
        .set(
            &pending_path(workspace, "expires_at"),
            &pending.expires_at_unix.to_string(),
        )
        .wrap_err("failed to persist pending expires_at")?;
    Ok(())
}

/// Returns `None` if any pending key is missing or unparseable.
pub fn load_pending_device(store: &dyn SecretStore, workspace: &str) -> Option<DevicePending> {
    let device_code = store.get(&pending_path(workspace, "device_code"))?;
    let user_code = store.get(&pending_path(workspace, "user_code"))?;
    let verification_url = store.get(&pending_path(workspace, "verification_url"))?;
    let interval_secs = store
        .get(&pending_path(workspace, "interval_secs"))
        .and_then(|s| s.parse::<u64>().ok())?;
    let expires_at_unix = store
        .get(&pending_path(workspace, "expires_at"))
        .and_then(|s| s.parse::<u64>().ok())?;
    Some(DevicePending {
        device_code,
        user_code,
        verification_url,
        interval_secs,
        expires_at_unix,
    })
}

pub fn clear_pending_device(store: &dyn SecretStore, workspace: &str) {
    for key in PENDING_KEYS {
        let _ = store.delete(&pending_path(workspace, key));
    }
}

/// Blocking device-login loop used by the CLI. Agents should use
/// [`request_device_code`] + [`poll_device_token_once`] directly.
async fn run_device_login(
    store: Arc<dyn SecretStore>,
    workspace: String,
    creds: OAuthClientCreds,
) -> Result<StoredTokens> {
    let http = reqwest::Client::builder()
        .build()
        .wrap_err("building http client")?;

    let mut pending = request_device_code(&http, &creds).await?;

    println!(
        "\n🔐 Workforce Google login (device flow)\n\
         \n  1. Visit: {}\n  2. Enter code: {}\n\
         \nWaiting for you to complete authorization…\n",
        pending.verification_url, pending.user_code
    );

    loop {
        tokio::time::sleep(Duration::from_secs(pending.interval_secs)).await;

        match poll_device_token_once(&http, store.as_ref(), &workspace, &creds, &pending).await? {
            DevicePollOutcome::Authorized(stored) => return Ok(stored),
            DevicePollOutcome::Pending => continue,
            DevicePollOutcome::SlowDown { new_interval_secs } => {
                pending.interval_secs = new_interval_secs;
                continue;
            }
            DevicePollOutcome::Denied => bail!("user denied authorization"),
            DevicePollOutcome::Expired => {
                bail!("device code expired before authorization completed")
            }
        }
    }
}

pub fn delete_workspace(store: &dyn SecretStore, workspace: &str) -> Result<()> {
    let base = format!("skills/google/workspaces/{workspace}");
    for key in [
        "email",
        "refresh_token",
        "access_token",
        "expires_at",
        "scopes",
    ] {
        let _ = store.delete(&format!("{base}/{key}"));
    }
    clear_pending_device(store, workspace);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[test]
    fn pkce_verifier_is_url_safe_and_long_enough() {
        let v = gen_code_verifier();
        assert!(v.len() >= 43, "too short: {}", v.len());
        for c in v.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-url-safe char: {c}"
            );
        }
    }

    #[test]
    fn pkce_challenge_is_deterministic_s256() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let c = code_challenge_s256(v);
        // RFC 7636 appendix B example.
        assert_eq!(c, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn two_verifiers_differ() {
        assert_ne!(gen_code_verifier(), gen_code_verifier());
    }

    #[test]
    fn url_encoding_spaces() {
        assert_eq!(url_encode("a b"), "a+b");
    }

    #[test]
    fn device_scopes_use_narrow_gmail_subset() {
        assert!(DEVICE_SCOPES.iter().any(|s| s.ends_with("/gmail.modify")));
        assert!(DEVICE_SCOPES
            .iter()
            .any(|s| s.ends_with("/gmail.settings.basic")));
        assert!(DEVICE_SCOPES.iter().any(|s| s.ends_with("/calendar")));
        // Broad scope must not leak into the device-flow list — Google rejects
        // it on the TV/Limited-Input client type the device flow requires.
        assert!(!DEVICE_SCOPES.contains(&"https://mail.google.com/"));
    }

    #[test]
    fn web_scopes_include_broad_gmail_scope() {
        assert!(WEB_SCOPES.contains(&"https://mail.google.com/"));
        assert!(WEB_SCOPES.iter().any(|s| s.ends_with("/calendar")));
        // Redundant narrower Gmail scopes should not appear alongside the
        // broad one — `mail.google.com/` already covers them.
        assert!(!WEB_SCOPES.iter().any(|s| s.ends_with("/gmail.modify")));
    }

    #[test]
    fn from_store_picks_up_redirect_uri_when_present() {
        let store = MemStore::default();
        store
            .set("skills/google/oauth_client/client_id", "cid")
            .unwrap();
        store
            .set("skills/google/oauth_client/client_secret", "secret")
            .unwrap();
        store
            .set(
                "skills/google/oauth_client/redirect_uri",
                "https://example.com/api/auth/google/callback",
            )
            .unwrap();

        let creds = OAuthClientCreds::from_store_or_bundled(&store).unwrap();
        assert_eq!(
            creds.redirect_uri.as_deref(),
            Some("https://example.com/api/auth/google/callback")
        );
    }

    #[test]
    fn from_store_defaults_redirect_uri_to_none() {
        let store = MemStore::default();
        store
            .set("skills/google/oauth_client/client_id", "cid")
            .unwrap();
        store
            .set("skills/google/oauth_client/client_secret", "secret")
            .unwrap();

        let creds = OAuthClientCreds::from_store_or_bundled(&store).unwrap();
        assert!(creds.redirect_uri.is_none());
    }

    #[test]
    fn start_web_flow_produces_expected_url_shape() {
        let creds = OAuthClientCreds {
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: "secret".into(),
            redirect_uri: Some("https://hub.example/api/auth/google/callback".into()),
        };
        let started = start_web_flow(&creds, "https://hub.example/api/auth/google/callback");

        assert!(started.authorization_url.starts_with(AUTH_URL));
        assert!(started.authorization_url.contains("response_type=code"));
        assert!(started
            .authorization_url
            .contains("client_id=cid.apps.googleusercontent.com"));
        assert!(started
            .authorization_url
            .contains("redirect_uri=https%3A%2F%2Fhub.example%2Fapi%2Fauth%2Fgoogle%2Fcallback"));
        assert!(started
            .authorization_url
            .contains("scope=https%3A%2F%2Fmail.google.com%2F"));
        assert!(started
            .authorization_url
            .contains("code_challenge_method=S256"));
        assert!(started.authorization_url.contains("access_type=offline"));
        assert!(started.authorization_url.contains("prompt=consent"));
        // State and verifier come from 32 random bytes base64url-encoded —
        // ≥ 40 chars catches a regression where the RNG is fed a tiny
        // buffer or the encoding is silently truncated.
        assert!(started.state.len() >= 40, "state={}", started.state);
        assert!(
            started.verifier.len() >= 40,
            "verifier={}",
            started.verifier
        );
        assert_ne!(started.state, started.verifier);
    }

    #[test]
    fn persist_tokens_at_user_scope_writes_expected_keys() {
        let vault = MemStore::default();
        let tokens = StoredTokens {
            email: Some("user@x.test".into()),
            refresh_token: "rt".into(),
            access_token: "at".into(),
            expires_at: 1_700_000_000,
            scopes: WEB_SCOPES.join(" "),
        };
        persist_tokens_at_user_scope(&vault, "kings", "default", &tokens).unwrap();

        let base = "users/kings/skills/google/workspaces/default";
        assert_eq!(
            vault.get(&format!("{base}/refresh_token")).as_deref(),
            Some("rt")
        );
        assert_eq!(
            vault.get(&format!("{base}/access_token")).as_deref(),
            Some("at")
        );
        assert_eq!(
            vault.get(&format!("{base}/email")).as_deref(),
            Some("user@x.test")
        );
        assert_eq!(
            vault.get(&format!("{base}/expires_at")).as_deref(),
            Some("1700000000")
        );
        assert!(vault.get(&format!("{base}/scopes")).is_some());
        // Must NOT land at the bare (device-flow) path.
        assert!(vault
            .get("skills/google/workspaces/default/refresh_token")
            .is_none());
    }

    #[derive(Default)]
    struct MemStore {
        inner: Mutex<HashMap<String, String>>,
    }

    impl SecretStore for MemStore {
        fn get(&self, path: &str) -> Option<String> {
            self.inner.lock().unwrap().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> Result<()> {
            self.inner
                .lock()
                .unwrap()
                .insert(path.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, path: &str) -> Result<bool> {
            Ok(self.inner.lock().unwrap().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            let guard = self.inner.lock().unwrap();
            guard
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    fn fixture() -> DevicePending {
        DevicePending {
            device_code: "dc-abc".into(),
            user_code: "ABCD-EFGH".into(),
            verification_url: "https://www.google.com/device".into(),
            interval_secs: 5,
            expires_at_unix: 1_700_000_000,
        }
    }

    #[test]
    fn pending_roundtrip() {
        let store = MemStore::default();
        let p = fixture();
        save_pending_device(&store, "default", &p).unwrap();
        let loaded = load_pending_device(&store, "default").expect("pending missing");
        assert_eq!(loaded.device_code, p.device_code);
        assert_eq!(loaded.user_code, p.user_code);
        assert_eq!(loaded.verification_url, p.verification_url);
        assert_eq!(loaded.interval_secs, p.interval_secs);
        assert_eq!(loaded.expires_at_unix, p.expires_at_unix);
    }

    #[test]
    fn pending_partial_is_none() {
        let store = MemStore::default();
        save_pending_device(&store, "default", &fixture()).unwrap();
        store
            .delete("skills/google/workspaces/default/pending/device_code")
            .unwrap();
        assert!(load_pending_device(&store, "default").is_none());
    }

    #[test]
    fn pending_unparseable_is_none() {
        let store = MemStore::default();
        save_pending_device(&store, "default", &fixture()).unwrap();
        store
            .set(
                "skills/google/workspaces/default/pending/expires_at",
                "not-a-number",
            )
            .unwrap();
        assert!(load_pending_device(&store, "default").is_none());
    }

    #[test]
    fn clear_is_idempotent() {
        let store = MemStore::default();
        clear_pending_device(&store, "default");
        save_pending_device(&store, "default", &fixture()).unwrap();
        clear_pending_device(&store, "default");
        clear_pending_device(&store, "default");
        assert!(load_pending_device(&store, "default").is_none());
    }

    #[test]
    fn clear_is_scoped_per_workspace() {
        let store = MemStore::default();
        save_pending_device(&store, "alice", &fixture()).unwrap();
        save_pending_device(&store, "bob", &fixture()).unwrap();
        clear_pending_device(&store, "alice");
        assert!(load_pending_device(&store, "alice").is_none());
        assert!(load_pending_device(&store, "bob").is_some());
    }

    #[test]
    fn delete_workspace_also_clears_pending() {
        let store = MemStore::default();
        let tokens = StoredTokens {
            email: Some("a@x".into()),
            refresh_token: "rt".into(),
            access_token: "at".into(),
            expires_at: 1,
            scopes: DEVICE_SCOPES.join(" "),
        };
        persist_tokens(&store, "alice", &tokens).unwrap();
        save_pending_device(&store, "alice", &fixture()).unwrap();
        delete_workspace(&store, "alice").unwrap();
        assert!(load_pending_device(&store, "alice").is_none());
        assert!(store
            .get("skills/google/workspaces/alice/refresh_token")
            .is_none());
    }
}
