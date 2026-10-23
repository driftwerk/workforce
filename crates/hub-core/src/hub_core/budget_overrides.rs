//! Runtime budget overrides — the SysAdmin-only, audited layer over the
//! config-file `[budget]` ceilings.
//!
//! The `[budget]` TOML table stays outside every runtime write path (the
//! overlay's locked-in test keeps it that way): an agent can never loosen
//! its own leash. What CAN move at runtime is this store — one row per
//! (scope, key), written only through SysAdmin-gated surfaces (the `budget`
//! tool and the admin API), optionally expiring, and appending every change
//! to an audit table. [`crate::hub_core::cost_governor::CostGovernor`] consults it
//! live (5s cache), so a granted boost lands in seconds on every instance
//! sharing the store.

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Serialize;

pub mod sqlite;

#[cfg(feature = "postgres")]
pub mod postgres;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideScope {
    Hub,
    Agent,
    Identity,
}

impl OverrideScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverrideScope::Hub => "hub",
            OverrideScope::Agent => "agent",
            OverrideScope::Identity => "identity",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hub" => Some(OverrideScope::Hub),
            "agent" => Some(OverrideScope::Agent),
            "identity" => Some(OverrideScope::Identity),
            _ => None,
        }
    }
}

/// One live ceiling replacement. `key` is the agent id or identity uuid
/// (empty for `hub`); `daily_usd = 0` means unlimited, matching the config
/// semantics; `expires_at = None` means until cleared.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetOverride {
    pub scope: OverrideScope,
    pub key: String,
    pub daily_usd: f64,
    pub expires_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub created_by: String,
    pub updated_at: DateTime<Utc>,
}

/// Append-only change record — the audit trail the config file used to
/// provide via git history.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideAuditEntry {
    pub action: String,
    pub scope: OverrideScope,
    pub key: String,
    pub daily_usd: Option<f64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub actor: String,
    pub at: DateTime<Utc>,
}

pub trait BudgetOverrideStore: Send + Sync {
    /// Overrides in force at `now` (expired rows filtered out).
    fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<BudgetOverride>>;
    /// Insert or replace the override for `(scope, key)`; audits the change
    /// and prunes rows that expired before `override_row.updated_at`.
    fn upsert(&self, override_row: &BudgetOverride) -> Result<()>;
    /// Remove the override for `(scope, key)`; audited. Returns whether a
    /// row existed.
    fn remove(&self, scope: OverrideScope, key: &str, actor: &str) -> Result<bool>;
    /// Newest-first audit entries, at most `limit`.
    fn audit(&self, limit: usize) -> Result<Vec<OverrideAuditEntry>>;
}

static GLOBAL_STORE: OnceLock<Arc<dyn BudgetOverrideStore>> = OnceLock::new();

/// Install the process-wide store (once, at boot). The governor, the budget
/// tool, and the gateway handlers all read through this.
pub fn set_global_store(store: Arc<dyn BudgetOverrideStore>) {
    if GLOBAL_STORE.set(store).is_err() {
        tracing::warn!("budget override store already installed");
    }
}

pub fn global_store() -> Option<Arc<dyn BudgetOverrideStore>> {
    GLOBAL_STORE.get().cloned()
}
