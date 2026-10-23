//! Spotify OAuth provider for `workforce-oauth-client`.
//!
//! Spotify authenticates the client with HTTP Basic auth at the token
//! endpoint, and does not need PKCE for a confidential app — the hub holds the
//! client secret, so the code-interception PKCE defends against cannot happen.

use workforce_oauth_client::{ClientAuth, OAuthProvider};

const AUTHORIZE: &str = "https://accounts.spotify.com/authorize";
const TOKEN: &str = "https://accounts.spotify.com/api/token";
const API_BASE: &str = "https://api.spotify.com/v1";

/// Everything the playback tools need, and nothing else.
///
/// Asked for once, at connection time: adding a scope later means sending the
/// person back through the whole authorization flow, so the read scopes are
/// requested even though only the write scope is strictly required to start
/// music. `user-read-playback-state` is what makes "what's playing?" and
/// device selection possible; without it the tools can act but never report.
const SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
];

pub struct SpotifyProvider;

impl SpotifyProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn api_base(&self) -> &'static str {
        API_BASE
    }
}

impl Default for SpotifyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for SpotifyProvider {
    fn name(&self) -> &str {
        "spotify"
    }
    fn authorization_endpoint(&self) -> &str {
        AUTHORIZE
    }
    fn token_endpoint(&self) -> &str {
        TOKEN
    }
    fn default_scopes(&self) -> &[&str] {
        SCOPES
    }
    fn uses_pkce(&self) -> bool {
        false
    }
    fn client_auth(&self) -> ClientAuth {
        ClientAuth::Basic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_uses_basic_auth_without_pkce() {
        let p = SpotifyProvider::new();
        assert_eq!(p.client_auth(), ClientAuth::Basic);
        assert!(!p.uses_pkce());
    }

    /// Scopes cannot be widened without sending the user through authorization
    /// again, so the set is pinned rather than trusted to stay right.
    #[test]
    fn the_scopes_cover_reading_and_controlling_playback() {
        let provider = SpotifyProvider::new();
        let scopes = provider.default_scopes();
        assert!(
            scopes.contains(&"user-modify-playback-state"),
            "cannot control playback"
        );
        assert!(
            scopes.contains(&"user-read-playback-state"),
            "cannot see devices"
        );
    }

    #[test]
    fn endpoints_are_spotifys_accounts_service() {
        let p = SpotifyProvider::new();
        assert!(p
            .authorization_endpoint()
            .starts_with("https://accounts.spotify.com/"));
        assert!(p
            .token_endpoint()
            .starts_with("https://accounts.spotify.com/"));
        assert_eq!(p.api_base(), "https://api.spotify.com/v1");
    }
}
