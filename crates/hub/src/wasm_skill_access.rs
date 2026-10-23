//! What is particular to WASM skills in the rules of
//! [`crate::skill_access`]: where they are enforced, and what building one
//! costs.
//!
//! Three consumers read the ownership rules for WASM skills:
//!
//! * [`WasmSkillToolAccess`] hides tools the active user may not run from the
//!   per-round tool list and its matching dispatch check.
//! * [`HubSkillUseGate`] is the authoritative answer at execute time, covering
//!   every caller that never sees a tool list (workflow `tool =` steps,
//!   subagents, `mcp-serve`).
//! * The `wasm_skill.*` tools, `cmd: skill`, and the REST handlers resolve
//!   management authority through [`SkillCaller::require`] over the
//!   [`SkillRegistry`] this module implements for the store.
//!
//! Separately, [`install_source_kind`] answers whether an install compiles on
//! the host, and [`BuildIsolation`] whether this deployment compiles under the
//! privilege-separated runner. Together they decide whether authoring needs
//! [`Capability::ShellExec`] or the member-level [`Capability::SkillsAuthor`].

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::tools::access::{ToolAccess, ToolAccessSnapshot};
use workforce_core::access::AccessLevel;
use workforce_wasm_runtime::{SkillUseGate, WasmSkillStore};

use crate::authz::IsAdminFn;
use crate::def_write_auth::TeamStoreHandle;
pub(crate) use crate::hub_core::skill_builds::BUILD_ISOLATION;
pub use crate::hub_core::skill_builds::{
    build_capability, build_isolation, install_capability, install_source, install_source_kind,
    BuildIsolation, InstallSource,
};
use crate::skill_access::SkillCaller;

/// Record how this process compiles skills. Called once from the serve path;
/// the default is the conservative arm, so any surface that never calls it
/// (CLI, tests) keeps builds admin-only.
pub fn set_build_isolation(isolation: BuildIsolation) {
    let _ = BUILD_ISOLATION.set(isolation);
}

/// Whether a host compile is open to this caller: always under a sandbox,
/// admin-only without one.
pub fn host_compile_allowed(isolation: BuildIsolation, is_admin: bool) -> bool {
    isolation == BuildIsolation::Sandboxed || is_admin
}

/// Gate a build on the deployment's isolation and the caller's standing. A
/// caller with no identity at all is the single-tenant CLI (or a surface that
/// never scopes one) — the same principal-less case the capability gate itself
/// skips, so this skips it too rather than inventing a second answer.
pub fn require_host_compile(caller: &SkillCaller, isolation: BuildIsolation) -> Result<(), String> {
    match caller.user.is_none() || host_compile_allowed(isolation, caller.is_admin) {
        true => Ok(()),
        false => Err(HOST_COMPILE_REFUSED.to_string()),
    }
}

/// Refusal shown when an unsandboxed deployment is asked to compile for a
/// non-admin. Names the two routes that do work, so the author is never stuck.
pub const HOST_COMPILE_REFUSED: &str =
    "Building a skill on this hub compiles Rust as the hub process, which is admin-only here. \
     Build it in CI instead (`wasm_skill.scaffold` writes a release workflow) and install the \
     published artifact with `github_repo = \"owner/repo@<tag>\"` plus its `sha256`, or ask an \
     admin to enable sandboxed builds.";

/// Hides the tools of every installed skill the active user may not run, so a
/// private skill never reaches another user's tool list — or their dispatcher,
/// which consults the same snapshot.
pub struct WasmSkillToolAccess {
    store: Arc<WasmSkillStore>,
    admin: IsAdminFn,
    teams: TeamStoreHandle,
}

impl WasmSkillToolAccess {
    pub fn new(store: Arc<WasmSkillStore>, admin: IsAdminFn, teams: TeamStoreHandle) -> Self {
        Self {
            store,
            admin,
            teams,
        }
    }
}

impl ToolAccess for WasmSkillToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        let caller = SkillCaller::current(&self.admin, &self.teams);
        let blocked: HashMap<String, String> = self
            .store
            .list_installed()
            .into_iter()
            .filter(|skill| !caller.access(skill).allows(AccessLevel::Run))
            .flat_map(|skill| {
                let reason = format!(
                    "`{}` is another user's private WASM skill — ask them to share it with you.",
                    skill.manifest.name
                );
                skill
                    .manifest
                    .tools
                    .iter()
                    .map(|tool| format!("{}.{}", skill.manifest.name, tool.name))
                    .map(|name| (name, reason.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        ToolAccessSnapshot::new(blocked)
    }
}

/// The full per-user tool-visibility policy for an agent: MCP shadowing, WASM
/// skill ownership, the database tools' need for a configured instance and
/// the environment tools' need for environments, merged into the single slot
/// the runner reads. Called from every `set_tool_access` site so no surface
/// gets part of it.
pub fn compose_tool_access(
    service: &Arc<crate::serve::AgentService>,
    mcp: Arc<dyn ToolAccess>,
) -> Arc<dyn ToolAccess> {
    let skills = crate::wasm_skill_tools::global_skill_store().map(|store| {
        Arc::new(WasmSkillToolAccess::new(
            store,
            crate::authz::make_admin_policy(Arc::clone(&service.identity_store)),
            Arc::clone(&service.team_store),
        )) as Arc<dyn ToolAccess>
    });
    let databases = database_tool_access(service.vault.clone(), service.global_vault.clone());
    let environments = crate::environments::EnvironmentToolAccess::policy(service.resolved.clone());
    Arc::new(workforce_agent::tools::access::CompositeToolAccess::new(
        std::iter::once(mcp)
            .chain(skills)
            .chain(databases)
            .chain(std::iter::once(environments))
            .collect(),
    ))
}

/// The `database.*` visibility policy over the same layered vault those tools
/// resolve their instances from; `None` for an agent without a vault, which
/// has no database tools to hide.
fn database_tool_access(
    vault: Option<Arc<crate::vault::Vault>>,
    global_vault: Option<Arc<crate::vault::Vault>>,
) -> Option<Arc<dyn ToolAccess>> {
    vault.map(|vault| {
        Arc::new(workforce_agent::tools::database::DatabaseToolAccess::new(
            Arc::new(crate::user_config::LayeredUserVault::new(
                vault,
                global_vault,
            )),
        )) as Arc<dyn ToolAccess>
    })
}

/// The authoritative use check, consulted by every skill tool before it runs —
/// including the paths that never build a tool list.
pub struct HubSkillUseGate {
    store: Arc<WasmSkillStore>,
    admin: IsAdminFn,
    teams: TeamStoreHandle,
}

impl HubSkillUseGate {
    pub fn new(store: Arc<WasmSkillStore>, admin: IsAdminFn, teams: TeamStoreHandle) -> Self {
        Self {
            store,
            admin,
            teams,
        }
    }
}

impl SkillUseGate for HubSkillUseGate {
    fn authorize_use(&self, skill: &str, user: Option<&str>) -> Result<(), String> {
        let user = user.map(str::to_string);
        SkillCaller::new(
            user.clone(),
            (self.admin)(),
            crate::team_reach::TeamReach::new(self.teams.read().clone(), user),
        )
        .require(&self.store, skill, AccessLevel::Run)
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::Capability;
    use serde_json::json;
    use workforce_core::access::Visibility;
    use workforce_wasm_runtime::SkillOwnership;

    use crate::skill_access::testing::store_with;

    /// A hub with no team store bound yet.
    fn no_teams() -> TeamStoreHandle {
        Arc::new(parking_lot::RwLock::new(None))
    }

    #[test]
    fn install_source_classification() {
        let cases = [
            (json!({"wasm_path": "a.wasm"}), InstallSource::Prebuilt),
            (json!({"project_path": "proj"}), InstallSource::HostCompile),
            (
                json!({"wasm_path": "a.wasm", "project_path": "proj"}),
                InstallSource::Prebuilt,
            ),
            (
                json!({"github_repo": "o/r@v1", "sha256": "ab"}),
                InstallSource::Prebuilt,
            ),
            (json!({"github_repo": "o/r@v1"}), InstallSource::HostCompile),
            (
                json!({"github_repo": "o/r#subpath=x", "sha256": "ab"}),
                InstallSource::HostCompile,
            ),
            (
                json!({"github_repo": format!("o/r@{}", "a".repeat(40)), "sha256": "ab"}),
                InstallSource::HostCompile,
            ),
            (json!({}), InstallSource::Prebuilt),
            (json!({"github_repo": "  "}), InstallSource::Prebuilt),
        ];
        for (input, expected) in cases {
            assert_eq!(install_source_kind(&input), expected, "input {input}");
        }
    }

    #[test]
    fn the_commit_pinned_subpath_form_compiles() {
        const FORM: &str =
            "acme/skills@0123456789abcdef0123456789abcdef01234567#subpath=hello-counter";
        let input = json!({"github_repo": FORM, "config": {"greeting": "hi"}});
        assert_eq!(
            install_capability(&input, BuildIsolation::InProcess),
            Capability::ShellExec
        );
        assert_eq!(
            install_capability(&input, BuildIsolation::Sandboxed),
            Capability::SkillsAuthor
        );
        assert_eq!(
            install_source(None, None, Some(FORM), None),
            InstallSource::HostCompile
        );
    }

    #[test]
    fn capabilities_follow_source_and_isolation() {
        let prebuilt = json!({"wasm_path": "a.wasm"});
        let compile = json!({"project_path": "proj"});
        for isolation in [BuildIsolation::InProcess, BuildIsolation::Sandboxed] {
            assert_eq!(
                install_capability(&prebuilt, isolation),
                Capability::SkillsAuthor
            );
        }
        assert_eq!(
            install_capability(&compile, BuildIsolation::InProcess),
            Capability::ShellExec
        );
        assert_eq!(
            install_capability(&compile, BuildIsolation::Sandboxed),
            Capability::SkillsAuthor
        );
        assert_eq!(
            build_capability(BuildIsolation::InProcess),
            Capability::ShellExec
        );
        assert_eq!(
            build_capability(BuildIsolation::Sandboxed),
            Capability::SkillsAuthor
        );
        assert!(host_compile_allowed(BuildIsolation::Sandboxed, false));
        assert!(host_compile_allowed(BuildIsolation::InProcess, true));
        assert!(!host_compile_allowed(BuildIsolation::InProcess, false));
        assert_eq!(build_isolation(), BuildIsolation::InProcess);
    }

    fn vault() -> Arc<crate::vault::Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        std::mem::forget(tmp);
        Arc::new(crate::vault::Vault::open(&path, "test-key").unwrap())
    }

    #[test]
    fn an_agent_with_no_database_instance_is_not_offered_the_database_tools() {
        let access = database_tool_access(Some(vault()), None).unwrap();
        let snapshot = access.snapshot();
        assert!(snapshot.is_blocked("database.columns"));
        assert!(snapshot.is_blocked("database.query"));
        assert!(!snapshot.is_blocked("database.list_instances"));
    }

    #[test]
    fn an_instance_in_the_global_vault_keeps_the_database_tools_offered() {
        let global = vault();
        global
            .set(
                "databases/instances/main/dsn",
                "postgres://ro@db.internal/app",
            )
            .unwrap();
        let access = database_tool_access(Some(vault()), Some(global)).unwrap();
        assert!(access.snapshot().is_empty());
    }

    /// An instance stored at a per-user level belongs to that user: the tools
    /// are theirs alone, and everyone else is answered exactly as on an
    /// install with no instance at all, so nothing says someone has one.
    #[test]
    fn a_users_own_instance_shows_the_database_tools_to_that_user_only() {
        use workforce_agent::runner::CURRENT_USER_ID;

        let nobody_has_one = database_tool_access(Some(vault()), None)
            .unwrap()
            .snapshot();
        for (agent_key, global_key) in [
            (Some("users/alice/databases/instances/main/dsn"), None),
            (None, Some("users/alice/databases/instances/main/dsn")),
        ] {
            let (agent, global) = (vault(), vault());
            for (store, key) in [(&agent, agent_key), (&global, global_key)] {
                if let Some(key) = key {
                    store.set(key, "postgres://ro@db.internal/app").unwrap();
                }
            }
            let access = database_tool_access(Some(agent), Some(global)).unwrap();
            let seen_by =
                |user: &str| CURRENT_USER_ID.sync_scope(user.to_string(), || access.snapshot());

            assert!(seen_by("alice").is_empty());
            assert_eq!(seen_by("bob"), nobody_has_one);
            assert_eq!(access.snapshot(), nobody_has_one, "no user in scope");
        }
    }

    #[test]
    fn an_agent_without_a_vault_gets_no_database_policy() {
        assert!(database_tool_access(None, None).is_none());
    }

    #[test]
    fn tool_access_blocks_only_unusable_skills() {
        let (_tmp, store) = store_with(&[
            (
                "private",
                SkillOwnership::owned("alice", Visibility::Private),
                vec![],
            ),
            (
                "published",
                SkillOwnership::owned("alice", Visibility::Global),
                vec![],
            ),
        ]);
        let access =
            WasmSkillToolAccess::new(Arc::clone(&store), crate::authz::never_admin(), no_teams());
        let snap = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope("bob".to_string(), || access.snapshot());
        assert!(snap.is_blocked("private.ping"));
        assert!(!snap.is_blocked("published.ping"));

        let owner_snap = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope("alice".to_string(), || access.snapshot());
        assert!(owner_snap.is_empty());
    }

    #[test]
    fn the_use_gate_answers_for_the_named_user() {
        let (_tmp, store) = store_with(&[(
            "private",
            SkillOwnership::owned("alice", Visibility::Private),
            vec![],
        )]);
        let gate =
            HubSkillUseGate::new(Arc::clone(&store), crate::authz::never_admin(), no_teams());
        assert!(gate.authorize_use("private", Some("alice")).is_ok());
        assert!(gate.authorize_use("private", Some("bob")).is_err());
        assert!(gate.authorize_use("private", None).is_err());
        assert!(gate.authorize_use("gone", Some("alice")).is_err());

        let admin_gate = HubSkillUseGate::new(store, crate::authz::always_admin(), no_teams());
        assert!(admin_gate.authorize_use("private", Some("bob")).is_ok());
    }
}
