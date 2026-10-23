//! FreeAgent OAuth provider for `workforce-oauth-client`.
//!
//! FreeAgent authenticates the client with HTTP Basic auth, does not use
//! PKCE, and takes no `scope` parameter (single implicit scope). Endpoints
//! are selected by the `sandbox` flag.

use workforce_oauth_client::{ClientAuth, OAuthProvider};

const PROD_AUTHORIZE: &str = "https://api.freeagent.com/v2/approve_app";
const PROD_TOKEN: &str = "https://api.freeagent.com/v2/token_endpoint";
const PROD_API_BASE: &str = "https://api.freeagent.com/v2";
const SANDBOX_AUTHORIZE: &str = "https://api.sandbox.freeagent.com/v2/approve_app";
const SANDBOX_TOKEN: &str = "https://api.sandbox.freeagent.com/v2/token_endpoint";
const SANDBOX_API_BASE: &str = "https://api.sandbox.freeagent.com/v2";

/// User-Agent FreeAgent requires on every request — including the token
/// endpoint, which is called by the `reqwest::Client` handed to the OAuth
/// client, not by [`super::client::FreeAgentClient`].
pub const USER_AGENT: &str = concat!(
    "workforce-freeagent/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/driftwerk/workforce)"
);

pub struct FreeAgentProvider {
    authorize: &'static str,
    token: &'static str,
    api_base: &'static str,
}

impl FreeAgentProvider {
    pub fn new(sandbox: bool) -> Self {
        if sandbox {
            Self {
                authorize: SANDBOX_AUTHORIZE,
                token: SANDBOX_TOKEN,
                api_base: SANDBOX_API_BASE,
            }
        } else {
            Self {
                authorize: PROD_AUTHORIZE,
                token: PROD_TOKEN,
                api_base: PROD_API_BASE,
            }
        }
    }

    pub fn api_base(&self) -> &'static str {
        self.api_base
    }
}

impl OAuthProvider for FreeAgentProvider {
    fn name(&self) -> &str {
        "freeagent"
    }
    fn authorization_endpoint(&self) -> &str {
        self.authorize
    }
    fn token_endpoint(&self) -> &str {
        self.token
    }
    fn default_scopes(&self) -> &[&str] {
        &[]
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
    fn sandbox_selects_sandbox_endpoints() {
        let p = FreeAgentProvider::new(true);
        assert_eq!(p.authorization_endpoint(), SANDBOX_AUTHORIZE);
        assert_eq!(p.token_endpoint(), SANDBOX_TOKEN);
        assert_eq!(p.api_base(), SANDBOX_API_BASE);
    }

    #[test]
    fn production_is_the_default_environment() {
        let p = FreeAgentProvider::new(false);
        assert_eq!(p.authorization_endpoint(), PROD_AUTHORIZE);
        assert_eq!(p.token_endpoint(), PROD_TOKEN);
        assert_eq!(p.api_base(), PROD_API_BASE);
    }

    #[test]
    fn freeagent_uses_basic_auth_and_no_pkce() {
        let p = FreeAgentProvider::new(false);
        assert_eq!(p.client_auth(), ClientAuth::Basic);
        assert!(!p.uses_pkce());
        assert!(p.default_scopes().is_empty());
    }
}
