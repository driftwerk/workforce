//! MCP **workspaces**: grouping several MCP servers (one per environment —
//! e.g. `harbor-staging`, `harbor-prod`) under one logical workspace
//! (`harbor`) so that exactly one environment is reachable at a time and a
//! non-default one (staging) is never auto-reached for prod work.
//!
//! A server with no workspace is **ungrouped** and always active — identical
//! to every install made before workspaces existed. Putting a server into a
//! workspace ([`set_member`]) opts it into gating: its tools are callable only
//! when its `env` is the workspace's *active* env, which is
//!   1. a per-session `cmd: use <ws> <env>` override (held in the
//!      [`crate::runner::ACTIVE_MCP_ENVS`] task-local), else
//!   2. the workspace's `default` member, else
//!   3. nothing — every member is **dormant** (the safe empty state: a
//!      workspace whose `default` (prod) is not yet configured exposes no
//!      tools at all until a human explicitly activates one).
//!
//! This module is the single source of truth consulted from two places:
//! - the runner's tool plan drops a dormant member's tools before anything
//!   is offered or catalogued, for every model and the pre-curated worker
//!   path (so a worker can't reach a dormant env either);
//! - the MCP call path (`McpTool::execute` in the hub) rejects a dormant call
//!   as defence-in-depth, so a stale tool name left in history can't fire.
//!
//! The registry is a process-global keyed by `server_id`; MCP tool names are
//! themselves process-global (`mcp_<server_id>_*`), so `server_id` uniqueness
//! per process is already assumed by the tool registry.

use std::collections::HashMap;
use std::sync::RwLock;

use once_cell::sync::Lazy;

/// One server's membership in a workspace.
#[derive(Debug, Clone)]
struct Membership {
    workspace: String,
    env: String,
    default: bool,
    /// The LLM-normalized tool-name prefix this server's tools share
    /// (`mcp_<server_id>_`, dots → underscores per `to_llm_name`). Stored so
    /// the tool filter can match by prefix without re-deriving it.
    llm_prefix: String,
}

static MEMBERS: Lazy<RwLock<HashMap<String, Membership>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// LLM-normalized tool prefix for a server: `mcp_<server_id>_` with dots
/// turned into underscores (matching `crate::runner::to_llm_name`).
fn llm_prefix_for(server_id: &str) -> String {
    format!("mcp_{}_", server_id.replace('.', "_"))
}

/// Record (or update) a server's workspace membership. Called from the hub at
/// server start/install. `env` empty is tolerated but means the member can
/// never be active (it has no env to match) — callers validate non-empty.
pub fn set_member(server_id: &str, workspace: &str, env: &str, default: bool) {
    if let Ok(mut members) = MEMBERS.write() {
        members.insert(
            server_id.to_string(),
            Membership {
                workspace: workspace.to_string(),
                env: env.to_string(),
                default,
                llm_prefix: llm_prefix_for(server_id),
            },
        );
    }
}

/// Whether any workspace member is registered at all. Lets the tool filter
/// skip the per-tool visibility scan (and its allocation) entirely on the
/// common path where no install uses workspaces.
pub fn any_workspaces() -> bool {
    MEMBERS.read().map(|m| !m.is_empty()).unwrap_or(false)
}

/// Drop a server's membership (uninstall, or re-grouping to ungrouped). No-op
/// if it wasn't a member — an ungrouped server is always active.
pub fn clear_member(server_id: &str) {
    if let Ok(mut members) = MEMBERS.write() {
        members.remove(server_id);
    }
}

/// Set `default = (env == default_env)` for every member of `workspace`, so at
/// most one member is the auto-active default. Returns the server_ids whose
/// flag changed (the caller persists those records). Human-gated upstream
/// (`cmd: default`).
pub fn set_workspace_default(workspace: &str, default_env: &str) -> Vec<String> {
    let mut changed = Vec::new();
    if let Ok(mut members) = MEMBERS.write() {
        for (server_id, m) in members.iter_mut() {
            if m.workspace != workspace {
                continue;
            }
            let should = m.env == default_env;
            if m.default != should {
                m.default = should;
                changed.push(server_id.clone());
            }
        }
    }
    changed
}

/// The active env for a workspace given a set of session overrides: the
/// override wins, else the `default` member's env, else `None` (all dormant).
fn active_env_with(
    members: &HashMap<String, Membership>,
    workspace: &str,
    overrides: &HashMap<String, String>,
) -> Option<String> {
    if let Some(env) = overrides.get(workspace) {
        return Some(env.clone());
    }
    // The first default by name: two variants both marked default must not
    // make the active one follow the map's iteration order.
    members
        .values()
        .filter(|m| m.workspace == workspace && m.default)
        .map(|m| m.env.as_str())
        .min()
        .map(str::to_string)
}

/// The member (if any) whose tool prefix matches `name`, choosing the LONGEST
/// matching prefix so a nested `server_id` (e.g. `harbor` vs
/// `harbor_staging`) resolves to the right one.
fn member_for_tool<'a>(
    members: &'a HashMap<String, Membership>,
    name: &str,
) -> Option<&'a Membership> {
    members
        .values()
        .filter(|m| name.starts_with(&m.llm_prefix))
        .max_by_key(|m| m.llm_prefix.len())
}

/// Whether an LLM-form tool name (`mcp_<server_id>_<tool>`) should be visible
/// to the model this turn. Ungrouped/non-MCP tools are always visible; a
/// workspace member's tools are visible only when its env is the workspace's
/// active env for this session. Reads the [`crate::runner::ACTIVE_MCP_ENVS`]
/// task-local, so a sub-agent (which never scopes it) sees only defaults.
pub fn is_tool_visible(name: &str) -> bool {
    let members = match MEMBERS.read() {
        Ok(m) => m,
        Err(_) => return true,
    };
    if members.is_empty() {
        return true;
    }
    let Some(m) = member_for_tool(&members, name) else {
        return true;
    };
    let overrides = crate::runner::current_active_mcp_envs();
    active_env_with(&members, &m.workspace, &overrides).as_deref() == Some(m.env.as_str())
}

/// Whether a server (by `server_id`) is reachable this turn — the call-path
/// guard. Ungrouped servers are always active; a workspace member is active
/// only when its env is the workspace's active env for this session.
pub fn is_server_active(server_id: &str) -> bool {
    let members = match MEMBERS.read() {
        Ok(m) => m,
        Err(_) => return true,
    };
    let Some(m) = members.get(server_id) else {
        return true;
    };
    let overrides = crate::runner::current_active_mcp_envs();
    active_env_with(&members, &m.workspace, &overrides).as_deref() == Some(m.env.as_str())
}

/// Agent-facing explanation returned when a dormant member's tool is called.
/// Tells the agent it cannot switch environments itself — a human must.
pub fn dormant_call_message(server_id: &str) -> String {
    let members = match MEMBERS.read() {
        Ok(m) => m,
        Err(_) => return format!("`{server_id}` is not reachable right now."),
    };
    let Some(m) = members.get(server_id) else {
        return format!("`{server_id}` is not reachable right now.");
    };
    let overrides = crate::runner::current_active_mcp_envs();
    let active = active_env_with(&members, &m.workspace, &overrides);
    let state = match active {
        Some(a) => format!("is currently pointed at `{a}`"),
        None => "has no environment active (no default set)".to_string(),
    };
    format!(
        "MCP workspace `{ws}` {state}, so the `{env}` server `{server_id}` is dormant and its \
         tools are not callable. You cannot switch environments yourself — ask a human to run \
         `cmd: use {ws} {env}` (it points THIS session at `{env}`), then retry.",
        ws = m.workspace,
        env = m.env,
        server_id = server_id,
        state = state,
    )
}

/// One member's row in a [`WorkspaceView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMemberView {
    pub env: String,
    pub server_id: String,
    pub default: bool,
    pub active: bool,
}

/// A workspace and the activation state of its members for this session.
/// Used to render the per-turn context note and the `mcp.workspace_list` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceView {
    pub workspace: String,
    pub active_env: Option<String>,
    pub members: Vec<WorkspaceMemberView>,
}

/// Snapshot of every workspace and its members' activation state given the
/// turn's [`crate::runner::ACTIVE_MCP_ENVS`] overrides. Sorted by workspace,
/// then env, for stable output.
pub fn workspace_summary() -> Vec<WorkspaceView> {
    workspace_summary_with(&crate::runner::current_active_mcp_envs())
}

/// Like [`workspace_summary`] but with explicit overrides — used where the
/// session's `mcp_active_envs` are known but the task-local isn't yet scoped
/// (e.g. per-turn prompt assembly).
pub fn workspace_summary_with(overrides: &HashMap<String, String>) -> Vec<WorkspaceView> {
    let members = match MEMBERS.read() {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    // Group (server_id, env, default) rows by workspace.
    let mut by_ws: HashMap<String, Vec<(String, String, bool)>> = HashMap::new();
    for (sid, m) in members.iter() {
        by_ws
            .entry(m.workspace.clone())
            .or_default()
            .push((sid.clone(), m.env.clone(), m.default));
    }
    let mut views: Vec<WorkspaceView> = by_ws
        .into_iter()
        .map(|(workspace, mut rows)| {
            let active_env = active_env_with(&members, &workspace, overrides);
            rows.sort_by(|a, b| a.1.cmp(&b.1));
            let member_views = rows
                .into_iter()
                .map(|(server_id, env, default)| WorkspaceMemberView {
                    active: active_env.as_deref() == Some(env.as_str()),
                    env,
                    server_id,
                    default,
                })
                .collect();
            WorkspaceView {
                workspace,
                active_env,
                members: member_views,
            }
        })
        .collect();
    views.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    views
}

/// Whether any server is grouped under `workspace` at all (independent of which
/// variant). Lets the unified `cmd: use <name>` front door tell a workspace name
/// apart from an environment name before deciding how to dispatch.
pub fn workspace_exists(workspace: &str) -> bool {
    MEMBERS
        .read()
        .map(|members| members.values().any(|m| m.workspace == workspace))
        .unwrap_or(false)
}

/// Whether `workspace` exists and has a member with `env` — used by `cmd: use`
/// / `cmd: default` to reject typos with a helpful list instead of silently
/// activating a non-existent env.
pub fn workspace_has_env(workspace: &str, env: &str) -> bool {
    MEMBERS
        .read()
        .map(|members| {
            members
                .values()
                .any(|m| m.workspace == workspace && m.env == env)
        })
        .unwrap_or(false)
}

/// Test-only: clear the registry so unit tests don't leak state into each
/// other (the registry is a process-global).
#[doc(hidden)]
pub fn clear_all_for_test() {
    if let Ok(mut members) = MEMBERS.write() {
        members.clear();
    }
}

/// Test-only: a process-wide lock serializing every test that mutates the
/// global member registry — shared across this module's tests AND the
/// runner's tool-plan tests so they don't stomp each other. Acquire it
/// (poison-tolerant), then clear, then populate.
#[cfg(test)]
pub(crate) static TEST_REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only: acquire [`TEST_REGISTRY_LOCK`] and clear the registry.
#[cfg(test)]
pub(crate) fn lock_and_clear_for_test() -> std::sync::MutexGuard<'static, ()> {
    let g = TEST_REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_all_for_test();
    g
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ACTIVE_MCP_ENVS;

    // The registry is a process-global; the test binary runs tests on multiple
    // threads. `lock_and_clear_for_test` serializes every test that touches it
    // (shared with the runner's tool-plan tests) and clears it.
    use super::lock_and_clear_for_test as guard;

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn ungrouped_server_always_active_and_visible() {
        let _g = guard();
        // No members registered at all.
        assert!(is_server_active("linear"));
        assert!(is_tool_visible("mcp_linear_issue_create"));
        assert!(is_tool_visible("filesystem_read"));
    }

    #[test]
    fn lone_nondefault_member_is_dormant() {
        let _g = guard();
        // Only staging exists, not marked default → must NOT be auto-reached.
        set_member("harbor-staging", "harbor", "staging", false);
        assert!(!is_server_active("harbor-staging"));
        assert!(!is_tool_visible("mcp_harbor-staging_get_order"));
    }

    #[test]
    fn default_member_is_auto_active() {
        let _g = guard();
        set_member("harbor-prod", "harbor", "prod", true);
        set_member("harbor-staging", "harbor", "staging", false);
        // prod is default → active without any override; staging dormant.
        assert!(is_server_active("harbor-prod"));
        assert!(is_tool_visible("mcp_harbor-prod_get_order"));
        assert!(!is_server_active("harbor-staging"));
        assert!(!is_tool_visible("mcp_harbor-staging_get_order"));
    }

    #[test]
    fn session_override_activates_nondefault_and_deactivates_default() {
        let _g = guard();
        set_member("harbor-prod", "harbor", "prod", true);
        set_member("harbor-staging", "harbor", "staging", false);
        ACTIVE_MCP_ENVS.sync_scope(overrides(&[("harbor", "staging")]), || {
            // override wins: staging active, prod (the default) now dormant.
            assert!(is_server_active("harbor-staging"));
            assert!(is_tool_visible("mcp_harbor-staging_get_order"));
            assert!(!is_server_active("harbor-prod"));
            assert!(!is_tool_visible("mcp_harbor-prod_get_order"));
        });
    }

    #[test]
    fn empty_default_means_nothing_active() {
        let _g = guard();
        // prod not configured yet; staging present but not default.
        set_member("harbor-staging", "harbor", "staging", false);
        // No override, no default → all dormant (the safe empty state).
        assert!(!is_server_active("harbor-staging"));
    }

    #[test]
    fn subagent_without_scope_sees_only_default() {
        let _g = guard();
        set_member("harbor-prod", "harbor", "prod", true);
        set_member("harbor-staging", "harbor", "staging", false);
        // No ACTIVE_MCP_ENVS scope at all (the run_isolated case): default only.
        let envs = crate::runner::current_active_mcp_envs();
        assert!(envs.is_empty());
        assert!(is_server_active("harbor-prod"));
        assert!(!is_server_active("harbor-staging"));
    }

    #[test]
    fn empty_inner_scope_defeats_inherited_override() {
        // Models the run_isolated guarantee: a parent turn pinned to staging,
        // then an inner EMPTY ACTIVE_MCP_ENVS scope (what run_isolated applies)
        // — the child must revert to the workspace default (prod), NOT inherit
        // staging. This is the mechanism that stops a human's `cmd: use
        // staging` leaking into a spawned sub-investigation, and it holds even
        // if run_isolated were ever awaited inline inside the parent's scope.
        let _g = guard();
        set_member("harbor-prod", "harbor", "prod", true);
        set_member("harbor-staging", "harbor", "staging", false);
        let parent = overrides(&[("harbor", "staging")]);
        ACTIVE_MCP_ENVS.sync_scope(parent, || {
            assert!(is_server_active("harbor-staging")); // parent on staging
            ACTIVE_MCP_ENVS.sync_scope(HashMap::new(), || {
                // child: empty override → default (prod) active, staging dormant.
                assert!(is_server_active("harbor-prod"));
                assert!(!is_server_active("harbor-staging"));
            });
        });
    }

    #[test]
    fn set_workspace_default_flips_exactly_one() {
        let _g = guard();
        set_member("harbor-prod", "harbor", "prod", true);
        set_member("harbor-staging", "harbor", "staging", false);
        let changed = set_workspace_default("harbor", "staging");
        assert_eq!(changed.len(), 2); // prod off, staging on
        assert!(is_server_active("harbor-staging"));
        assert!(!is_server_active("harbor-prod"));
    }

    #[test]
    fn longest_prefix_disambiguates_nested_server_ids() {
        let _g = guard();
        // server_ids where one's prefix nests inside the other's.
        set_member("patch", "a", "x", true);
        set_member("patch_extra", "b", "y", true);
        // a/x and b/y both default+active here; ensure the tool maps to the
        // right member, not the shorter-prefix one.
        assert!(is_tool_visible("mcp_patch_extra_do")); // matches patch_extra (active)
        assert!(is_tool_visible("mcp_patch_do")); // matches patch (active)
    }

    #[test]
    fn workspace_summary_marks_active_and_default() {
        let _g = guard();
        set_member("harbor-prod", "harbor", "prod", true);
        set_member("harbor-staging", "harbor", "staging", false);
        let views = workspace_summary();
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.workspace, "harbor");
        assert_eq!(v.active_env.as_deref(), Some("prod"));
        let prod = v.members.iter().find(|m| m.env == "prod").unwrap();
        let staging = v.members.iter().find(|m| m.env == "staging").unwrap();
        assert!(prod.default && prod.active);
        assert!(!staging.default && !staging.active);
    }
}
