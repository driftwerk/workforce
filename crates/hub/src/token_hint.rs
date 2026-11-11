//! What a failed sign-in may say about the credential it presented.
//!
//! No part of a secret is ever logged: not a prefix, not a suffix. A log line
//! that carries even four characters of a live token narrows a guess, and the
//! log pipeline is read by far more people than the vault is. What a failed
//! sign-in log needs is much less: *what sort* of credential was refused (so a
//! stale app is told from a scanner), and a way to see that two refusals came
//! from the same credential.
//!
//! The first is the token's kind, read from its public format marker. The
//! second is a keyed hash: HMAC-SHA-256 under a key drawn at random when the
//! process first needs it and never written anywhere, cut to eight hex digits.
//! It cannot be reversed or looked up offline, since the key is not known
//! beyond the process, and it changes at each restart, so it correlates within
//! one run and links nothing across runs.

use std::fmt;
use std::sync::OnceLock;

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

/// Hex digits of the keyed hash kept: 32 bits, enough to tell apart the few
/// credentials failing at once, far too few to identify one.
const TAG_HEX_DIGITS: usize = 8;

/// The process's hashing key.
fn key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        key
    })
}

/// A refused credential, as far as a log may describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHint {
    /// The sort of token it looks like.
    pub kind: &'static str,
    /// A keyed hash of it, to correlate refusals of one credential.
    pub tag: String,
}

impl fmt::Display for TokenHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} #{}", self.kind, self.tag)
    }
}

/// The kind of `token`, from its public format marker alone.
fn kind_of(token: &str) -> &'static str {
    if token.is_empty() {
        "empty"
    } else if token.starts_with("wfp_") {
        "device token"
    } else if token.starts_with("wfr_") {
        "API token"
    } else if token.starts_with("wfb_") {
        "Desk boss link token"
    } else if token.starts_with("eyJ") && token.matches('.').count() == 2 {
        "JWT"
    } else {
        "unknown format"
    }
}

/// What may be logged about `token`: its kind and a keyed tag.
pub fn hint(token: &str) -> TokenHint {
    tag_under(key(), token)
}

fn tag_under(key: &[u8; 32], token: &str) -> TokenHint {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any size");
    mac.update(token.as_bytes());
    let digest = hex::encode(mac.finalize().into_bytes());
    TokenHint {
        kind: kind_of(token),
        tag: digest[..TAG_HEX_DIGITS].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_is_read_from_the_format_marker() {
        assert_eq!(kind_of("wfp_abcdef"), "device token");
        assert_eq!(kind_of("wfr_abcdef"), "API token");
        assert_eq!(kind_of("wfb_abcdef"), "Desk boss link token");
        assert_eq!(kind_of("eyJhbGci.eyJzdWIi.c2ln"), "JWT");
        assert_eq!(kind_of("eyJnotajwt"), "unknown format");
        assert_eq!(kind_of("hunter2"), "unknown format");
        assert_eq!(kind_of(""), "empty");
    }

    #[test]
    fn one_token_has_one_tag_and_two_tokens_two() {
        let a = hint("wfp_aaaaaaaaaaaaaaaa");
        assert_eq!(a, hint("wfp_aaaaaaaaaaaaaaaa"));
        assert_ne!(a.tag, hint("wfp_aaaaaaaaaaaaaaab").tag);
        assert_eq!(a.tag.len(), TAG_HEX_DIGITS);
        assert!(a.tag.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_tag_depends_on_the_key() {
        let one = tag_under(&[1; 32], "wfp_aaaaaaaaaaaaaaaa");
        let other = tag_under(&[2; 32], "wfp_aaaaaaaaaaaaaaaa");
        assert_ne!(one.tag, other.tag, "a hash without the key proves nothing");
    }

    #[test]
    fn what_is_shown_holds_no_part_of_the_token() {
        let token = "wfp_Zq9xK3mP7vL2nR8tY4wB6cD1";
        let shown = hint(token).to_string();
        for width in 3..=token.len() {
            for start in 0..=token.len() - width {
                let piece = &token[start..start + width];
                // The marker is public and the tag is hex: a hex-looking
                // slice of the secret could coincide by chance, so only
                // pieces with a non-hex character are held to this.
                if piece.chars().all(|c| c.is_ascii_hexdigit()) || "wfp_".contains(piece) {
                    continue;
                }
                assert!(!shown.contains(piece), "{shown} shows {piece}");
            }
        }
    }
}
