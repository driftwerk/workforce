use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use eyre::Result;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::IdentityRole;
use workforce_pairing::DevicePairingService;

use crate::auth::constant_time_token_eq;

// ── Scope constants ──────────────────────────────────────────────────

pub const SCOPE_CHAT: &str = "chat";
pub const SCOPE_SESSIONS: &str = "sessions";
pub const SCOPE_AGENTS: &str = "agents";
pub const SCOPE_PAIR: &str = "pair";
pub const SCOPE_ADMIN: &str = "admin";
pub const SCOPE_HEARTBEAT: &str = "heartbeat";
pub const SCOPE_MODELS: &str = "models";
pub const SCOPE_WILDCARD: &str = "*";

/// True if `scopes` grants `required`. Empty `scopes` denies — every token
/// must declare its surface explicitly (use `["*"]` for full access). The
/// V002 pairing migration backfills legacy `[]` rows to `["*"]` so existing
/// CLI / iOS tokens are not bricked by this rule.
pub fn has_scope(scopes: &[String], required: &str) -> bool {
    scopes.iter().any(|s| s == SCOPE_WILDCARD || s == required)
}

/// Set this env var (to any value) to restore the legacy behaviour where every
/// minted token carries `["*"]` regardless of role. The rollback hatch for this
/// slice: a deploy that locks members or the operator out of chat / sessions is
/// reverted by setting this var and restarting the hub — no code change.
const LEGACY_WILDCARD_ENV: &str = "WORKFORCE_LEGACY_WILDCARD_SCOPES";

/// The scopes a freshly minted token should carry for `role`.
///
/// `SystemAdmin` stays broad (`["*"]`) so admin tooling is never accidentally
/// narrowed; `Manager` and `Member` get exactly the member-facing scope surface
/// and **never** `SCOPE_ADMIN`, so a member's token can no longer satisfy the
/// `require_scope(SCOPE_ADMIN)` gate on admin routes.
pub fn scopes_for_role(role: IdentityRole) -> Vec<String> {
    match role {
        IdentityRole::SystemAdmin => vec![SCOPE_WILDCARD.to_string()],
        IdentityRole::Manager | IdentityRole::Member => vec![
            SCOPE_CHAT.to_string(),
            SCOPE_SESSIONS.to_string(),
            SCOPE_AGENTS.to_string(),
            SCOPE_PAIR.to_string(),
            SCOPE_HEARTBEAT.to_string(),
            SCOPE_MODELS.to_string(),
        ],
    }
}

/// Role-appropriate token scopes, honouring the `WORKFORCE_LEGACY_WILDCARD_SCOPES`
/// rollback hatch. The mint sites call this; tests target the pure
/// [`resolve_token_scopes`] to stay free of process-env races.
fn token_scopes(role: IdentityRole) -> Vec<String> {
    resolve_token_scopes(role, std::env::var_os(LEGACY_WILDCARD_ENV).is_some())
}

/// The scope surface a paired device (iOS / mobile) is granted, regardless of
/// the owner identity's role. A device is the member-facing surface and never
/// admin: it performs chat / sessions / agents / pairing / heartbeat / models,
/// never admin operations, and is a higher-theft-risk credential than a
/// password-backed login — so even an admin's device stays bounded. This is
/// the single source of truth for device-token scopes (mirrors the JWT mint
/// path) and honours the same `WORKFORCE_LEGACY_WILDCARD_SCOPES` rollback hatch.
pub fn device_scopes() -> Vec<String> {
    token_scopes(IdentityRole::Member)
}

/// Pure resolver: when `legacy_wildcard` is set, fall back to `["*"]` for every
/// role; otherwise defer to [`scopes_for_role`].
fn resolve_token_scopes(role: IdentityRole, legacy_wildcard: bool) -> Vec<String> {
    if legacy_wildcard {
        vec![SCOPE_WILDCARD.to_string()]
    } else {
        scopes_for_role(role)
    }
}

#[derive(Debug, Clone)]
pub struct JwtConfig {
    pub secret: String,
    pub expiry_secs: u64,
}

impl JwtConfig {
    /// Minimum acceptable JWT secret length. HS256 is symmetric, so a
    /// captured token plus a short secret is brute-forceable offline.
    /// 32 bytes ≈ 256 bits of entropy if the source is CSPRNG-derived;
    /// it's the same shape as a SHA-256 hash and matches what every
    /// reasonable JWT library recommends.
    pub const MIN_SECRET_BYTES: usize = 32;

    /// Validating constructor — fails fast at startup if the secret is
    /// too short. Production callers must use this; tests that need a
    /// short test fixture can still build the struct literal directly.
    /// Closes audit finding F6 (2026-05-15).
    pub fn new(secret: String, expiry_secs: u64) -> Result<Self> {
        if secret.len() < Self::MIN_SECRET_BYTES {
            return Err(eyre::eyre!(
                "JWT secret must be at least {} bytes (got {}). \
                 HS256 makes the secret the load-bearing knob; a short \
                 secret can be brute-forced offline against any captured \
                 token. Generate with: openssl rand -hex 32",
                Self::MIN_SECRET_BYTES,
                secret.len()
            ));
        }
        Ok(Self {
            secret,
            expiry_secs,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: IdentityRole,
    #[serde(default)]
    pub identity_id: Option<String>,
    pub iat: u64,
    pub exp: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub device_id: Option<String>,
}

fn now_secs() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| eyre::eyre!("System time error: {}", e))
}

pub fn generate_token(config: &JwtConfig, sub: &str, role: IdentityRole) -> Result<String> {
    generate_token_with_identity(config, sub, role, None)
}

pub fn generate_token_with_identity(
    config: &JwtConfig,
    sub: &str,
    role: IdentityRole,
    identity_id: Option<String>,
) -> Result<String> {
    let now = now_secs()?;
    let claims = Claims {
        sub: sub.to_string(),
        role,
        identity_id,
        iat: now,
        exp: now + config.expiry_secs,
        scopes: token_scopes(role),
        device_id: None,
    };
    encode_claims(config, &claims)
}

fn encode_claims(config: &JwtConfig, claims: &Claims) -> Result<String> {
    let header = base64_url_encode(r#"{"alg":"HS256","typ":"JWT"}"#.as_bytes());
    let payload = base64_url_encode(&serde_json::to_vec(claims)?);
    let signing_input = format!("{}.{}", header, payload);
    let signature = hmac_sha256(config.secret.as_bytes(), signing_input.as_bytes());
    let sig_b64 = base64_url_encode(&signature);
    Ok(format!("{}.{}", signing_input, sig_b64))
}

pub fn validate_token(config: &JwtConfig, token: &str) -> Result<Claims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(eyre::eyre!("Invalid JWT format"));
    }

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected_sig = hmac_sha256(config.secret.as_bytes(), signing_input.as_bytes());

    let provided_sig = base64_url_decode(parts[2])?;
    let sig_match: bool = expected_sig.ct_eq(&provided_sig).into();
    if !sig_match {
        return Err(eyre::eyre!("Invalid JWT signature"));
    }

    let payload_bytes = base64_url_decode(parts[1])?;
    let claims: Claims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| eyre::eyre!("Invalid JWT payload: {}", e))?;

    if now_secs()? >= claims.exp {
        return Err(eyre::eyre!("JWT expired"));
    }

    Ok(claims)
}

pub fn refresh_token(config: &JwtConfig, claims: &Claims) -> Result<String> {
    let now = now_secs()?;
    let refreshed = Claims {
        iat: now,
        exp: now + config.expiry_secs,
        ..claims.clone()
    };
    encode_claims(config, &refreshed)
}

/// `claims` carrying `role`, the person's role now: a token never keeps a
/// role they have lost or misses one they were given. Scopes that were the
/// old role's follow it; scopes granted any other way are kept as far as the
/// new role grants them.
fn with_current_role(claims: Claims, role: IdentityRole) -> Claims {
    if claims.role == role {
        return claims;
    }
    let granted = token_scopes(role);
    let scopes = match claims.scopes == token_scopes(claims.role) {
        true => granted,
        false => claims
            .scopes
            .into_iter()
            .filter(|scope| has_scope(&granted, scope))
            .collect(),
    };
    Claims {
        role,
        scopes,
        ..claims
    }
}

/// `claims` carrying the role their person holds now, so a token minted
/// before a demotion grants nothing the person has lost; a suspended person
/// is the member they have become, as the capability matrix scores them. A
/// token naming no identity the store knows is left as it is, and so is
/// every token while the store cannot be read: a lookup outage must not
/// strip every administrator of the means to fix it.
pub(crate) fn current_claims(store: Option<&Arc<dyn IdentityStore>>, claims: Claims) -> Claims {
    let Some((id, store)) = claims
        .identity_id
        .as_deref()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .zip(store)
    else {
        return claims;
    };
    match store.get_by_id(id) {
        Ok(Some(identity)) => {
            let role = match identity.status {
                workforce_identity::types::IdentityStatus::Suspended => IdentityRole::Member,
                _ => identity.role,
            };
            with_current_role(claims, role)
        }
        Ok(None) => claims,
        Err(e) => {
            tracing::warn!(%id, error = %e, "Failed to read a token holder's current role; using the token's");
            claims
        }
    }
}

pub struct JwtAuthState {
    pub config: JwtConfig,
    pub static_token: Option<String>,
    pub device_pairing: Option<Arc<DevicePairingService>>,
    pub identity_store: Option<Arc<dyn IdentityStore>>,
}

pub async fn jwt_auth_middleware(
    State(state): State<Arc<JwtAuthState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => return Err(StatusCode::UNAUTHORIZED),
    };

    if let Some(ref static_tok) = state.static_token {
        if constant_time_token_eq(token, static_tok) {
            let claims = Claims {
                sub: "admin".to_string(),
                role: IdentityRole::SystemAdmin,
                identity_id: None,
                iat: 0,
                exp: u64::MAX,
                scopes: token_scopes(IdentityRole::SystemAdmin),
                device_id: None,
            };
            request.extensions_mut().insert(claims);
            return Ok(next.run(request).await);
        }
    }

    if let Ok(claims) = validate_token(&state.config, token) {
        request
            .extensions_mut()
            .insert(current_claims(state.identity_store.as_ref(), claims));
        return Ok(next.run(request).await);
    }

    if let Some(ref pairing) = state.device_pairing {
        match pairing.validate_token_async(token.to_string()).await {
            Ok(Some(device)) => {
                let now = now_secs().unwrap_or(0);
                let claims = Claims {
                    sub: device.device_name,
                    role: IdentityRole::Member,
                    identity_id: None,
                    iat: now,
                    exp: now + 3600,
                    scopes: device.scopes,
                    device_id: Some(device.id),
                };
                request.extensions_mut().insert(claims);
                return Ok(next.run(request).await);
            }
            Ok(None) => {}
            Err(e) => {
                // Store failure ≠ bad token: 503 tells the client to retry,
                // where a 401 would send it down token-recovery paths.
                tracing::warn!(
                    error = %e,
                    "Pairing store error during bearer auth — failing closed"
                );
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }

    Err(StatusCode::UNAUTHORIZED)
}

// ── Auth endpoints ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub identity_id: String,
    pub display_name: String,
    pub role: IdentityRole,
}

#[derive(Serialize)]
pub struct RefreshResponse {
    pub token: String,
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

fn json_error(status: StatusCode, msg: &str) -> (StatusCode, axum::Json<ErrorBody>) {
    (
        status,
        axum::Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
}

/// Dummy hash constant used to prevent timing side-channels when an
/// email is not found — we still run Argon2 verification so the
/// response time is indistinguishable from a wrong-password attempt.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$dummyhashvalue";

pub async fn login_handler(
    State(state): State<Arc<JwtAuthState>>,
    axum::Json(req): axum::Json<LoginRequest>,
) -> Result<axum::Json<LoginResponse>, (StatusCode, axum::Json<ErrorBody>)> {
    let store = state.identity_store.as_ref().ok_or_else(|| {
        json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Identity service not configured",
        )
    })?;

    let identity = store
        .get_by_email(&req.email)
        .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "Database error"))?;

    let (identity, hash) = match identity {
        Some(id) => {
            let h = id.password_hash.clone().unwrap_or(DUMMY_HASH.to_string());
            (Some(id), h)
        }
        None => (None, DUMMY_HASH.to_string()),
    };

    let valid =
        workforce_identity::password::verify_password(&req.password, &hash).unwrap_or(false);

    let identity = identity
        .filter(|id| valid && id.password_hash.is_some())
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "Invalid email or password"))?;

    if identity.status == workforce_identity::types::IdentityStatus::Suspended {
        return Err(json_error(
            StatusCode::UNAUTHORIZED,
            "Invalid email or password",
        ));
    }

    let token = generate_token_with_identity(
        &state.config,
        &identity.display_name,
        identity.role,
        Some(identity.id.to_string()),
    )
    .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "Token generation failed"))?;

    Ok(axum::Json(LoginResponse {
        token,
        identity_id: identity.id.to_string(),
        display_name: identity.display_name,
        role: identity.role,
    }))
}

pub async fn refresh_handler(
    State(state): State<Arc<JwtAuthState>>,
    request: Request,
) -> Result<axum::Json<RefreshResponse>, (StatusCode, axum::Json<ErrorBody>)> {
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "Missing bearer token"))?;

    // The static admin token has no expiry and cannot be "refreshed" — echo
    // it back so the frontend's on-mount refresh does not get 401'd into a
    // logout when the user is authenticated via the bootstrap token.
    if let Some(ref static_tok) = state.static_token {
        if constant_time_token_eq(token, static_tok) {
            return Ok(axum::Json(RefreshResponse {
                token: token.to_string(),
            }));
        }
    }

    let mut claims = validate_token(&state.config, token)
        .map_err(|_| json_error(StatusCode::UNAUTHORIZED, "Invalid or expired token"))?;

    if let (Some(ref store), Some(ref identity_id)) = (&state.identity_store, &claims.identity_id) {
        if let Ok(id) = uuid::Uuid::parse_str(identity_id) {
            let identity = store
                .get_by_id(id)
                .ok()
                .flatten()
                .filter(|i| i.status != workforce_identity::types::IdentityStatus::Suspended)
                .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "Session invalidated"))?;
            claims = with_current_role(claims, identity.role);
        }
    }

    let token = refresh_token(&state.config, &claims)
        .map_err(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "Token refresh failed"))?;

    Ok(axum::Json(RefreshResponse { token }))
}

// ── Bootstrap ───────────────────────────────────────────────────────

const MIN_BOOTSTRAP_PASSWORD_LEN: usize = 8;

pub fn bootstrap_admin(store: &dyn IdentityStore) -> Result<()> {
    let email = std::env::var("ADMIN_EMAIL").ok();
    let password = std::env::var("ADMIN_PASSWORD").ok();
    bootstrap_admin_with_credentials(store, email.as_deref(), password.as_deref())
}

pub fn bootstrap_admin_with_credentials(
    store: &dyn IdentityStore,
    email: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    if store.count()? > 0 {
        return Ok(());
    }

    let (email, password) = match (email, password) {
        (Some(e), Some(p)) if !e.is_empty() && p.len() >= MIN_BOOTSTRAP_PASSWORD_LEN => (e, p),
        (Some(_), Some(p)) if !p.is_empty() => {
            return Err(eyre::eyre!(
                "ADMIN_PASSWORD must be at least {} characters",
                MIN_BOOTSTRAP_PASSWORD_LEN
            ));
        }
        _ => return Ok(()),
    };

    let display_name = email.split('@').next().unwrap_or(email).to_string();

    let hash = workforce_identity::password::hash_password(password)?;
    let identity =
        workforce_identity::types::Identity::new(display_name, IdentityRole::SystemAdmin)
            .with_email(email)
            .with_password_hash(hash);

    store.create(&identity)?;
    tracing::info!(email = %email, id = %identity.id, "Bootstrap admin created");
    Ok(())
}

// ── Crypto helpers ──────────────────────────────────────────────────

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    let result = mac.finalize();
    result.into_bytes().into()
}

fn base64_url_encode(data: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(data)
}

fn base64_url_decode(data: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD
        .decode(data)
        .map_err(|e| eyre::eyre!("Base64 decode error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::types::Identity;

    fn test_config() -> JwtConfig {
        JwtConfig {
            secret: "test-secret-key-at-least-32-chars".to_string(),
            expiry_secs: 3600,
        }
    }

    #[test]
    fn generate_and_validate() {
        let config = test_config();
        let token = generate_token(&config, "kings", IdentityRole::SystemAdmin).unwrap();
        let claims = validate_token(&config, &token).unwrap();
        assert_eq!(claims.sub, "kings");
        assert_eq!(claims.role, IdentityRole::SystemAdmin);
    }

    // ── F6: secret strength validation ────────────────────────────────

    #[test]
    fn new_accepts_secret_at_min_bytes() {
        let secret = "a".repeat(JwtConfig::MIN_SECRET_BYTES);
        assert!(JwtConfig::new(secret, 3600).is_ok());
    }

    #[test]
    fn new_rejects_secret_below_min_bytes() {
        let too_short = "a".repeat(JwtConfig::MIN_SECRET_BYTES - 1);
        let err = JwtConfig::new(too_short, 3600).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("JWT secret must be at least"));
    }

    #[test]
    fn new_rejects_empty_secret() {
        assert!(JwtConfig::new(String::new(), 3600).is_err());
    }

    #[test]
    fn generate_with_identity_id() {
        let config = test_config();
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let token = generate_token_with_identity(
            &config,
            "kings",
            IdentityRole::SystemAdmin,
            Some(id.to_string()),
        )
        .unwrap();
        let claims = validate_token(&config, &token).unwrap();
        assert_eq!(claims.identity_id.as_deref(), Some(id));
    }

    #[test]
    fn reject_wrong_secret() {
        let config = test_config();
        let token = generate_token(&config, "kings", IdentityRole::SystemAdmin).unwrap();
        let bad = JwtConfig {
            secret: "different-secret-key-at-least-32c".to_string(),
            expiry_secs: 3600,
        };
        assert!(validate_token(&bad, &token).is_err());
    }

    #[test]
    fn reject_expired() {
        let config = JwtConfig {
            secret: "test-secret-key-at-least-32-chars".to_string(),
            expiry_secs: 0,
        };
        let token = generate_token(&config, "kings", IdentityRole::Member).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(validate_token(&config, &token).is_err());
    }

    #[test]
    fn reject_malformed_token() {
        let config = test_config();
        assert!(validate_token(&config, "not.a.valid.token").is_err());
        assert!(validate_token(&config, "only-one-part").is_err());
        assert!(validate_token(&config, "two.parts").is_err());
        assert!(validate_token(&config, "a.b.tampered").is_err());
    }

    #[test]
    fn role_permissions() {
        assert!(IdentityRole::SystemAdmin.can_manage_system());
        assert!(IdentityRole::SystemAdmin.can_manage_teams());
        assert!(IdentityRole::Manager.can_manage_teams());
        assert!(!IdentityRole::Manager.can_manage_system());
        assert!(!IdentityRole::Member.can_manage_teams());
        assert!(!IdentityRole::Member.can_manage_system());
    }

    #[test]
    fn scope_enforcement() {
        assert!(!has_scope(&[], "chat"));
        assert!(!has_scope(&[], "*"));
        assert!(has_scope(&["*".into()], "chat"));
        assert!(has_scope(&["*".into()], "admin"));
        assert!(has_scope(&["chat".into(), "sessions".into()], "chat"));
        assert!(!has_scope(&["chat".into()], "admin"));
        assert!(!has_scope(&["sessions".into()], "chat"));
    }

    // ── Role-appropriate token scopes (member-lockout safety net) ─────

    /// The exact set every non-admin token must carry — pinned here so a
    /// regression that drops a member-facing scope (locking members out of
    /// chat / sessions / agents / pairing / heartbeat / models) fails loudly.
    const MEMBER_SCOPES: [&str; 6] = [
        SCOPE_CHAT,
        SCOPE_SESSIONS,
        SCOPE_AGENTS,
        SCOPE_PAIR,
        SCOPE_HEARTBEAT,
        SCOPE_MODELS,
    ];

    #[test]
    fn scopes_for_member_is_the_six_member_scopes() {
        let scopes = scopes_for_role(IdentityRole::Member);
        assert_eq!(scopes, MEMBER_SCOPES.map(String::from).to_vec());
        assert!(!scopes.iter().any(|s| s == SCOPE_ADMIN));
        assert!(!scopes.iter().any(|s| s == SCOPE_WILDCARD));
    }

    #[test]
    fn scopes_for_manager_matches_member() {
        // Managers are not admins: same member-facing surface, no admin scope.
        let scopes = scopes_for_role(IdentityRole::Manager);
        assert_eq!(scopes, MEMBER_SCOPES.map(String::from).to_vec());
        assert!(!scopes.iter().any(|s| s == SCOPE_ADMIN));
        assert!(!scopes.iter().any(|s| s == SCOPE_WILDCARD));
    }

    #[test]
    fn scopes_for_system_admin_is_wildcard() {
        assert_eq!(
            scopes_for_role(IdentityRole::SystemAdmin),
            vec![SCOPE_WILDCARD.to_string()]
        );
    }

    #[test]
    fn device_scopes_are_the_member_surface_and_never_admin() {
        let scopes = device_scopes();
        assert_eq!(scopes, MEMBER_SCOPES.map(String::from).to_vec());
        // The sharp regression: a device token must fail the admin scope gate,
        // so it can never satisfy a scope-only `require_scope(SCOPE_ADMIN)`
        // route even if it was historically backfilled to `["*"]`.
        assert!(!has_scope(&scopes, SCOPE_ADMIN));
        assert!(!scopes.iter().any(|s| s == SCOPE_WILDCARD));
    }

    #[test]
    fn member_token_grants_every_member_scope_but_not_admin() {
        let config = test_config();
        let token = generate_token_with_identity(
            &config,
            "member",
            IdentityRole::Member,
            Some("id-member".to_string()),
        )
        .unwrap();
        let claims = validate_token(&config, &token).unwrap();

        // Member happy-path: not locked out of any member-facing surface.
        assert!(has_scope(&claims.scopes, SCOPE_CHAT));
        assert!(has_scope(&claims.scopes, SCOPE_SESSIONS));
        assert!(has_scope(&claims.scopes, SCOPE_AGENTS));
        assert!(has_scope(&claims.scopes, SCOPE_PAIR));
        assert!(has_scope(&claims.scopes, SCOPE_HEARTBEAT));
        assert!(has_scope(&claims.scopes, SCOPE_MODELS));

        // …but the toothless admin gate no longer passes.
        assert!(!has_scope(&claims.scopes, SCOPE_ADMIN));
    }

    #[test]
    fn system_admin_token_grants_admin_scope() {
        let config = test_config();
        let token = generate_token(&config, "admin", IdentityRole::SystemAdmin).unwrap();
        let claims = validate_token(&config, &token).unwrap();
        assert!(has_scope(&claims.scopes, SCOPE_ADMIN));
    }

    #[test]
    fn legacy_wildcard_rollback_restores_wildcard_for_member() {
        // Test the pure resolver directly: no process-env mutation, so this is
        // immune to parallel-test env races.
        assert_eq!(
            resolve_token_scopes(IdentityRole::Member, true),
            vec![SCOPE_WILDCARD.to_string()]
        );
        // Sanity: with the hatch off, the member set is unchanged.
        assert_eq!(
            resolve_token_scopes(IdentityRole::Member, false),
            scopes_for_role(IdentityRole::Member)
        );
    }

    #[test]
    fn refresh_preserves_all_fields() {
        let config = test_config();
        let token = generate_token_with_identity(
            &config,
            "kings",
            IdentityRole::Manager,
            Some("id-123".to_string()),
        )
        .unwrap();
        let claims = validate_token(&config, &token).unwrap();
        assert_eq!(claims.scopes, scopes_for_role(IdentityRole::Manager));

        let refreshed_token = refresh_token(&config, &claims).unwrap();
        let refreshed = validate_token(&config, &refreshed_token).unwrap();

        assert_eq!(refreshed.sub, "kings");
        assert_eq!(refreshed.role, IdentityRole::Manager);
        assert_eq!(refreshed.identity_id.as_deref(), Some("id-123"));
        assert!(refreshed.exp >= claims.exp);
        assert_eq!(refreshed.scopes, claims.scopes);
    }

    #[test]
    fn bootstrap_creates_admin() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        bootstrap_admin_with_credentials(&store, Some("admin@test.com"), Some("secretpassword123"))
            .unwrap();

        assert_eq!(store.count().unwrap(), 1);
        let admin = store.get_by_email("admin@test.com").unwrap().unwrap();
        assert_eq!(admin.role, IdentityRole::SystemAdmin);
        assert_eq!(admin.display_name, "admin");
        assert!(admin.password_hash.is_some());
    }

    #[test]
    fn bootstrap_skips_when_identities_exist() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        store
            .create(&Identity::new("Existing", IdentityRole::Member))
            .unwrap();

        bootstrap_admin_with_credentials(&store, Some("admin2@test.com"), Some("secret456789"))
            .unwrap();

        assert_eq!(store.count().unwrap(), 1);
        assert!(store.get_by_email("admin2@test.com").unwrap().is_none());
    }

    #[test]
    fn bootstrap_skips_when_credentials_missing() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        bootstrap_admin_with_credentials(&store, None, None).unwrap();
        assert_eq!(store.count().unwrap(), 0);

        bootstrap_admin_with_credentials(&store, Some("admin@test.com"), None).unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn bootstrap_rejects_short_password() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let result =
            bootstrap_admin_with_credentials(&store, Some("admin@test.com"), Some("short"));
        assert!(result.is_err());
        assert_eq!(store.count().unwrap(), 0);
    }

    // ── refresh_handler tests ────────────────────────────────────────

    fn refresh_state(static_token: Option<&str>) -> Arc<JwtAuthState> {
        Arc::new(JwtAuthState {
            config: test_config(),
            static_token: static_token.map(|s| s.to_string()),
            device_pairing: None,
            identity_store: None,
        })
    }

    fn refresh_request(auth_header: Option<&str>) -> Request {
        let mut builder = Request::builder().uri("/api/auth/refresh").method("POST");
        if let Some(h) = auth_header {
            builder = builder.header(axum::http::header::AUTHORIZATION, h);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn refresh_rejects_missing_authorization_header() {
        let state = refresh_state(None);
        let err = refresh_handler(State(state), refresh_request(None))
            .await
            .err()
            .unwrap_or_else(|| panic!("missing header should 401"));
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_rejects_non_bearer_authorization_header() {
        let state = refresh_state(None);
        let err = refresh_handler(State(state), refresh_request(Some("Basic abc123")))
            .await
            .err()
            .unwrap_or_else(|| panic!("non-bearer header should 401"));
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_rejects_invalid_jwt_signature() {
        let state = refresh_state(None);
        let err = refresh_handler(State(state), refresh_request(Some("Bearer not.a.validjwt")))
            .await
            .err()
            .unwrap_or_else(|| panic!("invalid jwt should 401"));
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_rejects_expired_jwt() {
        let expiring_config = JwtConfig {
            secret: "test-secret-key-at-least-32-chars".to_string(),
            expiry_secs: 0,
        };
        let token = generate_token(&expiring_config, "kings", IdentityRole::Member).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));

        let state = Arc::new(JwtAuthState {
            config: expiring_config,
            static_token: None,
            device_pairing: None,
            identity_store: None,
        });
        let err = refresh_handler(
            State(state),
            refresh_request(Some(&format!("Bearer {token}"))),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("expired jwt should 401"));
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refresh_issues_new_token_for_valid_jwt() {
        let state = refresh_state(None);
        let token = generate_token_with_identity(
            &state.config,
            "kings",
            IdentityRole::Manager,
            Some("id-abc".to_string()),
        )
        .unwrap();

        let resp = refresh_handler(
            State(Arc::clone(&state)),
            refresh_request(Some(&format!("Bearer {token}"))),
        )
        .await
        .ok()
        .unwrap_or_else(|| panic!("valid jwt should refresh"));

        let new_token = &resp.0.token;
        assert!(!new_token.is_empty());
        let claims = validate_token(&state.config, new_token).unwrap();
        assert_eq!(claims.sub, "kings");
        assert_eq!(claims.role, IdentityRole::Manager);
        assert_eq!(claims.identity_id.as_deref(), Some("id-abc"));
    }

    fn state_knowing(identity: &Identity) -> Arc<JwtAuthState> {
        let store = SqliteIdentityStore::in_memory().unwrap();
        store.create(identity).unwrap();
        Arc::new(JwtAuthState {
            config: test_config(),
            static_token: None,
            device_pairing: None,
            identity_store: Some(Arc::new(store)),
        })
    }

    fn token_for(state: &JwtAuthState, identity: &Identity, role: IdentityRole) -> String {
        generate_token_with_identity(
            &state.config,
            &identity.display_name,
            role,
            Some(identity.id.to_string()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_refreshed_token_carries_the_role_the_person_has_now() {
        let demoted = Identity::new("Ada", IdentityRole::Member);
        let state = state_knowing(&demoted);
        let minted_as_admin = token_for(&state, &demoted, IdentityRole::SystemAdmin);

        let resp = refresh_handler(
            State(Arc::clone(&state)),
            refresh_request(Some(&format!("Bearer {minted_as_admin}"))),
        )
        .await
        .ok()
        .unwrap_or_else(|| panic!("an active person's token refreshes"));
        let claims = validate_token(&state.config, &resp.0.token).unwrap();
        assert_eq!(claims.role, IdentityRole::Member);
        assert_eq!(claims.scopes, token_scopes(IdentityRole::Member));
    }

    #[test]
    fn scopes_granted_apart_from_the_role_survive_a_role_change() {
        let claims = Claims {
            sub: "ada".into(),
            role: IdentityRole::SystemAdmin,
            identity_id: None,
            iat: 0,
            exp: 1,
            scopes: vec!["chat".into()],
            device_id: None,
        };
        let changed = with_current_role(claims, IdentityRole::Member);
        assert_eq!(changed.role, IdentityRole::Member);
        assert_eq!(changed.scopes, ["chat"]);
    }

    #[tokio::test]
    async fn refresh_echoes_static_admin_token_unchanged() {
        let state = refresh_state(Some("admin-static-token"));
        let resp = refresh_handler(
            State(Arc::clone(&state)),
            refresh_request(Some("Bearer admin-static-token")),
        )
        .await
        .ok()
        .unwrap_or_else(|| panic!("static token should refresh"));
        assert_eq!(resp.0.token, "admin-static-token");
    }
}
