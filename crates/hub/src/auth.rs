use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

/// Auth token to validate against.
pub struct AuthState {
    pub token: String,
}

/// Constant-time token comparison to prevent timing side-channel attacks.
/// Hashes both tokens with blake3 first to ensure fixed-length comparison.
pub fn constant_time_token_eq(a: &str, b: &str) -> bool {
    // An empty token must never authenticate. Without this guard, an unset /
    // empty configured token (`state.auth_token = ""`) matches a tokenless
    // caller (`eq("", "")` was `true`), collapsing the admin checks in
    // `check_owner_auth`, `ws_local_agent_handler`, etc. to "anyone is admin"
    // (audit F2 — and the dependency under F1's `check_owner_auth` path).
    // Whether a string is empty is not secret, so this early return leaks no
    // timing about the secret's content.
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let hash_a = blake3::hash(a.as_bytes());
    let hash_b = blake3::hash(b.as_bytes());
    hash_a.as_bytes().ct_eq(hash_b.as_bytes()).into()
}

/// Bearer token auth middleware.
pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..];
            if constant_time_token_eq(token, &state.token) {
                Ok(next.run(request).await)
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matching_tokens() {
        assert!(constant_time_token_eq(
            "secret-token-123",
            "secret-token-123"
        ));
    }

    #[test]
    fn constant_time_eq_different_tokens() {
        assert!(!constant_time_token_eq(
            "secret-token-123",
            "secret-token-456"
        ));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_token_eq("short", "a-much-longer-token"));
    }

    #[test]
    fn constant_time_eq_empty_strings_never_match() {
        // audit F2: an empty/unset token must never authenticate — otherwise a
        // tokenless caller matches an unset `auth_token`.
        assert!(!constant_time_token_eq("", ""));
        assert!(!constant_time_token_eq("", "real-token"));
        assert!(!constant_time_token_eq("real-token", ""));
    }
}
