//! Long-lived, revocable bearer tokens for the remote MCP server.
//!
//! The plaintext is shown once at mint time; only its blake3 hash is stored.
//! Liveness is decided by the caller against a fresh `now` via
//! [`RemoteMcpToken::is_live`], so a stored row never has to be rewritten to
//! expire.

use base64::Engine;
use chrono::{DateTime, Utc};
use eyre::Result;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const TOKEN_PREFIX: &str = "wfr_";
const SECRET_BYTES: usize = 32;
const PREFIX_CHARS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteMcpToken {
    pub id: Uuid,
    pub identity_id: Uuid,
    pub label: String,
    pub token_hash: String,
    pub token_prefix: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl RemoteMcpToken {
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// A freshly minted token. `plaintext` leaves this struct exactly once, to the
/// user; `hash` and `prefix` are what the store keeps.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedToken {
    pub plaintext: String,
    pub hash: String,
    pub prefix: String,
}

impl std::fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedToken")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

pub fn mint() -> MintedToken {
    let mut secret = [0u8; SECRET_BYTES];
    rand::thread_rng().fill_bytes(&mut secret);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let plaintext = format!("{TOKEN_PREFIX}{body}");
    MintedToken {
        hash: hash_token(&plaintext),
        prefix: body.chars().take(PREFIX_CHARS).collect(),
        plaintext,
    }
}

pub fn hash_token(plaintext: &str) -> String {
    blake3::hash(plaintext.as_bytes()).to_hex().to_string()
}

pub trait RemoteMcpTokenStore: Send + Sync {
    fn create_token(&self, token: &RemoteMcpToken) -> Result<()>;
    fn get_token_by_hash(&self, token_hash: &str) -> Result<Option<RemoteMcpToken>>;
    /// Newest first; revoked tokens excluded.
    fn list_tokens_for_identity(&self, identity_id: Uuid) -> Result<Vec<RemoteMcpToken>>;
    /// Newest first across every identity; revoked tokens excluded.
    fn list_all_tokens(&self) -> Result<Vec<RemoteMcpToken>>;
    /// `Some(owner)` only revokes a token that owner holds. `false` when
    /// nothing matched: unknown id, wrong owner, or already revoked.
    fn revoke_token(&self, id: Uuid, identity_id: Option<Uuid>, at: DateTime<Utc>) -> Result<bool>;
    fn touch_token_last_used(&self, id: Uuid, at: DateTime<Utc>) -> Result<()>;
    fn count_live_tokens_for_identity(
        &self,
        identity_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn token(now: DateTime<Utc>) -> RemoteMcpToken {
        RemoteMcpToken {
            id: Uuid::new_v4(),
            identity_id: Uuid::new_v4(),
            label: "laptop".into(),
            token_hash: "h".into(),
            token_prefix: "p".into(),
            scopes: vec!["remote_mcp".into()],
            created_at: now,
            expires_at: now + Duration::days(90),
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn mint_produces_prefixed_unique_tokens_with_matching_hash_and_prefix() {
        let a = mint();
        let b = mint();
        assert_ne!(a.plaintext, b.plaintext);
        assert!(a.plaintext.starts_with(TOKEN_PREFIX));
        assert_eq!(a.plaintext.len(), TOKEN_PREFIX.len() + 43);
        assert!(!a.plaintext.contains('='));
        assert_eq!(a.hash, hash_token(&a.plaintext));
        assert_eq!(a.hash.len(), 64);
        assert_eq!(a.prefix.len(), PREFIX_CHARS);
        assert!(a.plaintext[TOKEN_PREFIX.len()..].starts_with(&a.prefix));
        assert!(!format!("{a:?}").contains(&a.plaintext));
    }

    #[test]
    fn is_live_requires_unrevoked_and_unexpired() {
        let now = Utc::now();
        let live = token(now);
        assert!(live.is_live(now));
        assert!(!live.is_live(now + Duration::days(90)));
        let revoked = RemoteMcpToken {
            revoked_at: Some(now),
            ..token(now)
        };
        assert!(!revoked.is_live(now));
    }
}
