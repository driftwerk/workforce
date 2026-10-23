//! Key hierarchy and sealing shared by the hub vault and the edge: Argon2id
//! of a passphrase gives a [`MasterKey`] that wraps random data keys
//! ([`Dek`]); each DEK derives an AES-256-GCM key for sealing and an HMAC
//! key for blind lookups. Every seal is bound to caller-supplied AAD, so
//! ciphertext moved to another key or context fails to open.
//!
//! The byte layouts here are frozen: hub vault rows written before this
//! crate existed must keep opening. [`Sealed::to_bytes`] is
//! `nonce ‖ ciphertext ‖ tag` with no version prefix, DEK wraps are bound to
//! `"wfk1" ‖ key_id`, and DEK subkeys come from HKDF-SHA256 with the
//! `workforce-vault/v1/*` labels.

use std::fmt;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use uuid::Uuid;
pub use zeroize::Zeroizing;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const SALT_LEN: usize = 16;

const ENC_INFO: &[u8] = b"workforce-vault/v1/enc";
const MAC_INFO: &[u8] = b"workforce-vault/v1/mac";
const WRAP_DOMAIN: &[u8] = b"wfk1";

/// Failures of this crate. Messages never carry key material, plaintext or
/// ciphertext, so they are safe to log as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid key-derivation parameters")]
    InvalidKdfParams,
    #[error("key derivation failed")]
    KeyDerivation,
    #[error("the system random source failed")]
    Randomness,
    #[error("malformed nonce")]
    MalformedNonce,
    #[error("malformed sealed data")]
    MalformedSealed,
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed: wrong key, wrong context or tampered data")]
    Decrypt,
    #[error("unwrapped key has the wrong length")]
    MalformedKey,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Argon2id cost. Stored alongside every wrap so it can be raised later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    /// The `argon2` crate's defaults (19 MiB, 2 passes, 1 lane), which is
    /// what every existing hub wrap was derived with.
    fn default() -> Self {
        let params = Params::default();
        Self {
            m_kib: params.m_cost(),
            t: params.t_cost(),
            p: params.p_cost(),
        }
    }
}

impl KdfParams {
    const MAX_M_KIB: u32 = 256 * 1024;
    const MAX_T: u32 = 16;
    const MAX_P: u32 = 8;

    /// A valid Argon2 cost no dearer than 256 MiB × 16 passes × 8 lanes, so a
    /// tampered wrap cannot make a process spend unbounded memory deriving a
    /// key.
    pub fn usable(&self) -> bool {
        self.m_kib <= Self::MAX_M_KIB
            && self.t <= Self::MAX_T
            && self.p <= Self::MAX_P
            && self.argon2().is_ok()
    }

    fn argon2(&self) -> Result<Params> {
        Params::new(self.m_kib, self.t, self.p, Some(KEY_LEN)).map_err(|_| Error::InvalidKdfParams)
    }
}

/// Nonce and ciphertext (with its appended GCM tag) of one seal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl Sealed {
    /// Assemble from separately stored columns; rejects a nonce of the wrong
    /// length rather than panicking on it.
    pub fn from_parts(nonce: &[u8], ciphertext: &[u8]) -> Result<Self> {
        Ok(Self {
            nonce: nonce.try_into().map_err(|_| Error::MalformedNonce)?,
            ciphertext: ciphertext.to_vec(),
        })
    }

    /// `nonce ‖ ciphertext ‖ tag`.
    pub fn to_bytes(&self) -> Vec<u8> {
        [&self.nonce[..], &self.ciphertext].concat()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(Error::MalformedSealed);
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        Self::from_parts(nonce, ciphertext)
    }
}

/// Key-encryption key: 32 bytes, usually Argon2id of a passphrase. Wraps
/// DEKs and can seal small payloads directly. Its key schedule is wiped on
/// drop.
pub struct MasterKey {
    cipher: Aes256Gcm,
}

impl MasterKey {
    pub fn from_bytes(key: &[u8; KEY_LEN]) -> Self {
        Self {
            cipher: cipher(key),
        }
    }

    /// Argon2id (v0x13) of `passphrase` over `salt` at `params`.
    pub fn derive(passphrase: &str, salt: &[u8], params: KdfParams) -> Result<Self> {
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.argon2()?);
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        argon
            .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
            .map_err(|_| Error::KeyDerivation)?;
        Ok(Self::from_bytes(&key))
    }

    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Sealed> {
        seal(&self.cipher, aad, plaintext)
    }

    pub fn open(&self, aad: &[u8], sealed: &Sealed) -> Result<Zeroizing<Vec<u8>>> {
        open(&self.cipher, aad, sealed)
    }

    /// Seal `dek`'s raw bytes, bound to its id.
    pub fn wrap_dek(&self, dek: &Dek) -> Result<Sealed> {
        self.seal(&wrap_aad(dek.id), dek.raw.as_slice())
    }

    /// The DEK `key_id` wrapped in `wrapped`; fails when this key did not
    /// wrap it or `key_id` is not the id it was wrapped under.
    pub fn unwrap_dek(&self, key_id: Uuid, wrapped: &Sealed) -> Result<Dek> {
        let raw = self.open(&wrap_aad(key_id), wrapped)?;
        let raw: [u8; KEY_LEN] = raw.as_slice().try_into().map_err(|_| Error::MalformedKey)?;
        Ok(Dek::from_bytes(key_id, Zeroizing::new(raw)))
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// A 32-byte subkey of `ikm` under `info`: HKDF-SHA256 with no salt, as the
/// DEK subkeys are derived. `info` is the domain label that keeps one
/// derivation apart from every other use of the same input key material;
/// callers choose a label of their own and never reuse this crate's.
pub fn derive_subkey(ikm: &[u8], info: &[u8]) -> Zeroizing<[u8; KEY_LEN]> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    Hkdf::<Sha256>::new(None, ikm)
        .expand(info, key.as_mut())
        .expect("a 32-byte HKDF-SHA256 output is always within range");
    key
}

/// Data-encryption key with its derived seal cipher and lookup MAC. Raw
/// bytes are kept only so the key can be wrapped, and are wiped on drop.
pub struct Dek {
    id: Uuid,
    raw: Zeroizing<[u8; KEY_LEN]>,
    cipher: Aes256Gcm,
    mac: Hmac<Sha256>,
}

impl Dek {
    /// A fresh random key under a fresh random id.
    pub fn generate() -> Result<Self> {
        Ok(Self::from_bytes(Uuid::new_v4(), Zeroizing::new(random()?)))
    }

    pub fn from_bytes(id: Uuid, raw: Zeroizing<[u8; KEY_LEN]>) -> Self {
        let hkdf = Hkdf::<Sha256>::new(None, raw.as_slice());
        let subkey = |info: &[u8]| {
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            hkdf.expand(info, key.as_mut())
                .expect("a 32-byte HKDF-SHA256 output is always within range");
            key
        };
        let cipher = cipher(&subkey(ENC_INFO));
        let mac = <Hmac<Sha256> as Mac>::new_from_slice(subkey(MAC_INFO).as_slice())
            .expect("HMAC accepts a key of any length");
        Self {
            id,
            raw,
            cipher,
            mac,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Sealed> {
        seal(&self.cipher, aad, plaintext)
    }

    pub fn open(&self, aad: &[u8], sealed: &Sealed) -> Result<Zeroizing<Vec<u8>>> {
        open(&self.cipher, aad, sealed)
    }

    /// Blind index for `path` within `scope`: stable per key, reveals
    /// neither, and distinguishes `("ab","c")` from `("a","bc")`.
    pub fn lookup_mac(&self, scope: &str, path: &str) -> Vec<u8> {
        let mut mac = self.mac.clone();
        mac.update(scope.as_bytes());
        mac.update(&[0]);
        mac.update(path.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
}

impl fmt::Debug for Dek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dek")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A fresh random salt for [`MasterKey::derive`].
pub fn generate_salt() -> Result<[u8; SALT_LEN]> {
    random()
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| Error::Randomness)?;
    Ok(bytes)
}

fn cipher(key: &[u8; KEY_LEN]) -> Aes256Gcm {
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key))
}

fn seal(cipher: &Aes256Gcm, aad: &[u8], msg: &[u8]) -> Result<Sealed> {
    seal_with_nonce(cipher, random()?, aad, msg)
}

fn seal_with_nonce(
    cipher: &Aes256Gcm,
    nonce: [u8; NONCE_LEN],
    aad: &[u8],
    msg: &[u8],
) -> Result<Sealed> {
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg, aad })
        .map_err(|_| Error::Encrypt)?;
    Ok(Sealed { nonce, ciphertext })
}

fn open(cipher: &Aes256Gcm, aad: &[u8], sealed: &Sealed) -> Result<Zeroizing<Vec<u8>>> {
    cipher
        .decrypt(
            Nonce::from_slice(&sealed.nonce),
            Payload {
                msg: &sealed.ciphertext,
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| Error::Decrypt)
}

fn wrap_aad(key_id: Uuid) -> Vec<u8> {
    [WRAP_DOMAIN, key_id.as_bytes()].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "correct horse battery staple";

    fn cheap() -> KdfParams {
        KdfParams {
            m_kib: 8,
            t: 1,
            p: 1,
        }
    }

    fn fixed_dek() -> Dek {
        let raw: [u8; KEY_LEN] = core::array::from_fn(|i| i as u8 + 1);
        Dek::from_bytes(fixed_id(), Zeroizing::new(raw))
    }

    fn fixed_id() -> Uuid {
        Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap()
    }

    fn fixed_nonce() -> [u8; NONCE_LEN] {
        core::array::from_fn(|i| 0xa0 + i as u8)
    }

    fn fixed_salt() -> [u8; SALT_LEN] {
        core::array::from_fn(|i| i as u8)
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
    fn default_cost_matches_what_existing_hub_wraps_were_derived_with() {
        assert_eq!(
            KdfParams::default(),
            KdfParams {
                m_kib: 19 * 1024,
                t: 2,
                p: 1,
            }
        );
    }

    #[test]
    fn master_key_round_trips_with_matching_aad() {
        let master = MasterKey::derive(PASSPHRASE, &fixed_salt(), cheap()).unwrap();
        let sealed = master.seal(b"ctx", b"hunter2").unwrap();
        assert_eq!(master.open(b"ctx", &sealed).unwrap().as_slice(), b"hunter2");
    }

    #[test]
    fn dek_round_trips_with_matching_aad() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal(b"ctx", b"xoxb-token").unwrap();
        assert_eq!(dek.open(b"ctx", &sealed).unwrap().as_slice(), b"xoxb-token");
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let right = MasterKey::derive(PASSPHRASE, &fixed_salt(), cheap()).unwrap();
        let wrong = MasterKey::derive("wrong", &fixed_salt(), cheap()).unwrap();
        let sealed = right.seal(b"ctx", b"secret").unwrap();
        assert_eq!(wrong.open(b"ctx", &sealed).unwrap_err(), Error::Decrypt);

        let other_dek = Dek::generate().unwrap();
        let sealed = Dek::generate().unwrap().seal(b"ctx", b"secret").unwrap();
        assert_eq!(other_dek.open(b"ctx", &sealed).unwrap_err(), Error::Decrypt);
    }

    #[test]
    fn wrong_aad_fails_to_open() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal(b"scope:a", b"secret").unwrap();
        assert_eq!(dek.open(b"scope:b", &sealed).unwrap_err(), Error::Decrypt);
        assert_eq!(dek.open(b"", &sealed).unwrap_err(), Error::Decrypt);
    }

    #[test]
    fn any_tampered_byte_fails_to_open() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal(b"ctx", b"secret").unwrap();
        let bytes = sealed.to_bytes();
        for i in 0..bytes.len() {
            let mut tampered = bytes.clone();
            tampered[i] ^= 0x01;
            let tampered = Sealed::from_bytes(&tampered).unwrap();
            assert_eq!(
                dek.open(b"ctx", &tampered).unwrap_err(),
                Error::Decrypt,
                "byte {i} flipped but still opened"
            );
        }
    }

    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let dek = Dek::generate().unwrap();
        let first = dek.seal(b"ctx", b"v").unwrap();
        let second = dek.seal(b"ctx", b"v").unwrap();
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn sealed_bytes_round_trip_and_reject_truncation() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal(b"ctx", b"secret").unwrap();
        let bytes = sealed.to_bytes();
        assert_eq!(bytes.len(), NONCE_LEN + b"secret".len() + TAG_LEN);
        assert_eq!(Sealed::from_bytes(&bytes).unwrap(), sealed);
        assert_eq!(
            Sealed::from_bytes(&bytes[..NONCE_LEN + TAG_LEN - 1]).unwrap_err(),
            Error::MalformedSealed
        );
        assert_eq!(
            Sealed::from_parts(&[0u8; 3], &sealed.ciphertext).unwrap_err(),
            Error::MalformedNonce
        );
    }

    #[test]
    fn a_master_key_unwraps_only_the_dek_it_wrapped_under_its_id() {
        let dek = Dek::generate().unwrap();
        let master = MasterKey::derive(PASSPHRASE, &fixed_salt(), cheap()).unwrap();
        let wrapped = master.wrap_dek(&dek).unwrap();

        let unwrapped = master.unwrap_dek(dek.id(), &wrapped).unwrap();
        assert_eq!(unwrapped.id(), dek.id());
        assert_eq!(unwrapped.lookup_mac("s", "p"), dek.lookup_mac("s", "p"));
        let sealed = dek.seal(b"ctx", b"v").unwrap();
        assert_eq!(unwrapped.open(b"ctx", &sealed).unwrap().as_slice(), b"v");

        let wrong = MasterKey::derive("wrong", &fixed_salt(), cheap()).unwrap();
        assert_eq!(
            wrong.unwrap_dek(dek.id(), &wrapped).unwrap_err(),
            Error::Decrypt
        );
        assert_eq!(
            master.unwrap_dek(Uuid::new_v4(), &wrapped).unwrap_err(),
            Error::Decrypt
        );
    }

    #[test]
    fn lookup_macs_are_stable_and_distinct() {
        let dek = Dek::generate().unwrap();
        assert_eq!(dek.lookup_mac("s", "p"), dek.lookup_mac("s", "p"));
        assert_ne!(dek.lookup_mac("s", "p"), dek.lookup_mac("s", "q"));
        assert_ne!(dek.lookup_mac("s", "p"), dek.lookup_mac("t", "p"));
        assert_ne!(dek.lookup_mac("ab", "c"), dek.lookup_mac("a", "bc"));
        assert_ne!(
            dek.lookup_mac("s", "p"),
            Dek::generate().unwrap().lookup_mac("s", "p")
        );
    }

    #[test]
    fn unusable_kdf_params_are_rejected_before_deriving() {
        assert_eq!(
            MasterKey::derive(PASSPHRASE, &fixed_salt(), KdfParams { t: 0, ..cheap() })
                .unwrap_err(),
            Error::InvalidKdfParams
        );
    }

    #[test]
    fn errors_never_mention_secret_material() {
        let dek = Dek::generate().unwrap();
        let sealed = dek.seal(b"ctx", b"hunter2").unwrap();
        let err = dek.open(b"other", &sealed).unwrap_err().to_string();
        assert!(!err.contains("hunter2"));
        assert!(!err.contains(&hex::encode(&sealed.ciphertext)));
    }

    // Vectors captured from the hub implementation this crate was extracted
    // from (crates/hub/src/vault/crypto.rs before the extraction). They pin
    // the HKDF labels, the wrap AAD, the Argon2id variant and the GCM layout:
    // a change to any of these would stop existing vault rows from opening.
    mod fixed_vectors {
        use super::*;

        const DEK_SEAL: &str =
            "65e1bc4c8a989248d7023ea29ca4f46af57326aba5a45bb77c6ab5261cfc9ffa9593e728da36751e50d13fdc98";
        const PATH_MAC: &str = "6b8d54aa048e6ab78951a1d1bb7ca536fd6d20405c99a2eaabfbdd897b0b95f5";
        const KEK_WRAP_CHEAP: &str =
            "5906d5716f193811e0983d2f56fdb08c8226ae15d2f240d1fac935330e2c57bc2622dbed70dad314dd7db02d87eb36c5";
        const KEK_WRAP_DEFAULT: &str =
            "66bb977908d9ddbf6b81dfd5f1304922d222a3e538b4411dd80d0a0cdb3001d779e8a5f43bd2a0de29ca49bb21c39ada";

        #[test]
        fn dek_seal_matches_the_hub_implementation() {
            let dek = fixed_dek();
            let sealed = seal_with_nonce(
                &dek.cipher,
                fixed_nonce(),
                b"wfv1-test-aad",
                b"workforce-crypto fixed vector",
            )
            .unwrap();
            assert_eq!(hex::encode(&sealed.ciphertext), DEK_SEAL);
            assert_eq!(
                dek.open(b"wfv1-test-aad", &sealed).unwrap().as_slice(),
                b"workforce-crypto fixed vector"
            );
        }

        #[test]
        fn lookup_mac_matches_the_hub_implementation() {
            assert_eq!(
                hex::encode(fixed_dek().lookup_mac("global", "slack/bot_token")),
                PATH_MAC
            );
        }

        #[test]
        fn dek_wrap_matches_the_hub_implementation_at_cheap_and_default_cost() {
            for (params, expected) in [
                (cheap(), KEK_WRAP_CHEAP),
                (KdfParams::default(), KEK_WRAP_DEFAULT),
            ] {
                let master = MasterKey::derive(PASSPHRASE, &fixed_salt(), params).unwrap();
                let dek = fixed_dek();
                let wrapped = seal_with_nonce(
                    &master.cipher,
                    fixed_nonce(),
                    &wrap_aad(dek.id()),
                    dek.raw.as_slice(),
                )
                .unwrap();
                assert_eq!(hex::encode(&wrapped.ciphertext), expected);
                let unwrapped = master.unwrap_dek(fixed_id(), &wrapped).unwrap();
                assert_eq!(unwrapped.raw, dek.raw);
            }
        }
    }
}
