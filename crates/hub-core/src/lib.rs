//! What every part of the hub shares: the agent service, the traits its
//! subsystems implement, the authorization layer and the shared leaf types.

#![recursion_limit = "256"]

pub mod authz;
pub mod cap_gate;
pub mod def_write_auth;
pub mod hosts;
pub mod hub_core;
pub mod resource_access;
pub mod run_steering;
pub mod scope_gate;
pub mod serve;
pub mod skill_access;
pub mod team_reach;
pub mod user_config;
pub mod vault;

pub use hub_core::budget_overrides;
pub use hub_core::decision_records;
pub use hub_core::leader_gate;
pub use hub_core::resolved_config;
pub use hub_core::resource_audit;
pub use hub_core::stop_registry;
pub use hub_core::workspace_admission;
