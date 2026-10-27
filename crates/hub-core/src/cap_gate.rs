//! Hub-side [`CapabilityGate`] — the pre-dispatch admin gate.
//!
//! Resolves the turn's `user_id` to a [`Principal`] (role **and** live status,
//! via the identity store), maps `(tool, input)` to the [`Capability`] it
//! requires ([`tool_capability`]), and hard-denies — before the cascade and
//! before any approval prompt — exactly when [`authorize`] returns
//! [`Decision::Deny`]. This is the single declarative gate that replaces every
//! Increment-1 inline `reject_non_admin` for the flat admin tools (`agent.*`,
//! `role.*`, skill/credential `system.*`).
//!
//! It is **always** consulted when wired — not gated on `[permissions] enabled`;
//! admin authority is constitutional, not opt-in. It is a *deny*-gate that only
//! touches registered admin capabilities: a non-privileged tool maps to no
//! capability and always passes, so a system / background turn is unaffected for
//! its normal tools. A caller that does not resolve to an *active* SystemAdmin —
//! including a no-user / unresolvable identity, or no store at all — is denied
//! those admin tools (fail-closed); a resolvable admin identity is the only way
//! to invoke them.

use std::sync::Arc;

use uuid::Uuid;
use workforce_agent::runner::CapabilityGate;
use workforce_identity::lifecycle::LifecycleOpStore;
use workforce_identity::store::IdentityStore;

use crate::authz::{
    authorize, tool_capability, Capability, CapabilityMatrixHandle, Decision, Principal,
};

/// Resolve a `user_id` string to its live [`Principal`]. `None` when there is
/// no store, the id isn't a UUID, or the store can't resolve it — the caller
/// then scores as a non-admin Member, so registered admin capabilities deny.
/// Shared by the pre-dispatch gate and the WASM skill tool invoker so both
/// score identically.
pub fn resolve_principal(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    user_id: &str,
) -> Option<Principal> {
    let uid = Uuid::parse_str(user_id).ok()?;
    let identity = identity_store?.get_by_id(uid).ok().flatten()?;
    Some(Principal {
        identity_id: Some(identity.id),
        role: identity.role,
        role_key: identity.effective_role_key().to_string(),
        status: identity.status,
    })
}

/// The non-admin fallback for an unresolvable identity — registered admin
/// capabilities still deny, everything else passes.
pub fn fallback_principal() -> Principal {
    Principal {
        identity_id: None,
        role: workforce_identity::types::IdentityRole::Member,
        role_key: "member".to_string(),
        status: workforce_identity::types::IdentityStatus::Active,
    }
}

/// Pre-dispatch admin gate backed by the identity store. The store is optional:
/// when absent (a deployment with no identity model) no caller resolves to an
/// admin, so every registered admin capability fails **closed** — the gate is
/// still wired so removing the inline checks never opens those tools.
pub struct HubCapabilityGate {
    identity_store: Option<Arc<dyn IdentityStore>>,
    matrix: CapabilityMatrixHandle,
    /// Where role changes are recorded, to tell a capability a person lost
    /// from one they never held.
    role_history: Option<Arc<dyn LifecycleOpStore>>,
}

impl HubCapabilityGate {
    pub fn new(
        identity_store: Option<Arc<dyn IdentityStore>>,
        matrix: CapabilityMatrixHandle,
    ) -> Self {
        Self {
            identity_store,
            matrix,
            role_history: None,
        }
    }

    /// Read the roles people held before from `ops`
    /// ([`crate::hub_core::roles::record_role_change`]).
    pub fn with_role_history(self, ops: Arc<dyn LifecycleOpStore>) -> Self {
        Self {
            role_history: Some(ops),
            ..self
        }
    }

    fn principal(&self, user_id: &str) -> Option<Principal> {
        resolve_principal(self.identity_store.as_ref(), user_id)
    }

    /// Human-readable, action-named denial — mirrors `reject_non_admin`'s
    /// wording so the agent surfaces a clear "ask an admin" message.
    fn deny_message(tool: &str, cap: Capability) -> String {
        format!(
            "{tool} requires the SystemAdmin role — you do not have permission to {}. \
             Ask a system administrator to perform this action.",
            action(cap)
        )
    }
}

/// What holding `cap` lets a person do, as a denial names it.
fn action(cap: Capability) -> &'static str {
    match cap {
        Capability::AgentsManage => "manage agents",
        Capability::PersonasManage => "manage role templates",
        Capability::SkillsManage => "install, uninstall, or disable skills",
        // Member-allowed in the default matrix, so this branch is normally
        // unreachable; kept for exhaustiveness and in case an admin tightens
        // it via the capability matrix.
        Capability::SkillsEnable => "enable skills",
        // Member-allowed in the default matrix like `SkillsEnable`; reached
        // only when an admin has tightened the row.
        Capability::SkillsAuthor => "author skills",
        Capability::VaultSharedWrite | Capability::VaultSharedDelete => "manage shared credentials",
        Capability::VaultSharedRead => "read shared credentials",
        Capability::PermissionsManage => "manage permissions",
        Capability::RolesAssign => "assign roles or manage users",
        Capability::ConfigManage => "change shared agent configuration (LLM or cost)",
        Capability::HeartbeatManage => "create or change heartbeat checks",
        Capability::ShellExec => "run shell commands or build/execute code on the host",
        Capability::DatabaseWrite => "run writing statements against a database",
        Capability::NetworkWrite => {
            "make outbound HTTP requests that change remote state (POST, PUT, PATCH, DELETE)"
        }
        Capability::DevicesManage => "administer device pairings or invoke devices",
        Capability::KnowledgeManage => "delete or prune the shared knowledge base",
        Capability::OpsRead => "read hub-wide operational logs",
        Capability::ProjectsManage => "create, delete, or modify projects",
        Capability::AgentExport => "export agents and their data",
        Capability::AgentImport => "import agents and their data",
        Capability::CrossuserRead => "read another user's data",
        Capability::OwnDataRw | Capability::OwnDataDestructive => "perform this action",
    }
}

/// Why an unattended run stopped instead of answering ([`stopped`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The person it runs as is suspended. Their work is held, so nothing
    /// is told where it delivers; the reason is for the run's own record.
    Suspended(String),
    /// A tool call needed a capability the person held under an earlier role
    /// and lacks now.
    Lost {
        tool: String,
        capability: Capability,
    },
}

impl Stop {
    /// What the run records as why it stopped.
    pub fn reason(&self) -> String {
        match self {
            Self::Suspended(reason) => reason.clone(),
            Self::Lost { tool, capability } => format!(
                "This run stopped: the person it runs as no longer has permission to {} ({tool}).",
                action(*capability)
            ),
        }
    }

    /// The one sentence told where the run delivers, which may be a shared
    /// channel, so it names the capability and nothing else about the
    /// person. None for a suspension.
    pub fn notice(&self) -> Option<String> {
        matches!(self, Self::Lost { .. }).then(|| self.reason())
    }
}

/// Why an unattended run as `principal` that was refused a tool call must be
/// reported as stopped rather than answered: the person is now suspended, or
/// `gate`, the one that refused it, says the call needs a capability they
/// lost. Any other refusal (a policy rule, an approval, a capability the
/// person never held) is left to the run, as before.
pub fn stopped(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    gate: Option<&dyn CapabilityGate>,
    principal: &str,
    invocations: &[workforce_agent::runner::ToolInvocation],
) -> Option<Stop> {
    let mut refused = invocations
        .iter()
        .filter(|call| call.denial_reason.is_some())
        .peekable();
    refused.peek()?;
    if let Some(person) = crate::hub_core::standing::suspended(identity_store, principal) {
        return Some(Stop::Suspended(crate::hub_core::standing::refusal(&person)));
    }
    let gate = gate?;
    refused.find_map(|call| {
        tool_capability(&call.tool_name, &call.input)
            .filter(|_| gate.lost(principal, &call.tool_name, &call.input))
            .map(|capability| Stop::Lost {
                tool: call.tool_name.clone(),
                capability,
            })
    })
}

impl HubCapabilityGate {
    /// The capability the call needs and `principal` lacks, if any. Not a
    /// privileged action ⇒ none.
    fn lacking(
        &self,
        principal: &Principal,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<Capability> {
        let cap = tool_capability(tool, input)?;
        let matrix = self.matrix.read().clone();
        (authorize(principal, cap, &matrix) == Decision::Deny).then_some(cap)
    }

    /// Whether a role `principal` held before, as recorded, granted `cap`.
    fn held_before(&self, principal: &Principal, cap: Capability) -> bool {
        let (Some(ops), Some(id)) = (self.role_history.as_deref(), principal.identity_id) else {
            return false;
        };
        let matrix = self.matrix.read().clone();
        crate::hub_core::roles::earlier_roles(ops, id)
            .into_iter()
            .any(|role_key| {
                let earlier = Principal {
                    role_key,
                    status: workforce_identity::types::IdentityStatus::Active,
                    ..principal.clone()
                };
                authorize(&earlier, cap, &matrix) != Decision::Deny
            })
    }
}

impl CapabilityGate for HubCapabilityGate {
    fn lost(&self, user_id: &str, tool: &str, input: &serde_json::Value) -> bool {
        self.principal(user_id).is_some_and(|principal| {
            self.lacking(&principal, tool, input)
                .is_some_and(|cap| self.held_before(&principal, cap))
        })
    }

    fn deny_reason(
        &self,
        user_id: &str,
        _agent_id: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        if let Some(person) =
            crate::hub_core::standing::suspended(self.identity_store.as_ref(), user_id)
        {
            return Some(crate::hub_core::standing::refusal(&person));
        }
        // An unresolvable identity scores as a non-admin Member, so a
        // registered admin capability still denies.
        let principal = self.principal(user_id).unwrap_or_else(fallback_principal);
        let cap = self.lacking(&principal, tool, input)?;
        tracing::warn!(
            target: "sentinel.audit",
            tool = %tool,
            actor = %user_id,
            capability = cap.key(),
            "privileged tool denied pre-dispatch: caller lacks the capability"
        );
        crate::hub_core::permissions_audit::emit_capability_deny(Some(user_id), tool, cap.key());
        Some(Self::deny_message(tool, cap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::CapabilityMatrix;
    use eyre::Result;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::store::IdentityFilter;
    use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

    #[derive(Default)]
    struct InMemoryStore {
        identities: parking_lot::Mutex<Vec<Identity>>,
    }

    impl IdentityStore for InMemoryStore {
        fn create(&self, identity: &Identity) -> Result<()> {
            self.identities.lock().push(identity.clone());
            Ok(())
        }
        fn get_by_id(&self, id: Uuid) -> Result<Option<Identity>> {
            Ok(self.identities.lock().iter().find(|i| i.id == id).cloned())
        }
        fn get_by_email(&self, _email: &str) -> Result<Option<Identity>> {
            Ok(None)
        }
        fn get_by_linked_id(
            &self,
            _platform: &str,
            _platform_id: &str,
        ) -> Result<Option<Identity>> {
            Ok(None)
        }
        fn update(&self, _identity: &Identity) -> Result<()> {
            Ok(())
        }
        fn delete(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }
        fn list(&self, _filter: &IdentityFilter) -> Result<Vec<Identity>> {
            Ok(self.identities.lock().clone())
        }
        fn count_filtered(&self, _filter: &IdentityFilter) -> Result<usize> {
            Ok(self.identities.lock().len())
        }
    }

    fn empty_matrix() -> CapabilityMatrixHandle {
        Arc::new(parking_lot::RwLock::new(Arc::new(
            CapabilityMatrix::default(),
        )))
    }

    /// Insert one identity with the given role+status and return a gate over
    /// the store plus that identity's id string.
    fn gate_with(role: IdentityRole, status: IdentityStatus) -> (HubCapabilityGate, String) {
        let identity = Identity::new("Test", role).with_status(status);
        let id = identity.id.to_string();
        let store = InMemoryStore::default();
        store.create(&identity).unwrap();
        (
            HubCapabilityGate::new(Some(Arc::new(store)), empty_matrix()),
            id,
        )
    }

    #[test]
    fn live_matrix_swap_flips_a_decision_without_rebuilding_the_gate() {
        let identity = Identity::new("M", IdentityRole::Member).with_status(IdentityStatus::Active);
        let uid = identity.id.to_string();
        let store = InMemoryStore::default();
        store.create(&identity).unwrap();
        let matrix = empty_matrix();
        let gate = HubCapabilityGate::new(Some(Arc::new(store)), matrix.clone());

        // `agents_manage` is admin-only by default → the member is denied.
        assert!(gate
            .deny_reason(&uid, "a", "agent.create", &serde_json::json!({}))
            .is_some());

        // An admin loosens it for members; swapping the snapshot takes effect
        // immediately, with no gate reconstruction.
        let loosened =
            CapabilityMatrix::from_overrides(&[workforce_permissions::CapabilityOverride {
                role: "member".into(),
                capability: "agents_manage".into(),
                decision: workforce_permissions::CapabilityDecision::Allow,
                updated_at: chrono::Utc::now(),
                updated_by: None,
            }]);
        *matrix.write() = Arc::new(loosened);
        assert!(gate
            .deny_reason(&uid, "a", "agent.create", &serde_json::json!({}))
            .is_none());
    }

    #[test]
    fn no_store_fails_closed_for_admin_tools() {
        let gate = HubCapabilityGate::new(None, empty_matrix());
        assert!(gate
            .deny_reason(
                &Uuid::new_v4().to_string(),
                "a",
                "agent.delete",
                &agent_delete()
            )
            .is_some());
        assert_eq!(
            gate.deny_reason("u", "a", "agent.list", &serde_json::json!({})),
            None
        );
    }

    fn agent_delete() -> serde_json::Value {
        serde_json::json!({ "id": "victim" })
    }

    fn refused(tool: &str, input: serde_json::Value) -> workforce_agent::runner::ToolInvocation {
        workforce_agent::runner::ToolInvocation {
            tool_name: tool.into(),
            input,
            output: serde_json::Value::Null,
            success: false,
            policy_decision: "deny".into(),
            duration_ms: 0,
            denial_reason: Some("Policy denied".into()),
            error: None,
        }
    }

    /// A store knowing `identity`, and a gate over it that reads the roles
    /// people held before from the same store.
    fn with_history(
        identity: &Identity,
    ) -> (
        Arc<dyn IdentityStore>,
        Arc<SqliteIdentityStore>,
        HubCapabilityGate,
    ) {
        let store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        store.create(identity).unwrap();
        let identities: Arc<dyn IdentityStore> = store.clone();
        let gate = HubCapabilityGate::new(Some(Arc::clone(&identities)), empty_matrix())
            .with_role_history(store.clone());
        (identities, store, gate)
    }

    fn demoted_from_admin(ops: &SqliteIdentityStore, who: &Identity) {
        crate::hub_core::roles::record_role_change(
            ops,
            who.id,
            "system_admin",
            "member",
            &crate::hub_core::roles::Actor(None),
        );
    }

    #[test]
    fn a_suspended_person_is_refused_every_tool_even_an_unprivileged_one() {
        let (gate, uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Suspended);
        for tool in ["agent.list", "memory.recall", "agent.delete"] {
            let reason = gate.deny_reason(&uid, "a", tool, &serde_json::json!({}));
            assert_eq!(
                reason.as_deref(),
                Some("Test is suspended, so nothing can run as them."),
                "{tool}"
            );
        }
        let (active, uid) = gate_with(IdentityRole::Member, IdentityStatus::AutoProvisioned);
        assert_eq!(
            active.deny_reason(&uid, "a", "agent.list", &serde_json::json!({})),
            None,
            "only a suspension refuses unprivileged tools"
        );
    }

    #[test]
    fn a_run_refused_a_capability_its_person_lost_stops_with_one_sentence_naming_only_it() {
        let ada = Identity::new("Ada", IdentityRole::Member);
        let (store, ops, gate) = with_history(&ada);
        demoted_from_admin(&ops, &ada);
        let calls = [
            refused("memory.recall", serde_json::json!({})),
            refused("agent.delete", agent_delete()),
        ];
        let stop = stopped(Some(&store), Some(&gate), &ada.id.to_string(), &calls).unwrap();
        let sentence = "This run stopped: the person it runs as no longer has permission to manage agents (agent.delete).";
        assert_eq!(stop.notice().as_deref(), Some(sentence));
        assert_eq!(stop.reason(), sentence);
    }

    /// A member who never held a capability their run was refused gets the
    /// run's own answer, as before: nothing about their standing changed.
    #[test]
    fn a_capability_the_person_never_held_is_left_to_the_run() {
        let mo = Identity::new("Mo", IdentityRole::Member);
        let (store, ops, gate) = with_history(&mo);
        let calls = [refused("agent.delete", agent_delete())];
        let uid = mo.id.to_string();
        assert_eq!(stopped(Some(&store), Some(&gate), &uid, &calls), None);
        crate::hub_core::roles::record_role_change(
            ops.as_ref(),
            mo.id,
            "manager",
            "member",
            &crate::hub_core::roles::Actor(None),
        );
        assert_eq!(
            stopped(Some(&store), Some(&gate), &uid, &calls),
            None,
            "a manager never held it either"
        );
        let unrecorded = HubCapabilityGate::new(Some(Arc::clone(&store)), empty_matrix());
        demoted_from_admin(&ops, &mo);
        assert_eq!(
            stopped(Some(&store), Some(&unrecorded), &uid, &calls),
            None,
            "a gate that reads no history knows of nothing lost"
        );
        assert_eq!(
            stopped(Some(&store), Some(&gate), "heartbeat", &calls),
            None,
            "a sentinel is nobody whose role changed"
        );
    }

    #[test]
    fn a_refusal_that_is_not_about_the_persons_standing_is_left_to_the_run() {
        let admin = Identity::new("Root", IdentityRole::SystemAdmin);
        let (store, _, gate) = with_history(&admin);
        let uid = admin.id.to_string();
        let policy_refusal = [refused("agent.delete", agent_delete())];
        assert_eq!(
            stopped(Some(&store), Some(&gate), &uid, &policy_refusal),
            None,
            "an admin refused by a policy rule still holds the capability"
        );
        let allowed = workforce_agent::runner::ToolInvocation {
            denial_reason: None,
            success: true,
            ..refused("agent.delete", agent_delete())
        };
        let member = Identity::new("Mo", IdentityRole::Member);
        let (members, ops, member_gate) = with_history(&member);
        demoted_from_admin(&ops, &member);
        let uid = member.id.to_string();
        assert_eq!(
            stopped(Some(&members), Some(&member_gate), &uid, &[allowed]),
            None,
            "nothing was refused"
        );
        assert_eq!(
            stopped(
                Some(&members),
                None,
                &uid,
                &[refused("agent.delete", agent_delete())]
            ),
            None,
            "without a gate nothing was refused for a capability"
        );
    }

    #[test]
    fn a_run_whose_person_was_suspended_meanwhile_stops_and_tells_no_one() {
        let ada =
            Identity::new("Ada", IdentityRole::SystemAdmin).with_status(IdentityStatus::Suspended);
        let (store, _, gate) = with_history(&ada);
        let stop = stopped(
            Some(&store),
            Some(&gate),
            &ada.id.to_string(),
            &[refused("memory.recall", serde_json::json!({}))],
        )
        .unwrap();
        assert_eq!(
            stop.reason(),
            "Ada is suspended, so nothing can run as them."
        );
        assert_eq!(
            stop.notice(),
            None,
            "where it delivers learns nothing of them"
        );
    }

    #[test]
    fn member_is_denied_a_registered_admin_tool() {
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        let reason = gate.deny_reason(&uid, "a", "agent.delete", &agent_delete());
        assert!(reason.is_some(), "member must be denied agent.delete");
        assert!(reason.unwrap().contains("SystemAdmin"));
    }

    #[test]
    fn active_admin_is_allowed_a_registered_admin_tool() {
        let (gate, uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(
            gate.deny_reason(&uid, "a", "agent.delete", &agent_delete()),
            None
        );
    }

    #[test]
    fn suspended_admin_is_denied() {
        // The suspended-admin residual: live status carried in the Principal.
        let (gate, uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Suspended);
        assert!(gate
            .deny_reason(&uid, "a", "agent.delete", &agent_delete())
            .is_some());
    }

    #[test]
    fn shared_vault_write_tools_denied_for_member() {
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        for tool in [
            "system.add_account",
            "system.remove_account",
            "system.reload_slack",
            "slack.connect",
            "slack.app_configure",
            "slack.disconnect",
            "role.delete",
            "system.install_catalog_skill",
        ] {
            assert!(
                gate.deny_reason(&uid, "a", tool, &serde_json::json!({}))
                    .is_some(),
                "{tool} must be denied for a member"
            );
        }
    }

    #[test]
    fn external_install_is_open_to_a_member_except_from_another_git_host() {
        let (member_gate, member_uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        let (admin_gate, admin_uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Active);
        let other_host =
            serde_json::json!({ "source": "https://gitlab.example.com/acme/skills.git" });
        for input in [
            serde_json::json!({
                "github_repo": "acme/skills@0123456789abcdef0123456789abcdef01234567#subpath=hello-world"
            }),
            serde_json::json!({ "source": "https://github.com/acme/skills#subpath=hello-world" }),
            serde_json::json!({ "source": "skills/hello-world" }),
            serde_json::json!({ "source": "/srv/skills/hello-world" }),
            other_host.clone(),
        ] {
            assert_eq!(
                admin_gate.deny_reason(&admin_uid, "a", "system.install_external_skill", &input),
                None,
                "an active admin must be allowed {input}"
            );
            assert_eq!(
                member_gate
                    .deny_reason(&member_uid, "a", "system.install_external_skill", &input)
                    .is_some(),
                input == other_host,
                "{input}"
            );
        }
        assert_eq!(
            member_gate.deny_reason(
                &member_uid,
                "a",
                "system.uninstall_external_skill",
                &serde_json::json!({ "name": "hello-world" })
            ),
            None
        );
    }

    #[test]
    fn mcp_and_config_tools_denied_for_member_allowed_for_admin() {
        let mutating = [
            // MCP server management (connect/disconnect / mutate toolset).
            "mcp.install",
            "mcp.uninstall",
            "mcp.set_tools",
            "mcp.update",
            // Shared LLM / cost configuration.
            "system.set_model",
            "system.set_provider",
            "system.set_routing",
            "system.set_failover_chain",
            "system.set_max_tokens",
            "system.set_max_rounds",
            "system.config",
            "pricing.update",
        ];
        let (member_gate, member_uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        let (admin_gate, admin_uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Active);
        for tool in mutating {
            assert!(
                member_gate
                    .deny_reason(&member_uid, "a", tool, &serde_json::json!({}))
                    .is_some(),
                "{tool} must be denied for a member"
            );
            assert_eq!(
                admin_gate.deny_reason(&admin_uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must be allowed for an active admin"
            );
        }
    }

    #[test]
    fn shell_exec_tools_denied_for_member_allowed_for_admin() {
        // The host code/command execution class — the most privileged tools, so
        // the gate hard-denies them for a Member before any approval prompt.
        let exec_tools = [
            "exec.run",
            "code.cargo",
            "code.build",
            "code.test",
            "code.lint",
            "code.git",
            // Claude Code CLI runners — host `Bash` access.
            "coding.claude",
            "coding.session",
            // The person's own terminals, driven and read.
            "desk.start",
            "desk.prompt",
            "desk.read",
            // Compiles a project on the host — the wasm-skill *lifecycle* tools
            // are member-level (`skills_author`); only the compile is not.
            "wasm_skill.build",
        ];
        let (member_gate, member_uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        let (admin_gate, admin_uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Active);
        for tool in exec_tools {
            assert!(
                member_gate
                    .deny_reason(&member_uid, "a", tool, &serde_json::json!({}))
                    .is_some(),
                "{tool} must be denied for a member"
            );
            assert_eq!(
                admin_gate.deny_reason(&admin_uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must be allowed for an active admin"
            );
        }
    }

    /// A member may author their own WASM skills — the gate has to let the
    /// lifecycle tools through, and still stop the shape that compiles on the
    /// host while this deployment builds in-process.
    #[test]
    fn wasm_skill_authoring_passes_the_gate_for_a_member() {
        let (member_gate, member_uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        for tool in [
            "wasm_skill.install",
            "wasm_skill.import",
            "wasm_skill.update",
            "wasm_skill.uninstall",
        ] {
            assert_eq!(
                member_gate.deny_reason(&member_uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must be open to a member"
            );
        }
        assert!(
            member_gate
                .deny_reason(
                    &member_uid,
                    "a",
                    "wasm_skill.install",
                    &serde_json::json!({ "project_path": "./skill" })
                )
                .is_some(),
            "an install that compiles on the host is not member-level here"
        );
    }

    #[test]
    fn device_knowledge_ops_tools_denied_for_member_allowed_for_admin() {
        let tools = [
            "pairing.approve",
            "pairing.reject",
            "pairing.revoke",
            "pairing.devices",
            "pairing.pending",
            "node_invoke",
            "knowledge.delete",
            "system.logs",
            "projects.create",
            "projects.delete",
            "projects.add_repo",
            "projects.open_workspace",
            "projects.index",
        ];
        let (member_gate, member_uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        let (admin_gate, admin_uid) = gate_with(IdentityRole::SystemAdmin, IdentityStatus::Active);
        for tool in tools {
            assert!(
                member_gate
                    .deny_reason(&member_uid, "a", tool, &serde_json::json!({}))
                    .is_some(),
                "{tool} must be denied for a member"
            );
            assert_eq!(
                admin_gate.deny_reason(&admin_uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must be allowed for an active admin"
            );
        }
    }

    #[test]
    fn device_knowledge_ops_read_siblings_are_never_gated() {
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        for tool in [
            "knowledge.index",
            "knowledge.refresh_index",
            "settings",
            "projects.list",
            "projects.show",
        ] {
            assert_eq!(
                gate.deny_reason(&uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must stay ungated"
            );
        }
    }

    #[test]
    fn shell_exec_read_only_code_siblings_are_never_gated() {
        // Read-only / worktree-confined code tools stay member-accessible — only
        // the execution runners are gated.
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        for tool in [
            "code.read",
            "code.search",
            "code.tree",
            "code.symbols",
            "code.diff",
            "code.find",
            "code.edit",
            "code.find_replace",
            "wasm_skill.scaffold",
            "wasm_skill.list",
            "wasm_skill.export",
            "wasm_skill.guide",
        ] {
            assert_eq!(
                gate.deny_reason(&uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must stay ungated"
            );
        }
    }

    #[test]
    fn mcp_and_config_read_only_siblings_are_never_gated() {
        // The read-only / per-user / dormant-only siblings stay member-accessible:
        // the gate must never flip them to a pre-prompt Deny.
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        for tool in [
            "mcp.list",
            "mcp.list_presets",
            "mcp.show",
            "mcp.list_remote_tools",
            "mcp.workspace_list",
            "mcp.workspace_add",
            "mcp.authorize",
            "system.list_models",
            "system.llm_status",
            "system.routing_status",
            "system.set_session_model",
            "pricing.list",
        ] {
            assert_eq!(
                gate.deny_reason(&uid, "a", tool, &serde_json::json!({})),
                None,
                "{tool} must stay ungated"
            );
        }
    }

    #[test]
    fn vault_set_global_denied_own_scope_allowed_for_member() {
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        // scope=global ⇒ shared write ⇒ denied.
        assert!(gate
            .deny_reason(
                &uid,
                "a",
                "vault.set",
                &serde_json::json!({"path": "x", "value": "y", "scope": "global"})
            )
            .is_some());
        // own per-user scope ⇒ not denied.
        assert_eq!(
            gate.deny_reason(
                &uid,
                "a",
                "vault.set",
                &serde_json::json!({"path": "x", "value": "y", "scope": "global_per_user"})
            ),
            None
        );
    }

    #[test]
    fn own_vault_delete_is_never_gate_denied() {
        // vault.delete maps to OwnDataDestructive (Ask) — the gate passes it
        // through to the tool's own inline store check. A member deleting their
        // own per-user secret must NOT be hard-denied here.
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        for path in ["users/me/skills/jira/api_token", "skills/jira/api_token"] {
            assert_eq!(
                gate.deny_reason(
                    &uid,
                    "a",
                    "vault.delete",
                    &serde_json::json!({"path": path})
                ),
                None,
                "{path}"
            );
        }
    }

    #[test]
    fn no_user_and_unresolvable_identity_pass_through() {
        let (gate, _uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        // An id that isn't in the store resolves to a non-admin Member, so a
        // registered admin tool is still denied — but a non-admin tool passes.
        let unknown = Uuid::new_v4().to_string();
        assert!(gate
            .deny_reason(&unknown, "a", "agent.delete", &agent_delete())
            .is_some());
        // A non-privileged tool is never gated regardless of identity.
        assert_eq!(
            gate.deny_reason(&unknown, "a", "agent.list", &serde_json::json!({})),
            None
        );
        // A non-UUID user id (e.g. a raw platform id) also resolves to
        // non-admin Member — registered admin tool denied, reads pass.
        assert!(gate
            .deny_reason("not-a-uuid", "a", "role.delete", &serde_json::json!({}))
            .is_some());
        assert_eq!(
            gate.deny_reason(
                "not-a-uuid",
                "a",
                "system.list_skills",
                &serde_json::json!({})
            ),
            None
        );
    }

    #[test]
    fn unregistered_non_destructive_tool_is_never_gated() {
        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        assert_eq!(
            gate.deny_reason(&uid, "a", "github.list_issues", &serde_json::json!({})),
            None
        );
    }

    /// A gate hard-Deny lands an access-audit event in the shared sink so the
    /// denied admin attempt shows in `GET /api/permissions/audit`, not only in
    /// the `sentinel.audit` tracing target. Asserted through the process-wide
    /// ring buffer the API reads, scoped to a unique actor so it's robust to
    /// any other event in the buffer.
    #[test]
    fn gate_deny_emits_an_access_audit_event() {
        use workforce_memory::access_audit::{self, AuditDecision, AuditOperation};

        let buffer = access_audit::install_ring_buffer(256);
        // The hub lib test binary has no other global-sink registration, so
        // routing the global sink at the buffer makes the emit observable.
        let _ = access_audit::set_global_sink(buffer.clone());

        let (gate, uid) = gate_with(IdentityRole::Member, IdentityStatus::Active);
        let reason = gate.deny_reason(&uid, "a", "agent.delete", &agent_delete());
        assert!(reason.is_some(), "member must be denied agent.delete");

        let event = buffer
            .snapshot()
            .into_iter()
            .find(|e| e.requester_identity_id.as_deref() == Some(uid.as_str()))
            .expect("gate Deny must emit an access-audit event for this actor");
        assert_eq!(event.operation, AuditOperation::PermissionDecision);
        assert_eq!(event.decision, AuditDecision::DeniedOwnerMismatch);
        assert_eq!(event.resource_id.as_deref(), Some("agent.delete"));
        assert_eq!(event.owner_identity_id.as_deref(), Some("agents_manage"));
    }
}
