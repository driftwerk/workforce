//! OIDC verification for HTTP triggers: verify a JWT a machine caller presents
//! against the issuer's JWKS, authorize it by a configurable principal claim,
//! and (optionally) resolve the caller to a workforce identity to run as.
//!
//! This is deliberately separate from the interactive login flow in
//! [`crate::oidc`], which is coupled to the authorization-code exchange and a
//! server-issued nonce that a presented machine token does not carry.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use tokio::sync::RwLock;
use tracing::warn;

use workforce_identity::store::IdentityStore;
use workforce_identity::types::IdentityStatus;
use workforce_orchestrator::workflow::definition::OidcTriggerAuth;

/// `Identity.linked_ids` key under which an OIDC `sub` is stored.
const OIDC_LINKED_ID_KEY: &str = "oidc";
/// How long a fetched JWKS is trusted before a refresh is required.
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);
/// Minimum gap between JWKS fetch attempts, so a flood of unknown `kid`s cannot
/// amplify into a request storm against the issuer's JWKS endpoint.
const JWKS_REFRESH_THROTTLE: Duration = Duration::from_secs(30);
/// Asymmetric algorithms accepted for trigger tokens. HMAC algorithms are
/// excluded to prevent an `alg=HS256` confusion attack against an RSA key.
const ACCEPTED_ALGS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
];

/// Cached decoding keys for one issuer with fetch timestamps.
struct IssuerKeys {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
    last_attempt: Instant,
}

/// Multi-issuer JWKS verifier with a TTL cache and refresh throttling.
pub struct OidcVerifier {
    http: reqwest::Client,
    cache: RwLock<HashMap<String, IssuerKeys>>,
}

impl Default for OidcVerifier {
    fn default() -> Self {
        Self::new()
    }
}

/// A caller whose token verified and whose principal is allowlisted.
pub struct VerifiedCaller {
    pub principal: String,
    pub email: Option<String>,
    pub email_verified: bool,
}

impl OidcVerifier {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Verify a presented JWT against `auth` and authorize its principal.
    /// Returns the verified caller, or `None` on any failure.
    pub async fn verify(&self, token: &str, auth: &OidcTriggerAuth) -> Option<VerifiedCaller> {
        let kid = decode_header(token).ok()?.kid?;
        let key = self.decoding_key(&auth.issuer, &kid).await?;
        let claims = verify_token_claims(token, &key, &auth.issuer, &auth.audience).ok()?;
        authorize_claims(&claims, auth)
    }

    /// The decoding key for `(issuer, kid)`, refreshing the JWKS when the cache
    /// misses or is stale. Refresh-on-unknown-kid is throttled.
    async fn decoding_key(&self, issuer: &str, kid: &str) -> Option<DecodingKey> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(issuer) {
                if entry.fetched_at.elapsed() < JWKS_TTL {
                    if let Some(key) = entry.keys.get(kid) {
                        return Some(key.clone());
                    }
                    if entry.last_attempt.elapsed() < JWKS_REFRESH_THROTTLE {
                        return None;
                    }
                }
            }
        }
        self.refresh(issuer).await;
        self.cache.read().await.get(issuer)?.keys.get(kid).cloned()
    }

    /// Fetch the issuer's JWKS and replace its cache entry. On failure, records
    /// the attempt time (throttle) and leaves any existing keys in place.
    async fn refresh(&self, issuer: &str) {
        let now = Instant::now();
        match self.fetch_keys(issuer).await {
            Ok(keys) => {
                self.cache.write().await.insert(
                    issuer.to_string(),
                    IssuerKeys {
                        keys,
                        fetched_at: now,
                        last_attempt: now,
                    },
                );
            }
            Err(e) => {
                warn!(issuer = %issuer, error = %e, "OIDC JWKS refresh failed");
                if let Some(entry) = self.cache.write().await.get_mut(issuer) {
                    entry.last_attempt = now;
                }
            }
        }
    }

    /// Discover `jwks_uri` from the issuer and fetch its keys as `kid → key`.
    async fn fetch_keys(&self, issuer: &str) -> eyre::Result<HashMap<String, DecodingKey>> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: serde_json::Value = self
            .http
            .get(&discovery_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let jwks_uri = discovery
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("OIDC discovery document has no jwks_uri"))?;
        let jwks: jsonwebtoken::jwk::JwkSet = self
            .http
            .get(jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(jwks_to_keys(&jwks))
    }
}

fn jwks_to_keys(jwks: &jsonwebtoken::jwk::JwkSet) -> HashMap<String, DecodingKey> {
    jwks.keys
        .iter()
        .filter_map(|jwk| Some((jwk.common.key_id.clone()?, DecodingKey::from_jwk(jwk).ok()?)))
        .collect()
}

/// Verify a JWT's signature and `iss`/`aud`/`exp` against `key`, returning its
/// claims as JSON. Pure — no network. Only asymmetric algorithms are accepted,
/// so an `alg=HS256` confusion attack against an RSA key is rejected.
fn verify_token_claims(
    token: &str,
    key: &DecodingKey,
    issuer: &str,
    audience: &str,
) -> Result<serde_json::Value, jsonwebtoken::errors::Error> {
    let header = decode_header(token)?;
    if !ACCEPTED_ALGS.contains(&header.alg) {
        return Err(jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into());
    }
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.set_required_spec_claims(&["exp", "aud", "iss"]);
    Ok(decode::<serde_json::Value>(token, key, &validation)?.claims)
}

/// Authorize verified claims: the configured principal claim must be present
/// and allowlisted. Returns the caller on success.
fn authorize_claims(claims: &serde_json::Value, auth: &OidcTriggerAuth) -> Option<VerifiedCaller> {
    let principal = claims.get(&auth.principal_claim).and_then(|v| v.as_str())?;
    if !auth.allowed_principals.iter().any(|p| p == principal) {
        warn!(principal = %principal, "OIDC principal not in trigger allowlist");
        return None;
    }
    Some(VerifiedCaller {
        principal: principal.to_string(),
        email: claims
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        email_verified: claims
            .get("email_verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Resolve the workforce identity a verified caller should run as: by linked
/// OIDC `sub`, then by verified email. No auto-provisioning — an unmapped
/// caller yields `None` and the workflow runs unscoped.
pub fn resolve_run_as(store: &dyn IdentityStore, caller: &VerifiedCaller) -> Option<String> {
    let active = |id: workforce_identity::types::Identity| {
        (id.status == IdentityStatus::Active).then(|| id.id.to_string())
    };
    if let Ok(Some(identity)) = store.get_by_linked_id(OIDC_LINKED_ID_KEY, &caller.principal) {
        if let Some(id) = active(identity) {
            return Some(id);
        }
    }
    if caller.email_verified {
        if let Some(email) = &caller.email {
            if let Ok(Some(identity)) = store.get_by_email(email) {
                return active(identity);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    const PRIV_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEAmZA1ivd3HPWJm+HT+pZAdIfEPrlFdX3ygMdc16QUFLUDC1P5
h3lKav5ZweBrP4Sqx2cuhlEARAAvx/eqPuwI3/DGAi6Dh0sBnUWgvhr6zGvXLUmz
VvJ/O2yFWjyFFkejLg7hp0Wezhq39JSkNGhRW1Y6f9IIGODE+L5NRKAneFvzee56
BnMHY9wDF2dQ8yPR8SUmVOTy/ss/F3QeebLknEDcCxubJIreRqYisLaz5EXB3GoW
5+tJjlpQbQIZ2KojXMyeN5sqWwZZhbY6EcG7BN8Z7ilJ9x6e7aTOtFWGhRXtuJDh
froLRx0kpEq4WIS/EATF0/8AYlxsNkrcGSBG7wIDAQABAoIBAHpHpPsllwnOU2d3
ae7OmkCwo4o2yaKHSgVVuw5Tr1HV2bi5g6dm4+VS6HJDm+MWZ08lUJzP/2AmreWT
x1MXbPG1rlaqZz3ickSj3x3CKheBf/SmDVKdAsd04LM8IdXd8YiJfRJPk+cWy4jM
m/zHXJFYlniV7LCyuuY/0xNkXEmv4s6YVI1NCvc5qpAes/EvH9CohUPlnWepHFgV
Y+wizcu59hSH0+rbtBcLC+iIF09u1q6sl64XOJhdOMwlhiGWx3kF2KepTFzlepEY
YFYhnAqRU4CViVL5BQSODK2uMGh2xDyzKQQJSj3KdMpPAT1s1zhUgOhtbHDSQ/m3
R4wcPSECgYEAy49lClZkg9hN+yshI/bstTuLfPwG6E9HBoEt8xycsTcgJzrZMQoC
XOTACs/m9rOTrsrDi7nA0iaAlxbnk8LazKhZjojo3BSyKOaJnWgnGIOLfXtLcFHB
esTB/tVFcOlrILGLKQURvRMUe/Xb/NkZuyzJyIVtOYoG19NJYDGC+/kCgYEAwR+R
40YM2Due+cbFIWskyj0ZYdyYnL8ViFrZjRWH9dj/YQtVECcIzMvfTyVcgkbyRIdl
iy1O0qaLR2JME/ymX09mHLvI4YIjS1MdQqxIYMUsVkzQcFxoN8204WAnCrbvqUnR
qh2jVHUY7DyQ/q6HxKtRjIEsWwUyYin80UscBCcCgYBc8Rz+KlH8jO+WBaw9Aooz
eXuH1n7E5SWVx7eWL/MvSu8oWks+g8ZuaZdxmFlpPLbqcPP1wRtUGHZQBgo4Qhiq
pxc06DgB/IUX/aLJtvtayv0OjxPztbB0gI/e7bumAT2DkOzsDr95p9OLgccu3GVO
O/Xp01n6kpXrRzJSdGjM6QKBgQCwco2yX0BBGEgp4igeXmxgsu5L8WcmX5fm20rF
kpSEVN0P53Cyr1dk9ihzgalCBlw3rJVkUeMgp7W9ZNwprr67einLuu7QyzxACBJF
5m7Cq6PUvZ376xH1WmcXO+iv63CQ9bcwfJ/L/KjmvnejmgbF89kwTJ0gFGFyOvaY
qs1QJQKBgQCZv+jfuKXTYamIoZLXrRWQh5/RDYGDS3ddm46AvaqpO8xfKKMLWVMM
0A7LdOl3XHADT6ryp+JhaMTqmAxngqxPuxDP3wIxtZoUZrqHCimFnncx22U6S6ys
ItKFbt4eprrwRUqnYSe7iDuORjgcuRlU6Ind4Yy+yJhh8YDudN6oPQ==
-----END RSA PRIVATE KEY-----";

    const PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAmZA1ivd3HPWJm+HT+pZA
dIfEPrlFdX3ygMdc16QUFLUDC1P5h3lKav5ZweBrP4Sqx2cuhlEARAAvx/eqPuwI
3/DGAi6Dh0sBnUWgvhr6zGvXLUmzVvJ/O2yFWjyFFkejLg7hp0Wezhq39JSkNGhR
W1Y6f9IIGODE+L5NRKAneFvzee56BnMHY9wDF2dQ8yPR8SUmVOTy/ss/F3QeebLk
nEDcCxubJIreRqYisLaz5EXB3GoW5+tJjlpQbQIZ2KojXMyeN5sqWwZZhbY6EcG7
BN8Z7ilJ9x6e7aTOtFWGhRXtuJDhfroLRx0kpEq4WIS/EATF0/8AYlxsNkrcGSBG
7wIDAQAB
-----END PUBLIC KEY-----";

    fn far_future() -> usize {
        // 2099-01-01; the token's exp must outlive the test run.
        4_070_908_800
    }

    fn sign(claims: serde_json::Value) -> String {
        let key = EncodingKey::from_rsa_pem(PRIV_PEM.as_bytes()).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".to_string());
        encode(&header, &claims, &key).unwrap()
    }

    fn pub_key() -> DecodingKey {
        DecodingKey::from_rsa_pem(PUB_PEM.as_bytes()).unwrap()
    }

    fn claims(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "iss": "https://issuer",
            "aud": "wf-triggers",
            "exp": far_future(),
            "sub": "svc-deploy",
        });
        let map = base.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
        base
    }

    #[test]
    fn valid_token_verifies() {
        let token = sign(claims(serde_json::json!({})));
        let out = verify_token_claims(&token, &pub_key(), "https://issuer", "wf-triggers").unwrap();
        assert_eq!(out.get("sub").and_then(|v| v.as_str()), Some("svc-deploy"));
    }

    #[test]
    fn wrong_audience_rejected() {
        let token = sign(claims(serde_json::json!({})));
        assert!(verify_token_claims(&token, &pub_key(), "https://issuer", "other").is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let token = sign(claims(serde_json::json!({})));
        assert!(verify_token_claims(&token, &pub_key(), "https://evil", "wf-triggers").is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let token = sign(claims(serde_json::json!({ "exp": 1_000_000_000usize })));
        assert!(verify_token_claims(&token, &pub_key(), "https://issuer", "wf-triggers").is_err());
    }

    #[test]
    fn hs256_token_rejected() {
        // An attacker forging `alg=HS256` and signing with the public key bytes
        // as an HMAC secret must not pass against the RSA decoding key.
        let key = EncodingKey::from_secret(PUB_PEM.as_bytes());
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-key".to_string());
        let token = encode(&header, &claims(serde_json::json!({})), &key).unwrap();
        assert!(verify_token_claims(&token, &pub_key(), "https://issuer", "wf-triggers").is_err());
    }

    fn auth(principals: &[&str]) -> OidcTriggerAuth {
        OidcTriggerAuth {
            issuer: "https://issuer".to_string(),
            audience: "wf-triggers".to_string(),
            principal_claim: "sub".to_string(),
            allowed_principals: principals.iter().map(|s| s.to_string()).collect(),
            run_as_caller: false,
        }
    }

    #[test]
    fn authorize_allows_listed_principal() {
        let c = claims(serde_json::json!({ "email": "a@x.io", "email_verified": true }));
        let caller = authorize_claims(&c, &auth(&["svc-deploy"])).unwrap();
        assert_eq!(caller.principal, "svc-deploy");
        assert_eq!(caller.email.as_deref(), Some("a@x.io"));
        assert!(caller.email_verified);
    }

    #[test]
    fn authorize_rejects_unlisted_principal() {
        let c = claims(serde_json::json!({}));
        assert!(authorize_claims(&c, &auth(&["someone-else"])).is_none());
    }

    #[test]
    fn authorize_rejects_missing_principal_claim() {
        let mut auth = auth(&["svc-deploy"]);
        auth.principal_claim = "email".to_string();
        assert!(authorize_claims(&claims(serde_json::json!({})), &auth).is_none());
    }
}
