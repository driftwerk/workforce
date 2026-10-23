//! Agent-callable tools for project CRUD + repo update.
//!
//! Each tool captures `agent_id` at construction so call sites can't
//! accidentally address another agent's projects. Project names and repo
//! aliases are restricted to a safe character class to prevent path
//! traversal when the names round-trip into the on-disk layout.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_knowledge_service::KnowledgeService;
use workforce_projects::{Project, ProjectRepo, ProjectStore, RepoSource};

use crate::knowledge::{
    drop_repo_everywhere, index_layout, IndexMode, MaintenanceRunner, PassRunOutcome,
};
use crate::vault::Vault;

use super::fetcher::{read_status, Fetcher};
use super::layout;
use super::layout::WorkRoots;
use super::updater::ProjectUpdater;
use super::usage::{project_references_by_slug, ProjectReference};
use super::work_session;
use crate::code::worktree::WorktreeManager;

#[derive(Clone)]
pub struct ProjectsToolsContext {
    pub agent_id: String,
    pub roots: WorkRoots,
    pub store: Arc<dyn ProjectStore>,
    pub agent_vault: Option<Arc<Vault>>,
    pub global_vault: Option<Arc<Vault>>,
    pub update_concurrency: usize,
    /// Optional knowledge service. When present, `purge=true` on
    /// `projects.delete` and `projects.remove_repo` cascades into the
    /// graph + vector store so the index doesn't outlive the project.
    pub knowledge: Option<Arc<KnowledgeService>>,
    /// Optional shared maintenance runner. When present, `projects.index`
    /// is registered so agents can trigger a project-scoped index pass.
    /// Absent when the hub has no knowledge engine configured.
    pub runner: Option<Arc<MaintenanceRunner>>,
}

impl ProjectsToolsContext {
    /// Whether `path`, wherever links in the agent's tree lead, is the agent's
    /// configuration, which no project move or purge may touch.
    fn reaches_config(&self, path: &std::path::Path) -> bool {
        let reaches = workforce_agent::tools::config_guard::reaches_config(
            &layout::agent_root(&self.roots.data_dir, &self.agent_id),
            path,
        );
        if reaches {
            warn!(path = %path.display(), "Leaving a project path that leads onto configuration alone");
        }
        reaches
    }

    /// Resolve the GitHub PAT for a repo owned by `owner`.
    ///
    /// `CURRENT_USER_ID` is read at call time via the task-local set by
    /// `serve.rs::process_message` (and `claude_code_chat::dispatch_turn`);
    /// `ProjectsToolsContext` is per-agent and shared across users, so
    /// identity must not be cached on the context.
    fn resolve_github_token(&self, owner: &str) -> Option<String> {
        let agent = self.agent_vault.as_ref()?.clone();
        crate::user_config::LayeredUserVault::new(agent, self.global_vault.clone())
            .get_github_pat(owner)
    }

    /// Pre-resolve a `{owner -> PAT}` map covering every repo in `project`,
    /// in the calling task so the `CURRENT_USER_ID` task-local is still
    /// set. Downstream consumers (`ProjectUpdater`, `work_session::ensure`)
    /// take this map and look up per-repo without needing vault access.
    fn resolve_github_tokens_for_project(
        &self,
        project: &workforce_projects::Project,
    ) -> std::collections::HashMap<String, String> {
        super::work_session::resolve_github_tokens(
            project,
            self.agent_vault.as_ref(),
            self.global_vault.as_ref(),
        )
    }
}

pub fn project_tools(ctx: ProjectsToolsContext) -> Vec<Arc<dyn Tool>> {
    let runner = ctx.runner.clone();
    let ctx = Arc::new(ctx);
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ListProjectsTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(ShowProjectTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(CreateProjectTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(RenameProjectTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(DeleteProjectTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(AddRepoTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(RemoveRepoTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(UpdateProjectTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(OpenWorkspaceTool {
            ctx: Arc::clone(&ctx),
        }),
    ];
    if let Some(runner) = runner {
        tools.push(Arc::new(IndexProjectTool { ctx, runner }));
    }
    tools
}

/// Tool names this module registers, in order. `projects.index` is
/// always listed so the default policy applies whenever a hub
/// eventually wires a maintenance runner; the tool itself only
/// instantiates when `ProjectsToolsContext::runner` is `Some`.
pub const TOOL_NAMES: &[&str] = &[
    "projects.list",
    "projects.show",
    "projects.create",
    "projects.rename",
    "projects.delete",
    "projects.add_repo",
    "projects.remove_repo",
    "projects.update",
    "projects.open_workspace",
    "projects.index",
];

/// Default policy rules for the `projects.*` surface — all `allow`.
/// Project operations are checkout/pull/manifest edits inside the
/// agent's own data dir; gating them on human approval was tried and
/// proved actively harmful for automation flows. Operators who want
/// stricter gating (e.g. blocking `projects.delete` outside business
/// hours) can override with a higher-priority rule in their policy
/// YAML.
pub fn default_policies() -> Vec<workforce_policy::rules::PolicyRule> {
    use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};

    TOOL_NAMES
        .iter()
        .map(|tool| PolicyRule {
            name: format!("{tool}-allow-default"),
            scope: PolicyScope::Global,
            tool: (*tool).to_string(),
            action: RuleAction::Allow,
            conditions: Vec::new(),
            priority: 10,
            constitutional: true,
        })
        .collect()
}

// ─── projects.list ────────────────────────────────────────────────────

struct ListProjectsTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for ListProjectsTool {
    fn name(&self) -> &str {
        "projects.list"
    }
    fn description(&self) -> &str {
        "List this agent's projects with their repo aliases."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let projects = self.ctx.store.list(&self.ctx.agent_id)?;
        let summaries: Vec<serde_json::Value> = projects
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "repo_count": p.repos.len(),
                    "repos": p.repos.iter().map(|r| &r.alias).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(ToolResult::success(
            json!({ "projects": summaries }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── projects.show ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ShowInput {
    name: String,
}

struct ShowProjectTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for ShowProjectTool {
    fn name(&self) -> &str {
        "projects.show"
    }
    fn description(&self) -> &str {
        "Show a project's manifest plus on-disk git status (branch, dirty, head commit) for each repo."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Project name." }
            },
            "required": ["name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: ShowInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        let project = match self.ctx.store.get(&self.ctx.agent_id, &params.name)? {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    missing_project(&params.name),
                    elapsed(start),
                ))
            }
        };
        let mut repos = Vec::with_capacity(project.repos.len());
        for repo in &project.repos {
            let dest = layout::work_clone_dir(
                &self.ctx.roots.data_dir,
                &self.ctx.agent_id,
                &project.name,
                &repo.alias,
            );
            let status = read_status(&dest).await.unwrap_or_default();
            repos.push(json!({
                "alias": repo.alias,
                "source": repo.source,
                "default_branch": project.effective_default_branch(&repo.alias),
                "setup": project.effective_setup(&repo.alias),
                "status": status,
            }));
        }
        Ok(ToolResult::success(
            json!({
                "name": project.name,
                "defaults": project.defaults,
                "repos": repos,
                "created_at": project.created_at,
                "updated_at": project.updated_at,
            }),
            elapsed(start),
        ))
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

// ─── projects.create ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateInput {
    name: String,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    setup: Option<String>,
}

struct CreateProjectTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for CreateProjectTool {
    fn name(&self) -> &str {
        "projects.create"
    }
    fn description(&self) -> &str {
        "Create an empty project. Add repos with `projects.add_repo`, or leave it repo-less to use as a persistent scratch coding workspace (coding.session / projects.open_workspace)."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "default_branch": {
                    "type": "string",
                    "description": "Project-wide default branch; per-repo overrides win.",
                },
                "setup": {
                    "type": "string",
                    "description": "Shell command run once in each fresh worktree before coding starts (e.g. `pnpm install --frozen-lockfile`); per-repo overrides win.",
                },
            },
            "required": ["name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: CreateInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        if self
            .ctx
            .store
            .get(&self.ctx.agent_id, &params.name)?
            .is_some()
        {
            return Ok(ToolResult::failure(
                format!("project '{}' already exists", params.name),
                elapsed(start),
            ));
        }
        let mut project = Project::new(&self.ctx.agent_id, &params.name);
        project.defaults.default_branch = params.default_branch;
        project.defaults.setup = non_blank(params.setup);
        self.ctx.store.put(&project)?;
        Ok(ToolResult::success(
            json!({ "name": project.name, "agent_id": project.agent_id }),
            elapsed(start),
        ))
    }
}

// ─── projects.rename ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct RenameInput {
    name: String,
    new_name: String,
}

struct RenameProjectTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for RenameProjectTool {
    fn name(&self) -> &str {
        "projects.rename"
    }
    fn description(&self) -> &str {
        "Rename a project. Fails if `new_name` already exists for this \
         agent. Repos, defaults, and `created_at` are preserved; work \
         clones move with the project root. cc-session worktrees are \
         torn down (their `.git/config` origin URLs encode the old \
         path and can't be rewritten safely). Active /cc threads bound \
         to the old project name will fail with 'project not found' \
         until restarted with the new name — rename should be a \
         deliberate operation, not done while threads are in flight."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Current project name." },
                "new_name": { "type": "string", "description": "New project name." },
            },
            "required": ["name", "new_name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: RenameInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        if let Err(e) = validate_safe_name(&params.new_name, "new project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        if params.name == params.new_name {
            return Ok(ToolResult::failure(
                "name and new_name are identical".to_string(),
                elapsed(start),
            ));
        }

        // Refuse early if the target on-disk dir already exists. This
        // prevents the post-store rename from silently merging the new
        // project into an unrelated dir tree (manual edits, leftover
        // state from a previous half-rename). Doing the check before
        // we mutate the store keeps "manifest renamed but disk didn't
        // move" out of the failure modes the user has to reason about.
        let new_root = layout::project_root(
            &self.ctx.roots.data_dir,
            &self.ctx.agent_id,
            &params.new_name,
        );
        if new_root.exists() {
            return Ok(ToolResult::failure(
                format!(
                    "on-disk dir for '{}' already exists at {}; resolve before retrying",
                    params.new_name,
                    new_root.display()
                ),
                elapsed(start),
            ));
        }

        // Atomic at the manifest layer. A concurrent rename racing with
        // us into the same `new_name` either loses (sees our row) or
        // wins (we see theirs); either way the trait/postgres
        // implementations enforce the conflict semantic.
        match self
            .ctx
            .store
            .rename(&self.ctx.agent_id, &params.name, &params.new_name)
        {
            Ok(true) => {}
            Ok(false) => {
                return Ok(ToolResult::failure(
                    missing_project(&params.name),
                    elapsed(start),
                ));
            }
            Err(e) => {
                return Ok(ToolResult::failure(e.to_string(), elapsed(start)));
            }
        }

        // Disk move is best-effort: the source dir may not exist (a
        // freshly created project that's never been updated or had a
        // /cc session). If a rename fails post-manifest-rename, the
        // next `projects.update` re-clones into the new dir — wasteful
        // but not destructive, so we warn and keep going rather than
        // rolling back the manifest.
        let old_root =
            layout::project_root(&self.ctx.roots.data_dir, &self.ctx.agent_id, &params.name);
        let mut moved_path: Option<String> = None;
        let mut cc_sessions_reset = false;
        if old_root.exists()
            && !self.ctx.reaches_config(&old_root)
            && !self.ctx.reaches_config(&new_root)
        {
            if let Some(parent) = new_root.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    warn!(
                        path = %parent.display(), error = %e,
                        "rename: failed to create parent dir for new project; \
                         on-disk move skipped",
                    );
                }
            }
            match tokio::fs::rename(&old_root, &new_root).await {
                Ok(()) => {
                    moved_path = Some(new_root.to_string_lossy().to_string());

                    // cc-session worktrees were `git clone --local`d from
                    // the work-clone path under the OLD project root, so
                    // their `.git/config` records that absolute path as
                    // `remote.origin.url`. The work clones moved with us,
                    // so those URLs no longer resolve and the next
                    // `git fetch origin` from the cc-session would fail.
                    // Wipe the cc-sessions tree so `cc_session::ensure`
                    // re-clones from the now-current work-clone paths on
                    // the next /cc turn. Active threads lose any
                    // uncommitted edits — that's the rename's cost, and
                    // why this should be a deliberate operation.
                    let cc_sessions = new_root.join("cc-sessions");
                    if cc_sessions.exists() {
                        if let Err(e) = tokio::fs::remove_dir_all(&cc_sessions).await {
                            warn!(
                                path = %cc_sessions.display(), error = %e,
                                "rename: failed to clear cc-sessions; stale origin URLs \
                                 will surface on the next /cc turn — delete the dir manually \
                                 to recover",
                            );
                        } else {
                            cc_sessions_reset = true;
                        }
                    }
                }
                Err(e) => warn!(
                    from = %old_root.display(),
                    to = %new_root.display(),
                    error = %e,
                    "rename: on-disk move failed; manifest rename stands, next \
                     `projects.update` will re-clone into the new path",
                ),
            }
        }

        Ok(ToolResult::success(
            json!({
                "renamed": true,
                "name": params.new_name,
                "previous_name": params.name,
                "moved_path": moved_path,
                "cc_sessions_reset": cc_sessions_reset,
            }),
            elapsed(start),
        ))
    }
}

// ─── projects.delete ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeleteInput {
    name: String,
    /// Also remove all on-disk areas (`work/`, `index/`, `cc-sessions/`).
    /// Defaults to false: a manifest delete keeps the cached git data
    /// around in case it was a mistake.
    #[serde(default)]
    purge: bool,
    /// DW-509: when true (default), include `shared_repos` (still
    /// referenced by sibling projects after this delete) and
    /// `orphaned_repos` (whose cascade just fired) in the response so
    /// the user can see which indexes survived purge and which need
    /// follow-up cleanup. Set false for noisy automation.
    #[serde(default = "default_true")]
    warn: bool,
}

struct DeleteProjectTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for DeleteProjectTool {
    fn name(&self) -> &str {
        "projects.delete"
    }
    fn description(&self) -> &str {
        "Delete a project's manifest. Pass `purge: true` to also delete its on-disk clones, \
         indexer copy, and any cc-session workspaces. Pass `warn: false` to suppress \
         the `shared_repos` / `orphaned_repos` audit fields in the response."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "purge": { "type": "boolean", "description": "Remove on-disk files too." },
                "warn": { "type": "boolean", "description": "Default true; report repos whose indexes are shared with sibling projects vs newly orphaned." },
            },
            "required": ["name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: DeleteInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        // Snapshot the project BEFORE deleting so we still know its
        // aliases when we cascade index + indexer-checkout cleanup.
        let project_snapshot = self.ctx.store.get(&self.ctx.agent_id, &params.name)?;
        let deleted = self.ctx.store.delete(&self.ctx.agent_id, &params.name)?;
        if !deleted {
            return Ok(ToolResult::failure(
                missing_project(&params.name),
                elapsed(start),
            ));
        }
        let mut purged_path = None;
        let mut index_purged: Vec<serde_json::Value> = Vec::new();
        let mut shared_repos: Vec<serde_json::Value> = Vec::new();
        let mut orphaned_repos: Vec<serde_json::Value> = Vec::new();
        if let Some(project) = project_snapshot {
            // Compute live usage AFTER deleting the project so a
            // sibling project that names the same upstream still
            // pins the shared checkout, and so the `shared_repos`
            // warning shows the post-delete state.
            let by_slug = project_references_by_slug(self.ctx.store.as_ref())?;
            let mut visited_slugs = std::collections::HashSet::new();
            for repo in &project.repos {
                let slug = repo.source.canonical_slug();
                // A project may legally bind two aliases to the same
                // upstream — `add_repo` dedups by alias, not by source.
                // Without this guard the cascade would fire twice for
                // the same shared dir (second drop is a no-op, second
                // remove_dir_all warns on a missing path) and the
                // audit lists would carry duplicate entries.
                if !visited_slugs.insert(slug.clone()) {
                    continue;
                }
                let referrers = by_slug.get(&slug);
                let is_shared = referrers.is_some_and(|refs| !refs.is_empty());

                if params.purge && !is_shared {
                    let index_root = index_layout::repo_dir(&self.ctx.roots.data_dir, &repo.source);
                    let repo_id = KnowledgeService::repo_id_for(&index_root);
                    if let Some(ks) = &self.ctx.knowledge {
                        let mut entry = drop_repo_everywhere(ks, &repo_id).await;
                        if let Some(obj) = entry.as_object_mut() {
                            obj.insert("alias".into(), json!(repo.alias));
                        }
                        index_purged.push(entry);
                    }
                    if index_root.exists() {
                        if let Err(e) = tokio::fs::remove_dir_all(&index_root).await {
                            warn!(
                                path = %index_root.display(), error = %e,
                                "failed to purge indexer checkout",
                            );
                        }
                    }
                }

                // Audit fields report the post-delete partition
                // unconditionally — `purge=false` users still want to
                // see "deleting this project would orphan repo X" so
                // they can decide whether to follow up with a manual
                // `knowledge.delete`.
                if params.warn {
                    if is_shared {
                        let used_by: Vec<&ProjectReference> = referrers
                            .map(|refs| refs.iter().collect())
                            .unwrap_or_default();
                        shared_repos.push(json!({
                            "alias": repo.alias,
                            "canonical_slug": slug,
                            "used_by": used_by,
                        }));
                    } else {
                        orphaned_repos.push(json!({
                            "alias": repo.alias,
                            "canonical_slug": slug,
                        }));
                    }
                }
            }
            if params.purge {
                let root = layout::project_root(
                    &self.ctx.roots.data_dir,
                    &self.ctx.agent_id,
                    &params.name,
                );
                if root.exists() && !self.ctx.reaches_config(&root) {
                    if let Err(e) = tokio::fs::remove_dir_all(&root).await {
                        warn!(path = %root.display(), error = %e, "failed to purge project dir");
                    }
                    purged_path = Some(root.to_string_lossy().to_string());
                }
            }
        }
        let mut output = json!({
            "deleted": true,
            "purged": purged_path,
            "index_purged": index_purged,
        });
        if params.warn {
            if let Some(obj) = output.as_object_mut() {
                obj.insert("shared_repos".into(), json!(shared_repos));
                obj.insert("orphaned_repos".into(), json!(orphaned_repos));
            }
        }
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

// ─── projects.add_repo ────────────────────────────────────────────────

#[derive(Deserialize)]
struct AddRepoInput {
    name: String,
    alias: String,
    source: RepoSource,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    setup: Option<String>,
    /// Run the initial `git clone` into `work/clones/{alias}` immediately
    /// after the manifest update. Defaults to true.
    #[serde(default = "default_true")]
    clone: bool,
}

fn default_true() -> bool {
    true
}

struct AddRepoTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for AddRepoTool {
    fn name(&self) -> &str {
        "projects.add_repo"
    }
    fn description(&self) -> &str {
        "Add or replace a repo in a project. Optionally clones the repo immediately."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Project name." },
                "alias": { "type": "string", "description": "Project-local short name for the repo." },
                "source": {
                    "type": "object",
                    "description": "Upstream repo. v1 supports kind=\"github\".",
                    "properties": {
                        "kind": { "type": "string", "enum": ["github"] },
                        "owner": { "type": "string" },
                        "repo": { "type": "string" },
                    },
                    "required": ["kind", "owner", "repo"],
                },
                "default_branch": { "type": "string" },
                "setup": {
                    "type": "string",
                    "description": "Shell command run once in this repo's fresh worktree before coding starts; overrides the project default.",
                },
                "clone": { "type": "boolean", "description": "Default true." },
            },
            "required": ["name", "alias", "source"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: AddRepoInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        if let Err(e) = validate_safe_name(&params.alias, "repo alias") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        let repo = ProjectRepo {
            alias: params.alias.clone(),
            source: params.source,
            default_branch: params.default_branch,
            setup: non_blank(params.setup),
        };
        self.ctx
            .store
            .add_repo(&self.ctx.agent_id, &params.name, repo.clone())?;

        if !params.clone {
            return Ok(ToolResult::success(
                json!({ "alias": params.alias, "cloned": false }),
                elapsed(start),
            ));
        }

        let project = self
            .ctx
            .store
            .get(&self.ctx.agent_id, &params.name)?
            .ok_or_else(|| eyre::eyre!("{}", missing_project(&params.name)))?;
        let branch = project.effective_default_branch(&params.alias).to_string();
        let dest = layout::work_clone_dir(
            &self.ctx.roots.data_dir,
            &self.ctx.agent_id,
            &params.name,
            &params.alias,
        );

        if dest.exists() {
            return Ok(ToolResult::success(
                json!({
                    "alias": params.alias,
                    "cloned": false,
                    "note": "destination already exists",
                }),
                elapsed(start),
            ));
        }

        let RepoSource::Github { owner, .. } = &repo.source;
        let token = self.ctx.resolve_github_token(owner);
        let fetcher = Fetcher::new(token);
        match fetcher.clone_repo(&repo.source, &dest, &branch).await {
            Ok(()) => Ok(ToolResult::success(
                json!({
                    "alias": params.alias,
                    "cloned": true,
                    "branch": branch,
                    "path": dest.to_string_lossy(),
                }),
                elapsed(start),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("manifest updated but clone failed: {e}"),
                elapsed(start),
            )),
        }
    }
}

// ─── projects.remove_repo ─────────────────────────────────────────────

#[derive(Deserialize)]
struct RemoveRepoInput {
    name: String,
    alias: String,
    /// Also remove the on-disk clone.
    #[serde(default)]
    purge: bool,
    /// DW-509: when true (default), report whether the index for this
    /// repo survives (`shared_with` lists sibling projects that still
    /// pin it) or was just orphaned (`orphaned: true`). Set false for
    /// noisy automation.
    #[serde(default = "default_true")]
    warn: bool,
}

struct RemoveRepoTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for RemoveRepoTool {
    fn name(&self) -> &str {
        "projects.remove_repo"
    }
    fn description(&self) -> &str {
        "Remove a repo from a project's manifest. Pass `purge: true` to also delete its working clone. \
         Pass `warn: false` to suppress the `shared_with` / `orphaned` audit fields in the response."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "alias": { "type": "string" },
                "purge": { "type": "boolean" },
                "warn": { "type": "boolean", "description": "Default true; report sibling projects pinning this repo's shared index." },
            },
            "required": ["name", "alias"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: RemoveRepoInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        if let Err(e) = validate_safe_name(&params.alias, "repo alias") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        // Resolve the upstream source from the project manifest BEFORE
        // we mutate state so the canonical-path derivation lines up
        // with the store snapshot the indexer last enumerated. Always
        // capture so DW-509 warnings can populate even on
        // `purge=false`.
        let source_for_lookup = self
            .ctx
            .store
            .get(&self.ctx.agent_id, &params.name)?
            .and_then(|p| p.repo(&params.alias).map(|r| r.source.clone()));

        let removed =
            self.ctx
                .store
                .remove_repo(&self.ctx.agent_id, &params.name, &params.alias)?;
        if !removed {
            return Ok(ToolResult::failure(
                format!(
                    "alias '{}' not found in project '{}'",
                    params.alias, params.name
                ),
                elapsed(start),
            ));
        }
        let mut purged_path = None;
        let mut index_purged = None;
        let mut shared_with: Vec<ProjectReference> = Vec::new();
        let mut orphaned = false;
        if let Some(source) = source_for_lookup.as_ref() {
            // Compute live usage AFTER mutation so the warning matches
            // the post-remove state.
            let by_slug = project_references_by_slug(self.ctx.store.as_ref())?;
            let referrers = by_slug.get(&source.canonical_slug());
            let referrers_vec: Vec<ProjectReference> = referrers.cloned().unwrap_or_default();
            orphaned = referrers_vec.is_empty();
            shared_with = referrers_vec;

            if params.purge && orphaned {
                let index_root = index_layout::repo_dir(&self.ctx.roots.data_dir, source);
                let repo_id = KnowledgeService::repo_id_for(&index_root);
                if let Some(ks) = &self.ctx.knowledge {
                    index_purged = Some(drop_repo_everywhere(ks, &repo_id).await);
                }
                if index_root.exists() {
                    if let Err(e) = tokio::fs::remove_dir_all(&index_root).await {
                        warn!(
                            path = %index_root.display(), error = %e,
                            "failed to purge indexer checkout",
                        );
                    }
                }
            }
        }
        if params.purge {
            let dest = layout::work_clone_dir(
                &self.ctx.roots.data_dir,
                &self.ctx.agent_id,
                &params.name,
                &params.alias,
            );
            if dest.exists() && !self.ctx.reaches_config(&dest) {
                if let Err(e) = tokio::fs::remove_dir_all(&dest).await {
                    warn!(path = %dest.display(), error = %e, "failed to purge repo clone");
                }
                purged_path = Some(dest.to_string_lossy().to_string());
            }
        }
        let mut output = json!({
            "removed": true,
            "purged": purged_path,
            "index_purged": index_purged,
        });
        if params.warn {
            if let Some(obj) = output.as_object_mut() {
                obj.insert("shared_with".into(), json!(shared_with));
                obj.insert("orphaned".into(), json!(orphaned));
            }
        }
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

// ─── projects.update ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct UpdateInput {
    name: String,
}

struct UpdateProjectTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for UpdateProjectTool {
    fn name(&self) -> &str {
        "projects.update"
    }
    fn description(&self) -> &str {
        "Pull the latest commits on every repo in a project (parallel). Returns one result per repo."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: UpdateInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        let mut project = match self.ctx.store.get(&self.ctx.agent_id, &params.name)? {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    missing_project(&params.name),
                    elapsed(start),
                ))
            }
        };
        let tokens = self.ctx.resolve_github_tokens_for_project(&project);
        let updater = ProjectUpdater::new(
            tokens,
            self.ctx.roots.data_dir.clone(),
            self.ctx.update_concurrency,
        );
        let reports = updater.update(&project).await?;
        project.updated_at = Utc::now();
        self.ctx.store.put(&project)?;
        Ok(ToolResult::success(
            json!({ "name": params.name, "repos": reports }),
            elapsed(start),
        ))
    }
}

// ─── projects.open_workspace ──────────────────────────────────────────

#[derive(Deserialize)]
struct OpenWorkspaceInput {
    name: String,
    /// Optional explicit scope id. Empty / missing → derived from the
    /// current session key (`CURRENT_SESSION_KEY` task-local). Workflows
    /// (`workflow:{id}:{step}`) collapse to a single `wf-{id}` so every
    /// step in a run shares one worktree set; DM/channel sessions get a
    /// stable hash-derived id.
    #[serde(default)]
    scope_id: Option<String>,
}

struct OpenWorkspaceTool {
    ctx: Arc<ProjectsToolsContext>,
}

#[async_trait]
impl Tool for OpenWorkspaceTool {
    fn name(&self) -> &str {
        "projects.open_workspace"
    }
    fn description(&self) -> &str {
        "Materialise a per-scope working tree for every repo in a project (siblings under one parent dir), or an empty persistent scratch dir for a project with no repos. Returns the parent path; agents should `cd` there to operate across repos. Idempotent: subsequent calls in the same scope return the same dir."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Project name." },
                "scope_id": {
                    "type": "string",
                    "description": "Optional explicit scope id. Defaults to one derived from the current session.",
                },
            },
            "required": ["name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: OpenWorkspaceInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&params.name, "project name") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        let scope_id = match resolve_scope_id(params.scope_id.as_deref()) {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        if let Err(e) = validate_safe_name(&scope_id, "scope id") {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }
        let project = match self.ctx.store.get(&self.ctx.agent_id, &params.name)? {
            Some(p) => p,
            None => {
                return Ok(ToolResult::failure(
                    missing_project(&params.name),
                    elapsed(start),
                ))
            }
        };
        let tokens = self.ctx.resolve_github_tokens_for_project(&project);
        let parent = match work_session::ensure(
            &self.ctx.roots,
            &self.ctx.agent_id,
            &project,
            &scope_id,
            &tokens,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let aliases: Vec<&str> = project.repos.iter().map(|r| r.alias.as_str()).collect();
        Ok(ToolResult::success(
            json!({
                "project": project.name,
                "scope_id": scope_id,
                "parent_dir": parent.to_string_lossy(),
                "repos": aliases,
            }),
            elapsed(start),
        ))
    }
}

/// Resolve a scope id from an explicit override or the current session key.
/// Returning an error rather than falling back to a random id keeps the
/// per-scope guarantee meaningful: a scope is the *workflow run* or *chat
/// thread* the tool fires in, not "whatever the kernel rolled this time".
fn resolve_scope_id(explicit: Option<&str>) -> std::result::Result<String, String> {
    if let Some(s) = explicit {
        if !s.is_empty() {
            return Ok(s.to_string());
        }
    }
    let key = workforce_agent::runner::CURRENT_SESSION_KEY
        .try_with(|k| k.clone())
        .ok();
    match key {
        Some(k) if !k.is_empty() => Ok(WorktreeManager::scope_id_for_session(&k)),
        _ => Err(
            "no current session — pass `scope_id` explicitly when calling outside a session".into(),
        ),
    }
}

// ─── projects.index ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct IndexInput {
    name: String,
    #[serde(default)]
    mode: Option<String>,
}

struct IndexProjectTool {
    ctx: Arc<ProjectsToolsContext>,
    runner: Arc<MaintenanceRunner>,
}

#[async_trait]
impl Tool for IndexProjectTool {
    fn name(&self) -> &str {
        "projects.index"
    }
    fn description(&self) -> &str {
        "Start indexing every repo in one of your projects in the background and return immediately. Pulls latest source, then runs the indexer (incremental by default; pass mode='full' to force a complete reindex). Per-repo failures don't abort siblings. Returns outcome:\"started\" with the initial queue — the pass is NOT finished on return: poll knowledge.status to track progress and confirm completion/failures, and don't report success until status shows it finished. Returns outcome:\"already_in_flight\" when a pass is already running. Use projects.list to see your project names. Use knowledge.refresh_index to re-index everything across all projects."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Project name." },
                "mode": {
                    "type": "string",
                    "enum": ["incremental", "full"],
                    "description": "'incremental' (default) reuses prior index when possible; 'full' rebuilds.",
                },
            },
            "required": ["name"],
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: IndexInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let mode = match parse_index_mode(params.mode.as_deref()) {
            Ok(m) => m,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        let outcome = match self
            .runner
            .start_pass_for_project(&self.ctx.agent_id, &params.name, mode)
            .await
        {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let body = match outcome {
            PassRunOutcome::Started { message, snapshot } => json!({
                "outcome": "started",
                "project": params.name,
                "agent_id": self.ctx.agent_id,
                "mode": snapshot.mode,
                "target_count": snapshot.targets.len(),
                "targets": snapshot.targets,
                "message": message,
            }),
            PassRunOutcome::AlreadyInFlight { message, in_flight } => json!({
                "outcome": "already_in_flight",
                "project": params.name,
                "agent_id": self.ctx.agent_id,
                "message": message,
                "in_flight": in_flight,
            }),
            // `start_pass_for_project` only returns `Started` / `AlreadyInFlight`,
            // never `Completed` (that's the blocking `run_pass_for_project`), but
            // the match must stay exhaustive.
            PassRunOutcome::Completed(report) => json!({
                "outcome": "completed",
                "project": params.name,
                "agent_id": self.ctx.agent_id,
                "mode": report.mode,
                "target_count": report.target_count,
                "duration_ms": report.duration_ms,
                "targets": report.targets,
            }),
        };
        Ok(ToolResult::success(body, elapsed(start)))
    }
}

fn parse_index_mode(s: Option<&str>) -> std::result::Result<IndexMode, String> {
    match s.unwrap_or("incremental") {
        "incremental" => Ok(IndexMode::Incremental),
        "full" => Ok(IndexMode::Full),
        other => Err(format!(
            "invalid mode '{other}'; expected 'incremental' or 'full'"
        )),
    }
}

// ─── helpers ──────────────────────────────────────────────────────────

fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn missing_project(name: &str) -> String {
    format!("project '{name}' not found")
}

/// Reject inputs that would break out of the project's on-disk layout when
/// concatenated as path segments. The character class is `[A-Za-z0-9._-]`
/// with no leading dot and a 1-64 byte length; that excludes `/`, `..`,
/// hidden-file forms, and absolute paths.
fn validate_safe_name(label: &str, kind: &str) -> std::result::Result<(), String> {
    if label.is_empty() || label.len() > 64 {
        return Err(format!("{kind} must be 1-64 characters"));
    }
    if label.starts_with('.') {
        return Err(format!("{kind} must not start with '.'"));
    }
    let ok = label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    if !ok {
        return Err(format!(
            "{kind} may only contain letters, digits, '_', '-', '.'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workforce_projects::sqlite::SqliteProjectStore;

    fn ctx(tmp: &TempDir) -> ProjectsToolsContext {
        ProjectsToolsContext {
            agent_id: "allan".into(),
            roots: WorkRoots::persistent(tmp.path()),
            store: Arc::new(SqliteProjectStore::in_memory().unwrap()),
            agent_vault: None,
            global_vault: None,
            update_concurrency: 2,
            knowledge: None,
            runner: None,
        }
    }

    fn ctx_with_knowledge(tmp: &TempDir, ks: Arc<KnowledgeService>) -> ProjectsToolsContext {
        ProjectsToolsContext {
            agent_id: "allan".into(),
            roots: WorkRoots::persistent(tmp.path()),
            store: Arc::new(SqliteProjectStore::in_memory().unwrap()),
            agent_vault: None,
            global_vault: None,
            update_concurrency: 2,
            knowledge: Some(ks),
            runner: None,
        }
    }

    #[tokio::test]
    async fn list_returns_empty_for_new_agent() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let list = tools.iter().find(|t| t.name() == "projects.list").unwrap();
        let res = list.execute(json!({})).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["projects"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_then_list_round_trip() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        let res = create.execute(json!({ "name": "harbor" })).await.unwrap();
        assert!(res.success);

        let list = tools.iter().find(|t| t.name() == "projects.list").unwrap();
        let res = list.execute(json!({})).await.unwrap();
        let arr = res.output["projects"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "harbor");
        assert_eq!(arr[0]["repo_count"], 0);
    }

    #[tokio::test]
    async fn setup_is_stored_per_project_and_per_repo_and_reported_by_show() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let tool = |name: &str| tools.iter().find(|t| t.name() == name).unwrap();
        assert!(
            tool("projects.create")
                .execute(json!({ "name": "harbor", "setup": "pnpm install --frozen-lockfile" }))
                .await
                .unwrap()
                .success
        );
        let github = |repo: &str| json!({ "kind": "github", "owner": "acme", "repo": repo });
        for (alias, setup) in [("web", Some("  ")), ("rails", Some("bundle install"))] {
            let res = tool("projects.add_repo")
                .execute(json!({
                    "name": "harbor",
                    "alias": alias,
                    "source": github(alias),
                    "setup": setup,
                    "clone": false,
                }))
                .await
                .unwrap();
            assert!(res.success, "{}", res.output);
        }

        let shown = tool("projects.show")
            .execute(json!({ "name": "harbor" }))
            .await
            .unwrap();
        assert!(shown.success);
        assert_eq!(
            shown.output["defaults"]["setup"],
            "pnpm install --frozen-lockfile"
        );
        let repos = shown.output["repos"].as_array().unwrap();
        assert_eq!(
            repos[0]["setup"], "pnpm install --frozen-lockfile",
            "a blank per-repo setup is no override"
        );
        assert_eq!(repos[1]["setup"], "bundle install");
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();
        let res = create.execute(json!({ "name": "harbor" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn add_repo_without_clone_updates_manifest_only() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();

        let add = tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap();
        let res = add
            .execute(json!({
                "name": "harbor",
                "alias": "rails",
                "source": { "kind": "github", "owner": "harborlabs", "repo": "HarborRails" },
                "clone": false,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["cloned"], false);

        let show = tools.iter().find(|t| t.name() == "projects.show").unwrap();
        let res = show.execute(json!({ "name": "harbor" })).await.unwrap();
        assert!(res.success);
        let repos = res.output["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["alias"], "rails");
        assert_eq!(repos[0]["status"]["cloned"], false);
    }

    #[tokio::test]
    async fn remove_repo_returns_false_when_alias_missing() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();

        let remove = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap();
        let res = remove
            .execute(json!({ "name": "harbor", "alias": "missing" }))
            .await
            .unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn delete_returns_false_when_project_missing() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let delete = tools
            .iter()
            .find(|t| t.name() == "projects.delete")
            .unwrap();
        let res = delete
            .execute(json!({ "name": "nonexistent" }))
            .await
            .unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn delete_with_purge_removes_project_dir() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();

        let project_dir = layout::project_root(tmp.path(), "allan", "harbor");
        tokio::fs::create_dir_all(&project_dir).await.unwrap();
        assert!(project_dir.exists());

        let delete = tools
            .iter()
            .find(|t| t.name() == "projects.delete")
            .unwrap();
        let res = delete
            .execute(json!({ "name": "harbor", "purge": true }))
            .await
            .unwrap();
        assert!(res.success);
        assert!(!project_dir.exists());
    }

    #[tokio::test]
    async fn show_returns_failure_for_missing_project() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let show = tools.iter().find(|t| t.name() == "projects.show").unwrap();
        let res = show.execute(json!({ "name": "nope" })).await.unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn update_with_no_repos_returns_empty_report() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();

        let update = tools
            .iter()
            .find(|t| t.name() == "projects.update")
            .unwrap();
        let res = update.execute(json!({ "name": "harbor" })).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output["repos"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn tool_names_match_registered_order() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = runner_for(&tmp, Arc::clone(&store));
        let mut c = ctx(&tmp);
        c.store = store;
        c.runner = Some(runner);
        let tools = project_tools(c);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, TOOL_NAMES);
    }

    #[test]
    fn validate_safe_name_accepts_typical_names() {
        for name in ["harbor", "p1", "rails-prod", "core-api.v2", "a_b"] {
            assert!(validate_safe_name(name, "x").is_ok(), "{name} rejected");
        }
    }

    #[test]
    fn validate_safe_name_rejects_traversal_and_separators() {
        for name in ["../etc", "..", ".", ".hidden", "/abs", "a/b", "", "a b"] {
            assert!(validate_safe_name(name, "x").is_err(), "{name} accepted");
        }
    }

    #[test]
    fn validate_safe_name_enforces_length_bound() {
        let long = "a".repeat(65);
        assert!(validate_safe_name(&long, "x").is_err());
        let at_cap = "a".repeat(64);
        assert!(validate_safe_name(&at_cap, "x").is_ok());
    }

    #[tokio::test]
    async fn create_rejects_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        let res = create
            .execute(json!({ "name": "../escape" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("project name"));
    }

    #[tokio::test]
    async fn open_workspace_fails_for_missing_project() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let open = tools
            .iter()
            .find(|t| t.name() == "projects.open_workspace")
            .unwrap();
        let res = open
            .execute(json!({ "name": "nope", "scope_id": "wf-test" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn open_workspace_materialises_scratch_dir_when_project_has_no_repos() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();
        let open = tools
            .iter()
            .find(|t| t.name() == "projects.open_workspace")
            .unwrap();
        let res = open
            .execute(json!({ "name": "harbor", "scope_id": "wf-test" }))
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        let out = res.output;
        assert_eq!(out["repos"], json!([]));
        let parent = std::path::PathBuf::from(out["parent_dir"].as_str().unwrap());
        assert!(parent.is_dir());
        // Empty but for the liveness sentinel the work-session sweeper keys on.
        let entries: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from(crate::projects::LIVENESS_FILE)]
        );
    }

    #[tokio::test]
    async fn open_workspace_requires_scope_id_or_session_key() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();
        let open = tools
            .iter()
            .find(|t| t.name() == "projects.open_workspace")
            .unwrap();
        // No `scope_id` and no `CURRENT_SESSION_KEY` task-local set →
        // tool refuses rather than silently picking a random scope.
        let res = open.execute(json!({ "name": "harbor" })).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("no current session"));
    }

    #[tokio::test]
    async fn open_workspace_derives_scope_id_from_session_key() {
        use workforce_agent::runner::CURRENT_SESSION_KEY;

        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();

        let open_arc = tools
            .iter()
            .find(|t| t.name() == "projects.open_workspace")
            .unwrap()
            .clone();
        let res = CURRENT_SESSION_KEY
            .scope("workflow:abc123:plan".to_string(), async move {
                open_arc.execute(json!({ "name": "harbor" })).await
            })
            .await
            .unwrap();
        assert!(res.success, "{:?}", res.error);
        assert_eq!(
            res.output["scope_id"].as_str().unwrap(),
            WorktreeManager::scope_id_for_session("workflow:abc123:plan")
        );
        assert!(
            std::path::Path::new(res.output["parent_dir"].as_str().unwrap()).is_dir(),
            "derived scope must have been materialised"
        );
    }

    #[test]
    fn resolve_scope_id_prefers_explicit_argument() {
        assert_eq!(
            resolve_scope_id(Some("explicit-scope")).unwrap(),
            "explicit-scope"
        );
    }

    #[test]
    fn resolve_scope_id_returns_error_outside_session() {
        let err = resolve_scope_id(None).unwrap_err();
        assert!(err.contains("no current session"));
    }

    #[tokio::test]
    async fn add_repo_rejects_traversal_alias() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();

        let add = tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap();
        let res = add
            .execute(json!({
                "name": "harbor",
                "alias": "../sibling",
                "source": { "kind": "github", "owner": "a", "repo": "b" },
                "clone": false,
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("repo alias"));
    }

    /// Helper for cascade tests: register a repo with the knowledge
    /// service at the same `repo_id` the cascade code will look for, so
    /// we can verify it gets dropped.
    async fn seed_repo_index(
        ks: &KnowledgeService,
        data_dir: &std::path::Path,
        owner: &str,
        repo: &str,
    ) -> String {
        let source = RepoSource::Github {
            owner: owner.into(),
            repo: repo.into(),
        };
        let index_root = index_layout::repo_dir(data_dir, &source);
        let repo_id = KnowledgeService::repo_id_for(&index_root);
        ks.graph()
            .register_repo(&repo_id, repo, &index_root.to_string_lossy())
            .await
            .unwrap();
        // Also create the on-disk dir so the cascade can validate the
        // remove_dir_all path.
        tokio::fs::create_dir_all(&index_root).await.unwrap();
        repo_id
    }

    #[tokio::test]
    async fn remove_repo_purge_cascades_to_index() {
        let tmp = TempDir::new().unwrap();
        let ks =
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).expect("in-memory KS"));
        let tools = project_tools(ctx_with_knowledge(&tmp, Arc::clone(&ks)));

        // Set up a project + repo so remove_repo can find the manifest
        // entry.
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();
        let add = tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap();
        add.execute(json!({
            "name": "harbor",
            "alias": "rails",
            "source": { "kind": "github", "owner": "a", "repo": "b" },
            "clone": false,
        }))
        .await
        .unwrap();

        let repo_id = seed_repo_index(&ks, tmp.path(), "a", "b").await;
        assert!(ks.graph().list_repos().await.unwrap().contains(&repo_id));

        let remove = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap();
        let res = remove
            .execute(json!({
                "name": "harbor",
                "alias": "rails",
                "purge": true,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(
            res.output["index_purged"]["repo_id"].as_str(),
            Some(repo_id.as_str())
        );
        assert!(!ks.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn remove_repo_without_purge_leaves_index_alone() {
        let tmp = TempDir::new().unwrap();
        let ks =
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).expect("in-memory KS"));
        let tools = project_tools(ctx_with_knowledge(&tmp, Arc::clone(&ks)));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();
        let add = tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap();
        add.execute(json!({
            "name": "harbor",
            "alias": "rails",
            "source": { "kind": "github", "owner": "a", "repo": "b" },
            "clone": false,
        }))
        .await
        .unwrap();

        let repo_id = seed_repo_index(&ks, tmp.path(), "a", "b").await;

        let remove = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap();
        remove
            .execute(json!({ "name": "harbor", "alias": "rails" }))
            .await
            .unwrap();

        // Index untouched without purge.
        assert!(ks.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn delete_purge_cascades_for_every_alias() {
        let tmp = TempDir::new().unwrap();
        let ks =
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).expect("in-memory KS"));
        let tools = project_tools(ctx_with_knowledge(&tmp, Arc::clone(&ks)));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "harbor" })).await.unwrap();
        let add = tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap();
        for alias in ["rails", "kotlin"] {
            add.execute(json!({
                "name": "harbor",
                "alias": alias,
                "source": { "kind": "github", "owner": "a", "repo": alias },
                "clone": false,
            }))
            .await
            .unwrap();
        }

        let id_rails = seed_repo_index(&ks, tmp.path(), "a", "rails").await;
        let id_kotlin = seed_repo_index(&ks, tmp.path(), "a", "kotlin").await;
        assert_eq!(ks.graph().list_repos().await.unwrap().len(), 2);

        let delete = tools
            .iter()
            .find(|t| t.name() == "projects.delete")
            .unwrap();
        let res = delete
            .execute(json!({ "name": "harbor", "purge": true }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");

        let purged = res.output["index_purged"].as_array().unwrap();
        assert_eq!(purged.len(), 2);
        let ids: Vec<&str> = purged
            .iter()
            .map(|p| p["repo_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&id_rails.as_str()));
        assert!(ids.contains(&id_kotlin.as_str()));
        assert!(ks.graph().list_repos().await.unwrap().is_empty());
    }

    /// Refcount guard: when a sibling project still names the same
    /// upstream, removing it from one project with `purge=true` must
    /// leave the shared checkout + index intact. This is the DW-506
    /// invariant — without it, project A removing a shared repo would
    /// silently break project B's index.
    #[tokio::test]
    async fn remove_repo_purge_keeps_shared_index_when_sibling_project_uses_it() {
        let tmp = TempDir::new().unwrap();
        let ks =
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).expect("in-memory KS"));
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let ctx = ProjectsToolsContext {
            agent_id: "allan".into(),
            roots: WorkRoots::persistent(tmp.path()),
            store: Arc::clone(&store),
            agent_vault: None,
            global_vault: None,
            update_concurrency: 2,
            knowledge: Some(Arc::clone(&ks)),
            runner: None,
        };
        let tools = project_tools(ctx);

        // Project A and project B both reference acme/shared.
        for project_name in ["alpha", "beta"] {
            tools
                .iter()
                .find(|t| t.name() == "projects.create")
                .unwrap()
                .execute(json!({ "name": project_name }))
                .await
                .unwrap();
            tools
                .iter()
                .find(|t| t.name() == "projects.add_repo")
                .unwrap()
                .execute(json!({
                    "name": project_name,
                    "alias": "shared",
                    "source": { "kind": "github", "owner": "acme", "repo": "shared" },
                    "clone": false,
                }))
                .await
                .unwrap();
        }

        let repo_id = seed_repo_index(&ks, tmp.path(), "acme", "shared").await;
        assert!(ks.graph().list_repos().await.unwrap().contains(&repo_id));

        // Remove the repo from project alpha with purge=true. The
        // shared checkout must stay because project beta still names
        // acme/shared.
        let res = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap()
            .execute(json!({
                "name": "alpha",
                "alias": "shared",
                "purge": true,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        // index_purged should be null because the refcount check
        // skipped the cascade.
        assert!(res.output["index_purged"].is_null());
        assert!(ks.graph().list_repos().await.unwrap().contains(&repo_id));

        // Now remove from beta as well — repo is no longer named by
        // any project, so the cascade fires and the index drops.
        let res = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap()
            .execute(json!({
                "name": "beta",
                "alias": "shared",
                "purge": true,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(
            res.output["index_purged"]["repo_id"].as_str(),
            Some(repo_id.as_str())
        );
        assert!(!ks.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn rename_round_trips_via_tool() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        create.execute(json!({ "name": "old-name" })).await.unwrap();

        let rename = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap();
        let res = rename
            .execute(json!({ "name": "old-name", "new_name": "new-name" }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["name"], json!("new-name"));
        assert_eq!(res.output["previous_name"], json!("old-name"));

        // The list tool's output should reflect the new name only.
        let list = tools.iter().find(|t| t.name() == "projects.list").unwrap();
        let listed = list.execute(json!({})).await.unwrap();
        let names: Vec<&str> = listed.output["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["new-name"]);
    }

    #[tokio::test]
    async fn rename_moves_on_disk_dir_when_present() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();

        // Seed an on-disk dir at the old project_root so rename
        // exercises the directory-move branch (project's never been
        // /cc'd in this test, so layout::project_root has no real data
        // — we synthesise it).
        let old_root = layout::project_root(tmp.path(), "allan", "alpha");
        tokio::fs::create_dir_all(old_root.join("work/clones/some-repo"))
            .await
            .unwrap();
        tokio::fs::write(old_root.join("work/clones/some-repo/marker"), b"1")
            .await
            .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap()
            .execute(json!({ "name": "alpha", "new_name": "beta" }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");

        let new_root = layout::project_root(tmp.path(), "allan", "beta");
        assert!(!old_root.exists());
        assert!(new_root.join("work/clones/some-repo/marker").exists());
        assert_eq!(
            res.output["moved_path"].as_str(),
            Some(new_root.to_string_lossy().as_ref())
        );
    }

    /// Stale-origin guard: the cc-session subtree was
    /// `git clone --local`d from a work_clone under the old project
    /// root, so each `.git/config` records that absolute path as
    /// `remote.origin.url`. Renaming the project must wipe
    /// cc-sessions so the next /cc turn re-materializes them from the
    /// now-current work-clones — leaving them in place would surface
    /// "fatal: '/old/path' does not appear to be a git repository" on
    /// the next `git fetch origin`.
    #[tokio::test]
    async fn rename_resets_cc_sessions_to_avoid_stale_origin_urls() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();

        let old_root = layout::project_root(tmp.path(), "allan", "alpha");
        let cc_session = old_root.join("cc-sessions/some-uuid/repo");
        tokio::fs::create_dir_all(&cc_session).await.unwrap();
        tokio::fs::write(
            cc_session.join("config-marker"),
            format!("origin = {}", old_root.display()),
        )
        .await
        .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap()
            .execute(json!({ "name": "alpha", "new_name": "beta" }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["cc_sessions_reset"], json!(true));

        let new_root = layout::project_root(tmp.path(), "allan", "beta");
        assert!(!new_root.join("cc-sessions").exists());
        // Project root itself must still exist — only the cc-sessions
        // subtree is reset; work clones survive the move.
        assert!(new_root.exists());
    }

    #[tokio::test]
    async fn rename_fails_when_destination_taken() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let create = tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap();
        for n in ["alpha", "beta"] {
            create.execute(json!({ "name": n })).await.unwrap();
        }

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap()
            .execute(json!({ "name": "alpha", "new_name": "beta" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn rename_fails_when_source_missing() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        let res = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap()
            .execute(json!({ "name": "missing", "new_name": "anything" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn rename_rejects_invalid_new_name() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap()
            .execute(json!({ "name": "alpha", "new_name": "../sibling" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("new project name"));
    }

    #[tokio::test]
    async fn rename_rejects_no_op_same_name() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.rename")
            .unwrap()
            .execute(json!({ "name": "alpha", "new_name": "alpha" }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("identical"));
    }

    /// Same refcount guard at project-delete granularity: deleting a
    /// project with `purge=true` skips per-repo index drops for any
    /// repo a sibling project still references.
    #[tokio::test]
    async fn delete_purge_keeps_shared_indexes_used_by_other_projects() {
        let tmp = TempDir::new().unwrap();
        let ks =
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).expect("in-memory KS"));
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let ctx = ProjectsToolsContext {
            agent_id: "allan".into(),
            roots: WorkRoots::persistent(tmp.path()),
            store: Arc::clone(&store),
            agent_vault: None,
            global_vault: None,
            update_concurrency: 2,
            knowledge: Some(Arc::clone(&ks)),
            runner: None,
        };
        let tools = project_tools(ctx);

        for project_name in ["alpha", "beta"] {
            tools
                .iter()
                .find(|t| t.name() == "projects.create")
                .unwrap()
                .execute(json!({ "name": project_name }))
                .await
                .unwrap();
            tools
                .iter()
                .find(|t| t.name() == "projects.add_repo")
                .unwrap()
                .execute(json!({
                    "name": project_name,
                    "alias": "shared",
                    "source": { "kind": "github", "owner": "acme", "repo": "shared" },
                    "clone": false,
                }))
                .await
                .unwrap();
        }
        // alpha-only repo gets purged; the shared one does not.
        tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap()
            .execute(json!({
                "name": "alpha",
                "alias": "lonely",
                "source": { "kind": "github", "owner": "acme", "repo": "lonely" },
                "clone": false,
            }))
            .await
            .unwrap();

        let id_shared = seed_repo_index(&ks, tmp.path(), "acme", "shared").await;
        let id_lonely = seed_repo_index(&ks, tmp.path(), "acme", "lonely").await;

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.delete")
            .unwrap()
            .execute(json!({ "name": "alpha", "purge": true }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");

        let purged = res.output["index_purged"].as_array().unwrap();
        let purged_ids: Vec<&str> = purged
            .iter()
            .map(|p| p["repo_id"].as_str().unwrap())
            .collect();
        assert_eq!(purged_ids, vec![id_lonely.as_str()]);
        let remaining = ks.graph().list_repos().await.unwrap();
        assert!(remaining.contains(&id_shared));
        assert!(!remaining.contains(&id_lonely));
    }

    // ─── DW-509: warn output on delete + remove_repo ─────────────────

    #[tokio::test]
    async fn remove_repo_warn_lists_shared_with_when_sibling_project_pins_it() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let ctx = ProjectsToolsContext {
            agent_id: "allan".into(),
            roots: WorkRoots::persistent(tmp.path()),
            store: Arc::clone(&store),
            agent_vault: None,
            global_vault: None,
            update_concurrency: 2,
            knowledge: None,
            runner: None,
        };
        let tools = project_tools(ctx);

        for project in ["alpha", "beta"] {
            tools
                .iter()
                .find(|t| t.name() == "projects.create")
                .unwrap()
                .execute(json!({ "name": project }))
                .await
                .unwrap();
            tools
                .iter()
                .find(|t| t.name() == "projects.add_repo")
                .unwrap()
                .execute(json!({
                    "name": project,
                    "alias": "shared",
                    "source": { "kind": "github", "owner": "acme", "repo": "shared" },
                    "clone": false,
                }))
                .await
                .unwrap();
        }

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap()
            .execute(json!({ "name": "alpha", "alias": "shared" }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["orphaned"], json!(false));
        let shared_with = res.output["shared_with"].as_array().unwrap();
        assert_eq!(shared_with.len(), 1);
        assert_eq!(shared_with[0]["project"], json!("beta"));
    }

    #[tokio::test]
    async fn remove_repo_warn_marks_orphaned_when_last_referrer_drops() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();
        tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap()
            .execute(json!({
                "name": "alpha",
                "alias": "lonely",
                "source": { "kind": "github", "owner": "acme", "repo": "lonely" },
                "clone": false,
            }))
            .await
            .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap()
            .execute(json!({ "name": "alpha", "alias": "lonely" }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["orphaned"], json!(true));
        assert!(res.output["shared_with"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn remove_repo_warn_false_suppresses_audit_fields() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        tools
            .iter()
            .find(|t| t.name() == "projects.create")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();
        tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap()
            .execute(json!({
                "name": "alpha",
                "alias": "x",
                "source": { "kind": "github", "owner": "acme", "repo": "x" },
                "clone": false,
            }))
            .await
            .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.remove_repo")
            .unwrap()
            .execute(json!({ "name": "alpha", "alias": "x", "warn": false }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert!(res.output.get("shared_with").is_none());
        assert!(res.output.get("orphaned").is_none());
    }

    #[tokio::test]
    async fn delete_warn_partitions_repos_into_shared_and_orphaned() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let ctx = ProjectsToolsContext {
            agent_id: "allan".into(),
            roots: WorkRoots::persistent(tmp.path()),
            store: Arc::clone(&store),
            agent_vault: None,
            global_vault: None,
            update_concurrency: 2,
            knowledge: None,
            runner: None,
        };
        let tools = project_tools(ctx);

        // alpha pins shared (also pinned by beta) and lonely (alpha only).
        for project in ["alpha", "beta"] {
            tools
                .iter()
                .find(|t| t.name() == "projects.create")
                .unwrap()
                .execute(json!({ "name": project }))
                .await
                .unwrap();
            tools
                .iter()
                .find(|t| t.name() == "projects.add_repo")
                .unwrap()
                .execute(json!({
                    "name": project,
                    "alias": "shared",
                    "source": { "kind": "github", "owner": "acme", "repo": "shared" },
                    "clone": false,
                }))
                .await
                .unwrap();
        }
        tools
            .iter()
            .find(|t| t.name() == "projects.add_repo")
            .unwrap()
            .execute(json!({
                "name": "alpha",
                "alias": "lonely",
                "source": { "kind": "github", "owner": "acme", "repo": "lonely" },
                "clone": false,
            }))
            .await
            .unwrap();

        let res = tools
            .iter()
            .find(|t| t.name() == "projects.delete")
            .unwrap()
            .execute(json!({ "name": "alpha" }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");

        let shared = res.output["shared_repos"].as_array().unwrap();
        let orphaned = res.output["orphaned_repos"].as_array().unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0]["alias"], json!("shared"));
        assert_eq!(shared[0]["used_by"][0]["project"], json!("beta"));
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0]["alias"], json!("lonely"));
    }

    // ─── projects.index ───────────────────────────────────────────────

    #[test]
    fn parse_index_mode_defaults_to_incremental() {
        assert!(matches!(
            parse_index_mode(None),
            Ok(crate::knowledge::IndexMode::Incremental)
        ));
    }

    #[test]
    fn parse_index_mode_accepts_known_values() {
        assert!(matches!(
            parse_index_mode(Some("incremental")),
            Ok(crate::knowledge::IndexMode::Incremental)
        ));
        assert!(matches!(
            parse_index_mode(Some("full")),
            Ok(crate::knowledge::IndexMode::Full)
        ));
    }

    #[test]
    fn parse_index_mode_rejects_unknown_value() {
        let err = parse_index_mode(Some("weird")).unwrap_err();
        assert!(err.contains("weird"));
        assert!(err.contains("incremental"));
        assert!(err.contains("full"));
    }

    fn runner_for(tmp: &TempDir, store: Arc<dyn ProjectStore>) -> Arc<MaintenanceRunner> {
        Arc::new(MaintenanceRunner::new(
            Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).unwrap()),
            store,
            tmp.path().to_path_buf(),
            tmp.path().join("ws"),
            std::collections::BTreeMap::new(),
            None,
            1,
        ))
    }

    #[tokio::test]
    async fn projects_index_returns_failure_for_unknown_project() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = runner_for(&tmp, Arc::clone(&store));
        let mut c = ctx(&tmp);
        c.store = store;
        c.runner = Some(Arc::clone(&runner));
        let tools = project_tools(c);

        let tool = tools
            .iter()
            .find(|t| t.name() == "projects.index")
            .expect("projects.index registered when runner present");
        let res = tool.execute(json!({ "name": "missing" })).await.unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("missing"));
        assert!(err.contains("allan"));
        assert!(err.contains("projects.list"));
    }

    #[tokio::test]
    async fn projects_index_returns_failure_for_invalid_mode() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store.put(&Project::new("allan", "alpha")).unwrap();
        let runner = runner_for(&tmp, Arc::clone(&store));
        let mut c = ctx(&tmp);
        c.store = store;
        c.runner = Some(Arc::clone(&runner));
        let tools = project_tools(c);

        let tool = tools.iter().find(|t| t.name() == "projects.index").unwrap();
        let res = tool
            .execute(json!({ "name": "alpha", "mode": "weird" }))
            .await
            .unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("weird"));
        assert!(err.contains("incremental"));
        assert!(err.contains("full"));
    }

    #[tokio::test]
    async fn projects_index_with_zero_repos_returns_empty_report() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        store.put(&Project::new("allan", "alpha")).unwrap();
        let runner = runner_for(&tmp, Arc::clone(&store));
        let mut c = ctx(&tmp);
        c.store = store;
        c.runner = Some(Arc::clone(&runner));
        let tools = project_tools(c);

        let tool = tools.iter().find(|t| t.name() == "projects.index").unwrap();
        let res = tool.execute(json!({ "name": "alpha" })).await.unwrap();
        assert!(res.success, "{res:?}");
        // projects.index now starts the pass in the background and returns
        // immediately; completion is observed via knowledge.status.
        assert_eq!(res.output["outcome"], json!("started"));
        assert_eq!(res.output["project"], json!("alpha"));
        assert_eq!(res.output["agent_id"], json!("allan"));
        assert_eq!(res.output["mode"], json!("incremental"));
        assert_eq!(res.output["target_count"], json!(0));
        assert!(res.output["targets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn project_tools_omits_index_tool_when_runner_absent() {
        let tmp = TempDir::new().unwrap();
        let tools = project_tools(ctx(&tmp));
        assert!(tools.iter().all(|t| t.name() != "projects.index"));
    }

    #[test]
    fn project_tools_includes_index_tool_when_runner_present() {
        let tmp = TempDir::new().unwrap();
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = runner_for(&tmp, Arc::clone(&store));
        let mut c = ctx(&tmp);
        c.store = store;
        c.runner = Some(runner);
        let tools = project_tools(c);
        assert!(tools.iter().any(|t| t.name() == "projects.index"));
    }

    #[test]
    fn tool_names_includes_projects_index() {
        assert!(TOOL_NAMES.contains(&"projects.index"));
    }
}
