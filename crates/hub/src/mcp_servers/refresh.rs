//! Periodic `tools/list` refresh per server. Uses the newer
//! `tokio::time::interval` + `MissedTickBehavior::Delay` template
//! (see `crates/hub/src/claude_code_chat.rs::spawn_periodic_*`).
//!
//! Diff semantics: each tick re-fetches the upstream tool list.
//! New tools get registered as `mcp.<server_id>.<name>`; tools the
//! server no longer advertises get unregistered. In-flight calls
//! to a dropped tool complete safely thanks to
//! `ToolRegistry::unregister`'s "Arc lives until last reader
//! finishes" semantic.
//!
//! Failure handling: consecutive errors back off exponentially
//! (1×, 2×, 4×, ...) up to 8× the configured interval before
//! resetting on a successful refresh. Defends against vendor
//! outage spam without losing visibility.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::cost::CostTracker;
use workforce_mcp_client::{McpClient, McpToolDef};

use super::discovery::DiscoveryIdentity;
use super::tool::McpTool;
use super::tool_allowlist::ToolAllowlist;

const MAX_BACKOFF_MULTIPLIER: u32 = 8;

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    server_id: String,
    agent_id: String,
    client: Arc<McpClient>,
    registry: Arc<ToolRegistry>,
    initial_tools: Vec<McpToolDef>,
    interval_secs: u64,
    cost: Option<Arc<CostTracker>>,
    discovery: DiscoveryIdentity,
    allowlist: ToolAllowlist,
) -> tokio::task::JoinHandle<()> {
    let interval_duration = Duration::from_secs(interval_secs);
    tokio::spawn(async move {
        let mut tick = interval(interval_duration);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Skip the immediate first tick — the tools we registered
        // at startup are still fresh.
        tick.tick().await;
        let mut current: HashSet<String> = initial_tools.into_iter().map(|t| t.name).collect();
        let mut backoff: u32 = 1;
        loop {
            tick.tick().await;
            // Filter at fetch time, every tick: a tool newly advertised
            // upstream that the operator denied must stay unregistered, and
            // an over-cap server only stays registerable because the cap
            // counts the kept subset.
            match discovery
                .run(client.tools_list_filtered(|n| allowlist.keeps_name(n)))
                .await
            {
                Ok(latest) => {
                    let new_names: HashSet<String> =
                        latest.iter().map(|t| t.name.clone()).collect();
                    diff_register(
                        &server_id,
                        &agent_id,
                        &client,
                        &registry,
                        &current,
                        &new_names,
                        latest,
                        cost.as_ref(),
                    );
                    current = new_names;
                    backoff = 1;
                }
                Err(e) => {
                    warn!(
                        server = %server_id,
                        error = %e,
                        backoff_multiplier = backoff,
                        "MCP tools/list refresh failed; backing off"
                    );
                    let extra = interval_duration.saturating_mul(backoff.saturating_sub(1));
                    if !extra.is_zero() {
                        tokio::time::sleep(extra).await;
                    }
                    backoff = (backoff.saturating_mul(2)).min(MAX_BACKOFF_MULTIPLIER);
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn diff_register(
    server_id: &str,
    agent_id: &str,
    client: &Arc<McpClient>,
    registry: &Arc<ToolRegistry>,
    previous: &HashSet<String>,
    latest: &HashSet<String>,
    latest_defs: Vec<McpToolDef>,
    cost: Option<&Arc<CostTracker>>,
) {
    let mut added = 0usize;
    for def in latest_defs {
        if previous.contains(&def.name) {
            continue;
        }
        let tool = McpTool::new(server_id, agent_id, Arc::clone(client), def, cost.cloned());
        registry.register(Arc::new(tool));
        added += 1;
    }
    let mut removed = 0usize;
    for gone in previous.difference(latest) {
        let qualified = format!("mcp.{server_id}.{gone}");
        if registry.unregister(&qualified) {
            removed += 1;
        }
    }
    if added > 0 || removed > 0 {
        info!(
            server = %server_id,
            added,
            removed,
            "MCP tool list refreshed"
        );
    }
}
