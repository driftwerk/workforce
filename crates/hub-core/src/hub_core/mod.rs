//! What every part of the hub shares with `AgentService`: the shapes the
//! subsystems show it, and the infrastructure it holds that nothing else
//! owns, held here so the service and its traits name no subsystem. Each
//! subsystem re-exports its own from where it lived.

pub mod access_gate;
pub mod approvals;
pub mod broadcast;
pub mod budget_overrides;
pub mod capabilities;
pub mod cc_budget;
pub mod claims;
pub mod coding_sessions;
pub mod cost_forecast;
pub mod cost_governor;
pub mod curation;
pub mod decision_records;
pub mod environments;
pub mod formatting;
pub mod fs_watcher;
pub mod grants;
pub mod heartbeat_signature;
pub mod heartbeats;
pub mod jobs;
pub mod layout;
pub mod leader_gate;
pub mod live_turns;
pub mod local_agent_registry;
pub mod logins;
pub mod memory;
pub mod monitors;
pub mod origins;
pub mod owners;
pub mod permissions_audit;
pub mod pipelines;
pub mod playbooks;
pub mod pools;
pub mod profile;
pub mod recipients;
pub mod resolved_config;
pub mod resource_audit;
pub mod roles;
pub mod schedules;
pub mod session_citations;
pub mod skill_builds;
pub mod skill_state;
pub mod slot;
pub mod standing;
pub mod stop_registry;
pub mod subscriptions;
pub mod tool_loading;
pub mod turn_journal;
pub mod vault_audit;
pub mod vault_scopes;
pub mod workflows;
pub mod workspace_admission;
