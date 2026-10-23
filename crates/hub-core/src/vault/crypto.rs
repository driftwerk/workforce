//! Vault glue over `workforce_crypto` for the Postgres backend: a KEK is the
//! master key derived from the passphrase plus the salt and cost it was
//! derived with; a DEK seals entries and audit records bound (AAD) to its
//! key id, scope and lookup key, so ciphertext moved between rows, scopes or
//! keys fails to open. The primitives and byte layouts live in the crate.

use eyre::{bail, Result};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use workforce_common::settings::VaultEntry;
use workforce_crypto::MasterKey;
use zeroize::Zeroizing;

use crate::hub_core::vault_audit::AuditEntry;

pub(super) use workforce_crypto::{KdfParams, Sealed, NONCE_LEN};

/// A vault entry sealed into its row.
pub(super) struct SealedEntry {
    pub key_id: Uuid,
    pub path_mac: Vec<u8>,
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

/// Key-encryption key: Argon2id of the vault passphrase, remembering the
/// salt and cost so the wrap row can record them.
pub(super) struct Kek {
    salt: Vec<u8>,
    params: KdfParams,
    master: MasterKey,
}

impl Kek {
    pub fn derive(passphrase: &str, salt: Vec<u8>, params: KdfParams) -> Result<Self> {
        let master = MasterKey::derive(passphrase, &salt, params)?;
        Ok(Self {
            salt,
            params,
            master,
        })
    }

    /// A KEK over a fresh random salt.
    pub fn generate(passphrase: &str, params: KdfParams) -> Result<Self> {
        Self::derive(
            passphrase,
            workforce_crypto::generate_salt()?.to_vec(),
            params,
        )
    }

    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    pub fn params(&self) -> KdfParams {
        self.params
    }

    pub fn wrap(&self, dek: &Dek) -> Result<Sealed> {
        Ok(self.master.wrap_dek(&dek.0)?)
    }

    /// The DEK `key_id` wrapped here, or `None` when this KEK didn't wrap it.
    pub fn unwrap(&self, key_id: Uuid, nonce: &[u8], wrapped: &[u8]) -> Option<Dek> {
        let sealed = Sealed::from_parts(nonce, wrapped).ok()?;
        self.master.unwrap_dek(key_id, &sealed).ok().map(Dek)
    }
}

/// Data-encryption key for one generation of vault rows.
pub(super) struct Dek(workforce_crypto::Dek);

impl Dek {
    pub fn generate() -> Result<Self> {
        Ok(Self(workforce_crypto::Dek::generate()?))
    }

    pub fn id(&self) -> Uuid {
        self.0.id()
    }

    /// Lookup key for `path` within `scope`; reveals neither.
    pub fn path_mac(&self, scope: &str, path: &str) -> Vec<u8> {
        self.0.lookup_mac(scope, path)
    }

    pub fn seal_entry(&self, scope: &str, path: &str, entry: &VaultEntry) -> Result<SealedEntry> {
        let path_mac = self.path_mac(scope, path);
        let plaintext = Zeroizing::new(serde_json::to_vec(&Stored { path, entry })?);
        let Sealed { nonce, ciphertext } = self
            .0
            .seal(&entry_aad(self.id(), scope, &path_mac), &plaintext)?;
        Ok(SealedEntry {
            key_id: self.id(),
            path_mac,
            nonce,
            ciphertext,
        })
    }

    pub fn open_entry(
        &self,
        scope: &str,
        path_mac: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<(String, VaultEntry)> {
        let sealed = Sealed::from_parts(nonce, ciphertext)?;
        let plaintext = self
            .0
            .open(&entry_aad(self.id(), scope, path_mac), &sealed)?;
        let Stored { path, entry }: Stored<String, VaultEntry> =
            serde_json::from_slice(&plaintext)?;
        if !bool::from(self.path_mac(scope, &path).ct_eq(path_mac)) {
            bail!("vault row does not match its lookup key");
        }
        Ok((path, entry))
    }

    pub fn seal_audit(&self, scope: &str, entry: &AuditEntry<'_>) -> Result<Sealed> {
        Ok(self
            .0
            .seal(&audit_aad(self.id(), scope), &serde_json::to_vec(entry)?)?)
    }

    pub fn open_audit(
        &self,
        scope: &str,
        nonce: &[u8],
        details: &[u8],
    ) -> Result<AuditEntry<'static>> {
        let sealed = Sealed::from_parts(nonce, details)?;
        let plaintext = self.0.open(&audit_aad(self.id(), scope), &sealed)?;
        Ok(serde_json::from_slice(&plaintext)?)
    }
}

#[derive(Serialize, Deserialize)]
struct Stored<P, E> {
    path: P,
    #[serde(flatten)]
    entry: E,
}

fn entry_aad(key_id: Uuid, scope: &str, path_mac: &[u8]) -> Vec<u8> {
    let parts: [&[u8]; 5] = [
        b"wfv1",
        key_id.as_bytes(),
        &scope_len(scope),
        scope.as_bytes(),
        path_mac,
    ];
    parts.concat()
}

fn audit_aad(key_id: Uuid, scope: &str) -> Vec<u8> {
    let parts: [&[u8]; 4] = [
        b"wfa1",
        key_id.as_bytes(),
        &scope_len(scope),
        scope.as_bytes(),
    ];
    parts.concat()
}

fn scope_len(scope: &str) -> [u8; 4] {
    u32::try_from(scope.len()).unwrap_or(u32::MAX).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_core::vault_audit::AuditAction;
    use chrono::Utc;

    fn cheap() -> KdfParams {
        KdfParams {
            m_kib: 8,
            t: 1,
            p: 1,
        }
    }

    fn entry(value: &str) -> VaultEntry {
        let now = Utc::now();
        VaultEntry {
            value: value.into(),
            created_at: now,
            updated_at: now,
            created_by: "cli".into(),
            updated_by: "cli".into(),
        }
    }

    #[test]
    fn only_valid_bounded_costs_are_usable() {
        assert!(KdfParams::default().usable());
        assert!(cheap().usable());
        assert!(!KdfParams {
            m_kib: u32::MAX,
            ..cheap()
        }
        .usable());
        assert!(!KdfParams { t: 0, ..cheap() }.usable());
        assert!(!KdfParams { p: 1000, ..cheap() }.usable());
    }

    #[test]
    fn a_kek_opens_only_what_it_wrapped() {
        let dek = Dek::generate().unwrap();
        let kek = Kek::generate("right", cheap()).unwrap();
        let wrapped = kek.wrap(&dek).unwrap();

        let opened = kek
            .unwrap(dek.id(), &wrapped.nonce, &wrapped.ciphertext)
            .unwrap();
        assert_eq!(opened.path_mac("s", "p"), dek.path_mac("s", "p"));

        let wrong = Kek::derive("wrong", kek.salt().to_vec(), kek.params()).unwrap();
        assert!(wrong
            .unwrap(dek.id(), &wrapped.nonce, &wrapped.ciphertext)
            .is_none());
        assert!(kek
            .unwrap(Uuid::new_v4(), &wrapped.nonce, &wrapped.ciphertext)
            .is_none());
    }

    #[test]
    fn entries_round_trip() {
        let dek = Dek::generate().unwrap();
        let sealed = dek
            .seal_entry("global", "slack/bot_token", &entry("xoxb"))
            .unwrap();
        let (path, opened) = dek
            .open_entry(
                "global",
                &sealed.path_mac,
                &sealed.nonce,
                &sealed.ciphertext,
            )
            .unwrap();
        assert_eq!(path, "slack/bot_token");
        assert_eq!(opened.value, "xoxb");
        assert_eq!(sealed.key_id, dek.id());
    }

    #[test]
    fn ciphertext_cannot_move_between_rows_scopes_or_keys() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal_entry("global", "a", &entry("secret")).unwrap();
        let other_path = dek.path_mac("global", "b");
        let other_scope = dek.path_mac("agent:x", "a");

        assert!(dek
            .open_entry("global", &other_path, &sealed.nonce, &sealed.ciphertext)
            .is_err());
        assert!(dek
            .open_entry("agent:x", &other_scope, &sealed.nonce, &sealed.ciphertext)
            .is_err());
        let other_key = Dek::generate().unwrap();
        assert!(other_key
            .open_entry(
                "global",
                &sealed.path_mac,
                &sealed.nonce,
                &sealed.ciphertext
            )
            .is_err());
    }

    #[test]
    fn a_row_claiming_another_paths_lookup_key_is_rejected() {
        let dek = Dek::generate().unwrap();
        let claimed = dek.path_mac("global", "b");
        let plaintext = serde_json::to_vec(&Stored {
            path: "a",
            entry: &entry("secret"),
        })
        .unwrap();
        let forged = dek
            .0
            .seal(&entry_aad(dek.id(), "global", &claimed), &plaintext)
            .unwrap();

        let err = dek
            .open_entry("global", &claimed, &forged.nonce, &forged.ciphertext)
            .unwrap_err();
        assert!(err.to_string().contains("lookup key"));
    }

    #[test]
    fn lookup_keys_are_stable_and_distinct() {
        let dek = Dek::generate().unwrap();
        assert_eq!(dek.path_mac("s", "p"), dek.path_mac("s", "p"));
        assert_ne!(dek.path_mac("s", "p"), dek.path_mac("s", "q"));
        assert_ne!(dek.path_mac("s", "p"), dek.path_mac("t", "p"));
        assert_ne!(dek.path_mac("ab", "c"), dek.path_mac("a", "bc"));
        assert_ne!(
            dek.path_mac("s", "p"),
            Dek::generate().unwrap().path_mac("s", "p")
        );
    }

    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let dek = Dek::generate().unwrap();
        let first = dek.seal_entry("s", "p", &entry("v")).unwrap();
        let second = dek.seal_entry("s", "p", &entry("v")).unwrap();
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn audit_records_round_trip_within_their_scope_only() {
        let dek = Dek::generate().unwrap();
        let record = AuditEntry {
            ts: Utc::now(),
            actor: "user:u1".into(),
            action: AuditAction::Set,
            path: "users/u1/token".into(),
            ok: true,
            value_sha256_first8: Some("abcd1234".into()),
            prev_value_sha256_first8: None,
        };
        let sealed = dek.seal_audit("global", &record).unwrap();

        let opened = dek
            .open_audit("global", &sealed.nonce, &sealed.ciphertext)
            .unwrap();
        assert_eq!(opened.path, "users/u1/token");
        assert_eq!(opened.action, AuditAction::Set);
        assert!(dek
            .open_audit("agent:x", &sealed.nonce, &sealed.ciphertext)
            .is_err());
    }

    #[test]
    fn malformed_nonces_are_rejected_not_panicked_on() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal_entry("s", "p", &entry("v")).unwrap();
        assert!(dek
            .open_entry("s", &sealed.path_mac, &[0u8; 3], &sealed.ciphertext)
            .is_err());
    }
}
