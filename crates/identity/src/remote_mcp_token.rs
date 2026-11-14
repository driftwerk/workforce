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
/// What a Desk boss link token starts with. A different prefix from
/// [`TOKEN_PREFIX`], so the remote MCP routes, which take only `wfr_`, never
/// look one up, and redaction knows it by sight.
pub const DESK_BOSS_TOKEN_PREFIX: &str = "wfb_";
/// What a Desk knows token starts with: the token a person's Desk pushes
/// what one of their agents here knows of their Desk work with (Sees).
pub const DESK_KNOWS_TOKEN_PREFIX: &str = "wfk_";
/// The scope entry that names the paired device a token was minted by:
/// `device:<device id>`. It binds the token to that device, and is not a
/// kind of its own.
pub const DEVICE_SCOPE_PREFIX: &str = "device:";
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
    /// What a [`TokenScope::DeskBoss`] token links, and nothing for any
    /// other kind.
    pub desk_link: Option<DeskLink>,
}

/// What one token can be used for. A token is exactly one kind, read from
/// its scopes by [`TokenScope::of`], which fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    /// The hub as an MCP server for the person's own clients (`/mcp`).
    RemoteMcp,
    /// One Desk boss link and nothing else: an engine's uplink to an agent
    /// here that bosses one of the person's Desk groups. It reaches no
    /// other route.
    DeskBoss,
    /// What one agent here knows of the person's Desk work, pushed by their
    /// Desk, and nothing else: it reaches only the Desk knows routes.
    DeskKnows,
}

impl TokenScope {
    pub const ALL: [TokenScope; 3] = [
        TokenScope::RemoteMcp,
        TokenScope::DeskBoss,
        TokenScope::DeskKnows,
    ];

    /// The scope entry stored for this kind.
    pub fn key(self) -> &'static str {
        match self {
            TokenScope::RemoteMcp => "remote_mcp",
            TokenScope::DeskBoss => "desk_boss",
            TokenScope::DeskKnows => "desk_knows",
        }
    }

    /// The prefix a plaintext token of this kind starts with.
    pub fn prefix(self) -> &'static str {
        match self {
            TokenScope::RemoteMcp => TOKEN_PREFIX,
            TokenScope::DeskBoss => DESK_BOSS_TOKEN_PREFIX,
            TokenScope::DeskKnows => DESK_KNOWS_TOKEN_PREFIX,
        }
    }

    /// The one kind `scopes` names, or `None`. Fails closed: exactly one
    /// known kind, at most one `device:<id>` with a non-empty id, and
    /// nothing else. A scope this hub doesn't know may be one meant to
    /// narrow the token, so the token is refused rather than given
    /// anything. A Desk boss token must name its device.
    pub fn of(scopes: &[String]) -> Option<TokenScope> {
        let (devices, kinds): (Vec<&String>, Vec<&String>) = scopes
            .iter()
            .partition(|s| s.starts_with(DEVICE_SCOPE_PREFIX));
        if devices.len() > 1 || devices.iter().any(|d| d.len() == DEVICE_SCOPE_PREFIX.len()) {
            return None;
        }
        let [kind] = kinds.as_slice() else {
            return None;
        };
        let kind = TokenScope::ALL.into_iter().find(|k| k.key() == *kind)?;
        match kind {
            TokenScope::DeskBoss | TokenScope::DeskKnows if devices.is_empty() => None,
            kind => Some(kind),
        }
    }
}

/// What one Desk boss link token binds, beside its person and device: the
/// agent here that bosses, the Desk group, and the engine (one running Desk
/// backend) whose uplink carries it. `desk_group_key`, `group_name` and the
/// engine's names are what the minting Desk says; the hub keeps them for
/// display and never trusts them for authority. `group_key` is the hub's
/// own, derived from the person, the device and `desk_group_key`, so two
/// people's or two Desks' groups can never share one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskLink {
    /// Stays the same across a renewal, so the link keeps its identity.
    pub link_id: Uuid,
    pub agent_id: String,
    /// The hub's key for the group: see the type's docs.
    pub group_key: String,
    /// The group's key as the minting Desk named it.
    #[serde(default)]
    pub desk_group_key: String,
    pub group_name: String,
    pub engine_id: String,
    pub engine_name: String,
    /// The engine's Ed25519 public key, registered at mint:
    /// `ed25519:<base64url, unpadded, of the 32 key bytes>`. The uplink
    /// proves the private half on every connect.
    pub engine_public_key: String,
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
            .field("desk_link", &self.desk_link)
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

    /// This token's one kind, or `None` when it can't be told: its scopes
    /// as [`TokenScope::of`] reads them, and a link exactly when it is a
    /// Desk boss token.
    pub fn scope(&self) -> Option<TokenScope> {
        match (TokenScope::of(&self.scopes)?, &self.desk_link) {
            (TokenScope::RemoteMcp, None) => Some(TokenScope::RemoteMcp),
            (TokenScope::DeskBoss, Some(_)) => Some(TokenScope::DeskBoss),
            (TokenScope::DeskKnows, Some(_)) => Some(TokenScope::DeskKnows),
            _ => None,
        }
    }

    /// The paired device its scopes say minted it.
    pub fn minted_by_device(&self) -> Option<&str> {
        minted_by_device(&self.scopes)
    }
}

/// The device `scopes` say minted a token, when they name exactly one.
pub fn minted_by_device(scopes: &[String]) -> Option<&str> {
    let mut devices = scopes
        .iter()
        .filter_map(|s| s.strip_prefix(DEVICE_SCOPE_PREFIX));
    match (devices.next(), devices.next()) {
        (Some(d), None) if !d.is_empty() => Some(d),
        _ => None,
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
    mint_as(TokenScope::RemoteMcp)
}

/// A fresh token of `kind`, starting with that kind's prefix.
pub fn mint_as(kind: TokenScope) -> MintedToken {
    let mut secret = [0u8; SECRET_BYTES];
    rand::thread_rng().fill_bytes(&mut secret);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let plaintext = format!("{}{body}", kind.prefix());
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
            desk_link: None,
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

    fn scopes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn link() -> DeskLink {
        DeskLink {
            link_id: Uuid::new_v4(),
            agent_id: "allan".into(),
            group_key: "d-1/g-acme".into(),
            desk_group_key: "d-1/g-acme".into(),
            group_name: "Acme".into(),
            engine_id: "devbox".into(),
            engine_name: "devbox".into(),
            engine_public_key: "ed25519:AAAA".into(),
        }
    }

    #[test]
    fn a_token_is_exactly_one_known_kind_or_none() {
        let of = |list: &[&str]| TokenScope::of(&scopes(list));
        assert_eq!(of(&["remote_mcp"]), Some(TokenScope::RemoteMcp));
        assert_eq!(
            of(&["remote_mcp", "device:d1"]),
            Some(TokenScope::RemoteMcp)
        );
        assert_eq!(of(&["device:d1", "desk_boss"]), Some(TokenScope::DeskBoss));
        for refused in [
            &[][..],
            &["desk_boss"][..],
            &["remote_mcp", "desk_boss", "device:d1"][..],
            &["remote_mcp", "remote_mcp"][..],
            &["remote_mcp", "admin"][..],
            &["Remote_MCP"][..],
            &["remote_mcp", "device:"][..],
            &["desk_boss", "device:"][..],
            &["remote_mcp", "device:a", "device:b"][..],
            &["desk_boss", "device:a", "device:b"][..],
            &["device:a"][..],
            &["*"][..],
        ] {
            assert_eq!(TokenScope::of(&scopes(refused)), None, "{refused:?}");
        }
    }

    #[test]
    fn a_desk_boss_token_needs_its_link_and_no_other_kind_has_one() {
        let now = Utc::now();
        let boss = RemoteMcpToken {
            scopes: scopes(&["desk_boss", "device:d1"]),
            desk_link: Some(link()),
            ..token(now)
        };
        assert_eq!(boss.scope(), Some(TokenScope::DeskBoss));
        assert_eq!(boss.minted_by_device(), Some("d1"));
        let unlinked = RemoteMcpToken {
            desk_link: None,
            ..boss.clone()
        };
        assert_eq!(unlinked.scope(), None);
        let mcp_with_link = RemoteMcpToken {
            desk_link: Some(link()),
            ..token(now)
        };
        assert_eq!(mcp_with_link.scope(), None);
        assert_eq!(token(now).scope(), Some(TokenScope::RemoteMcp));
    }

    #[test]
    fn each_kind_mints_with_its_own_prefix() {
        let boss = mint_as(TokenScope::DeskBoss);
        assert!(boss.plaintext.starts_with(DESK_BOSS_TOKEN_PREFIX));
        assert!(!boss.plaintext.starts_with(TOKEN_PREFIX));
        assert_eq!(boss.plaintext.len(), DESK_BOSS_TOKEN_PREFIX.len() + 43);
        assert_eq!(boss.hash, hash_token(&boss.plaintext));
        assert!(mint().plaintext.starts_with(TOKEN_PREFIX));
    }

    #[test]
    fn minted_by_device_names_one_device_or_none() {
        assert_eq!(
            minted_by_device(&scopes(&["remote_mcp", "device:d1"])),
            Some("d1")
        );
        assert_eq!(minted_by_device(&scopes(&["remote_mcp"])), None);
        assert_eq!(minted_by_device(&scopes(&["device:"])), None);
        assert_eq!(minted_by_device(&scopes(&["device:a", "device:b"])), None);
    }
}
