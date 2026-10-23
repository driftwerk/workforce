//! The scope a vault tool runs in.

use serde::Deserialize;
use workforce_common::settings::VaultSource;

/// Which tier `vault.set` writes to (and which `vault.get`/`vault.move`
/// target). Mirrors [`workforce_common::settings::VaultSource`] but is the
/// LLM-facing surface — it carries serde aliases that preserve the
/// pre-2026-05 scope names so existing prompts/transcripts keep working.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VaultScope {
    AgentPerUser,
    #[serde(alias = "user")]
    GlobalPerUser,
    #[serde(alias = "agent_shared")]
    AgentSpecific,
    Global,
}

impl VaultScope {
    pub fn to_tier(self) -> VaultSource {
        match self {
            VaultScope::AgentPerUser => VaultSource::AgentPerUser,
            VaultScope::GlobalPerUser => VaultSource::GlobalPerUser,
            VaultScope::AgentSpecific => VaultSource::AgentSpecific,
            VaultScope::Global => VaultSource::Global,
        }
    }
}
