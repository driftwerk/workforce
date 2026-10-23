//! Per-tool, hierarchical permission store for the workforce hub.
//!
//! Implements phase 1 of the User RBAC + Permission Cascade plan: the
//! data model and persistence layer. The cascade evaluator (phase 2)
//! and runtime wiring (phase 3) sit on top of [`store::PermissionStore`].
//!
//! Hierarchy: System → Team → Agent → User. System rules live in the
//! existing `policy-engine` YAML with `constitutional: true`; everything
//! Team / Agent / User lives in this crate's DB-backed store.

pub mod cache;
pub mod capability;
pub mod cascade;
pub mod migration;
pub mod sqlite;
pub mod store;
pub mod types;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub mod postgres_migrate;

pub use cache::{PermissionCache, PermissionEvaluator, DEFAULT_CACHE_CAPACITY};
pub use capability::{
    CapabilityAuditEntry, CapabilityChange, CapabilityDecision, CapabilityOverride,
    CapabilityOverrideStore, CapabilityStore, CustomRole, CustomRoleStore,
};
pub use cascade::{
    evaluate, legal_user_targets, overlay_user_choice, tool_matches, user_choice_is_legal,
    CascadeRequest, SystemRule,
};
pub use store::PermissionStore;
pub use types::{
    ApproverRef, PermissionFilter, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef,
    PermissionRule, PermissionScope, PermissionState,
};
