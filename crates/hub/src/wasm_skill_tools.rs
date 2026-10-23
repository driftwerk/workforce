//! Agent-facing tools for managing WASM-based skills.
//!
//! These tools let agents install, uninstall, list, build, and scaffold WASM
//! skills at runtime. They follow the same `SystemToolContext` pattern used by
//! the built-in skill management tools in `system_tool.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::paths::CanonicalRoots;
use workforce_agent::tools::{config_guard, Tool, ToolResult};
use workforce_wasm_runtime::manifest::SkillManifest;
use workforce_wasm_runtime::tool::WasmSkillTool;
use workforce_wasm_runtime::types::{InstalledWasmSkill, WasmSkillState};
use workforce_wasm_runtime::{WasmSkillEngine, WasmSkillStore};

use crate::git_hardening::hardened_git;
pub(crate) use crate::hub_core::skill_builds::{is_commit_sha, parse_github_repo_ref};
use crate::skill_state::SkillStateTracker;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::registry::ToolRegistry;

/// Process-wide handle to the WASM skill store, for surfaces that run outside
/// the tool context (the `cmd: skill` consent command). Set once at wiring.
static GLOBAL_SKILL_STORE: std::sync::OnceLock<Arc<WasmSkillStore>> = std::sync::OnceLock::new();

pub fn set_global_skill_store(store: Arc<WasmSkillStore>) {
    let _ = GLOBAL_SKILL_STORE.set(store);
}

pub fn global_skill_store() -> Option<Arc<WasmSkillStore>> {
    GLOBAL_SKILL_STORE.get().cloned()
}

/// Process-wide WASM engine — a pure compile cache, shared so a module
/// installed via any agent is instantly executable from every agent (and only
/// one epoch ticker runs). Per-agent context (locker vault, tool invoker)
/// rides on each registered `WasmSkillTool`, not on the engine.
static GLOBAL_SKILL_ENGINE: std::sync::OnceLock<Arc<WasmSkillEngine>> = std::sync::OnceLock::new();

/// True exactly once per process — the shared engine must be registered with
/// memory metrics a single time, not once per agent.
pub fn engine_not_yet_metered() -> bool {
    static METERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    !METERED.swap(true, std::sync::atomic::Ordering::SeqCst)
}

pub fn global_skill_engine() -> Option<Arc<WasmSkillEngine>> {
    match GLOBAL_SKILL_ENGINE.get() {
        Some(e) => Some(Arc::clone(e)),
        None => match WasmSkillEngine::new() {
            Ok(e) => {
                let _ = GLOBAL_SKILL_ENGINE.set(Arc::new(e));
                GLOBAL_SKILL_ENGINE.get().map(Arc::clone)
            }
            Err(e) => {
                tracing::warn!(error = %e, "WASM engine unavailable");
                None
            }
        },
    }
}

/// The skill-locker vault: reads prefer the agent-independent global vault
/// and fall back to the agent vault (where pre-migration grants live);
/// writes always land in the primary, so a grant made through any agent is
/// visible to every agent from then on.
pub struct SkillLockerVault {
    primary: Arc<dyn workforce_common::settings::SecretStore>,
    fallback: Option<Arc<dyn workforce_common::settings::SecretStore>>,
}

impl SkillLockerVault {
    /// `global` when present (org hubs), else the agent vault, with the agent
    /// vault as legacy read fallback. `None` when the agent has no vault.
    pub fn compose(
        global: Option<Arc<crate::vault::Vault>>,
        agent: Option<Arc<crate::vault::Vault>>,
    ) -> Option<Arc<dyn workforce_common::settings::SecretStore>> {
        let as_store =
            |v: Arc<crate::vault::Vault>| v as Arc<dyn workforce_common::settings::SecretStore>;
        Self::compose_stores(global.map(as_store), agent.map(as_store))
    }

    fn compose_stores(
        global: Option<Arc<dyn workforce_common::settings::SecretStore>>,
        agent: Option<Arc<dyn workforce_common::settings::SecretStore>>,
    ) -> Option<Arc<dyn workforce_common::settings::SecretStore>> {
        match (global, agent) {
            (Some(primary), fallback) => Some(Arc::new(Self { primary, fallback })),
            (None, Some(agent)) => Some(agent),
            (None, None) => None,
        }
    }
}

impl workforce_common::settings::SecretStore for SkillLockerVault {
    fn get(&self, path: &str) -> Option<String> {
        self.primary
            .get(path)
            .or_else(|| self.fallback.as_ref().and_then(|f| f.get(path)))
    }
    fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
        self.primary.set(path, value)
    }
    /// The fallback goes first: if the primary delete then fails, the primary
    /// still answers reads, so a stale fallback copy never resurfaces.
    fn delete(&self, path: &str) -> eyre::Result<bool> {
        let fallback = self
            .fallback
            .as_ref()
            .map(|f| f.delete(path))
            .transpose()?
            .unwrap_or(false);
        let primary = self.primary.delete(path)?;
        Ok(primary || fallback)
    }
    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        let mut paths = self.primary.list(prefix);
        if let Some(fallback) = &self.fallback {
            for p in fallback.list(prefix) {
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        paths
    }
    fn set_with_actor(
        &self,
        path: &str,
        value: &str,
        actor: workforce_common::settings::VaultActor<'_>,
    ) -> eyre::Result<()> {
        self.primary.set_with_actor(path, value, actor)
    }
}

/// One entry per agent: everything needed to (de)register a skill's tools for
/// that agent. `Weak` so a dropped agent prunes itself from the roster.
struct SkillHost {
    agent_id: String,
    registry: std::sync::Weak<ToolRegistry>,
    skill_state: std::sync::Weak<SkillStateTracker>,
    locker: Option<Arc<dyn workforce_common::settings::SecretStore>>,
    bindings: workforce_wasm_runtime::SkillRuntimeBindings,
}

/// The agents a skill install, update or uninstall reaches: each one's tools
/// are (de)registered in the same step, so a skill built via one agent is
/// runnable via another with no reboot. A hub has one roster,
/// [`SkillHosts::process`]; a clone is another handle on the same roster.
#[derive(Clone, Default)]
pub struct SkillHosts(Arc<parking_lot::Mutex<Vec<SkillHost>>>);

impl SkillHosts {
    /// The roster every agent service in this process joins.
    pub fn process() -> Self {
        static HOSTS: std::sync::LazyLock<SkillHosts> =
            std::sync::LazyLock::new(SkillHosts::default);
        HOSTS.clone()
    }

    /// Add an agent to the roster.
    pub fn register(
        &self,
        agent_id: &str,
        registry: &Arc<ToolRegistry>,
        skill_state: &Arc<SkillStateTracker>,
        locker: Option<Arc<dyn workforce_common::settings::SecretStore>>,
        invoker: workforce_wasm_runtime::types::SkillInvokerSlot,
    ) {
        self.0.lock().push(SkillHost {
            agent_id: agent_id.to_string(),
            registry: Arc::downgrade(registry),
            skill_state: Arc::downgrade(skill_state),
            locker,
            bindings: workforce_wasm_runtime::SkillRuntimeBindings::new(invoker, skill_use_gate()),
        });
    }

    /// Run `f` for every live host, pruning dead ones.
    fn for_each(
        &self,
        mut f: impl FnMut(&str, &Arc<ToolRegistry>, &Arc<SkillStateTracker>, &SkillHost),
    ) {
        self.0.lock().retain(
            |host| match (host.registry.upgrade(), host.skill_state.upgrade()) {
                (Some(registry), Some(state)) => {
                    f(&host.agent_id, &registry, &state, host);
                    true
                }
                _ => false,
            },
        );
    }

    /// Register a skill's tools on every live host and mark it loaded. The
    /// tools are rebuilt per host so each carries that agent's locker and
    /// invoker.
    fn register_skill(
        &self,
        manifest: &SkillManifest,
        engine: &Arc<WasmSkillEngine>,
        binary_version: u32,
        config: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut tool_names = Vec::new();
        self.for_each(|agent_id, registry, state, host| {
            let tools = WasmSkillTool::from_manifest(
                manifest,
                Arc::clone(engine),
                binary_version,
                config.clone(),
                host.locker.clone(),
                host.bindings.clone(),
            );
            tool_names = tools.iter().map(|t| t.name().to_string()).collect();
            for tool in &tools {
                registry.register(Arc::clone(tool));
            }
            state.mark_loaded(
                &manifest.name,
                tool_names.clone(),
                "wasm",
                Some(format!("v{}", manifest.version)),
            );
            tracing::debug!(skill = %manifest.name, agent = %agent_id, "skill tools registered");
        });
        tool_names
    }

    /// Unregister a skill's tools from every live host.
    fn unload_skill(&self, skill_name: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.for_each(|_, registry, state, _| {
            removed = unload_wasm_skill(skill_name, registry, state);
        });
        removed
    }
}

/// Process-wide slot holding the [`workforce_wasm_runtime::SkillUseGate`] every
/// registered skill tool consults before it runs. Filled once the store and the
/// identity store exist; every host shares the one slot, so wiring it late
/// covers tools registered before it.
fn skill_use_gate() -> workforce_wasm_runtime::SkillUseGateSlot {
    static SLOT: std::sync::OnceLock<workforce_wasm_runtime::SkillUseGateSlot> =
        std::sync::OnceLock::new();
    Arc::clone(SLOT.get_or_init(Default::default))
}

/// Bind the gate that decides who may call a skill's tools. Idempotent — the
/// serve path calls it once per process, from whichever agent initialises the
/// store first.
pub fn set_skill_use_gate(gate: Arc<dyn workforce_wasm_runtime::SkillUseGate>) {
    *skill_use_gate().write() = Some(gate);
}

/// Whether a use gate is wired — the multi-user serve path must always wire
/// one, since an unwired slot is permissive by design for single-tenant CLI.
pub fn skill_use_gate_is_wired() -> bool {
    skill_use_gate().read().is_some()
}

/// Fail loudly at boot if skills are installable but nothing decides who may
/// call them. The slot is permissive when empty — the right default for a
/// single-tenant CLI and the wrong one for a hub, where it would make every
/// private skill everyone's. Called once the store exists.
pub fn assert_use_gate_wired() {
    if !skill_use_gate_is_wired() {
        tracing::error!(
            target: "sentinel.audit",
            "WASM skill use gate is not wired: skill ownership is not being enforced on this \
             hub, so every installed skill is callable by every user. This is a wiring bug in \
             the serve path."
        );
    }
}

/// Remove an installed skill everywhere: its tools from every agent registry,
/// its module from the engine cache, and its files from the store. Shared by
/// the `wasm_skill.uninstall` tool and the management API so both leave the
/// same state behind.
pub(crate) async fn uninstall_skill(
    hosts: &SkillHosts,
    engine: &WasmSkillEngine,
    store: &WasmSkillStore,
    installed: &workforce_wasm_runtime::InstalledWasmSkill,
) -> eyre::Result<Vec<String>> {
    let name = &installed.manifest.name;
    let removed = hosts.unload_skill(name);
    engine.remove_module(name, installed.binary_version).await;
    store.remove(name)?;
    info!(skill = %name, removed = removed.len(), "WASM skill uninstalled");
    Ok(removed)
}

/// Evict any prior live version, compile the stored bytes, and register the
/// skill's tools on every agent. The shared tail of install, update, import
/// and catalog installs — `Err` is the user-facing failure text.
pub(crate) async fn activate_stored_skill(
    hosts: &SkillHosts,
    engine: &Arc<WasmSkillEngine>,
    previous_version: Option<u32>,
    manifest: &SkillManifest,
    binary_version: u32,
    wasm_bytes: &[u8],
    config: &HashMap<String, String>,
) -> Result<Vec<String>, String> {
    if let Some(prev) = previous_version {
        hosts.unload_skill(&manifest.name);
        engine.remove_module(&manifest.name, prev).await;
    }
    engine
        .load_module(&manifest.name, binary_version, wasm_bytes)
        .await
        .map_err(|e| format!("Failed to compile WASM: {e}"))?;
    let tool_names = hosts.register_skill(manifest, engine, binary_version, config);
    Ok(tool_names)
}

/// The sandbox contract every skill author must know up front — shown on the
/// authoring tools so the model reads it before writing code, not after an
/// hour of denied calls.
const SANDBOX_CONTRACT: &str = "Call wasm_skill.guide FIRST — it is the complete authoring \
    contract. SANDBOX CONTRACT: a skill can only (1) reach hosts listed in Skill.toml \
    [network] allowed_hosts — every other host is refused at runtime; (2) read vault keys \
    inside its own namespace wasm-skills/{skill}/… — never the user's general vault; (3) \
    read a Skill.toml-declared [[credentials]] entry only after the USER approves it by \
    typing `cmd: secret <name> <value>` then `cmd: skill grant <skill> <name>` in chat (or, \
    from their own Claude Code over remote MCP, vault.set then skill.grant) — the agent \
    cannot provision it via vault tools or tool arguments; (4) invoke platform \
    tools declared in [[permissions]] only after the USER runs `cmd: skill grant <skill> \
    tools`, and every call is capped by the invoking user's own capabilities. Grants pin \
    the skill's CONSENT CONTRACT (capabilities, [network] allowed_hosts, [[credentials]], \
    [[permissions]]): editing any of those invalidates every grant until the user re-runs \
    `cmd: skill grant`, while pure code rebuilds keep them. Smoke-test without any grants \
    via wasm_skill.test. A skill belongs to the user who installed it and is PRIVATE until \
    they share it (`cmd: skill share <skill> <@user> [manage]`) or publish it \
    (`cmd: skill visibility <skill> everyone`); another user's skill cannot be called, \
    changed, or overwritten by installing over its name.";

/// Shared context for WASM skill tools.
#[derive(Clone)]
pub struct WasmSkillContext {
    pub engine: Arc<WasmSkillEngine>,
    pub store: Arc<WasmSkillStore>,
    pub tools: Arc<ToolRegistry>,
    pub skill_state: Arc<SkillStateTracker>,
    /// The agents an install, update or uninstall through these tools reaches.
    pub hosts: SkillHosts,
    pub data_dir: PathBuf,
    /// Supply-chain policy for installs. Cloud deployments tighten these;
    /// single-tenant CLI keeps the permissive defaults.
    pub policy: WasmInstallPolicy,
    /// Agent vault, used to resolve `skills/github/token` for authenticated
    /// fetches when the github skill is configured. `None` for callers
    /// that don't have a vault (single-tenant CLI).
    pub agent_vault: Option<Arc<crate::vault::Vault>>,
    /// Optional global vault for the same lookup; if set, layered with
    /// `agent_vault` via [`crate::user_config::LayeredUserVault`] so a
    /// per-user GitHub PAT at `users/{uid}/skills/github/token` takes
    /// precedence over the agent-shared default.
    pub global_vault: Option<Arc<crate::vault::Vault>>,
    /// Unix socket of the privilege-separated runner daemon (audit F7). When
    /// `Some`, the skill `cargo build` dispatches to the daemon (fail-closed if
    /// unreachable) so it runs as the unprivileged agent user; `None` builds
    /// in-process (dev/local). See the ambient-credential caveat on
    /// [`build_skill_project`].
    pub runner: Option<RunnerEndpoint>,
    /// Roots every agent-supplied path resolves against, in the hub's usual
    /// order (agent workspace first, then worktrees). Relative paths land in
    /// the persistent workspace instead of the container filesystem a deploy
    /// discards.
    pub path_roots: Vec<PathBuf>,
    /// Status-aware admin policy, shared with the vault tools. Only decides the
    /// bypass over per-skill ownership — authoring itself is member-level.
    pub admin: crate::authz::IsAdminFn,
    /// Late-bound team store: how a team-visible skill, and a grant naming a
    /// team, reach the caller of the current turn.
    pub teams: crate::def_write_auth::TeamStoreHandle,
    /// The `[[skills.catalogs]]` a skill can be published to.
    pub catalogs: Vec<workforce_common::settings::CatalogSource>,
}

impl WasmSkillContext {
    /// The identity driving this call, with its admin standing.
    pub(crate) fn caller(&self) -> crate::skill_access::SkillCaller {
        crate::skill_access::SkillCaller::current(&self.admin, &self.teams)
    }

    /// Refuse a host compile this caller may not drive here.
    fn allow_host_compile(&self) -> Result<(), String> {
        crate::wasm_skill_access::require_host_compile(
            &self.caller(),
            crate::wasm_skill_access::build_isolation(),
        )
    }

    /// The agent's persistent workspace — the first path root, by the same
    /// convention every other path-taking tool uses.
    fn workspace_root(&self) -> Option<&Path> {
        self.path_roots.first().map(PathBuf::as_path)
    }

    /// Whether the hub writing at `path` would land on the agent's
    /// configuration.
    fn reaches_config(&self, path: &Path) -> bool {
        self.workspace_root()
            .is_some_and(|workspace| config_guard::reaches_config(workspace, path))
    }

    /// Anchor an agent-supplied path to the agent's workspace.
    fn resolve_path(&self, raw: &str) -> Result<PathBuf, String> {
        workforce_agent::tools::paths::resolve_in_roots(raw, &self.path_roots)
    }

    /// Resolve the GitHub token for a `wasm_skill.install` fetch.
    ///
    /// Mirrors the resolution policy used by the `github.clone` tool: a
    /// per-org workspace token (`skills/github/workspaces/{owner}/token`)
    /// wins over the bare default (`skills/github/token`) when an `owner`
    /// is known, and per-user overrides win over agent-shared values
    /// across both forms (via `LayeredUserVault`). Returns `None` when no
    /// token is configured — the installer then falls back to anonymous
    /// fetches, which only works for public repos.
    ///
    /// Pass `Some(owner)` whenever the caller knows which GitHub org the
    /// fetch will hit (i.e. `github_repo` installs and updates). Pass
    /// `None` for `project_path` builds, where the deps' owners aren't
    /// known up-front — those resolve via the bare flat path only.
    pub fn resolve_github_token(&self, owner: Option<&str>) -> Option<String> {
        use workforce_common::settings::SecretStore;
        let agent = self.agent_vault.as_ref()?.clone();
        let layered = crate::user_config::LayeredUserVault::new(agent, self.global_vault.clone());
        match owner {
            Some(o) => layered.get_github_pat(o),
            None => layered.get("skills/github/token"),
        }
    }
}

/// Where `wasm_skill.scaffold` puts projects when the caller names no
/// directory: under the agent's persistent workspace, never the process CWD.
const DEFAULT_SCAFFOLD_DIR: &str = "wasm-skills";

/// Record the project a skill was installed from, so the source stays
/// recoverable (`wasm_skill.restore_source`) for as long as the skill is
/// installed. A snapshot failure never fails an install that already
/// succeeded — the binary is live either way — but it is logged and reported,
/// because silently losing the source is the whole failure this closes.
fn record_source_snapshot(
    store: &WasmSkillStore,
    skill: &str,
    project_dir: &Path,
    workspace: Option<&Path>,
    result: &mut ToolResult,
) {
    if !result.success || !result.output.is_object() {
        return;
    }
    if let Some(installed) = store.get_installed(skill) {
        snapshot_into_result(store, skill, project_dir, &installed, result);
    }
    note_project_location(
        project_dir,
        workspace,
        store.has_source_snapshot(skill),
        result,
    );
}

/// A rebuild that changed the binary but not `[skill] version` is what
/// `wasm_skill.publish` refuses; say so at build time, when the fix is a
/// one-line edit, rather than at publish time.
pub(crate) fn unbumped_rebuild_note(
    previous: Option<&InstalledWasmSkill>,
    installed: &InstalledWasmSkill,
) -> Option<String> {
    previous
        .filter(|p| {
            p.manifest.version == installed.manifest.version && p.sha256 != installed.sha256
        })
        .map(|p| {
            format!(
                "Rebuilt '{}' at version {} with different bytes. Bump [skill] version in \
                 Skill.toml before wasm_skill.publish, or the publish is refused.",
                installed.manifest.name, p.manifest.version
            )
        })
}

fn note_unbumped_rebuild(
    previous: Option<&InstalledWasmSkill>,
    store: &WasmSkillStore,
    skill: &str,
    result: &mut ToolResult,
) {
    if !result.success || !result.output.is_object() {
        return;
    }
    if let Some(note) = store
        .get_installed(skill)
        .and_then(|installed| unbumped_rebuild_note(previous, &installed))
    {
        result.output["version_note"] = serde_json::json!(note);
    }
}

/// Say when a project lives outside the agent's persistent workspace — a git
/// worktree or other scratch tree, torn down without warning. Allowed, since
/// the stored snapshot makes it recoverable, but not left to be discovered.
fn note_project_location(
    project_dir: &Path,
    workspace: Option<&Path>,
    durable_copy: bool,
    result: &mut ToolResult,
) {
    if !result.output.is_object() || workspace.is_none_or(|root| project_dir.starts_with(root)) {
        return;
    }
    result.output["project_outside_workspace"] = serde_json::json!(true);
    result.output["project_outside_workspace_note"] = serde_json::json!(match durable_copy {
        true =>
            "This project directory is outside your persistent workspace and will not survive a \
             restart. The source stored with the installed skill is the durable copy — recover it \
             with wasm_skill.restore_source.",
        false =>
            "This project directory is outside your persistent workspace and will not survive a \
             restart, and nothing durable holds this source yet. Install the skill, or keep the \
             project under your workspace.",
    });
}

fn snapshot_into_result(
    store: &WasmSkillStore,
    skill: &str,
    project_dir: &Path,
    installed: &workforce_wasm_runtime::InstalledWasmSkill,
    result: &mut ToolResult,
) {
    match store.snapshot_source(skill, project_dir, installed) {
        Ok(snapshot) => {
            result.output["source_snapshot"] = serde_json::json!({
                "files": snapshot.files,
                "bytes": snapshot.bytes,
            });
            if let Some(map) = result.output.as_object_mut() {
                map.remove("source_snapshot_stale");
            }
        }
        Err(e) => {
            tracing::warn!(
                skill = %skill,
                project = %project_dir.display(),
                error = %e,
                "Failed to snapshot WASM skill source"
            );
            result.output["source_snapshot_error"] = serde_json::json!(e.to_string());
        }
    }
}

/// Cloud-vs-local install policy. See [`workforce_common::settings::WasmSkillsConfig`].
#[derive(Debug, Clone, Copy)]
pub struct WasmInstallPolicy {
    pub allow_build_on_install: bool,
    pub require_sha256_pin: bool,
}

impl Default for WasmInstallPolicy {
    fn default() -> Self {
        Self {
            allow_build_on_install: true,
            require_sha256_pin: false,
        }
    }
}

impl From<&workforce_common::settings::WasmSkillsConfig> for WasmInstallPolicy {
    fn from(c: &workforce_common::settings::WasmSkillsConfig) -> Self {
        Self {
            allow_build_on_install: c.allow_build_on_install,
            require_sha256_pin: c.require_sha256_pin,
        }
    }
}

/// Create all WASM skill management tools.
pub fn wasm_skill_tools(ctx: WasmSkillContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WasmSkillInstallTool::new(ctx.clone())),
        Arc::new(WasmSkillUninstallTool::new(ctx.clone())),
        Arc::new(WasmSkillUpdateTool::new(ctx.clone())),
        Arc::new(WasmSkillListTool::new(ctx.clone())),
        Arc::new(WasmSkillBuildTool::new(ctx.clone())),
        Arc::new(WasmSkillRestoreSourceTool::new(ctx.clone())),
        Arc::new(WasmSkillExportTool::new(ctx.clone())),
        Arc::new(WasmSkillImportTool::new(ctx.clone())),
        Arc::new(WasmSkillTestTool::new(ctx.clone())),
        Arc::new(crate::wasm_skill_publish::WasmSkillPublishTool::github(
            ctx.clone(),
        )),
        Arc::new(WasmSkillScaffoldTool::new(ctx)),
        Arc::new(WasmSkillGuideTool),
    ]
}

// ─── wasm_skill.guide ───────────────────────────────────────────────────────

/// The complete authoring contract, servable in one call so an agent never
/// has to discover the rules by hitting them. Every rule stated here is
/// enforced by the runtime; keep them in lockstep.
pub const AUTHORING_GUIDE: &str = r#"# WASM Skill Authoring Guide

Read this fully BEFORE writing any skill code. Every rule is host-enforced;
violating one costs a failed call, not a warning.

## What a skill is
Sandboxed Rust compiled to wasm32-unknown-unknown using `workforce-skill-sdk`.
Deterministic, fast, repeatable — the preferred form for API-shaped automation
(auth → call → summarize). Author with `wasm_skill.scaffold`, compile+install
with `wasm_skill.build` (project_path, install=true).

## Where a skill project lives
Every path you give these tools is relative to your persistent workspace —
the same root `filesystem.*` and `code.*` use. `wasm_skill.scaffold` defaults
to `wasm-skills/{name}`; keep using that relative form and every tool agrees
on the location. Absolute paths outside the workspace are refused, and paths
written anywhere else do not survive a restart.

Installing from a project also stores a snapshot of its source
(Skill.toml, Cargo.toml, src/) beside the installed binary. If the project
directory is ever missing or stale, `wasm_skill.restore_source` brings the
real source back — check `wasm_skill.list` (`source_snapshot`) before
assuming work is lost, and never rebuild from a stub you find on disk.

## Skill.toml — the entire contract in one file
```toml
[skill]
name = "record-keeper"                # kebab-case
version = "0.1.0"
description = "Apply bulk changes to records"

[capabilities]                        # each gates a host API family
http = true                           # http::fetch
vault = true                          # vault::get / vault::set
tools = true                          # tools::invoke (needs [[permissions]])

[network]
allowed_hosts = ["api.example.com"]   # http::fetch refuses EVERY other host

[[credentials]]                       # secrets the USER provisions
name = "prod_password"                # lowercase identifier; 'tools' reserved
description = "Admin password for the records API"
scope = "user"                        # or "shared" (admin-granted, all users)

[[permissions]]                       # platform tools the skill may invoke
tool = "slack.post_message"           # exact name, no wildcards
description = "Post the run summary"
allowed_hosts = []                    # non-empty → URL args limited to these

[[tools]]                             # what the skill EXPOSES to the agent
name = "apply_change"
description = "Apply an action to a batch of recipients"
requires_approval = true              # a human must approve EVERY call
approval_summary = "apply `{{action}}` to {{recipients|count}} recipient(s)"
input_schema = { type = "object", properties = { action = { type = "string" }, recipients = { type = "array" } } }
```

## The guest API (workforce_skill_sdk::prelude::*)
- `vault::get("name")` / `vault::set("key", v)` — ONLY the skill's own locker
  (`wasm-skills/{skill}/users/{uid}|shared/…`), never the user's general
  vault. Declared credentials are READ-ONLY to skill code.
- `http::fetch(method, url, headers, body)` — hosts in [network]
  allowed_hosts only. Internal/metadata IPs always refused.
- `tools::invoke("tool.name", &json!({...}))` — declared [[permissions]]
  only; returns `{"success", "output", "error"}`. Rate-limited calls (429)
  are retried host-side with backoff automatically, honouring the server's
  Retry-After; a refusal that outlives the retries adds
  `"rate_limited": {"retry_after_secs": N|null}` to the envelope — branch
  on that key, never on error prose. Never pass
  `[[secret:…]]` or `[[vault:…]]` markers — the call fails.
- `tools::invoke_many(&[("tool.name", json!({...})), …])` — same governance
  per element, dispatched host-side with bounded per-service concurrency
  (different services run concurrently; same-service calls are limited to a
  few in flight); one envelope per element, in order. Guests have no
  threads, so use this instead of a serial loop for fan-out (e.g. one query
  per service).
- `tools::call("tool.name", &json!({...}))` — `tools::invoke` with the
  envelope unwrapped: `Ok(output)` or `Err("tool.name failed: …")`. Pass
  `&tools::call` wherever the SDK asks for a `tools::Call`, and a closure
  in the skill's own tests.
- `pipeline::Shipped::new("id", include_str!("….json")).with_guide(md)` —
  a pipeline the skill ships: `.ensure(&tools::call)` registers the
  definition when the hub holds none or an older `revision`;
  `.record(&tools::call, item, stage, event, detail, run)` ensures then
  records one move and returns `recorded → <column>` or `skipped: …`
  (best-effort, never a failed step). Needs `pipeline.get`,
  `pipeline.define` and `pipeline.record` in [[permissions]]; `run` is
  the `_instance_id` a tool step's input carries.
- `log::info/warn/error`, `config::get`, `artifact::read/stage`.
- Budgets per invocation: 30s guest CPU (the clock re-arms after each host
  call, so tool/HTTP wall time never consumes it), 256 MiB memory,
  32 tool calls, 64 HTTP fetches.

## Credentials & consent — the agent can NEVER provision secrets
1. Declare the credential in Skill.toml, build, install.
2. The USER types two chat commands: `cmd: secret <name> <value>` then
   `cmd: skill grant <skill> <name>`. Do not ask for the value in plain chat
   and do not try vault.set — both are blocked.
3. Tool permissions need their own approval: the USER types
   `cmd: skill grant <skill> tools`.
4. Grants pin the skill's CONSENT CONTRACT — capabilities, [network]
   allowed_hosts, [[credentials]] names+scopes, [[permissions]] tools+hosts.
   Rebuilding with an unchanged contract KEEPS every grant; editing any part
   of the contract invalidates them all until the user re-runs
   `cmd: skill grant` (`cmd: skill grants <skill>` shows status). Re-granting
   a credential keeps its stored value. Grants approved before contract
   pinning migrate automatically on their next successful use — no action
   needed while the build they pinned is still installed.

## Tools that must not run unattended — requires_approval
A tool that should never fire without a human decision declares
`requires_approval = true` and an `approval_summary`. The HOST renders that
template into the prompt the approver reads, from the call's own arguments:
`{{field}}` interpolates a dotted lowercase path, `{{field|count}}` how many
items it holds, and an absent field renders `(missing)` so a hole reads as a
hole. The guest never runs to produce it, so the sentence cannot disagree with
what would execute, and an unusable template just falls back to the generic
parameter summary.

The gate only ever ADDS a prompt. It cannot loosen a hub rule, and a member's
saved "allow every time" cannot skip it. It asks that A human decides, not
WHICH human — four-eyes stays an operator's permission rule with designated
approvers and self-approval off.

Where there is nobody to ask, a gated tool does not run at all: a workflow tool
step fails, a scheduled job fails, and other skills could never reach it
anyway. `wasm_skill.test` still dry-runs it, so the gate stays testable.

Adding or removing a gate changes the CONSENT CONTRACT and users re-run
`cmd: skill grant`; rewording the summary does not.

Every `cmd: skill …` command in this guide is also a `skill.*` tool on the
user's own remote MCP connection (Claude Code): `skill.grant`, `skill.grants`,
`skill.revoke`, `skill.share`, `skill.unshare`, `skill.visibility`. There the
value comes from the user's own vault (`vault.set` first), never from a tool
argument. Those tools exist only on that surface — no agent has them.

## Who owns a skill, and who can use it
A skill belongs to the user who installed it. Nobody needs an admin:
- New installs are PRIVATE — only the owner can call the tools. Pass
  `visibility = "team"` (anyone in a team with you) or `visibility = "everyone"`
  on install, or run `cmd: skill visibility <skill> team|everyone` later.
- The owner shares with named people and whole teams:
  `cmd: skill share <skill> <@user|team:<name>>` lets them use it; adding
  `manage` also lets them update and uninstall it.
  `cmd: skill unshare <skill> <@user|team:<name>>` takes it back. Only the OWNER
  changes who reaches a skill — a `manage` grantee can edit it but not
  re-share it.
- `wasm_skill.list` shows only the skills the caller may use, with `owner`,
  `visibility` and `your_access`. Another user's private skill is not in your
  tool list and cannot be called.
- Skills installed for the whole org (no owner) stay usable by everyone and
  are administered by admins.
- External (SKILL.md) skills installed with `system.install_external_skill` follow the same
  ownership rules, and `cmd: skill share`, `cmd: skill unshare` and
  `cmd: skill visibility` work on them by name.

## Sharing across agents and users (org hubs)
Installs are hub-wide and INSTANT: a built or edited skill is immediately
runnable via every agent — no restart. Identity is always the requesting
user, never the agent. Scopes:
- `scope = "user"` credential: each user grants their own once (`cmd: secret`
  + `cmd: skill grant`), then it works for them on every agent.
- `scope = "shared"` credential: ONE value serves every caller, so the skill's
  OWNER provides it — `cmd: secret <name> <value>` then
  `cmd: skill grant <skill> <name>`.
- Tools: each user grants their own with `cmd: skill grant <skill> tools`,
  OR an admin covers everyone at once with
  `cmd: skill grant <skill> tools shared` (that one is admin-only: it consents
  on other users' behalf) — each call still runs with the invoking user's own
  capability limits.
A contract change invalidates ALL grants (per-user and shared) at once;
pure code rebuilds keep them.

## Installing from GitHub
`wasm_skill.install` with `github_repo = "owner/repo@<ref>[#subpath=dir]"`.
A supply-chain pin is REQUIRED, one of:
- an exact commit ref: `owner/repo@<40-hex-commit-sha>#subpath=my-skill`
  (recommended for clone-and-build; resolve a tag to its commit first), or
- `sha256` of a published release `.wasm` asset (repo-root releases only).
Private repos are read with the caller's GitHub token:
`skills/github/workspaces/<owner>/token`, else `skills/github/token`.
Reinstalling identical bytes+manifest keeps the same fingerprint, so existing
grants survive a same-version reinstall on a new hub or agent.
Installing again replaces the skill's config with the `config` you pass, so
pass the whole set again. `wasm_skill.update` only moves a release-tag install
to a newer release; update a commit install by installing the new commit.
The same `github_repo` installs an external (SKILL.md) skill with
`system.install_external_skill`, and a skill a configured catalog lists
installs by name with `system.install_catalog_skill`.

## Moving a skill between hubs without a repo
`wasm_skill.export <skill>` hands the user a downloadable `.wfskill.zip`
(manifest + binary + provenance digest). They upload it in the other hub's
chat and its agent runs `wasm_skill.import` with the uploaded attachment's
artifact id — the recorded digest is verified before anything installs.
Grants are per-hub: after import, users (or the skill's owner for shared
scope) run
`cmd: skill grant` there — unless that hub already granted the skill with
the same consent contract, in which case its grants stay valid.

## Publishing to the catalog — wasm_skill.publish
`wasm_skill.publish {skill, catalog?, base?, summary?}` opens a pull request
against a configured `[[skills.catalogs]]` repo containing the installed
skill's stored source snapshot and its `catalog.json` entry (kind wasm,
built from source on install). Deterministic: the hub copies exactly what
is installed, so build and install first, then publish. It is refused when
the installed version already exists in the repo with different source or a
different consent contract — bump `[skill] version` in Skill.toml, rebuild,
publish again. Uses the caller's GitHub token for the repo's org
(`skills/github/workspaces/<owner>/token`), never the catalog read token.
An installed external (SKILL.md) skill publishes the same way with
`system.publish_external_skill`, which takes its installed files.

## Testing without grants — wasm_skill.test
Dry-run any INSTALLED skill tool against mocks; no grants — credentials,
platform tools, and the network are never real:
`wasm_skill.test {skill, tool, input, mock_tools: {"datadog.logs_search":
{"success": true, "output": {…}}}, mock_credentials: {"api_key": "test"}}`.
Declared credentials resolve from mock_credentials, [[permissions]] tools
return your canned envelopes (every call lands in the result's `invoked`
list), outbound HTTP is refused (a host denial in test mode does not mean
the manifest is missing the host), and vault writes go to a throwaway
store; only artifact reads/stages use the real session-scoped store. The
real sandbox still applies, so declaration mistakes surface here. Iterate
with build → test; ask the user for grants once, when the skill is ready
to run for real.

## Tool invocation limits
Each `tools::invoke` passes the calling user's capability limits and the
policy engine — a skill acts as the user, never more. Never grantable:
wasm_skill.*, vault.*, secret.*, system.*, mcp.*, agent.*, role.*, pairing.*,
exec.*, code.*, coding.*, workflow, subagent, jobs, and other skills' tools.
Approval-gated (Ask-tier) tools deny — an admin must pre-allow the capability.

## Using a skill from a workflow (deterministic steps, no LLM)
```toml
[[steps]]
id = "apply"
name = "Apply the change"
tool = "record-keeper.apply_change"
task = '{"recipients": "{{r}}"}'      # template resolves, then parses as JSON
```
The result's fields become step outputs (`steps.apply.<key>`); an `outcome`
string field routes branches. Only WASM skill tools may be step tools, and a
tool with `requires_approval` cannot be one: a workflow runs unattended, so
there is nobody for it to ask.

## Failure modes → fixes
- Manifest edit "didn't take" (grant says undeclared after a rebuild) → you
  installed via `wasm_path`, which reads the Skill.toml BESIDE the .wasm.
  Always build+install with `project_path` after editing Skill.toml.
- "host not in manifest [network] allowed_hosts" → add the host, rebuild
  (a [network] edit changes the contract, so the user re-grants).
- "has not been granted by the user" → the user runs the cmd: commands above.
- "grant is stale: the skill's consent contract changed" → user re-runs
  `cmd: skill grant`. Pure code rebuilds never cause this.
- "not declared in [[permissions]]" → declare it, rebuild, user re-grants.
- "requires interactive approval" → admin pre-allows the capability.
- "requires a human to approve every call" → the tool declares
  `requires_approval`; call it from a conversation with the agent, never from
  a workflow tool step or a job.
- "belongs to another user" → that skill is not yours; ask its owner to run
  `cmd: skill share <skill> <you> manage`, or pick a different name.
- "compiles Rust as the hub process, which is admin-only here" → this hub
  does not sandbox builds. Build in CI (the scaffold writes a release
  workflow) and install the published asset with `github_repo` + `sha256`.
- Repeated 429s from a platform tool → already retried with backoff
  host-side; if it still fails, fan out smaller batches via
  `tools::invoke_many` or split the work across calls.
"#;

/// `wasm_skill.guide` — hands the agent the full authoring contract.
#[derive(Debug)]
pub struct WasmSkillGuideTool;

#[async_trait]
impl Tool for WasmSkillGuideTool {
    fn name(&self) -> &str {
        "wasm_skill.guide"
    }

    fn description(&self) -> &str {
        "The complete WASM skill authoring guide: Skill.toml schema, the guest API, \
         credential/tool consent flow, sandbox limits, and workflow integration. Call \
         this BEFORE scaffolding or editing any skill."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult::success(
            serde_json::json!({"guide": AUTHORING_GUIDE}),
            0,
        ))
    }
}

// ─── wasm_skill.test ────────────────────────────────────────────────────────

/// In-memory locker for a test run: every grant-record path answers with the
/// current fingerprints, declared credentials resolve from the supplied
/// mocks by trailing key, and writes land in a throwaway map — the real
/// vault is never constructed, so no stored secret is reachable.
struct TestLockerVault {
    grants_prefix: String,
    grant_record: String,
    credentials: HashMap<String, String>,
    writes: parking_lot::Mutex<HashMap<String, String>>,
}

impl workforce_common::settings::SecretStore for TestLockerVault {
    fn get(&self, path: &str) -> Option<String> {
        if path.starts_with(&self.grants_prefix) {
            return Some(self.grant_record.clone());
        }
        if let Some(written) = self.writes.lock().get(path) {
            return Some(written.clone());
        }
        self.credentials
            .iter()
            .find(|(key, _)| path.ends_with(&format!("/{key}")))
            .map(|(_, value)| value.clone())
    }
    fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
        self.writes
            .lock()
            .insert(path.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, path: &str) -> eyre::Result<bool> {
        Ok(self.writes.lock().remove(path).is_some())
    }
    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        self.writes
            .lock()
            .keys()
            .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
            .cloned()
            .collect()
    }
}

/// Records every invocation and answers from canned envelopes — the mock
/// dispatch side of `wasm_skill.test`. Nothing reaches the real pipeline.
struct TestToolInvoker {
    responses: HashMap<String, serde_json::Value>,
    calls: parking_lot::Mutex<Vec<serde_json::Value>>,
}

impl workforce_wasm_runtime::types::SkillToolInvoker for TestToolInvoker {
    fn invoke(
        &self,
        _ctx: &workforce_wasm_runtime::types::SkillCallContext,
        permission: &workforce_wasm_runtime::manifest::ManifestPermission,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.calls
            .lock()
            .push(serde_json::json!({"tool": permission.tool, "args": args}));
        Ok(self
            .responses
            .get(&permission.tool)
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "success": false,
                    "output": serde_json::Value::Null,
                    "error": format!(
                        "no mock response configured for '{}' — add it to mock_tools",
                        permission.tool
                    ),
                })
            }))
    }
}

/// `wasm_skill.test` — dry-run an installed skill tool against mocks. The
/// real sandbox (declarations, budgets, limits) still applies; only the
/// endpoints are fake, so no grants are needed and nothing real is touched.
pub struct WasmSkillTestTool {
    ctx: WasmSkillContext,
}

impl WasmSkillTestTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct TestInput {
    /// Installed skill name.
    skill: String,
    /// Tool to run — bare (`fetch`) or qualified (`skill.fetch`).
    tool: String,
    #[serde(default)]
    input: serde_json::Value,
    /// Canned platform envelope per [[permissions]] tool name.
    #[serde(default)]
    mock_tools: HashMap<String, serde_json::Value>,
    /// Value per declared credential name.
    #[serde(default)]
    mock_credentials: HashMap<String, String>,
    /// Config overrides; defaults to the installed config.
    #[serde(default)]
    config: Option<HashMap<String, String>>,
}

#[async_trait]
impl Tool for WasmSkillTestTool {
    fn name(&self) -> &str {
        "wasm_skill.test"
    }

    fn description(&self) -> &str {
        "Dry-run an installed WASM skill tool against mocks — no grants needed; \
         credentials, platform tools, and the network are never real. Declared \
         credentials resolve from mock_credentials, [[permissions]] tools return the \
         canned mock_tools envelopes (every call is recorded in the result's `invoked` \
         list), outbound HTTP is refused, and vault writes go to a throwaway store. \
         Only artifact reads/stages hit the real session-scoped store. The real \
         sandbox (declarations, budgets, 30s/256MiB limits) still applies. Iterate \
         build → test; ask the user for grants once the skill is ready to run for real."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Installed skill name" },
                "tool": {
                    "type": "string",
                    "description": "Skill tool to run (bare or skill-qualified name)"
                },
                "input": {
                    "type": "object",
                    "description": "Input object passed to the tool handler"
                },
                "mock_tools": {
                    "type": "object",
                    "description": "Canned platform envelope ({success, output, error}) \
                                    per [[permissions]] tool name; unmocked tools return \
                                    an explanatory failure envelope",
                    "additionalProperties": { "type": "object" }
                },
                "mock_credentials": {
                    "type": "object",
                    "description": "Value per declared credential name",
                    "additionalProperties": { "type": "string" }
                },
                "config": {
                    "type": "object",
                    "description": "Config overrides (defaults to the installed config)",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["skill", "tool"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: TestInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // A dry run reaches no further than calling the skill's tool would, so
        // it asks for the same standing.
        let installed = match self.ctx.caller().require(
            &self.ctx.store,
            &params.skill,
            workforce_core::access::AccessLevel::Run,
        ) {
            Ok(installed) => installed,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };
        let bare_tool = params
            .tool
            .strip_prefix(&format!("{}.", params.skill))
            .unwrap_or(&params.tool)
            .to_string();
        let Some(handler) = installed.manifest.handler_for_tool(&bare_tool) else {
            let declared: Vec<&str> = installed
                .manifest
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect();
            return Ok(ToolResult::failure(
                format!(
                    "Skill '{}' declares no tool '{bare_tool}'. Declared: [{}]",
                    params.skill,
                    declared.join(", ")
                ),
                duration(),
            ));
        };
        let tool_input = match params.input {
            serde_json::Value::Null => serde_json::json!({}),
            v if v.is_object() => v,
            _ => {
                return Ok(ToolResult::failure(
                    "input must be a JSON object",
                    duration(),
                ))
            }
        };

        // Blocking outbound HTTP is what makes "nothing real is touched"
        // true; the fingerprints are computed from the SAME cleared clone so
        // the mock grant records still validate against it.
        let mut test_manifest = installed.manifest.clone();
        test_manifest.allowed_hosts.clear();
        let build_fingerprint =
            workforce_wasm_runtime::types::grant_fingerprint(&installed.sha256, &test_manifest)
                .map_err(|e| eyre::eyre!("fingerprint failed: {e}"))?;
        let grant_record = serde_json::json!({
            "fingerprint": build_fingerprint,
            "contract_fingerprint":
                workforce_wasm_runtime::types::contract_fingerprint(&test_manifest),
            "granted_at": chrono::Utc::now().to_rfc3339(),
            "granted_by": "wasm_skill.test",
        })
        .to_string();
        let locker = Arc::new(TestLockerVault {
            grants_prefix: format!("wasm-skills/{}/grants/", params.skill),
            grant_record,
            credentials: params.mock_credentials,
            writes: parking_lot::Mutex::new(HashMap::new()),
        });
        let invoker = Arc::new(TestToolInvoker {
            responses: params.mock_tools,
            calls: parking_lot::Mutex::new(Vec::new()),
        });

        if !self
            .ctx
            .engine
            .has_module(&params.skill, installed.binary_version)
            .await
        {
            let bytes = match self
                .ctx
                .store
                .load_binary(&params.skill, installed.binary_version)
            {
                Ok(b) => b,
                Err(e) => return Ok(ToolResult::failure(e.to_string(), duration())),
            };
            if let Err(e) = self
                .ctx
                .engine
                .load_module(&params.skill, installed.binary_version, &bytes)
                .await
            {
                return Ok(ToolResult::failure(e.to_string(), duration()));
            }
        }

        let request = workforce_wasm_runtime::types::WasmToolRequest {
            tool_name: format!("{}.{bare_tool}", params.skill),
            handler,
            input: tool_input,
            agent_id: workforce_agent::runner::CURRENT_AGENT_ID
                .try_with(|id| id.clone())
                .unwrap_or_else(|_| "wasm-skill-test".to_string()),
            user_id: workforce_agent::runner::CURRENT_USER_ID
                .try_with(|id| id.clone())
                .ok()
                .or_else(|| Some("wasm-skill-test-user".to_string())),
            task_id: None,
        };
        let config = params.config.unwrap_or_else(|| installed.config.clone());
        let response = self
            .ctx
            .engine
            .execute_tool(
                &params.skill,
                installed.binary_version,
                &test_manifest,
                &request,
                &config,
                Some(locker as Arc<dyn workforce_common::settings::SecretStore>),
                Some(Arc::clone(&invoker)
                    as Arc<dyn workforce_wasm_runtime::types::SkillToolInvoker>),
            )
            .await;

        match response {
            Ok(resp) => Ok(ToolResult::success(
                serde_json::json!({
                    "skill": params.skill,
                    "tool": request.tool_name,
                    "mode": "mock — no grants used; outbound HTTP blocked (an \
                             allowed_hosts denial in test mode does NOT mean the \
                             manifest is missing the host)",
                    "success": resp.success,
                    "result": resp.result,
                    "error": resp.error,
                    "policy_notice": resp.policy_notice,
                    "invoked": invoker.calls.lock().clone(),
                }),
                duration(),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("WASM execution error: {e}"),
                duration(),
            )),
        }
    }
}

// ─── wasm_skill.install ─────────────────────────────────────────────────────

/// Install a WASM skill from a local `.wasm` file or a local project directory.
///
/// The tool accepts either:
/// - `wasm_path`: path to a pre-built `.wasm` file
/// - `project_path`: path to a skill project directory (must contain `Skill.toml`)
/// - `github_repo`: GitHub repository (e.g., `user/repo` or `user/repo@v1.0.0`)
pub struct WasmSkillInstallTool {
    ctx: WasmSkillContext,
}

impl WasmSkillInstallTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Default, Deserialize)]
struct InstallInput {
    /// Path to a pre-built `.wasm` file.
    #[serde(default)]
    wasm_path: Option<String>,
    /// Path to a skill project directory containing `Skill.toml`.
    #[serde(default)]
    project_path: Option<String>,
    /// GitHub repository reference (e.g., `user/repo` or `user/repo@v1.0.0`).
    #[serde(default)]
    github_repo: Option<String>,
    /// Optional config values for the skill.
    #[serde(default)]
    config: HashMap<String, String>,
    /// Optional SHA-256 pin for the installed `.wasm` bytes. When set,
    /// the install flow computes the digest of the loaded bytes and
    /// rejects on mismatch — supply-chain protection against a
    /// substituted binary at the source. The computed digest is always
    /// persisted in the install record regardless.
    #[serde(default)]
    sha256: Option<String>,
    /// Who may use the skill once installed. Defaults to `private` — the
    /// installer alone — and is ignored when the skill already has an owner.
    #[serde(default)]
    visibility: Option<String>,
}

/// Short, stable tag for the install source — feeds the audit log without
/// echoing arbitrary operator-supplied paths back into the structured event.
fn install_source_label(params: &InstallInput) -> &'static str {
    if params.github_repo.is_some() {
        "github_repo"
    } else if params.project_path.is_some() {
        "project_path"
    } else if params.wasm_path.is_some() {
        "wasm_path"
    } else {
        "unspecified"
    }
}

/// Why this install must carry a sha256 pin, or `None` if pinning is
/// optional for the source/policy combination. `github_repo` is always
/// pinned (the remote bytes are otherwise unverified — supply-chain
/// substitution risk). `wasm_path` and `project_path` are pinned only when
/// the deployment opts in via `[skills.wasm] require_sha256_pin = true`.
/// An `unspecified` source falls through here so the downstream
/// "Provide one of: ..." branch can surface the better error.
fn pin_requirement_reason(source: &str, policy_requires: bool) -> Option<&'static str> {
    match source {
        "github_repo" => {
            Some("github_repo installs require a supply-chain pin to prevent substitution")
        }
        "wasm_path" | "project_path" if policy_requires => {
            Some("this deployment requires every install to carry a sha256 pin")
        }
        _ => None,
    }
}

/// The commit an installed skill's recorded source pins: a GitHub install
/// from `repo@<40-hex>`, and nothing for any other source.
pub(crate) fn source_commit(source: &str) -> Option<&str> {
    let (_, tag, _) = parse_github_repo_ref(source.strip_prefix("github:")?);
    tag.filter(|tag| is_commit_sha(tag))
}

/// Whether a `github_repo` ref is already source-pinned to an exact commit
/// (`repo@<40-hex>[#subpath=…]`), which satisfies the pin requirement without
/// a wasm digest.
fn github_ref_is_commit_pinned(repo_ref: &str) -> bool {
    let (_, tag, _) = parse_github_repo_ref(repo_ref);
    tag.is_some_and(is_commit_sha)
}

/// How to satisfy the pin for a GitHub install — shown when neither pin form
/// is present. Both routes are one step; there is no dead end.
const GITHUB_PIN_RECIPE: &str = "Pin one of two ways: (1) reference an exact commit — \
    `github_repo = \"owner/repo@<40-hex-commit-sha>[#subpath=…]\"` (recommended for \
    clone-and-build installs; resolve a tag to its commit with the github tools or \
    `git rev-parse <tag>`), or (2) pass `sha256` of a published release `.wasm` asset.";

#[async_trait]
impl Tool for WasmSkillInstallTool {
    fn name(&self) -> &str {
        "wasm_skill.install"
    }

    fn description(&self) -> &str {
        static DESC: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!(
                "Install a WASM skill (a Skill.toml project) from a local .wasm file, a local \
                 project directory, or a GitHub repo; installing a skill that is already installed \
                 replaces it. An external skill (a SKILL.md directory) installs with \
                 system.install_external_skill, which takes the same github_repo, and a skill a \
                 configured catalog lists installs by name with system.install_catalog_skill. \
                 {SANDBOX_CONTRACT}"
            )
        });
        &DESC
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "wasm_path": {
                    "type": "string",
                    "description": "Path to a pre-built .wasm file. The Skill.toml BESIDE the .wasm is installed as the manifest — after editing Skill.toml, prefer project_path so the current file is read and re-built."
                },
                "project_path": {
                    "type": "string",
                    "description": "Path to a skill project directory containing Skill.toml and a Cargo.toml. The tool will build the .wasm and install it."
                },
                "github_repo": {
                    "type": "string",
                    "description": "GitHub repo. Forms: 'user/repo', 'user/repo@v1.0.0', 'user/repo@<40-hex-commit-sha>', or 'user/repo#subpath=hello-world' for a mono-repo (forms combine). A supply-chain pin is required, and it also picks the route: pass `sha256` (with no subpath and no commit-sha ref) to install a published release .wasm asset — nothing is compiled, and the install fails rather than falling back to a build; pass an exact commit-sha ref to clone and build that commit on this hub, which needs build rights here. A subpath always clones and builds (treating clone/{subpath}/ as the project dir). Private repos are read with your GitHub token for the owner (skills/github/workspaces/<owner>/token, else skills/github/token)."
                },
                "config": {
                    "type": "object",
                    "description": "Optional config values for the skill. They replace any config the skill already has, so pass the whole set again when reinstalling.",
                    "additionalProperties": { "type": "string" }
                },
                "sha256": {
                    "type": "string",
                    "description": "Hex-encoded SHA-256 pin for the loaded .wasm. REQUIRED for github_repo installs (supply-chain protection — the remote bytes are otherwise unverified). Optional for wasm_path / project_path; set [skills.wasm] require_sha256_pin = true to require it there too. Install fails on digest mismatch; the computed digest is always persisted on the install record."
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "everyone"],
                    "description": "Who may call the new skill's tools: 'private' (default) is you plus anyone you share it with; 'team' is anyone in a team with you; 'everyone' publishes it to all users. Change it later with `cmd: skill visibility <skill> private|team|everyone`. Ignored when reinstalling a skill that already has an owner."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: InstallInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let source = install_source_label(&params);
        let commit_pinned = source == "github_repo"
            && params
                .github_repo
                .as_deref()
                .is_some_and(github_ref_is_commit_pinned);
        if let Some(reason) = pin_requirement_reason(source, self.ctx.policy.require_sha256_pin) {
            if params.sha256.is_none() && !commit_pinned {
                tracing::warn!(
                    target: "sentinel.audit",
                    source = %source,
                    reason = %reason,
                    "wasm_skill.install rejected: missing supply-chain pin",
                );
                let recipe = if source == "github_repo" {
                    GITHUB_PIN_RECIPE
                } else {
                    "Compute the digest of the loaded .wasm (`sha256sum your-skill.wasm`) \
                     and pass it as `sha256`."
                };
                return Ok(ToolResult::failure(
                    format!("{reason}. {recipe}"),
                    duration(),
                ));
            }
        }

        // One classification for "does this compile on the host?", shared with
        // the capability gate, so the branch the gate priced is the branch that
        // runs. Both the deployment policy and the caller's own authority are
        // checked against it — `allow_build_on_install` covers the cloned-repo
        // branch as well as `project_path`.
        let compiles = crate::wasm_skill_access::install_source(
            params.wasm_path.as_deref(),
            params.project_path.as_deref(),
            params.github_repo.as_deref(),
            params.sha256.as_deref(),
        ) == crate::wasm_skill_access::InstallSource::HostCompile;
        if compiles {
            if !self.ctx.policy.allow_build_on_install {
                return Ok(ToolResult::failure(
                    "wasm_skill.install rejected: build-on-install is disabled in this \
                     deployment. Build the .wasm separately and install it with `wasm_path`, \
                     or install a published release asset with `github_repo` + `sha256`."
                        .to_string(),
                    duration(),
                ));
            }
            if let Err(refusal) = self.ctx.allow_host_compile() {
                tracing::warn!(
                    target: "sentinel.audit",
                    source = %source,
                    "wasm_skill.install rejected: host compile not permitted for this caller",
                );
                return Ok(ToolResult::failure(refusal, duration()));
            }
        }

        // Determine source and load manifest + binary
        let mut project_source: Option<PathBuf> = None;
        let (manifest, wasm_bytes, source) = if let Some(ref wasm_path) = params.wasm_path {
            // Install from pre-built .wasm file — expect Skill.toml alongside it
            let wasm_file = match self.ctx.resolve_path(wasm_path) {
                Ok(path) => path,
                Err(e) => return Ok(ToolResult::failure(e, duration())),
            };
            let wasm_file = wasm_file.as_path();
            if !wasm_file.is_file() {
                return Ok(ToolResult::failure(
                    format!("WASM file not found: {}", wasm_path),
                    duration(),
                ));
            }
            let manifest_path = wasm_file
                .parent()
                .map(|dir| dir.join("Skill.toml"))
                .unwrap_or_else(|| PathBuf::from("Skill.toml"));
            let manifest = match SkillManifest::from_file(&manifest_path) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Failed to read Skill.toml at {}: {}",
                            manifest_path.display(),
                            e
                        ),
                        duration(),
                    ))
                }
            };
            let wasm_bytes = std::fs::read(wasm_file)?;
            (manifest, wasm_bytes, wasm_path.clone())
        } else if let Some(ref project_path) = params.project_path {
            // Install from project directory — build first, then install
            let project_dir = match self.ctx.resolve_path(project_path) {
                Ok(dir) => dir,
                Err(e) => return Ok(ToolResult::failure(e, duration())),
            };
            let project_dir = project_dir.as_path();
            let manifest_path = project_dir.join("Skill.toml");
            let manifest = match SkillManifest::from_file(&manifest_path) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        match crate::system_tool::holds_external_skill(project_dir) {
                            true => crate::system_tool::external_skill_found(
                                "source",
                                &project_dir.display().to_string(),
                            ),
                            false => format!(
                                "Failed to read Skill.toml at {}: {}",
                                manifest_path.display(),
                                e
                            ),
                        },
                        duration(),
                    ))
                }
            };
            // Build the .wasm. project_path installs use whatever git
            // creds the operator's environment provides; we pass the
            // vault token through too, since some local-dev `Cargo.toml`s
            // also reference private workforce git deps. Owner unknown
            // here (the project's git deps may span any number of orgs),
            // so we resolve via the bare flat path.
            let token = self.ctx.resolve_github_token(None);
            let wasm_bytes =
                match build_skill_project(project_dir, token.as_deref(), self.ctx.runner.as_ref())
                    .await
                {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Build failed: {}", e),
                            duration(),
                        ))
                    }
                };
            project_source = Some(project_dir.to_path_buf());
            (manifest, wasm_bytes, project_path.clone())
        } else if let Some(ref github_repo) = params.github_repo {
            let (repo, _, _) = parse_github_repo_ref(github_repo);
            let owner = repo.split('/').next();
            let token = self.ctx.resolve_github_token(owner);
            match fetch_from_github(
                github_repo,
                token.as_deref(),
                self.ctx.runner.as_ref(),
                compiles,
            )
            .await
            {
                Ok(fetched) => (
                    fetched.manifest,
                    fetched.wasm_bytes,
                    format!("github:{}", github_repo),
                ),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        crate::system_tool::with_clone_hint(
                            format!("GitHub fetch failed: {e}"),
                            &e,
                            crate::system_tool::RepoAccess::Github {
                                owner: owner.unwrap_or(repo),
                                token: token.is_some(),
                            },
                        ),
                        duration(),
                    ))
                }
            }
        } else {
            return Ok(ToolResult::failure(
                "Provide one of: wasm_path, project_path, or github_repo".to_string(),
                duration(),
            ));
        };

        let skill_name = manifest.name.clone();
        let requested_visibility = match params.visibility.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                match workforce_core::access::Visibility::parse_word(raw) {
                    Some(visibility) => Some(visibility),
                    None => {
                        return Ok(ToolResult::failure(
                            format!(
                                "`{raw}` is not a visibility — use `private`, `team` or \
                                 `everyone`."
                            ),
                            duration(),
                        ))
                    }
                }
            }
            _ => None,
        };
        let caller = self.ctx.caller();
        let previous = match caller.require_install(&self.ctx.store, &skill_name) {
            Ok(previous) => previous,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };
        let ownership = caller.ownership(requested_visibility, previous.as_ref());

        // Validate + verify the pin + re-version on disk first, so a rejected
        // reinstall (bad manifest / sha mismatch) leaves the live skill intact.
        let installed = match self.ctx.store.install_prebuilt(
            manifest.clone(),
            &wasm_bytes,
            params.sha256.as_deref(),
            source,
            params.config.clone(),
            ownership,
        ) {
            Ok(installed) => installed,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), duration())),
        };
        let binary_version = installed.binary_version;
        // `install_prebuilt` preserves stored ownership so an update can never
        // reassign a skill — which also means it ignores a visibility the
        // caller asked for on their *own* skill. Apply that here, where the
        // caller's standing is known.
        let installed = match requested_visibility {
            Some(requested)
                if requested != installed.visibility
                    && caller.controls_access(&installed)
                    && crate::skill_access::SkillRegistry::fixed_reach(
                        &self.ctx.store,
                        &installed,
                        requested,
                    )
                    .is_none() =>
            {
                match self.ctx.store.set_access(
                    &skill_name,
                    workforce_core::access::AccessWrite::kept(requested, installed.shares.clone()),
                ) {
                    Ok(updated) => updated,
                    Err(e) => {
                        tracing::warn!(skill = %skill_name, error = %e, "Failed to set skill visibility");
                        installed
                    }
                }
            }
            _ => installed,
        };

        let tool_names = match activate_stored_skill(
            &self.ctx.hosts,
            &self.ctx.engine,
            previous.map(|p| p.binary_version),
            &manifest,
            binary_version,
            &wasm_bytes,
            &params.config,
        )
        .await
        {
            Ok(names) => names,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };

        info!(
            skill = %skill_name,
            version = %manifest.version,
            tools = tool_names.len(),
            "WASM skill installed and activated"
        );

        let mut result = ToolResult::success(
            serde_json::json!({
                "skill": skill_name,
                "version": manifest.version,
                "installed": true,
                "tools_loaded": tool_names.len(),
                "tool_names": tool_names,
                "visibility": installed.visibility.word(),
                "sharing": crate::skill_access::sharing_hint(&installed.manifest.name, &installed),
            }),
            duration(),
        )
        .loading(tool_names);
        match project_source {
            Some(ref project_dir) => record_source_snapshot(
                &self.ctx.store,
                &skill_name,
                project_dir,
                self.ctx.workspace_root(),
                &mut result,
            ),
            // A binary-only install leaves whatever source the last project
            // build stored: still recoverable, no longer what is running.
            None if self.ctx.store.has_source_snapshot(&skill_name) => {
                result.output["source_snapshot_stale"] =
                    serde_json::json!(!self.ctx.store.source_snapshot_matches_binary(&skill_name));
            }
            None => {}
        }
        Ok(result)
    }
}

/// Write one file of a scaffold the hub lays out in the agent's tree, where
/// the agent uid can add entries while it is written.
fn put(roots: &CanonicalRoots, path: impl AsRef<Path>, body: impl AsRef<[u8]>) -> eyre::Result<()> {
    let path = path.as_ref();
    let real = roots
        .resolve(path)
        .ok_or_else(|| eyre::eyre!("{} leads outside the workspace", path.display()))?;
    if let Some(dir) = real.parent() {
        config_guard::create_dirs(roots, dir)?;
    }
    Ok(config_guard::write(roots, &real, body.as_ref())?)
}

// ─── wasm_skill.restore_source ──────────────────────────────────────────────

/// Copy a skill's stored source snapshot back into the workspace.
pub struct WasmSkillRestoreSourceTool {
    ctx: WasmSkillContext,
}

impl WasmSkillRestoreSourceTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RestoreSourceInput {
    /// Installed skill whose source to restore.
    skill: String,
    /// Where to restore it (defaults to `wasm-skills/{skill}`).
    #[serde(default)]
    dest_dir: Option<String>,
}

#[async_trait]
impl Tool for WasmSkillRestoreSourceTool {
    fn name(&self) -> &str {
        "wasm_skill.restore_source"
    }

    fn description(&self) -> &str {
        "Restore an installed WASM skill's source (Skill.toml, Cargo.toml, src/) from the \
         snapshot taken when it was built, into your workspace. Use this when the project \
         directory is missing or stale — the installed skill always carries the source it \
         was built from."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Installed skill name" },
                "dest_dir": {
                    "type": "string",
                    "description": "Directory to restore into, relative to your workspace (default: wasm-skills/{skill}). Must not already contain files."
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RestoreSourceInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // The snapshot is the author's source: recovering it is a management
        // act, not a use of the skill.
        if let Err(refusal) = self.ctx.caller().require(
            &self.ctx.store,
            &params.skill,
            workforce_core::access::AccessLevel::Manage,
        ) {
            return Ok(ToolResult::failure(refusal, duration()));
        }
        if !self.ctx.store.has_source_snapshot(&params.skill) {
            // Only skills the caller could actually restore — naming someone
            // else's would leak what they have installed.
            let recoverable: Vec<String> = self
                .ctx
                .caller()
                .visible(&self.ctx.store)
                .into_iter()
                .filter(|(_, access)| access.allows(workforce_core::access::AccessLevel::Manage))
                .map(|(skill, _)| skill.manifest.name)
                .filter(|name| self.ctx.store.has_source_snapshot(name))
                .collect();
            return Ok(ToolResult::failure(
                format!(
                    "No source snapshot for '{}'. Skills with recoverable source: [{}]. \
                     A snapshot is written when a skill is built and installed from a project.",
                    params.skill,
                    recoverable.join(", ")
                ),
                duration(),
            ));
        }

        let fallback = format!("{DEFAULT_SCAFFOLD_DIR}/{}", params.skill);
        let dest = match self
            .ctx
            .resolve_path(params.dest_dir.as_deref().unwrap_or(&fallback))
        {
            Ok(path) => path,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };
        if self.ctx.reaches_config(&dest) {
            return Ok(ToolResult::failure(
                workforce_common::config_files::WRITE_REFUSED,
                duration(),
            ));
        }

        match self.ctx.store.restore_source(
            &params.skill,
            &dest,
            &CanonicalRoots::new(&self.ctx.path_roots),
        ) {
            Ok(snapshot) => Ok(ToolResult::success(
                serde_json::json!({
                    "skill": params.skill,
                    "restored": true,
                    "path": dest.display().to_string(),
                    "files": snapshot.files,
                    "bytes": snapshot.bytes,
                    "matches_installed_binary": self
                        .ctx
                        .store
                        .source_snapshot_matches_binary(&params.skill),
                }),
                duration(),
            )),
            Err(e) => Ok(ToolResult::failure(e.to_string(), duration())),
        }
    }
}

// ─── wasm_skill.export / wasm_skill.import ──────────────────────────────────

const ARCHIVE_MANIFEST: &str = "Skill.toml";
const ARCHIVE_WASM: &str = "skill.wasm";
const ARCHIVE_META: &str = "export.json";
const ZIP_MIME: &str = "application/zip";
/// Largest accepted import archive (compressed).
const MAX_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
/// Zip-bomb guard: total uncompressed size across entries.
const MAX_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;

/// Provenance carried inside an exported archive. `sha256` makes the archive
/// tamper-evident; `grant_fingerprint` lets an admin confirm cross-hub that
/// an import is byte-identical to the source install.
#[derive(Debug, serde::Serialize, Deserialize)]
struct SkillExportMeta {
    name: String,
    version: String,
    binary_version: u32,
    sha256: String,
    grant_fingerprint: String,
    exported_at: String,
}

fn build_skill_archive(
    manifest_toml: &str,
    wasm_bytes: &[u8],
    meta: &SkillExportMeta,
) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    let mut put = |name: &str, bytes: &[u8]| -> Result<(), String> {
        writer
            .start_file(name, options)
            .and_then(|()| writer.write_all(bytes).map_err(Into::into))
            .map_err(|e| format!("archive write failed at {name}: {e}"))
    };
    put(ARCHIVE_MANIFEST, manifest_toml.as_bytes())?;
    put(ARCHIVE_WASM, wasm_bytes)?;
    let meta_json =
        serde_json::to_vec_pretty(meta).map_err(|e| format!("metadata encode failed: {e}"))?;
    put(ARCHIVE_META, &meta_json)?;
    writer
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|e| format!("archive finalize failed: {e}"))
}

fn parse_skill_archive(bytes: &[u8]) -> Result<(SkillManifest, Vec<u8>, SkillExportMeta), String> {
    use std::io::Read;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "archive is {} bytes, over the {MAX_ARCHIVE_BYTES}-byte limit",
            bytes.len()
        ));
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("not a readable zip archive: {e}"))?;
    let total_unpacked: u64 = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.size()))
        .sum();
    if total_unpacked > MAX_UNPACKED_BYTES {
        return Err(format!(
            "archive unpacks to {total_unpacked} bytes, over the {MAX_UNPACKED_BYTES}-byte limit"
        ));
    }
    let mut read_entry = |name: &str| -> Result<Vec<u8>, String> {
        let mut file = archive
            .by_name(name)
            .map_err(|_| format!("archive is missing {name} — not a skill export"))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| format!("failed to read {name}: {e}"))?;
        Ok(buf)
    };
    let manifest_bytes = read_entry(ARCHIVE_MANIFEST)?;
    let wasm_bytes = read_entry(ARCHIVE_WASM)?;
    let meta_bytes = read_entry(ARCHIVE_META)?;

    let manifest = SkillManifest::from_toml(
        std::str::from_utf8(&manifest_bytes).map_err(|_| "Skill.toml is not UTF-8".to_string())?,
    )
    .and_then(|m| m.validate().map(|()| m))
    .map_err(|e| format!("archive manifest invalid: {e}"))?;
    let meta: SkillExportMeta = serde_json::from_slice(&meta_bytes)
        .map_err(|e| format!("archive metadata invalid: {e}"))?;

    let computed = workforce_wasm_runtime::store::sha256_hex(&wasm_bytes);
    if !computed.eq_ignore_ascii_case(&meta.sha256) {
        return Err(format!(
            "archive integrity check failed: skill.wasm digest {computed} does not match \
             the recorded {} — the archive was modified or corrupted",
            meta.sha256
        ));
    }
    if manifest.name != meta.name {
        return Err(format!(
            "archive metadata names '{}' but the manifest is '{}'",
            meta.name, manifest.name
        ));
    }
    Ok((manifest, wasm_bytes, meta))
}

/// `wasm_skill.export` — package an installed skill for transfer to another
/// hub as a Slack-downloadable zip.
pub struct WasmSkillExportTool {
    ctx: WasmSkillContext,
}

impl WasmSkillExportTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ExportInput {
    skill: String,
}

#[async_trait]
impl Tool for WasmSkillExportTool {
    fn name(&self) -> &str {
        "wasm_skill.export"
    }

    fn description(&self) -> &str {
        "Export an installed WASM skill as a downloadable zip (Skill.toml + binary + \
         provenance digest) for importing on another hub with wasm_skill.import. The \
         user downloads it from the chat and uploads it to the other hub's chat."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Installed skill name to export" }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ExportInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // An export hands over the skill's compiled bytes, so it asks for the
        // same standing as changing the skill.
        let installed = match self.ctx.caller().require(
            &self.ctx.store,
            &params.skill,
            workforce_core::access::AccessLevel::Manage,
        ) {
            Ok(installed) => installed,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };
        if installed.sha256.is_empty() {
            return Ok(ToolResult::failure(
                format!(
                    "Skill '{}' predates build fingerprinting — rebuild it once \
                     (wasm_skill.build) and export again.",
                    params.skill
                ),
                duration(),
            ));
        }
        let fingerprint = installed
            .grant_fingerprint()
            .map_err(|e| eyre::eyre!("fingerprint failed: {e}"))?;
        let wasm_bytes = match self
            .ctx
            .store
            .load_binary(&params.skill, installed.binary_version)
        {
            Ok(b) => b,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), duration())),
        };
        let manifest_toml = installed
            .manifest
            .to_toml()
            .map_err(|e| eyre::eyre!("manifest serialize failed: {e}"))?;
        let meta = SkillExportMeta {
            name: installed.manifest.name.clone(),
            version: installed.manifest.version.clone(),
            binary_version: installed.binary_version,
            sha256: installed.sha256.clone(),
            grant_fingerprint: fingerprint.clone(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        };
        let archive = match build_skill_archive(&manifest_toml, &wasm_bytes, &meta) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };

        let Some(artifact_store) = workforce_agent::artifacts::global_store() else {
            return Ok(ToolResult::failure(
                "No artifact store is configured, so the export cannot be delivered",
                duration(),
            ));
        };
        let agent_id = workforce_agent::runner::CURRENT_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_default();
        let filename = format!("{}-v{}.wfskill.zip", meta.name, meta.version);
        let size = archive.len() as u64;
        let descriptor = workforce_agent::artifacts::ArtifactDescriptor::from_context(
            "wasm_skill.export",
            workforce_agent::artifacts::ContentType::OctetStream,
            agent_id.clone(),
        )
        .with_file_meta(filename.clone(), ZIP_MIME);
        let handle = match artifact_store.write(
            &agent_id,
            descriptor,
            workforce_agent::artifacts::ArtifactBody::Bytes(archive),
        ) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the export for delivery: {e}"),
                    duration(),
                ))
            }
        };

        Ok(ToolResult::success(
            serde_json::json!({
                workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY: [
                    workforce_agent::artifacts::download_marker(
                        &handle.id, ZIP_MIME, &filename, size,
                    )
                ],
                "skill": meta.name,
                "version": meta.version,
                "sha256": meta.sha256,
                "grant_fingerprint": meta.grant_fingerprint,
                "filename": filename,
            }),
            duration(),
        ))
    }
}

/// `wasm_skill.import` — install a skill from an uploaded export archive.
pub struct WasmSkillImportTool {
    ctx: WasmSkillContext,
}

impl WasmSkillImportTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ImportInput {
    /// Artifact id of the uploaded `.wfskill.zip` attachment.
    artifact_id: String,
    #[serde(default)]
    config: HashMap<String, String>,
}

#[async_trait]
impl Tool for WasmSkillImportTool {
    fn name(&self) -> &str {
        "wasm_skill.import"
    }

    fn description(&self) -> &str {
        "Install a WASM skill from an uploaded export archive (.wfskill.zip produced by \
         wasm_skill.export on another hub). Pass the uploaded attachment's artifact id. \
         The archive's recorded digest is verified before anything installs; grants are \
         per-hub, so users re-run cmd: skill grant here afterwards."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "Artifact id of the uploaded .wfskill.zip attachment"
                },
                "config": {
                    "type": "object",
                    "description": "Optional config values for the skill",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["artifact_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ImportInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let Some(artifact_store) = workforce_agent::artifacts::global_store() else {
            return Ok(ToolResult::failure(
                "No artifact store is configured, so uploads cannot be read",
                duration(),
            ));
        };
        let agent_id = workforce_agent::runner::CURRENT_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_default();
        let resolved = match workforce_agent::artifacts::resolve_bytes_input(
            artifact_store.as_ref(),
            &agent_id,
            workforce_agent::artifacts::BytesRequest {
                artifact_id: Some(&params.artifact_id),
                ..Default::default()
            },
        ) {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), duration())),
        };

        let (manifest, wasm_bytes, meta) = match parse_skill_archive(&resolved.bytes) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };

        let caller = self.ctx.caller();
        let previous = match caller.require_install(&self.ctx.store, &manifest.name) {
            Ok(previous) => previous,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };
        let ownership = caller.ownership(None, previous.as_ref());
        let installed = match self.ctx.store.install_prebuilt(
            manifest.clone(),
            &wasm_bytes,
            Some(&meta.sha256),
            format!("import:{}", params.artifact_id),
            params.config.clone(),
            ownership,
        ) {
            Ok(i) => i,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), duration())),
        };
        let tool_names = match activate_stored_skill(
            &self.ctx.hosts,
            &self.ctx.engine,
            previous.map(|p| p.binary_version),
            &manifest,
            installed.binary_version,
            &wasm_bytes,
            &params.config,
        )
        .await
        {
            Ok(names) => names,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };

        info!(
            skill = %manifest.name,
            version = %manifest.version,
            sha256 = %meta.sha256,
            "WASM skill imported from archive"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "skill": manifest.name,
                "version": manifest.version,
                "imported": true,
                "sha256": meta.sha256,
                "grant_fingerprint": meta.grant_fingerprint,
                "tools_loaded": tool_names.len(),
                "tool_names": tool_names,
                "visibility": installed.visibility.word(),
                "sharing": crate::skill_access::sharing_hint(&installed.manifest.name, &installed),
                "note": "Grants are per-hub: users (or the skill's owner, for shared scope) \
                         must run cmd: skill grant here before credentials or tools resolve — \
                         unless this hub already granted the skill with the same consent \
                         contract, in which case its grants stay valid.",
            }),
            duration(),
        )
        .loading(tool_names))
    }
}

// ─── wasm_skill.uninstall ───────────────────────────────────────────────────

pub struct WasmSkillUninstallTool {
    ctx: WasmSkillContext,
}

impl WasmSkillUninstallTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UninstallInput {
    skill: String,
}

#[async_trait]
impl Tool for WasmSkillUninstallTool {
    fn name(&self) -> &str {
        "wasm_skill.uninstall"
    }

    fn description(&self) -> &str {
        "Uninstall a WASM skill, removing its tools and binary"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Name of the WASM skill to uninstall"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: UninstallInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let installed = match self.ctx.caller().require(
            &self.ctx.store,
            &params.skill,
            workforce_core::access::AccessLevel::Manage,
        ) {
            Ok(installed) => installed,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };

        let removed = uninstall_skill(
            &self.ctx.hosts,
            &self.ctx.engine,
            &self.ctx.store,
            &installed,
        )
        .await?;

        Ok(ToolResult::success(
            serde_json::json!({
                "skill": params.skill,
                "uninstalled": true,
                "tools_removed": removed,
            }),
            duration(),
        ))
    }
}

// ─── wasm_skill.list ────────────────────────────────────────────────────────

pub struct WasmSkillListTool {
    ctx: WasmSkillContext,
}

impl WasmSkillListTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for WasmSkillListTool {
    fn name(&self) -> &str {
        "wasm_skill.list"
    }

    fn description(&self) -> &str {
        "List the WASM skills you can use — your own, ones shared with you, and ones \
         published to everyone — with their versions, status, and who they belong to"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let skills: Vec<serde_json::Value> = self
            .ctx
            .caller()
            .visible(&self.ctx.store)
            .iter()
            .map(|(s, access)| {
                let loaded = self.ctx.skill_state.is_loaded(&s.manifest.name);
                serde_json::json!({
                    "name": s.manifest.name,
                    "version": s.manifest.version,
                    "description": s.manifest.description,
                    "state": s.state,
                    "loaded": loaded,
                    "source": s.source,
                    "source_snapshot": self.ctx.store.has_source_snapshot(&s.manifest.name),
                    "source_snapshot_matches_binary": self
                        .ctx
                        .store
                        .source_snapshot_matches_binary(&s.manifest.name),
                    "tools": s.manifest.qualified_tool_names(),
                    "capabilities": s.manifest.capabilities,
                    "installed_at": s.installed_at.to_rfc3339(),
                    "owner": s.owner,
                    "visibility": s.visibility.word(),
                    "shared_with": s.shares.len(),
                    "your_access": access.as_str(),
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "count": skills.len(),
                "skills": skills,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── wasm_skill.update ──────────────────────────────────────────────────────

/// Update an installed WASM skill from its GitHub source.
///
/// Checks for newer releases by comparing semver tags. If a newer version
/// exists, downloads and installs it while preserving the skill's config.
pub struct WasmSkillUpdateTool {
    ctx: WasmSkillContext,
}

impl WasmSkillUpdateTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    /// Name of the installed WASM skill to update.
    skill: String,
    /// Specific version tag to update to (e.g., "v2.0.0"). If omitted, updates to latest.
    #[serde(default)]
    version: Option<String>,
}

#[async_trait]
impl Tool for WasmSkillUpdateTool {
    fn name(&self) -> &str {
        "wasm_skill.update"
    }

    fn description(&self) -> &str {
        "Update an installed WASM skill to a newer GitHub release, keeping its config. Only for \
         skills installed from a release tag: update a skill installed from a commit by \
         installing the new commit with wasm_skill.install, passing its config again."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Name of the installed WASM skill to update"
                },
                "version": {
                    "type": "string",
                    "description": "Specific version tag (e.g., 'v2.0.0'). Omit for latest."
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: UpdateInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let installed = match self.ctx.caller().require(
            &self.ctx.store,
            &params.skill,
            workforce_core::access::AccessLevel::Manage,
        ) {
            Ok(installed) => installed,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };

        // Only skills installed from GitHub can be updated this way
        let github_repo = match installed.source.strip_prefix("github:") {
            Some(repo) => repo.to_string(),
            None => {
                return Ok(ToolResult::failure(
                    format!(
                        "Skill '{}' was installed from '{}', not from GitHub. \
                         Reinstall with wasm_skill.install to update.",
                        params.skill, installed.source
                    ),
                    duration(),
                ))
            }
        };

        // Strip any existing version tag from the repo reference
        let base_repo = github_repo
            .split('@')
            .next()
            .unwrap_or(&github_repo)
            .to_string();

        let current_version = &installed.manifest.version;

        // Check for available versions via GitHub API
        let target_tag = match &params.version {
            Some(v) => v.clone(),
            None => {
                // Fetch latest release tag
                match fetch_latest_github_tag(&base_repo).await {
                    Ok(tag) => tag,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Failed to check for updates: {}", e),
                            duration(),
                        ))
                    }
                }
            }
        };

        // Parse versions for comparison
        let target_ver_str = target_tag.strip_prefix('v').unwrap_or(&target_tag);
        let current_semver = match semver::Version::parse(current_version) {
            Ok(v) => v,
            Err(_) => {
                return Ok(ToolResult::failure(
                    format!("Current version '{}' is not valid semver", current_version),
                    duration(),
                ))
            }
        };
        let target_semver = match semver::Version::parse(target_ver_str) {
            Ok(v) => v,
            Err(_) => {
                return Ok(ToolResult::failure(
                    format!("Target version '{}' is not valid semver", target_ver_str),
                    duration(),
                ))
            }
        };

        if target_semver <= current_semver {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "skill": params.skill,
                    "current_version": current_version,
                    "target_version": target_ver_str,
                    "up_to_date": true,
                    "message": format!(
                        "Skill '{}' is already at v{} (target: v{})",
                        params.skill, current_version, target_ver_str
                    ),
                }),
                duration(),
            ));
        }

        // The fetch falls back to clone-and-build when the release carries no
        // `.wasm` asset, so an update is a potential host compile. The gate
        // can't see that from the input (the source lives on the record), so
        // the authority check happens here.
        if let Err(refusal) = self.ctx.allow_host_compile() {
            return Ok(ToolResult::failure(refusal, duration()));
        }

        // Fetch the new version from GitHub
        let repo_ref = format!("{}@{}", base_repo, target_tag);
        let owner = base_repo.split('/').next();
        let token = self.ctx.resolve_github_token(owner);
        let (manifest, wasm_bytes) =
            match fetch_from_github(&repo_ref, token.as_deref(), self.ctx.runner.as_ref(), true)
                .await
            {
                Ok(fetched) => (fetched.manifest, fetched.wasm_bytes),
                Err(e) => {
                    return Ok(ToolResult::failure(
                        crate::system_tool::with_clone_hint(
                            format!("Failed to fetch v{}: {}", target_ver_str, e),
                            &e,
                            crate::system_tool::RepoAccess::Github {
                                owner: owner.unwrap_or(&base_repo),
                                token: token.is_some(),
                            },
                        ),
                        duration(),
                    ))
                }
            };

        if let Err(e) = manifest.validate() {
            return Ok(ToolResult::failure(
                format!("New version has invalid manifest: {}", e),
                duration(),
            ));
        }

        // Preserve existing config
        let preserved_config = installed.config.clone();
        let old_binary_version = installed.binary_version;

        // Re-version on disk first, then evict the prior live version, so a
        // failed store leaves the running skill intact.
        let new_installed = match self.ctx.store.install_prebuilt(
            manifest.clone(),
            &wasm_bytes,
            None,
            format!("github:{}", repo_ref),
            preserved_config.clone(),
            // Ignored: the record already has an owner, which the store keeps.
            self.ctx.caller().ownership(None, Some(&installed)),
        ) {
            Ok(installed) => installed,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to store updated binary: {}", e),
                    duration(),
                ))
            }
        };
        let new_binary_version = new_installed.binary_version;

        let tool_names = match activate_stored_skill(
            &self.ctx.hosts,
            &self.ctx.engine,
            Some(old_binary_version),
            &manifest,
            new_binary_version,
            &wasm_bytes,
            &preserved_config,
        )
        .await
        {
            Ok(names) => names,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };

        info!(
            skill = %params.skill,
            from = %current_version,
            to = %manifest.version,
            "WASM skill updated"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "skill": params.skill,
                "updated": true,
                "from_version": current_version,
                "to_version": manifest.version,
                "tools_loaded": tool_names.len(),
                "tool_names": tool_names,
            }),
            duration(),
        )
        .loading(tool_names))
    }
}

// ─── wasm_skill.build ───────────────────────────────────────────────────────

/// Build a WASM skill from a local Rust project directory.
pub struct WasmSkillBuildTool {
    ctx: WasmSkillContext,
}

impl WasmSkillBuildTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct BuildInput {
    /// Path to the skill project directory.
    project_path: String,
    /// If true, install the skill after building.
    #[serde(default)]
    install: bool,
    /// Optional config values (used if install=true).
    #[serde(default)]
    config: HashMap<String, String>,
    /// Who may use the skill once installed (used if install=true).
    #[serde(default)]
    visibility: Option<String>,
}

#[async_trait]
impl Tool for WasmSkillBuildTool {
    fn name(&self) -> &str {
        "wasm_skill.build"
    }

    fn description(&self) -> &str {
        static DESC: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!(
                "Build a WASM skill from a local Rust project directory. Optionally install \
                 it immediately. Rebuilding invalidates every user credential grant until \
                 the user re-runs `cmd: skill grant`. {SANDBOX_CONTRACT}"
            )
        });
        &DESC
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project_path": {
                    "type": "string",
                    "description": "Path to the skill project directory (must contain Skill.toml and Cargo.toml)"
                },
                "install": {
                    "type": "boolean",
                    "description": "If true, install the skill after building (default: false)"
                },
                "config": {
                    "type": "object",
                    "description": "Optional config values for the skill (used if install=true)",
                    "additionalProperties": { "type": "string" }
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "everyone"],
                    "description": "Who may call the skill once installed (used if install=true): 'private' (default) is you plus anyone you share it with; 'team' is anyone in a team with you; 'everyone' publishes it to all users."
                }
            },
            "required": ["project_path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: BuildInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if let Err(refusal) = self.ctx.allow_host_compile() {
            return Ok(ToolResult::failure(refusal, duration()));
        }

        let project_dir = match self.ctx.resolve_path(&params.project_path) {
            Ok(dir) => dir,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };
        let project_dir = project_dir.as_path();

        // Validate project structure
        if !project_dir.join("Skill.toml").is_file() {
            return Ok(ToolResult::failure(
                format!("No Skill.toml found in {}", project_dir.display()),
                duration(),
            ));
        }
        if !project_dir.join("Cargo.toml").is_file() {
            return Ok(ToolResult::failure(
                format!("No Cargo.toml found in {}", project_dir.display()),
                duration(),
            ));
        }

        let manifest = match SkillManifest::from_file(&project_dir.join("Skill.toml")) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to parse Skill.toml: {}", e),
                    duration(),
                ))
            }
        };

        if let Err(e) = manifest.validate() {
            return Ok(ToolResult::failure(
                format!("Invalid Skill.toml: {}", e),
                duration(),
            ));
        }

        // Build the .wasm — pass the vault GitHub token so cargo's
        // transitive git fetch (e.g. workforce-skill-sdk on a private
        // repo) authenticates. Owner unknown (the project's git deps
        // may span any number of orgs); resolve via the bare flat path.
        let token = self.ctx.resolve_github_token(None);
        let wasm_bytes = match build_skill_project(
            project_dir,
            token.as_deref(),
            self.ctx.runner.as_ref(),
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Build failed: {}", e),
                    duration(),
                ))
            }
        };

        let wasm_size = wasm_bytes.len();
        let skill_name = manifest.name.clone();
        let skill_version = manifest.version.clone();

        if params.install {
            let wasm_output = match find_wasm_output(project_dir, &skill_name) {
                Some(path) => path,
                None => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Build succeeded but could not locate .wasm binary in {}",
                            project_dir.display()
                        ),
                        duration(),
                    ))
                }
            };
            let install_input = serde_json::json!({
                "wasm_path": wasm_output,
                "config": params.config,
                "visibility": params.visibility,
            });
            let previous = self.ctx.store.get_installed(&skill_name);
            let install_tool = WasmSkillInstallTool::new(self.ctx.clone());
            let mut result = install_tool.execute(install_input).await?;
            record_source_snapshot(
                &self.ctx.store,
                &skill_name,
                project_dir,
                self.ctx.workspace_root(),
                &mut result,
            );
            note_unbumped_rebuild(previous.as_ref(), &self.ctx.store, &skill_name, &mut result);
            return Ok(result);
        }

        let mut result = ToolResult::success(
            serde_json::json!({
                "skill": skill_name,
                "version": skill_version,
                "built": true,
                "wasm_size_bytes": wasm_size,
                "output": find_wasm_output(project_dir, &skill_name),
            }),
            duration(),
        );
        note_project_location(project_dir, self.ctx.workspace_root(), false, &mut result);
        Ok(result)
    }
}

// ─── wasm_skill.scaffold ────────────────────────────────────────────────────

/// Scaffold a new WASM skill project from a template.
pub struct WasmSkillScaffoldTool {
    ctx: WasmSkillContext,
}

impl WasmSkillScaffoldTool {
    pub fn new(ctx: WasmSkillContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ScaffoldInput {
    /// Skill name (kebab-case).
    name: String,
    /// Human-readable description.
    description: String,
    /// Directory to create the project in, relative to the workspace.
    #[serde(default)]
    output_dir: Option<String>,
    /// Capabilities the skill needs.
    #[serde(default)]
    capabilities: Vec<String>,
    /// Tool names to scaffold.
    #[serde(default)]
    tools: Vec<ScaffoldToolDef>,
}

#[derive(Deserialize)]
struct ScaffoldToolDef {
    name: String,
    description: String,
}

#[async_trait]
impl Tool for WasmSkillScaffoldTool {
    fn name(&self) -> &str {
        "wasm_skill.scaffold"
    }

    fn description(&self) -> &str {
        static DESC: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!(
                "Create a new WASM skill project from a template with Skill.toml, \
                 Cargo.toml, and src/lib.rs. {SANDBOX_CONTRACT}"
            )
        });
        &DESC
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name in kebab-case (e.g., 'weather-lookup')"
                },
                "description": {
                    "type": "string",
                    "description": "Human-readable description of the skill"
                },
                "output_dir": {
                    "type": "string",
                    "description": "Directory to create the project in, relative to your workspace (default: wasm-skills)"
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Capabilities needed: http, vault, config, artifacts, memory, nats"
                },
                "tools": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["name", "description"]
                    },
                    "description": "Tools to scaffold in the skill"
                }
            },
            "required": ["name", "description"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ScaffoldInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Validate name
        if params.name.is_empty()
            || !params
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Ok(ToolResult::failure(
                "Skill name must be kebab-case (lowercase letters, digits, hyphens)".to_string(),
                duration(),
            ));
        }

        let base_dir = match self
            .ctx
            .resolve_path(params.output_dir.as_deref().unwrap_or(DEFAULT_SCAFFOLD_DIR))
        {
            Ok(dir) => dir,
            Err(e) => return Ok(ToolResult::failure(e, duration())),
        };
        let project_dir = base_dir.join(&params.name);
        if self.ctx.reaches_config(&project_dir) {
            return Ok(ToolResult::failure(
                workforce_common::config_files::WRITE_REFUSED,
                duration(),
            ));
        }

        if project_dir.exists() {
            return Ok(ToolResult::failure(
                format!("Directory already exists: {}", project_dir.display()),
                duration(),
            ));
        }

        let roots = CanonicalRoots::new(&self.ctx.path_roots);
        let src_dir = project_dir.join("src");

        // Default tool if none provided
        let tools = if params.tools.is_empty() {
            vec![ScaffoldToolDef {
                name: "run".to_string(),
                description: format!("Run the {} skill", params.name),
            }]
        } else {
            params.tools
        };

        // Generate Skill.toml
        let caps_toml = if params.capabilities.is_empty() {
            String::new()
        } else {
            let mut caps = String::from("\n[capabilities]\n");
            for cap in &params.capabilities {
                caps.push_str(&format!("{} = true\n", cap));
            }
            caps
        };

        let tools_toml: String = tools
            .iter()
            .map(|t| {
                format!(
                    r#"
[[tools]]
name = "{}"
description = "{}"
input_schema = {{ type = "object", properties = {{}} }}
"#,
                    t.name, t.description
                )
            })
            .collect();

        let skill_toml = format!(
            r#"[skill]
name = "{name}"
version = "0.1.0"
description = "{description}"
min_workforce_version = "0.1.0"
{caps}
# Hosts this skill may reach via http::fetch. EVERY other host is refused at
# runtime, so list each API host here (hostname only, `*.example.com` wildcards
# allowed). Requires `http = true` under [capabilities].
# [network]
# allowed_hosts = ["api.example.com"]

# Secrets this skill needs. A declared credential is provisioned by the USER —
# they type `cmd: secret <name> <value>` then `cmd: skill grant {name} <name>`
# in chat — and read in skill code with vault::get("<name>"). Skill code cannot
# write it. Grants pin this file's consent contract (capabilities, network
# hosts, credentials, permissions): editing those invalidates them until the
# user re-approves, while pure code rebuilds keep them.
# scope = "user" gives each caller their own value; "shared" (admin-granted)
# serves every caller. Requires `vault = true` under [capabilities].
# [[credentials]]
# name = "api_key"
# description = "What this credential is for"
# scope = "user"

# Platform tools this skill may invoke with tools::invoke (exact names, no
# wildcards; requires `tools = true` under [capabilities]). The USER approves
# the whole list with `cmd: skill grant {name} tools`; every call also passes
# the invoking user's own capability limits. Admin/vault/exec families and
# other skills are never grantable.
# [[permissions]]
# tool = "slack.post_message"
# description = "Why this skill needs it"

# A tool that must never run unattended adds, beside its name/description:
#   requires_approval = true
#   approval_summary = "apply `{{{{action}}}}` to {{{{recipients|count}}}} recipient(s)"
# The host renders the summary from the call's own arguments — `{{{{field}}}}`
# interpolates, `{{{{field|count}}}}` counts — into the prompt the approver reads.
# The gate only ever ADDS a prompt, and a gated tool cannot run as a workflow
# tool step or a job, where there is nobody to ask.
{tools}"#,
            name = params.name,
            description = params.description,
            caps = caps_toml,
            tools = tools_toml,
        );

        put(&roots, project_dir.join("Skill.toml"), skill_toml)?;

        // Generate Cargo.toml
        // Convert skill name to a valid crate name (hyphens to underscores for lib name)
        let crate_name = params.name.replace('-', "_");
        let cargo_toml = format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

# Prevent cargo from absorbing this project into a parent workspace.
[workspace]

[lib]
name = "{crate_name}"
crate-type = ["cdylib"]

[dependencies]
workforce-skill-sdk = "0.1"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"

[profile.release]
opt-level = "s"
lto = true
strip = true
"#,
            name = params.name,
            crate_name = crate_name,
        );

        put(&roots, project_dir.join("Cargo.toml"), cargo_toml)?;

        // Generate .cargo/config.toml for convenient `cargo build --release`
        let cargo_config_dir = project_dir.join(".cargo");
        put(
            &roots,
            cargo_config_dir.join("config.toml"),
            "[build]\ntarget = \"wasm32-unknown-unknown\"\n",
        )?;

        // Generate src/lib.rs
        let tool_handlers: String = tools
            .iter()
            .map(|t| {
                format!(
                    r#"
workforce_skill_sdk::tool!({name}, |input: ToolInput| {{
    // TODO: Implement {name}
    log::info(&format!("{name} called with: {{}}", input.raw()));
    ToolOutput::success(json!({{
        "status": "ok",
        "message": "{desc}"
    }}))
}});
"#,
                    name = t.name,
                    desc = t.description,
                )
            })
            .collect();

        let lib_rs = format!(
            r#"use workforce_skill_sdk::prelude::*;

workforce_skill_sdk::init!();

// The guest API (each family needs its capability in Skill.toml):
//   vault::get("api_key")            — this skill's own locker; the USER
//                                      provisions declared credentials via
//                                      `cmd: secret` + `cmd: skill grant`.
//   http::fetch("POST", url, &[("content-type", "application/json")], Some(body))
//                                    — hosts from [network] allowed_hosts only.
//   tools::invoke("slack.post_message", &json!({{"channel": c, "text": t}}))
//                                    — tools from [[permissions]], after the
//                                      user runs `cmd: skill grant <skill> tools`;
//                                      429s are retried host-side with backoff.
//   tools::invoke_many(&[("a.b", json!({{..}})), ("c.d", json!({{..}}))])
//                                    — fan-out with bounded per-service
//                                      concurrency, one envelope each.
//   log::info(...), config::get("units")
{handlers}"#,
            handlers = tool_handlers,
        );

        put(&roots, src_dir.join("lib.rs"), lib_rs)?;

        put(
            &roots,
            project_dir.join("README.md"),
            format!(
                "# {name}\n\n{description}\n\nA workforce WASM skill. The complete \
                 authoring contract — Skill.toml schema, guest API, sandbox limits, and \
                 the credential/tool consent flow — is served by the `wasm_skill.guide` \
                 tool; read it before editing. Build and install with `wasm_skill.build` \
                 (project_path, install=true); smoke-test without grants via \
                 `wasm_skill.test`. Grants pin the skill's consent contract \
                 (capabilities, network hosts, credentials, permissions): editing those \
                 needs a re-run of `cmd: skill grant {name} …`, while pure code rebuilds \
                 keep grants valid.\n\nThe skill belongs to whoever installs it and starts \
                 private. Share it with `cmd: skill share {name} <@user>` (add `manage` to \
                 let them edit it) or publish it to everyone with \
                 `cmd: skill visibility {name} everyone`.\n",
                name = params.name,
                description = params.description,
            ),
        )?;

        // Generate .gitignore
        put(
            &roots,
            project_dir.join(".gitignore"),
            "/target\nCargo.lock\n",
        )?;

        // Generate GitHub Actions CI workflow for auto-building releases
        let ci_dir = project_dir.join(".github/workflows");
        let ci_workflow = format!(
            r#"name: Release

on:
  push:
    tags:
      - "v*"

permissions:
  contents: write

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-unknown-unknown

      - name: Build WASM
        run: cargo build --release --target wasm32-unknown-unknown

      - name: Upload to Release
        uses: softprops/action-gh-release@v2
        with:
          files: |
            target/wasm32-unknown-unknown/release/{crate_name}.wasm
            Skill.toml
"#,
            crate_name = crate_name,
        );
        put(&roots, ci_dir.join("release.yml"), ci_workflow)?;

        info!(
            skill = %params.name,
            tools = tools.len(),
            path = %project_dir.display(),
            "WASM skill project scaffolded"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "skill": params.name,
                "scaffolded": true,
                "path": project_dir.display().to_string(),
                "files": [
                    "Skill.toml",
                    "Cargo.toml",
                    ".cargo/config.toml",
                    "src/lib.rs",
                    ".gitignore",
                    ".github/workflows/release.yml"
                ],
                "next_steps": format!(
                    "1. cd {path}\n\
                     2. Edit src/lib.rs to implement your tools\n\
                     3. cargo build --release  (builds to wasm32 via .cargo/config.toml)\n\
                     4. Use wasm_skill.install to install\n\
                     5. git init && git remote add origin <repo-url> && git push to publish",
                    path = project_dir.display()
                ),
            }),
            duration(),
        ))
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Dependency names a project pulls straight from git, across every dependency
/// table a manifest can declare them in (`[dependencies]`, the dev/build
/// variants, and the per-target forms). A sandboxed build has no credentials to
/// fetch them with, so this is what turns that into one clear refusal.
fn git_dependencies(cargo_toml: &str) -> Vec<String> {
    fn collect(value: &toml::Value, out: &mut Vec<String>) {
        let Some(table) = value.as_table() else {
            return;
        };
        for (key, entry) in table {
            match key.ends_with("dependencies") {
                true => out.extend(
                    entry
                        .as_table()
                        .into_iter()
                        .flatten()
                        .filter(|(_, spec)| spec.get("git").is_some())
                        .map(|(name, _)| name.clone()),
                ),
                false => collect(entry, out),
            }
        }
    }
    let Ok(parsed) = cargo_toml.parse::<toml::Value>() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    collect(&parsed, &mut names);
    names.sort();
    names.dedup();
    names
}

/// Largest slice of cargo stderr propagated on a failed build. The tail is
/// where cargo puts the errors; an unbounded dump can reach megabytes.
const MAX_BUILD_STDERR_BYTES: usize = 16 * 1024;

/// The last `max` bytes of `stderr`, cut on a char boundary, with a marker
/// when anything was dropped.
fn stderr_tail(stderr: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if stderr.len() <= max {
        return std::borrow::Cow::Borrowed(stderr);
    }
    let mut start = stderr.len() - max;
    while !stderr.is_char_boundary(start) {
        start += 1;
    }
    std::borrow::Cow::Owned(format!(
        "… (stderr truncated to the last {max} bytes)\n{}",
        &stderr[start..]
    ))
}

/// Build a skill project using `cargo build --target wasm32-unknown-unknown --release`.
///
/// `runner = Some` routes the build through the privilege-separated
/// daemon so it runs as the unprivileged `agent` user (audit F7): the skill's
/// `build.rs`/proc-macros can no longer read the hub user's vault. `None` builds
/// in-process (dev/local), unchanged.
///
/// AMBIENT-CREDENTIAL CAVEAT: when `token` is set we authenticate cargo's git
/// fetcher with the PAT via a per-build `GIT_CONFIG_GLOBAL` rewrite rule
/// (git ≥ 2.32) + `CARGO_NET_GIT_FETCH_WITH_CLI=true`. That token is passed via
/// `cmd.env`, which the executor's controlled (scrubbed-by-design) environment
/// does NOT carry and the wire `ExecRequest` has no env field to convey. So a
/// dispatched build cannot fetch a git dependency at all — rather than let that
/// surface as an opaque cargo authentication failure, the project is refused up
/// front with the remedy ([`git_dependencies`]). Registry dependencies, which
/// is what the scaffold generates now that `workforce-skill-sdk` is published,
/// are unaffected. The `None` (dev) arm keeps the token passthrough verbatim.
async fn build_skill_project(
    project_dir: &Path,
    token: Option<&str>,
    runner: Option<&RunnerEndpoint>,
) -> eyre::Result<Vec<u8>> {
    // Force the build output to land under `project_dir/target/`. Without
    // `--target-dir` cargo inherits the workspace root's target dir when
    // the skill repo is a Cargo workspace (mono-repo layout), so the
    // build output ends up at `repo/target/...` and our fixed lookup
    // under `project_dir/target/...` finds nothing.
    if runner.is_some() {
        let cargo_toml = std::fs::read_to_string(project_dir.join("Cargo.toml"))
            .map_err(|e| eyre::eyre!("Failed to read Cargo.toml: {e}"))?;
        let git_deps = git_dependencies(&cargo_toml);
        if !git_deps.is_empty() {
            eyre::bail!(
                "this hub builds skills in a sandbox that has no git credentials, so the git \
                 dependenc{} {} cannot be fetched. Depend on published crates instead — \
                 `workforce-skill-sdk = \"0.1\"` is on crates.io.",
                if git_deps.len() == 1 { "y" } else { "ies" },
                git_deps.join(", ")
            );
        }
    }

    let target_dir_root = project_dir.join("target");
    let target_dir_str = target_dir_root.display().to_string();
    let args = [
        "build",
        "--target",
        "wasm32-unknown-unknown",
        "--release",
        "--target-dir",
        &target_dir_str,
    ];

    let (stderr, success) = match runner {
        // Intentional forward-compat seam: serve.rs hardwires `runner:
        // None` for wasm_skill builds today (the per-build private-dep token
        // rides `GIT_CONFIG_GLOBAL`, which the secret-free runner scrubs), so
        // this arm is currently unreachable in prod. It is kept — not deleted —
        // so that once the protocol gains a per-request scoped-credential
        // channel, wiring the socket in is a one-line config change with the
        // dispatch path already proven. Privilege-separated build as the agent
        // user; the per-build token env cannot cross the wire protocol yet.
        Some(endpoint) => {
            let outcome = crate::code::utils::run_command(
                Some(endpoint),
                "cargo",
                &args,
                project_dir,
                crate::code::utils::CODE_TOOL_TIMEOUT_SECS,
            )
            .await?;
            let success = outcome.success();
            (outcome.stderr, success)
        }
        // In-process (dev/local): keep the token git-config passthrough verbatim.
        None => {
            let mut cmd = tokio::process::Command::new("cargo");
            cmd.args(args).current_dir(project_dir);

            let _git_config_guard = if let Some(t) = token {
                use std::io::Write;
                let mut tmp = tempfile::Builder::new()
                    .prefix("wf-gitconfig-")
                    .suffix(".cfg")
                    .tempfile()?;
                writeln!(
                    tmp,
                    "[url \"https://x-access-token:{t}@github.com/\"]\n    insteadOf = https://github.com/"
                )?;
                cmd.env("GIT_CONFIG_GLOBAL", tmp.path());
                cmd.env("CARGO_NET_GIT_FETCH_WITH_CLI", "true");
                Some(tmp)
            } else {
                None
            };

            let output = cmd
                .output()
                .await
                .map_err(|e| eyre::eyre!("Failed to run cargo build: {}", e))?;
            (
                String::from_utf8_lossy(&output.stderr).into_owned(),
                output.status.success(),
            )
        }
    };

    if !success {
        let scrubbed = match token {
            Some(t) => stderr.replace(t, "***"),
            None => stderr,
        };
        let tail = stderr_tail(&scrubbed, MAX_BUILD_STDERR_BYTES);
        tracing::warn!(
            project = %project_dir.display(),
            "wasm skill cargo build failed:\n{tail}"
        );
        return Err(eyre::eyre!("cargo build failed:\n{}", tail));
    }

    let release_dir = target_dir_root.join("wasm32-unknown-unknown/release");
    let wasm_file = find_wasm_in_dir(&release_dir)?;

    // Keep the artifact's sibling Skill.toml current: a `wasm_path` install
    // reads the manifest BESIDE the .wasm, and a stale copy there silently
    // installs an outdated contract (observed live: a [[credentials]] edit
    // that never reached the installed build).
    //
    // The build just ran the project's own code as the agent uid, which can
    // leave links in either place; the hub reads and writes neither through
    // one.
    let roots = workforce_agent::tools::paths::CanonicalRoots::new(&[project_dir.to_path_buf()]);
    let refreshed =
        crate::code::refactor::apply::open_regular(&project_dir.join("Skill.toml"), false)
            .map_err(workforce_agent::tools::config_guard::WriteError::Io)
            .and_then(|mut manifest| {
                let target = roots
                    .resolve(&release_dir.join("Skill.toml"))
                    .ok_or_else(|| {
                        workforce_agent::tools::config_guard::WriteError::Io(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "the release directory leads outside the project",
                        ))
                    })?;
                workforce_agent::tools::config_guard::write_from(&roots, &target, &mut manifest)
            });
    refreshed.map_err(|e| {
        eyre::eyre!(
            "Built OK but failed to refresh Skill.toml beside the artifact in {}: {}",
            release_dir.display(),
            e
        )
    })?;

    std::fs::read(&wasm_file).map_err(|e| {
        eyre::eyre!(
            "Failed to read built WASM at {}: {}",
            wasm_file.display(),
            e
        )
    })
}

/// Find the .wasm file in a target directory.
fn find_wasm_in_dir(dir: &Path) -> eyre::Result<PathBuf> {
    if !dir.is_dir() {
        return Err(eyre::eyre!(
            "Build output directory not found: {}",
            dir.display()
        ));
    }

    let mut wasm_files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "wasm").unwrap_or(false))
        .collect();

    // Prefer files that don't contain "deps" in the name
    wasm_files.sort_by_key(|p| {
        if p.to_string_lossy().contains("deps") {
            1
        } else {
            0
        }
    });

    wasm_files
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("No .wasm file found in {}", dir.display()))
}

/// Find the expected WASM output path for a skill project.
fn find_wasm_output(project_dir: &Path, skill_name: &str) -> Option<String> {
    let crate_name = skill_name.replace('-', "_");
    let path = project_dir
        .join("target/wasm32-unknown-unknown/release")
        .join(format!("{}.wasm", crate_name));
    if path.is_file() {
        Some(path.display().to_string())
    } else {
        None
    }
}

/// Unload all tools belonging to a WASM skill from the registry.
fn unload_wasm_skill(
    skill_name: &str,
    registry: &ToolRegistry,
    skill_state: &SkillStateTracker,
) -> Vec<String> {
    let prefix = format!("{}.", skill_name);
    let all_names = registry.names();
    let to_remove: Vec<String> = all_names
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect();
    for name in &to_remove {
        registry.unregister(name);
    }
    skill_state.mark_unloaded(skill_name);
    to_remove
}

/// Fetch a WASM skill from a GitHub repository. Tries the Releases API
/// first (release artifact `.wasm`), then falls back to `git clone` and
/// `cargo build`. With a `#subpath=` set on `repo_ref`, only the
/// clone-and-build path is used.
///
/// `allow_build` is the caller's classification
/// ([`crate::wasm_skill_access::install_source`]): a digest-pinned install of a
/// release asset never compiles, so the fallback is refused rather than run
/// under a capability decision that priced the asset path.
///
/// `token`, when set, authenticates both the Releases API call (as
/// `Authorization: Bearer ...`) and the `git clone` (via
/// `https://x-access-token:{token}@github.com/...`). When `None`,
/// fetches are anonymous — works for public repos but is rate-limited
/// to 60 requests/hour on the API side.
/// A skill obtained from GitHub: its manifest, binary, and — when it was
/// compiled from a clone — the commit that was built.
pub(crate) struct FetchedSkill {
    pub manifest: SkillManifest,
    pub wasm_bytes: Vec<u8>,
    pub commit: Option<String>,
}

pub(crate) async fn fetch_from_github(
    repo_ref: &str,
    token: Option<&str>,
    runner: Option<&RunnerEndpoint>,
    allow_build: bool,
) -> eyre::Result<FetchedSkill> {
    let (repo, tag, subpath) = parse_github_repo_ref(repo_ref);
    if let Some(p) = subpath {
        workforce_skills::installer::validate_subpath(p)?;
    }

    // A commit-sha ref is a source pin: release assets attach to tags, so
    // skip straight to clone-and-build and verify the checkout below.
    let commit_pin = tag.filter(|t| is_commit_sha(t));

    // Strategy 1 (release artifact) only handles repo-root manifests
    // today; subpath releases fall through to clone-and-build.
    if subpath.is_none() && commit_pin.is_none() {
        let tag_path = tag.unwrap_or("latest");
        let release_url = if tag_path == "latest" {
            format!("https://api.github.com/repos/{}/releases/latest", repo)
        } else {
            format!(
                "https://api.github.com/repos/{}/releases/tags/{}",
                repo, tag_path
            )
        };

        let client = reqwest::Client::new();
        let mut release_req = client
            .get(&release_url)
            .header("User-Agent", "workforce-wasm-runtime")
            .header("Accept", "application/vnd.github.v3+json");
        if let Some(t) = token {
            release_req = release_req.header("Authorization", format!("Bearer {t}"));
        }

        if let Ok(resp) = release_req.send().await {
            if resp.status().is_success() {
                let release: serde_json::Value = resp.json().await?;
                if let Some(assets) = release["assets"].as_array() {
                    for asset in assets {
                        let asset_name = asset["name"].as_str().unwrap_or("");
                        if asset_name.ends_with(".wasm") {
                            let download_url = asset["browser_download_url"]
                                .as_str()
                                .ok_or_else(|| eyre::eyre!("No download URL for asset"))?;
                            let mut dl_req = client
                                .get(download_url)
                                .header("User-Agent", "workforce-wasm-runtime");
                            if let Some(t) = token {
                                dl_req = dl_req.header("Authorization", format!("Bearer {t}"));
                            }
                            let wasm_bytes = dl_req.send().await?.bytes().await?.to_vec();
                            let manifest = fetch_manifest_from_github(repo, tag, token).await?;
                            return Ok(FetchedSkill {
                                manifest,
                                wasm_bytes,
                                commit: None,
                            });
                        }
                    }
                }
            }
        }
    }

    if !allow_build {
        eyre::bail!(
            "no published .wasm release asset found for {repo_ref}, and this install is \
             digest-pinned to one. Publish a release asset, or install from source with an \
             exact commit-sha ref (`owner/repo@<40-hex-commit-sha>`)."
        );
    }

    // Strategy 2: Clone and build from source. With a subpath, we treat
    // the cloned `clone_dir/{subpath}/` as the project dir; without one,
    // the clone root. When a token is set, embed it in the clone URL so
    // private repos resolve.
    let tmp_dir = tempfile::tempdir()?;
    let clone_dir = tmp_dir.path().join("repo");
    let clone_dir_str = clone_dir
        .to_str()
        .ok_or_else(|| eyre::eyre!("Temporary path contains invalid UTF-8"))?;

    let clone_url = match token {
        Some(t) => format!("https://x-access-token:{t}@github.com/{repo}.git"),
        None => format!("https://github.com/{repo}.git"),
    };
    // A commit pin needs history (`--branch` only takes branch/tag names), so
    // clone full and check the exact commit out; otherwise shallow-clone the
    // named ref as before.
    let mut clone_args = match commit_pin {
        Some(_) => vec!["clone"],
        None => {
            let mut args = vec!["clone", "--depth", "1"];
            if let Some(t) = tag {
                args.extend(["--branch", t]);
            }
            args
        }
    };
    clone_args.push(&clone_url);
    clone_args.push(clone_dir_str);

    // Redact the token from any stderr that echoes the clone URL before
    // surfacing the error to the agent / logs.
    let scrub = |stderr: &[u8]| {
        let text = String::from_utf8_lossy(stderr).into_owned();
        match token {
            Some(t) => text.replace(t, "***"),
            None => text,
        }
    };

    let clone_output = hardened_git()
        .args(&clone_args)
        .output()
        .await
        .map_err(|e| eyre::eyre!("Failed to run git clone: {}", e))?;

    if !clone_output.status.success() {
        return Err(workforce_skills::installer::CloneFailed(
            scrub(&clone_output.stderr).trim().to_string(),
        )
        .into());
    }

    if let Some(sha) = commit_pin {
        let checkout = hardened_git()
            .args(["-C", clone_dir_str, "checkout", "--detach", sha])
            .output()
            .await
            .map_err(|e| eyre::eyre!("Failed to run git checkout: {}", e))?;
        if !checkout.status.success() {
            return Err(eyre::eyre!(
                "git checkout of pinned commit {} failed: {}",
                sha,
                scrub(&checkout.stderr).trim()
            ));
        }
    }
    // The pin is only worth anything if the working tree provably IS that
    // commit — verify rather than trust the checkout's exit code; the same
    // answer is the provenance recorded for an unpinned (branch) ref.
    let head = hardened_git()
        .args(["-C", clone_dir_str, "rev-parse", "HEAD"])
        .output()
        .await
        .map_err(|e| eyre::eyre!("Failed to run git rev-parse: {}", e))?;
    let head_sha = String::from_utf8_lossy(&head.stdout).trim().to_lowercase();
    if let Some(sha) = commit_pin.filter(|sha| head_sha != sha.to_lowercase()) {
        return Err(eyre::eyre!(
            "pinned commit mismatch: requested {}, checkout produced {}",
            sha,
            head_sha
        ));
    }

    let project_dir = match subpath {
        Some(p) => clone_dir.join(p),
        None => clone_dir.clone(),
    };
    if !project_dir.is_dir() {
        return Err(eyre::eyre!(
            "Subpath '{}' not found in cloned repository",
            subpath.unwrap_or("")
        ));
    }
    if crate::system_tool::holds_external_skill(&project_dir) {
        eyre::bail!(crate::system_tool::external_skill_found(
            "github_repo",
            repo_ref
        ));
    }
    let manifest = SkillManifest::from_file(&project_dir.join("Skill.toml"))?;
    let wasm_bytes = build_skill_project(&project_dir, token, runner).await?;

    Ok(FetchedSkill {
        manifest,
        wasm_bytes,
        commit: Some(head_sha),
    })
}

/// Fetch Skill.toml from a GitHub repo (raw content). Authenticates with
/// `token` when provided so private repos resolve.
async fn fetch_manifest_from_github(
    repo: &str,
    tag: Option<&str>,
    token: Option<&str>,
) -> eyre::Result<SkillManifest> {
    let branch = tag.unwrap_or("main");
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/Skill.toml",
        repo, branch
    );

    let mut req = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "workforce-wasm-runtime");
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;

    if !resp.status().is_success() {
        return Err(eyre::eyre!(
            "Failed to fetch Skill.toml from {}: HTTP {}",
            url,
            resp.status()
        ));
    }

    let toml_str = resp.text().await?;
    SkillManifest::from_toml(&toml_str)
}

/// Fetch the latest release tag from a GitHub repository.
async fn fetch_latest_github_tag(repo: &str) -> eyre::Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);

    let resp = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "workforce-wasm-runtime")
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(eyre::eyre!(
            "Failed to fetch releases for {}: HTTP {}",
            repo,
            resp.status()
        ));
    }

    let release: serde_json::Value = resp.json().await?;
    release["tag_name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("No tag_name in latest release for {}", repo))
}

/// Load all previously installed WASM skills from the store at startup.
pub async fn load_installed_wasm_skills(
    engine: &WasmSkillEngine,
    store: &WasmSkillStore,
    registry: &ToolRegistry,
    skill_state: &SkillStateTracker,
    locker: Option<Arc<dyn workforce_common::settings::SecretStore>>,
    invoker: workforce_wasm_runtime::types::SkillInvokerSlot,
) {
    let engine_arc = Arc::new(engine.clone());
    for installed in store.list_installed() {
        let name = &installed.manifest.name;
        let version = installed.binary_version;

        if installed.state != WasmSkillState::Active {
            tracing::debug!(skill = %name, state = ?installed.state, "Skipping non-active WASM skill");
            continue;
        }

        // Load binary from disk and compile
        match store.load_binary(name, version) {
            Ok(wasm_bytes) => {
                if let Err(e) = engine.load_module(name, version, &wasm_bytes).await {
                    tracing::error!(skill = %name, error = %e, "Failed to compile WASM skill at startup");
                    continue;
                }
            }
            Err(e) => {
                tracing::error!(skill = %name, error = %e, "Failed to load WASM binary at startup");
                continue;
            }
        }

        // Register tools
        let tools = WasmSkillTool::from_manifest(
            &installed.manifest,
            Arc::clone(&engine_arc),
            version,
            installed.config.clone(),
            locker.clone(),
            workforce_wasm_runtime::SkillRuntimeBindings::new(
                Arc::clone(&invoker),
                skill_use_gate(),
            ),
        );
        let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        for tool in &tools {
            registry.register(Arc::clone(tool));
        }

        skill_state.mark_loaded(
            name,
            tool_names.clone(),
            "wasm",
            Some(format!("v{}", installed.manifest.version)),
        );

        info!(
            skill = %name,
            version = %installed.manifest.version,
            tools = tool_names.len(),
            "WASM skill loaded at startup"
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hub_core::skill_builds::github_ref_builds_from_source;

    struct MapVault(parking_lot::Mutex<HashMap<String, String>>);

    impl MapVault {
        fn new() -> Arc<Self> {
            Arc::new(Self(parking_lot::Mutex::new(HashMap::new())))
        }
    }

    /// A context rooted at a temp workspace: the hub wiring, minus the agent.
    pub(crate) fn test_ctx(workspace: &Path, store_dir: &Path) -> WasmSkillContext {
        WasmSkillContext {
            engine: Arc::new(WasmSkillEngine::new().expect("engine")),
            store: Arc::new(
                workforce_wasm_runtime::WasmSkillStore::open(store_dir).expect("store"),
            ),
            tools: Arc::new(ToolRegistry::new()),
            skill_state: Arc::new(SkillStateTracker::new()),
            hosts: SkillHosts::default(),
            data_dir: workspace.to_path_buf(),
            policy: WasmInstallPolicy::default(),
            agent_vault: None,
            global_vault: None,
            runner: None,
            path_roots: vec![workspace.canonicalize().expect("canonical workspace")],
            admin: crate::authz::always_admin(),
            teams: Arc::new(parking_lot::RwLock::new(None)),
            catalogs: Vec::new(),
        }
    }

    /// Install a minimal skill so snapshots have a binary to bind to.
    fn install_test_skill(ctx: &WasmSkillContext, name: &str) {
        let manifest = SkillManifest::from_toml(&format!(
            "[skill]\nname = \"{name}\"\nversion = \"1.0.0\"\n\
             description = \"test\"\nauthor = \"test\"\nlicense = \"MIT\"\n\
             [[tools]]\nname = \"ping\"\ndescription = \"pong\"\n\
             input_schema = {{ type = \"object\" }}\n"
        ))
        .expect("manifest parses");
        ctx.store
            .install_prebuilt(
                manifest,
                b"fake-wasm",
                None,
                "test".into(),
                HashMap::new(),
                workforce_wasm_runtime::SkillOwnership::operator(),
            )
            .expect("installs");
    }

    fn scaffold_input(name: &str, output_dir: Option<&str>) -> serde_json::Value {
        let mut input = serde_json::json!({
            "name": name,
            "description": "A test skill",
        });
        if let Some(dir) = output_dir {
            input["output_dir"] = serde_json::json!(dir);
        }
        input
    }

    #[test]
    fn unbumped_rebuild_note_fires_only_on_same_version_with_new_bytes() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(store_dir.path()).unwrap();
        let manifest = |version: &str| {
            SkillManifest::from_toml(&format!(
                "[skill]\nname = \"demo\"\nversion = \"{version}\"\ndescription = \"d\"\n\
                 author = \"t\"\nlicense = \"MIT\"\n[[tools]]\nname = \"ping\"\n\
                 description = \"pong\"\ninput_schema = {{ type = \"object\" }}\n"
            ))
            .unwrap()
        };
        let install = |version: &str, bytes: &[u8]| {
            store
                .install_prebuilt(
                    manifest(version),
                    bytes,
                    None,
                    "test".into(),
                    HashMap::new(),
                    workforce_wasm_runtime::SkillOwnership::operator(),
                )
                .unwrap()
        };
        let first = install("1.0.0", b"a");
        assert!(unbumped_rebuild_note(None, &first).is_none());
        let same = install("1.0.0", b"a");
        assert!(unbumped_rebuild_note(Some(&first), &same).is_none());
        let bumped = install("1.1.0", b"b");
        assert!(unbumped_rebuild_note(Some(&same), &bumped).is_none());
        let drifted = install("1.1.0", b"c");
        let note = unbumped_rebuild_note(Some(&bumped), &drifted).unwrap();
        assert!(note.contains("'demo' at version 1.1.0"), "{note}");
        assert!(note.contains("wasm_skill.publish"));
    }

    /// The published SDK's prelude carries no macros, so the template must
    /// name them by path or the very first build fails.
    #[tokio::test]
    async fn scaffold_names_sdk_macros_by_path() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let tool = WasmSkillScaffoldTool::new(test_ctx(workspace.path(), store.path()));
        let result = tool
            .execute(scaffold_input("macro-skill", None))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let lib_rs =
            std::fs::read_to_string(workspace.path().join("wasm-skills/macro-skill/src/lib.rs"))
                .unwrap();
        assert!(lib_rs.contains("workforce_skill_sdk::init!();"));
        assert!(lib_rs.contains("workforce_skill_sdk::tool!(run, |input: ToolInput|"));
        assert!(!lib_rs.contains("\ntool!("));

        // The scaffolded manifest has to parse and validate as written, and
        // its commented hints must survive the format! braces intact.
        let skill_toml =
            std::fs::read_to_string(workspace.path().join("wasm-skills/macro-skill/Skill.toml"))
                .unwrap();
        assert!(
            skill_toml.contains(
                "approval_summary = \"apply `{{action}}` to {{recipients|count}} recipient(s)\""
            ),
            "{skill_toml}"
        );
        workforce_wasm_runtime::SkillManifest::from_toml(&skill_toml)
            .and_then(|m| m.validate())
            .expect("the scaffolded manifest parses and validates");
    }

    /// The incident shape: a relative path must land in the persistent
    /// workspace, not wherever the hub process happens to be running.
    #[tokio::test]
    async fn scaffold_defaults_into_the_workspace_not_the_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store.path());
        let tool = WasmSkillScaffoldTool::new(ctx);

        let result = tool
            .execute(scaffold_input("demo-skill", None))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);

        let expected = workspace
            .path()
            .canonicalize()
            .unwrap()
            .join("wasm-skills/demo-skill");
        assert!(expected.join("Skill.toml").is_file());
        assert_eq!(result.output["path"], expected.display().to_string());

        let again = tool
            .execute(scaffold_input("demo-skill", None))
            .await
            .unwrap();
        assert!(
            !again.success,
            "the exists-guard must refuse a second scaffold"
        );
    }

    #[tokio::test]
    async fn scaffold_refuses_a_path_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let tool = WasmSkillScaffoldTool::new(test_ctx(workspace.path(), store.path()));

        for escape in ["/tmp/elsewhere", "../../../tmp/elsewhere"] {
            let result = tool
                .execute(scaffold_input("demo-skill", Some(escape)))
                .await
                .unwrap();
            assert!(!result.success, "{escape} should be refused");
        }
        assert!(!workspace.path().join("wasm-skills").exists());
    }

    /// The hub writes a scaffold, so the directories it acts on unattended
    /// are refused as its destination however they are spelled.
    #[tokio::test]
    async fn scaffold_refuses_a_destination_in_the_agents_configuration() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let tool = WasmSkillScaffoldTool::new(test_ctx(workspace.path(), store.path()));

        for (name, dir) in [
            ("learning", "."),
            ("plan", "learning"),
            ("check", "notes/../heartbeats"),
        ] {
            let result = tool.execute(scaffold_input(name, Some(dir))).await.unwrap();
            assert!(!result.success, "{dir}/{name} was scaffolded");
        }
        for dir in ["learning", "heartbeats"] {
            assert!(!workspace.path().join(dir).exists(), "{dir}");
        }
    }

    #[tokio::test]
    async fn build_resolves_a_relative_project_path_against_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store.path());
        WasmSkillScaffoldTool::new(ctx.clone())
            .execute(scaffold_input("demo-skill", None))
            .await
            .unwrap();

        // The build itself needs a wasm toolchain, so this asserts the step
        // before it: the manifest was found and parsed at the workspace-
        // relative path. A CWD-relative resolution stops at "No Skill.toml".
        let resolved = ctx
            .resolve_path("wasm-skills/demo-skill")
            .expect("resolves");
        assert_eq!(
            resolved,
            workspace
                .path()
                .canonicalize()
                .unwrap()
                .join("wasm-skills/demo-skill")
        );
        assert!(resolved.join("Skill.toml").is_file());

        let result = WasmSkillBuildTool::new(ctx)
            .execute(serde_json::json!({ "project_path": "wasm-skills/demo-skill" }))
            .await
            .unwrap();
        let error = result.error.unwrap_or_default();
        assert!(
            error.is_empty() || error.starts_with("Build failed"),
            "expected the run to reach the build step, got: {error}"
        );
    }

    #[tokio::test]
    async fn install_refuses_a_wasm_path_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("skill.wasm"), b"\0asm").unwrap();

        let result = WasmSkillInstallTool::new(test_ctx(workspace.path(), store.path()))
            .execute(serde_json::json!({
                "wasm_path": outside.path().join("skill.wasm").display().to_string(),
            }))
            .await
            .unwrap();
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("outside the allowed directories"), "{error}");
    }

    #[tokio::test]
    async fn restore_source_names_what_is_recoverable_when_this_one_is_not() {
        let workspace = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store_dir.path());
        install_test_skill(&ctx, "demo-skill");

        let result = WasmSkillRestoreSourceTool::new(ctx)
            .execute(serde_json::json!({ "skill": "demo-skill" }))
            .await
            .unwrap();
        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("No source snapshot"), "{error}");
    }

    #[tokio::test]
    async fn restore_source_reports_what_is_recoverable_and_refuses_a_busy_dest() {
        let workspace = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store_dir.path());
        let tool = WasmSkillRestoreSourceTool::new(ctx.clone());

        let missing = tool
            .execute(serde_json::json!({ "skill": "demo-skill" }))
            .await
            .unwrap();
        assert!(!missing.success);
        assert!(missing
            .error
            .unwrap()
            .contains("No WASM skill named `demo-skill` is installed"));

        // Snapshot a project, then restore it into a fresh directory.
        let project = workspace.path().join("wasm-skills/demo-skill");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("Skill.toml"),
            "[skill]\nname = \"demo-skill\"\n",
        )
        .unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(project.join("src/lib.rs"), "// the real implementation").unwrap();
        install_test_skill(&ctx, "demo-skill");
        ctx.store
            .snapshot_source(
                "demo-skill",
                &project,
                &ctx.store.get_installed("demo-skill").unwrap(),
            )
            .unwrap();

        let restored = tool
            .execute(serde_json::json!({ "skill": "demo-skill", "dest_dir": "recovered" }))
            .await
            .unwrap();
        assert!(restored.success, "{:?}", restored.error);
        assert_eq!(restored.output["files"], 3);
        assert_eq!(
            std::fs::read_to_string(
                workspace
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("recovered/src/lib.rs")
            )
            .unwrap(),
            "// the real implementation"
        );

        let busy = tool
            .execute(serde_json::json!({ "skill": "demo-skill", "dest_dir": "recovered" }))
            .await
            .unwrap();
        assert!(!busy.success, "must not overwrite existing work");

        // An empty learning directory would otherwise take the snapshot's
        // README.md as a learning plan.
        std::fs::create_dir(workspace.path().join("learning")).unwrap();
        let into_config = tool
            .execute(serde_json::json!({ "skill": "demo-skill", "dest_dir": "learning" }))
            .await
            .unwrap();
        assert!(!into_config.success);
        assert_eq!(
            std::fs::read_dir(workspace.path().join("learning"))
                .unwrap()
                .count(),
            0
        );
    }

    /// Building from outside the persistent workspace (a git worktree) is
    /// allowed but flagged: the stored snapshot is then the durable copy.
    #[tokio::test]
    async fn a_project_outside_the_workspace_is_flagged() {
        let workspace = tempfile::tempdir().unwrap();
        let worktrees = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(workspace.path(), store_dir.path());
        ctx.path_roots
            .push(worktrees.path().canonicalize().unwrap());

        install_test_skill(&ctx, "demo-skill");
        let outside = worktrees.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(outside.join("src")).unwrap();
        std::fs::write(outside.join("Skill.toml"), "[skill]\n").unwrap();
        std::fs::write(outside.join("src/lib.rs"), "// impl").unwrap();

        let mut result = ToolResult::success(serde_json::json!({ "installed": true }), 0);
        record_source_snapshot(
            &ctx.store,
            "demo-skill",
            &outside,
            ctx.workspace_root(),
            &mut result,
        );
        assert_eq!(result.output["project_outside_workspace"], true);
        assert!(result.output["project_outside_workspace_note"]
            .as_str()
            .unwrap()
            .contains("restore_source"));

        let inside = workspace
            .path()
            .canonicalize()
            .unwrap()
            .join("wasm-skills/demo");
        std::fs::create_dir_all(inside.join("src")).unwrap();
        std::fs::write(inside.join("Skill.toml"), "[skill]\n").unwrap();
        std::fs::write(inside.join("src/lib.rs"), "// impl").unwrap();
        let mut result = ToolResult::success(serde_json::json!({ "installed": true }), 0);
        record_source_snapshot(
            &ctx.store,
            "demo-skill",
            &inside,
            ctx.workspace_root(),
            &mut result,
        );
        assert!(result.output.get("project_outside_workspace").is_none());
        assert!(result
            .output
            .get("project_outside_workspace_note")
            .is_none());
    }

    /// A build that installs nothing stores nothing: the note must not point
    /// at a durable copy that does not exist.
    #[test]
    fn a_build_only_run_outside_the_workspace_says_nothing_is_stored() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut result = ToolResult::success(serde_json::json!({ "built": true }), 0);
        note_project_location(
            &outside.path().canonicalize().unwrap(),
            Some(&workspace.path().canonicalize().unwrap()),
            false,
            &mut result,
        );
        let note = result.output["project_outside_workspace_note"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(note.contains("nothing durable"), "{note}");
        assert!(!note.contains("restore_source"), "{note}");
    }

    #[tokio::test]
    async fn list_reports_snapshot_availability() {
        let workspace = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store_dir.path());

        let project = workspace.path().join("wasm-skills/demo-skill");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Skill.toml"), "[skill]\n").unwrap();
        std::fs::write(project.join("src/lib.rs"), "// impl").unwrap();
        assert!(!ctx.store.has_source_snapshot("demo-skill"));
        install_test_skill(&ctx, "demo-skill");
        ctx.store
            .snapshot_source(
                "demo-skill",
                &project,
                &ctx.store.get_installed("demo-skill").unwrap(),
            )
            .unwrap();
        assert!(ctx.store.has_source_snapshot("demo-skill"));
    }

    impl workforce_common::settings::SecretStore for MapVault {
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
            self.0.lock().insert(path.into(), value.into());
            Ok(())
        }
        fn delete(&self, path: &str) -> eyre::Result<bool> {
            Ok(self.0.lock().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.0
                .lock()
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    #[test]
    fn locker_reads_prefer_primary_and_fall_back_writes_hit_primary_only() {
        use workforce_common::settings::SecretStore;
        let global = MapVault::new();
        let agent = MapVault::new();
        agent.set("wasm-skills/s/shared/legacy", "old").unwrap();
        global.set("wasm-skills/s/shared/both", "new").unwrap();
        agent.set("wasm-skills/s/shared/both", "old").unwrap();

        let locker = SkillLockerVault::compose_stores(
            Some(Arc::clone(&global) as _),
            Some(Arc::clone(&agent) as _),
        )
        .unwrap();
        assert_eq!(
            locker.get("wasm-skills/s/shared/both").as_deref(),
            Some("new")
        );
        assert_eq!(
            locker.get("wasm-skills/s/shared/legacy").as_deref(),
            Some("old")
        );

        locker.set("wasm-skills/s/shared/fresh", "v").unwrap();
        assert_eq!(
            global.get("wasm-skills/s/shared/fresh").as_deref(),
            Some("v")
        );
        assert_eq!(agent.get("wasm-skills/s/shared/fresh"), None);

        assert!(locker.delete("wasm-skills/s/shared/legacy").unwrap());
        assert_eq!(locker.get("wasm-skills/s/shared/legacy"), None);
    }

    #[test]
    fn locker_without_global_is_the_agent_vault() {
        use workforce_common::settings::SecretStore;
        let agent = MapVault::new();
        agent.set("k", "v").unwrap();
        let locker = SkillLockerVault::compose_stores(None, Some(Arc::clone(&agent) as _)).unwrap();
        assert_eq!(locker.get("k").as_deref(), Some("v"));
        assert!(SkillLockerVault::compose_stores(None, None).is_none());
    }

    #[tokio::test]
    async fn install_and_unload_propagate_to_every_registered_host() {
        let reg_a = Arc::new(ToolRegistry::new());
        let reg_b = Arc::new(ToolRegistry::new());
        let state_a = Arc::new(SkillStateTracker::new());
        let state_b = Arc::new(SkillStateTracker::new());
        let slot: workforce_wasm_runtime::types::SkillInvokerSlot =
            Arc::new(parking_lot::RwLock::new(None));
        let hosts = SkillHosts::default();
        hosts.register("a", &reg_a, &state_a, None, Arc::clone(&slot));
        hosts.register("b", &reg_b, &state_b, None, Arc::clone(&slot));
        {
            let dead = Arc::new(ToolRegistry::new());
            let dead_state = Arc::new(SkillStateTracker::new());
            hosts.register("dead", &dead, &dead_state, None, Arc::clone(&slot));
        }

        let manifest = SkillManifest::from_toml(
            r#"
[skill]
name = "roster-skill"
version = "0.1.0"
description = "roster test"

[[tools]]
name = "ping"
description = "ping"
"#,
        )
        .unwrap();
        let engine = global_skill_engine().unwrap();
        let names = hosts.register_skill(&manifest, &engine, 1, &HashMap::new());
        assert_eq!(names, vec!["roster-skill.ping".to_string()]);
        assert!(reg_a.has("roster-skill.ping"));
        assert!(reg_b.has("roster-skill.ping"));

        let removed = hosts.unload_skill("roster-skill");
        assert_eq!(removed, vec!["roster-skill.ping".to_string()]);
        assert!(!reg_a.has("roster-skill.ping"));
        assert!(!reg_b.has("roster-skill.ping"));
    }

    /// The guide is the agent's substitute for an hour of trial-and-error —
    /// pin the load-bearing rules so an edit can't silently drop one.
    #[test]
    fn authoring_guide_states_every_enforced_rule() {
        for phrase in [
            "[network]\nallowed_hosts",
            "[[credentials]]",
            "[[permissions]]",
            "tools::invoke",
            "tools::invoke_many",
            "tools::call",
            "pipeline::Shipped",
            "cmd: secret <name> <value>",
            "cmd: skill grant <skill> <name>",
            "cmd: skill grant <skill> tools",
            "Grants pin the skill's CONSENT CONTRACT",
            "Rebuilding with an unchanged contract KEEPS every grant",
            "retried host-side with backoff",
            "tool = \"record-keeper.apply_change\"",
            "requires_approval = true",
            "approval_summary = \"apply `{{action}}` to {{recipients|count}} recipient(s)\"",
            "The gate only ever ADDS a prompt",
            "`wasm_skill.test` still dry-runs it",
            "32 tool calls",
            "64 HTTP fetches",
            "re-arms after each host",
            "wasm_skill.test",
            "mock_credentials",
            "wasm_skill.*, vault.*",
            "cmd: skill grant <skill> tools shared",
            "Installs are hub-wide and INSTANT",
            "owner/repo@<40-hex-commit-sha>#subpath=my-skill",
            "wasm_skill.export <skill>",
            "wasm_skill.import",
            "system.install_external_skill",
            "replaces the skill's config",
            "skills/github/workspaces/<owner>/token",
            "system.publish_external_skill",
        ] {
            assert!(AUTHORING_GUIDE.contains(phrase), "guide lost: {phrase}");
        }
        assert!(SANDBOX_CONTRACT.starts_with("Call wasm_skill.guide FIRST"));
        assert!(SANDBOX_CONTRACT.contains("CONSENT CONTRACT"));
        assert!(SANDBOX_CONTRACT.contains("wasm_skill.test"));
    }

    fn sample_meta(sha256: &str) -> SkillExportMeta {
        SkillExportMeta {
            name: "roundtrip".into(),
            version: "0.1.0".into(),
            binary_version: 3,
            sha256: sha256.into(),
            grant_fingerprint: "fp".into(),
            exported_at: "2026-08-18T00:00:00Z".into(),
        }
    }

    const ROUNDTRIP_MANIFEST: &str = r#"
[skill]
name = "roundtrip"
version = "0.1.0"
description = "round trip"

[[tools]]
name = "ping"
description = "ping"
"#;

    #[test]
    fn skill_archive_round_trips() {
        let wasm = b"fake-wasm-bytes";
        let sha = workforce_wasm_runtime::store::sha256_hex(wasm);
        let archive = build_skill_archive(ROUNDTRIP_MANIFEST, wasm, &sample_meta(&sha)).unwrap();
        let (manifest, bytes, meta) = parse_skill_archive(&archive).unwrap();
        assert_eq!(manifest.name, "roundtrip");
        assert_eq!(bytes, wasm);
        assert_eq!(meta.sha256, sha);
        assert_eq!(meta.binary_version, 3);
    }

    #[test]
    fn tampered_archive_fails_integrity() {
        let wasm = b"fake-wasm-bytes";
        let archive =
            build_skill_archive(ROUNDTRIP_MANIFEST, wasm, &sample_meta("not-the-digest")).unwrap();
        let err = parse_skill_archive(&archive).unwrap_err();
        assert!(err.contains("integrity"), "{err}");
    }

    #[test]
    fn archive_missing_entries_or_junk_input_fail_clearly() {
        assert!(parse_skill_archive(b"not a zip")
            .unwrap_err()
            .contains("not a readable zip"));

        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("Skill.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(ROUNDTRIP_MANIFEST.as_bytes()).unwrap();
        let partial = writer.finish().unwrap().into_inner();
        let err = parse_skill_archive(&partial).unwrap_err();
        assert!(err.contains("missing skill.wasm"), "{err}");
    }

    #[test]
    fn mismatched_metadata_name_is_rejected() {
        let wasm = b"fake-wasm-bytes";
        let sha = workforce_wasm_runtime::store::sha256_hex(wasm);
        let mut meta = sample_meta(&sha);
        meta.name = "somebody-else".into();
        let archive = build_skill_archive(ROUNDTRIP_MANIFEST, wasm, &meta).unwrap();
        let err = parse_skill_archive(&archive).unwrap_err();
        assert!(err.contains("names 'somebody-else'"), "{err}");
    }

    #[test]
    fn commit_sha_refs_satisfy_the_github_pin() {
        assert!(is_commit_sha("d906911446a777a2bc96e01e80dd7aedbdefcda6"));
        assert!(!is_commit_sha("v0.1.0"));
        assert!(!is_commit_sha("d90691"));
        assert!(!is_commit_sha("g906911446a777a2bc96e01e80dd7aedbdefcda6"));

        assert!(github_ref_is_commit_pinned(
            "driftwerk/workforce-skills@d906911446a777a2bc96e01e80dd7aedbdefcda6#subpath=hello-world"
        ));
        assert!(!github_ref_is_commit_pinned(
            "driftwerk/workforce-skills@v0.1.0#subpath=hello-world"
        ));
        assert!(!github_ref_is_commit_pinned("driftwerk/workforce-skills"));

        assert!(GITHUB_PIN_RECIPE.contains("40-hex-commit-sha"));
        assert!(GITHUB_PIN_RECIPE.contains("release `.wasm` asset"));
    }

    #[test]
    fn parse_github_repo_ref_bare() {
        let (repo, tag, subpath) = parse_github_repo_ref("driftwerk/workforce-skills");
        assert_eq!(repo, "driftwerk/workforce-skills");
        assert_eq!(tag, None);
        assert_eq!(subpath, None);
    }

    #[test]
    fn parse_github_repo_ref_with_tag() {
        let (repo, tag, subpath) = parse_github_repo_ref("driftwerk/workforce-skills@v0.1.0");
        assert_eq!(repo, "driftwerk/workforce-skills");
        assert_eq!(tag, Some("v0.1.0"));
        assert_eq!(subpath, None);
    }

    #[test]
    fn parse_github_repo_ref_with_subpath() {
        let (repo, tag, subpath) =
            parse_github_repo_ref("driftwerk/workforce-skills#subpath=hello-world");
        assert_eq!(repo, "driftwerk/workforce-skills");
        assert_eq!(tag, None);
        assert_eq!(subpath, Some("hello-world"));
    }

    #[test]
    fn parse_github_repo_ref_with_tag_and_subpath() {
        let (repo, tag, subpath) =
            parse_github_repo_ref("driftwerk/workforce-skills@v0.1.0#subpath=hello-world");
        assert_eq!(repo, "driftwerk/workforce-skills");
        assert_eq!(tag, Some("v0.1.0"));
        assert_eq!(subpath, Some("hello-world"));
    }

    #[test]
    fn parse_github_repo_ref_strips_subpath_slashes() {
        let (_, _, subpath) = parse_github_repo_ref("foo/bar#subpath=/nested/skill/");
        assert_eq!(subpath, Some("nested/skill"));
    }

    #[test]
    fn parse_github_repo_ref_ignores_other_fragments() {
        let (_, _, subpath) = parse_github_repo_ref("foo/bar#ref=main&subpath=hello");
        assert_eq!(subpath, Some("hello"));
    }

    #[test]
    fn parse_github_repo_ref_empty_subpath_treated_as_none() {
        let (_, _, subpath) = parse_github_repo_ref("foo/bar#subpath=");
        assert_eq!(subpath, None);
    }

    const PINNED_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const PINNED_FORM: &str =
        "acme/skills@0123456789abcdef0123456789abcdef01234567#subpath=hello-counter";

    #[tokio::test]
    async fn the_github_form_agents_use_is_pinned_and_compiles() {
        assert_eq!(
            parse_github_repo_ref(PINNED_FORM),
            ("acme/skills", Some(PINNED_COMMIT), Some("hello-counter"))
        );
        assert!(github_ref_is_commit_pinned(PINNED_FORM));
        assert!(github_ref_builds_from_source(PINNED_FORM));
        assert_eq!(
            source_commit(&format!("github:{PINNED_FORM}")),
            Some(PINNED_COMMIT)
        );

        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = test_ctx(tmp.path(), &tmp.path().join("store"));
        let closed = WasmSkillContext {
            policy: WasmInstallPolicy {
                allow_build_on_install: false,
                ..WasmInstallPolicy::default()
            },
            ..ctx.clone()
        };
        let refused = WasmSkillInstallTool::new(closed)
            .execute(serde_json::json!({
                "github_repo": PINNED_FORM,
                "config": { "greeting": "hi" },
            }))
            .await
            .expect("tool ran");
        let refusal = refused.error.unwrap_or_default();
        assert!(
            refusal.contains("build-on-install is disabled"),
            "{refusal}"
        );

        let unpinned = WasmSkillInstallTool::new(ctx.clone())
            .execute(serde_json::json!({ "github_repo": "acme/skills#subpath=hello-counter" }))
            .await
            .expect("tool ran");
        let refusal = unpinned.error.unwrap_or_default();
        assert!(refusal.contains("require a supply-chain pin"), "{refusal}");
        assert!(refusal.contains(GITHUB_PIN_RECIPE), "{refusal}");
        assert!(ctx.store.list_installed().is_empty());
    }

    #[tokio::test]
    async fn reinstalling_replaces_the_config_with_the_config_passed() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = test_ctx(tmp.path(), &tmp.path().join("store"));
        std::fs::write(tmp.path().join("configured.wasm"), b"not-real-wasm").expect("write");
        std::fs::write(
            tmp.path().join("Skill.toml"),
            "[skill]\nname = \"configured\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             author = \"a\"\nlicense = \"MIT\"\n[[tools]]\nname = \"ping\"\n\
             description = \"p\"\ninput_schema = { type = \"object\" }\n",
        )
        .expect("write manifest");
        let install = WasmSkillInstallTool::new(ctx.clone());

        for (config, expected) in [
            (
                Some(serde_json::json!({ "greeting": "hi" })),
                &[("greeting", "hi")][..],
            ),
            (
                Some(serde_json::json!({ "tone": "warm" })),
                &[("tone", "warm")],
            ),
            (None, &[]),
        ] {
            let mut input = serde_json::json!({ "wasm_path": "configured.wasm" });
            if let Some(config) = config {
                input["config"] = config;
            }
            install.execute(input.clone()).await.expect("tool ran");
            let expected: HashMap<String, String> = expected
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            assert_eq!(
                ctx.store
                    .get_installed("configured")
                    .expect("stored")
                    .config,
                expected,
                "{input}"
            );
        }
    }

    #[tokio::test]
    async fn an_external_skill_passed_as_a_project_names_the_external_install() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = test_ctx(tmp.path(), &tmp.path().join("store"));
        let skill = tmp.path().join("hello-world");
        std::fs::create_dir_all(&skill).expect("dir");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: hello-world\nversion: 1.0.0\ndescription: Says hello\n---\nSay hello.\n",
        )
        .expect("write");

        let result = WasmSkillInstallTool::new(ctx.clone())
            .execute(serde_json::json!({ "project_path": "hello-world" }))
            .await
            .expect("tool ran");
        let refusal = result.error.unwrap_or_default();
        let canonical = skill.canonicalize().expect("canonical");
        assert!(
            refusal.contains("system.install_external_skill"),
            "{refusal}"
        );
        assert!(
            refusal.contains(&format!("source = \"{}\"", canonical.display())),
            "{refusal}"
        );
        assert!(ctx.store.list_installed().is_empty());

        std::fs::write(
            skill.join("Skill.toml"),
            "[skill]\nname = \"hello-world\"\n",
        )
        .expect("write");
        assert!(!crate::system_tool::holds_external_skill(&skill));
    }

    #[tokio::test]
    async fn install_keeps_its_schema_and_routes_the_other_kinds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let tool = WasmSkillInstallTool::new(test_ctx(tmp.path(), &tmp.path().join("store")));
        let schema = tool.input_schema();
        let mut properties: Vec<&str> = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();
        properties.sort_unstable();
        assert_eq!(
            properties,
            [
                "config",
                "github_repo",
                "project_path",
                "sha256",
                "visibility",
                "wasm_path"
            ]
        );
        assert!(schema.get("required").is_none());
        assert_eq!(
            schema["properties"]["visibility"]["enum"],
            serde_json::json!(["private", "team", "everyone"])
        );
        let description = tool.description();
        assert!(description.ends_with(SANDBOX_CONTRACT));
        assert!(description.contains("system.install_external_skill"));
        assert!(description.contains("system.install_catalog_skill"));
    }

    #[test]
    fn install_source_label_picks_first_set_field() {
        let github = InstallInput {
            github_repo: Some("foo/bar".into()),
            project_path: Some("/tmp/x".into()),
            wasm_path: Some("/tmp/y.wasm".into()),
            ..Default::default()
        };
        assert_eq!(install_source_label(&github), "github_repo");

        let project = InstallInput {
            project_path: Some("/tmp/x".into()),
            wasm_path: Some("/tmp/y.wasm".into()),
            ..Default::default()
        };
        assert_eq!(install_source_label(&project), "project_path");

        let wasm = InstallInput {
            wasm_path: Some("/tmp/y.wasm".into()),
            ..Default::default()
        };
        assert_eq!(install_source_label(&wasm), "wasm_path");

        assert_eq!(
            install_source_label(&InstallInput::default()),
            "unspecified"
        );
    }

    #[test]
    fn github_repo_always_requires_pin() {
        assert!(pin_requirement_reason("github_repo", false).is_some());
        assert!(pin_requirement_reason("github_repo", true).is_some());
    }

    #[test]
    fn local_sources_require_pin_only_under_policy() {
        for source in ["wasm_path", "project_path"] {
            assert!(pin_requirement_reason(source, false).is_none(), "{source}");
            assert!(pin_requirement_reason(source, true).is_some(), "{source}");
        }
    }

    #[test]
    fn unspecified_source_skips_pin_check() {
        // The "no source given" failure is surfaced by a separate downstream
        // branch with a clearer message; pin_requirement_reason just
        // declines to gate this case.
        assert!(pin_requirement_reason("unspecified", false).is_none());
        assert!(pin_requirement_reason("unspecified", true).is_none());
    }

    #[test]
    fn stderr_tail_keeps_short_output_and_truncates_long_on_char_boundary() {
        assert_eq!(stderr_tail("error: bad", 16 * 1024), "error: bad");

        let long = format!("{}error: the actual failure", "x".repeat(20_000));
        let tail = stderr_tail(&long, 1024);
        assert!(tail.starts_with("… (stderr truncated"));
        assert!(tail.ends_with("error: the actual failure"));

        // A multi-byte char straddling the cut must not split.
        let unicode = format!("{}é-tail", "🦀".repeat(1000));
        let tail = stderr_tail(&unicode, 10);
        assert!(tail.ends_with("é-tail"));
    }

    #[test]
    fn test_locker_vault_answers_grants_credentials_and_writes() {
        use workforce_common::settings::SecretStore;
        let locker = TestLockerVault {
            grants_prefix: "wasm-skills/s/grants/".into(),
            grant_record: "record".into(),
            credentials: [("api_key".to_string(), "test-value".to_string())].into(),
            writes: parking_lot::Mutex::new(HashMap::new()),
        };

        // Any grant record resolves; user- and shared-scope alike.
        assert_eq!(
            locker
                .get("wasm-skills/s/grants/users/u1/api_key")
                .as_deref(),
            Some("record")
        );
        assert_eq!(
            locker.get("wasm-skills/s/grants/shared/_tools").as_deref(),
            Some("record")
        );
        // A guest key that merely CONTAINS "grants" is not a grant record.
        assert_eq!(locker.get("wasm-skills/s/users/u1/grants/x"), None);

        // Declared credentials resolve by trailing key under any scope.
        assert_eq!(
            locker.get("wasm-skills/s/users/u1/api_key").as_deref(),
            Some("test-value")
        );
        assert_eq!(
            locker.get("wasm-skills/s/shared/api_key").as_deref(),
            Some("test-value")
        );
        assert_eq!(locker.get("wasm-skills/s/users/u1/unknown"), None);

        // Writes are absorbed and win over the mock on read-back.
        locker
            .set("wasm-skills/s/users/u1/api_key", "written")
            .unwrap();
        assert_eq!(
            locker.get("wasm-skills/s/users/u1/api_key").as_deref(),
            Some("written")
        );
        assert!(locker.delete("wasm-skills/s/users/u1/api_key").unwrap());
    }

    #[test]
    fn test_tool_invoker_returns_mocks_and_records_calls() {
        use workforce_wasm_runtime::types::{SkillCallContext, SkillToolInvoker};
        let invoker = TestToolInvoker {
            responses: [(
                "datadog.logs_search".to_string(),
                serde_json::json!({"success": true, "output": {"rows": 2}}),
            )]
            .into(),
            calls: parking_lot::Mutex::new(Vec::new()),
        };
        let ctx = SkillCallContext {
            skill_name: "s".into(),
            grant_fingerprint: "fp".into(),
            agent_id: "a".into(),
            user_id: Some("u".into()),
        };
        let permission = |tool: &str| workforce_wasm_runtime::manifest::ManifestPermission {
            tool: tool.into(),
            description: "d".into(),
            allowed_hosts: vec![],
        };

        let hit = invoker
            .invoke(
                &ctx,
                &permission("datadog.logs_search"),
                &serde_json::json!({"q": 1}),
            )
            .unwrap();
        assert_eq!(hit["success"], true);

        let miss = invoker
            .invoke(
                &ctx,
                &permission("slack.post_message"),
                &serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(miss["success"], false);
        assert!(miss["error"]
            .as_str()
            .unwrap()
            .contains("add it to mock_tools"));

        let calls = invoker.calls.lock().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["tool"], "datadog.logs_search");
        assert_eq!(calls[0]["args"]["q"], 1);
        assert_eq!(calls[1]["tool"], "slack.post_message");
    }

    // ── ownership, authoring, and the sandbox line ──────────────────────

    fn member_ctx(workspace: &Path, store_dir: &Path) -> WasmSkillContext {
        WasmSkillContext {
            admin: crate::authz::never_admin(),
            teams: Arc::new(parking_lot::RwLock::new(None)),
            ..test_ctx(workspace, store_dir)
        }
    }

    fn install_owned(ctx: &WasmSkillContext, name: &str, owner: &str) {
        let manifest = SkillManifest::from_toml(&format!(
            "[skill]\nname = \"{name}\"\nversion = \"1.0.0\"\n\
             description = \"test\"\nauthor = \"test\"\nlicense = \"MIT\"\n\
             [[tools]]\nname = \"ping\"\ndescription = \"pong\"\n\
             input_schema = {{ type = \"object\" }}\n"
        ))
        .expect("manifest parses");
        ctx.store
            .install_prebuilt(
                manifest,
                b"fake-wasm",
                None,
                "test".into(),
                HashMap::new(),
                workforce_wasm_runtime::SkillOwnership::owned(
                    owner,
                    workforce_core::access::Visibility::Private,
                ),
            )
            .expect("installs");
    }

    /// The whole point of the feature: a member installs their own skill, and
    /// it lands owned by them and private.
    #[tokio::test]
    async fn an_install_is_owned_by_the_installer_and_private_by_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        let wasm = tmp.path().join("hand-built.wasm");
        std::fs::write(&wasm, b"not-real-wasm").expect("write");
        std::fs::write(
            tmp.path().join("Skill.toml"),
            "[skill]\nname = \"mine\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             author = \"a\"\nlicense = \"MIT\"\n[[tools]]\nname = \"ping\"\n\
             description = \"p\"\ninput_schema = { type = \"object\" }\n",
        )
        .expect("write manifest");

        let install = WasmSkillInstallTool::new(ctx.clone());
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("alice".to_string(), async {
                install
                    .execute(serde_json::json!({ "wasm_path": "hand-built.wasm" }))
                    .await
                    .expect("tool ran")
            })
            .await;

        // The bytes are not a real module, so activation fails — but the store
        // records ownership before that, which is what this asserts.
        let record = ctx.store.get_installed("mine").expect("stored");
        assert_eq!(record.owner.as_deref(), Some("alice"));
        assert_eq!(
            record.visibility,
            workforce_core::access::Visibility::Private
        );
        assert!(record.shares.is_empty(), "{result:?}");
    }

    /// Reinstalling your own skill keeps its ownership, but an explicit
    /// `visibility` is a request the author made — it has to land, or the
    /// parameter silently does nothing on every path but the first install.
    #[tokio::test]
    async fn reinstalling_your_own_skill_applies_a_requested_visibility() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        install_owned(&ctx, "mine", "alice");
        ctx.store
            .set_access(
                "mine",
                workforce_core::access::AccessWrite::kept(
                    workforce_core::access::Visibility::Private,
                    vec![workforce_core::access::Grant::user(
                        "bob",
                        workforce_core::access::GrantLevel::Run,
                    )],
                ),
            )
            .expect("shared");
        std::fs::write(tmp.path().join("mine.wasm"), b"bytes").expect("write");
        std::fs::write(
            tmp.path().join("Skill.toml"),
            "[skill]\nname = \"mine\"\nversion = \"2.0.0\"\ndescription = \"d\"\n\
             author = \"a\"\nlicense = \"MIT\"\n[[tools]]\nname = \"ping\"\n\
             description = \"p\"\ninput_schema = { type = \"object\" }\n",
        )
        .expect("write manifest");

        let install = WasmSkillInstallTool::new(ctx.clone());
        workforce_agent::runner::CURRENT_USER_ID
            .scope("alice".to_string(), async {
                install
                    .execute(
                        serde_json::json!({ "wasm_path": "mine.wasm", "visibility": "everyone" }),
                    )
                    .await
                    .expect("tool ran")
            })
            .await;

        let record = ctx.store.get_installed("mine").expect("stored");
        assert_eq!(
            record.visibility,
            workforce_core::access::Visibility::Global
        );
        assert_eq!(record.owner.as_deref(), Some("alice"), "still alice's");
        assert_eq!(record.shares.len(), 1, "grantees survive the publish");
    }

    /// A stranger cannot use a reinstall to publish (or hide) a skill they do
    /// not control — the name check refuses them long before visibility.
    #[tokio::test]
    async fn install_refuses_a_name_another_user_owns() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        install_owned(&ctx, "taken", "alice");
        std::fs::write(tmp.path().join("taken.wasm"), b"bytes").expect("write");
        std::fs::write(
            tmp.path().join("Skill.toml"),
            "[skill]\nname = \"taken\"\nversion = \"2.0.0\"\ndescription = \"d\"\n\
             author = \"a\"\nlicense = \"MIT\"\n[[tools]]\nname = \"ping\"\n\
             description = \"p\"\ninput_schema = { type = \"object\" }\n",
        )
        .expect("write manifest");

        let install = WasmSkillInstallTool::new(ctx.clone());
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("bob".to_string(), async {
                install
                    .execute(serde_json::json!({ "wasm_path": "taken.wasm" }))
                    .await
                    .expect("tool ran")
            })
            .await;
        assert!(!result.success);
        assert!(
            result.error.unwrap_or_default().contains("another user"),
            "refusal should name the collision"
        );
        assert_eq!(
            ctx.store
                .get_installed("taken")
                .expect("still there")
                .manifest
                .version,
            "1.0.0",
            "the owner's skill must be untouched"
        );
    }

    /// Uninstall, update, export, and restore_source all read the same
    /// authority, so a stranger gets the same refusal from each.
    #[tokio::test]
    async fn management_tools_refuse_a_stranger() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        install_owned(&ctx, "mine", "alice");

        let refusals = workforce_agent::runner::CURRENT_USER_ID
            .scope("bob".to_string(), async {
                let args = serde_json::json!({ "skill": "mine" });
                vec![
                    WasmSkillUninstallTool::new(ctx.clone())
                        .execute(args.clone())
                        .await
                        .expect("ran"),
                    WasmSkillUpdateTool::new(ctx.clone())
                        .execute(args.clone())
                        .await
                        .expect("ran"),
                    WasmSkillExportTool::new(ctx.clone())
                        .execute(args.clone())
                        .await
                        .expect("ran"),
                    WasmSkillRestoreSourceTool::new(ctx.clone())
                        .execute(args)
                        .await
                        .expect("ran"),
                ]
            })
            .await;
        for result in refusals {
            assert!(!result.success);
            assert!(
                result.error.unwrap_or_default().contains("another user"),
                "every management tool must refuse a stranger"
            );
        }
        assert!(ctx.store.get_installed("mine").is_some());
    }

    #[tokio::test]
    async fn list_shows_only_what_the_caller_can_use() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        install_owned(&ctx, "hers", "alice");
        install_owned(&ctx, "published", "alice");
        ctx.store
            .set_access(
                "published",
                workforce_core::access::AccessWrite::kept(
                    workforce_core::access::Visibility::Global,
                    Vec::new(),
                ),
            )
            .expect("published");

        let names = |result: &ToolResult| -> Vec<String> {
            result.output["skills"]
                .as_array()
                .expect("skills")
                .iter()
                .map(|s| s["name"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        let list = WasmSkillListTool::new(ctx.clone());
        let bob = workforce_agent::runner::CURRENT_USER_ID
            .scope("bob".to_string(), async {
                list.execute(serde_json::json!({})).await.expect("ran")
            })
            .await;
        assert_eq!(names(&bob), vec!["published".to_string()]);

        let alice = workforce_agent::runner::CURRENT_USER_ID
            .scope("alice".to_string(), async {
                list.execute(serde_json::json!({})).await.expect("ran")
            })
            .await;
        assert_eq!(
            names(&alice),
            vec!["hers".to_string(), "published".to_string()]
        );
        assert_eq!(alice.output["skills"][0]["your_access"], "manage");
    }

    /// Without a sandbox, compiling on the host stays admin-only — and the
    /// refusal has to name a route that works.
    #[tokio::test]
    async fn a_member_cannot_compile_on_an_unsandboxed_hub() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        let build = WasmSkillBuildTool::new(ctx.clone());
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("bob".to_string(), async {
                build
                    .execute(serde_json::json!({ "project_path": "proj" }))
                    .await
                    .expect("ran")
            })
            .await;
        assert!(!result.success);
        let message = result.error.unwrap_or_default();
        assert!(message.contains("admin-only"), "got: {message}");
        assert!(message.contains("CI"), "must offer the CI route: {message}");
    }

    /// The single-tenant CLI has no identity at all — the same case the
    /// capability gate skips, so builds there must still work.
    #[tokio::test]
    async fn an_identity_less_caller_may_still_build() {
        let tmp = tempfile::tempdir().expect("tmp");
        let ctx = member_ctx(tmp.path(), &tmp.path().join("store"));
        let result = WasmSkillBuildTool::new(ctx)
            .execute(serde_json::json!({ "project_path": "missing-project" }))
            .await
            .expect("ran");
        assert!(!result.success);
        let message = result.error.unwrap_or_default();
        assert!(
            message.contains("No Skill.toml"),
            "should fail on the project, not on authority: {message}"
        );
    }

    #[test]
    fn git_dependencies_are_found_in_every_table_form() {
        let manifest = r#"
[package]
name = "s"

[dependencies]
serde = "1"
private-lib = { git = "https://github.com/acme/private-lib" }

[build-dependencies]
gen = { git = "ssh://git@github.com/acme/gen" }

[target.'cfg(unix)'.dev-dependencies]
harness = { git = "https://example.com/harness" }
"#;
        assert_eq!(
            git_dependencies(manifest),
            vec![
                "gen".to_string(),
                "harness".to_string(),
                "private-lib".to_string()
            ]
        );
        assert!(git_dependencies("[dependencies]\nserde = \"1\"\n").is_empty());
        assert!(git_dependencies("not : valid : toml").is_empty());
    }

    /// A registered host reads the process-wide gate slot, so binding the gate
    /// after tools are registered still covers them. That the serve path
    /// *binds* one is enforced at boot, not here — an unwired slot is
    /// permissive by design (see `assert_use_gate_wired`).
    #[test]
    fn registered_hosts_read_the_shared_use_gate_slot() {
        struct DenyAll;
        impl workforce_wasm_runtime::SkillUseGate for DenyAll {
            fn authorize_use(&self, _skill: &str, _user: Option<&str>) -> Result<(), String> {
                Err("denied".to_string())
            }
        }
        set_skill_use_gate(Arc::new(DenyAll));
        assert!(skill_use_gate_is_wired());

        let registry = Arc::new(ToolRegistry::new());
        let state = Arc::new(SkillStateTracker::new());
        let hosts = SkillHosts::default();
        hosts.register("agent", &registry, &state, None, Arc::default());
        let wired = hosts
            .0
            .lock()
            .iter()
            .filter(|h| h.agent_id == "agent")
            .all(|h| h.bindings.authorize_use("any", Some("bob")).is_err());
        assert!(wired, "a registered host must read the wired gate");
    }
}
