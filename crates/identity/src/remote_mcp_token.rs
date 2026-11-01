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
const FINGERPRINT_CONTEXT: &str = "workforce remote-mcp key fingerprint v1";
const FINGERPRINT_HEX_CHARS: usize = 8;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteMcpToken {
    pub id: Uuid,
    pub identity_id: Uuid,
    pub label: String,
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Everything but the stored hash: a Debug line is a log line.
impl std::fmt::Debug for RemoteMcpToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteMcpToken")
            .field("id", &self.id)
            .field("identity_id", &self.identity_id)
            .field("label", &self.label)
            .field("scopes", &self.scopes)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("last_used_at", &self.last_used_at)
            .field("revoked_at", &self.revoked_at)
            .finish_non_exhaustive()
    }
}

impl RemoteMcpToken {
    /// Non-secret label for telling keys apart in a listing. See
    /// [`fingerprint`].
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.token_hash)
    }

    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// A freshly minted token. `plaintext` leaves this struct exactly once, to the
/// user; `hash` is what the store keeps.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedToken {
    pub plaintext: String,
    pub hash: String,
}

impl std::fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No part of the secret: a Debug line is a log line.
        f.debug_struct("MintedToken").finish_non_exhaustive()
    }
}

pub fn mint() -> MintedToken {
    let mut secret = [0u8; SECRET_BYTES];
    rand::thread_rng().fill_bytes(&mut secret);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let plaintext = format!("{TOKEN_PREFIX}{body}");
    MintedToken {
        hash: hash_token(&plaintext),
        plaintext,
    }
}

pub fn hash_token(plaintext: &str) -> String {
    blake3::hash(plaintext.as_bytes()).to_hex().to_string()
}

/// A short label that names a key without revealing any of it.
///
/// Derived from the stored hash (never from the key), through a
/// domain-separated blake3 `derive_key`, so it is not a slice of the value
/// authentication compares against. The key is 256 random bits and the hash
/// is one-way, so 32 bits of a further one-way digest tell an attacker
/// nothing about the key body. Computed on read, so every key has one with
/// no backfill.
pub fn fingerprint(token_hash: &str) -> String {
    let digest = blake3::derive_key(FINGERPRINT_CONTEXT, token_hash.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..FINGERPRINT_HEX_CHARS].to_string()
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
            scopes: vec!["remote_mcp".into()],
            created_at: now,
            expires_at: now + Duration::days(90),
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn mint_produces_prefixed_unique_tokens_with_matching_hash() {
        let a = mint();
        let b = mint();
        assert_ne!(a.plaintext, b.plaintext);
        assert!(a.plaintext.starts_with(TOKEN_PREFIX));
        assert_eq!(a.plaintext.len(), TOKEN_PREFIX.len() + 43);
        assert!(!a.plaintext.contains('='));
        assert_eq!(a.hash, hash_token(&a.plaintext));
        assert_eq!(a.hash.len(), 64);
        assert!(!format!("{a:?}").contains(&a.plaintext));
    }

    /// Every window of 6 to 12 characters of the key body, to catch any
    /// fragment (shorter windows could match unrelated output by chance).
    fn fragments(body: &str) -> Vec<&str> {
        (0..=body.len() - 6)
            .flat_map(|i| (6..=(body.len() - i).min(12)).map(move |n| (i, n)))
            .map(|(i, n)| &body[i..i + n])
            .collect()
    }

    #[test]
    fn fingerprint_is_short_stable_and_not_a_slice_of_key_or_hash() {
        let minted = mint();
        let fp = fingerprint(&minted.hash);
        assert_eq!(fp.len(), FINGERPRINT_HEX_CHARS);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp, fingerprint(&minted.hash));
        assert_ne!(fp, fingerprint(&mint().hash));
        assert!(!minted.hash.contains(&fp));
        assert!(!minted.plaintext.contains(&fp));
    }

    #[test]
    fn debug_output_carries_no_fragment_of_the_key() {
        let minted = mint();
        let body = &minted.plaintext[TOKEN_PREFIX.len()..];
        let record = RemoteMcpToken {
            token_hash: minted.hash.clone(),
            ..token(Utc::now())
        };
        for shown in [format!("{minted:?}"), format!("{record:?}")] {
            for frag in fragments(body) {
                assert!(!shown.contains(frag), "debug output leaked a key fragment");
            }
            assert!(
                !shown.contains(&minted.hash),
                "debug output leaked the hash"
            );
        }
        assert_eq!(record.fingerprint(), fingerprint(&minted.hash));
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
