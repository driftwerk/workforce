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
//!   hub length                  2 bytes  big-endian length of the next field
//!   hub                        that many the boss hub as the engine fixed it
//!                                        at link time ([`canonical_hub`]), UTF-8
//!   engine_id                  the rest  the engine's id as registered, UTF-8
//!   ```
//!
//!   Every field but the last has a fixed or stated length, so the framing
//!   is unambiguous. The hub's name makes a proof one hub asked for useless
//!   to any other, so a hub can't relay a challenge it was sent elsewhere.
//!   The context keeps a signature from meaning anything to any other
//!   protocol the key might ever be used in.
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

/// The longest hub name a challenge carries.
pub const HUB_MAX_LEN: usize = 255;

/// One uplink challenge: the hub's nonce, for one link of one engine, to one
/// hub.
#[derive(Debug, Clone, Copy)]
pub struct Challenge<'a> {
    pub nonce: &'a [u8; NONCE_LEN],
    pub token_id: Uuid,
    /// The boss hub, as [`canonical_hub`] gives it.
    pub hub: &'a str,
    pub engine_id: &'a str,
}

/// The name a boss hub goes by in a challenge: its `https` origin, the host
/// in lower case and the port only when it isn't 443, with no user, path,
/// query or fragment. `None` for anything else, so a challenge can't be
/// made for a hub without one.
pub fn canonical_hub(url: &str) -> Option<String> {
    let url = url::Url::parse(url.trim()).ok()?;
    let plain = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path(), "" | "/");
    let host = url.host_str().filter(|_| plain)?.to_ascii_lowercase();
    let name = match url.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    };
    (name.len() <= HUB_MAX_LEN).then_some(name)
}

/// The bytes an engine signs to prove one link to the hub that sent the
/// nonce: see the module docs. `None` when the hub's name is too long to
/// frame.
pub fn challenge_message(c: Challenge<'_>) -> Option<Vec<u8>> {
    let hub_len = u16::try_from(c.hub.len())
        .ok()
        .filter(|n| usize::from(*n) <= HUB_MAX_LEN)?;
    let mut message = Vec::with_capacity(
        CHALLENGE_CONTEXT.len() + NONCE_LEN + 16 + 2 + c.hub.len() + c.engine_id.len(),
    );
    message.extend_from_slice(CHALLENGE_CONTEXT);
    message.extend_from_slice(c.nonce);
    message.extend_from_slice(c.token_id.as_bytes());
    message.extend_from_slice(&hub_len.to_be_bytes());
    message.extend_from_slice(c.hub.as_bytes());
    message.extend_from_slice(c.engine_id.as_bytes());
    Some(message)
}

/// Whether `signature` proves the challenge `c` under the key registered at
/// mint.
pub fn verify_challenge(key: &EnginePublicKey, c: Challenge<'_>, signature: &[u8]) -> bool {
    challenge_message(c).is_some_and(|message| key.verify(&message, signature))
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

    /// This engine's proof of the challenge `c`, or `None` when it can't be
    /// framed.
    pub fn sign_challenge(&self, c: Challenge<'_>) -> Option<[u8; SIGNATURE_LEN]> {
        challenge_message(c).map(|message| self.sign(&message))
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
    const VECTOR_HUB: &str = "https://workforce.example.com";

    fn vector_seed() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }

    fn challenge<'a>(
        nonce: &'a [u8; NONCE_LEN],
        token_id: Uuid,
        hub: &'a str,
        engine_id: &'a str,
    ) -> Challenge<'a> {
        Challenge {
            nonce,
            token_id,
            hub,
            engine_id,
        }
    }

    fn vector() -> Challenge<'static> {
        challenge(
            &VECTOR_NONCE,
            Uuid::parse_str(VECTOR_TOKEN).unwrap(),
            VECTOR_HUB,
            "devbox",
        )
    }

    /// The challenge framing, pinned: computed independently (Python's
    /// `cryptography`) from seed 00 01 … 1f, nonce a5 × 32, token
    /// 6f1c2b0e-…-2a1d5e8c9f00, hub `https://workforce.example.com` and
    /// engine `devbox`. RB5's uplink must produce exactly these bytes.
    #[test]
    fn the_challenge_framing_is_pinned() {
        let message = challenge_message(vector()).unwrap();
        assert_eq!(
            hex::encode(&message),
            concat!(
                "776f726b666f726365206465736b2d626f73732075706c696e6b206368616c6c",
                "656e676520763100",
                "a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5",
                "6f1c2b0e8d4a4c3e9b7f2a1d5e8c9f00",
                "001d",
                "68747470733a2f2f776f726b666f7263652e6578616d706c652e636f6d",
                "646576626f78"
            )
        );
        let key = EngineKey::from_seed(&vector_seed()).unwrap();
        assert_eq!(
            key.public_key().to_text(),
            concat!("ed25519:", "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg")
        );
        let signature = key.sign_challenge(vector()).unwrap();
        assert_eq!(
            hex::encode(signature),
            concat!(
                "9f974d85444d83fe70ee7002c7f2287071b9eaea49f9a342fc788740c90cb78c",
                "58837ccf0ec693c32f85fd43db93c9980a711169b61294668b4195877b620801"
            )
        );
        let registered = EnginePublicKey::parse(&key.public_key().to_text()).unwrap();
        assert!(verify_challenge(&registered, vector(), &signature));
    }

    #[test]
    fn a_proof_holds_only_for_its_own_nonce_token_hub_engine_and_key() {
        let token = Uuid::parse_str(VECTOR_TOKEN).unwrap();
        let key = EngineKey::from_seed(&vector_seed()).unwrap();
        let public = key.public_key();
        let good = key.sign_challenge(vector()).unwrap();
        assert!(verify_challenge(&public, vector(), &good));
        let mut other_nonce = VECTOR_NONCE;
        other_nonce[0] ^= 1;
        let refused = |c: Challenge<'_>, sig: &[u8]| !verify_challenge(&public, c, sig);
        assert!(refused(
            challenge(&other_nonce, token, VECTOR_HUB, "devbox"),
            &good
        ));
        assert!(refused(
            challenge(&VECTOR_NONCE, Uuid::new_v4(), VECTOR_HUB, "devbox"),
            &good
        ));
        for hub in [
            "https://workforce.example.org",
            "https://workforce.example.co",
            "https://workforce.example.com:8443",
            "",
        ] {
            assert!(
                refused(challenge(&VECTOR_NONCE, token, hub, "devbox"), &good),
                "{hub}"
            );
        }
        // The length prefix keeps the hub and engine fields apart.
        assert!(refused(
            challenge(
                &VECTOR_NONCE,
                token,
                "https://workforce.example.comdevbox",
                ""
            ),
            &good
        ));
        for engine in ["devbox2", "devbo", "Devbox", ""] {
            assert!(
                refused(challenge(&VECTOR_NONCE, token, VECTOR_HUB, engine), &good),
                "{engine}"
            );
        }
        for bit in [0usize, 7, 255, 256, 511] {
            let mut flipped = good;
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert!(refused(vector(), &flipped), "bit {bit}");
        }
        for wrong_len in [&good[..63], &[][..], &[good.as_slice(), &[0]].concat()[..]] {
            assert!(refused(vector(), wrong_len));
        }
        let (stranger, _) = EngineKey::generate().unwrap();
        assert!(!verify_challenge(&stranger.public_key(), vector(), &good));
        // A signature over the bare fields, without the context, is no proof.
        let bare = key.sign(&challenge_message(vector()).unwrap()[CHALLENGE_CONTEXT.len()..]);
        assert!(refused(vector(), &bare));
        // A hub name too long to frame is never signed nor accepted.
        let long = format!("https://{}.example.com", "a".repeat(300));
        assert!(key
            .sign_challenge(challenge(&VECTOR_NONCE, token, &long, "devbox"))
            .is_none());
        assert!(refused(
            challenge(&VECTOR_NONCE, token, &long, "devbox"),
            &good
        ));
    }

    #[test]
    fn a_hub_is_named_by_its_https_origin_alone() {
        for (url, want) in [
            (
                "https://workforce.example.com",
                "https://workforce.example.com",
            ),
            (
                "https://Workforce.Example.com/",
                "https://workforce.example.com",
            ),
            (
                "https://workforce.example.com:443/",
                "https://workforce.example.com",
            ),
            (
                "https://workforce.example.com:8443",
                "https://workforce.example.com:8443",
            ),
        ] {
            assert_eq!(canonical_hub(url).as_deref(), Some(want), "{url}");
        }
        for refused in [
            "http://workforce.example.com",
            "https://user@workforce.example.com",
            "https://workforce.example.com/hub",
            "https://workforce.example.com/?a=1",
            "https://workforce.example.com/#x",
            "workforce.example.com",
            "",
        ] {
            assert_eq!(canonical_hub(refused), None, "{refused}");
        }
    }

    #[test]
    fn a_generated_key_round_trips_through_its_stored_form() {
        let (key, stored) = EngineKey::generate().unwrap();
        let again = EngineKey::load(&stored).unwrap();
        assert_eq!(key.public_key(), again.public_key());
        let nonce = fresh_nonce().unwrap();
        let c = challenge(&nonce, Uuid::new_v4(), VECTOR_HUB, "mac");
        let proof = again.sign_challenge(c).unwrap();
        assert!(verify_challenge(
            &EnginePublicKey::parse(&key.public_key().to_text()).unwrap(),
            c,
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
