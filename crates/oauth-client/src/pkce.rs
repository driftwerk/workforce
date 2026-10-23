use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 32 random bytes, base64url-encoded (no padding). Used as both PKCE
/// code-verifier and as opaque OAuth `state` tokens. Per RFC 7636 the
/// verifier must be 43-128 chars from the `[A-Z][a-z][0-9]-._~` set;
/// this satisfies that.
pub fn gen_random_url_safe() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256 challenge: `BASE64URL(SHA256(verifier))`.
pub fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_is_url_safe_and_long_enough() {
        let v = gen_random_url_safe();
        assert!(v.len() >= 43, "verifier too short: {} chars", v.len());
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')));
    }

    #[test]
    fn challenge_is_deterministic_s256() {
        let v = "test-verifier";
        let c1 = code_challenge_s256(v);
        let c2 = code_challenge_s256(v);
        assert_eq!(c1, c2);
        assert_ne!(c1, v);
    }

    #[test]
    fn two_verifiers_differ() {
        assert_ne!(gen_random_url_safe(), gen_random_url_safe());
    }
}
