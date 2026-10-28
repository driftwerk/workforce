//! Optional OIDC / OAuth2 login support (gated by the `oidc` cargo feature).
//!
//! Workforce continues to issue its own local JWTs for every authenticated
//! request. The IdP is only consulted at login time via a standard
//! Authorization Code + PKCE flow:
//!
//! 1. `initiate` generates a CSRF `state`, a PKCE verifier, and a nonce,
//!    stores them server-side with a TTL, and returns the IdP's
//!    authorization URL.
//! 2. The user authenticates at the IdP and is redirected back with a
//!    `code` and the same `state`.
//! 3. `complete` pops the stored verifier/nonce, exchanges the code for
//!    tokens at the IdP, validates the `id_token` against the JWKS, and
//!    returns the extracted claims along with the raw id_token (the raw
//!    token is forwarded to the browser so it can be used as
//!    `id_token_hint` for RP-initiated logout later).
//!
//! The `openidconnect` crate handles all the security-critical bits:
//! PKCE S256, JWKS fetching + caching, id_token signature validation,
//! issuer/audience/expiry checks, and clock-skew tolerance.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::{eyre, Context, Result};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};
use tokio::sync::RwLock;
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;
use workforce_common::settings::{OidcConfig, OidcDefaultRole};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus, ProvisioningSource};

/// Key under which the OIDC `sub` is stored in `Identity::linked_ids`.
pub const OIDC_LINKED_ID_KEY: &str = "oidc";

/// Maximum age of an unclaimed CSRF/PKCE state entry before it is pruned.
const STATE_TTL: Duration = Duration::from_secs(10 * 60);

/// How often the background cleanup task prunes expired state entries.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Short-lived per-login state held between `initiate` and `complete`.
struct OidcStateEntry {
    pkce_verifier: PkceCodeVerifier,
    nonce: Nonce,
    expires_at: Instant,
}

/// Subset of id_token claims the hub cares about, extracted after
/// signature + issuer + audience + expiry validation.
#[derive(Debug, Clone)]
pub struct OidcClaims {
    pub sub: String,
    pub email: String,
    pub email_verified: bool,
    pub display_name: String,
}

/// Result of a successful OIDC login: the extracted claims plus the raw
/// id_token string. The raw token is forwarded to the frontend so it can
/// be replayed as `id_token_hint` on RP-initiated logout.
#[derive(Debug, Clone)]
pub struct OidcLoginResult {
    pub claims: OidcClaims,
    pub raw_id_token: String,
}

/// Outcome of mapping OIDC claims to a workforce identity.
#[derive(Debug)]
pub enum IdentityResolution {
    /// Existing identity, possibly updated to add the OIDC linked id.
    Found(Identity),
    /// Newly created identity from the claims + configured default role.
    Created(Identity),
}

impl IdentityResolution {
    pub fn into_identity(self) -> Identity {
        match self {
            Self::Found(i) | Self::Created(i) => i,
        }
    }
}

/// The client `CoreClient::from_provider_metadata` builds: the authorization
/// endpoint is always known, the token and userinfo endpoints only when the
/// discovery document publishes them.
type DiscoveredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// The HTTP client for discovery, JWKS and the token exchange. Redirects
/// are not followed, so an IdP response cannot bounce a request (and the
/// client secret or code with it) somewhere else (SSRF).
fn idp_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building the OIDC HTTP client")
}

/// Runtime OIDC provider — constructed once at startup via `discover`
/// and shared through `GatewayState`.
pub struct OidcProvider {
    config: OidcConfig,
    client: DiscoveredClient,
    http: reqwest::Client,
    /// The IdP's end_session_endpoint, if published in the discovery doc.
    /// openidconnect::core does not expose this field directly, so we
    /// fetch the discovery document ourselves alongside the main discover
    /// call and parse just this one field.
    end_session_endpoint: Option<Url>,
    state_store: Arc<RwLock<HashMap<String, OidcStateEntry>>>,
}

impl OidcProvider {
    /// Discover the IdP metadata and build the OAuth2 client. On success,
    /// also spawns a background task that periodically prunes expired
    /// state entries.
    pub async fn discover(config: OidcConfig) -> Result<Arc<Self>> {
        let issuer = IssuerUrl::new(config.issuer_url.clone())
            .with_context(|| format!("invalid OIDC issuer_url: {}", config.issuer_url))?;

        let http = idp_http_client()?;
        let provider_metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .with_context(|| format!("OIDC discovery failed for {}", config.issuer_url))?;

        let redirect = RedirectUrl::new(config.redirect_uri.clone())
            .with_context(|| format!("invalid OIDC redirect_uri: {}", config.redirect_uri))?;

        let client = CoreClient::from_provider_metadata(
            provider_metadata,
            ClientId::new(config.client_id.clone()),
            Some(ClientSecret::new(config.client_secret.clone())),
        )
        .set_redirect_uri(redirect);

        let end_session_endpoint = fetch_end_session_endpoint(&config.issuer_url).await;

        let provider = Arc::new(Self {
            config,
            client,
            http,
            end_session_endpoint,
            state_store: Arc::new(RwLock::new(HashMap::new())),
        });

        // Spawn a background cleanup task holding only a Weak reference to
        // the state store, so a dropped provider releases its allocation
        // and the task exits on its next tick.
        let weak_store = Arc::downgrade(&provider.state_store);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                let Some(store) = weak_store.upgrade() else {
                    break;
                };
                let now = Instant::now();
                store
                    .write()
                    .await
                    .retain(|_, entry| entry.expires_at > now);
            }
        });

        info!(
            issuer = %provider.config.issuer_url,
            display_name = %provider.config.display_name,
            end_session_endpoint = ?provider.end_session_endpoint.as_ref().map(|u| u.as_str()),
            "OIDC provider initialised"
        );

        Ok(provider)
    }

    pub fn display_name(&self) -> &str {
        &self.config.display_name
    }

    pub fn end_session_endpoint(&self) -> Option<&Url> {
        self.end_session_endpoint.as_ref()
    }

    /// Derive the `post_logout_redirect_uri` from the configured
    /// `redirect_uri` by stripping the path and appending `/`. The
    /// frontend appends `#logged_out=1` client-side.
    pub fn post_logout_redirect_uri(&self) -> String {
        match Url::parse(&self.config.redirect_uri) {
            Ok(mut u) => {
                u.set_path("/");
                u.set_query(None);
                u.set_fragment(None);
                u.to_string()
            }
            Err(_) => "/".to_string(),
        }
    }

    /// Start a login flow — generate CSRF state + PKCE verifier + nonce,
    /// store them with a TTL, and return the IdP authorization URL for
    /// the browser to navigate to.
    pub async fn initiate(&self) -> Url {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf_token, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();

        self.state_store.write().await.insert(
            csrf_token.secret().clone(),
            OidcStateEntry {
                pkce_verifier,
                nonce,
                expires_at: Instant::now() + STATE_TTL,
            },
        );

        auth_url
    }

    /// Complete a login flow — validate state, exchange code, verify
    /// id_token, and return the extracted claims + raw id_token.
    pub async fn complete(&self, code: &str, state: &str) -> Result<OidcLoginResult> {
        let entry = {
            let mut guard = self.state_store.write().await;
            guard
                .remove(state)
                .ok_or_else(|| eyre!("unknown or replayed OIDC state"))?
        };
        if entry.expires_at <= Instant::now() {
            return Err(eyre!("OIDC state expired"));
        }

        let token_response = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .context("IdP publishes no token endpoint")?
            .set_pkce_verifier(entry.pkce_verifier)
            .request_async(&self.http)
            .await
            .context("OIDC token exchange failed")?;

        let id_token = token_response
            .id_token()
            .ok_or_else(|| eyre!("IdP response missing id_token"))?;
        let raw_id_token = id_token.to_string();

        let id_token_verifier = self.client.id_token_verifier();
        let claims = id_token
            .claims(&id_token_verifier, &entry.nonce)
            .context("id_token claims validation failed")?;

        let sub = claims.subject().as_str().to_string();
        let email = claims
            .email()
            .map(|e| e.as_str().to_string())
            .ok_or_else(|| eyre!("id_token missing email claim"))?;
        let email_verified = claims.email_verified().unwrap_or(false);
        let display_name = claims
            .name()
            .and_then(|name| name.get(None).map(|n| n.as_str().to_string()))
            .or_else(|| claims.preferred_username().map(|n| n.as_str().to_string()))
            .unwrap_or_else(|| email.split('@').next().unwrap_or(&email).to_string());

        Ok(OidcLoginResult {
            claims: OidcClaims {
                sub,
                email,
                email_verified,
                display_name,
            },
            raw_id_token,
        })
    }

    pub fn is_email_allowed(&self, email: &str) -> bool {
        is_email_allowed(&self.config, email)
    }

    pub fn resolve_identity(
        &self,
        store: &dyn IdentityStore,
        claims: &OidcClaims,
    ) -> Result<IdentityResolution> {
        resolve_identity(&self.config, store, claims)
    }
}

/// Case-insensitive exact-suffix match against the configured allowlist.
/// An empty allowlist allows all verified emails.
pub fn is_email_allowed(config: &OidcConfig, email: &str) -> bool {
    if config.allowed_email_domains.is_empty() {
        return true;
    }
    let email_lower = email.to_ascii_lowercase();
    let Some(at_idx) = email_lower.rfind('@').map(|i| i + 1) else {
        return false;
    };
    let domain = &email_lower[at_idx..];
    config
        .allowed_email_domains
        .iter()
        .any(|suffix| domain == suffix.to_ascii_lowercase())
}

/// Resolve or provision a workforce identity from validated OIDC claims.
/// Lookup order: `linked_ids["oidc"]` → email → auto-create.
pub fn resolve_identity(
    config: &OidcConfig,
    store: &dyn IdentityStore,
    claims: &OidcClaims,
) -> Result<IdentityResolution> {
    if !claims.email_verified {
        return Err(eyre!("OIDC email not verified"));
    }
    if !is_email_allowed(config, &claims.email) {
        return Err(eyre!("email domain not allowed by OIDC config"));
    }

    if let Some(identity) = store.get_by_linked_id(OIDC_LINKED_ID_KEY, &claims.sub)? {
        ensure_active(&identity)?;
        return Ok(IdentityResolution::Found(identity));
    }

    if let Some(mut identity) = store.get_by_email(&claims.email)? {
        ensure_active(&identity)?;
        if identity
            .linked_ids
            .get(OIDC_LINKED_ID_KEY)
            .is_none_or(|existing| existing != &claims.sub)
        {
            identity
                .linked_ids
                .insert(OIDC_LINKED_ID_KEY.to_string(), claims.sub.clone());
            identity.updated_at = chrono::Utc::now();
            store.update(&identity)?;
        }
        return Ok(IdentityResolution::Found(identity));
    }

    if !config.auto_create_users {
        return Err(eyre!(
            "OIDC user {} not pre-provisioned and auto_create_users is disabled",
            claims.email
        ));
    }

    let role = map_default_role(config.default_role);
    let mut identity = Identity::new(claims.display_name.clone(), role);
    identity.id = Uuid::new_v4();
    identity.email = Some(claims.email.clone());
    identity.status = IdentityStatus::Active;
    identity.provisioning_source = ProvisioningSource::OidcAuto;
    identity
        .linked_ids
        .insert(OIDC_LINKED_ID_KEY.to_string(), claims.sub.clone());

    store.create(&identity)?;
    Ok(IdentityResolution::Created(identity))
}

fn ensure_active(identity: &Identity) -> Result<()> {
    if identity.status == IdentityStatus::Suspended {
        return Err(eyre!("identity {} is suspended", identity.id));
    }
    Ok(())
}

fn map_default_role(role: OidcDefaultRole) -> IdentityRole {
    match role {
        OidcDefaultRole::SystemAdmin => IdentityRole::SystemAdmin,
        OidcDefaultRole::Manager => IdentityRole::Manager,
        OidcDefaultRole::Member => IdentityRole::Member,
    }
}

/// Fetch the discovery document and parse just the `end_session_endpoint`
/// field. openidconnect::core doesn't expose this through its typed API
/// (it's not part of the OIDC Core 1.0 spec, though most IdPs including
/// Keycloak publish it), so we do a small side-channel GET here. Errors
/// are logged and swallowed — logout just falls back to local-only.
async fn fetch_end_session_endpoint(issuer_url: &str) -> Option<Url> {
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    );
    let resp = match reqwest::get(&discovery_url).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, url = %discovery_url, "failed to fetch OIDC discovery for end_session_endpoint");
            return None;
        }
    };
    let json = match resp.json::<serde_json::Value>().await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to parse OIDC discovery JSON");
            return None;
        }
    };
    json.get("end_session_endpoint")
        .and_then(|v| v.as_str())
        .and_then(|s| Url::parse(s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;

    fn test_config(domains: Vec<&str>, auto_create: bool) -> OidcConfig {
        OidcConfig {
            enabled: true,
            display_name: "Test".to_string(),
            issuer_url: "https://example.test".to_string(),
            client_id: "cid".to_string(),
            client_secret: "secret".to_string(),
            redirect_uri: "https://wf.example.test/api/auth/oidc/callback".to_string(),
            allowed_email_domains: domains.into_iter().map(|s| s.to_string()).collect(),
            default_role: OidcDefaultRole::Member,
            auto_create_users: auto_create,
        }
    }

    fn claims(email: &str, verified: bool) -> OidcClaims {
        OidcClaims {
            sub: format!("sub-of-{email}"),
            email: email.to_string(),
            email_verified: verified,
            display_name: email.split('@').next().unwrap_or(email).to_string(),
        }
    }

    #[test]
    fn email_allowlist_empty_allows_all() {
        let cfg = test_config(vec![], true);
        assert!(is_email_allowed(&cfg, "anyone@anywhere.com"));
        assert!(is_email_allowed(&cfg, "kings@harbor.example"));
    }

    #[test]
    fn email_allowlist_exact_suffix_match() {
        let cfg = test_config(vec!["harbor.example"], true);
        assert!(is_email_allowed(&cfg, "kings@harbor.example"));
        assert!(is_email_allowed(&cfg, "KINGS@Harbor.Example"));
        assert!(!is_email_allowed(&cfg, "eve@evil.com"));
    }

    #[test]
    fn email_allowlist_rejects_prefix_and_subdomain_impersonation() {
        let cfg = test_config(vec!["harbor.example"], true);
        // `fooharbor.example` must not match `harbor.example`.
        assert!(!is_email_allowed(&cfg, "kings@fooharbor.example"));
        // Subdomain must not match either — this is exact-suffix, not "*.suffix".
        assert!(!is_email_allowed(&cfg, "kings@sub.harbor.example"));
    }

    #[test]
    fn email_allowlist_rejects_missing_at_sign() {
        let cfg = test_config(vec!["harbor.example"], true);
        assert!(!is_email_allowed(&cfg, "not-an-email"));
    }

    #[test]
    fn email_allowlist_multiple_domains() {
        let cfg = test_config(vec!["harbor.example", "driftwerk.com"], true);
        assert!(is_email_allowed(&cfg, "a@harbor.example"));
        assert!(is_email_allowed(&cfg, "b@driftwerk.com"));
        assert!(!is_email_allowed(&cfg, "c@example.com"));
    }

    #[test]
    fn resolve_identity_auto_creates_new_user() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec!["harbor.example"], true);
        let resolved = resolve_identity(&cfg, &store, &claims("new@harbor.example", true)).unwrap();
        let identity = match resolved {
            IdentityResolution::Created(i) => i,
            IdentityResolution::Found(_) => panic!("expected Created"),
        };
        assert_eq!(identity.email.as_deref(), Some("new@harbor.example"));
        assert_eq!(identity.role, IdentityRole::Member);
        assert_eq!(identity.provisioning_source, ProvisioningSource::OidcAuto);
        assert_eq!(identity.status, IdentityStatus::Active);
        assert_eq!(
            identity.linked_ids.get(OIDC_LINKED_ID_KEY),
            Some(&"sub-of-new@harbor.example".to_string())
        );
    }

    #[test]
    fn resolve_identity_finds_by_sub() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec![], true);

        let mut existing = Identity::new("Old Name", IdentityRole::Manager);
        existing.email = Some("a@example.com".to_string());
        existing.linked_ids.insert(
            OIDC_LINKED_ID_KEY.to_string(),
            "sub-of-a@example.com".to_string(),
        );
        store.create(&existing).unwrap();

        let resolved = resolve_identity(&cfg, &store, &claims("a@example.com", true)).unwrap();
        let found = match resolved {
            IdentityResolution::Found(i) => i,
            IdentityResolution::Created(_) => panic!("expected Found"),
        };
        assert_eq!(found.id, existing.id);
        assert_eq!(found.role, IdentityRole::Manager);
    }

    #[test]
    fn resolve_identity_merges_by_email_and_links_sub() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec![], true);

        let mut existing = Identity::new("Password User", IdentityRole::SystemAdmin);
        existing.email = Some("kings@harbor.example".to_string());
        existing.password_hash = Some("hash".to_string());
        store.create(&existing).unwrap();

        let resolved =
            resolve_identity(&cfg, &store, &claims("kings@harbor.example", true)).unwrap();
        let found = match resolved {
            IdentityResolution::Found(i) => i,
            IdentityResolution::Created(_) => panic!("expected Found"),
        };
        assert_eq!(found.id, existing.id);
        assert_eq!(found.role, IdentityRole::SystemAdmin);
        let expected_sub = "sub-of-kings@harbor.example".to_string();
        assert_eq!(
            found.linked_ids.get(OIDC_LINKED_ID_KEY),
            Some(&expected_sub)
        );

        // Re-read from the store to confirm the link was persisted.
        let reloaded = store.get_by_id(existing.id).unwrap().unwrap();
        assert_eq!(
            reloaded.linked_ids.get(OIDC_LINKED_ID_KEY),
            Some(&expected_sub)
        );
    }

    #[test]
    fn resolve_identity_rejects_unverified_email() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec![], true);
        assert!(resolve_identity(&cfg, &store, &claims("x@y.com", false)).is_err());
    }

    #[test]
    fn resolve_identity_rejects_suspended_user() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec![], true);

        let mut existing = Identity::new("Suspended", IdentityRole::Member);
        existing.email = Some("x@y.com".to_string());
        existing.status = IdentityStatus::Suspended;
        store.create(&existing).unwrap();

        assert!(resolve_identity(&cfg, &store, &claims("x@y.com", true)).is_err());
    }

    #[test]
    fn resolve_identity_rejects_unprovisioned_when_auto_create_disabled() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec![], false);
        assert!(resolve_identity(&cfg, &store, &claims("stranger@x.com", true)).is_err());
    }

    #[test]
    fn resolve_identity_rejects_disallowed_domain() {
        let store = SqliteIdentityStore::in_memory().unwrap();
        let cfg = test_config(vec!["harbor.example"], true);
        assert!(resolve_identity(&cfg, &store, &claims("eve@evil.com", true)).is_err());
    }

    #[test]
    fn map_default_role_covers_all_variants() {
        assert_eq!(
            map_default_role(OidcDefaultRole::SystemAdmin),
            IdentityRole::SystemAdmin
        );
        assert_eq!(
            map_default_role(OidcDefaultRole::Manager),
            IdentityRole::Manager
        );
        assert_eq!(
            map_default_role(OidcDefaultRole::Member),
            IdentityRole::Member
        );
    }

    #[test]
    fn provisioning_source_oidc_auto_round_trips() {
        let s = serde_json::to_string(&ProvisioningSource::OidcAuto).unwrap();
        assert_eq!(s, "\"oidc_auto\"");
        let parsed: ProvisioningSource = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, ProvisioningSource::OidcAuto);
    }
}
