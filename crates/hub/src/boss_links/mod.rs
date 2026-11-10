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
