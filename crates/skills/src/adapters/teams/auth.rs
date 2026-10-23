//! Bot Connector request authentication.
//!
//! Every inbound `POST /api/messages` carries a JWT minted by the Azure Bot
//! Connector. Before we act on an [`super::types::Activity`] we verify, per
//! <https://learn.microsoft.com/azure/bot-service/rest-api/bot-framework-rest-connector-authentication>:
//!
//! 1. signature against the keys in the **static** Bot Framework metadata doc;
//! 2. `iss == https://api.botframework.com`;
//! 3. `aud == <our Microsoft App ID>`;
//! 4. `exp` valid (industry-standard 5-minute clock skew);
//! 5. the token's `serviceurl` claim equals the Activity's `serviceUrl`.
//!
//! Point 5 is the load-bearing one: `serviceUrl` is where we send replies *with
//! our connector bearer token*. Without the bind, a forged Activity with an
//! attacker-controlled `serviceUrl` would exfiltrate our replies and token. Any
//! failure ⇒ the route returns `403`.
//!
//! The JWKS cache / throttle / algorithm-allowlist mirror
//! `hub::trigger_oidc` so the two do not drift on the security-relevant bits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eyre::{bail, eyre, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use tokio::sync::RwLock;
use tracing::warn;

/// Static OpenID metadata document for the Bot Framework channel keys.
const METADATA_URL: &str = "https://login.botframework.com/v1/.well-known/openidconfiguration";
/// The only issuer accepted for channel→bot tokens (public cloud).
const ISSUER: &str = "https://api.botframework.com";
/// Industry-standard clock-skew allowance.
const CLOCK_SKEW: u64 = 300;
/// How long fetched keys are trusted before a refresh is required.
const JWKS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Minimum gap between fetch attempts, so a flood of unknown `kid`s cannot
/// amplify into a request storm against the metadata endpoint.
const JWKS_REFRESH_THROTTLE: Duration = Duration::from_secs(30);
/// Asymmetric algorithms only — excluding HMAC defeats an `alg=HS256` confusion
/// attack that signs with the RSA public-key bytes as an HMAC secret.
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

/// Cached signing keys with fetch timestamps.
struct CachedKeys {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
    last_attempt: Instant,
}

/// Validates Bot Connector tokens against the Bot Framework signing keys.
pub struct BotConnectorValidator {
    http: reqwest::Client,
    cache: RwLock<Option<CachedKeys>>,
}

impl Default for BotConnectorValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl BotConnectorValidator {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            cache: RwLock::new(None),
        }
    }

    /// Verify a presented token for `expected_app_id` and bind it to
    /// `activity_service_url`. `Ok(())` ⇒ trust the request; any `Err` ⇒ `403`.
    pub async fn validate(
        &self,
        token: &str,
        expected_app_id: &str,
        activity_service_url: &str,
    ) -> Result<()> {
        let kid = decode_header(token)?
            .kid
            .ok_or_else(|| eyre!("token header missing kid"))?;
        let key = self
            .decoding_key(&kid)
            .await
            .ok_or_else(|| eyre!("no Bot Framework signing key for kid {kid}"))?;
        verify_connector_token(token, &key, expected_app_id, activity_service_url)
    }

    /// Decoding key for `kid`, refreshing the JWKS on miss/stale (throttled).
    async fn decoding_key(&self, kid: &str) -> Option<DecodingKey> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.as_ref() {
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
        self.refresh().await;
        self.cache.read().await.as_ref()?.keys.get(kid).cloned()
    }

    /// Fetch the metadata doc, then its JWKS, and replace the cache. On failure,
    /// record the attempt time (throttle) and keep any existing keys.
    async fn refresh(&self) {
        let now = Instant::now();
        match self.fetch_keys().await {
            Ok(keys) => {
                *self.cache.write().await = Some(CachedKeys {
                    keys,
                    fetched_at: now,
                    last_attempt: now,
                });
            }
            Err(e) => {
                warn!(error = %e, "Bot Framework JWKS refresh failed");
                if let Some(entry) = self.cache.write().await.as_mut() {
                    entry.last_attempt = now;
                }
            }
        }
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>> {
        let metadata: serde_json::Value = self
            .http
            .get(METADATA_URL)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let jwks_uri = metadata
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("Bot Framework metadata has no jwks_uri"))?;
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

/// Pure verification: signature + `iss`/`aud`/`exp` + the `serviceUrl` bind.
/// No network — the caller supplies the already-resolved signing `key`, which
/// keeps this unit-testable with a self-signed key.
fn verify_connector_token(
    token: &str,
    key: &DecodingKey,
    expected_app_id: &str,
    activity_service_url: &str,
) -> Result<()> {
    let header = decode_header(token)?;
    if !ACCEPTED_ALGS.contains(&header.alg) {
        bail!("rejected token algorithm {:?}", header.alg);
    }
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[expected_app_id]);
    validation.leeway = CLOCK_SKEW;
    validation.set_required_spec_claims(&["exp", "aud", "iss"]);

    let claims = decode::<serde_json::Value>(token, key, &validation)?.claims;

    let claim_url = claims
        .get("serviceurl")
        .or_else(|| claims.get("serviceUrl"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("token missing serviceurl claim"))?;
    if normalize_url(claim_url) != normalize_url(activity_service_url) {
        bail!("token serviceUrl does not match the activity serviceUrl");
    }
    Ok(())
}

/// Trailing-slash-insensitive comparison (Teams sometimes appends one).
fn normalize_url(url: &str) -> &str {
    url.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    // Test-only RSA keypair (shared shape with hub::trigger_oidc tests).
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

    const APP_ID: &str = "bot-app-id";
    const SERVICE_URL: &str = "https://smba.trafficmanager.net/amer/";

    fn far_future() -> usize {
        4_070_908_800 // 2099-01-01
    }

    fn pub_key() -> DecodingKey {
        DecodingKey::from_rsa_pem(PUB_PEM.as_bytes()).unwrap()
    }

    fn claims(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "iss": ISSUER,
            "aud": APP_ID,
            "exp": far_future(),
            "serviceurl": SERVICE_URL,
        });
        let map = base.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
        base
    }

    fn sign(alg: Algorithm, claims: serde_json::Value) -> String {
        let key = EncodingKey::from_rsa_pem(PRIV_PEM.as_bytes()).unwrap();
        let mut header = Header::new(alg);
        header.kid = Some("test-key".to_string());
        encode(&header, &claims, &key).unwrap()
    }

    #[test]
    fn valid_token_passes() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        verify_connector_token(&token, &pub_key(), APP_ID, SERVICE_URL).unwrap();
    }

    #[test]
    fn tampered_signature_rejected() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        let (head, _sig) = token.rsplit_once('.').unwrap();
        let tampered = format!("{head}.AAAAinvalidsignatureAAAA");
        assert!(verify_connector_token(&tampered, &pub_key(), APP_ID, SERVICE_URL).is_err());
    }

    #[test]
    fn trailing_slash_difference_is_tolerated() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        // Activity url without the trailing slash still matches the claim.
        verify_connector_token(
            &token,
            &pub_key(),
            APP_ID,
            "https://smba.trafficmanager.net/amer",
        )
        .unwrap();
    }

    #[test]
    fn forged_service_url_rejected() {
        // Attacker controls the Activity serviceUrl but not the signed claim.
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        let err = verify_connector_token(&token, &pub_key(), APP_ID, "https://evil.example.com/")
            .unwrap_err()
            .to_string();
        assert!(err.contains("serviceUrl"), "{err}");
    }

    #[test]
    fn wrong_audience_rejected() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        assert!(verify_connector_token(&token, &pub_key(), "other-app", SERVICE_URL).is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let token = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "iss": "https://evil" })),
        );
        assert!(verify_connector_token(&token, &pub_key(), APP_ID, SERVICE_URL).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let token = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "exp": 1_000_000_000usize })),
        );
        assert!(verify_connector_token(&token, &pub_key(), APP_ID, SERVICE_URL).is_err());
    }

    #[test]
    fn hs256_confusion_rejected() {
        // Forge alg=HS256, signing with the RSA public-key bytes as the HMAC secret.
        let key = EncodingKey::from_secret(PUB_PEM.as_bytes());
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-key".to_string());
        let token = encode(&header, &claims(serde_json::json!({})), &key).unwrap();
        assert!(verify_connector_token(&token, &pub_key(), APP_ID, SERVICE_URL).is_err());
    }

    #[test]
    fn missing_service_url_claim_rejected() {
        let mut c = claims(serde_json::json!({}));
        c.as_object_mut().unwrap().remove("serviceurl");
        let token = sign(Algorithm::RS256, c);
        assert!(verify_connector_token(&token, &pub_key(), APP_ID, SERVICE_URL).is_err());
    }
}
