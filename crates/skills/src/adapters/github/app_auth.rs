//! GitHub App identity for the writes a personal token may not make.
//!
//! GitHub refuses a review that approves or requests changes on a pull
//! request opened by the same account, and every PR the pipeline opens is
//! opened by its token. A GitHub App is a second identity: the hub signs a
//! short-lived JWT with the App's private key, exchanges it for an
//! installation token scoped to the repository, and submits the review with
//! that. Installation tokens last an hour and are cached per repository.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use eyre::{eyre, Result, WrapErr};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;

/// The App's identity as the vault holds it: the App (or client) ID and the
/// RSA private key PEM GitHub generated for it.
#[derive(Debug, Clone)]
pub struct AppCredentials {
    pub app_id: String,
    pub private_key_pem: String,
}

/// JWT lifetime. GitHub allows ten minutes; nine leaves room for clock drift
/// on top of the sixty seconds the issued-at claim is set in the past.
const JWT_LIFETIME: Duration = Duration::minutes(9);
const JWT_DRIFT: Duration = Duration::seconds(60);
/// An installation token is reused until this close to its expiry.
const TOKEN_MARGIN: Duration = Duration::minutes(5);

#[derive(Serialize)]
struct Claims<'a> {
    iat: i64,
    exp: i64,
    iss: &'a str,
}

/// The App-level bearer token: RS256, issued sixty seconds ago, valid for
/// nine minutes, issuer the App ID.
pub fn app_jwt(creds: &AppCredentials, now: DateTime<Utc>) -> Result<String> {
    let key = EncodingKey::from_rsa_pem(creds.private_key_pem.as_bytes())
        .wrap_err("GitHub App private key is not a valid RSA PEM")?;
    let claims = Claims {
        iat: (now - JWT_DRIFT).timestamp(),
        exp: (now + JWT_LIFETIME).timestamp(),
        iss: &creds.app_id,
    };
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
        .wrap_err("signing the GitHub App JWT")
}

/// An installation token and when GitHub stops honouring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl InstallationToken {
    fn usable_at(&self, now: DateTime<Utc>) -> bool {
        now + TOKEN_MARGIN < self.expires_at
    }
}

/// Installation tokens by repository, reused while they have more than
/// [`TOKEN_MARGIN`] left.
#[derive(Debug, Default)]
pub struct InstallationTokens {
    by_repo: Mutex<HashMap<String, InstallationToken>>,
}

impl InstallationTokens {
    /// The cached token for `repo` when it is still usable at `now`.
    pub fn cached(&self, repo: &str, now: DateTime<Utc>) -> Option<InstallationToken> {
        self.by_repo
            .lock()
            .ok()?
            .get(repo)
            .filter(|t| t.usable_at(now))
            .cloned()
    }

    pub fn remember(&self, repo: &str, token: InstallationToken) {
        if let Ok(mut map) = self.by_repo.lock() {
            map.insert(repo.to_string(), token);
        }
    }

    /// Drop `repo`'s token: GitHub stopped honouring it before its expiry.
    pub fn forget(&self, repo: &str) {
        if let Ok(mut map) = self.by_repo.lock() {
            map.remove(repo);
        }
    }
}

/// Why the App could not act on a repository. `NotInstalled` is the one a
/// caller may fall back from: the App exists but this repository is outside
/// its installation.
#[derive(Debug)]
pub enum AppError {
    NotInstalled { repo: String },
    Other(eyre::Report),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::NotInstalled { repo } => {
                write!(f, "the GitHub App is not installed on {repo}")
            }
            AppError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<eyre::Report> for AppError {
    fn from(e: eyre::Report) -> Self {
        AppError::Other(e)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Other(e.into())
    }
}

/// The installation id GitHub reports for a repository, from a
/// `GET /repos/{owner}/{repo}/installation` body.
pub fn installation_id_of(body: &serde_json::Value) -> Result<u64> {
    body["id"]
        .as_u64()
        .ok_or_else(|| eyre!("installation lookup returned no id: {body}"))
}

/// The token GitHub minted, from a
/// `POST /app/installations/{id}/access_tokens` body.
pub fn installation_token_of(body: &serde_json::Value) -> Result<InstallationToken> {
    let token = body["token"]
        .as_str()
        .ok_or_else(|| eyre!("access token response carried no token"))?;
    let expires_at = body["expires_at"]
        .as_str()
        .ok_or_else(|| eyre!("access token response carried no expires_at"))?;
    let expires_at = DateTime::parse_from_rfc3339(expires_at)
        .wrap_err("access token expires_at is not RFC 3339")?
        .with_timezone(&Utc);
    Ok(InstallationToken {
        token: token.to_string(),
        expires_at,
    })
}

#[cfg(test)]
pub(crate) mod test_keys {
    pub const PRIVATE_PEM: &str = include_str!("test_app_key.pem");
    pub const PUBLIC_PEM: &str = include_str!("test_app_key.pub.pem");
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct Decoded {
        iat: i64,
        exp: i64,
        iss: String,
    }

    fn creds() -> AppCredentials {
        AppCredentials {
            app_id: "12345".into(),
            private_key_pem: test_keys::PRIVATE_PEM.into(),
        }
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_jwt_is_rs256_issued_a_minute_ago_for_nine_minutes_by_the_app() {
        let now = at("2026-09-13T10:00:00Z");
        let jwt = app_jwt(&creds(), now).unwrap();
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
        let key = DecodingKey::from_rsa_pem(test_keys::PUBLIC_PEM.as_bytes()).unwrap();
        let claims = decode::<Decoded>(&jwt, &key, &validation).unwrap().claims;
        assert_eq!(claims.iss, "12345");
        assert_eq!(claims.iat, now.timestamp() - 60);
        assert_eq!(claims.exp, now.timestamp() + 9 * 60);
        assert!(
            claims.exp - claims.iat <= 10 * 60,
            "GitHub caps the JWT at ten minutes"
        );
    }

    #[test]
    fn a_pem_with_windows_line_endings_and_surrounding_whitespace_still_signs() {
        let creds = AppCredentials {
            app_id: "12345".into(),
            private_key_pem: format!("\n  {}\n\n", test_keys::PRIVATE_PEM.replace('\n', "\r\n")),
        };
        assert!(app_jwt(&creds, Utc::now()).is_ok());
    }

    #[test]
    fn a_forgotten_token_is_minted_again() {
        let cache = InstallationTokens::default();
        let token = InstallationToken {
            token: "ghs_abc".into(),
            expires_at: at("2026-09-13T11:00:00Z"),
        };
        cache.remember("org/repo", token);
        cache.forget("org/repo");
        assert_eq!(cache.cached("org/repo", at("2026-09-13T10:00:00Z")), None);
    }

    #[test]
    fn a_key_that_is_not_rsa_pem_is_refused_before_signing() {
        let bad = AppCredentials {
            app_id: "1".into(),
            private_key_pem: "not a key".into(),
        };
        let err = app_jwt(&bad, Utc::now()).unwrap_err();
        assert!(format!("{err:#}").contains("not a valid RSA PEM"));
    }

    #[test]
    fn a_cached_token_is_reused_until_five_minutes_before_expiry() {
        let cache = InstallationTokens::default();
        let token = InstallationToken {
            token: "ghs_abc".into(),
            expires_at: at("2026-09-13T11:00:00Z"),
        };
        cache.remember("org/repo", token.clone());
        assert_eq!(
            cache.cached("org/repo", at("2026-09-13T10:54:59Z")),
            Some(token)
        );
        assert_eq!(cache.cached("org/repo", at("2026-09-13T10:55:00Z")), None);
        assert_eq!(cache.cached("org/other", at("2026-09-13T10:00:00Z")), None);
    }

    #[test]
    fn github_responses_are_read_into_the_installation_and_token() {
        assert_eq!(installation_id_of(&json!({ "id": 987 })).unwrap(), 987);
        assert!(installation_id_of(&json!({ "message": "Not Found" })).is_err());
        let token = installation_token_of(&json!({
            "token": "ghs_xyz",
            "expires_at": "2026-09-13T11:00:00Z"
        }))
        .unwrap();
        assert_eq!(token.token, "ghs_xyz");
        assert_eq!(token.expires_at, at("2026-09-13T11:00:00Z"));
        assert!(installation_token_of(&json!({ "token": "ghs_xyz" })).is_err());
    }
}
