//! Desk boss links on the boss hub (remote bosses, RB4).
//!
//! A person's paired Desk can make an agent on this hub the boss of one of
//! their Desk groups. Each engine that holds the group's sessions (the Mac's,
//! a machine's) then opens its own uplink here (RB5), carrying one link
//! token per (engine, group, agent).
//!
//! - **The token** is a `desk_boss` kind in the remote MCP token table
//!   ([`TokenScope::DeskBoss`]), prefixed `wfb_`, bound to the person, the
//!   minting device (`device:<id>`) and the agent, with a 30-day life
//!   ([`DeskBossConfig::token_ttl_days`]). It reaches nothing but a boss
//!   link: the remote MCP routes take only `wfr_` remote MCP tokens, and the
//!   REST and WebSocket surfaces don't know either kind.
//! - **Mint, list and revoke** ([`links`]) are device-only, through the
//!   paired Desk's own device token, behind `[desk_boss] enabled` (off by
//!   default), the person's [`Capability::DeskBoss`] and the agent's own
//!   `[desk_boss] accept` (off by default).
//! - **The engine key** ([`engine_key`]): each engine registers its Ed25519
//!   public key at mint, and proves the private half on every connect, so a
//!   copied token is useless alone.
//! - **Unpairing the device** revokes every link it minted
//!   ([`crate::remote_mcp::tokens::revoke_device_tokens`]), and a link whose
//!   device is no longer paired is refused on the spot ([`auth`]).
//!
//! [`TokenScope::DeskBoss`]: workforce_identity::remote_mcp_token::TokenScope::DeskBoss
//! [`DeskBossConfig::token_ttl_days`]: workforce_common::settings::DeskBossConfig::token_ttl_days
//! [`Capability::DeskBoss`]: crate::authz::Capability::DeskBoss

pub mod auth;
pub mod engine_key;
pub mod links;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use uuid::Uuid;
use workforce_common::settings::DeskBossConfig;
use workforce_identity::remote_mcp_token::{RemoteMcpToken, TokenScope};

/// `[desk_boss]` as it stands now: set at boot from the install config, and
/// again whenever the config overlay changes (`cmd: workforce serve`'s
/// overlay watcher), so an admin turning boss work off stops every link on
/// its next frame, without a restart.
#[derive(Clone, Default)]
pub struct LiveConfig(Arc<parking_lot::RwLock<DeskBossConfig>>);

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LiveConfig").field(&*self.0.read()).finish()
    }
}

impl LiveConfig {
    pub fn new(config: DeskBossConfig) -> Self {
        Self(Arc::new(parking_lot::RwLock::new(config)))
    }

    /// The settings now.
    pub fn get(&self) -> DeskBossConfig {
        self.0.read().clone()
    }

    /// Replaces the settings, for every holder of this handle at once.
    pub fn set(&self, config: DeskBossConfig) {
        let mut now = self.0.write();
        if *now != config {
            tracing::info!(enabled = config.enabled, "[desk_boss] changed");
            *now = config;
        }
    }
}

/// The hub's own key for a Desk group: derived from the person, the minting
/// device and the group key that Desk named, so no two people's or two
/// Desks' groups can share one, whatever Desk sends.
pub fn group_key(person: Uuid, device_id: &str, desk_group_key: &str) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(person.as_bytes());
    input.extend_from_slice(&(device_id.len() as u64).to_be_bytes());
    input.extend_from_slice(device_id.as_bytes());
    input.extend_from_slice(desk_group_key.as_bytes());
    blake3::derive_key("workforce desk-boss group key v1", &input)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Why a write of an agent's `config.toml` that changes its `[desk_boss]`
/// is refused, when the agent itself (`agent_tool`) makes it: whether it
/// takes boss work is an admin's to say, never its own.
pub fn desk_boss_refused(before: Option<&str>, body: &str, via: &str) -> Option<String> {
    let desk_boss = |b: Option<&str>| {
        b.and_then(|b| toml::from_str::<toml::Table>(b).ok())
            .and_then(|t| t.get("desk_boss").cloned())
    };
    (via == "agent_tool" && desk_boss(before) != desk_boss(Some(body)))
        .then(|| "Whether this agent takes boss work from Desk is an admin's to set.".to_string())
}

/// One `sentinel.audit` line and one `audit_log` row for a Desk boss link
/// `token` that `actor` ended, as `how` says (`device`, `person`, `admin`,
/// `unpaired`, `device gone`, `replaced`, `renewed`). Never the token, nor
/// any part of it. Nothing for a token of another kind.
pub fn audit_revoke(
    telemetry: Option<&Arc<dyn workforce_common::telemetry::TelemetryStore>>,
    actor: &str,
    token: &RemoteMcpToken,
    how: &str,
) {
    if token.scope() != Some(TokenScope::DeskBoss) {
        return;
    }
    audit(telemetry, actor, "desk_boss.revoke", token, how);
}

/// [`audit_revoke`]'s line and row, for any action on a link.
pub(crate) fn audit(
    telemetry: Option<&Arc<dyn workforce_common::telemetry::TelemetryStore>>,
    actor: &str,
    action: &str,
    token: &RemoteMcpToken,
    how: &str,
) {
    let Some(link) = &token.desk_link else {
        return;
    };
    let device = token.minted_by_device().unwrap_or("");
    tracing::info!(
        target: "sentinel.audit",
        actor = %actor,
        identity_id = %token.identity_id,
        token_id = %token.id,
        link_id = %link.link_id,
        agent = %link.agent_id,
        engine = %link.engine_id,
        device = %device,
        how = %how,
        expires_at = %token.expires_at,
        "{action}"
    );
    let Some(telemetry) = telemetry else {
        return;
    };
    let detail = serde_json::json!({
        "token_id": token.id,
        "identity_id": token.identity_id,
        "agent_id": link.agent_id,
        "group": link.group_name,
        "engine": link.engine_name,
        "device_id": device,
        "how": how,
    })
    .to_string();
    if let Err(e) = telemetry.record_audit(
        actor,
        action,
        &format!("desk_boss_link:{}", link.link_id),
        Some(&detail),
        workforce_common::admin::AuditOutcome::Success,
    ) {
        tracing::warn!(error = %e, "a Desk boss link's audit row wasn't written");
    }
}
