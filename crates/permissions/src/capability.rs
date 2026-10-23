//! Persisted, admin-editable overrides for the hub's capability matrix.
//!
//! The hub's `authz::decision_for` is the built-in default for every
//! `(role, capability)` cell. This store holds only the cells an admin has
//! changed; an empty store reproduces the built-in matrix exactly. Roles and
//! capabilities are opaque string keys here — their meaning (and validation)
//! lives in the hub's `authz` module, keeping this crate free of any hub
//! dependency. A dedicated generation counter lets the hub poll for changes
//! made out-of-band (CLI break-glass, another replica) without a restart.

use chrono::{DateTime, Utc};
use eyre::Result;
use uuid::Uuid;

/// A capability decision — mirrors the hub's `authz::Decision`. Kept local so
/// the persistence layer carries no hub dependency; the hub maps the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityDecision {
    Allow,
    Ask,
    Deny,
}

impl CapabilityDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    /// Parse a stored key, failing closed to `Deny` for anything unrecognised
    /// (only our own writes reach this column, so it is a defensive default).
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "allow" => Self::Allow,
            "ask" => Self::Ask,
            _ => Self::Deny,
        }
    }
}

/// One stored override: a cell whose decision an admin has set away from (or
/// equal to) the built-in default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityOverride {
    pub role: String,
    pub capability: String,
    pub decision: CapabilityDecision,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<Uuid>,
}

/// A requested change to one cell. `decision = None` clears the override (the
/// cell reverts to its built-in default); `Some(d)` upserts it. The hub
/// decides which by comparing the admin's choice against the built-in default,
/// so this store never needs to know what the defaults are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityChange {
    pub role: String,
    pub capability: String,
    pub decision: Option<CapabilityDecision>,
}

/// One immutable audit record. `old`/`new` are `None` when the cell was (or
/// becomes) the built-in default, so a revert reads as `Some(deny) -> None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAuditEntry {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub actor_id: Option<Uuid>,
    pub role: String,
    pub capability: String,
    pub old_decision: Option<CapabilityDecision>,
    pub new_decision: Option<CapabilityDecision>,
}

/// Persistence for capability overrides. Mirrors [`crate::store::PermissionStore`]
/// in shape: a synchronous trait with SQLite and Postgres implementations on the
/// same backing stores.
pub trait CapabilityOverrideStore: Send + Sync {
    /// Every override currently set. The hub layers these over the built-in
    /// defaults to build the live matrix snapshot.
    fn list_overrides(&self) -> Result<Vec<CapabilityOverride>>;

    /// Apply a batch atomically: upsert/clear each cell, append one audit row
    /// per cell that actually changed, and bump the generation once if any did.
    /// Returns the generation after applying. No-op changes (the new state
    /// equals the current one) are skipped — neither audited nor counted.
    fn apply_changes(&self, changes: &[CapabilityChange], actor: Option<Uuid>) -> Result<u64>;

    /// Monotonic counter bumped on every mutating [`Self::apply_changes`].
    /// Polled by the hub's reload loop to converge out-of-band edits.
    fn current_generation(&self) -> Result<u64>;

    /// Most-recent audit records, newest first, capped at `limit`.
    fn list_audit(&self, limit: usize) -> Result<Vec<CapabilityAuditEntry>>;
}

/// An admin-defined, team-free role. Its `key` is the matrix lookup key (a stable
/// lowercase slug) carried on the identity's `role` column; `name` is the display
/// label. Built-in roles (`member`/`manager`/`system_admin`) are not stored here
/// — only custom additions are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomRole {
    pub id: Uuid,
    pub key: String,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
}

/// Persistence for custom roles. Shares the capability backend so a role and its
/// matrix override cells live in one store.
pub trait CustomRoleStore: Send + Sync {
    /// Every custom role, newest first.
    fn list_custom_roles(&self) -> Result<Vec<CustomRole>>;

    /// Insert a new custom role. Errors if the `key` already exists.
    fn create_custom_role(&self, role: &CustomRole) -> Result<()>;

    /// Update a role's display fields (its `key` is immutable). Returns whether a
    /// row matched.
    fn update_custom_role(&self, id: Uuid, name: &str, description: &str) -> Result<bool>;

    /// Delete a role and, atomically, its capability-override rows (so a later
    /// role reusing the same key can't inherit stale grants), bumping the override
    /// generation. Returns whether a row matched.
    fn delete_custom_role(&self, id: Uuid) -> Result<bool>;

    /// Resolve a custom role by its matrix key.
    fn get_custom_role_by_key(&self, key: &str) -> Result<Option<CustomRole>>;
}

/// The combined capability backend — overrides plus custom roles — so the hub can
/// hold a single handle and call either family without trait upcasting.
pub trait CapabilityStore: CapabilityOverrideStore + CustomRoleStore {}

impl<T: CapabilityOverrideStore + CustomRoleStore> CapabilityStore for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_str_round_trips() {
        for d in [
            CapabilityDecision::Allow,
            CapabilityDecision::Ask,
            CapabilityDecision::Deny,
        ] {
            assert_eq!(CapabilityDecision::from_str_lossy(d.as_str()), d);
        }
    }

    #[test]
    fn decision_unknown_fails_closed_to_deny() {
        assert_eq!(
            CapabilityDecision::from_str_lossy("nonsense"),
            CapabilityDecision::Deny
        );
    }
}
