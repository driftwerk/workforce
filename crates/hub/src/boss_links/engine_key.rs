//! An engine's Ed25519 key: the proof of possession behind every Desk boss
//! link, so a link token copied out of a log, a backup or this hub's
//! database can't open an uplink on its own.
//!
//! - Each engine (one running Desk backend, the Mac's or a machine's) makes
//!   one keypair at its first link and keeps the private half at its own
//!   reserved path ([`workforce_common::vault_scope::DESK_ENGINE_KEY`]),
//!   which no agent tool can read. It never leaves the engine.
//! - Desk registers the public half on the boss hub when it mints the link
//!   (`POST /api/me/desk-boss-links`), as [`EnginePublicKey::to_text`].
//! - On every uplink connect (RB5) the boss hub sends a fresh 32-byte nonce,
//!   and the engine signs [`challenge_message`] for each link it carries:
//!
//!   ```text
//!   CHALLENGE_CONTEXT          40 bytes  "workforce desk-boss uplink challenge v1\0"
//!   nonce                      32 bytes  the hub's, fresh per connect
//!   token_id                   16 bytes  the link token's id, as raw UUID bytes
//!   engine_id                  the rest  the engine's id as registered, UTF-8
//!   ```
//!
//!   Every field but the last has a fixed length, so the framing is
//!   unambiguous. The context keeps a signature from meaning anything to
//!   any other protocol the key might ever be used in.
//! - The hub checks it with [`verify_challenge`] against the key registered
//!   at mint, and drops a link whose proof fails.
//!
//! Signing and verifying are `ring`'s, which the tree already builds for
//! rustls. A key is refused at registration when it isn't a canonical
//! encoding or is one of the eight small-order points, whose "signatures"
//! need no private key.

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use uuid::Uuid;

/// What a registered public key starts with.
pub const KEY_PREFIX: &str = "ed25519:";
/// Prepended to every challenge an engine signs: see the module docs.
pub const CHALLENGE_CONTEXT: &[u8] = b"workforce desk-boss uplink challenge v1\0";
/// The length of the hub's challenge nonce.
pub const NONCE_LEN: usize = 32;
/// The length of an Ed25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;
/// The length of an Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// Why a public key can't be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("an engine key is written ed25519:<base64url of its 32 bytes>")]
    Malformed,
    #[error("that isn't a usable Ed25519 public key")]
    Weak,
    #[error("the engine key couldn't be made or read")]
    Crypto,
}

/// An engine's registered public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EnginePublicKey([u8; PUBLIC_KEY_LEN]);

impl std::fmt::Debug for EnginePublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl EnginePublicKey {
    /// `ed25519:<base64url, unpadded, of exactly 32 bytes>`, canonical and
    /// not of small order. Nothing else is read as a key.
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let body = text.strip_prefix(KEY_PREFIX).ok_or(KeyError::Malformed)?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| KeyError::Malformed)?;
        let bytes: [u8; PUBLIC_KEY_LEN] = bytes.try_into().map_err(|_| KeyError::Malformed)?;
        Self::from_bytes(bytes)
    }

    /// The key `bytes` are, when they are canonical and not of small order.
    pub fn from_bytes(bytes: [u8; PUBLIC_KEY_LEN]) -> Result<Self, KeyError> {
        if !canonical(&bytes) || small_order(&bytes) {
            return Err(KeyError::Weak);
        }
        Ok(Self(bytes))
    }

    pub fn bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.0
    }

    /// The form registered at mint and stored with the link.
    pub fn to_text(&self) -> String {
        format!(
            "{KEY_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.0)
        )
    }

    /// Whether `signature` is this key's over `message`.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        signature.len() == SIGNATURE_LEN
            && UnparsedPublicKey::new(&signature::ED25519, &self.0)
                .verify(message, signature)
                .is_ok()
    }
}

/// The bytes an engine signs to prove the link `token_id` to a hub that
/// sent `nonce`: see the module docs.
pub fn challenge_message(nonce: &[u8; NONCE_LEN], token_id: Uuid, engine_id: &str) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(CHALLENGE_CONTEXT.len() + NONCE_LEN + 16 + engine_id.len());
    message.extend_from_slice(CHALLENGE_CONTEXT);
    message.extend_from_slice(nonce);
    message.extend_from_slice(token_id.as_bytes());
    message.extend_from_slice(engine_id.as_bytes());
    message
}

/// Whether `signature` proves the link `token_id` of the engine `engine_id`
/// to the hub that sent `nonce`, under the key registered at mint.
pub fn verify_challenge(
    key: &EnginePublicKey,
    nonce: &[u8; NONCE_LEN],
    token_id: Uuid,
    engine_id: &str,
    signature: &[u8],
) -> bool {
    key.verify(&challenge_message(nonce, token_id, engine_id), signature)
}

/// A fresh challenge nonce, from the system's random source.
pub fn fresh_nonce() -> Result<[u8; NONCE_LEN], KeyError> {
    use ring::rand::SecureRandom as _;
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| KeyError::Crypto)?;
    Ok(nonce)
}

/// An engine's keypair. Its `Debug` shows the public half only.
pub struct EngineKey(Ed25519KeyPair);

impl std::fmt::Debug for EngineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("EngineKey")
            .field(&self.public_key())
            .finish()
    }
}

/// The private half as the engine stores it (PKCS#8 v2), wiped on drop and
/// never printed.
pub struct StoredEngineKey(zeroize::Zeroizing<Vec<u8>>);

impl std::fmt::Debug for StoredEngineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoredEngineKey(…redacted)")
    }
}

impl StoredEngineKey {
    pub fn from_pkcs8(bytes: Vec<u8>) -> Self {
        Self(zeroize::Zeroizing::new(bytes))
    }

    pub fn pkcs8(&self) -> &[u8] {
        &self.0
    }
}

impl EngineKey {
    /// A new keypair, and what the engine stores to load it again.
    pub fn generate() -> Result<(Self, StoredEngineKey), KeyError> {
        let document =
            Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| KeyError::Crypto)?;
        let stored = StoredEngineKey::from_pkcs8(document.as_ref().to_vec());
        Ok((Self::load(&stored)?, stored))
    }

    /// The keypair the engine stored.
    pub fn load(stored: &StoredEngineKey) -> Result<Self, KeyError> {
        Ed25519KeyPair::from_pkcs8(stored.pkcs8())
            .map(Self)
            .map_err(|_| KeyError::Crypto)
    }

    /// The keypair of a 32-byte RFC 8032 secret key: for test vectors.
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, KeyError> {
        Ed25519KeyPair::from_seed_unchecked(seed)
            .map(Self)
            .map_err(|_| KeyError::Crypto)
    }

    pub fn public_key(&self) -> EnginePublicKey {
        let mut bytes = [0u8; PUBLIC_KEY_LEN];
        bytes.copy_from_slice(self.0.public_key().as_ref());
        EnginePublicKey(bytes)
    }

    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        let mut out = [0u8; SIGNATURE_LEN];
        out.copy_from_slice(self.0.sign(message).as_ref());
        out
    }

    /// This engine's proof of the link `token_id` to the hub that sent
    /// `nonce`.
    pub fn sign_challenge(
        &self,
        nonce: &[u8; NONCE_LEN],
        token_id: Uuid,
        engine_id: &str,
    ) -> [u8; SIGNATURE_LEN] {
        self.sign(&challenge_message(nonce, token_id, engine_id))
    }
}

/// p = 2^255 - 19, little-endian, with the sign bit clear.
const FIELD_P: [u8; 32] = [
    0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
];

/// Whether the y coordinate `key` encodes (its sign bit set aside) is below
/// p, as RFC 8032 requires of a canonical encoding.
fn canonical(key: &[u8; 32]) -> bool {
    let mut y = *key;
    y[31] &= 0x7f;
    // Little-endian compare, most significant byte first.
    for i in (0..32).rev() {
        match y[i].cmp(&FIELD_P[i]) {
            std::cmp::Ordering::Less => return true,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    false
}

/// The y coordinates of the eight points of small order, sign bit clear:
/// 0 (order 4), 1 (the identity), p - 1 (order 2), and the two of order 8.
const SMALL_ORDER_Y: [[u8; 32]; 5] = [
    [0; 32],
    [
        0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0,
    ],
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98,
        0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53,
        0xfc, 0x05,
    ],
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67,
        0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac,
        0x03, 0x7a,
    ],
];

fn small_order(key: &[u8; 32]) -> bool {
    let mut y = *key;
    y[31] &= 0x7f;
    SMALL_ORDER_Y.contains(&y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("hex")
    }

    fn arr<const N: usize>(s: &str) -> [u8; N] {
        unhex(s).try_into().expect("length")
    }

    /// RFC 8032 §7.1, test 1: the crate is Ed25519 as the RFC defines it.
    #[test]
    fn rfc_8032_test_1() {
        let seed = arr::<32>(concat!(
            "9d61b19deffd5a60ba844af492ec2cc4",
            "4449c5697b326919703bac031cae7f60"
        ));
        let public = arr::<32>(concat!(
            "d75a980182b10ab7d54bfed3c964073a",
            "0ee172f3daa62325af021a68f707511a"
        ));
        let signature = unhex(concat!(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155",
            "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        ));
        let key = EngineKey::from_seed(&seed).unwrap();
        assert_eq!(key.public_key().bytes(), &public);
        assert_eq!(key.sign(b"").to_vec(), signature);
        assert!(EnginePublicKey::from_bytes(public)
            .unwrap()
            .verify(b"", &signature));
    }

    const VECTOR_TOKEN: &str = "6f1c2b0e-8d4a-4c3e-9b7f-2a1d5e8c9f00";
    const VECTOR_NONCE: [u8; NONCE_LEN] = [0xa5; NONCE_LEN];

    fn vector_seed() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }

    /// The challenge framing, pinned: computed independently (Python's
    /// `cryptography`) from seed 00 01 … 1f, nonce a5 × 32, token
    /// 6f1c2b0e-…-2a1d5e8c9f00 and engine `devbox`. RB5's uplink must
    /// produce exactly these bytes.
    #[test]
    fn the_challenge_framing_is_pinned() {
        let token = Uuid::parse_str(VECTOR_TOKEN).unwrap();
        let message = challenge_message(&VECTOR_NONCE, token, "devbox");
        assert_eq!(
            hex::encode(&message),
            concat!(
                "776f726b666f726365206465736b2d626f73732075706c696e6b206368616c6c",
                "656e676520763100",
                "a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5",
                "6f1c2b0e8d4a4c3e9b7f2a1d5e8c9f00",
                "646576626f78"
            )
        );
        let key = EngineKey::from_seed(&vector_seed()).unwrap();
        assert_eq!(
            key.public_key().to_text(),
            concat!("ed25519:", "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg")
        );
        let signature = key.sign_challenge(&VECTOR_NONCE, token, "devbox");
        assert_eq!(
            hex::encode(signature),
            concat!(
                "d8be0fe55a905c0c5d61490823bef41476f455e654461eb00a407c623e740484",
                "a519d064097849526b15d5f8ce256d78b609b77f78711cfb38b8b3e2c85a1706"
            )
        );
        let registered = EnginePublicKey::parse(&key.public_key().to_text()).unwrap();
        assert!(verify_challenge(
            &registered,
            &VECTOR_NONCE,
            token,
            "devbox",
            &signature
        ));
    }

    #[test]
    fn a_proof_holds_only_for_its_own_nonce_token_engine_and_key() {
        let token = Uuid::parse_str(VECTOR_TOKEN).unwrap();
        let key = EngineKey::from_seed(&vector_seed()).unwrap();
        let public = key.public_key();
        let good = key.sign_challenge(&VECTOR_NONCE, token, "devbox");
        assert!(verify_challenge(
            &public,
            &VECTOR_NONCE,
            token,
            "devbox",
            &good
        ));
        let mut other_nonce = VECTOR_NONCE;
        other_nonce[0] ^= 1;
        assert!(!verify_challenge(
            &public,
            &other_nonce,
            token,
            "devbox",
            &good
        ));
        assert!(!verify_challenge(
            &public,
            &VECTOR_NONCE,
            Uuid::new_v4(),
            "devbox",
            &good
        ));
        for engine in ["devbox2", "devbo", "Devbox", ""] {
            assert!(
                !verify_challenge(&public, &VECTOR_NONCE, token, engine, &good),
                "{engine}"
            );
        }
        for bit in [0usize, 7, 255, 256, 511] {
            let mut flipped = good;
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert!(
                !verify_challenge(&public, &VECTOR_NONCE, token, "devbox", &flipped),
                "bit {bit}"
            );
        }
        for wrong_len in [&good[..63], &[][..], &[good.as_slice(), &[0]].concat()[..]] {
            assert!(!verify_challenge(
                &public,
                &VECTOR_NONCE,
                token,
                "devbox",
                wrong_len
            ));
        }
        let (stranger, _) = EngineKey::generate().unwrap();
        assert!(!verify_challenge(
            &stranger.public_key(),
            &VECTOR_NONCE,
            token,
            "devbox",
            &good
        ));
        // A signature over the bare fields, without the context, is no proof.
        let bare =
            key.sign(&challenge_message(&VECTOR_NONCE, token, "devbox")[CHALLENGE_CONTEXT.len()..]);
        assert!(!verify_challenge(
            &public,
            &VECTOR_NONCE,
            token,
            "devbox",
            &bare
        ));
    }

    #[test]
    fn a_generated_key_round_trips_through_its_stored_form() {
        let (key, stored) = EngineKey::generate().unwrap();
        let again = EngineKey::load(&stored).unwrap();
        assert_eq!(key.public_key(), again.public_key());
        let nonce = fresh_nonce().unwrap();
        let token = Uuid::new_v4();
        let proof = again.sign_challenge(&nonce, token, "mac");
        assert!(verify_challenge(
            &EnginePublicKey::parse(&key.public_key().to_text()).unwrap(),
            &nonce,
            token,
            "mac",
            &proof
        ));
        assert_ne!(fresh_nonce().unwrap(), nonce);
        assert!(!format!("{stored:?}").contains(&hex::encode(stored.pkcs8())));
        assert_eq!(
            format!("{key:?}"),
            format!("EngineKey({})", key.public_key().to_text())
        );
        assert!(EngineKey::load(&StoredEngineKey::from_pkcs8(vec![1, 2, 3])).is_err());
    }

    #[test]
    fn only_a_well_formed_key_is_read() {
        let text = EngineKey::from_seed(&vector_seed())
            .unwrap()
            .public_key()
            .to_text();
        assert!(EnginePublicKey::parse(&text).is_ok());
        let body = &text[KEY_PREFIX.len()..];
        for bad in [
            body.to_string(),
            format!("ED25519:{body}"),
            format!("ed25519:{body}="),
            format!("ed25519: {body}"),
            format!("ed25519:{}", &body[1..]),
            format!("ed25519:{body}A"),
            format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode([0xfb; 32])
            ),
            "ed25519:".to_string(),
            String::new(),
        ] {
            assert_eq!(
                EnginePublicKey::parse(&bad),
                Err(KeyError::Malformed),
                "{bad}"
            );
        }
    }

    /// Every encoding of every point of small order, and every
    /// non-canonical encoding, is refused; the list is checked against
    /// curve25519-dalek's own eight-torsion points and ed25519-dalek's
    /// `is_weak`.
    #[test]
    fn small_order_and_non_canonical_keys_are_refused() {
        use curve25519_dalek::constants::EIGHT_TORSION;
        for point in EIGHT_TORSION {
            let bytes = point.compress().to_bytes();
            for sign in [0u8, 0x80] {
                let mut b = bytes;
                b[31] = (b[31] & 0x7f) | sign;
                assert_eq!(
                    EnginePublicKey::from_bytes(b),
                    Err(KeyError::Weak),
                    "{b:02x?}"
                );
            }
        }
        for y in SMALL_ORDER_Y {
            if let Ok(k) = ed25519_dalek::VerifyingKey::from_bytes(&y) {
                assert!(k.is_weak(), "{y:02x?} is listed but isn't weak");
            }
        }
        // p, p + 1, …, 2^255 - 1 are non-canonical.
        for low in [0xedu8, 0xee, 0xff] {
            let mut b = FIELD_P;
            b[0] = low;
            assert_eq!(EnginePublicKey::from_bytes(b), Err(KeyError::Weak));
        }
        // p - 1 is canonical, though of small order; p - 2 is a fine
        // encoding as far as canonicity goes.
        let mut below = FIELD_P;
        below[0] = 0xeb;
        assert!(canonical(&below));
        for _ in 0..64 {
            let (key, _) = EngineKey::generate().unwrap();
            let bytes = *key.public_key().bytes();
            assert!(EnginePublicKey::from_bytes(bytes).is_ok());
            assert!(!ed25519_dalek::VerifyingKey::from_bytes(&bytes)
                .unwrap()
                .is_weak());
        }
    }
}
