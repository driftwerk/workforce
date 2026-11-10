//! Minting, listing and revoking Desk boss link tokens, for the paired Desk
//! that holds them and only it.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use workforce_common::settings::AgentWorkspaceConfig;
use workforce_identity::remote_mcp_token::{
    self, DeskLink, RemoteMcpToken, RemoteMcpTokenStore, TokenScope, DEVICE_SCOPE_PREFIX,
};
use workforce_identity::types::IdentityStatus;

use super::engine_key::{EnginePublicKey, KeyError};
use crate::authz::Capability;
use crate::gateway::GatewayState;

/// Held while a device mints, so the per-device cap and a link's
/// replacement hold however many mints race.
static LINK_MINT: std::sync::Mutex<()> = std::sync::Mutex::new(());

const LABEL_MAX_CHARS: usize = 64;
const AGENT_ID_MAX_CHARS: usize = 128;
const GROUP_KEY_MAX_CHARS: usize = 128;
const NAME_MAX_CHARS: usize = 64;
const ENGINE_ID_MAX_CHARS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("This hub's admin hasn't let Desk give its agents boss work.")]
    Disabled,
    #[error("Only an active person here can link a boss.")]
    IdentityNotActive,
    #[error("This hub's admin hasn't let you make its agents your bosses in Desk.")]
    NotAllowed,
    /// Unknown, or not one the person may use: the same words either way.
    #[error("There's no agent here by that name that you can use.")]
    AgentUnreachable,
    #[error("That agent doesn't take boss work from Desk.")]
    AgentDeclines,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    BadKey(KeyError),
    #[error("This Desk already holds {0} boss links here; stop one first.")]
    LinkCap(usize),
    /// `renews` names no live link of this device for the same agent,
    /// group and engine.
    #[error("There's no link of this Desk's to renew by that id.")]
    RenewsUnknown,
    #[error("link store error: {0}")]
    Store(#[from] eyre::Report),
}

/// What Desk asks to link.
#[derive(Debug, Clone)]
pub struct MintRequest {
    pub agent_id: String,
    pub group_key: String,
    pub group_name: String,
    pub engine_id: String,
    pub engine_name: String,
    pub engine_public_key: String,
    /// The live link token this one renews: the link keeps its id, and the
    /// old token stays live until Desk revokes it.
    pub renews: Option<Uuid>,
}

/// A freshly minted link. `plaintext` leaves this struct exactly once, to
/// the minting Desk.
#[derive(Clone)]
pub struct MintedLink {
    pub plaintext: String,
    pub record: RemoteMcpToken,
    pub link: DeskLink,
}

impl std::fmt::Debug for MintedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedLink")
            .field("record", &self.record)
            .finish_non_exhaustive()
    }
}

fn plain(text: &str, max: usize) -> Option<&str> {
    let text = text.trim();
    let chars = text.chars().count();
    ((1..=max).contains(&chars) && !text.chars().any(char::is_control)).then_some(text)
}

/// An engine id is signed into every uplink proof, so it is kept to plain
/// ASCII that reads the same everywhere.
fn engine_id_ok(id: &str) -> bool {
    (1..=ENGINE_ID_MAX_CHARS).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

struct Checked {
    group_key: String,
    group_name: String,
    engine_id: String,
    engine_name: String,
    key: EnginePublicKey,
}

fn check(req: &MintRequest) -> Result<Checked, LinkError> {
    let group_key = plain(&req.group_key, GROUP_KEY_MAX_CHARS).ok_or(LinkError::Invalid(
        "A group's key is 1–128 printable characters.",
    ))?;
    let group_name = plain(&req.group_name, NAME_MAX_CHARS).ok_or(LinkError::Invalid(
        "A group's name is 1–64 printable characters.",
    ))?;
    if !engine_id_ok(&req.engine_id) {
        return Err(LinkError::Invalid(
            "An engine's id is 1–64 letters, digits, '-', '_', '.' or ':'.",
        ));
    }
    let engine_name = plain(&req.engine_name, NAME_MAX_CHARS).ok_or(LinkError::Invalid(
        "An engine's name is 1–64 printable characters.",
    ))?;
    let key = EnginePublicKey::parse(req.engine_public_key.trim()).map_err(LinkError::BadKey)?;
    Ok(Checked {
        group_key: group_key.to_string(),
        group_name: group_name.to_string(),
        engine_id: req.engine_id.clone(),
        engine_name: engine_name.to_string(),
        key,
    })
}

fn store(state: &GatewayState) -> Result<&dyn RemoteMcpTokenStore, LinkError> {
    state
        .remote_mcp_tokens
        .as_deref()
        .ok_or_else(|| LinkError::Store(eyre::eyre!("no token store")))
}

/// Whether `[desk_boss] enabled` lets links be made and used at all.
pub fn require_enabled(state: &GatewayState) -> Result<(), LinkError> {
    state
        .desk_boss
        .enabled
        .then_some(())
        .ok_or(LinkError::Disabled)
}

fn require_active(state: &GatewayState, person: Uuid) -> Result<(), LinkError> {
    state
        .identity_store
        .as_ref()
        .ok_or(LinkError::IdentityNotActive)?
        .get_by_id(person)?
        .filter(|i| i.status == IdentityStatus::Active)
        .map(|_| ())
        .ok_or(LinkError::IdentityNotActive)
}

/// Whether `person` holds [`Capability::DeskBoss`] outright under the live
/// matrix, by their own role (a device acts for them here, not as a member).
pub fn holds_desk_boss(state: &GatewayState, person: Uuid) -> bool {
    crate::authz::holds_capability(
        state.identity_store.as_ref(),
        &person.to_string(),
        Capability::DeskBoss,
    )
}

/// The agent `agent_id` names, when `person` may use it and it accepts boss
/// work in its own `[desk_boss] accept`, read from its config each time so
/// turning it off takes effect at once. Its canonical id.
pub fn boss_agent(state: &GatewayState, person: Uuid, agent_id: &str) -> Result<String, LinkError> {
    if agent_id.trim().is_empty() || agent_id.chars().count() > AGENT_ID_MAX_CHARS {
        return Err(LinkError::AgentUnreachable);
    }
    let service = crate::remote_mcp::access::reachable(state, &person.to_string(), Some(agent_id))
        .map_err(|_| LinkError::AgentUnreachable)?;
    // The registry's own spelling of the id `reachable` resolved.
    let live: Vec<String> = state
        .registry
        .agent_ids()
        .iter()
        .map(|a| a.to_string())
        .collect();
    let id = crate::agent_registry::canonical_agent_id(agent_id.trim(), &live)
        .map(str::to_string)
        .ok_or(LinkError::AgentUnreachable)?;
    let accepts = AgentWorkspaceConfig::load(&service.workspace)
        .ok()
        .flatten()
        .is_some_and(|c| c.accepts_desk_boss());
    if !accepts {
        return Err(LinkError::AgentDeclines);
    }
    Ok(id)
}

/// The live boss links the device `device_id` minted for `person`, newest
/// first.
fn device_links(
    store: &dyn RemoteMcpTokenStore,
    person: Uuid,
    device_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<RemoteMcpToken>, LinkError> {
    Ok(store
        .list_tokens_for_identity(person)?
        .into_iter()
        .filter(|t| {
            t.is_live(now)
                && t.scope() == Some(TokenScope::DeskBoss)
                && t.minted_by_device() == Some(device_id)
        })
        .collect())
}

fn same_link(link: &DeskLink, agent_id: &str, group_key: &str, engine_id: &str) -> bool {
    link.agent_id == agent_id && link.group_key == group_key && link.engine_id == engine_id
}

fn label(agent: &str, group: &str, engine: &str) -> String {
    let full = format!("{agent} bosses {group} on {engine}");
    full.chars().take(LABEL_MAX_CHARS).collect()
}

/// Mints a boss link for `person`, at the request of their paired device
/// `device_id`. Checks, in order: `[desk_boss] enabled`; the person is
/// active and holds [`Capability::DeskBoss`]; they may use the agent and it
/// accepts boss work; what Desk sent is well formed, the engine key
/// included; and the device's live links stay under
/// `[desk_boss] max_links_per_device`.
///
/// A fresh link for an (agent, group, engine) this device already links
/// replaces it, so a device holds one per triple. A renewal keeps the old
/// link's id and leaves its old token live, for Desk to revoke once the
/// engine holds the new one; any older token of the link goes now, so a
/// link never has more than two.
pub fn mint(
    state: &GatewayState,
    person: Uuid,
    device_id: &str,
    req: &MintRequest,
) -> Result<MintedLink, LinkError> {
    require_enabled(state)?;
    require_active(state, person)?;
    if !holds_desk_boss(state, person) {
        return Err(LinkError::NotAllowed);
    }
    let agent_id = boss_agent(state, person, &req.agent_id)?;
    let checked = check(req)?;
    let store = store(state)?;

    let _one = LINK_MINT.lock().unwrap_or_else(|e| e.into_inner());
    let now = Utc::now();
    let held = device_links(store, person, device_id, now)?;
    let is_this = |t: &RemoteMcpToken| {
        t.desk_link
            .as_ref()
            .is_some_and(|l| same_link(l, &agent_id, &checked.group_key, &checked.engine_id))
    };
    let link_id = match req.renews {
        Some(old_id) => {
            let old = held
                .iter()
                .find(|t| t.id == old_id && is_this(t))
                .ok_or(LinkError::RenewsUnknown)?;
            let link_id = old
                .desk_link
                .as_ref()
                .map(|l| l.link_id)
                .unwrap_or_default();
            for stale in held.iter().filter(|t| {
                t.id != old_id && t.desk_link.as_ref().is_some_and(|l| l.link_id == link_id)
            }) {
                store.revoke_token(stale.id, Some(person), now)?;
            }
            link_id
        }
        None => {
            let mut left = Vec::new();
            for t in held {
                if is_this(&t) {
                    store.revoke_token(t.id, Some(person), now)?;
                } else {
                    left.push(t);
                }
            }
            let links: std::collections::HashSet<Uuid> = left
                .iter()
                .filter_map(|t| t.desk_link.as_ref().map(|l| l.link_id))
                .collect();
            let cap = state.desk_boss.link_cap();
            if links.len() >= cap {
                return Err(LinkError::LinkCap(cap));
            }
            Uuid::new_v4()
        }
    };

    let link = DeskLink {
        link_id,
        agent_id: agent_id.clone(),
        group_key: checked.group_key,
        group_name: checked.group_name,
        engine_id: checked.engine_id,
        engine_name: checked.engine_name,
        engine_public_key: checked.key.to_text(),
    };
    let minted = remote_mcp_token::mint_as(TokenScope::DeskBoss);
    let record = RemoteMcpToken {
        id: Uuid::new_v4(),
        identity_id: person,
        label: label(&agent_id, &link.group_name, &link.engine_name),
        token_hash: minted.hash,
        scopes: vec![
            TokenScope::DeskBoss.key().to_string(),
            format!("{DEVICE_SCOPE_PREFIX}{device_id}"),
        ],
        created_at: now,
        expires_at: now + Duration::days(i64::from(state.desk_boss.ttl_days())),
        last_used_at: None,
        revoked_at: None,
        desk_link: Some(link.clone()),
    };
    store.create_token(&record)?;
    audit(
        state,
        person,
        "desk_boss.mint",
        &record,
        &link,
        device_id,
        req.renews.is_some(),
    );
    Ok(MintedLink {
        plaintext: minted.plaintext,
        record,
        link,
    })
}

/// The live boss links the device `device_id` minted for `person`: never
/// any other device's, nor any other kind of token.
pub fn list(
    state: &GatewayState,
    person: Uuid,
    device_id: &str,
) -> Result<Vec<RemoteMcpToken>, LinkError> {
    require_enabled(state)?;
    device_links(store(state)?, person, device_id, Utc::now())
}

/// Revokes the boss link token `id`, when it is a live one the device
/// `device_id` minted for `person`. Anything else is `false`, and nothing
/// says which. Works while `[desk_boss]` is off too, so a link can always
/// be stopped.
pub fn revoke(
    state: &GatewayState,
    person: Uuid,
    device_id: &str,
    id: Uuid,
) -> Result<bool, LinkError> {
    let store = store(state)?;
    let now = Utc::now();
    let Some(token) = device_links(store, person, device_id, now)?
        .into_iter()
        .find(|t| t.id == id)
    else {
        return Ok(false);
    };
    let revoked = store.revoke_token(id, Some(person), now)?;
    if revoked {
        if let Some(link) = &token.desk_link {
            audit(
                state,
                person,
                "desk_boss.revoke",
                &token,
                link,
                device_id,
                false,
            );
        }
    }
    Ok(revoked)
}

/// One audit line and one `audit_log` row: who, which link, which device.
/// Never the token, nor any part of it.
fn audit(
    state: &GatewayState,
    person: Uuid,
    action: &str,
    token: &RemoteMcpToken,
    link: &DeskLink,
    device_id: &str,
    renewal: bool,
) {
    tracing::info!(
        target: "sentinel.audit",
        identity_id = %person,
        token_id = %token.id,
        link_id = %link.link_id,
        agent = %link.agent_id,
        engine = %link.engine_id,
        device = %device_id,
        renewal,
        expires_at = %token.expires_at,
        "{action}"
    );
    if let Some(telemetry) = &state.telemetry_store {
        let detail = serde_json::json!({
            "token_id": token.id,
            "agent_id": link.agent_id,
            "group": link.group_name,
            "engine": link.engine_name,
            "device_id": device_id,
            "renewal": renewal,
        })
        .to_string();
        if let Err(e) = telemetry.record_audit(
            &person.to_string(),
            action,
            &format!("desk_boss_link:{}", link.link_id),
            Some(&detail),
            workforce_common::admin::AuditOutcome::Success,
        ) {
            tracing::warn!(error = %e, "a Desk boss link's audit row wasn't written");
        }
    }
}
