//! The ChatGPT login Codex writes as `auth.json`, and its refresh.
//!
//! A refresh token is single-use: refreshing returns a new one and a second
//! refresh with the old one fails `refresh_token_reused`. So every refresh
//! and every write of a login happens under the vault's fleet-wide refresh
//! lock for that slot, after a `sync`, and a stored login is only replaced by
//! a refresh of the very token it holds.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use workforce_common::secret_lock::RefreshGuard;
use workforce_common::settings::{SecretStore, VaultActor};

use super::resolve::VaultSlot;

/// Codex's OAuth token endpoint.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex's public OAuth client id, which ChatGPT logins are issued to.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Codex refreshes a login with no readable expiry once it is this old.
const STALE_WITHOUT_EXPIRY: TimeDelta = TimeDelta::days(8);
const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";

const JWT_FIELDS: [&str; 2] = ["/tokens/access_token", "/tokens/id_token"];
const API_KEY_FIELD: &str = "/OPENAI_API_KEY";
const ELLIPSES: [&str; 2] = ["…", "..."];
const MIN_JWT_SEGMENT: usize = 10;
const MIN_API_KEY_LEN: usize = 20;
const ABBREVIATED_HINT: &str = "(it was abbreviated on the way in); store auth.json directly in the vault, never through a chat message";
/// How long a refused refresh's response may run in an error.
const MAX_ERROR_BODY: usize = 300;
/// A run of token characters this long is treated as a secret in errors.
const MIN_SECRET_RUN: usize = 32;
/// The writes a refreshed login gets before it is kept in memory instead.
const STORE_ATTEMPTS: u32 = 3;
const STORE_BACKOFF: Duration = Duration::from_millis(200);

/// `$CODEX_HOME/auth.json`. Fields this hub does not use are kept as they
/// were, so a write-back never drops what the CLI stored.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    /// Codex writes `null` here for a ChatGPT login.
    #[serde(rename = "OPENAI_API_KEY", default)]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl std::fmt::Debug for AuthJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthJson")
            .field("auth_mode", &self.auth_mode)
            .field("account_id", &self.account_id())
            .field("last_refresh", &self.last_refresh)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

fn abbreviated(value: &str) -> bool {
    ELLIPSES.iter().any(|e| value.contains(e))
}

fn whole_jwt(value: &str) -> bool {
    let segments: Vec<&str> = value.split('.').collect();
    segments.len() == 3 && segments.iter().all(|s| s.len() >= MIN_JWT_SEGMENT)
}

fn field_name(pointer: &str) -> &str {
    pointer.rsplit('/').next().unwrap_or(pointer)
}

/// The claims of a JWT, unverified: only its own expiry and account are read.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The account a ChatGPT sign-in belongs to, as its id token names it
/// (unverified, for display).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub email: Option<String>,
    pub plan: Option<String>,
}

/// The tokens a sign-in was issued.
pub struct TokenSet {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

impl AuthJson {
    /// The `auth.json` Codex writes for a fresh sign-in.
    pub fn signed_in(tokens: TokenSet, now: DateTime<Utc>) -> Self {
        let account_id = jwt_claims(&tokens.id_token).and_then(|claims| {
            claims
                .get(ACCOUNT_CLAIM)?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_string)
        });
        Self {
            auth_mode: Some("chatgpt".to_string()),
            openai_api_key: None,
            tokens: Some(Tokens {
                id_token: tokens.id_token,
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                account_id,
                rest: Map::new(),
            }),
            last_refresh: Some(now),
            rest: Map::new(),
        }
    }

    /// Whose account the sign-in is and on which plan.
    pub fn account(&self) -> Account {
        let claims = self
            .tokens
            .as_ref()
            .and_then(|tokens| jwt_claims(&tokens.id_token));
        let claim = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
        Account {
            email: claim(claims.as_ref().and_then(|c| c.get("email"))),
            plan: claim(
                claims
                    .as_ref()
                    .and_then(|c| c.get(ACCOUNT_CLAIM))
                    .and_then(|auth| auth.get("chatgpt_plan_type")),
            ),
        }
    }

    /// Parse what `codex login` wrote. A ChatGPT login carries
    /// `tokens.access_token` (and usually `tokens.id_token`), an API-key login
    /// carries `OPENAI_API_KEY`; every token present must be whole — no
    /// ellipsis, and a JWT's three segments each long enough to be real.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| {
            format!("not JSON ({e}); store the whole auth.json as `codex login` wrote it")
        })?;
        let field = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let api_key = field(API_KEY_FIELD);
        let jwts: Vec<(&str, &str)> = JWT_FIELDS
            .iter()
            .filter_map(|p| field(p).map(|v| (field_name(p), v)))
            .collect();
        if jwts.is_empty() && api_key.is_none() {
            return Err("auth.json carries neither tokens.access_token nor OPENAI_API_KEY; run `codex login --device-auth` and store the file it writes".to_string());
        }
        let cut_token = jwts
            .iter()
            .find(|(_, token)| abbreviated(token) || !whole_jwt(token))
            .map(|(name, _)| *name)
            .or_else(|| {
                value
                    .pointer("/tokens")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .find(|(_, v)| v.as_str().is_some_and(abbreviated))
                    .map(|(name, _)| name.as_str())
            });
        if let Some(name) = cut_token {
            return Err(format!(
                "tokens.{name} is not a whole token {ABBREVIATED_HINT}"
            ));
        }
        if api_key.is_some_and(|key| abbreviated(key) || key.len() < MIN_API_KEY_LEN) {
            return Err(format!(
                "OPENAI_API_KEY is not a whole key {ABBREVIATED_HINT}"
            ));
        }
        serde_json::from_value(value).map_err(|e| {
            format!(
                "auth.json is not a Codex login ({}); run `codex login --device-auth` and store the file it writes",
                without_values(&e.to_string())
            )
        })
    }

    pub fn render(&self) -> String {
        serde_json::to_string_pretty(self).expect("auth.json serialises")
    }

    pub fn access_token(&self) -> Option<&str> {
        self.tokens.as_ref().map(|t| t.access_token.as_str())
    }

    pub fn refresh_token(&self) -> Option<&str> {
        self.tokens.as_ref().map(|t| t.refresh_token.as_str())
    }

    /// The ChatGPT account the login belongs to: stable across refreshes.
    pub fn account_id(&self) -> Option<String> {
        let tokens = self.tokens.as_ref()?;
        tokens.account_id.clone().or_else(|| {
            jwt_claims(&tokens.id_token)?
                .get(ACCOUNT_CLAIM)?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_string)
        })
    }

    /// The member of the account the login signed in as (the id token's
    /// `sub`): on a Team plan the account is the workspace.
    pub fn member(&self) -> Option<String> {
        jwt_claims(&self.tokens.as_ref()?.id_token)?
            .get("sub")?
            .as_str()
            .map(str::to_string)
    }

    /// Identifies this very sign-in, which a refresh replaces: a hash of its
    /// refresh token (an API-key login's key).
    pub fn fingerprint(&self) -> String {
        let secret = self
            .refresh_token()
            .or(self.openai_api_key.as_deref())
            .unwrap_or_default();
        hex::encode(&Sha256::digest(secret.as_bytes())[..8])
    }

    /// When the access token expires, from its (unverified) `exp` claim.
    fn access_expires_at(&self) -> Option<DateTime<Utc>> {
        let exp = jwt_claims(&self.tokens.as_ref()?.access_token)?
            .get("exp")?
            .as_i64()?;
        DateTime::from_timestamp(exp, 0)
    }

    /// Whether the access token expires within `horizon` of `now`. A token
    /// with no readable expiry follows Codex: refresh once the login is
    /// eight days old. An API-key login never needs one.
    fn needs_refresh(&self, now: DateTime<Utc>, horizon: TimeDelta) -> bool {
        match (&self.tokens, self.access_expires_at()) {
            (None, _) => false,
            (Some(_), Some(expires)) => now
                .checked_add_signed(horizon)
                .is_none_or(|limit| expires <= limit),
            (Some(_), None) => self
                .last_refresh
                .is_none_or(|last| last + STALE_WITHOUT_EXPIRY <= now),
        }
    }

    fn refreshed(mut self, response: RefreshResponse, now: DateTime<Utc>) -> Self {
        if let Some(tokens) = self.tokens.as_mut() {
            [
                (&mut tokens.id_token, response.id_token),
                (&mut tokens.access_token, response.access_token),
                (&mut tokens.refresh_token, response.refresh_token),
            ]
            .into_iter()
            .for_each(|(field, returned)| {
                if let Some(value) = returned {
                    *field = value;
                }
            });
        }
        self.last_refresh = Some(now);
        self
    }
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Why a refresh token was refused for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefreshFailure {
    #[error("the refresh token expired; sign in again")]
    Expired,
    #[error("the refresh token was already used, so another copy of this login refreshed it; sign in again")]
    Reused,
    #[error("the refresh token was revoked; sign in again")]
    Revoked,
    #[error("the refresh token was refused; sign in again")]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    #[error("ChatGPT login can no longer be refreshed: {0}")]
    Permanent(RefreshFailure),
    #[error("ChatGPT token refresh failed: {0}")]
    Transient(String),
    #[error("no usable ChatGPT login: {0}")]
    Unusable(String),
}

/// Codex's classification of a refused refresh (`login/src/auth/manager.rs`):
/// a 401, a named refresh-token code or an `invalid_grant` 400 is permanent;
/// anything else may pass.
fn classify(status: reqwest::StatusCode, body: &str) -> RefreshError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let code = parsed.as_ref().and_then(|v| {
        match v.get("error") {
            Some(Value::Object(error)) => error.get("code"),
            Some(code @ Value::String(_)) => Some(code),
            _ => v.get("code"),
        }
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
    });
    let failure = match code.as_deref() {
        Some("refresh_token_expired") => Some(RefreshFailure::Expired),
        Some("refresh_token_reused") => Some(RefreshFailure::Reused),
        Some("refresh_token_invalidated") => Some(RefreshFailure::Revoked),
        Some("invalid_grant") if status == reqwest::StatusCode::BAD_REQUEST => {
            Some(RefreshFailure::Other)
        }
        _ if status == reqwest::StatusCode::UNAUTHORIZED => Some(RefreshFailure::Other),
        _ => None,
    };
    match failure {
        Some(failure) => RefreshError::Permanent(failure),
        None => RefreshError::Transient(format!("{status}: {}", error_excerpt(body))),
    }
}

/// `text` with every quoted value elided, as serde errors quote the value
/// they refused.
fn without_values(text: &str) -> String {
    text.split('"')
        .enumerate()
        .map(|(i, part)| if i % 2 == 1 { "…" } else { part })
        .collect::<Vec<_>>()
        .join("\"")
}

/// A response body fit for an error: long token-like runs elided, and cut to
/// [`MAX_ERROR_BODY`] characters.
fn error_excerpt(body: &str) -> String {
    let token_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=');
    let mut out = String::new();
    let mut rest = body;
    while let Some(start) = rest.find(token_char) {
        out.push_str(&rest[..start]);
        let run = &rest[start..];
        let len = run.find(|c: char| !token_char(c)).unwrap_or(run.len());
        out.push_str(if len >= MIN_SECRET_RUN {
            "…"
        } else {
            &run[..len]
        });
        rest = &run[len..];
    }
    out.push_str(rest);
    match out.char_indices().nth(MAX_ERROR_BODY) {
        Some((cut, _)) => format!("{}…", &out[..cut]),
        None => out,
    }
}

/// Refreshes ChatGPT logins against the token endpoint.
#[derive(Clone)]
pub struct TokenRefresher {
    http: reqwest::Client,
    token_url: String,
}

impl TokenRefresher {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            token_url: TOKEN_URL.to_string(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn at(http: reqwest::Client, token_url: String) -> Self {
        Self { http, token_url }
    }

    /// Spend `auth`'s refresh token for a new set. The caller holds the
    /// slot's refresh lock and persists the result.
    pub async fn refresh(&self, auth: &AuthJson) -> Result<AuthJson, RefreshError> {
        let refresh_token = auth.refresh_token().ok_or_else(|| {
            RefreshError::Unusable("an API-key login has nothing to refresh".into())
        })?;
        let response = self
            .http
            .post(&self.token_url)
            .json(&RefreshRequest {
                client_id: CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token,
            })
            .send()
            .await
            .map_err(|e| RefreshError::Transient(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(classify(status, &body));
        }
        let refreshed: RefreshResponse = response
            .json()
            .await
            .map_err(|e| RefreshError::Transient(e.to_string()))?;
        Ok(auth.clone().refreshed(refreshed, Utc::now()))
    }

    /// `seen`, the slot's login, refreshed first when its access token
    /// expires within `horizon` (`TimeDelta::MAX`: refresh unless a peer
    /// already has). One refresh per slot runs at a time, fleet-wide; a
    /// caller that waited on the lock gets the login its peer stored.
    pub async fn fresh(
        &self,
        slot: &VaultSlot,
        seen: &AuthJson,
        horizon: TimeDelta,
    ) -> Result<AuthJson, RefreshError> {
        if !seen.needs_refresh(Utc::now(), horizon) {
            slot.retry_unstored().await;
            return Ok(seen.clone());
        }
        let _guard = slot.lock().await.map_err(transient)?;
        slot.vault.sync().map_err(transient)?;
        let stored = slot.auth()?;
        if stored.refresh_token() != seen.refresh_token()
            || !stored.needs_refresh(Utc::now(), horizon)
        {
            return Ok(stored);
        }
        let refreshed = self.refresh(&stored).await?;
        slot.store(&refreshed).await;
        tracing::info!(slot = %slot.key, "refreshed ChatGPT login");
        Ok(refreshed)
    }

    /// The login to retry with after `failed` was refused as unauthorized,
    /// in Codex's order: the stored login when another process has already
    /// replaced it, else a refresh of it.
    pub async fn renew(
        &self,
        slot: &VaultSlot,
        failed: &AuthJson,
    ) -> Result<AuthJson, RefreshError> {
        slot.vault.sync().map_err(transient)?;
        let stored = slot.auth()?;
        if stored.access_token() != failed.access_token() {
            return Ok(stored);
        }
        self.fresh(slot, &stored, TimeDelta::MAX).await
    }
}

fn transient(e: eyre::Report) -> RefreshError {
    RefreshError::Transient(e.to_string())
}

/// Refreshed logins the vault would not store, by slot. Each holds the only
/// live refresh token of its login, so it is served in place of the stored
/// one and written again on the slot's next use, until a write lands or a
/// new sign-in replaces it.
static UNSTORED: LazyLock<parking_lot::Mutex<HashMap<String, AuthJson>>> =
    LazyLock::new(Default::default);

/// The refreshed login `slot` holds only in memory, if its write failed.
pub fn unstored(slot: &VaultSlot) -> Option<AuthJson> {
    UNSTORED.lock().get(&slot.location()).cloned()
}

impl VaultSlot {
    /// The slot's login: the one held in memory while its write is failing,
    /// else the stored one.
    fn auth(&self) -> Result<AuthJson, RefreshError> {
        if let Some(auth) = unstored(self) {
            return Ok(auth);
        }
        let text = self
            .vault
            .get(&self.key)
            .ok_or_else(|| RefreshError::Unusable(format!("nothing stored at {}", self.key)))?;
        AuthJson::parse(text.trim()).map_err(RefreshError::Unusable)
    }

    pub async fn lock(&self) -> eyre::Result<RefreshGuard> {
        self.vault.refresh_lock().lock(&self.location()).await
    }

    /// Drop a refreshed login held in memory for this slot, once the slot
    /// holds another sign-in or none. The caller holds the slot's lock.
    pub fn forget_unstored(&self) {
        UNSTORED.lock().remove(&self.location());
    }

    /// Store a refreshed login, retrying a failed write. The caller holds
    /// the slot's lock. A login no write stored is kept in memory, since its
    /// predecessor's refresh token is already spent.
    async fn store(&self, auth: &AuthJson) {
        let rendered = auth.render();
        let mut last_error = None;
        for attempt in 0..STORE_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(STORE_BACKOFF * attempt).await;
            }
            match self.vault.set_with_actor(
                &self.key,
                &rendered,
                VaultActor::System("chatgpt_refresh"),
            ) {
                Ok(()) => {
                    self.forget_unstored();
                    return;
                }
                Err(e) => last_error = Some(e),
            }
        }
        tracing::error!(
            slot = %self.key,
            error = ?last_error,
            "could not store a refreshed ChatGPT login; serving it from memory until a write succeeds"
        );
        UNSTORED.lock().insert(self.location(), auth.clone());
    }

    /// Write a login held in memory to the vault, if there is one.
    async fn retry_unstored(&self) {
        if unstored(self).is_none() {
            return;
        }
        let Ok(_guard) = self.lock().await else {
            return;
        };
        if let Some(auth) = unstored(self) {
            self.store(&auth).await;
        }
    }
}

/// After a CLI round that may have refreshed the login it was handed, keep
/// the refreshed tokens in the slot the login came from — unless the slot
/// moved on from `handed`, the [`AuthJson::fingerprint`] of the login the
/// round was handed, which only a new sign-in or another holder's refresh
/// does. Returns the login the slot holds.
pub async fn write_back(
    slot: &VaultSlot,
    handed: &str,
    after: &AuthJson,
) -> eyre::Result<AuthJson> {
    if after.fingerprint() == handed {
        slot.vault.sync()?;
        return slot.auth().map_err(|e| eyre::eyre!(e));
    }
    let _guard = slot.lock().await?;
    slot.vault.sync()?;
    let stored = slot.auth().map_err(|e| eyre::eyre!(e))?;
    let current = stored.fingerprint();
    if current == handed {
        slot.store(after).await;
        return Ok(after.clone());
    }
    if current != after.fingerprint() {
        tracing::warn!(
            slot = %slot.key,
            "ChatGPT login changed during the round; keeping the stored login over the round's refresh"
        );
    }
    Ok(stored)
}

/// Why an `auth.json` with an API key and no sign-in serves no plan.
pub const API_KEY_FILE: &str = "This file holds an API key, not a ChatGPT sign-in: sign in with Connect, or paste the auth.json `codex login --device-auth` writes";

/// How a person stores a ChatGPT sign-in made outside the hub.
pub const PASTE_FALLBACK: &str = "run `codex login --device-auth` in a fresh CODEX_HOME and store \
     the auth.json it writes under My Settings → ChatGPT, or with codex.setup_auth";

/// Why a login may not be stored.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("this sign-in is already stored on this hub as {0}. A refresh token works for one copy only, so each place needs its own sign-in: {PASTE_FALLBACK}")]
    Duplicate(&'static str),
    #[error(transparent)]
    Vault(#[from] eyre::Report),
}

/// Store a new login in `slot`, under the slot's refresh lock so an
/// in-flight refresh of the old login cannot overwrite it. A ChatGPT sign-in
/// already stored at any of `known` is refused: two copies of one refresh
/// token each spend it, and the second refresh fails for good.
pub async fn store_login(
    slot: &VaultSlot,
    value: &str,
    known: &[VaultSlot],
    actor: VaultActor<'_>,
) -> Result<(), StoreError> {
    let _guard = slot.lock().await?;
    refuse_elsewhere(value, slot, known)?;
    slot.vault.set_with_actor(&slot.key, value, actor)?;
    slot.forget_unstored();
    Ok(())
}

/// Refuse the ChatGPT sign-in `value` when a slot of `known` other than
/// `target` already holds it.
pub fn refuse_elsewhere(
    value: &str,
    target: &VaultSlot,
    known: &[VaultSlot],
) -> Result<(), StoreError> {
    let Ok(login) = AuthJson::parse(value) else {
        return Ok(());
    };
    let same_sign_in = |other: &AuthJson| {
        login.tokens.is_some()
            && other.account_id() == login.account_id()
            && other.refresh_token() == login.refresh_token()
    };
    let target = target.location();
    match known
        .iter()
        .filter(|other| other.location() != target)
        .find(|other| other.auth().is_ok_and(|auth| same_sign_in(&auth)))
    {
        Some(copy) => Err(StoreError::Duplicate(
            match copy.key.starts_with(crate::user_config::USER_PREFIX) {
                true => "a person's own login",
                false => "a login an agent or the hub shares",
            },
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use crate::hub_core::logins::test_support::jwt;
    use crate::vault::Vault;

    #[test]
    fn account_reads_email_and_plan_claims() {
        let signed_in = |claims: Value| {
            AuthJson::signed_in(
                TokenSet {
                    id_token: jwt(claims),
                    access_token: jwt(serde_json::json!({})),
                    refresh_token: "rt".into(),
                },
                Utc::now(),
            )
        };
        let account = signed_in(serde_json::json!({
            "email": "alice@example.com",
            ACCOUNT_CLAIM: { "chatgpt_plan_type": "pro", "chatgpt_account_id": "acct-1" },
        }))
        .account();
        assert_eq!(account.email.as_deref(), Some("alice@example.com"));
        assert_eq!(account.plan.as_deref(), Some("pro"));
        let bare = signed_in(serde_json::json!({ "sub": "user-1" })).account();
        assert_eq!((bare.email, bare.plan), (None, None));
    }

    fn login(refresh_token: &str, expires: DateTime<Utc>) -> String {
        serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt(serde_json::json!({
                    "sub": "user-1",
                    ACCOUNT_CLAIM: {"chatgpt_account_id": "acct-from-claim"}
                })),
                "access_token": jwt(serde_json::json!({"exp": expires.timestamp()})),
                "refresh_token": refresh_token,
                "account_id": "acct-1"
            },
            "last_refresh": "2026-09-13T00:00:00.123456Z",
            "agent_identity": {"kind": "future-field"}
        })
        .to_string()
    }

    fn slot(key: &str) -> (VaultSlot, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(&dir.path().join("vault.enc"), "test-key").unwrap();
        (
            VaultSlot {
                vault: Arc::new(vault),
                key: key.to_string(),
            },
            dir,
        )
    }

    /// Hands out `rt-1`, `rt-2`, … on each refresh, like the real endpoint.
    struct Rotating(AtomicUsize);

    impl Respond for Rotating {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            let expires = Utc::now() + TimeDelta::hours(1);
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "access_token": jwt(serde_json::json!({"exp": expires.timestamp()})),
                    "refresh_token": format!("rt-{n}"),
                }))
                .set_delay(std::time::Duration::from_millis(30))
        }
    }

    async fn token_server() -> (MockServer, TokenRefresher) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_partial_json(serde_json::json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token"
            })))
            .respond_with(Rotating(AtomicUsize::new(0)))
            .mount(&server)
            .await;
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            format!("{}/oauth/token", server.uri()),
        );
        (server, refresher)
    }

    #[test]
    fn auth_json_roundtrip_preserves_unknown_fields() {
        let text = login("rt-0", Utc::now());
        let auth = AuthJson::parse(&text).unwrap();
        let original: Value = serde_json::from_str(&text).unwrap();
        let rendered: Value = serde_json::from_str(&auth.render()).unwrap();
        assert_eq!(rendered, original);
        assert_eq!(auth.account_id().as_deref(), Some("acct-1"));
        assert!(
            !format!("{auth:?}").contains("rt-0"),
            "Debug never prints tokens"
        );
    }

    #[test]
    fn account_id_falls_back_to_the_id_token_claim() {
        let mut auth = AuthJson::parse(&login("rt-0", Utc::now())).unwrap();
        auth.tokens.as_mut().unwrap().account_id = None;
        assert_eq!(auth.account_id().as_deref(), Some("acct-from-claim"));
    }

    #[test]
    fn access_expiry_from_jwt_exp() {
        let now = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let expires = now + TimeDelta::minutes(4);
        let auth = AuthJson::parse(&login("rt-0", expires)).unwrap();
        assert_eq!(auth.access_expires_at(), Some(expires));
        assert!(auth.needs_refresh(now, TimeDelta::minutes(5)));
        assert!(!auth.needs_refresh(now, TimeDelta::minutes(3)));
        assert!(auth.needs_refresh(now, TimeDelta::MAX));

        let mut opaque = auth.clone();
        opaque.tokens.as_mut().unwrap().access_token = "not-a-jwt".into();
        opaque.last_refresh = Some(now - TimeDelta::days(9));
        assert!(opaque.needs_refresh(now, TimeDelta::zero()));
        opaque.last_refresh = Some(now - TimeDelta::days(7));
        assert!(!opaque.needs_refresh(now, TimeDelta::zero()));

        let api_key =
            AuthJson::parse(r#"{"OPENAI_API_KEY":"sk-proj-0123456789abcdefghij"}"#).unwrap();
        assert!(!api_key.needs_refresh(now, TimeDelta::MAX));
    }

    #[test]
    fn refresh_failures_classify_like_codex() {
        use reqwest::StatusCode;
        let permanent = |status, body| match classify(status, body) {
            RefreshError::Permanent(failure) => Some(failure),
            _ => None,
        };
        assert_eq!(
            permanent(
                StatusCode::BAD_REQUEST,
                r#"{"error":{"code":"refresh_token_reused"}}"#
            ),
            Some(RefreshFailure::Reused)
        );
        assert_eq!(
            permanent(
                StatusCode::BAD_REQUEST,
                r#"{"error":"refresh_token_expired"}"#
            ),
            Some(RefreshFailure::Expired)
        );
        assert_eq!(
            permanent(
                StatusCode::BAD_REQUEST,
                r#"{"code":"refresh_token_invalidated"}"#
            ),
            Some(RefreshFailure::Revoked)
        );
        assert_eq!(
            permanent(StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#),
            Some(RefreshFailure::Other)
        );
        assert_eq!(
            permanent(StatusCode::UNAUTHORIZED, "nope"),
            Some(RefreshFailure::Other)
        );
        assert!(matches!(
            classify(StatusCode::BAD_GATEWAY, "upstream"),
            RefreshError::Transient(_)
        ));
    }

    #[tokio::test]
    async fn concurrent_refresh_single_flight() {
        let (server, refresher) = token_server().await;
        let (slot, _dir) = slot("users/u1/codex/auth_json");
        slot.vault
            .set(
                &slot.key,
                &login("rt-0", Utc::now() + TimeDelta::minutes(1)),
            )
            .unwrap();

        let calls: Vec<_> = (0..8)
            .map(|_| {
                let (refresher, slot) = (refresher.clone(), slot.clone());
                tokio::spawn(async move {
                    let seen = slot.auth().unwrap();
                    refresher.fresh(&slot, &seen, TimeDelta::minutes(5)).await
                })
            })
            .collect();
        let results = futures::future::join_all(calls).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        for result in results {
            assert_eq!(result.unwrap().unwrap().refresh_token(), Some("rt-1"));
        }
        let stored = AuthJson::parse(&slot.vault.get(&slot.key).unwrap()).unwrap();
        assert_eq!(stored.refresh_token(), Some("rt-1"));
        assert_eq!(
            stored.rest.get("agent_identity"),
            Some(&serde_json::json!({"kind": "future-field"})),
            "a refresh keeps what the CLI stored"
        );
    }

    #[tokio::test]
    async fn refresh_reuses_a_peer_refresh_after_lock() {
        let (server, refresher) = token_server().await;
        let (slot, _dir) = slot("codex/auth_json");
        let stale = login("rt-0", Utc::now() - TimeDelta::minutes(1));
        slot.vault.set(&slot.key, &stale).unwrap();

        let guard = slot.lock().await.unwrap();
        let waiting = {
            let (refresher, slot) = (refresher.clone(), slot.clone());
            let seen = AuthJson::parse(&stale).unwrap();
            tokio::spawn(async move { refresher.fresh(&slot, &seen, TimeDelta::MAX).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        slot.vault
            .set(
                &slot.key,
                &login("rt-peer", Utc::now() + TimeDelta::hours(1)),
            )
            .unwrap();
        drop(guard);

        let got = waiting.await.unwrap().unwrap();
        assert_eq!(got.refresh_token(), Some("rt-peer"));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_refused_refresh_is_permanent_and_keeps_the_stored_login() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": {"code": "refresh_token_reused"}})),
            )
            .mount(&server)
            .await;
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            format!("{}/oauth/token", server.uri()),
        );
        let (slot, _dir) = slot("codex/auth_json");
        let stale = login("rt-0", Utc::now());
        slot.vault.set(&slot.key, &stale).unwrap();

        let seen = AuthJson::parse(&stale).unwrap();
        assert_eq!(
            refresher.fresh(&slot, &seen, TimeDelta::minutes(5)).await,
            Err(RefreshError::Permanent(RefreshFailure::Reused))
        );
        assert_eq!(slot.vault.get(&slot.key).as_deref(), Some(stale.as_str()));
    }

    #[tokio::test]
    async fn write_back_stores_a_refresh_made_during_the_round() {
        let (slot, _dir) = slot("users/u1/codex/auth_json");
        let expires = Utc::now() + TimeDelta::hours(1);
        slot.vault.set(&slot.key, &login("rt-0", expires)).unwrap();
        let started = AuthJson::parse(&login("rt-0", expires)).unwrap();
        let after = AuthJson::parse(&login("rt-1", expires)).unwrap();

        let unchanged = write_back(&slot, &started.fingerprint(), &started)
            .await
            .unwrap();
        assert_eq!(unchanged.refresh_token(), Some("rt-0"));
        let kept = write_back(&slot, &started.fingerprint(), &after)
            .await
            .unwrap();
        assert_eq!(kept.refresh_token(), Some("rt-1"));
        let stored = AuthJson::parse(&slot.vault.get(&slot.key).unwrap()).unwrap();
        assert_eq!(stored.refresh_token(), Some("rt-1"));
        let again = write_back(&slot, &started.fingerprint(), &after)
            .await
            .unwrap();
        assert_eq!(
            again.refresh_token(),
            Some("rt-1"),
            "keeping twice is harmless"
        );
    }

    #[tokio::test]
    async fn write_back_refuses_when_store_changed_since_round_start() {
        let (slot, _dir) = slot("codex/auth_json");
        let expires = Utc::now() + TimeDelta::hours(1);
        let started = AuthJson::parse(&login("rt-0", expires)).unwrap();
        let relogin = login("rt-new-sign-in", expires);
        slot.vault.set(&slot.key, &relogin).unwrap();

        let after = AuthJson::parse(&login("rt-1", expires)).unwrap();
        let held = write_back(&slot, &started.fingerprint(), &after)
            .await
            .unwrap();
        assert_eq!(held.refresh_token(), Some("rt-new-sign-in"));
        assert_eq!(slot.vault.get(&slot.key).as_deref(), Some(relogin.as_str()));
    }

    #[tokio::test]
    async fn a_refresh_the_vault_cannot_store_is_served_until_a_write_lands() {
        let (slot, dir) = slot("codex/auth_json");
        let vault_file = dir.path().join("vault.enc");
        let aside = dir.path().join("vault.enc.aside");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(crate::hub_core::logins::test_support::BlocksTheVault {
                vault_file: vault_file.clone(),
                aside: aside.clone(),
            })
            .mount(&server)
            .await;
        let refresher = TokenRefresher::at(
            reqwest::Client::new(),
            format!("{}/oauth/token", server.uri()),
        );
        let stale = login("rt-0", Utc::now() + TimeDelta::minutes(1));
        slot.vault.set(&slot.key, &stale).unwrap();
        let seen = AuthJson::parse(&stale).unwrap();

        let refreshed = refresher
            .fresh(&slot, &seen, TimeDelta::minutes(5))
            .await
            .unwrap();
        assert_eq!(refreshed.refresh_token(), Some("rt-1"));
        std::fs::remove_dir_all(&vault_file).unwrap();
        std::fs::rename(&aside, &vault_file).unwrap();
        assert_eq!(slot.vault.get(&slot.key).as_deref(), Some(stale.as_str()));

        let peer = refresher
            .fresh(&slot, &seen, TimeDelta::minutes(5))
            .await
            .unwrap();
        assert_eq!(
            peer.refresh_token(),
            Some("rt-1"),
            "a caller holding the spent login is handed the one in memory"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        refresher
            .fresh(&slot, &peer, TimeDelta::minutes(5))
            .await
            .unwrap();
        let stored = AuthJson::parse(&slot.vault.get(&slot.key).unwrap()).unwrap();
        assert_eq!(stored.refresh_token(), Some("rt-1"));
        assert!(unstored(&slot).is_none());
    }

    #[tokio::test]
    async fn store_login_refuses_a_sign_in_stored_in_another_slot() {
        let (own, _dir) = slot("users/u1/codex/auth_json");
        let shared = VaultSlot {
            vault: Arc::clone(&own.vault),
            key: "codex/auth_json".into(),
        };
        let known = [shared.clone(), own.clone()];
        let expires = Utc::now() + TimeDelta::hours(1);
        let owners = login("rt-shared", expires);
        shared.vault.set(&shared.key, &owners).unwrap();

        let err = store_login(&own, &owners, &known, VaultActor::Cli)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::Duplicate(kind) if kind.contains("shares")),
            "{err}"
        );
        assert!(own.vault.get(&own.key).is_none());

        let mine = login("rt-mine", expires);
        store_login(&own, &mine, &known, VaultActor::Cli)
            .await
            .unwrap();
        store_login(&own, &mine, &known, VaultActor::Cli)
            .await
            .expect("storing a sign-in again in its own slot is no copy");
    }

    #[test]
    fn errors_never_quote_a_token() {
        let token = jwt(serde_json::json!({"sub": "user-1", "secret": "s3cr3t"}));
        let text = serde_json::json!({
            "OPENAI_API_KEY": "sk-proj-0123456789abcdefghij",
            "tokens": token
        })
        .to_string();
        let err = AuthJson::parse(&text).unwrap_err();
        assert!(err.contains("not a Codex login"), "{err}");
        assert!(!err.contains(&token), "{err}");

        let body = format!(
            "upstream refused {token} {}",
            "because the service is busy ".repeat(20)
        );
        let RefreshError::Transient(reason) = classify(reqwest::StatusCode::BAD_GATEWAY, &body)
        else {
            panic!("a 502 is transient");
        };
        assert!(!reason.contains(&token), "{reason}");
        assert!(reason.contains("upstream refused … because"), "{reason}");
        assert!(reason.chars().count() <= MAX_ERROR_BODY + 20, "{reason}");
    }

    #[tokio::test]
    async fn store_login_waits_for_an_in_flight_refresh() {
        let (slot, _dir) = slot("codex/auth_json");
        let guard = slot.lock().await.unwrap();
        let storing = {
            let slot = slot.clone();
            tokio::spawn(async move { store_login(&slot, "new-login", &[], VaultActor::Cli).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            slot.vault.get(&slot.key).is_none(),
            "held off by the refresh lock"
        );
        drop(guard);
        storing.await.unwrap().unwrap();
        assert_eq!(slot.vault.get(&slot.key).as_deref(), Some("new-login"));
    }
}
