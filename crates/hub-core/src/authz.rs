//! Shared authorization gate for privileged agent tools.
//!
//! One mechanism — `IsAdminFn` + [`make_admin_policy`] — decides "is the
//! current turn's user a SystemAdmin", and every privileged tool
//! (`vault.delete`, `agent.*`, `role.*`, `system.{disable,uninstall}_skill`)
//! is constructed with the *same* closure. The default is deny: a turn with
//! no identity in scope, or no identity store wired, resolves to non-admin.
//!
//! This is the "subtract, don't stack" authorization gate: the documented role→capability
//! truth is encoded once in [`capability_summary`] / [`decision_for`], the
//! caller is resolved into a [`Principal`], and [`authorize`] is the single
//! decision point. The admin **tool** checks are enforced pre-dispatch by the
//! [`crate::cap_gate::HubCapabilityGate`] off the [`tool_capability`] registry;
//! `make_admin_policy` remains only for the vault tools' own scope guards.
//!
//! KNOWN LIMITATIONS (fail-closed, surfaced to the admin UI):
//! * The vault tools (`vault.set` global, `vault.move`/`vault.delete` shared,
//!   `vault.rename`, `vault.copy`) keep their own inline `is_effective_admin`
//!   guard *in addition to* the matrix. Granting `vault_shared_write` /
//!   `vault_shared_delete` to a non-`system_admin` role therefore takes effect
//!   for the cap-gate-only tools (e.g. `system.add_account`) but the vault tools
//!   still require SystemAdmin. This denies more than the matrix shows (never
//!   less), so it is safe; the matrix UI flags these capabilities accordingly.
//! * Custom and `manager` JWTs mint Member [`crate::jwt`] scopes, so a matrix
//!   grant bites on the agent-tool path and the `require_capability_*` REST
//!   routes, but not on routes still gated by `require_scope(SCOPE_ADMIN)`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{IdentityRole, IdentityStatus};
use workforce_permissions::{
    CapabilityDecision as StoredDecision, CapabilityOverride, CapabilityStore,
};

/// Closure that returns `true` iff the active identity has the SystemAdmin
/// role. Wired from `serve.rs` via the identity store; defaults to deny-all so
/// single-tenant CLI usage and identity-less turns don't reach admin paths.
pub type IsAdminFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Handle shape shared by `serve.rs` for the lazily-populated identity store.
type IdentityStoreHandle = Arc<parking_lot::RwLock<Option<Arc<dyn IdentityStore>>>>;

/// `IsAdminFn` that always returns `true` — single-tenant CLI use, where the
/// operator at the keyboard is trusted with every tier.
pub fn always_admin() -> IsAdminFn {
    Arc::new(|| true)
}

/// `IsAdminFn` that always returns `false` — the deny default and the lever
/// adversarial tests pull to assert a Member is rejected.
pub fn never_admin() -> IsAdminFn {
    Arc::new(|| false)
}

/// The single status-aware admin decision, shared by [`make_admin_policy`]
/// (the gate) and [`capability_summary`] (whoami / `GET /api/me/capabilities`).
/// Admin power requires *both* the SystemAdmin role and an `Active` status —
/// a suspended (or otherwise non-active) SystemAdmin resolves to a non-admin
/// everywhere, so the gate denies them and whoami never reports admin caps.
pub fn is_effective_admin(role: IdentityRole, status: IdentityStatus) -> bool {
    role.can_manage_system() && status == IdentityStatus::Active
}

/// The team-management counterpart of [`is_effective_admin`]: a SystemAdmin or
/// Manager role, and an `Active` status, as the capability matrix demotes any
/// other status to the member row.
pub fn is_effective_manager(role: IdentityRole, status: IdentityStatus) -> bool {
    role.can_manage_teams() && status == IdentityStatus::Active
}

/// A resolved caller: the live identity (id, role, role key, and **status**) the
/// engine scores. `role` is the lossy built-in enum used only for the
/// [`is_effective_admin`] hard gates; `role_key` carries the caller's real role
/// (built-in or custom) and is the matrix lookup key. Resolved once per call
/// site so the suspended-admin residual is closed everywhere, not just the two
/// Increment-1 inline paths. `identity_id == None` is a system / CLI / background
/// caller — trusted, never member-gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub identity_id: Option<uuid::Uuid>,
    pub role: IdentityRole,
    pub role_key: String,
    pub status: IdentityStatus,
}

/// The single authorization decision for `(principal, capability)`: the live
/// matrix cell. `matrix` layers admin overrides over the built-in defaults; an
/// empty matrix ([`CapabilityMatrix::default`]) reproduces [`decision_for`].
pub fn authorize(p: &Principal, cap: Capability, matrix: &CapabilityMatrix) -> Decision {
    matrix.decision(&p.role_key, p.status, cap)
}

/// Edge helper for the gateway call sites: `Err(Deny)` iff [`authorize`] is a
/// hard `Deny`; `Ok(())` for `Allow`/`Ask` (the route's own flow owns the
/// confirm). Returns the [`Decision`] in the `Err` so the caller can render it.
pub fn enforce(p: &Principal, cap: Capability, matrix: &CapabilityMatrix) -> Result<(), Decision> {
    match authorize(p, cap, matrix) {
        Decision::Deny => Err(Decision::Deny),
        other => {
            debug_assert!(matches!(other, Decision::Allow | Decision::Ask));
            Ok(())
        }
    }
}

/// A live, hot-swappable capability-matrix snapshot: the gate and resolver read
/// it on every call; an admin save atomically replaces the inner `Arc`.
pub type CapabilityMatrixHandle = Arc<parking_lot::RwLock<Arc<CapabilityMatrix>>>;

/// Admin overrides layered over the built-in [`baseline_for`] defaults, keyed
/// `capability → (role_key → decision)`. `role_key` is any built-in
/// (`member` / `manager` / `system_admin`) or a custom-role key. A non-`Active`
/// caller always resolves through the `member` row, and the
/// [protected admin capabilities](is_protected_admin_cap) are never overridable
/// (their rows are dropped here), so generalizing to custom roles can never widen
/// the named-admin surface. The default (empty) reproduces the built-in matrix.
#[derive(Debug, Clone, Default)]
pub struct CapabilityMatrix {
    overrides: HashMap<Capability, HashMap<String, Decision>>,
}

impl CapabilityMatrix {
    /// Build from persisted overrides. Rows with an unknown capability key, or
    /// targeting a [protected admin capability](is_protected_admin_cap), are
    /// dropped — the former could never be read, the latter must always fall to
    /// the built-in baseline so admin export/import and `crossuser_read` can be
    /// neither re-granted to another role nor revoked from admin through the
    /// matrix.
    pub fn from_overrides(overrides: &[CapabilityOverride]) -> Self {
        let overrides = overrides
            .iter()
            .filter_map(|o| {
                let cap = Capability::from_key(&o.capability)?;
                (!is_protected_admin_cap(cap))
                    .then(|| (cap, o.role.clone(), decision_from_stored(o.decision)))
            })
            .fold(
                HashMap::<Capability, HashMap<String, Decision>>::new(),
                |mut acc, (cap, role, decision)| {
                    acc.entry(cap).or_default().insert(role, decision);
                    acc
                },
            );
        Self { overrides }
    }

    /// The effective decision for a caller's `role_key` at `status`: the admin
    /// override for that role if set, else the built-in [`baseline_for`] default.
    /// A non-`Active` caller is demoted to the `member` row (the generalized
    /// suspended-admin residual), and any non-`system_admin` role is hard-denied
    /// every [protected admin capability](is_protected_admin_cap).
    pub fn decision(&self, role_key: &str, status: IdentityStatus, cap: Capability) -> Decision {
        let effective = if status == IdentityStatus::Active {
            role_key
        } else {
            "member"
        };
        if is_protected_admin_cap(cap) && effective != "system_admin" {
            return Decision::Deny;
        }
        self.overrides
            .get(&cap)
            .and_then(|by_role| by_role.get(effective))
            .copied()
            .unwrap_or_else(|| baseline_for(effective, cap))
    }
}

/// The process-wide matrix snapshot. Initialised empty (built-in defaults),
/// replaced at boot from the override store and thereafter on every admin save
/// and the convergence reloader. One cell is the single source of truth the
/// gate, the vault resolver, and every `capability_summary` / `enforce` call
/// site read — so a save takes effect everywhere at once, with no restart.
fn matrix_cell() -> &'static CapabilityMatrixHandle {
    static CELL: std::sync::OnceLock<CapabilityMatrixHandle> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        Arc::new(parking_lot::RwLock::new(Arc::new(
            CapabilityMatrix::default(),
        )))
    })
}

/// A clone of the shared matrix handle — for the gate and the vault resolver,
/// which hold it and read it on every call.
pub fn capability_matrix_handle() -> CapabilityMatrixHandle {
    Arc::clone(matrix_cell())
}

/// The current matrix snapshot (a cheap `Arc` clone). The read path for
/// `capability_summary` / `enforce` call sites that don't hold the handle.
pub fn capability_matrix() -> Arc<CapabilityMatrix> {
    matrix_cell().read().clone()
}

/// Atomically replace the live matrix; readers see it on their next call.
pub fn set_capability_matrix(next: CapabilityMatrix) {
    *matrix_cell().write() = Arc::new(next);
}

#[allow(clippy::type_complexity)]
fn capability_store_cell() -> &'static parking_lot::RwLock<Option<Arc<dyn CapabilityStore>>> {
    static CELL: std::sync::OnceLock<parking_lot::RwLock<Option<Arc<dyn CapabilityStore>>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| parking_lot::RwLock::new(None))
}

/// Late-bind the capability backend (overrides + custom roles) at boot so the
/// admin handlers can persist changes. `None` until wired (or when no store is
/// configured).
pub fn set_capability_store(store: Arc<dyn CapabilityStore>) {
    *capability_store_cell().write() = Some(store);
}

/// The capability backend, if wired. The admin matrix / custom-role handlers
/// persist through it.
pub fn capability_store() -> Option<Arc<dyn CapabilityStore>> {
    capability_store_cell().read().clone()
}

/// Map a persisted [`StoredDecision`] to the engine's [`Decision`].
pub fn decision_from_stored(d: StoredDecision) -> Decision {
    match d {
        StoredDecision::Allow => Decision::Allow,
        StoredDecision::Ask => Decision::Ask,
        StoredDecision::Deny => Decision::Deny,
    }
}

/// Map an engine [`Decision`] to the persisted [`StoredDecision`].
pub fn decision_to_stored(d: Decision) -> StoredDecision {
    match d {
        Decision::Allow => StoredDecision::Allow,
        Decision::Ask => StoredDecision::Ask,
        Decision::Deny => StoredDecision::Deny,
    }
}

/// Build the shared `IsAdminFn`: `true` iff the task-local `CURRENT_USER_ID`
/// resolves to an `Active` identity whose role
/// [`IdentityRole::can_manage_system`]. Returns `false` when no identity is in
/// scope, the store is unset, or the identity is not active (suspended).
pub fn make_admin_policy(identity_store: IdentityStoreHandle) -> IsAdminFn {
    Arc::new(move || {
        let Some(uid) = crate::user_config::current_identity_id() else {
            return false;
        };
        let Some(store) = identity_store.read().clone() else {
            return false;
        };
        uuid::Uuid::parse_str(&uid)
            .ok()
            .and_then(|u| store.get_by_id(u).ok().flatten())
            .map(|i| is_effective_admin(i.role, i.status))
            .unwrap_or(false)
    })
}

/// Whether `uid` resolves to an identity the live capability matrix outright
/// allows `capability` — the standing a surface without the pre-dispatch gate
/// reads. `Ask` is not holding it: a confirmation cannot be asked for there.
pub fn holds_capability(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    uid: &str,
    capability: Capability,
) -> bool {
    holds_capability_in(identity_store, uid, capability, &capability_matrix())
}

fn holds_capability_in(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    uid: &str,
    capability: Capability,
    matrix: &CapabilityMatrix,
) -> bool {
    crate::cap_gate::resolve_principal(identity_store, uid)
        .is_some_and(|principal| authorize(&principal, capability, matrix) == Decision::Allow)
}

/// The [`IsAdminFn`] shape of [`holds_capability`] for the turn's identity, so
/// a tool can resolve standing that follows the live matrix, not the role.
pub fn make_capability_policy(
    identity_store: IdentityStoreHandle,
    capability: Capability,
) -> IsAdminFn {
    Arc::new(move || {
        crate::user_config::current_identity_id()
            .is_some_and(|uid| holds_capability(identity_store.read().as_ref(), &uid, capability))
    })
}

// ───────────────────────── Capability matrix ──────────────────────────────

/// A single authorization decision for a capability, as encoded by the
/// role→capability matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The role may perform this capability outright.
    Allow,
    /// Permitted but requires confirmation (self-serve, or routed to an
    /// admin via approval routing). Used for own-data destructive actions.
    Ask,
    /// Hard deny. A constitutional baseline `Deny` is absolute — per-user
    /// overrides can tighten but never loosen it.
    Deny,
}

/// The capabilities the matrix scores. Stable string keys (the serde rename)
/// are the contract the UI / audit export reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read/write the caller's own data (own vault user-slots, memory,
    /// schedules, connected creds).
    OwnDataRw,
    /// Destructive action on the caller's own data (delete/drop) — confirmed.
    OwnDataDestructive,
    /// Read a shared (T3) / global (T4) vault secret.
    VaultSharedRead,
    /// Write a shared / global vault secret.
    VaultSharedWrite,
    /// Delete a shared / global vault secret.
    VaultSharedDelete,
    /// Create / update / delete agents.
    AgentsManage,
    /// Manage permission rules, approvers, and policy.
    PermissionsManage,
    /// Assign roles / manage users.
    RolesAssign,
    /// Install / uninstall skills, and disable a loaded skill agent-wide — the
    /// privileged lifecycle that adds skill code or removes a running skill for
    /// everyone. Admin-only.
    SkillsManage,
    /// Enable (load) an already-installed, already-credentialed skill at runtime
    /// (`system.enable_skill`). Member-allowed: it registers no new code and
    /// grants no new credential — it loads tools from creds the vault already
    /// holds, and a per-user (OAuth) skill still resolves the caller's own
    /// account at call time. Separated from [`Capability::SkillsManage`] so a
    /// member can self-serve a configured skill (e.g. re-connect Google after a
    /// restart) without holding the install/credential privileges. Member-Allow
    /// means the cap gate passes it to the permission cascade, so an admin can
    /// still scope it per-role/user with a permission rule.
    SkillsEnable,
    /// Author and manage your **own** WASM skills — install prebuilt bytes,
    /// update, uninstall, share, and publish a skill you own. Member-allowed:
    /// a WASM skill is sandboxed, its host calls are manifest-declared, and
    /// `wf_tool_invoke` runs every call through the *calling* user's own
    /// capability matrix — so a skill can never reach past its caller. Per-skill
    /// ownership ([`crate::skill_access`]) decides which skills the holder
    /// may touch; this cell only decides whether they may author at all.
    SkillsAuthor,
    /// Manage `role.*` persona templates.
    PersonasManage,
    /// Persist shared LLM / cost configuration (`system.set_*`, `pricing.update`).
    ConfigManage,
    /// Create / update / delete / trigger the agent's heartbeat checks — the
    /// periodic, unattended self-checks (`heartbeat.{create,update,delete,trigger}`).
    /// Admin baseline, but delegable: a heartbeat check carries no per-user owner
    /// and its body runs detached as the fail-closed `"heartbeat"` principal, so
    /// authoring one grants no execution privilege — an admin may hand it to a
    /// non-admin role via the matrix without widening the shared-config surface.
    HeartbeatManage,
    /// Execute arbitrary shell commands or build/run code on the host
    /// (`exec.run`, the `code.*` build/test/lint/cargo/git runners,
    /// `wasm_skill.build`). The most privileged capability: a holder can read any
    /// file the process can (incl. the decrypted vault key from the environment),
    /// so it is admin-only and the audit's hardest line.
    ShellExec,
    /// Run a writing statement (DML/DDL) against a configured database instance
    /// via `database.execute`. Arbitrary writes to production customer data are
    /// admin-only; the per-instance `allow_writes` opt-in gates it further.
    DatabaseWrite,
    /// Make an outbound HTTP request that can change remote state — `http.request`
    /// with POST, PUT, PATCH, or DELETE. The read-only GET/HEAD forms stay ungated,
    /// at parity with `web.fetch`. Delegable: unlike [`Capability::ShellExec`] this
    /// grants no reach on the host, so an admin can hand a role the ability to
    /// drive external APIs without handing over the shell.
    NetworkWrite,
    /// Administer the device fleet — connect a device to someone else's account,
    /// reject/revoke device pairings (which mint bearer tokens + scopes), link an
    /// existing device, and invoke device capabilities. Admin-only: a pending
    /// request carries no verified owner, so this cannot be owner-scoped.
    /// Approving a code into one's own account is not this capability.
    DevicesManage,
    /// Mutate the shared, org-wide knowledge base (drop/prune a repo's graph +
    /// vector index). Admin-only: the index is a single shared resource with no
    /// per-user owner, so destruction must not be a mere self-serve confirm.
    KnowledgeManage,
    /// Read hub-wide operational data that spans every user (the in-memory log
    /// ring). Admin-only **read** — distinct from [`Capability::CrossuserRead`]
    /// (which is nobody's, reserved for the `system` principal): operators may
    /// read ops/diagnostics, but no human role may read another user's *data*.
    OpsRead,
    /// Create / mutate the shared, per-agent project pool — projects, their repo
    /// manifests, host clones/worktrees, and indexing. Admin-only: a `Project` is
    /// keyed `(agent_id, name)` with no per-user owner (a shared collaborative
    /// pool fed into the org index), so it cannot be owner-scoped; gating
    /// `add_repo`/`open_workspace` also closes the org-PAT private-source clone →
    /// `code.read` exfiltration chain. Read-only `projects.list`/`show` stay open.
    ProjectsManage,
    /// Export an agent bundle — every user's transcripts + memories + the
    /// referenced identities' emails for that agent. A bulk cross-user
    /// data-egress surface, so it is its own admin-only capability rather than
    /// folded into [`Capability::AgentsManage`].
    AgentExport,
    /// Import an agent bundle — writes config, transcripts and memories and
    /// creates/reconciles identities across every user in the bundle. Admin-only.
    AgentImport,
    /// Read another user's / channel's data.
    CrossuserRead,
}

impl Capability {
    /// Every capability, in matrix order — drives the [`CapabilitySummary`].
    pub const ALL: [Capability; 24] = [
        Capability::OwnDataRw,
        Capability::OwnDataDestructive,
        Capability::VaultSharedRead,
        Capability::VaultSharedWrite,
        Capability::VaultSharedDelete,
        Capability::AgentsManage,
        Capability::PermissionsManage,
        Capability::RolesAssign,
        Capability::SkillsManage,
        Capability::SkillsEnable,
        Capability::SkillsAuthor,
        Capability::PersonasManage,
        Capability::ConfigManage,
        Capability::HeartbeatManage,
        Capability::ShellExec,
        Capability::DatabaseWrite,
        Capability::NetworkWrite,
        Capability::DevicesManage,
        Capability::KnowledgeManage,
        Capability::OpsRead,
        Capability::ProjectsManage,
        Capability::AgentExport,
        Capability::AgentImport,
        Capability::CrossuserRead,
    ];

    /// Stable string key for this capability (matches the serde rename).
    pub fn key(self) -> &'static str {
        match self {
            Capability::OwnDataRw => "own_data_rw",
            Capability::OwnDataDestructive => "own_data_destructive",
            Capability::VaultSharedRead => "vault_shared_read",
            Capability::VaultSharedWrite => "vault_shared_write",
            Capability::VaultSharedDelete => "vault_shared_delete",
            Capability::AgentsManage => "agents_manage",
            Capability::PermissionsManage => "permissions_manage",
            Capability::RolesAssign => "roles_assign",
            Capability::SkillsManage => "skills_manage",
            Capability::SkillsEnable => "skills_enable",
            Capability::SkillsAuthor => "skills_author",
            Capability::PersonasManage => "personas_manage",
            Capability::ConfigManage => "config_manage",
            Capability::HeartbeatManage => "heartbeat_manage",
            Capability::ShellExec => "shell_exec",
            Capability::DatabaseWrite => "database_write",
            Capability::NetworkWrite => "network_write",
            Capability::DevicesManage => "devices_manage",
            Capability::KnowledgeManage => "knowledge_manage",
            Capability::OpsRead => "ops_read",
            Capability::ProjectsManage => "projects_manage",
            Capability::AgentExport => "agent_export",
            Capability::AgentImport => "agent_import",
            Capability::CrossuserRead => "crossuser_read",
        }
    }

    /// Inverse of [`Self::key`]: parse a capability key, `None` if unknown.
    pub fn from_key(s: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.key() == s)
    }

    /// Short human-readable title for the admin UI and the My Permissions page.
    pub fn label(self) -> &'static str {
        match self {
            Capability::OwnDataRw => "Own data",
            Capability::OwnDataDestructive => "Delete own data",
            Capability::VaultSharedRead => "Read shared secrets",
            Capability::VaultSharedWrite => "Write shared secrets",
            Capability::VaultSharedDelete => "Delete shared secrets",
            Capability::AgentsManage => "Manage agents",
            Capability::PermissionsManage => "Manage permissions",
            Capability::RolesAssign => "Assign roles & manage users",
            Capability::SkillsManage => "Install & manage skills",
            Capability::SkillsEnable => "Enable configured skills",
            Capability::SkillsAuthor => "Author your own skills",
            Capability::PersonasManage => "Manage role templates",
            Capability::ConfigManage => "Change shared configuration",
            Capability::HeartbeatManage => "Manage heartbeats",
            Capability::ShellExec => "Run shell & execute code",
            Capability::DatabaseWrite => "Write to databases",
            Capability::NetworkWrite => "Change remote state over HTTP",
            Capability::DevicesManage => "Administer devices",
            Capability::KnowledgeManage => "Manage knowledge base",
            Capability::OpsRead => "Read operational logs",
            Capability::ProjectsManage => "Manage projects",
            Capability::AgentExport => "Export agents",
            Capability::AgentImport => "Import agents",
            Capability::CrossuserRead => "Read other users' data",
        }
    }

    /// Coarse grouping for the catalog UI.
    pub fn category(self) -> &'static str {
        match self {
            Capability::OwnDataRw | Capability::OwnDataDestructive => "own_data",
            Capability::VaultSharedRead
            | Capability::VaultSharedWrite
            | Capability::VaultSharedDelete => "vault",
            Capability::ShellExec | Capability::DatabaseWrite | Capability::NetworkWrite => {
                "execution"
            }
            Capability::OpsRead
            | Capability::ConfigManage
            | Capability::HeartbeatManage
            | Capability::KnowledgeManage => "system",
            Capability::SkillsAuthor | Capability::SkillsEnable | Capability::SkillsManage => {
                "skills"
            }
            _ => "admin",
        }
    }

    /// A plain-language explanation of what holding this capability lets a role
    /// do — the single source of truth the admin tooltips and My Permissions page
    /// render. Answers "what does this control?" for every capability.
    pub fn description(self) -> &'static str {
        match self {
            Capability::OwnDataRw => {
                "Read and write the caller's own data — their per-user vault secrets, memories, \
                 schedules, and connected credentials."
            }
            Capability::OwnDataDestructive => {
                "Delete or drop the caller's own data. Always confirmed before it runs."
            }
            Capability::VaultSharedRead => "Read shared (team) and global vault secrets.",
            Capability::VaultSharedWrite => {
                "Create or update shared and global vault secrets, including org-wide LLM \
                 accounts, tokens, and the Slack app credentials."
            }
            Capability::VaultSharedDelete => "Delete shared and global vault secrets.",
            Capability::AgentsManage => "Create, update, and delete agents.",
            Capability::PermissionsManage => {
                "Manage permission rules, the capability matrix, approvers, and policy."
            }
            Capability::RolesAssign => "Assign roles to people and manage user accounts.",
            Capability::SkillsManage => {
                "Install, uninstall, or disable skills agent-wide, and connect or mutate MCP \
                 servers — the privileged skill lifecycle that adds code or credentials. For \
                 external (SKILL.md) skills it also means installing for everyone, installing \
                 from any directory on the hub or another git host, and managing every external \
                 skill."
            }
            Capability::SkillsEnable => {
                "Load an already-installed, already-credentialed skill at runtime. Member-allowed: \
                 it adds no new code or credential."
            }
            Capability::SkillsAuthor => {
                "Write, install, update, share, and publish your own skills, WASM and external \
                 (SKILL.md). Member-allowed: a WASM skill runs sandboxed and every platform tool \
                 it invokes is checked against the calling user's own capabilities, and an \
                 external skill is instructions the caller's agent carries out with the caller's \
                 own tools, so neither grants any reach its caller lacks. Skills owned by someone \
                 else stay untouchable unless they share them with you."
            }
            Capability::PersonasManage => "Create, update, and delete role.* persona templates.",
            Capability::ConfigManage => {
                "Persist shared LLM, routing, and cost configuration for every user."
            }
            Capability::HeartbeatManage => {
                "Create, update, delete, and trigger the agent's heartbeat checks — the periodic, \
                 unattended self-checks that run detached as the ownerless heartbeat principal."
            }
            Capability::ShellExec => {
                "Run arbitrary shell commands and build or execute code on the host — exec.run, the \
                 code.* cargo/build/test/lint/git runners, the coding.* Claude Code CLI runners, \
                 and wasm_skill.build — and use the tools a person's own machine offers over the \
                 node uplink (node.*) that it does not mark read-only. The most privileged \
                 capability: a holder can read any file the process can, including the decrypted \
                 vault key from the host environment."
            }
            Capability::DatabaseWrite => {
                "Run writing statements (DML/DDL) against a configured database via \
                 database.execute. Gated further by each instance's allow_writes opt-in."
            }
            Capability::NetworkWrite => {
                "Make outbound HTTP requests that can change remote state — http.request with \
                 POST, PUT, PATCH, or DELETE. Read-only GET and HEAD requests are always \
                 allowed. Credentials come from operator-configured endpoints and are only ever \
                 sent to the endpoint they are configured under, never to a URL the agent \
                 chooses."
            }
            Capability::DevicesManage => {
                "Administer the device fleet — connect a device to someone else's account, reject \
                 or revoke device pairings (which mint bearer tokens), link existing devices, and \
                 invoke device capabilities. Connecting a device to one's own account needs no \
                 capability."
            }
            Capability::KnowledgeManage => {
                "Delete or prune the shared, org-wide knowledge base index for a repository."
            }
            Capability::OpsRead => {
                "Read hub-wide operational data spanning every user — the in-process log ring \
                 (system.logs)."
            }
            Capability::ProjectsManage => {
                "Create, delete, and modify the shared per-agent project pool, its repositories, \
                 host checkouts, and indexing."
            }
            Capability::AgentExport => {
                "Export an agent bundle — every user's transcripts, memories, and the referenced \
                 emails for that agent. A bulk cross-user data egress, locked to System Admin."
            }
            Capability::AgentImport => {
                "Import an agent bundle — writes config, transcripts, and memories and reconciles \
                 identities across users. Locked to System Admin."
            }
            Capability::CrossuserRead => {
                "Read another user's or channel's data. Reserved for the confined system principal \
                 — never granted to any human role."
            }
        }
    }

    /// Representative tools this capability governs, for the catalog UI. Listed
    /// tools that resolve a capability from their name alone map back through
    /// [`tool_capability`]; arg-dependent and REST-only surfaces are described in
    /// [`Self::description`] instead.
    pub fn governed_tools(self) -> &'static [&'static str] {
        match self {
            Capability::OwnDataRw => &["vault.set", "vault.move"],
            Capability::OwnDataDestructive => &["vault.delete"],
            Capability::VaultSharedWrite => &[
                "system.add_account",
                "system.add_llm_token",
                "system.reload_slack",
                "slack.connect",
                "slack.app_configure",
                "slack.disconnect",
            ],
            Capability::AgentsManage => &["agent.create", "agent.update", "agent.delete"],
            Capability::PersonasManage => &["role.create", "role.update", "role.delete"],
            Capability::SkillsManage => &[
                "system.install_catalog_skill",
                "system.disable_skill",
                "mcp.install",
            ],
            Capability::SkillsEnable => &["system.enable_skill"],
            Capability::SkillsAuthor => &[
                "wasm_skill.install",
                "wasm_skill.import",
                "wasm_skill.update",
                "wasm_skill.uninstall",
                "system.install_external_skill",
                "system.uninstall_external_skill",
            ],
            Capability::ConfigManage => {
                &["system.set_model", "system.set_provider", "pricing.update"]
            }
            Capability::HeartbeatManage => &[
                "heartbeat.create",
                "heartbeat.update",
                "heartbeat.delete",
                "heartbeat.trigger",
            ],
            Capability::ShellExec => &[
                "exec.run",
                "code.cargo",
                "code.build",
                "code.test",
                "code.lint",
                "code.git",
                "coding.claude",
                "coding.session",
                "desk.sessions",
                "desk.start",
                "desk.prompt",
                "desk.read",
                "desk.wait",
                "desk.approve",
                "desk.stop",
                "desk.interrupt",
                "crew.overview",
                "crew.activity",
                "crew.history",
                "crew.spend",
                "crew.nudge",
                "crew.approve",
                "crew.deny",
                "crew.stop",
                "crew.interrupt",
                "crew.resume",
                "crew.restart",
                "crew.start_session",
                "crew.assign",
                "wasm_skill.build",
            ],
            Capability::DatabaseWrite => &["database.execute"],
            Capability::NetworkWrite => &["http.request"],
            Capability::DevicesManage => &[
                "pairing.revoke",
                "pairing.devices",
                "pairing.link",
                "node_invoke",
            ],
            Capability::KnowledgeManage => &["knowledge.delete"],
            Capability::OpsRead => &["system.logs"],
            Capability::ProjectsManage => &[
                "projects.create",
                "projects.delete",
                "projects.add_repo",
                "projects.index",
            ],
            Capability::RolesAssign => &["identity.lifecycle"],
            Capability::VaultSharedRead
            | Capability::VaultSharedDelete
            | Capability::PermissionsManage
            | Capability::AgentExport
            | Capability::AgentImport
            | Capability::CrossuserRead => &[],
        }
    }
}

/// The role's full capability scorecard: the role itself plus a stable-keyed
/// map of every capability → [`Decision`]. Serialized for `identity.whoami`
/// and `GET /api/me/capabilities`; capability booleans only, never secrets.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilitySummary {
    /// The role's string key — a built-in (`member`/`manager`/`system_admin`)
    /// or a custom-role key.
    pub role: String,
    /// Capability key → decision, in matrix order.
    pub capabilities: Vec<CapabilityDecision>,
}

/// One row of the capability scorecard.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityDecision {
    pub capability: &'static str,
    pub decision: Decision,
}

/// Score every capability for a `role_key` at `status` through the live matrix.
/// `role_key` is the caller's real role (a built-in `member`/`manager`/
/// `system_admin` or a custom-role key). A non-active (suspended) caller scores
/// through the `member` row — the same demotion [`CapabilityMatrix::decision`]
/// applies — so whoami / `GET /api/me/capabilities` never report elevated caps
/// for a suspended user.
pub fn capability_summary(
    role_key: &str,
    status: IdentityStatus,
    matrix: &CapabilityMatrix,
) -> CapabilitySummary {
    let capabilities = Capability::ALL
        .into_iter()
        .map(|cap| CapabilityDecision {
            capability: cap.key(),
            decision: matrix.decision(role_key, status, cap),
        })
        .collect();
    CapabilitySummary {
        role: role_key.to_string(),
        capabilities,
    }
}

/// The named-admin capabilities the matrix must never grant to a
/// non-`system_admin` role: bulk cross-user data egress
/// ([`Capability::AgentExport`] / [`Capability::AgentImport`]) and
/// [`Capability::CrossuserRead`] (reserved for the confined `system` principal).
/// Enforced as a hard floor in [`CapabilityMatrix::decision`], as a dropped row
/// in [`CapabilityMatrix::from_overrides`], and rejected at write time by the
/// matrix save handler — so generalizing the matrix to custom roles can never
/// widen this surface.
pub fn is_protected_admin_cap(cap: Capability) -> bool {
    matches!(
        cap,
        Capability::AgentExport | Capability::AgentImport | Capability::CrossuserRead
    )
}

/// The built-in baseline decision for a `role_key` / capability, before any admin
/// override. `system_admin` may do everything (own-destructive still confirms,
/// `crossuser_read` stays `system`-only); `manager` is a Member plus read-only
/// operational visibility ([`Capability::OpsRead`]); `member` and every custom
/// role self-serve their own data and are denied every shared / admin capability
/// until an admin grants it via the matrix.
pub fn baseline_for(role_key: &str, cap: Capability) -> Decision {
    match role_key {
        "system_admin" => admin_baseline(cap),
        "manager" => manager_baseline(cap),
        _ => member_baseline(cap),
    }
}

/// A Member self-serves their own data (destructive → Ask) and may enable an
/// already-configured skill; every shared / admin capability is a hard Deny.
fn member_baseline(cap: Capability) -> Decision {
    match cap {
        Capability::OwnDataRw | Capability::SkillsEnable | Capability::SkillsAuthor => {
            Decision::Allow
        }
        Capability::OwnDataDestructive => Decision::Ask,
        _ => Decision::Deny,
    }
}

/// A Manager is a Member plus read-only operational visibility — a distinct,
/// enforced row an admin can elevate further via the matrix.
fn manager_baseline(cap: Capability) -> Decision {
    match cap {
        Capability::OpsRead => Decision::Allow,
        other => member_baseline(other),
    }
}

/// An active SystemAdmin may do everything, except own-destructive still confirms
/// and `crossuser_read` stays reserved for the `system` principal.
fn admin_baseline(cap: Capability) -> Decision {
    match cap {
        Capability::CrossuserRead => Decision::Deny,
        Capability::OwnDataDestructive => Decision::Ask,
        _ => Decision::Allow,
    }
}

/// The Member (`is_admin == false`) or active SystemAdmin (`is_admin == true`)
/// baseline — a thin two-role wrapper over [`baseline_for`] kept for the
/// matrix-save minimization and the break-glass CLI.
pub fn decision_for(cap: Capability, is_admin: bool) -> Decision {
    baseline_for(if is_admin { "system_admin" } else { "member" }, cap)
}

// ──────────────────────── Action → capability registry ─────────────────────

/// Map a tool call `(name, input)` to the [`Capability`] it requires, or `None`
/// when the tool is not a privileged action (no gate). The single declarative
/// table the pre-dispatch [`crate::cap_gate::HubCapabilityGate`] consults — it
/// replaces every Increment-1 inline `reject_non_admin`.
///
/// The mapping mirrors each tool's *own* Increment-1 predicate exactly so the
/// gate hard-denies precisely what those inline checks denied — no broader (a
/// looser mapping would flip member-accessible reads, e.g. `agent.list`, to a
/// pre-prompt Deny, a regression Phase 1 must not introduce):
///
/// * the mutating `agent.{create,update,delete}` / `role.{create,update,delete}`
///   ops → admin capabilities; the `agent.list`/`agent.show`/`role.list`/… reads
///   stay ungated;
/// * `system.enable_skill` → `SkillsEnable` (member-allowed: load a configured
///   skill, no code/credential added); `disable_skill` and
///   `install_catalog_skill` → `SkillsManage` (admin-only);
///   `install_external_skill` → `SkillsAuthor`, or `SkillsManage` for a git URL
///   on another host; `uninstall_external_skill` → `SkillsAuthor`, with
///   per-skill ownership deciding inside; the shared credential `system.*`
///   tools (LLM accounts/tokens, `reload_slack`) → `VaultSharedWrite`;
/// * the MCP tools that connect/disconnect a server or mutate the registered
///   toolset (`mcp.{install,uninstall,set_tools,update}`) → `SkillsManage`;
///   the read-only / per-user / dormant-only siblings stay ungated;
/// * the shared-config `system.set_{model,provider,routing,failover_chain,`
///   `max_tokens,max_rounds}` tools, `system.config`'s `set`/`unset` and
///   `pricing.update` → `ConfigManage`; the read-only config/pricing tools
///   (`system.config` `get` among them) and session-only `set_session_model`
///   stay ungated;
/// * the code/command-execution tools (`exec.run`, the `code.*` cargo/build/test/
///   lint/git runners) → `ShellExec`; the read-only `code.*`
///   tools stay ungated. The wasm-skill lifecycle (`wasm_skill.{install,import,`
///   `update,uninstall}`) → `SkillsAuthor`, with per-skill ownership deciding
///   *which* skill, and a host compile escalating to `ShellExec` unless the
///   deployment sandboxes builds;
/// * the arg-aware vault tools pick by **scope**, reusing
///   [`crate::hub_core::vault_scopes::VaultScope`] /
///   [`workforce_common::settings::VaultSource`] rather than re-deriving tiers:
///   - `vault.set` mirrors `tier.requires_admin_to_write()` (Global only) →
///     `VaultSharedWrite`, else `OwnDataRw`;
///   - `vault.move` mirrors `authorize_move`'s "touches a shared tier" → either
///     endpoint non-per-user is `VaultSharedWrite`, else `OwnDataRw`;
///   - `vault.delete` keys off the path: an explicit `users/{uid}/…` path is
///     the caller's own per-user entry → `OwnDataDestructive`. A *bare* path may
///     resolve to a per-user tier (the self-service `vault.list`→delete flow), a
///     fact only the live store knows — so it is **never** hard-denied here
///     (`OwnDataDestructive`); the tool's own inline store check stays the
///     authority for the bare own-vs-shared split.
///
/// Any other tool that is destructive-by-name
/// ([`workforce_policy::rules::is_destructive`]) but unregistered defaults to
/// `OwnDataDestructive` (Ask) — secure-by-default, the single shared destructive
/// definition the `@destructive` cascade baseline also reads.
pub fn tool_capability(tool: &str, input: &serde_json::Value) -> Option<Capability> {
    match tool {
        // Agent lifecycle (mutating only — reads stay ungated).
        "agent.create" | "agent.update" | "agent.delete" => Some(Capability::AgentsManage),
        // Persona templates (mutating only).
        "role.create" | "role.update" | "role.delete" => Some(Capability::PersonasManage),
        // Merging, transferring from and deleting identities; the tool also
        // requires an active System Admin itself, holdings reads included.
        "identity.lifecycle" => Some(Capability::RolesAssign),
        // Enabling (loading) a configured skill at runtime is member-allowed
        // (`SkillsEnable`) — it adds no code and no credential, just loads tools
        // from creds the vault already holds, with per-user OAuth resolved at
        // call time. Disabling (unloads agent-wide) and a catalog install (for
        // everyone) stay admin-only (`SkillsManage`). An external skill is owned
        // like a WASM skill: installing and uninstalling one's own is authoring,
        // with ownership decided per skill inside the tool, and only a clone from
        // another git host needs `SkillsManage`.
        "system.enable_skill" => Some(Capability::SkillsEnable),
        "system.disable_skill" | "system.install_catalog_skill" => Some(Capability::SkillsManage),
        "system.install_external_skill" => Some(
            crate::hub_core::capabilities::external_install_capability(input),
        ),
        "system.uninstall_external_skill" => Some(Capability::SkillsAuthor),
        // MCP server management — connecting an external server (which can carry
        // upstream auth) or mutating the agent's registered toolset/allowlists is
        // the same capability class as skills. The read-only siblings (`mcp.list`,
        // `mcp.show`, `mcp.list_presets`, `mcp.list_remote_tools`,
        // `mcp.workspace_list`), the per-user OAuth consent flow (`mcp.authorize`,
        // own-data), and `mcp.workspace_add` (adds a server as DORMANT only — a
        // restriction, never an install or toolset change) stay ungated.
        "mcp.install" | "mcp.uninstall" | "mcp.set_tools" | "mcp.update" => {
            Some(Capability::SkillsManage)
        }
        // Shared LLM / cost configuration — the `system.set_*` tools persist the
        // active model/provider/routing/failover/limits to the shared workspace
        // config, and `pricing.update` persists shared cost data. The read-only
        // config/pricing tools (`system.llm_status`, `system.list_models`,
        // `system.routing_status`, `pricing.list`, …) and the session-only
        // `system.set_session_model` override stay ungated.
        "system.set_model"
        | "system.set_provider"
        | "system.set_routing"
        | "system.set_failover_chain"
        | "system.set_max_tokens"
        | "system.set_max_rounds"
        | "pricing.update" => Some(Capability::ConfigManage),
        // Its own config.toml, read redacted by anyone, changed like the
        // setters above; an action it does not know is treated as a change.
        "system.config" => match input.get("action").and_then(|a| a.as_str()) {
            Some("get") => None,
            _ => Some(Capability::ConfigManage),
        },
        // Heartbeat checks are agent-level self-checks with **no per-user owner**
        // — a body runs detached as the non-identity `"heartbeat"` pseudo-user
        // (which fail-closes to Member, so it reaches no exec/admin tools). The
        // mutating tools carry their own [`Capability::HeartbeatManage`]: an
        // admin baseline that, unlike the shared-config `ConfigManage`, an admin
        // can delegate to a non-admin role without also handing over the LLM /
        // pricing tools. `heartbeat.read`/`.list` stay ungated.
        "heartbeat.create" | "heartbeat.update" | "heartbeat.delete" | "heartbeat.trigger" => {
            Some(Capability::HeartbeatManage)
        }
        // Device fleet administration. `pairing.{approve,reject,revoke}` mint /
        // tear down device bearer tokens + scopes, `pairing.devices` lists the
        // whole approved fleet, and `pairing.pending` leaks the approval `code`;
        // `node_invoke` triggers a device capability (camera/location/notify) by
        // broadcasting to ALL connected clients. None of these carry a verified
        // owner (pending requests have only an unverified `identity_hint`,
        // approved devices have no owner field, the invoke is an untargeted
        // broadcast), so owner-scoping is not expressible — they are admin-only.
        "pairing.reject" | "pairing.revoke" | "pairing.devices" | "pairing.pending"
        | "pairing.link" | "node_invoke" => Some(Capability::DevicesManage),
        // Approving a code connects the device to the account of the person
        // asking — their own, which any member may do (the tool refuses a turn
        // with no person, and `[pairing] who_can_connect = "admins"` refuses
        // members). Connecting it to someone else (`for`) is fleet admin.
        "pairing.approve" => input
            .get("for")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.trim().is_empty())
            .then_some(Capability::DevicesManage),
        // `knowledge.delete` drops/prunes the **shared, org-wide** knowledge index
        // (graph + vector store) for a repo — a single resource with no per-user
        // owner, so the default `is_destructive` → `OwnDataDestructive` (Ask) is
        // insufficient (it would let any member wipe shared retrieval behind a
        // mere confirm). The routine `knowledge.index`/`refresh_index`/status
        // (re)indexing stays ungated (constitutional Allow).
        "knowledge.delete" => Some(Capability::KnowledgeManage),
        // `system.logs` reads the process-wide log ring spanning every user (and
        // any secret that leaked into a log line) — an admin ops read.
        // `settings` shows config values and where they come from, a credential
        // only as set or not, so it stays ungated.
        "system.logs" => Some(Capability::OpsRead),
        // Project pool management. A `Project` is keyed `(agent_id, name)` with no
        // per-user owner — a shared collaborative pool fed into the org-wide
        // index — so these cannot be owner-scoped; they are admin-only. Gating
        // `add_repo`/`open_workspace` also cuts the exfiltration chain (clone any
        // private repo the org PAT can reach → read it via the ungated `code.*`
        // readers). `index` PAT-pulls fresh source so it is gated too (unlike the
        // ungated `knowledge.index`/`refresh_index`, which reindex an existing
        // checkout). This arm precedes the `is_destructive` fallback so
        // `projects.delete`/`remove_repo` upgrade from `OwnDataDestructive` (Ask)
        // to a hard admin Deny (they touch shared state, not own data). The
        // read-only `projects.list`/`show` stay ungated.
        "projects.create"
        | "projects.delete"
        | "projects.rename"
        | "projects.update"
        | "projects.add_repo"
        | "projects.remove_repo"
        | "projects.open_workspace"
        | "projects.index" => Some(Capability::ProjectsManage),
        // Shared-credential management (org/shared vault writes + the Slack
        // workspace-token reload, which the Increment-1 gate guards as a
        // shared-vault write). `slack.connect` (and its earlier name
        // `slack.app_configure`) and `slack.disconnect` write or delete an
        // agent's Slack bot/app/signing tokens (`slack.{ws}.bot_token` etc.,
        // no per-user owner) and rewrite the Slack skill config — the same
        // shared-credential write class; the per-user
        // `slack.user_token_{set,clear}` (which use `set_with_actor(User)`)
        // stay own-data, and `slack.app_scaffold` / `slack.status` only emit a
        // manifest template or read state, so they stay ungated.
        "system.add_account"
        | "system.remove_account"
        | "system.add_llm_token"
        | "system.remove_llm_token"
        | "system.reload_slack"
        | "slack.connect"
        | "slack.app_configure"
        | "slack.disconnect" => Some(Capability::VaultSharedWrite),
        // Arbitrary code / command execution on the host. `exec.run` is an
        // unconfined shell (its `workdir` arg is sandboxed, the command string is
        // not — it can read any path, dump the vault key from the environment, or
        // `rm -rf`). The `code.*` runners shell out to `cargo`/`git` inside a
        // worktree, and `cargo build|test|lint` executes that crate's `build.rs` /
        // test code as the host process — same host-exec reach. A Member can plant
        // a malicious `build.rs` with the (worktree-confined) `filesystem.write` /
        // `code.edit` and then run it, so the write tools stay member-accessible
        // and the *execution* is the gated privilege. The read-only `code.*`
        // tools (`code.read`/`search`/`tree`/`symbols`/`diff`/`find`) stay ungated.
        // `coding.claude` / `coding.session` spawn the Claude Code CLI with its
        // `Bash` tool enabled — an unconfined host shell, the exact reach
        // `exec.run` has — so they belong to this class. Gating only `exec.run`
        // would leave the identical escalation one tool over.
        // `desk.*` drives the person's own Claude Code / Codex terminals, and
        // reads them — the same host reach.
        "exec.run" | "code.cargo" | "code.build" | "code.test" | "code.lint" | "code.git"
        | "coding.claude" | "coding.session" | "desk.sessions" | "desk.start" | "desk.prompt"
        | "desk.read" | "desk.wait" | "desk.approve" | "desk.stop" | "desk.interrupt"
        // The crew overseer reads and drives the same terminals.
        | "crew.overview" | "crew.activity" | "crew.history" | "crew.spend" | "crew.nudge" | "crew.approve"
        | "crew.deny" | "crew.stop" | "crew.interrupt" | "crew.resume" | "crew.restart" | "crew.start_session"
        | "crew.assign" => Some(Capability::ShellExec),
        // A language server loads the project the way a build does (build
        // scripts, proc macros, compiler plugins), so every `code.refactor`
        // action it answers is this class too. Only `replace_definition`, a
        // parse-and-write confined like `code.edit`, stays ungated; an
        // unreadable action is gated.
        "code.refactor" => crate::hub_core::capabilities::runs_language_server(input)
            .then_some(Capability::ShellExec),
        // Direct database writes — arbitrary DML/DDL against a configured
        // instance. The per-instance `allow_writes` opt-in gates it further; the
        // read-only `database.*` tools stay ungated.
        "database.execute" => Some(Capability::DatabaseWrite),
        // Outbound HTTP. Read-only `GET`/`HEAD` stay ungated (parity with the
        // ungated `web.fetch`); the state-changing methods are the privilege.
        // The method lives in the arguments, so this reads the same predicate
        // the tool itself uses — an unparseable or absent method is treated as
        // mutating, which fails closed and which the tool rejects anyway.
        // `workforce_policy::rules::is_destructive` classifies on the tool NAME,
        // so it cannot see `method: "DELETE"`; this arm is what covers it.
        "http.request" => workforce_agent::tools::http::is_mutating_request(input)
            .then_some(Capability::NetworkWrite),
        // WASM skill lifecycle. A skill is an owned resource: which skills the
        // caller may touch is decided per skill by ownership
        // ([`crate::skill_access`]), so this cell only asks whether they may
        // author at all. The privilege that survives that is *compiling on the
        // host*: `wasm_skill.build` — and an install that has to build rather than
        // move prebuilt bytes — runs a project's `build.rs` and proc-macros as the
        // hub process, so it needs `ShellExec` unless the deployment compiles
        // under the privilege-separated runner. `update` can't be classified from
        // its input (it re-fetches the recorded source), so it maps to the author
        // cell and re-checks compile authority inline, as `install` does.
        // `wasm_skill.scaffold` (writes a template project, installs nothing), the
        // read-only `wasm_skill.list` / `wasm_skill.export` / `wasm_skill.guide`,
        // and `wasm_skill.test` (mocks only — strictly less reach than calling the
        // skill's registered tool) stay ungated.
        "wasm_skill.build" => Some(crate::hub_core::skill_builds::build_capability(
            crate::hub_core::skill_builds::build_isolation(),
        )),
        "wasm_skill.install" => Some(crate::hub_core::skill_builds::install_capability(
            input,
            crate::hub_core::skill_builds::build_isolation(),
        )),
        "wasm_skill.uninstall" | "wasm_skill.update" | "wasm_skill.import" => {
            Some(Capability::SkillsAuthor)
        }
        // Arg-aware vault tools.
        "vault.set" => Some(vault_set_capability(input)),
        "vault.move" => Some(vault_move_capability(input)),
        "vault.delete" => Some(Capability::OwnDataDestructive),
        // `claude_code.setup_token`, `codex.setup_auth` and `chatgpt.login`
        // store a subscription login. The default `scope='user'` writes a
        // per-user key (own data, ungated); only an explicit
        // `scope='agent_shared'` writes the *shared* agent login (a
        // shared-credential write). Mirror the tools' own scope branch so the
        // gate denies a Member exactly the shared write.
        "claude_code.setup_token" | "codex.setup_auth" | "chatgpt.login" => {
            setup_token_capability(input)
        }
        // The remote-only consent tools act on the caller's own grants and
        // skills, with owner/admin standing checked inside; `skill.revoke`
        // withdraws the caller's own consent, not data, and `skill.transfer`
        // gives away a skill the caller already owns, so neither is a
        // destructive-by-name Ask. Named rather than left to fall through: the
        // fallback would gate them the day a verb they happen to contain joins
        // `is_destructive`, and nothing else would notice.
        "skill.grant" | "skill.grants" | "skill.revoke" | "skill.share" | "skill.unshare"
        | "skill.visibility" | "skill.transfer" => None,
        // A tool a person's own machine offers over the node uplink runs
        // there with whatever reach its server has, which the hub cannot
        // see: the command-running class, unless its node says it only
        // reads.
        other if other.starts_with(crate::hub_core::local_agent_registry::NODE_TOOL_PREFIX) => {
            (!crate::hub_core::local_agent_registry::is_read_only_tool(other))
                .then_some(Capability::ShellExec)
        }
        // Secure-by-default: an unregistered destructive-by-name tool confirms
        // on the caller's own data (Ask). One shared destructive definition.
        other if workforce_policy::rules::is_destructive(other) => {
            Some(Capability::OwnDataDestructive)
        }
        _ => None,
    }
}

/// `vault.set` requires admin for a tier that
/// [`VaultSource::requires_admin_to_write`] — i.e. the `global` (T4) tier —
/// and for a subscription login in any shared tier, the login everyone on
/// the agent runs on. An absent or per-user scope is the caller's own data.
/// Mirrors the inline `tier.requires_admin_to_write()` predicate in
/// `VaultSetTool::execute` and the guarded store's shared-login check.
fn vault_set_capability(input: &serde_json::Value) -> Capability {
    let login = input
        .get("path")
        .and_then(|path| path.as_str())
        .and_then(workforce_common::subscription::SubscriptionKind::of_vault_key)
        .is_some();
    match parse_vault_scope(input.get("scope")).map(|scope| scope.to_tier()) {
        Some(tier) if tier.requires_admin_to_write() || (login && !tier.is_per_user()) => {
            Capability::VaultSharedWrite
        }
        _ => Capability::OwnDataRw,
    }
}

/// `claude_code.setup_token` is a shared-credential write only for an explicit
/// `scope='agent_shared'` (the shared `claude_code/oauth_token` key any user of
/// the agent then drives Claude Code under). An **absent** scope is deliberately
/// *not* mapped to a shared write, and that is safe — not a gap — because of a
/// load-bearing invariant: the tool resolves an absent scope to per-user when a
/// user identity is in scope and to `agent_shared` only when there is none
/// (`scope.unwrap_or(match uid { Some => User, None => AgentShared })`), and the
/// gate reads the **same** `CURRENT_USER_ID` task-local it would. So:
///   * gate runs (user in scope) ⟺ the tool picks per-user (own data) ⟹ ungated
///     is correct, and mapping absent→shared here would instead *over-deny* the
///     recommended per-user setup a Member is entitled to;
///   * the only path an absent scope writes the shared key is with no user in
///     scope, where the gate early-returns ungated anyway (the system-principal
///     path), so no Member ever reaches it.
///
/// Mapping only the explicit shared scope is therefore both correct and minimal.
fn setup_token_capability(input: &serde_json::Value) -> Option<Capability> {
    match input.get("scope").and_then(|v| v.as_str()) {
        Some("agent_shared") => Some(Capability::VaultSharedWrite),
        _ => None,
    }
}

/// `vault.move` requires admin when it touches a shared tier (either endpoint is
/// not per-user) — the exact `authorize_move` predicate. A move that stays
/// within the per-user tiers is own-data. Unparseable tiers fall through to
/// own-data: the tool's own `serde` parse + inline `authorize_move` remain the
/// authority, so the gate must not over-deny on a shape it can't read.
fn vault_move_capability(input: &serde_json::Value) -> Capability {
    let from = parse_vault_scope(input.get("from_tier"));
    let to = parse_vault_scope(input.get("to_tier"));
    match (from, to) {
        (Some(f), Some(t)) if !f.to_tier().is_per_user() || !t.to_tier().is_per_user() => {
            Capability::VaultSharedWrite
        }
        _ => Capability::OwnDataRw,
    }
}

/// Reuse the tool-facing [`VaultScope`](crate::hub_core::vault_scopes::VaultScope) parsing
/// (including its serde aliases) so the registry and the tools agree on what a
/// scope string means — no duplicated tier table.
fn parse_vault_scope(
    value: Option<&serde_json::Value>,
) -> Option<crate::hub_core::vault_scopes::VaultScope> {
    value
        .cloned()
        .and_then(|v| serde_json::from_value::<crate::hub_core::vault_scopes::VaultScope>(v).ok())
}

/// Whether `uid` names an active administrator in `service`'s identity store.
pub fn is_admin(service: &crate::serve::AgentService, uid: &str) -> bool {
    let Some(store) = service.identity_store.read().clone() else {
        return false;
    };
    uuid::Uuid::parse_str(uid)
        .ok()
        .and_then(|u| store.get_by_id(u).ok().flatten())
        // The shared decision, not a copy of it: `is_effective_admin` also
        // requires an Active status, so a suspended admin is denied here too.
        .map(|i| is_effective_admin(i.role, i.status))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(summary: &CapabilitySummary, key: &str) -> Decision {
        summary
            .capabilities
            .iter()
            .find(|c| c.capability == key)
            .unwrap_or_else(|| panic!("capability {key} missing"))
            .decision
    }

    #[test]
    fn member_summary_denies_shared_and_admin_capabilities() {
        let s = summary(IdentityRole::Member, IdentityStatus::Active);
        assert_eq!(s.role, "member");
        assert_eq!(decision(&s, "own_data_rw"), Decision::Allow);
        assert_eq!(decision(&s, "own_data_destructive"), Decision::Ask);
        assert_eq!(decision(&s, "vault_shared_read"), Decision::Deny);
        assert_eq!(decision(&s, "vault_shared_write"), Decision::Deny);
        assert_eq!(decision(&s, "vault_shared_delete"), Decision::Deny);
        assert_eq!(decision(&s, "agents_manage"), Decision::Deny);
        assert_eq!(decision(&s, "permissions_manage"), Decision::Deny);
        assert_eq!(decision(&s, "roles_assign"), Decision::Deny);
        assert_eq!(decision(&s, "skills_manage"), Decision::Deny);
        // Enabling a configured skill is member-allowed (the install/disable
        // lifecycle stays admin-only under skills_manage).
        assert_eq!(decision(&s, "skills_enable"), Decision::Allow);
        assert_eq!(decision(&s, "personas_manage"), Decision::Deny);
        assert_eq!(decision(&s, "config_manage"), Decision::Deny);
        assert_eq!(decision(&s, "crossuser_read"), Decision::Deny);
    }

    #[test]
    fn manager_is_a_distinct_enforced_role() {
        // Manager is a Member plus read-only operational visibility — no longer a
        // silent collapse to Member.
        let manager = summary(IdentityRole::Manager, IdentityStatus::Active);
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        assert_eq!(decision(&manager, "ops_read"), Decision::Allow);
        assert_eq!(decision(&member, "ops_read"), Decision::Deny);
        // Every other admin capability is still denied for a Manager.
        for key in [
            "shell_exec",
            "vault_shared_delete",
            "agents_manage",
            "config_manage",
            "database_write",
        ] {
            assert_eq!(decision(&manager, key), Decision::Deny, "manager {key}");
        }
        // Own data and skill-enable match Member exactly.
        assert_eq!(decision(&manager, "own_data_rw"), Decision::Allow);
        assert_eq!(decision(&manager, "own_data_destructive"), Decision::Ask);
        assert_eq!(decision(&manager, "skills_enable"), Decision::Allow);
    }

    #[test]
    fn custom_role_defaults_to_member_until_granted() {
        // An unconfigured custom role scores exactly like a Member.
        let reviewer = capability_summary(
            "reviewer",
            IdentityStatus::Active,
            &CapabilityMatrix::default(),
        );
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let pairs = |s: &CapabilitySummary| {
            s.capabilities
                .iter()
                .map(|c| (c.capability, c.decision))
                .collect::<Vec<_>>()
        };
        assert_eq!(pairs(&reviewer), pairs(&member));
        assert_eq!(reviewer.role, "reviewer");
    }

    #[test]
    fn system_admin_summary_allows_shared_and_admin_capabilities() {
        let s = summary(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(s.role, "system_admin");
        assert_eq!(decision(&s, "own_data_rw"), Decision::Allow);
        assert_eq!(decision(&s, "own_data_destructive"), Decision::Ask);
        assert_eq!(decision(&s, "vault_shared_read"), Decision::Allow);
        assert_eq!(decision(&s, "vault_shared_write"), Decision::Allow);
        assert_eq!(decision(&s, "vault_shared_delete"), Decision::Allow);
        assert_eq!(decision(&s, "agents_manage"), Decision::Allow);
        assert_eq!(decision(&s, "permissions_manage"), Decision::Allow);
        assert_eq!(decision(&s, "roles_assign"), Decision::Allow);
        assert_eq!(decision(&s, "skills_manage"), Decision::Allow);
        assert_eq!(decision(&s, "skills_enable"), Decision::Allow);
        assert_eq!(decision(&s, "personas_manage"), Decision::Allow);
        assert_eq!(decision(&s, "config_manage"), Decision::Allow);
        // Cross-user read stays Deny even for an admin — it's `system`-only.
        assert_eq!(decision(&s, "crossuser_read"), Decision::Deny);
    }

    #[test]
    fn all_capabilities_scored_in_matrix_order() {
        let s = summary(IdentityRole::Member, IdentityStatus::Active);
        let keys: Vec<&str> = s.capabilities.iter().map(|c| c.capability).collect();
        let expected: Vec<&str> = Capability::ALL.into_iter().map(|c| c.key()).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn config_manage_appears_in_matrix_member_deny_admin_allow() {
        // The new shared-config capability flows into the matrix off
        // `Capability::ALL`: a Member is denied, an active SystemAdmin allowed.
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let admin = summary(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(decision(&member, "config_manage"), Decision::Deny);
        assert_eq!(decision(&admin, "config_manage"), Decision::Allow);
    }

    #[test]
    fn database_execute_is_admin_only_and_reads_ungated() {
        assert_eq!(
            tool_capability("database.execute", &serde_json::json!({})),
            Some(Capability::DatabaseWrite)
        );
        assert_eq!(
            tool_capability("database.query", &serde_json::json!({})),
            None
        );
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let admin = summary(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(decision(&member, "database_write"), Decision::Deny);
        assert_eq!(decision(&admin, "database_write"), Decision::Allow);
    }

    #[test]
    fn http_request_gates_on_the_method_and_fails_closed() {
        // Read-only methods stay ungated, at parity with `web.fetch`; every
        // state-changing method needs the capability.
        for read in ["GET", "HEAD", "get", " head "] {
            assert_eq!(
                tool_capability("http.request", &serde_json::json!({ "method": read })),
                None,
                "{read} must stay ungated"
            );
        }
        for write in ["POST", "PUT", "PATCH", "DELETE", "delete"] {
            assert_eq!(
                tool_capability("http.request", &serde_json::json!({ "method": write })),
                Some(Capability::NetworkWrite),
                "{write} must require network_write"
            );
        }
        // An absent or unparseable method fails closed. `is_destructive` reads
        // the tool NAME only, so nothing else would have caught `DELETE`.
        for unclear in [
            serde_json::json!({}),
            serde_json::json!({ "method": "NONSENSE" }),
            serde_json::json!({ "method": 7 }),
        ] {
            assert_eq!(
                tool_capability("http.request", &unclear),
                Some(Capability::NetworkWrite),
                "{unclear} must fail closed"
            );
        }
        assert!(!workforce_policy::rules::is_destructive("http.request"));
        // Listing endpoints is a read.
        assert_eq!(
            tool_capability("http.endpoints", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn network_write_is_admin_by_default_and_delegable() {
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let admin = summary(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(decision(&member, "network_write"), Decision::Deny);
        assert_eq!(decision(&admin, "network_write"), Decision::Allow);
        // Unlike bulk cross-user egress, an admin may hand this to another role.
        assert!(!is_protected_admin_cap(Capability::NetworkWrite));
    }

    #[test]
    fn setup_token_is_shared_write_only_for_agent_shared_scope() {
        // Per-user (default / explicit `user`) is own-data → ungated; only the
        // explicit `agent_shared` scope writes the shared key → VaultSharedWrite.
        assert_eq!(
            tool_capability("claude_code.setup_token", &serde_json::json!({})),
            None,
            "bare setup_token (per-user default) must be ungated"
        );
        assert_eq!(
            tool_capability(
                "claude_code.setup_token",
                &serde_json::json!({"scope": "user"})
            ),
            None,
            "scope=user must be ungated"
        );
        assert_eq!(
            tool_capability(
                "claude_code.setup_token",
                &serde_json::json!({"scope": "agent_shared"})
            ),
            Some(Capability::VaultSharedWrite),
            "scope=agent_shared writes the shared key → admin-only"
        );
    }

    #[test]
    fn shared_subscription_logins_are_shared_writes() {
        for tool in ["codex.setup_auth", "chatgpt.login"] {
            assert_eq!(
                tool_capability(tool, &serde_json::json!({})),
                None,
                "{tool}"
            );
            assert_eq!(
                tool_capability(tool, &serde_json::json!({"scope": "agent_shared"})),
                Some(Capability::VaultSharedWrite),
                "{tool}"
            );
        }
        for path in ["codex/auth_json", "claude_code/oauth_token"] {
            assert_eq!(
                tool_capability(
                    "vault.set",
                    &serde_json::json!({"path": path, "scope": "agent_specific"})
                ),
                Some(Capability::VaultSharedWrite),
                "{path}"
            );
            assert_eq!(
                tool_capability(
                    "vault.set",
                    &serde_json::json!({"path": path, "scope": "agent_per_user"})
                ),
                Some(Capability::OwnDataRw),
                "{path}"
            );
        }
    }

    #[test]
    fn device_knowledge_ops_caps_appear_in_matrix_member_deny_admin_allow() {
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let admin = summary(IdentityRole::SystemAdmin, IdentityStatus::Active);
        for key in [
            "devices_manage",
            "knowledge_manage",
            "ops_read",
            "projects_manage",
        ] {
            assert_eq!(decision(&member, key), Decision::Deny, "member {key}");
            assert_eq!(decision(&admin, key), Decision::Allow, "admin {key}");
        }
    }

    #[test]
    fn shell_exec_appears_in_matrix_member_deny_admin_allow() {
        // The code/command-execution capability is the audit's hardest line:
        // Member denied, active SystemAdmin allowed.
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let admin = summary(IdentityRole::SystemAdmin, IdentityStatus::Active);
        assert_eq!(decision(&member, "shell_exec"), Decision::Deny);
        assert_eq!(decision(&admin, "shell_exec"), Decision::Allow);
    }

    /// A tool on a person's own machine runs commands there unless its node
    /// says it only reads, and an unknown one is assumed to.
    #[tokio::test]
    async fn a_node_tool_is_command_running_unless_its_node_marks_it_read_only() {
        use crate::hub_core::local_agent_registry::{LocalAgentRegistry, NodeHello};
        use workforce_gateway_client::protocol::NodeToolSpec;
        let tool = |name: &str, read_only| NodeToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only,
        };
        let registry = LocalAgentRegistry::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        registry
            .register(
                NodeHello::agent(
                    "authz-mac".into(),
                    "Mac".into(),
                    vec![],
                    vec![tool("files_read", true), tool("desk_start", false)],
                ),
                "alice".into(),
                tx,
                vec![],
            )
            .await
            .unwrap();
        let none = serde_json::json!({});
        assert_eq!(tool_capability("node.authz-mac.files_read", &none), None);
        for gated in [
            "node.authz-mac.desk_start",
            "node.authz-mac.unknown",
            "node.x.y",
        ] {
            assert_eq!(
                tool_capability(gated, &none),
                Some(Capability::ShellExec),
                "{gated}"
            );
        }
    }

    #[test]
    fn suspended_system_admin_scores_as_non_admin() {
        // A suspended SystemAdmin keeps the role string (factual) but loses
        // every admin/shared capability — the same decision the gate makes.
        let suspended = summary(IdentityRole::SystemAdmin, IdentityStatus::Suspended);
        let member = summary(IdentityRole::Member, IdentityStatus::Active);
        let pairs = |s: &CapabilitySummary| {
            s.capabilities
                .iter()
                .map(|c| (c.capability, c.decision))
                .collect::<Vec<_>>()
        };
        assert_eq!(suspended.role, "system_admin");
        assert_eq!(pairs(&suspended), pairs(&member));
        assert_eq!(decision(&suspended, "vault_shared_delete"), Decision::Deny);
        assert_eq!(decision(&suspended, "agents_manage"), Decision::Deny);
    }

    #[test]
    fn is_effective_admin_requires_active_status() {
        assert!(is_effective_admin(
            IdentityRole::SystemAdmin,
            IdentityStatus::Active
        ));
        assert!(!is_effective_admin(
            IdentityRole::SystemAdmin,
            IdentityStatus::Suspended
        ));
        assert!(!is_effective_admin(
            IdentityRole::SystemAdmin,
            IdentityStatus::AutoProvisioned
        ));
        assert!(!is_effective_admin(
            IdentityRole::Member,
            IdentityStatus::Active
        ));
    }

    #[test]
    fn is_effective_manager_requires_a_team_role_and_active_status() {
        for role in [IdentityRole::SystemAdmin, IdentityRole::Manager] {
            assert!(is_effective_manager(role, IdentityStatus::Active));
            assert!(!is_effective_manager(role, IdentityStatus::Suspended));
            assert!(!is_effective_manager(role, IdentityStatus::AutoProvisioned));
        }
        assert!(!is_effective_manager(
            IdentityRole::Member,
            IdentityStatus::Active
        ));
    }

    #[test]
    fn never_admin_and_always_admin_defaults() {
        assert!(!(never_admin())());
        assert!((always_admin())());
    }

    // ───────────────────── authorize / enforce ────────────────────────────

    fn principal(role: IdentityRole, status: IdentityStatus) -> Principal {
        Principal {
            identity_id: Some(uuid::Uuid::nil()),
            role,
            role_key: role.as_str().to_string(),
            status,
        }
    }

    fn member() -> Principal {
        principal(IdentityRole::Member, IdentityStatus::Active)
    }

    fn admin() -> Principal {
        principal(IdentityRole::SystemAdmin, IdentityStatus::Active)
    }

    fn suspended_admin() -> Principal {
        principal(IdentityRole::SystemAdmin, IdentityStatus::Suspended)
    }

    fn custom(role_key: &str, status: IdentityStatus) -> Principal {
        Principal {
            identity_id: Some(uuid::Uuid::nil()),
            role: IdentityRole::from_str_lossy(role_key),
            role_key: role_key.to_string(),
            status,
        }
    }

    #[test]
    fn authorize_matches_the_matrix_per_role_and_status() {
        // Own data is available to every role; destructive on own data is Ask.
        for p in [member(), admin(), suspended_admin()] {
            assert_eq!(authz(&p, Capability::OwnDataRw), Decision::Allow);
            assert_eq!(authz(&p, Capability::OwnDataDestructive), Decision::Ask);
        }
        // Every shared/admin capability: Allow only for an ACTIVE admin.
        let admin_caps = [
            Capability::VaultSharedRead,
            Capability::VaultSharedWrite,
            Capability::VaultSharedDelete,
            Capability::AgentsManage,
            Capability::PermissionsManage,
            Capability::RolesAssign,
            Capability::SkillsManage,
            Capability::PersonasManage,
            Capability::ConfigManage,
            Capability::ShellExec,
            Capability::DevicesManage,
            Capability::KnowledgeManage,
            Capability::OpsRead,
        ];
        for cap in admin_caps {
            assert_eq!(authz(&admin(), cap), Decision::Allow, "{cap:?}");
            assert_eq!(authz(&member(), cap), Decision::Deny, "{cap:?}");
            // A suspended admin scores as a Member — the residual the design
            // closes by carrying live status in the Principal.
            assert_eq!(authz(&suspended_admin(), cap), Decision::Deny, "{cap:?}");
        }
        // Cross-user read is never a human capability — Deny for both roles.
        assert_eq!(authz(&admin(), Capability::CrossuserRead), Decision::Deny);
        assert_eq!(authz(&member(), Capability::CrossuserRead), Decision::Deny);
    }

    #[test]
    fn enforce_errs_only_on_hard_deny() {
        assert!(enf(&member(), Capability::OwnDataRw).is_ok());
        assert!(enf(&member(), Capability::OwnDataDestructive).is_ok()); // Ask ⇒ Ok
        assert_eq!(
            enf(&member(), Capability::AgentsManage),
            Err(Decision::Deny)
        );
        assert!(enf(&admin(), Capability::AgentsManage).is_ok());
        assert_eq!(
            enf(&suspended_admin(), Capability::VaultSharedDelete),
            Err(Decision::Deny)
        );
    }

    /// `HeartbeatManage` is admin-only by default yet delegable: it is not on the
    /// protected floor, so an admin can grant it to a non-admin role and that
    /// grant alone unlocks heartbeat management without touching shared config.
    #[test]
    fn heartbeat_manage_is_admin_default_but_delegable() {
        assert!(!is_protected_admin_cap(Capability::HeartbeatManage));
        assert_eq!(
            authz(&member(), Capability::HeartbeatManage),
            Decision::Deny
        );
        assert_eq!(
            authz(&admin(), Capability::HeartbeatManage),
            Decision::Allow
        );

        let matrix = CapabilityMatrix::from_overrides(&[override_(
            "member",
            "heartbeat_manage",
            StoredDecision::Allow,
        )]);
        assert_eq!(
            matrix.decision(
                "member",
                IdentityStatus::Active,
                Capability::HeartbeatManage
            ),
            Decision::Allow
        );
        // The grant is scoped to heartbeats — shared config stays admin-only.
        assert_eq!(
            matrix.decision("member", IdentityStatus::Active, Capability::ConfigManage),
            Decision::Deny
        );
    }

    // ───────────────── tool_capability registry mappings ──────────────────

    /// Every flat admin tool maps to an admin capability — a Member is denied,
    /// an active SystemAdmin allowed. A typo'd match arm fails here.
    #[test]
    fn flat_admin_tools_deny_member_allow_admin() {
        let no_input = serde_json::json!({});
        let cases: &[(&str, Capability)] = &[
            ("agent.create", Capability::AgentsManage),
            ("agent.update", Capability::AgentsManage),
            ("agent.delete", Capability::AgentsManage),
            ("role.create", Capability::PersonasManage),
            ("role.update", Capability::PersonasManage),
            ("role.delete", Capability::PersonasManage),
            ("identity.lifecycle", Capability::RolesAssign),
            ("system.disable_skill", Capability::SkillsManage),
            ("system.install_catalog_skill", Capability::SkillsManage),
            ("system.add_account", Capability::VaultSharedWrite),
            ("system.remove_account", Capability::VaultSharedWrite),
            ("system.add_llm_token", Capability::VaultSharedWrite),
            ("system.remove_llm_token", Capability::VaultSharedWrite),
            ("system.reload_slack", Capability::VaultSharedWrite),
            // MCP server management — connect/disconnect / mutate the toolset.
            ("mcp.install", Capability::SkillsManage),
            ("mcp.uninstall", Capability::SkillsManage),
            ("mcp.set_tools", Capability::SkillsManage),
            ("mcp.update", Capability::SkillsManage),
            // Shared LLM / cost configuration.
            ("system.set_model", Capability::ConfigManage),
            ("system.set_provider", Capability::ConfigManage),
            ("system.set_routing", Capability::ConfigManage),
            ("system.set_failover_chain", Capability::ConfigManage),
            ("system.set_max_tokens", Capability::ConfigManage),
            ("system.set_max_rounds", Capability::ConfigManage),
            ("pricing.update", Capability::ConfigManage),
            // Heartbeat management — own delegable capability, no per-user owner.
            ("heartbeat.create", Capability::HeartbeatManage),
            ("heartbeat.update", Capability::HeartbeatManage),
            ("heartbeat.delete", Capability::HeartbeatManage),
            ("heartbeat.trigger", Capability::HeartbeatManage),
            // Device fleet (no verified owner → admin-only).
            ("pairing.reject", Capability::DevicesManage),
            ("pairing.link", Capability::DevicesManage),
            ("pairing.revoke", Capability::DevicesManage),
            ("pairing.devices", Capability::DevicesManage),
            ("pairing.pending", Capability::DevicesManage),
            ("node_invoke", Capability::DevicesManage),
            // Shared knowledge-base destruction + hub-wide ops read.
            ("knowledge.delete", Capability::KnowledgeManage),
            ("system.logs", Capability::OpsRead),
            // Shared per-agent project pool (no owner → admin-only).
            ("projects.create", Capability::ProjectsManage),
            ("projects.delete", Capability::ProjectsManage),
            ("projects.rename", Capability::ProjectsManage),
            ("projects.update", Capability::ProjectsManage),
            ("projects.add_repo", Capability::ProjectsManage),
            ("projects.remove_repo", Capability::ProjectsManage),
            ("projects.open_workspace", Capability::ProjectsManage),
            ("projects.index", Capability::ProjectsManage),
            // Code / command execution on the host.
            ("exec.run", Capability::ShellExec),
            ("code.cargo", Capability::ShellExec),
            ("code.build", Capability::ShellExec),
            ("code.test", Capability::ShellExec),
            ("code.lint", Capability::ShellExec),
            ("code.git", Capability::ShellExec),
            ("coding.claude", Capability::ShellExec),
            ("coding.session", Capability::ShellExec),
            ("wasm_skill.build", Capability::ShellExec),
            // Shared Slack app credential write.
            ("slack.connect", Capability::VaultSharedWrite),
            ("slack.app_configure", Capability::VaultSharedWrite),
            ("slack.disconnect", Capability::VaultSharedWrite),
        ];
        for (tool, expected) in cases {
            assert_eq!(
                tool_capability(tool, &no_input),
                Some(*expected),
                "{tool} should map to {expected:?}"
            );
            assert_eq!(
                authz(&member(), *expected),
                Decision::Deny,
                "{tool}: member must be denied"
            );
            assert_eq!(
                authz(&admin(), *expected),
                Decision::Allow,
                "{tool}: admin must be allowed"
            );
        }
    }

    /// Reading the agent's own config is open to anyone (credentials come back
    /// blanked); changing it is `ConfigManage` like the `system.set_*` tools,
    /// and so is an action the gate does not recognise.
    #[test]
    fn system_config_gates_changes_not_reads() {
        let call = |action: &str| {
            tool_capability("system.config", &serde_json::json!({ "action": action }))
        };
        assert_eq!(call("get"), None);
        assert_eq!(call("set"), Some(Capability::ConfigManage));
        assert_eq!(call("unset"), Some(Capability::ConfigManage));
        assert_eq!(call("wipe"), Some(Capability::ConfigManage));
        assert_eq!(
            tool_capability("system.config", &serde_json::json!({})),
            Some(Capability::ConfigManage)
        );
    }

    /// Authoring a WASM skill is member-level: the sandbox plus the per-call
    /// capability check on `wf_tool_invoke` mean a skill grants no reach its
    /// caller lacks, and *which* skill the caller may touch is decided by
    /// ownership, not by this cell. The privilege that survives is compiling on
    /// the host, which stays `ShellExec` unless the deployment sandboxes builds.
    #[test]
    fn wasm_skill_authoring_is_member_level_but_compiling_is_not() {
        let no_input = serde_json::json!({});
        for tool in [
            "wasm_skill.install",
            "wasm_skill.import",
            "wasm_skill.update",
            "wasm_skill.uninstall",
        ] {
            assert_eq!(
                tool_capability(tool, &no_input),
                Some(Capability::SkillsAuthor),
                "{tool}"
            );
        }
        assert_eq!(
            authz(&member(), Capability::SkillsAuthor),
            Decision::Allow,
            "a member must be able to author their own skills"
        );

        // The compiling shapes price differently on a hub that builds in-process.
        assert_eq!(
            tool_capability("wasm_skill.build", &no_input),
            Some(Capability::ShellExec)
        );
        assert_eq!(
            tool_capability(
                "wasm_skill.install",
                &serde_json::json!({ "project_path": "./skill" })
            ),
            Some(Capability::ShellExec)
        );
        assert_eq!(
            tool_capability(
                "wasm_skill.install",
                &serde_json::json!({ "github_repo": "o/r@v1", "sha256": "ab" })
            ),
            Some(Capability::SkillsAuthor),
            "a digest-pinned release asset moves bytes; it never compiles"
        );
        assert_eq!(authz(&member(), Capability::ShellExec), Decision::Deny);
    }

    #[test]
    fn structural_edits_are_gated_only_where_a_language_server_runs() {
        assert_eq!(
            tool_capability(
                "code.refactor",
                &serde_json::json!({ "action": "replace_definition" })
            ),
            None,
            "a parse-and-write is confined like code.edit"
        );
        for action in [
            "rename",
            "diagnostics",
            "references",
            "definition",
            "surprise",
        ] {
            assert_eq!(
                tool_capability("code.refactor", &serde_json::json!({ "action": action })),
                Some(Capability::ShellExec),
                "{action}"
            );
        }
        assert_eq!(
            tool_capability("code.refactor", &serde_json::json!({})),
            Some(Capability::ShellExec),
            "an unreadable call fails closed"
        );
    }

    #[test]
    fn external_skill_lifecycle_is_member_level_except_other_git_hosts() {
        for (input, expected) in [
            (
                serde_json::json!({
                    "github_repo": "acme/skills@0123456789abcdef0123456789abcdef01234567#subpath=hello-world"
                }),
                Capability::SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "https://github.com/acme/skills#subpath=hello-world" }),
                Capability::SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "skills/hello-world", "visibility": "everyone" }),
                Capability::SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "/srv/skills/hello-world" }),
                Capability::SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "https://gitlab.example.com/acme/skills.git" }),
                Capability::SkillsManage,
            ),
        ] {
            assert_eq!(
                tool_capability("system.install_external_skill", &input),
                Some(expected),
                "{input}"
            );
        }
        assert_eq!(
            tool_capability("system.uninstall_external_skill", &serde_json::json!({})),
            Some(Capability::SkillsAuthor)
        );
        assert_eq!(authz(&member(), Capability::SkillsAuthor), Decision::Allow);
        assert_eq!(authz(&member(), Capability::SkillsManage), Decision::Deny);
        for tool in [
            "system.publish_external_skill",
            "system.list_external_skills",
        ] {
            assert_eq!(
                tool_capability(tool, &serde_json::json!({})),
                None,
                "{tool}"
            );
        }
    }

    #[test]
    fn holds_capability_follows_the_live_matrix() {
        use workforce_identity::sqlite::SqliteIdentityStore;
        use workforce_identity::types::Identity;

        let store = SqliteIdentityStore::in_memory().unwrap();
        let member = Identity::new("Member", IdentityRole::Member);
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        let suspended = Identity::new("Suspended", IdentityRole::SystemAdmin)
            .with_status(IdentityStatus::Suspended);
        for identity in [&member, &admin, &suspended] {
            store.create(identity).unwrap();
        }
        let store: Arc<dyn IdentityStore> = Arc::new(store);
        let holds = |identity: &str, matrix: &CapabilityMatrix| {
            holds_capability_in(Some(&store), identity, Capability::SkillsManage, matrix)
        };
        let defaults = CapabilityMatrix::default();
        let member_allowed = CapabilityMatrix::from_overrides(&[CapabilityOverride {
            role: "member".into(),
            capability: "skills_manage".into(),
            decision: StoredDecision::Allow,
            updated_at: chrono::Utc::now(),
            updated_by: None,
        }]);

        assert!(!holds(&member.id.to_string(), &defaults));
        assert!(holds(&admin.id.to_string(), &defaults));
        assert!(!holds(&suspended.id.to_string(), &defaults));
        assert!(holds(&member.id.to_string(), &member_allowed));
        assert!(!holds(&uuid::Uuid::new_v4().to_string(), &defaults));
        assert!(!holds("not-a-uuid", &defaults));
        assert!(!holds_capability(
            None,
            &admin.id.to_string(),
            Capability::SkillsManage
        ));
    }

    /// `system.enable_skill` maps to the member-allowed `SkillsEnable`
    /// capability (not the admin-only `SkillsManage`), so a member can load a
    /// configured skill at runtime while install/disable stay admin-only. This
    /// is the registered cell that the cap gate passes to the cascade, so it is
    /// *not* in the ungated (`None`) list below.
    #[test]
    fn enable_skill_is_member_allowed_but_registered() {
        let no_input = serde_json::json!({});
        assert_eq!(
            tool_capability("system.enable_skill", &no_input),
            Some(Capability::SkillsEnable),
        );
        assert_eq!(authz(&member(), Capability::SkillsEnable), Decision::Allow);
        assert_eq!(authz(&admin(), Capability::SkillsEnable), Decision::Allow);
        // The privileged skill lifecycle stays admin-only.
        assert_eq!(
            tool_capability("system.disable_skill", &no_input),
            Some(Capability::SkillsManage),
        );
        assert_eq!(authz(&member(), Capability::SkillsManage), Decision::Deny);
    }

    /// Read / status / set tools that had no Increment-1 inline check stay
    /// ungated (`None`) so the gate never flips a member-accessible read to a
    /// pre-prompt Deny.
    #[test]
    fn member_accessible_tools_are_not_gated() {
        let no_input = serde_json::json!({});
        for tool in [
            "agent.list",
            "agent.show",
            "role.list",
            "role.show",
            "system.list_skills",
            "system.list_catalog_skills",
            "system.list_tools",
            "system.list_accounts",
            "system.list_llm_tokens",
            "system.list_models",
            "system.llm_status",
            "system.routing_status",
            "system.set_session_model",
            "vault.get",
            "vault.list",
            "skill.grant",
            "skill.grants",
            "skill.revoke",
            "skill.share",
            "skill.unshare",
            "skill.visibility",
            "skill.transfer",
            // MCP read-only / per-user / dormant-only siblings stay ungated.
            "mcp.list",
            "mcp.list_presets",
            "mcp.show",
            "mcp.list_remote_tools",
            "mcp.workspace_list",
            "mcp.workspace_add",
            "mcp.authorize",
            "pricing.list",
            // Read-only code tools stay ungated — only the build/test/lint/cargo/
            // git *runners* (which execute code) are gated under ShellExec. The
            // write tools (`code.edit`/`code.find_replace`) are worktree-confined
            // and harmless without an execution tool, so they stay ungated too.
            "code.read",
            "code.search",
            "code.tree",
            "code.symbols",
            "code.diff",
            "code.find",
            "code.edit",
            "code.find_replace",
            // WASM scaffold writes a template only; list is read-only.
            "wasm_skill.scaffold",
            "wasm_skill.list",
            // Slack: per-user token tools (own data) + the manifest-only scaffold.
            "slack.user_token_set",
            "slack.user_token_clear",
            "slack.app_scaffold",
            // Heartbeat reads stay ungated; only the mutating ops are admin.
            "heartbeat.read",
            "heartbeat.list",
            // Knowledge (re)indexing + status are routine; only delete is gated.
            "knowledge.index",
            "knowledge.refresh_index",
            // Config disclosure that carries no secret values.
            "settings",
            // Project reads stay ungated; only the mutating ops are admin.
            "projects.list",
            "projects.show",
        ] {
            assert_eq!(
                tool_capability(tool, &no_input),
                None,
                "{tool} must be ungated"
            );
        }
    }

    #[test]
    fn vault_set_global_is_shared_write_other_scopes_own() {
        // scope=global (T4) → admin-only shared write.
        assert_eq!(
            tool_capability("vault.set", &serde_json::json!({"scope": "global"})),
            Some(Capability::VaultSharedWrite)
        );
        // scope=agent_specific (T3) is member-writable today — must NOT be a
        // shared write (would over-deny vs the inline `requires_admin_to_write`).
        assert_eq!(
            tool_capability("vault.set", &serde_json::json!({"scope": "agent_specific"})),
            Some(Capability::OwnDataRw)
        );
        // Per-user scopes + absent scope (defaults to per-user when a uid is in
        // scope) → own data.
        for v in [
            serde_json::json!({"scope": "global_per_user"}),
            serde_json::json!({"scope": "agent_per_user"}),
            serde_json::json!({"scope": "user"}), // alias
            serde_json::json!({}),
        ] {
            assert_eq!(
                tool_capability("vault.set", &v),
                Some(Capability::OwnDataRw),
                "{v} should be own-data"
            );
        }
    }

    #[test]
    fn vault_move_touching_shared_is_shared_write_else_own() {
        // Either endpoint non-per-user (T3/T4) ⇒ shared write.
        for v in [
            serde_json::json!({"from_tier": "global_per_user", "to_tier": "global"}),
            serde_json::json!({"from_tier": "agent_specific", "to_tier": "agent_per_user"}),
            serde_json::json!({"from_tier": "agent_specific", "to_tier": "global"}),
        ] {
            assert_eq!(
                tool_capability("vault.move", &v),
                Some(Capability::VaultSharedWrite),
                "{v} touches a shared tier"
            );
        }
        // Both endpoints per-user ⇒ own data (no admin needed).
        assert_eq!(
            tool_capability(
                "vault.move",
                &serde_json::json!({"from_tier": "agent_per_user", "to_tier": "global_per_user"})
            ),
            Some(Capability::OwnDataRw)
        );
    }

    #[test]
    fn vault_delete_is_own_destructive_never_hard_denied() {
        // vault.delete is ALWAYS OwnDataDestructive (Ask) here — a bare path
        // may resolve to a per-user tier (the self-service list→delete flow),
        // which only the live store knows, so the gate must not hard-deny it.
        // The tool's own inline store check is the authority for the bare
        // own-vs-shared split. Both an explicit own path and a bare path map
        // identically.
        for path in [
            "users/00000000-0000-0000-0000-000000000000/skills/jira/api_token",
            "skills/jira/api_token",
            "slack.T0/bot_token",
        ] {
            assert_eq!(
                tool_capability("vault.delete", &serde_json::json!({"path": path})),
                Some(Capability::OwnDataDestructive),
                "{path}"
            );
        }
        // Ask is never a hard Deny, for any role — so the gate passes
        // vault.delete through to the inline check.
        assert_ne!(
            authz(&member(), Capability::OwnDataDestructive),
            Decision::Deny
        );
    }

    #[test]
    fn unregistered_destructive_tool_defaults_to_own_destructive() {
        let no_input = serde_json::json!({});
        // Destructive-by-name but not in the registry ⇒ OwnDataDestructive
        // (Ask) — secure-by-default, one shared `is_destructive` definition.
        assert_eq!(
            tool_capability("github.delete_branch", &no_input),
            Some(Capability::OwnDataDestructive)
        );
        assert_eq!(
            tool_capability("files.remove", &no_input),
            Some(Capability::OwnDataDestructive)
        );
        // Non-destructive, unregistered ⇒ not gated.
        assert_eq!(tool_capability("github.list_issues", &no_input), None);
        assert_eq!(tool_capability("data.query", &no_input), None);
    }

    // ── capability matrix engine ──

    fn authz(p: &Principal, cap: Capability) -> Decision {
        authorize(p, cap, &CapabilityMatrix::default())
    }

    fn summary(role: IdentityRole, status: IdentityStatus) -> CapabilitySummary {
        capability_summary(role.as_str(), status, &CapabilityMatrix::default())
    }

    fn enf(p: &Principal, cap: Capability) -> Result<(), Decision> {
        enforce(p, cap, &CapabilityMatrix::default())
    }

    fn override_(role: &str, cap: &str, decision: StoredDecision) -> CapabilityOverride {
        CapabilityOverride {
            role: role.into(),
            capability: cap.into(),
            decision,
            updated_at: chrono::Utc::now(),
            updated_by: None,
        }
    }

    #[test]
    fn empty_matrix_reproduces_builtin_defaults() {
        let matrix = CapabilityMatrix::default();
        for cap in Capability::ALL {
            assert_eq!(
                matrix.decision("member", IdentityStatus::Active, cap),
                baseline_for("member", cap),
                "member {cap:?}"
            );
            assert_eq!(
                matrix.decision("manager", IdentityStatus::Active, cap),
                baseline_for("manager", cap),
                "manager {cap:?}"
            );
            assert_eq!(
                matrix.decision("system_admin", IdentityStatus::Active, cap),
                baseline_for("system_admin", cap),
                "admin {cap:?}"
            );
            // A custom role with no overrides falls through to the member baseline.
            assert_eq!(
                matrix.decision("reviewer", IdentityStatus::Active, cap),
                baseline_for("member", cap),
                "custom {cap:?}"
            );
        }
    }

    #[test]
    fn protected_admin_caps_are_locked_to_system_admin() {
        // Even with an explicit override row present, the protected caps deny for
        // every non-admin role and the rows are dropped from the matrix.
        let matrix = CapabilityMatrix::from_overrides(&[
            override_("member", "agent_export", StoredDecision::Allow),
            override_("reviewer", "agent_import", StoredDecision::Allow),
            override_("manager", "crossuser_read", StoredDecision::Allow),
            override_("system_admin", "agent_export", StoredDecision::Deny),
        ]);
        for role in ["member", "manager", "reviewer"] {
            for cap in [
                Capability::AgentExport,
                Capability::AgentImport,
                Capability::CrossuserRead,
            ] {
                assert_eq!(
                    matrix.decision(role, IdentityStatus::Active, cap),
                    Decision::Deny,
                    "{role} must never hold {cap:?}"
                );
            }
        }
        // The admin keeps export/import (the Deny override row was dropped) and
        // crossuser_read stays system-only.
        assert_eq!(
            matrix.decision(
                "system_admin",
                IdentityStatus::Active,
                Capability::AgentExport
            ),
            Decision::Allow
        );
        assert_eq!(
            matrix.decision(
                "system_admin",
                IdentityStatus::Active,
                Capability::AgentImport
            ),
            Decision::Allow
        );
        assert_eq!(
            matrix.decision(
                "system_admin",
                IdentityStatus::Active,
                Capability::CrossuserRead
            ),
            Decision::Deny
        );
    }

    #[test]
    fn custom_role_override_resolves_and_does_not_leak() {
        let matrix = CapabilityMatrix::from_overrides(&[override_(
            "reviewer",
            "ops_read",
            StoredDecision::Allow,
        )]);
        assert_eq!(
            matrix.decision("reviewer", IdentityStatus::Active, Capability::OpsRead),
            Decision::Allow
        );
        // The grant is scoped to the custom role — a Member is unaffected.
        assert_eq!(
            matrix.decision("member", IdentityStatus::Active, Capability::OpsRead),
            Decision::Deny
        );
    }

    #[test]
    fn non_active_demotes_any_role_to_member() {
        let matrix = CapabilityMatrix::from_overrides(&[override_(
            "reviewer",
            "shell_exec",
            StoredDecision::Allow,
        )]);
        // A suspended custom/manager role loses its elevation and reads the
        // member row.
        assert_eq!(
            matrix.decision("reviewer", IdentityStatus::Suspended, Capability::ShellExec),
            Decision::Deny
        );
        assert_eq!(
            matrix.decision("manager", IdentityStatus::Suspended, Capability::OpsRead),
            Decision::Deny
        );
    }

    #[test]
    fn override_beats_default_per_effective_role() {
        let matrix = CapabilityMatrix::from_overrides(&[
            override_("member", "shell_exec", StoredDecision::Allow),
            override_("system_admin", "vault_shared_delete", StoredDecision::Deny),
        ]);
        assert_eq!(
            matrix.decision("member", IdentityStatus::Active, Capability::ShellExec),
            Decision::Allow
        );
        assert_eq!(
            matrix.decision(
                "system_admin",
                IdentityStatus::Active,
                Capability::VaultSharedDelete
            ),
            Decision::Deny
        );
        assert_eq!(
            matrix.decision("member", IdentityStatus::Active, Capability::OwnDataRw),
            Decision::Allow
        );
    }

    #[test]
    fn suspended_admin_uses_member_overrides() {
        let matrix = CapabilityMatrix::from_overrides(&[
            override_("member", "config_manage", StoredDecision::Ask),
            override_("system_admin", "config_manage", StoredDecision::Allow),
        ]);
        assert_eq!(
            matrix.decision(
                "system_admin",
                IdentityStatus::Suspended,
                Capability::ConfigManage
            ),
            Decision::Ask
        );
    }

    #[test]
    fn manager_override_resolves_for_manager_only() {
        let matrix = CapabilityMatrix::from_overrides(&[override_(
            "manager",
            "knowledge_manage",
            StoredDecision::Allow,
        )]);
        // A manager-keyed row now resolves for a Manager (no collapse to Member).
        assert_eq!(
            matrix.decision(
                "manager",
                IdentityStatus::Active,
                Capability::KnowledgeManage
            ),
            Decision::Allow
        );
        // It does not leak to Member.
        assert_eq!(
            matrix.decision(
                "member",
                IdentityStatus::Active,
                Capability::KnowledgeManage
            ),
            Decision::Deny
        );
    }

    #[test]
    fn from_overrides_keeps_all_role_rows_drops_unknown_caps() {
        let matrix = CapabilityMatrix::from_overrides(&[
            override_("manager", "shell_exec", StoredDecision::Allow),
            override_("reviewer", "vault_shared_read", StoredDecision::Allow),
            override_("member", "not_a_capability", StoredDecision::Allow),
            override_("member", "ops_read", StoredDecision::Allow),
        ]);
        // Manager and custom rows are now kept (was: dropped).
        assert_eq!(
            matrix.decision("manager", IdentityStatus::Active, Capability::ShellExec),
            Decision::Allow
        );
        assert_eq!(
            matrix.decision(
                "reviewer",
                IdentityStatus::Active,
                Capability::VaultSharedRead
            ),
            Decision::Allow
        );
        // The unknown-capability row is silently dropped.
        assert_eq!(
            matrix.decision("member", IdentityStatus::Active, Capability::OpsRead),
            Decision::Allow
        );
    }

    #[test]
    fn authorize_scores_custom_role_through_role_key() {
        // authorize() reads the principal's role_key, so a custom role resolves
        // to its own matrix row rather than the lossy Member enum.
        let matrix = CapabilityMatrix::from_overrides(&[override_(
            "reviewer",
            "ops_read",
            StoredDecision::Allow,
        )]);
        let p = custom("reviewer", IdentityStatus::Active);
        assert_eq!(authorize(&p, Capability::OpsRead, &matrix), Decision::Allow);
        // Protected caps stay denied for the custom role even through authorize.
        assert_eq!(
            authorize(&p, Capability::AgentExport, &matrix),
            Decision::Deny
        );
        // The lossy enum is Member (custom collapses) — the hard admin gates read
        // it, so a custom role never reaches the named-SystemAdmin checks.
        assert_eq!(p.role, IdentityRole::Member);
    }

    #[test]
    fn decision_conversions_round_trip() {
        for d in [Decision::Allow, Decision::Ask, Decision::Deny] {
            assert_eq!(decision_from_stored(decision_to_stored(d)), d);
        }
    }

    #[test]
    fn capability_key_round_trips() {
        for cap in Capability::ALL {
            assert_eq!(Capability::from_key(cap.key()), Some(cap));
        }
        assert_eq!(Capability::from_key("nope"), None);
    }

    #[test]
    fn approving_a_device_for_oneself_is_open_and_for_someone_else_is_fleet_admin() {
        assert_eq!(
            tool_capability("pairing.approve", &serde_json::json!({ "code": "ABC" })),
            None
        );
        assert_eq!(
            tool_capability(
                "pairing.approve",
                &serde_json::json!({ "code": "ABC", "for": "a@example.com" })
            ),
            Some(Capability::DevicesManage)
        );
        assert_eq!(
            tool_capability("pairing.approve", &serde_json::json!({ "for": " " })),
            None,
            "a blank `for` approves for the caller, as the tool does"
        );
    }

    #[test]
    fn governed_tools_map_back_to_their_capability() {
        // Every name-mapped tool listed in the catalog resolves the capability it
        // is filed under — so the displayed list can't drift from the registry.
        let no_input = serde_json::json!({});
        for cap in Capability::ALL {
            for tool in cap.governed_tools() {
                assert_eq!(
                    tool_capability(tool, &no_input),
                    Some(cap),
                    "{tool} is filed under {cap:?} but maps elsewhere"
                );
            }
        }
    }

    #[test]
    fn every_capability_has_human_metadata() {
        for cap in Capability::ALL {
            assert!(!cap.label().is_empty(), "{cap:?} label");
            assert!(cap.description().len() > 20, "{cap:?} description");
            assert!(!cap.category().is_empty(), "{cap:?} category");
        }
        // The ShellExec description must spell out the host-execution risk.
        assert!(Capability::ShellExec.description().contains("vault key"));
    }
}
