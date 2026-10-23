//! Channel persona overrides — a hot, channel-scoped system-prompt + response-mode
//! layer that makes a bound agent converse in a watched channel.
//!
//! This is a *derived* projection of the enabled `persona` monitors: the monitor
//! engine rebuilds it on every reload via [`PersonaOverrides::replace_all`]. It is
//! deliberately NOT vault-persisted (the monitor defs are the source of truth) and
//! is consulted by the channel adapter at message time, so changes take effect
//! without a restart. Precedence at the adapter: a user's runtime `set_mode`
//! (vault-persisted [`crate::engagement::EngagementOverrides`]) wins over a
//! persona, which wins over the static channel config, which wins over the default.

use std::collections::HashMap;

use tokio::sync::RwLock;

use crate::settings::ResponseMode;

/// One channel's persona: the system prompt the bound agent should use, the
/// response mode, and which agent the monitor named (for diagnostics).
#[derive(Debug, Clone)]
pub struct ChannelPersona {
    pub system_prompt: String,
    pub mode: ResponseMode,
    pub agent: String,
    pub monitor_id: String,
    /// Mirrors the monitor binding's `exclude_thread_replies`: when set, the
    /// bound agent does not respond to messages posted as thread replies in this
    /// channel (it still engages with top-level messages and thread roots).
    pub exclude_thread_replies: bool,
}

/// The live channel→persona map, rebuilt from monitor defs on reload.
#[derive(Default)]
pub struct PersonaOverrides {
    map: RwLock<HashMap<String, ChannelPersona>>,
}

impl PersonaOverrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// The persona for a channel, if any.
    pub async fn resolve(&self, channel: &str) -> Option<ChannelPersona> {
        self.map.read().await.get(channel).cloned()
    }

    /// Replace the whole map — the engine calls this after every reload so a
    /// disabled/deleted persona is dropped and edits take effect at once.
    pub async fn replace_all(&self, next: HashMap<String, ChannelPersona>) {
        *self.map.write().await = next;
    }

    /// Channels currently bound to a persona (for diagnostics / logging).
    pub async fn channels(&self) -> Vec<String> {
        self.map.read().await.keys().cloned().collect()
    }
}
