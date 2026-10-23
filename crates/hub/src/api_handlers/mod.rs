//! HTTP API handlers for Config, Policy, Engagement Mode, and Memory Browser
//! management (DW-425).
//!
//! Each sub-module wraps existing internal agent tools with HTTP-accessible
//! Axum handlers. The routes are registered in [`crate::gateway::build_router`].

pub mod budget;
pub mod channel_display;
pub mod config;
pub mod engagement;
pub mod environments;
pub mod errors;
pub mod evals;
pub mod external_skills;
pub mod identity;
pub mod identity_display;
pub mod identity_lifecycle;
pub mod know_how;
pub mod lessons;
pub mod memory;
pub mod my_settings;
pub mod owned_skills;
pub mod permissions;
pub mod policy;
pub mod relationship;
pub mod remote_mcp_tokens;
pub mod settings;
pub mod stats;
pub mod subscriptions;
#[cfg(test)]
mod tests;
pub mod wasm_skills;

use std::sync::Arc;

use crate::gateway::GatewayState;
use crate::serve::AgentService;

use errors::ApiError;

// ── Shared auth helpers ───────────────────────────────────────────────────
//
// The scope-aware predicate `crate::gateway::require_scope` is the entry
// point for read endpoints (it threads identity, scope, and 401/403
// shaping in one call). `check_owner_auth` remains for write endpoints
// that must enforce the stricter SystemAdmin-role gate, which is not
// expressible through scopes alone.

/// Check owner-only authentication for write endpoints — admin token only (constant-time).
pub(crate) fn check_owner_auth(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> bool {
    crate::gateway::check_owner_auth(state, headers, query_token)
}

/// Resolve an agent service by ID, returning a structured ApiError on failure.
pub(crate) fn resolve_agent(state: &GatewayState, id: &str) -> Result<Arc<AgentService>, ApiError> {
    state
        .registry
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("agent '{}' not found", id)))
}
