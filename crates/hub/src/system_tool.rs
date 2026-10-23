//! System administration tools — let authorized agents manage skills at runtime.
//!
//! These tools allow agents to enable/disable built-in skills, install/uninstall
//! external skills, check credential status, and manage the tool runtime without
//! server restarts. Access is controlled by the policy engine — `system.*` tools
//! can be restricted to specific agents via policy rules.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::WorkforceConfig;
use workforce_core::access::{AccessLevel, Visibility};
use workforce_skills::installer::{
    self, CloneFailed, GithubRef, InstallOptions, Installed, ManifestKind, SkillOrigin,
};

pub(crate) use crate::hub_core::capabilities::InstallExternalInput;
use crate::skill_access::{sharing_hint, SkillCaller, SkillRegistry};
use crate::skill_catalog::{InstallAuthority, SkillCatalog};
use crate::skill_state::SkillStateTracker;
use crate::vault::Vault;

/// Shared context for all system tools.
#[derive(Clone)]
pub struct SystemToolContext {
    pub vault: Arc<Vault>,
    pub global_vault: Option<Arc<Vault>>,
    pub tools: Arc<ToolRegistry>,
    pub skill_state: Arc<SkillStateTracker>,
    pub config: WorkforceConfig,
    /// The live external (SKILL.md) skill set every agent shares: `system.enable_skill` serves
    /// bodies from it and the install tools refresh it.
    pub discovered_skills: Arc<crate::external_skills::ExternalSkills>,
    /// Status-aware admin policy, deciding the caller's standing on catalog WASM installs.
    pub admin: crate::authz::IsAdminFn,
    /// Roots a member's `source` directory must resolve inside, the same roots `wasm_skill.install`
    /// confines `project_path` to.
    pub path_roots: Vec<PathBuf>,
    /// Whether the caller holds skills_manage, the standing that installs external skills for
    /// everyone and manages every external skill.
    pub skill_managers: crate::authz::IsAdminFn,
    /// Late-bound team store: how a team-visible skill, and a grant naming a team, reach the
    /// caller of the current turn.
    pub teams: crate::def_write_auth::TeamStoreHandle,
}

impl SystemToolContext {
    /// Catalog tokens are org credentials, so the catalog reads them from the global vault.
    fn skill_catalog(&self) -> SkillCatalog {
        SkillCatalog::live(
            &self.config,
            self.global_vault.clone(),
            Arc::clone(&self.discovered_skills),
        )
    }
}

pub(crate) const REQUIREMENTS_MET: &str = "all requirements met";

/// Create all system tools.
pub fn system_tools(ctx: SystemToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SystemListToolsTool::new(Arc::clone(&ctx.tools))),
        Arc::new(SystemListSkillsTool::new(ctx.clone())),
        Arc::new(SystemEnableSkillTool::new(ctx.clone())),
        Arc::new(SystemDisableSkillTool::new(ctx.clone())),
        Arc::new(SystemSkillStatusTool::new(ctx.clone())),
        Arc::new(SystemInstallExternalSkillTool::new(ctx.clone())),
        Arc::new(SystemUninstallExternalSkillTool::new(ctx.clone())),
        Arc::new(SystemListCatalogSkillsTool::new(ctx.clone())),
        Arc::new(SystemInstallCatalogSkillTool::new(ctx.clone())),
        Arc::new(
            crate::external_skill_publish::SystemPublishExternalSkillTool::github(ctx.clone()),
        ),
        Arc::new(SystemListExternalSkillsTool::new(ctx)),
    ]
}

// ─── system.list_tools ───────────────────────────────────────────────────────

pub struct SystemListToolsTool {
    tools: Arc<ToolRegistry>,
}

impl SystemListToolsTool {
    pub fn new(tools: Arc<ToolRegistry>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Tool for SystemListToolsTool {
    fn name(&self) -> &str {
        "system.list_tools"
    }

    fn description(&self) -> &str {
        "List all tools currently registered and available to you. Returns names and descriptions only — load one with `tool_load` if it is not already in your tool list."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Optional prefix to filter tools"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let prefix = input.get("prefix").and_then(|v| v.as_str()).unwrap_or("");

        let defs: Vec<serde_json::Value> = self
            .tools
            .definitions()
            .into_iter()
            .filter(|d| prefix.is_empty() || d.name.starts_with(prefix))
            .map(|d| {
                serde_json::json!({
                    "name": d.name,
                    "description": d.description,
                })
            })
            .collect();

        let count = defs.len();
        Ok(ToolResult::success(
            serde_json::json!({
                "total": count,
                "tools": defs,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── system.list_skills ──────────────────────────────────────────────────────

pub struct SystemListSkillsTool {
    ctx: SystemToolContext,
}

impl SystemListSkillsTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemListSkillsTool {
    fn name(&self) -> &str {
        "system.list_skills"
    }

    fn description(&self) -> &str {
        "List all available built-in skills, their status"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let credential_status = crate::skill_loader::check_skill_credentials(
            Arc::clone(&self.ctx.vault),
            self.ctx.global_vault.clone(),
        );
        let loaded_skills = self.ctx.skill_state.all_loaded();

        let skills: Vec<serde_json::Value> = credential_status
            .iter()
            .map(|cs| {
                let loaded = loaded_skills.iter().find(|ls| ls.name == cs.name);
                serde_json::json!({
                    "name": cs.name,
                    "description": cs.description,
                    "status": if loaded.is_some() {
                        "loaded"
                    } else if cs.ready {
                        "available"
                    } else {
                        "needs_credentials"
                    },
                    "loaded_tools": loaded.map(|ls| &ls.tool_names).cloned().unwrap_or_default(),
                    "loaded_source": loaded.map(|ls| &ls.source),
                    "credentials": cs.credentials.iter().map(|c| serde_json::json!({
                        "vault_path": c.vault_path,
                        "description": c.description,
                        "configured": c.configured,
                        "required": c.required,
                    })).collect::<Vec<_>>(),
                    "setup_hint": cs.setup_hint,
                    "configured_workspaces": cs.configured_workspaces,
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({ "skills": skills }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── system.enable_skill ─────────────────────────────────────────────────────

/// Load an external (SKILL.md) skill on demand: external skills register no
/// tools — the body IS the procedure — so this returns the full instructions
/// (run via generic tools like `exec.run`), or a requirements-not-met error.
fn external_skill_load_result(
    skill: &workforce_skills::scanner::DiscoveredSkill,
    duration_ms: u64,
) -> ToolResult {
    let meta = &skill.skill.meta;
    if let Some(reason) = skill.unavailable_reason() {
        return ToolResult::failure(
            format!(
                "Skill '{}' is installed but not ready to use:\n{reason}",
                meta.name
            ),
            duration_ms,
        );
    }
    let procedure = format!(
        "Loaded skill '{}' v{}. Follow this procedure step by step for the task. \
         External skills register no dedicated tools — carry out the steps with the \
         generic tools they reference (e.g. `exec.run`, `vault.get`).\n\n{}",
        meta.name, meta.version, skill.skill.body
    );
    ToolResult::success(
        serde_json::json!({
            "skill": meta.name,
            "version": meta.version,
            "kind": "external",
            "loaded": true,
            "procedure": procedure,
        }),
        duration_ms,
    )
}

pub struct SystemEnableSkillTool {
    ctx: SystemToolContext,
}

impl SystemEnableSkillTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct EnableInput {
    skill: String,
}

#[async_trait]
impl Tool for SystemEnableSkillTool {
    fn name(&self) -> &str {
        "system.enable_skill"
    }

    fn description(&self) -> &str {
        "Load a skill on demand: built-in skills register their tools from vault \
         credentials; external (SKILL.md) skills return their full procedure for you to follow"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name to enable (e.g., 'github')"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: EnableInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Slack is a long-running socket-mode connection managed by
        // SlackManager, not a stateless tool registration. Dispatch to the
        // already-registered system.reload_slack tool, then translate its
        // result into the same envelope shape the other skills return so
        // skill_state stays consistent and `system.skill_status: slack`
        // reports accurately.
        if params.skill == "slack" {
            return self.enable_slack(duration).await;
        }

        // External (SKILL.md) skills are served on demand (the body is the
        // procedure), to a caller who may use them: they register no tools, so
        // this is the check every surface that loads one passes through.
        // Built-in names take precedence; anything else falls through to the
        // vault loader below.
        let is_builtin = crate::skill_loader::builtin_skill_catalog()
            .iter()
            .any(|s| s.name == params.skill);
        let skills = self.ctx.discovered_skills.as_ref();
        let caller = SkillCaller::current(&self.ctx.skill_managers, &self.ctx.teams);
        if !is_builtin && skills.installed(&params.skill).is_some() {
            return Ok(
                match caller.require(skills, &params.skill, AccessLevel::Run) {
                    Ok(skill) => external_skill_load_result(&skill.discovered, duration()),
                    Err(refusal) => ToolResult::failure(refusal, duration()),
                },
            );
        }

        // If already loaded, auto-reload: unload first, then re-load from vault.
        let reloaded = if self.ctx.skill_state.is_loaded(&params.skill) {
            let removed = crate::skill_loader::unload_skill(&params.skill, &self.ctx.tools);
            self.ctx.skill_state.mark_unloaded(&params.skill);
            info!(
                skill = %params.skill,
                removed = removed.len(),
                "Auto-unloading skill for reload via system.enable_skill"
            );
            true
        } else {
            false
        };

        match crate::skill_loader::load_skill_from_vault(
            &params.skill,
            Arc::clone(&self.ctx.vault),
            self.ctx.global_vault.clone(),
            &self.ctx.config,
            &self.ctx.tools,
        ) {
            Ok(result) => {
                let count = result.tool_names.len();
                self.ctx.skill_state.mark_loaded(
                    &params.skill,
                    result.tool_names.clone(),
                    "vault",
                    result.context_hint.clone(),
                );
                info!(
                    skill = %params.skill,
                    count,
                    "Skill enabled at runtime via system.enable_skill"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "skill": params.skill,
                        "enabled": true,
                        "reloaded": reloaded,
                        "tools_loaded": count,
                        "tool_names": result.tool_names,
                        "context": result.context_hint,
                    }),
                    duration(),
                )
                .loading(result.tool_names))
            }
            Err(e) => {
                let available: Vec<String> = crate::skill_loader::builtin_skill_catalog()
                    .iter()
                    .map(|s| s.name.to_string())
                    .chain(
                        caller
                            .visible(skills)
                            .into_iter()
                            .filter(|(skill, _)| skill.discovered.is_available())
                            .map(|(skill, _)| skill.name().to_string()),
                    )
                    .collect();
                Ok(ToolResult::failure(
                    format!(
                        "Failed to enable skill '{}': {}. Available skills: {}.",
                        params.skill,
                        e,
                        available.join(", ")
                    ),
                    duration(),
                ))
            }
        }
    }
}

impl SystemEnableSkillTool {
    /// Slack-specific enable path: dispatches to `system.reload_slack` and
    /// normalises the result so it matches the envelope returned for other
    /// builtin skills (skill / enabled / reloaded / context fields).
    async fn enable_slack(&self, duration: impl Fn() -> u64) -> eyre::Result<ToolResult> {
        let Some(reload_tool) = self.ctx.tools.get("system.reload_slack") else {
            return Ok(ToolResult::failure(
                "system.reload_slack not registered — Slack support not active in this build"
                    .to_string(),
                duration(),
            ));
        };
        let inner = reload_tool.execute(serde_json::json!({})).await?;
        if !inner.success {
            return Ok(ToolResult::failure(
                inner
                    .error
                    .unwrap_or_else(|| "Slack reload failed".to_string()),
                duration(),
            ));
        }
        let workspaces = inner
            .output
            .get("workspaces")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let context_hint = if workspaces > 0 {
            Some(format!(
                "{} workspace{} connected",
                workspaces,
                if workspaces == 1 { "" } else { "s" }
            ))
        } else {
            Some("no workspaces had credentials — set tokens via vault".to_string())
        };
        if workspaces > 0 {
            // Slack tools are pre-registered at hub startup, so the
            // tool_names list is empty here — the state tracker is just
            // recording that the skill is now actively connected.
            self.ctx
                .skill_state
                .mark_loaded("slack", Vec::new(), "vault", context_hint.clone());
        } else {
            self.ctx.skill_state.mark_unloaded("slack");
        }
        info!(
            skill = "slack",
            workspaces, "Slack enabled at runtime via system.enable_skill"
        );
        Ok(ToolResult::success(
            serde_json::json!({
                "skill": "slack",
                "enabled": workspaces > 0,
                "reloaded": true,
                "workspaces": workspaces,
                "context": context_hint,
            }),
            duration(),
        ))
    }
}

// ─── system.disable_skill ────────────────────────────────────────────────────

pub struct SystemDisableSkillTool {
    ctx: SystemToolContext,
}

impl SystemDisableSkillTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct DisableInput {
    skill: String,
}

#[async_trait]
impl Tool for SystemDisableSkillTool {
    fn name(&self) -> &str {
        "system.disable_skill"
    }

    fn description(&self) -> &str {
        "Disable a skill by removing its tools from the runtime"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name to disable (e.g., 'github')"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: DisableInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if !self.ctx.skill_state.is_loaded(&params.skill) {
            return Ok(ToolResult::failure(
                format!("Skill '{}' is not currently loaded", params.skill),
                duration(),
            ));
        }

        let removed = crate::skill_loader::unload_skill(&params.skill, &self.ctx.tools);
        self.ctx.skill_state.mark_unloaded(&params.skill);

        info!(
            skill = %params.skill,
            removed = removed.len(),
            "Skill disabled at runtime via system.disable_skill"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "skill": params.skill,
                "disabled": true,
                "tools_removed": removed.len(),
                "removed_tools": removed,
            }),
            duration(),
        ))
    }
}

// ─── system.skill_status ─────────────────────────────────────────────────────

pub struct SystemSkillStatusTool {
    ctx: SystemToolContext,
}

impl SystemSkillStatusTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct StatusInput {
    skill: String,
}

#[async_trait]
impl Tool for SystemSkillStatusTool {
    fn name(&self) -> &str {
        "system.skill_status"
    }

    fn description(&self) -> &str {
        "Check the detailed status of a built-in skill: whether its credentials are configured \
         and which of its tools are loaded"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name to check (e.g., 'github')"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: StatusInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let cred_statuses = crate::skill_loader::check_skill_credentials(
            Arc::clone(&self.ctx.vault),
            self.ctx.global_vault.clone(),
        );
        let cred_status = cred_statuses.iter().find(|s| s.name == params.skill);

        let Some(cs) = cred_status else {
            return Ok(ToolResult::failure(
                format!("Unknown skill: '{}'", params.skill),
                duration(),
            ));
        };

        let loaded = self.ctx.skill_state.get(&params.skill);

        Ok(ToolResult::success(
            serde_json::json!({
                "name": cs.name,
                "description": cs.description,
                "status": if loaded.is_some() {
                    "loaded"
                } else if cs.ready {
                    "available"
                } else {
                    "needs_credentials"
                },
                "credentials_ready": cs.ready,
                "credentials": cs.credentials.iter().map(|c| serde_json::json!({
                    "vault_path": c.vault_path,
                    "description": c.description,
                    "configured": c.configured,
                    "required": c.required,
                })).collect::<Vec<_>>(),
                "loaded_tools": loaded.as_ref().map(|l| &l.tool_names),
                "loaded_source": loaded.as_ref().map(|l| &l.source),
                "setup_hint": cs.setup_hint,
                "configured_workspaces": cs.configured_workspaces,
            }),
            duration(),
        ))
    }
}

// ─── Installing skills: routing and failure guidance ───────────────────────

/// What every agent is told about installing skills. Static: it sits in the cached prompt prefix.
pub(crate) const INSTALL_GUIDE: &str = "\n\n## Installing Skills\n\n\
An external skill is a directory with a SKILL.md: instructions you load with \
`system.enable_skill` and carry out with the tools you already have. A WASM skill is a \
directory with a Skill.toml and Rust source: it adds tools named `<skill>.<tool>`.\n\
- A skill a configured catalog lists (`system.list_catalog_skills`) installs by name, either \
kind, with `system.install_catalog_skill`. Check there first. A catalog install is for every \
user and needs the skills_manage capability (administrators by default); without it, ask an \
administrator to install it.\n\
- Any other skill installs from its repository, with the tool for its kind: \
`system.install_external_skill` for a SKILL.md, `wasm_skill.install` for a Skill.toml. Both take \
`github_repo = \"owner/repo@<ref>#subpath=<dir>\"`. For `wasm_skill.install` the ref must be a \
40-hex commit (or, for a skill at the root of its repository, pass a release asset's `sha256`), \
and building needs permission to build on this hub. Private GitHub repositories are read with \
the caller's token at `skills/github/workspaces/<owner>/token`, else `skills/github/token`. The \
tool for the wrong kind says so and gives the call to make.\n\
- To write an external skill, save a SKILL.md (a `---` header with `name`, `version` and \
`description`, then the steps) in a directory in your workspace, such as \
`skills/<name>/SKILL.md`, and install it with `system.install_external_skill` and \
`source = \"skills/<name>\"`. To write a WASM skill, call `wasm_skill.guide` first.\n\
- Any user can install a skill of either kind for themselves: it belongs to them, and only they \
and the people they share it with can use it. Pass `visibility = \"everyone\"` to make it every \
user's. An external skill installed by a holder of skills_manage is for every user unless they \
pass `visibility = \"private\"`.\n\
- Installing a skill again updates it and keeps its owner and sharing, so to change a skill, \
edit its files and install it again: the change is live on the next message. An external skill \
keeps its config.toml and a catalog install keeps a WASM skill's config, but \
`wasm_skill.install` replaces the config with the `config` it is given, so pass all of it.\n\
- The owner decides who else can use a skill of either kind, and types the command in chat: \
`cmd: skill share <skill> <@user> [manage]`, `cmd: skill unshare <skill> <@user>` and \
`cmd: skill visibility <skill> private|everyone`.\n\
- `system.list_external_skills` and `wasm_skill.list` show the skills you can use, with their \
owner and visibility; `system.uninstall_external_skill` and `wasm_skill.uninstall` remove a \
skill you manage.\n\
- To add a skill you manage to a catalog, open a pull request with \
`system.publish_external_skill` or `wasm_skill.publish`. Each publishes exactly what is \
installed, with its `catalog.json` entry, using your GitHub token for the catalog repository's \
owner, and refuses a version the catalog already has with different content: bump the version \
and install the skill again first. Once merged, the skill installs by name with \
`system.install_catalog_skill`. `wasm_skill.guide` covers WASM authoring, grants and sharing.\n";

/// How an install read a repository, which decides what to suggest when its clone fails.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RepoAccess<'a> {
    Github { owner: &'a str, token: bool },
    Anonymous,
}

/// `message`, followed on its own line by what to do next when `error` is a failed clone.
pub(crate) fn with_clone_hint(
    message: String,
    error: &eyre::Report,
    access: RepoAccess<'_>,
) -> String {
    match error.downcast_ref::<CloneFailed>() {
        Some(_) => format!("{message}\n{}", clone_hint(access)),
        None => message,
    }
}

fn clone_hint(access: RepoAccess<'_>) -> String {
    match access {
        RepoAccess::Github {
            owner,
            token: false,
        } => format!(
            "No GitHub token is set for `{owner}`, so only public repositories can be read. If the \
             repository is private, store a token that can read it with vault.set at \
             `skills/github/workspaces/{owner}/token` (for that organisation) or \
             `skills/github/token` (for any), then install again. A token you store is your own; \
             an administrator can store one for everyone with scope `global`."
        ),
        RepoAccess::Github { owner, token: true } => format!(
            "The GitHub token set for `{owner}` was used: check the repository name and ref, and \
             that the token can read the repository."
        ),
        RepoAccess::Anonymous => "This repository was cloned without credentials, because only \
             github.com repositories are read with a GitHub token, so it must be public. Pass a \
             GitHub repository as `github_repo`."
            .to_string(),
    }
}

/// Whether `dir` holds an external skill and no WASM manifest.
pub(crate) fn holds_external_skill(dir: &Path) -> bool {
    dir.join("SKILL.md").is_file() && !dir.join("Skill.toml").is_file()
}

pub(crate) fn external_skill_found(argument: &str, value: &str) -> String {
    format!(
        "`{value}` is an external skill: its directory has a SKILL.md and no Skill.toml, and \
         wasm_skill.install installs only WASM skills. Install it with \
         system.install_external_skill and `{argument} = \"{value}\"`."
    )
}

pub(crate) fn wasm_skill_found(
    name: &str,
    origin: SkillOrigin<'_>,
    commit: Option<&str>,
) -> String {
    let install = match (origin, commit) {
        (SkillOrigin::Github(repo), Some(commit)) => format!(
            "Install it with wasm_skill.install and `github_repo = \"{}\"`, pinned to the commit \
             that source points at now",
            GithubRef {
                git_ref: Some(commit),
                ..repo
            }
        ),
        (SkillOrigin::Local(path), _) => format!(
            "Install it with wasm_skill.install and `project_path = \"{path}\"`, which must be a \
             directory in your workspace"
        ),
        (SkillOrigin::Github(_), None) | (SkillOrigin::Git(_), _) => {
            "Install it with wasm_skill.install: from GitHub with \
             `github_repo = \"owner/repo@<40-hex-commit>#subpath=<dir>\"`, from a project in your \
             workspace with `project_path`, or from a built module with `wasm_path`"
                .to_string()
        }
    };
    format!(
        "`{name}` is a WASM skill: its directory has a Skill.toml and no SKILL.md, and \
         system.install_external_skill installs only external (SKILL.md) skills. {install}."
    )
}

// ─── system.install_external_skill ───────────────────────────────────────────

pub struct SystemInstallExternalSkillTool {
    ctx: SystemToolContext,
}

impl SystemInstallExternalSkillTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }

    fn github_token(&self, origin: SkillOrigin<'_>) -> Option<String> {
        match origin {
            SkillOrigin::Github(repo) => crate::user_config::resolve_github_pat(
                Some(self.ctx.vault.as_ref()),
                self.ctx.global_vault.as_deref(),
                repo.owner(),
            ),
            SkillOrigin::Git(_) | SkillOrigin::Local(_) => None,
        }
    }

    /// Where `caller` may read `origin` from, as the location and ref to locate. A skills manager
    /// reads any directory on the hub and any git host; anyone else reads GitHub and directories
    /// inside their workspace roots. A relative path resolves against the workspace for everyone.
    fn location(
        &self,
        caller: &SkillCaller,
        origin: SkillOrigin<'_>,
    ) -> Result<(String, Option<String>), String> {
        let located = || {
            let (location, git_ref) = origin.locate_args();
            (location, git_ref.map(str::to_string))
        };
        match origin {
            SkillOrigin::Github(_) => Ok(located()),
            SkillOrigin::Git(_) if !caller.is_member() => Ok(located()),
            SkillOrigin::Git(url) => Err(format!(
                "`{url}` is on a git host other than GitHub, and installing from one needs the \
                 skills_manage capability (administrators by default). Pass a GitHub repository \
                 as `github_repo`, or save the skill in your workspace and pass that directory as \
                 `source`."
            )),
            SkillOrigin::Local(path) => {
                let resolved = match caller.is_member() {
                    false => workforce_agent::tools::paths::resolve_against_root(
                        Path::new(path),
                        &self.ctx.path_roots,
                    ),
                    true => {
                        workforce_agent::tools::paths::resolve_in_roots(path, &self.ctx.path_roots)?
                    }
                };
                let location = resolved.display().to_string();
                let read_as_directory =
                    matches!(SkillOrigin::from_source(&location), SkillOrigin::Local(_));
                match (resolved.is_dir(), read_as_directory) {
                    (false, _) if GithubRef::parse(path).is_well_formed() => Err(format!(
                        "`{path}` is not a directory on this hub. To install from GitHub, pass it \
                         as `github_repo` instead of `source`."
                    )),
                    (_, false) => Err(format!(
                        "`{path}` resolves to `{location}`, which reads as a git URL rather than a \
                         directory. Pass a directory whose path does not end in `.git`."
                    )),
                    (_, true) => Ok((location, None)),
                }
            }
        }
    }

    async fn install(
        &self,
        caller: SkillCaller,
        origin: SkillOrigin<'_>,
        (location, git_ref): (String, Option<String>),
        requested: Option<Visibility>,
    ) -> Result<(Installed, Option<String>), String> {
        let token = self.github_token(origin);
        let access = match origin {
            SkillOrigin::Github(repo) => RepoAccess::Github {
                owner: repo.owner(),
                token: token.is_some(),
            },
            SkillOrigin::Git(_) | SkillOrigin::Local(_) => RepoAccess::Anonymous,
        };
        let located = tokio::task::spawn_blocking(move || {
            installer::locate_skill(
                &location,
                &InstallOptions {
                    token: token.as_deref(),
                    git_ref: git_ref.as_deref(),
                },
            )
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| with_clone_hint(e.to_string(), &e, access))?;
        if located.kind() == ManifestKind::Wasm {
            return Err(wasm_skill_found(located.name(), origin, located.commit()));
        }
        let commit = located.commit().map(str::to_string);
        self.ctx
            .discovered_skills
            .install(
                caller,
                located,
                requested,
                crate::wasm_skill_tools::global_skill_store(),
            )
            .await
            .map(|installed| (installed, commit))
            .map_err(|e| e.to_string())
    }

    fn installed_output(
        &self,
        given: &str,
        installed: &Installed,
        commit: Option<String>,
    ) -> serde_json::Value {
        let path = installed.path.display().to_string();
        match self
            .ctx
            .discovered_skills
            .snapshot()
            .iter()
            .find(|s| s.discovered.path == installed.path)
        {
            Some(skill) => {
                let (meta, access) = (&skill.discovered.skill.meta, &skill.access);
                info!(
                    name = %meta.name,
                    version = %meta.version,
                    source = %given,
                    available = skill.discovered.is_available(),
                    replaced = installed.replaced,
                    owner = ?access.owner,
                    "External skill installed via system.install_external_skill"
                );
                serde_json::json!({
                    "installed": true,
                    "name": meta.name,
                    "version": meta.version,
                    "description": meta.description,
                    "author": meta.author,
                    "tools": meta.tools,
                    "available": skill.discovered.is_available(),
                    "gating": skill
                        .discovered
                        .unavailable_reason()
                        .unwrap_or_else(|| REQUIREMENTS_MET.to_string()),
                    "path": path,
                    "replaced": installed.replaced,
                    "previous_version": installed.previous_version,
                    "commit": commit,
                    "owner": access.owner,
                    "visibility": access.visibility.word(),
                    "shared_with": access.shares.len(),
                    "sharing": match access.owner {
                        None => format!(
                            "Every user can use `{}`: it was installed for the whole org.",
                            meta.name
                        ),
                        Some(_) => sharing_hint(&meta.name, access),
                    },
                })
            }
            None => serde_json::json!({
                "installed": true,
                "warning": "Installed, but it is not in the live skill list: its SKILL.md \
                            could not be read. See the hub log.",
                "path": path,
            }),
        }
    }
}

#[async_trait]
impl Tool for SystemInstallExternalSkillTool {
    fn name(&self) -> &str {
        "system.install_external_skill"
    }

    fn description(&self) -> &str {
        "Install or update an external skill: a directory with a SKILL.md whose instructions \
         agents load with system.enable_skill. Pass `github_repo` \
         (`owner/repo[@ref][#subpath=dir]`, the form wasm_skill.install takes) or `source` (a \
         skill directory in your workspace, such as one you have just written). GitHub \
         repositories are read with your GitHub token, so private ones install too. The skill \
         belongs to you, and only you and the people you share it with can use it, unless you \
         pass `visibility = \"everyone\"`; an install by a holder of skills_manage is for everyone \
         unless they pass `visibility = \"private\"`, and they may also pass any directory on the \
         hub or another git URL. Installing a skill again replaces it, keeping its config.toml, \
         owner and sharing, and the change is live when this returns. A skill a configured \
         catalog lists installs by name with system.install_catalog_skill instead. A WASM skill \
         (Skill.toml) installs with wasm_skill.install; pass one here and the reply gives that \
         call."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "github_repo": {
                    "type": "string",
                    "description": "GitHub repository: 'owner/repo', 'owner/repo@<branch, tag or 40-hex commit>', and '#subpath=<dir>' for a skill inside a larger repository; the forms combine, e.g. 'acme/skills@main#subpath=hello-world'. Without a ref the default branch is installed. Read with your GitHub token for the owner (skills/github/workspaces/<owner>/token, then skills/github/token)."
                },
                "source": {
                    "type": "string",
                    "description": "A skill directory in your workspace (a relative path resolves against it), or a git URL with '#subpath=<dir>' for a skill inside a larger repository. An https://github.com URL is read with your GitHub token. Other git hosts, and directories outside your workspace, need the skills_manage capability. Pass either source or github_repo, not both."
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "everyone"],
                    "description": "Who may use the skill: 'private' is you plus anyone you share it with, and is the default unless you hold skills_manage; 'team' is anyone in a team with you; 'everyone' is every user. Change it later with `cmd: skill visibility <skill> private|team|everyone`. Ignored when reinstalling a skill that has an owner other than you."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: InstallExternalInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let caller = SkillCaller::current(&self.ctx.skill_managers, &self.ctx.teams);
        let prepared = params.origin().and_then(|(given, origin)| {
            let requested = params.requested_visibility()?;
            let location = self.location(&caller, origin)?;
            Ok((given, origin, location, requested))
        });
        let (given, origin, location, requested) = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => return Ok(ToolResult::failure(refusal, duration())),
        };
        Ok(
            match self.install(caller, origin, location, requested).await {
                Ok((installed, commit)) => ToolResult::success(
                    self.installed_output(given, &installed, commit),
                    duration(),
                ),
                Err(reason) => ToolResult::failure(
                    format!("Failed to install skill from '{given}': {reason}"),
                    duration(),
                ),
            },
        )
    }
}

// ─── system.uninstall_external_skill ────────────────────────────────────────

pub struct SystemUninstallExternalSkillTool {
    ctx: SystemToolContext,
}

impl SystemUninstallExternalSkillTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UninstallExternalInput {
    name: String,
}

#[async_trait]
impl Tool for SystemUninstallExternalSkillTool {
    fn name(&self) -> &str {
        "system.uninstall_external_skill"
    }

    fn description(&self) -> &str {
        "Uninstall an external (SKILL.md) skill you manage, by name, however it was installed: \
         your own, one shared with you to manage, or, with the skills_manage capability, any. A \
         WASM skill is removed with wasm_skill.uninstall."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name to uninstall (from SKILL.md metadata)"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: UninstallExternalInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .discovered_skills
            .uninstall(
                SkillCaller::current(&self.ctx.skill_managers, &self.ctx.teams),
                params.name.clone(),
            )
            .await
        {
            Ok(()) => {
                info!(
                    name = %params.name,
                    "External skill uninstalled via system.uninstall_external_skill"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "uninstalled": true,
                        "name": params.name,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to uninstall skill '{}': {}", params.name, e),
                duration(),
            )),
        }
    }
}

// ─── system.list_catalog_skills ─────────────────────────────────────────────

pub struct SystemListCatalogSkillsTool {
    ctx: SystemToolContext,
}

impl SystemListCatalogSkillsTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ListCatalogInput {
    #[serde(default)]
    catalog: Option<String>,
}

#[async_trait]
impl Tool for SystemListCatalogSkillsTool {
    fn name(&self) -> &str {
        "system.list_catalog_skills"
    }

    fn description(&self) -> &str {
        "List the skills in this hub's configured skill catalogs — external (SKILL.md) and WASM — \
         with whether each is installed and at which version. Install or update one with \
         system.install_catalog_skill. Check here first when asked to install a skill by name."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "catalog": {
                    "type": "string",
                    "description": "Only list this catalog (a name from [[skills.catalogs]]); omit to list every catalog"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ListCatalogInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .skill_catalog()
            .list(params.catalog.as_deref())
            .await
        {
            Ok(listing) => Ok(ToolResult::success(
                serde_json::to_value(listing)?,
                duration(),
            )),
            Err(e) => Ok(ToolResult::failure(e.to_string(), duration())),
        }
    }
}

// ─── system.install_catalog_skill ───────────────────────────────────────────

pub struct SystemInstallCatalogSkillTool {
    ctx: SystemToolContext,
}

impl SystemInstallCatalogSkillTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct InstallCatalogInput {
    skill: String,
    #[serde(default)]
    catalog: Option<String>,
}

#[async_trait]
impl Tool for SystemInstallCatalogSkillTool {
    fn name(&self) -> &str {
        "system.install_catalog_skill"
    }

    fn description(&self) -> &str {
        "Install a skill from a configured skill catalog, or update an installed one to the \
         catalog's version. The skill is live for every agent as soon as this returns — no \
         restart. An updated external skill keeps its config.toml, owner and sharing; an updated \
         WASM skill keeps its config, owner and sharing. A skill no catalog lists installs from \
         its repository \
         instead: system.install_external_skill for a SKILL.md skill, wasm_skill.install for a \
         Skill.toml skill."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name from system.list_catalog_skills"
                },
                "catalog": {
                    "type": "string",
                    "description": "Catalog name from [[skills.catalogs]] (optional when only one is configured)"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: InstallCatalogInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let authority = InstallAuthority::Agent {
            caller: SkillCaller::current(&self.ctx.admin, &self.ctx.teams),
            policy: (&self.ctx.config.skills.wasm).into(),
        };
        match self
            .ctx
            .skill_catalog()
            .install(&authority, params.catalog.as_deref(), &params.skill)
            .await
        {
            Ok(outcome) => {
                info!(
                    skill = %params.skill,
                    catalog = ?params.catalog,
                    replaced = outcome.replaced,
                    active = outcome.active,
                    "Skill installed from catalog via system.install_catalog_skill"
                );
                Ok(ToolResult::success(
                    serde_json::to_value(outcome)?,
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to install '{}' from the catalog: {e}", params.skill),
                duration(),
            )),
        }
    }
}

// ─── system.list_external_skills ────────────────────────────────────────────

pub struct SystemListExternalSkillsTool {
    ctx: SystemToolContext,
}

impl SystemListExternalSkillsTool {
    pub fn new(ctx: SystemToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ListExternalInput {
    #[serde(default)]
    query: Option<String>,
}

#[async_trait]
impl Tool for SystemListExternalSkillsTool {
    fn name(&self) -> &str {
        "system.list_external_skills"
    }

    fn description(&self) -> &str {
        "List the external (SKILL.md) skills you can use now — your own, ones shared with you, \
         and ones for everyone — with their version, requirements, path, owner and visibility. \
         WASM skills are listed by wasm_skill.list, and what a catalog offers by \
         system.list_catalog_skills."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional search query to filter skills by name"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ListExternalInput = serde_json::from_value(input)?;
        let query = params.query.map(|q| q.to_lowercase());

        let result: Vec<serde_json::Value> = SkillCaller::current(&self.ctx.skill_managers, &self.ctx.teams)
            .visible(self.ctx.discovered_skills.as_ref())
            .into_iter()
            .filter(|(skill, _)| {
                let meta = &skill.discovered.skill.meta;
                query.as_deref().is_none_or(|q| {
                    meta.name.to_lowercase().contains(q)
                        || meta.description.to_lowercase().contains(q)
                        || meta.tools.iter().any(|t| t.to_lowercase().contains(q))
                })
            })
            .map(|(skill, access)| {
                let (discovered, record) = (&skill.discovered, &skill.access);
                let meta = &discovered.skill.meta;
                serde_json::json!({
                    "name": meta.name,
                    "version": meta.version,
                    "description": meta.description,
                    "author": meta.author,
                    "tools": meta.tools,
                    "available": discovered.is_available(),
                    "gating": discovered.unavailable_reason().unwrap_or_else(|| REQUIREMENTS_MET.to_string()),
                    "path": discovered.path.display().to_string(),
                    "body": discovered.skill.body,
                    "owner": record.owner,
                    "visibility": record.visibility.word(),
                    "shared_with": record.shares.len(),
                    "your_access": access.as_str(),
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "skills": result,
                "count": result.len(),
                "skills_dir": self.ctx.discovered_skills.skills_dir().display().to_string(),
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hub_core::capabilities::external_install_capability;
    use std::path::PathBuf;

    use crate::skill_catalog::tests::{catalog_repo, git, skill_md, COMMIT};

    // The admin gating for `system.{enable,disable,install,uninstall}_skill` and
    // `system.reload_slack` is no longer an inline check inside `execute()` — it
    // is enforced pre-dispatch by the `HubCapabilityGate` off the
    // `tool_capability` registry. The member-denied / admin-allowed adversarial
    // coverage lives in `crate::authz` (registry + `authorize`) and
    // `crate::cap_gate` (principal resolution), and the no-prompt short-circuit
    // in the runner tests. These tools therefore execute with no admin check of
    // their own.

    fn external_skill(
        name: &str,
        body: &str,
        gating_passed: bool,
    ) -> workforce_skills::scanner::DiscoveredSkill {
        let content =
            format!("---\nname: {name}\nversion: 1.2.3\ndescription: A test skill\n---\n{body}");
        workforce_skills::scanner::DiscoveredSkill {
            skill: workforce_skills::skill_file::SkillFile::parse(&content)
                .expect("valid SKILL.md"),
            path: std::path::PathBuf::from("/tmp/skills").join(name),
            gating: workforce_skills::gating::GatingResult {
                passed: gating_passed,
                checks: Vec::new(),
            },
            config_status: workforce_skills::config_loader::ConfigStatus::Unconfigured,
            security_findings: Vec::new(),
        }
    }

    #[test]
    fn external_skill_load_returns_full_procedure_when_available() {
        let skill = external_skill("figma", "Step 1: do the thing.", true);
        let result = external_skill_load_result(&skill, 0);
        assert!(result.success);
        let procedure = result.output["procedure"]
            .as_str()
            .expect("procedure string");
        assert!(procedure.contains("Loaded skill 'figma' v1.2.3"));
        assert!(procedure.contains("Step 1: do the thing."));
        assert_eq!(result.output["kind"].as_str(), Some("external"));
    }

    #[test]
    fn external_skill_load_reports_unmet_requirements() {
        let skill = external_skill("figma", "Step 1: do the thing.", false);
        let result = external_skill_load_result(&skill, 0);
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not ready"),
            "expected a requirements-not-met message, got {:?}",
            result.error
        );
    }

    #[test]
    fn skill_state_tracker_basic() {
        let tracker = SkillStateTracker::new();
        assert!(!tracker.is_loaded("github"));
        assert!(tracker.all_loaded().is_empty());

        tracker.mark_loaded(
            "github",
            vec!["github.list_repos".to_string()],
            "vault",
            None,
        );
        assert!(tracker.is_loaded("github"));
        assert_eq!(tracker.all_loaded().len(), 1);

        let state = tracker.get("github").unwrap();
        assert_eq!(state.source, "vault");
        assert_eq!(state.tool_names, vec!["github.list_repos"]);

        tracker.mark_unloaded("github");
        assert!(!tracker.is_loaded("github"));
        assert!(tracker.all_loaded().is_empty());
    }

    pub(crate) fn test_context(
        dir: &std::path::Path,
        config: WorkforceConfig,
    ) -> SystemToolContext {
        let skills_dir = dir.join("skills");
        SystemToolContext {
            vault: Arc::new(Vault::open(&dir.join("vault.enc"), "test-key").unwrap()),
            global_vault: None,
            tools: Arc::new(ToolRegistry::new()),
            skill_state: Arc::new(SkillStateTracker::new()),
            config,
            discovered_skills: Arc::new(crate::external_skills::ExternalSkills::new(
                skills_dir.clone(),
                vec![skills_dir],
            )),
            admin: crate::authz::always_admin(),
            teams: Arc::new(parking_lot::RwLock::new(None)),
            path_roots: Vec::new(),
            skill_managers: crate::authz::always_admin(),
        }
    }

    async fn run(ctx: &SystemToolContext, name: &str, input: serde_json::Value) -> ToolResult {
        system_tools(ctx.clone())
            .into_iter()
            .find(|tool| tool.name() == name)
            .expect("tool is registered")
            .execute(input)
            .await
            .unwrap()
    }

    fn write_skill(dir: &std::path::Path, name: &str, requires: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\nversion: 1.0.0\ndescription: A test skill\n{requires}---\n{body}\n"
            ),
        )
        .unwrap();
    }

    const SKILL_TOML: &str =
        "[skill]\nname = \"hello-counter\"\nversion = \"0.1.0\"\ndescription = \"Counts greetings\"\n";

    async fn install(ctx: &SystemToolContext, input: serde_json::Value) -> ToolResult {
        run(ctx, "system.install_external_skill", input).await
    }

    fn from_source(source: impl std::fmt::Display) -> serde_json::Value {
        serde_json::json!({ "source": source.to_string() })
    }

    fn is_live(ctx: &SystemToolContext, name: &str) -> bool {
        ctx.discovered_skills
            .snapshot()
            .iter()
            .any(|s| s.name() == name)
    }

    #[test]
    fn system_tools_include_the_catalog_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let names: Vec<String> = system_tools(test_context(tmp.path(), WorkforceConfig::default()))
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        for expected in [
            "system.list_catalog_skills",
            "system.install_catalog_skill",
            "system.install_external_skill",
            "system.uninstall_external_skill",
            "system.list_external_skills",
            "system.publish_external_skill",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} in {names:?}"
            );
        }
    }

    /// No agent tool decides who reaches an owned resource, and none gives one
    /// away. A tool runs inside a turn whose content can be injected, and a
    /// transfer is strictly worse there than a share: a share widens who
    /// reaches a resource while leaving its owner in control, while a transfer
    /// removes that control outright and the victim has no standing left to
    /// reverse it. Every such operation is therefore person-driven — REST,
    /// `cmd:`, the remote surface the user's own client calls, and the UI.
    ///
    /// The absence is the invariant, so it is asserted rather than assumed:
    /// symmetry with the other operations is not a reason to complete the set.
    #[tokio::test]
    async fn no_agent_tool_shares_publishes_or_hands_over_an_owned_resource() {
        let tmp = tempfile::tempdir().unwrap();
        let names: Vec<String> = system_tools(test_context(tmp.path(), WorkforceConfig::default()))
            .iter()
            .map(|tool| tool.name().to_string())
            .chain(
                crate::wasm_skill_tools::wasm_skill_tools(
                    crate::wasm_skill_tools::tests::test_ctx(tmp.path(), &tmp.path().join("store")),
                )
                .iter()
                .map(|tool| tool.name().to_string()),
            )
            .collect();
        for forbidden in ["transfer", "owner", "share", "unshare", "visibility"] {
            assert!(
                !names
                    .iter()
                    .any(|name| name.ends_with(&format!(".{forbidden}"))),
                "{forbidden} must stay off the agent surface: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn installed_external_skill_is_live_and_uninstall_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let source = tmp.path().join("source").join("hello-world");
        write_skill(&source, "hello-world", "", "Say hello to everyone.");

        let installed = run(
            &ctx,
            "system.install_external_skill",
            serde_json::json!({ "source": source.display().to_string() }),
        )
        .await;
        assert!(installed.success, "{:?}", installed.error);
        assert_eq!(installed.output["available"], true);
        assert_eq!(installed.output["gating"], REQUIREMENTS_MET);
        assert!(is_live(&ctx, "hello-world"));

        let enabled = run(
            &ctx,
            "system.enable_skill",
            serde_json::json!({ "skill": "hello-world" }),
        )
        .await;
        assert!(enabled.success, "{:?}", enabled.error);
        assert!(enabled.output["procedure"]
            .as_str()
            .unwrap_or_default()
            .contains("Say hello to everyone."));

        let uninstalled = run(
            &ctx,
            "system.uninstall_external_skill",
            serde_json::json!({ "name": "hello-world" }),
        )
        .await;
        assert!(uninstalled.success, "{:?}", uninstalled.error);
        assert!(!is_live(&ctx, "hello-world"));

        let gone = run(
            &ctx,
            "system.enable_skill",
            serde_json::json!({ "skill": "hello-world" }),
        )
        .await;
        assert!(!gone.success);
    }

    #[tokio::test]
    async fn reinstalling_an_external_skill_updates_it_and_keeps_config() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let source = tmp.path().join("source").join("hello-world");
        write_skill(&source, "hello-world", "", "Say hello.");

        let first = install(&ctx, from_source(source.display())).await;
        assert!(first.success, "{:?}", first.error);
        assert_eq!(first.output["replaced"], false);
        let installed = PathBuf::from(first.output["path"].as_str().unwrap());
        std::fs::write(installed.join("config.toml"), "greeting = \"hi\"\n").unwrap();
        std::fs::write(source.join("SKILL.md"), skill_md("hello-world", "2.0.0")).unwrap();

        let second = install(&ctx, from_source(source.display())).await;
        assert!(second.success, "{:?}", second.error);
        assert_eq!(second.output["replaced"], true);
        assert_eq!(second.output["previous_version"], "1.0.0");
        assert_eq!(second.output["version"], "2.0.0");
        assert_eq!(second.output["commit"], serde_json::Value::Null);
        assert_eq!(
            std::fs::read_to_string(installed.join("config.toml")).unwrap(),
            "greeting = \"hi\"\n"
        );
        assert!(ctx
            .discovered_skills
            .snapshot()
            .iter()
            .any(|s| s.name() == "hello-world" && s.discovered.skill.meta.version == "2.0.0"));
    }

    #[tokio::test]
    async fn installs_from_a_git_repository_subpath_and_reports_the_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let repo = catalog_repo(
            tmp.path(),
            &[(
                "skills/hello-world/SKILL.md",
                &skill_md("hello-world", "1.0.0"),
            )],
        );

        let installed = install(
            &ctx,
            from_source(format!("{}#subpath=skills/hello-world", repo.display())),
        )
        .await;
        assert!(installed.success, "{:?}", installed.error);
        assert_eq!(
            installed.output["commit"],
            git(&repo, &["rev-parse", "HEAD"])
        );
        assert_eq!(installed.output["replaced"], false);
        assert!(is_live(&ctx, "hello-world"));
        let path = PathBuf::from(installed.output["path"].as_str().unwrap());
        assert!(path.join("SKILL.md").is_file());
        assert!(!path.join(".git").exists());
    }

    #[tokio::test]
    async fn a_wasm_skill_is_refused_with_the_wasm_install_to_use() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let local = tmp.path().join("source").join("hello-counter");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("Skill.toml"), SKILL_TOML).unwrap();

        let from_dir = install(&ctx, from_source(local.display())).await;
        let refusal = from_dir.error.unwrap_or_default();
        assert!(refusal.contains("is a WASM skill"), "{refusal}");
        assert!(
            refusal.contains(&format!("project_path = \"{}\"", local.display())),
            "{refusal}"
        );

        let repo = catalog_repo(
            tmp.path(),
            &[("skills/hello-counter/Skill.toml", SKILL_TOML)],
        );
        let from_git = install(
            &ctx,
            from_source(format!("{}#subpath=skills/hello-counter", repo.display())),
        )
        .await;
        let refusal = from_git.error.unwrap_or_default();
        assert!(refusal.contains("is a WASM skill"), "{refusal}");
        assert!(refusal.contains("wasm_skill.install"), "{refusal}");
        assert!(!ctx.discovered_skills.skills_dir().exists());
    }

    #[tokio::test]
    async fn a_directory_with_both_manifests_installs_as_external() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let source = tmp.path().join("source").join("hello-world");
        write_skill(&source, "hello-world", "", "Say hello.");
        std::fs::write(source.join("Skill.toml"), SKILL_TOML).unwrap();

        let installed = install(&ctx, from_source(source.display())).await;
        assert!(installed.success, "{:?}", installed.error);
        assert_eq!(installed.output["name"], "hello-world");
        assert!(is_live(&ctx, "hello-world"));
    }

    #[tokio::test]
    async fn input_errors_say_what_to_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let repo = catalog_repo(
            tmp.path(),
            &[(
                "skills/hello-world/SKILL.md",
                &skill_md("hello-world", "1.0.0"),
            )],
        );

        for (input, expected) in [
            (
                serde_json::json!({ "github_repo": "acme/skills", "source": "/srv/skills/hello-world" }),
                "Pass either `github_repo` or `source`, not both.",
            ),
            (serde_json::json!({}), "Pass `github_repo`"),
            (
                serde_json::json!({ "github_repo": " ", "source": "" }),
                "Pass `github_repo`",
            ),
            (
                serde_json::json!({ "github_repo": "https://github.com/acme/skills" }),
                "Pass a git URL as `source`",
            ),
            (
                from_source("acme/skills#subpath=hello-world"),
                "pass it as `github_repo`",
            ),
            (
                from_source(format!("{}#subpath=missing", repo.display())),
                "Subpath 'missing' not found",
            ),
        ] {
            let result = install(&ctx, input.clone()).await;
            assert!(!result.success, "{input}");
            let error = result.error.unwrap_or_default();
            assert!(error.contains(expected), "{input}: {error}");
        }
        assert!(!ctx.discovered_skills.skills_dir().exists());
    }

    #[tokio::test]
    async fn a_skill_installed_under_another_directory_is_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        let local = tmp.path().join("source").join("my-dir");
        write_skill(&local, "hello-world", "", "Say hello.");
        let first = install(&ctx, from_source(local.display())).await;
        assert!(first.success, "{:?}", first.error);

        let repo = catalog_repo(
            tmp.path(),
            &[(
                "skills/hello-world/SKILL.md",
                &skill_md("hello-world", "1.0.0"),
            )],
        );
        let from_git = install(
            &ctx,
            from_source(format!("{}#subpath=skills/hello-world", repo.display())),
        )
        .await;
        let refusal = from_git.error.unwrap_or_default();
        assert!(refusal.contains("already installed in"), "{refusal}");
        assert!(refusal.contains("my-dir"), "{refusal}");
    }

    /// A context whose callers hold no skills_manage, with a workspace root.
    fn member_context(dir: &std::path::Path) -> SystemToolContext {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        SystemToolContext {
            path_roots: vec![workspace.canonicalize().unwrap()],
            skill_managers: crate::authz::never_admin(),
            ..test_context(dir, WorkforceConfig::default())
        }
    }

    async fn as_user<T>(uid: &str, work: impl std::future::Future<Output = T>) -> T {
        workforce_agent::runner::CURRENT_USER_ID
            .scope(uid.to_string(), work)
            .await
    }

    fn enable(skill: &str) -> serde_json::Value {
        serde_json::json!({ "skill": skill })
    }

    #[test]
    fn external_install_capability_prices_only_other_git_hosts_as_skills_manage() {
        use crate::authz::Capability::{SkillsAuthor, SkillsManage};
        for (input, expected) in [
            (serde_json::json!({}), SkillsAuthor),
            (serde_json::json!("not an object"), SkillsAuthor),
            (
                serde_json::json!({ "github_repo": "acme/skills", "source": "https://gitlab.example.com/acme/skills.git" }),
                SkillsAuthor,
            ),
            (
                serde_json::json!({ "github_repo": "acme/skills@main#subpath=hello-world" }),
                SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "https://github.com/acme/skills#subpath=hello-world" }),
                SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "skills/hello-world" }),
                SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "/srv/skills/hello-world" }),
                SkillsAuthor,
            ),
            (
                serde_json::json!({ "source": "https://gitlab.example.com/acme/skills.git#subpath=hello-world" }),
                SkillsManage,
            ),
            (
                serde_json::json!({ "github_repo": "https://github.com/acme/skills" }),
                SkillsAuthor,
            ),
            (
                serde_json::json!({ "github_repo": "acme/skills", "visibility": "public" }),
                SkillsAuthor,
            ),
        ] {
            assert_eq!(external_install_capability(&input), expected, "{input}");
        }
    }

    #[tokio::test]
    async fn a_member_installs_from_their_workspace_privately() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = member_context(tmp.path());
        write_skill(
            &ctx.path_roots[0].join("skills/hello-world"),
            "hello-world",
            "",
            "Say hello.",
        );

        let installed = as_user("alice", install(&ctx, from_source("skills/hello-world"))).await;
        assert!(installed.success, "{:?}", installed.error);
        assert_eq!(installed.output["owner"], "alice");
        assert_eq!(installed.output["visibility"], "private");
        assert_eq!(installed.output["shared_with"], 0);
        assert_eq!(
            installed.output["sharing"],
            "Only you can use `hello-world`. Share it with `cmd: skill share hello-world <user>` \
             (add `manage` to let them edit it), or publish it to everyone with \
             `cmd: skill visibility hello-world everyone`."
        );

        let loaded = as_user(
            "alice",
            run(&ctx, "system.enable_skill", enable("hello-world")),
        )
        .await;
        assert!(loaded.success, "{:?}", loaded.error);
        let refused = as_user(
            "bob",
            run(&ctx, "system.enable_skill", enable("hello-world")),
        )
        .await;
        let refusal = refused.error.unwrap_or_default();
        assert!(refusal.contains("private to another user"), "{refusal}");

        let listing = |uid: &'static str| {
            let ctx = ctx.clone();
            async move {
                as_user(uid, run(&ctx, "system.enable_skill", enable("nope")))
                    .await
                    .error
                    .unwrap_or_default()
            }
        };
        assert!(listing("alice").await.contains("hello-world"));
        let for_bob = listing("bob").await;
        assert!(for_bob.contains("Available skills:"), "{for_bob}");
        assert!(!for_bob.contains("hello-world"), "{for_bob}");
    }

    #[tokio::test]
    async fn a_member_source_is_confined_to_their_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = member_context(tmp.path());
        let outside = tmp.path().join("outside").join("hello-world");
        write_skill(&outside, "hello-world", "", "Say hello.");

        for source in [
            outside.display().to_string(),
            "../outside/hello-world".to_string(),
        ] {
            let refused = as_user("alice", install(&ctx, from_source(&source))).await;
            let error = refused.error.unwrap_or_default();
            assert!(
                error.starts_with(&format!("Path '{source}' resolves to ")),
                "{error}"
            );
            assert!(error.contains("outside the allowed directories"), "{error}");
        }

        let workspace = &ctx.path_roots[0];
        write_skill(
            &workspace.join("hello.git"),
            "hello-world",
            "",
            "Say hello.",
        );
        let refused = as_user("alice", install(&ctx, from_source("hello.git/"))).await;
        assert_eq!(
            refused.error,
            Some(format!(
                "`hello.git/` resolves to `{}`, which reads as a git URL rather than a directory. \
                 Pass a directory whose path does not end in `.git`.",
                workspace.join("hello.git").display()
            ))
        );
        assert!(!ctx.discovered_skills.skills_dir().exists());
    }

    #[tokio::test]
    async fn a_member_cannot_install_from_another_git_host() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = member_context(tmp.path());
        let url = "https://gitlab.example.com/acme/skills.git#subpath=hello-world";

        let refused = as_user("alice", install(&ctx, from_source(url))).await;
        assert_eq!(
            refused.error.as_deref(),
            Some(
                "`https://gitlab.example.com/acme/skills.git#subpath=hello-world` is on a git \
                 host other than GitHub, and installing from one needs the skills_manage \
                 capability (administrators by default). Pass a GitHub repository as \
                 `github_repo`, or save the skill in your workspace and pass that directory as \
                 `source`."
            )
        );
        let invalid = as_user(
            "alice",
            install(
                &ctx,
                serde_json::json!({ "source": "skills/hello-world", "visibility": "public" }),
            ),
        )
        .await;
        assert_eq!(
            invalid.error.as_deref(),
            Some("`public` is not a visibility — use `private`, `team` or `everyone`.")
        );
    }

    #[tokio::test]
    async fn list_external_skills_is_scoped_to_the_caller() {
        use crate::external_skills::tests::{owned, record_in, share};

        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        for name in ["mine", "carols", "hidden", "org"] {
            write_skill(&skills_dir.join(name), name, "", "Body.");
        }
        record_in(
            &skills_dir.join("mine"),
            &owned(
                Some("alice"),
                Visibility::Private,
                vec![share("bob", workforce_core::access::GrantLevel::Run)],
            ),
        );
        record_in(
            &skills_dir.join("carols"),
            &owned(Some("carol"), Visibility::Global, vec![]),
        );
        record_in(
            &skills_dir.join("hidden"),
            &owned(Some("carol"), Visibility::Private, vec![]),
        );
        let member = member_context(tmp.path());
        let manager = test_context(tmp.path(), WorkforceConfig::default());
        let listed = |output: serde_json::Value| {
            output["skills"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| {
                    (
                        s["name"].as_str().unwrap().to_string(),
                        s["your_access"].as_str().unwrap().to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let pair = |name: &str, access: &str| (name.to_string(), access.to_string());
        let list = serde_json::json!({});

        let alice = as_user(
            "alice",
            run(&member, "system.list_external_skills", list.clone()),
        )
        .await;
        assert_eq!(
            listed(alice.output.clone()),
            [
                pair("carols", "run"),
                pair("mine", "manage"),
                pair("org", "run")
            ]
        );
        let mine = alice.output["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "mine")
            .cloned()
            .unwrap();
        assert_eq!(mine["owner"], "alice");
        assert_eq!(mine["visibility"], "private");
        assert_eq!(mine["shared_with"], 1);

        let bob = as_user(
            "bob",
            run(&member, "system.list_external_skills", list.clone()),
        )
        .await;
        assert_eq!(
            listed(bob.output),
            [
                pair("carols", "run"),
                pair("mine", "run"),
                pair("org", "run")
            ]
        );
        let root = as_user(
            "root",
            run(&manager, "system.list_external_skills", list.clone()),
        )
        .await;
        assert_eq!(
            listed(root.output),
            [
                pair("carols", "manage"),
                pair("hidden", "manage"),
                pair("mine", "manage"),
                pair("org", "manage")
            ]
        );
        let nobody = run(&member, "system.list_external_skills", list).await;
        assert_eq!(
            listed(nobody.output),
            [pair("carols", "run"), pair("org", "manage")]
        );
    }

    #[tokio::test]
    async fn uninstall_external_skill_needs_manage() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = member_context(tmp.path());
        write_skill(
            &ctx.path_roots[0].join("skills/hello-world"),
            "hello-world",
            "",
            "Say hello.",
        );
        let installed = as_user("alice", install(&ctx, from_source("skills/hello-world"))).await;
        assert!(installed.success, "{:?}", installed.error);
        let uninstall = serde_json::json!({ "name": "hello-world" });

        let refused = as_user(
            "bob",
            run(&ctx, "system.uninstall_external_skill", uninstall.clone()),
        )
        .await;
        assert_eq!(
            refused.error.as_deref(),
            Some(
                "Failed to uninstall skill 'hello-world': Skill `hello-world` belongs to another \
                 user. Ask them to share it with you (`cmd: skill share hello-world <you> \
                 manage`)."
            )
        );
        assert!(is_live(&ctx, "hello-world"));

        let removed = as_user(
            "alice",
            run(&ctx, "system.uninstall_external_skill", uninstall),
        )
        .await;
        assert!(removed.success, "{:?}", removed.error);
        assert!(!is_live(&ctx, "hello-world"));
    }

    /// Skills installed before ownership existed have no access record, and
    /// every surface and caller must treat them exactly as before: for everyone,
    /// in every prompt, loadable by anyone.
    #[tokio::test]
    async fn a_legacy_skill_without_a_record_is_unchanged_for_every_caller() {
        use crate::external_skills::{may_read_config, may_read_instructions, AccessRecord};

        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let legacy = skills_dir.join("hello-world");
        write_skill(&legacy, "hello-world", "", "Say hello.");
        std::fs::write(legacy.join("config.toml"), "greeting = \"hi\"\n").unwrap();
        let member = member_context(tmp.path());
        let manager = test_context(tmp.path(), WorkforceConfig::default());
        assert_eq!(
            member
                .discovered_skills
                .installed("hello-world")
                .unwrap()
                .access,
            AccessRecord::org_wide()
        );

        for (ctx, uid, rest_caller) in [
            (
                &member,
                Some("alice"),
                SkillCaller::local(Some("alice".into()), false),
            ),
            (
                &manager,
                Some("root"),
                SkillCaller::local(Some("root".into()), true),
            ),
            (&member, None, SkillCaller::local(None, true)),
        ] {
            let skills = &ctx.discovered_skills;
            assert_eq!(
                skills
                    .reachable(&crate::team_reach::TeamReach::new(
                        None,
                        uid.map(str::to_string),
                    ))
                    .iter()
                    .map(|s| s.name().to_string())
                    .collect::<Vec<_>>(),
                ["hello-world"],
                "{uid:?}"
            );
            let (loaded, listed) = match uid {
                Some(uid) => {
                    as_user(uid, async {
                        (
                            run(ctx, "system.enable_skill", enable("hello-world")).await,
                            run(ctx, "system.list_external_skills", serde_json::json!({})).await,
                        )
                    })
                    .await
                }
                None => (
                    run(ctx, "system.enable_skill", enable("hello-world")).await,
                    run(ctx, "system.list_external_skills", serde_json::json!({})).await,
                ),
            };
            assert!(loaded.success, "{uid:?}: {:?}", loaded.error);
            assert!(loaded.output["procedure"]
                .as_str()
                .unwrap_or_default()
                .contains("Say hello."));
            let listed = &listed.output["skills"][0];
            assert_eq!(listed["name"], "hello-world", "{uid:?}");
            assert_eq!(listed["owner"], serde_json::Value::Null);
            assert_eq!(listed["visibility"], "everyone");
            assert!(may_read_instructions(&rest_caller, &legacy), "{uid:?}");
            assert!(may_read_config(&rest_caller, &legacy), "{uid:?}");
            assert!(
                crate::skill_loader::global_skills_list(&skills_dir, &rest_caller)
                    .iter()
                    .any(|skill| skill.name == "hello-world"),
                "{uid:?}"
            );
        }
        assert!(!legacy
            .join(workforce_skills::installer::ACCESS_FILE)
            .exists());
    }

    #[test]
    fn github_sources_read_with_the_owner_token_and_other_hosts_get_none() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        ctx.vault.set("skills/github/token", "flat").unwrap();
        ctx.vault
            .set("skills/github/workspaces/acme/token", "acme-token")
            .unwrap();
        let tool = SystemInstallExternalSkillTool::new(ctx);

        assert_eq!(
            tool.github_token(SkillOrigin::Github(GithubRef::parse(
                "acme/skills@main#subpath=hello-world"
            )))
            .as_deref(),
            Some("acme-token")
        );
        assert_eq!(
            tool.github_token(SkillOrigin::Github(GithubRef::parse("other/skills")))
                .as_deref(),
            Some("flat")
        );
        for (source, token) in [
            (
                "https://github.com/acme/skills.git#subpath=hello-world",
                Some("acme-token"),
            ),
            ("https://github.com/other/skills", Some("flat")),
            ("https://gitlab.example.com/acme/skills.git", None),
            ("https://github.com.evil.example/acme/skills.git", None),
            ("https://evil.example/github.com/acme/skills.git", None),
            ("https://github.com/acme/skills@evil.example", None),
            ("http://github.com/acme/skills.git", None),
            ("git@github.com:acme/skills.git", None),
            ("/srv/skills/hello-world", None),
        ] {
            let origin = SkillOrigin::from_source(source);
            assert_eq!(tool.github_token(origin).as_deref(), token, "{source}");
            assert!(
                token.is_none() || origin.locate_args().0.starts_with("https://github.com/"),
                "{source}"
            );
        }
    }

    #[tokio::test]
    async fn both_install_tools_read_github_with_the_same_token() {
        let tmp = tempfile::tempdir().unwrap();
        let global = Arc::new(Vault::open(&tmp.path().join("global.enc"), "test-key").unwrap());
        let ctx = SystemToolContext {
            global_vault: Some(Arc::clone(&global)),
            ..test_context(tmp.path(), WorkforceConfig::default())
        };
        global
            .set(
                "users/alice/skills/github/workspaces/acme/token",
                "alice-token",
            )
            .unwrap();
        ctx.vault
            .set("skills/github/workspaces/acme/token", "agent-token")
            .unwrap();
        global.set("skills/github/token", "global-token").unwrap();
        let wasm = crate::wasm_skill_tools::WasmSkillContext {
            agent_vault: Some(Arc::clone(&ctx.vault)),
            global_vault: ctx.global_vault.clone(),
            ..crate::wasm_skill_tools::tests::test_ctx(tmp.path(), &tmp.path().join("wasm-skills"))
        };
        let external = SystemInstallExternalSkillTool::new(ctx);
        let tokens = |reference: &str| {
            let repo = GithubRef::parse(reference);
            (
                external.github_token(SkillOrigin::Github(repo)),
                wasm.resolve_github_token(Some(repo.owner())),
            )
        };
        let both = |token: &str| (Some(token.to_string()), Some(token.to_string()));

        let as_alice = workforce_agent::runner::CURRENT_USER_ID
            .scope("alice".to_string(), async {
                (
                    tokens(&format!("acme/skills@{COMMIT}#subpath=hello-counter")),
                    tokens("other/skills"),
                )
            })
            .await;
        assert_eq!(as_alice, (both("alice-token"), both("global-token")));
        let as_bob = workforce_agent::runner::CURRENT_USER_ID
            .scope("bob".to_string(), async {
                tokens("acme/skills#subpath=hello-world")
            })
            .await;
        assert_eq!(as_bob, both("agent-token"));
        assert_eq!(tokens("acme/skills"), both("agent-token"));
    }

    #[test]
    fn external_install_schema_takes_github_repo_or_source() {
        let tmp = tempfile::tempdir().unwrap();
        let schema = SystemInstallExternalSkillTool::new(test_context(
            tmp.path(),
            WorkforceConfig::default(),
        ))
        .input_schema();
        assert!(schema["properties"]["github_repo"].is_object());
        assert!(schema["properties"]["source"].is_object());
        assert!(schema.get("required").is_none());
    }

    #[test]
    fn clone_failures_say_what_to_do_next() {
        let failed = eyre::Report::new(CloneFailed("fatal: repository not found".into()));
        let hinted = |access| with_clone_hint("GitHub fetch failed".to_string(), &failed, access);

        let tokenless = hinted(RepoAccess::Github {
            owner: "acme",
            token: false,
        });
        assert!(
            tokenless.starts_with("GitHub fetch failed\n"),
            "{tokenless}"
        );
        assert!(tokenless.contains("`skills/github/workspaces/acme/token`"));
        assert!(tokenless.contains("`skills/github/token`"));
        assert!(hinted(RepoAccess::Github {
            owner: "acme",
            token: true,
        })
        .contains("the token can read the repository"));
        assert!(hinted(RepoAccess::Anonymous).contains("github.com"));
        assert_eq!(
            with_clone_hint(
                "other".to_string(),
                &eyre::eyre!("other"),
                RepoAccess::Anonymous
            ),
            "other"
        );
    }

    #[test]
    fn wrong_kind_replies_give_the_call_to_make() {
        let wasm = wasm_skill_found(
            "hello-counter",
            SkillOrigin::Github(GithubRef::parse("acme/skills@main#subpath=hello-counter")),
            Some(COMMIT),
        );
        assert!(
            wasm.contains(&format!(
                "github_repo = \"acme/skills@{COMMIT}#subpath=hello-counter\""
            )),
            "{wasm}"
        );
        let reference = format!("acme/skills@{COMMIT}#subpath=hello-world");
        let external = external_skill_found("github_repo", &reference);
        assert!(
            external.contains("system.install_external_skill"),
            "{external}"
        );
        assert!(
            external.contains(&format!("github_repo = \"{reference}\"")),
            "{external}"
        );
    }

    #[test]
    fn install_guide_and_the_skills_page_agree() {
        let page = &crate::remote_mcp::help::resolve("skill").unwrap().markdown;
        for phrase in [
            "system.list_catalog_skills",
            "system.install_catalog_skill",
            "system.install_external_skill",
            "wasm_skill.install",
            "system.uninstall_external_skill",
            "wasm_skill.uninstall",
            "system.list_external_skills",
            "wasm_skill.list",
            "wasm_skill.publish",
            "skills/github/workspaces/<owner>/token",
            "skills/github/token",
            "system.publish_external_skill",
            "cmd: skill share",
            "cmd: skill visibility",
            "visibility = \"everyone\"",
            "skills_manage",
        ] {
            assert!(INSTALL_GUIDE.contains(phrase), "guide lost {phrase}");
            assert!(page.contains(phrase), "skills page lost {phrase}");
        }
    }

    #[tokio::test]
    async fn list_external_skills_reports_the_live_set() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir.join("hello-world"), "hello-world", "", "Hello.");
        write_skill(
            &skills_dir.join("unmet"),
            "unmet",
            "requires:\n  bins: [definitely_not_a_real_binary_12345]\n",
            "Never ready.",
        );
        let ctx = test_context(tmp.path(), WorkforceConfig::default());
        write_skill(
            &skills_dir.join("unrefreshed"),
            "unrefreshed",
            "",
            "Not yet live.",
        );

        let listed = run(&ctx, "system.list_external_skills", serde_json::json!({})).await;
        assert!(listed.success, "{:?}", listed.error);
        assert_eq!(listed.output["count"], 2);
        assert_eq!(
            listed.output["skills_dir"],
            skills_dir.display().to_string()
        );
        let skill = |name: &str| {
            listed.output["skills"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == name)
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(skill("hello-world")["gating"], REQUIREMENTS_MET);
        assert_eq!(skill("unmet")["available"], false);
        assert!(skill("unmet")["gating"]
            .as_str()
            .unwrap_or_default()
            .contains("definitely_not_a_real_binary_12345"));
        assert!(skill("unrefreshed").is_null());

        let filtered = run(
            &ctx,
            "system.list_external_skills",
            serde_json::json!({ "query": "hello" }),
        )
        .await;
        assert_eq!(filtered.output["count"], 1);
    }

    #[tokio::test]
    async fn catalog_tools_report_catalog_selection_without_fetching() {
        let unconfigured = tempfile::tempdir().unwrap();
        let ctx = test_context(unconfigured.path(), WorkforceConfig::default());
        let refused = run(
            &ctx,
            "system.install_catalog_skill",
            serde_json::json!({ "skill": "hello-world" }),
        )
        .await;
        assert!(!refused.success);
        assert!(
            refused
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("No [[skills.catalogs]]"),
            "{:?}",
            refused.error
        );
        let listed = run(&ctx, "system.list_catalog_skills", serde_json::json!({})).await;
        assert!(listed.success, "{:?}", listed.error);
        assert_eq!(listed.output["catalogs"], serde_json::json!([]));

        let configured = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.skills.catalogs = ["example", "acme"]
            .into_iter()
            .map(|name| workforce_common::settings::CatalogSource {
                name: name.into(),
                repo: "https://github.com/acme/skills".into(),
                git_ref: "main".into(),
                manifest: "workforce/catalog.json".into(),
            })
            .collect();
        let ctx = test_context(configured.path(), config);
        let ambiguous = run(
            &ctx,
            "system.install_catalog_skill",
            serde_json::json!({ "skill": "hello-world" }),
        )
        .await;
        assert!(
            ambiguous
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Several catalogs"),
            "{:?}",
            ambiguous.error
        );
        for name in ["system.install_catalog_skill", "system.list_catalog_skills"] {
            let unknown = run(
                &ctx,
                name,
                serde_json::json!({ "skill": "hello-world", "catalog": "missing" }),
            )
            .await;
            assert!(!unknown.success, "{name}");
            assert!(
                unknown
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("Unknown catalog 'missing'"),
                "{name}: {:?}",
                unknown.error
            );
        }
    }
}
