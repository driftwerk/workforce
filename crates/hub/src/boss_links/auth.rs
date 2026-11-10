//! Whether a presented boss link token is live, checked in full on every
//! connect and frame: what the uplink (RB5) asks before it trusts a link,
//! and the one place a `wfb_` token is ever looked up.

use chrono::Utc;
use tracing::warn;

use workforce_identity::remote_mcp_token::{
    self, DeskLink, RemoteMcpToken, TokenScope, DESK_BOSS_TOKEN_PREFIX,
};
use workforce_identity::types::{Identity, IdentityStatus};

use super::engine_key::{self, EnginePublicKey, NONCE_LEN};
use super::links::{self, LinkError};
use crate::gateway::GatewayState;

/// A boss link that is live right now.
pub struct LiveLink {
    pub token: RemoteMcpToken,
    pub link: DeskLink,
    pub person: Identity,
    pub device_id: String,
    /// The bossing agent's canonical id.
    pub agent_id: String,
    pub engine_key: EnginePublicKey,
}

impl std::fmt::Debug for LiveLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveLink")
            .field("token", &self.token)
            .field("person", &self.person.id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl LiveLink {
    /// Whether `signature` proves this link's engine key over the hub's
    /// challenge `nonce` ([`engine_key::verify_challenge`]).
    pub fn proves(&self, nonce: &[u8; NONCE_LEN], signature: &[u8]) -> bool {
        engine_key::verify_challenge(
            &self.engine_key,
            nonce,
            self.token.id,
            &self.link.engine_id,
            signature,
        )
    }
}

/// Why a link is refused. Each reads to the engine in plain words; none
/// says whether a token it doesn't know exists.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkRefusal {
    #[error("this hub doesn't know that boss link, or it has ended")]
    Unauthenticated,
    #[error("the person this boss link is for isn't active here")]
    NotActive,
    #[error("this hub's admin has turned off boss work from Desk")]
    Disabled,
    #[error("this hub's admin no longer lets that person make its agents bosses")]
    NotAllowed,
    #[error("the bossing agent is gone, or no longer the person's to use")]
    AgentGone,
    #[error("the bossing agent no longer takes boss work from Desk")]
    AgentDeclines,
    #[error("this hub couldn't check the boss link; try again")]
    Unavailable,
}

/// The live link `presented` is, checked in full: a `wfb_` token this hub
/// minted, unrevoked and unexpired, of exactly the `desk_boss` kind with
/// its link; its minting device still paired to its person, as the same
/// pairing; the person active; `[desk_boss] enabled`; the person still
/// holding the capability; the agent still theirs to use and still
/// accepting; the registered engine key readable. Fails closed.
pub fn authenticate_link(state: &GatewayState, presented: &str) -> Result<LiveLink, LinkRefusal> {
    if !presented.starts_with(DESK_BOSS_TOKEN_PREFIX) {
        return Err(LinkRefusal::Unauthenticated);
    }
    let (Some(tokens), Some(identities)) = (&state.remote_mcp_tokens, &state.identity_store) else {
        return Err(LinkRefusal::Unavailable);
    };
    let now = Utc::now();
    let token = tokens
        .get_token_by_hash(&remote_mcp_token::hash_token(presented))
        .map_err(|e| {
            warn!(error = %e, "boss link lookup failed — failing closed");
            LinkRefusal::Unavailable
        })?
        .filter(|t| t.is_live(now))
        .ok_or(LinkRefusal::Unauthenticated)?;
    if token.scope() != Some(TokenScope::DeskBoss) {
        return Err(LinkRefusal::Unauthenticated);
    }
    let (Some(link), Some(device_id)) = (token.desk_link.clone(), token.minted_by_device()) else {
        return Err(LinkRefusal::Unauthenticated);
    };
    let device_id = device_id.to_string();
    crate::remote_mcp::auth::device_still_paired(state, &device_id, &token).map_err(
        |e| match e {
            crate::remote_mcp::auth::AuthError::Unavailable => LinkRefusal::Unavailable,
            _ => LinkRefusal::Unauthenticated,
        },
    )?;
    let person = identities
        .get_by_id(token.identity_id)
        .map_err(|e| {
            warn!(error = %e, "identity lookup for a boss link failed — failing closed");
            LinkRefusal::Unavailable
        })?
        .ok_or(LinkRefusal::Unauthenticated)?;
    if person.status != IdentityStatus::Active {
        return Err(LinkRefusal::NotActive);
    }
    links::require_enabled(state).map_err(|_| LinkRefusal::Disabled)?;
    if !links::holds_desk_boss(state, person.id) {
        return Err(LinkRefusal::NotAllowed);
    }
    let agent_id = match links::boss_agent(state, person.id, &link.agent_id) {
        Ok(id) => id,
        Err(LinkError::AgentDeclines) => return Err(LinkRefusal::AgentDeclines),
        Err(_) => return Err(LinkRefusal::AgentGone),
    };
    let engine_key = EnginePublicKey::parse(&link.engine_public_key)
        .map_err(|_| LinkRefusal::Unauthenticated)?;
    Ok(LiveLink {
        token,
        link,
        person,
        device_id,
        agent_id,
        engine_key,
    })
}
