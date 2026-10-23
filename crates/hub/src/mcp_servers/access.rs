//! Per-user tool-visibility decision for MCP shadows. Implements the
//! [`workforce_agent::tools::access::ToolAccess`] trait so the runner
//! can ask, once per LLM round, which tools are off-limits for the
//! active user — and surface the same answer at both the LLM
//! tool-list boundary and the dispatch path.
//!
//! Decision per installed server with a non-empty shadow plan:
//!
//! | user has token? | `fallback_to_builtin` | built-in registered? | result |
//! |---|---|---|---|
//! | yes | — | — | hide built-ins (MCP is the surface) |
//! | no  | true | yes | hide MCP (built-in is the surface) |
//! | no  | true | no  | hide built-ins (no fallback possible) |
//! | no  | false | — | hide built-ins (MCP fails loudly with authorize hint) |
//!
//! "User has token" is a vault probe at the path the live `AuthProvider`
//! would read, so visibility tracks the auth path exactly. PAT and
//! OAuth collapse into one predicate via [`VaultCtx::token_present`].

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::settings::McpAuthBinding;

use super::registry::McpServerRegistry;
use super::server::{ShadowPlan, TokenProbe};
use super::shadows;
use crate::user_config::current_identity_id;
use crate::vault::Vault;

pub struct McpToolAccess {
    servers: Arc<McpServerRegistry>,
    tools: Arc<ToolRegistry>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

impl McpToolAccess {
    pub fn new(
        servers: Arc<McpServerRegistry>,
        tools: Arc<ToolRegistry>,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        Self {
            servers,
            tools,
            agent_vault,
            global_vault,
        }
    }
}

impl ToolAccess for McpToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        let plans = self.servers.shadow_plans();
        if plans.is_empty() {
            return ToolAccessSnapshot::default();
        }
        let user_id = current_identity_id();
        let ctx = VaultCtx {
            user_id: user_id.as_deref(),
            agent_vault: self.agent_vault.as_deref(),
            global_vault: self.global_vault.as_deref(),
        };
        let registered = self.tools.names();
        let mut blocked = HashMap::new();
        for (server_id, plan) in plans {
            apply_plan(&mut blocked, &server_id, &plan, &registered, &ctx);
        }
        ToolAccessSnapshot::new(blocked)
    }
}

/// Borrowed snapshot of the vault chain at the moment a [`ToolAccessSnapshot`]
/// is computed. Holding `Option<&Vault>` instead of `Arc<Vault>` keeps the
/// pure helpers below cheap and side-effect-free.
struct VaultCtx<'a> {
    user_id: Option<&'a str>,
    agent_vault: Option<&'a Vault>,
    global_vault: Option<&'a Vault>,
}

impl<'a> VaultCtx<'a> {
    /// Resolution chain matching [`crate::user_config::LayeredUserVault::get`]:
    /// `users/{uid}/{path}` in global → in agent → bare `{path}` in agent → in global.
    fn layered_get(&self, path: &str) -> Option<String> {
        let user_key = self.user_id.map(|uid| format!("users/{uid}/{path}"));
        user_key
            .as_deref()
            .and_then(|k| self.global_vault.and_then(|v| v.get(k)))
            .or_else(|| {
                user_key
                    .as_deref()
                    .and_then(|k| self.agent_vault.and_then(|v| v.get(k)))
            })
            .or_else(|| self.agent_vault.and_then(|v| v.get(path)))
            .or_else(|| self.global_vault.and_then(|v| v.get(path)))
    }

    fn token_present(&self, probe: &TokenProbe, server_id: &str) -> bool {
        match probe {
            TokenProbe::Pat { vault_path } => self.layered_get(vault_path).is_some(),
            TokenProbe::Oauth { binding } => {
                let Some(g) = self.global_vault else {
                    return false;
                };
                let path = match binding {
                    McpAuthBinding::User => {
                        let Some(uid) = self.user_id else {
                            return false;
                        };
                        format!("users/{uid}/mcp/v1/{server_id}/refresh_token")
                    }
                    McpAuthBinding::Shared => format!("mcp/v1/{server_id}/refresh_token"),
                };
                g.get(&path).is_some()
            }
        }
    }
}

fn apply_plan(
    blocked: &mut HashMap<String, String>,
    server_id: &str,
    plan: &ShadowPlan,
    registered: &[String],
    ctx: &VaultCtx<'_>,
) {
    let mcp_prefix = format!("mcp.{server_id}.");
    let is_builtin =
        |name: &str| !name.starts_with(&mcp_prefix) && shadows::matches_any(&plan.patterns, name);
    let has_token = ctx.token_present(&plan.token_probe, server_id);
    let builtin_registered = registered.iter().any(|n| is_builtin(n));
    let route_to_mcp = has_token || !plan.fallback_to_builtin || !builtin_registered;

    let (reason, hidden): (String, Vec<&String>) = if route_to_mcp {
        let reason = if has_token {
            format!(
                "`{server_id}` MCP server is active for this user; use the `mcp.{server_id}.*` tools instead."
            )
        } else {
            format!(
                "`{server_id}` MCP server requires per-user authorisation. \
                 Authorise via Settings → MCP Servers (or call `mcp.authorize`) \
                 then call the `mcp.{server_id}.*` tools."
            )
        };
        (
            reason,
            registered.iter().filter(|n| is_builtin(n)).collect(),
        )
    } else {
        let reason = format!(
            "`{server_id}` MCP server is not authorised for this user; falling back to the built-in skill. \
             Authorise via Settings → MCP Servers to switch this user onto the MCP surface."
        );
        (
            reason,
            registered
                .iter()
                .filter(|n| n.starts_with(&mcp_prefix))
                .collect(),
        )
    };

    for name in hidden {
        blocked.insert(name.clone(), reason.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workforce_common::settings::McpAuthBinding;

    fn tmp_vault(seed: &[(&str, &str)]) -> (TempDir, Arc<Vault>) {
        let tmp = TempDir::new().unwrap();
        let vault = Vault::open(&tmp.path().join("vault.json"), "test-key").unwrap();
        for (k, v) in seed {
            vault.set(k, v).unwrap();
        }
        (tmp, Arc::new(vault))
    }

    /// Built-in adapter tools register under DOTTED internal names in
    /// production (`linear.issue_create`), so the fixtures must use the
    /// dotted form — the underscored patterns in presets are expected to
    /// match these via [`shadows::matches`]'s normalisation. Using fake
    /// underscored names here previously hid the routing bug entirely.
    fn registered_tool_names() -> Vec<String> {
        vec![
            "linear.issue_create".to_string(),
            "linear.issue_get".to_string(),
            "mcp.linear.create_issue".to_string(),
            "mcp.linear.list_issues".to_string(),
            "filesystem_read".to_string(),
        ]
    }

    fn oauth_user_plan() -> ShadowPlan {
        ShadowPlan {
            patterns: vec!["linear_*".to_string()],
            fallback_to_builtin: false,
            token_probe: TokenProbe::Oauth {
                binding: McpAuthBinding::User,
            },
        }
    }

    fn ctx<'a>(
        user_id: Option<&'a str>,
        agent_vault: Option<&'a Vault>,
        global_vault: Option<&'a Vault>,
    ) -> VaultCtx<'a> {
        VaultCtx {
            user_id,
            agent_vault,
            global_vault,
        }
    }

    fn run(
        plan: &ShadowPlan,
        registered: &[String],
        ctx: &VaultCtx<'_>,
    ) -> HashMap<String, String> {
        let mut blocked = HashMap::new();
        apply_plan(&mut blocked, "linear", plan, registered, ctx);
        blocked
    }

    #[test]
    fn empty_plans_yield_empty_snapshot() {
        let plan = ShadowPlan {
            patterns: vec![],
            fallback_to_builtin: false,
            token_probe: TokenProbe::Oauth {
                binding: McpAuthBinding::User,
            },
        };
        assert!(run(&plan, &registered_tool_names(), &ctx(None, None, None)).is_empty());
    }

    #[test]
    fn user_with_oauth_hides_builtins() {
        let (_tmp, v) = tmp_vault(&[("users/u1/mcp/v1/linear/refresh_token", "rt")]);
        let blocked = run(
            &oauth_user_plan(),
            &registered_tool_names(),
            &ctx(Some("u1"), None, Some(&v)),
        );
        assert!(blocked.contains_key("linear.issue_create"));
        assert!(blocked.contains_key("linear.issue_get"));
        assert!(!blocked.contains_key("mcp.linear.create_issue"));
        assert!(!blocked.contains_key("filesystem_read"));
    }

    #[test]
    fn user_without_oauth_no_fallback_still_hides_builtins() {
        let (_tmp, v) = tmp_vault(&[]);
        let blocked = run(
            &oauth_user_plan(),
            &registered_tool_names(),
            &ctx(Some("u1"), None, Some(&v)),
        );
        assert!(blocked.contains_key("linear.issue_create"));
        assert!(!blocked.contains_key("mcp.linear.create_issue"));
        let reason = blocked.get("linear.issue_create").unwrap();
        assert!(
            reason.contains("authoris") || reason.contains("authorize"),
            "expected authorise hint, got: {reason}"
        );
    }

    #[test]
    fn user_without_oauth_with_fallback_hides_mcp() {
        let (_tmp, v) = tmp_vault(&[]);
        let plan = ShadowPlan {
            fallback_to_builtin: true,
            ..oauth_user_plan()
        };
        let blocked = run(
            &plan,
            &registered_tool_names(),
            &ctx(Some("u1"), None, Some(&v)),
        );
        assert!(blocked.contains_key("mcp.linear.create_issue"));
        assert!(blocked.contains_key("mcp.linear.list_issues"));
        assert!(!blocked.contains_key("linear.issue_create"));
        assert!(!blocked.contains_key("linear.issue_get"));
    }

    #[test]
    fn fallback_with_no_builtin_registered_still_hides_builtins() {
        let (_tmp, v) = tmp_vault(&[]);
        let plan = ShadowPlan {
            fallback_to_builtin: true,
            ..oauth_user_plan()
        };
        let only_mcp = vec![
            "mcp.linear.create_issue".to_string(),
            "filesystem_read".to_string(),
        ];
        let blocked = run(&plan, &only_mcp, &ctx(Some("u1"), None, Some(&v)));
        assert!(blocked.is_empty());
    }

    #[test]
    fn missing_user_id_treated_as_no_token() {
        let (_tmp, v) = tmp_vault(&[("users/u1/mcp/v1/linear/refresh_token", "rt")]);
        let blocked = run(
            &oauth_user_plan(),
            &registered_tool_names(),
            &ctx(None, None, Some(&v)),
        );
        assert!(blocked.contains_key("linear.issue_create"));
        assert!(!blocked.contains_key("mcp.linear.create_issue"));
    }

    #[test]
    fn shared_binding_probes_shared_path() {
        let (_tmp, v) = tmp_vault(&[("mcp/v1/linear/refresh_token", "rt")]);
        let plan = ShadowPlan {
            token_probe: TokenProbe::Oauth {
                binding: McpAuthBinding::Shared,
            },
            ..oauth_user_plan()
        };
        let blocked = run(&plan, &registered_tool_names(), &ctx(None, None, Some(&v)));
        assert!(blocked.contains_key("linear.issue_create"));
        assert!(!blocked.contains_key("mcp.linear.create_issue"));
    }

    #[test]
    fn pat_present_hides_builtins() {
        let (_tmp, v) = tmp_vault(&[("skills/test/token", "ghp_x")]);
        let plan = ShadowPlan {
            patterns: vec!["linear_*".to_string()],
            fallback_to_builtin: false,
            token_probe: TokenProbe::Pat {
                vault_path: "skills/test/token".to_string(),
            },
        };
        let blocked = run(&plan, &registered_tool_names(), &ctx(None, Some(&v), None));
        assert!(blocked.contains_key("linear.issue_create"));
    }

    #[test]
    fn pat_absent_with_fallback_hides_mcp() {
        let (_tmp, v) = tmp_vault(&[]);
        let plan = ShadowPlan {
            patterns: vec!["linear_*".to_string()],
            fallback_to_builtin: true,
            token_probe: TokenProbe::Pat {
                vault_path: "skills/test/token".to_string(),
            },
        };
        let blocked = run(&plan, &registered_tool_names(), &ctx(None, Some(&v), None));
        assert!(blocked.contains_key("mcp.linear.create_issue"));
        assert!(!blocked.contains_key("linear.issue_create"));
    }
}
