#[path = "api/support.rs"]
mod support;

#[path = "api/admin_tests.rs"]
mod admin_tests;

#[path = "api/admin_scope_routes.rs"]
mod admin_scope_routes;

#[path = "api/agent_rename_routes.rs"]
mod agent_rename_routes;

#[path = "api/agent_config_routes.rs"]
mod agent_config_routes;

#[path = "api/authz_routes.rs"]
mod authz_routes;

#[path = "api/caller_identity_routes.rs"]
mod caller_identity_routes;

#[cfg(feature = "postgres")]
#[path = "api/desk_conversation_routes.rs"]
mod desk_conversation_routes;

#[path = "api/eval_routes.rs"]
mod eval_routes;

#[path = "api/login_store_routes.rs"]
mod login_store_routes;

#[path = "api/monitor_routes.rs"]
mod monitor_routes;

#[path = "api/pair_routes.rs"]
mod pair_routes;

#[path = "api/schedule_routes.rs"]
mod schedule_routes;

#[path = "api/session_routes.rs"]
mod session_routes;

#[path = "api/settings_routes.rs"]
mod settings_routes;

#[path = "api/stats_routes.rs"]
mod stats_routes;

#[path = "api/trigger_routes.rs"]
mod trigger_routes;

#[path = "api/slack_routes.rs"]
mod slack_routes;

#[path = "api/lifecycle_routes.rs"]
mod lifecycle_routes;

#[path = "api/decision_routes.rs"]
mod decision_routes;

#[path = "api/ws_routes.rs"]
mod ws_routes;

#[path = "api/node_routes.rs"]
mod node_routes;

#[cfg(feature = "postgres")]
#[path = "api/desk_floor_routes.rs"]
mod desk_floor_routes;
