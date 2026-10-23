//! Microsoft Teams adapter (Bot Framework).
//!
//! Unlike Slack's socket-mode transport, Teams **pushes** events: the Azure Bot
//! Connector calls the hub's public `POST /api/messages`, which authenticates
//! the request ([`auth`]), parses the [`types::Activity`], and routes it through
//! [`event_handler`]. Outbound replies and proactive messages go back out via
//! the Bot Framework REST connector (later increment).
//!
//! Module layout deliberately mirrors the Slack adapter so the two stay legible
//! side by side:
//! - [`config`]        — `TeamsSkillConfig` (apps, agent bindings)
//! - [`types`]         — Bot Framework Activity wire types
//! - [`event_handler`] — `Activity` → [`event_handler::TeamsDisposition`]
//! - [`auth`]          — Bot Connector JWT validation
//!
//!

pub mod auth;
pub mod client;
pub mod config;
pub mod event_handler;
pub mod surface;
pub mod types;

pub use auth::BotConnectorValidator;
pub use client::BotFrameworkClient;
pub use config::{TeamsAppConfig, TeamsSkillConfig};
pub use event_handler::{
    InboundMessage, InvokeActivity, TeamsAction, TeamsDisposition, TeamsEventHandler, TeamsIdentity,
};
pub use surface::TeamsSurface;
pub use types::{
    Activity, ChannelAccount, ConversationAccount, ConversationReference, InvokeResponse,
};
