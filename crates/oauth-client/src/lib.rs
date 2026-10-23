//! Generic OAuth 2.1 + PKCE client primitives shared by every workforce
//! provider that needs a redirect-and-callback flow (Google, Linear,
//! Atlassian, future MCP servers).
//!
//! Lifted from the original Google adapter (`crates/skills/src/adapters/
//! google/{oauth.rs, token_store.rs, web_flow.rs}`) and made
//! provider-agnostic. The Google adapter now implements
//! [`OAuthProvider`] and reuses the same primitives.
//!
//! Also fixes a pre-existing bug where rotated refresh tokens (mandated
//! by OAuth 2.1 for many providers — Linear, Atlassian) were silently
//! dropped during refresh: [`RefreshResult::refresh_token`] is `Option`
//! and [`TokenStore::access_token`] persists it when present.

mod discovery;
mod error;
mod flow;
mod pkce;
mod provider;
mod registration;
mod state_store;
mod token_store;

pub use discovery::{discover, AuthServerMetadata};
pub use error::OAuthError;
pub use flow::{OAuthClient, OAuthClientCreds, RefreshResult, StoredTokens, WebFlowStart};
pub use pkce::{code_challenge_s256, gen_random_url_safe};
pub use provider::{ClientAuth, DiscoveredProvider, OAuthProvider};
pub use registration::{register_client, ClientRegistrationRequest, ClientRegistrationResponse};
pub use state_store::{
    InMemoryOAuthStateStore, OAuthStateStore, SqliteOAuthStateStore, StateEntry, STATE_TTL,
};
pub use token_store::{
    Clock, PathLayout, SystemClock, TokenStatus, TokenStore, REFRESH_SLACK_SECS,
};
