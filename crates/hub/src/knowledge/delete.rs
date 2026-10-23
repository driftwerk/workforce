//! `knowledge.delete` — drop indexed repos on demand.
//!
//! Two actions:
//!
//! - `repo`: drop one repo's index. Identify it by `repo_id` (legacy
//!   path-derived id) or — preferred post-DW-506 — by `source` (the
//!   `RepoSource` shape `{kind, owner, repo}`), which the tool maps
//!   through the canonical checkout layout to compute the matching
//!   `repo_id`. The `check_only` and `force` flags (DW-508) gate the
//!   destructive path on multi-project usage so a careless delete
//!   can't silently break a sibling project's index.
//! - `prune`: drop every `repo_id` not present in `known_repo_ids`. The
//!   intended caller passes the full list of repos it still wants kept;
//!   anything else is treated as an orphan. Guarded by `dry_run` (default
//!   true), `confirm`, an `acknowledge_count` over a small threshold, and a
//!   refusal when `known_repo_ids` is empty.
//!
//! Vector store cleanup is non-fatal — a Weaviate outage shouldn't block the
//! graph drop, and the next prune will pick up any leftover.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_codebase_knowledge::graph::RepoLineage;
use workforce_knowledge_service::KnowledgeService as InnerKS;
use workforce_projects::{ProjectStore, RepoSource};

use crate::knowledge::index_layout;
use crate::projects::usage::{project_references_by_slug, ProjectReference};

use super::KnowledgeService;

const PRUNE_ACK_THRESHOLD: usize = 10;

#[derive(Deserialize)]
struct Input {
    action: String,
    /// `action=repo`: the repo_id to drop. Optional when `source` is
    /// provided; the tool prefers `source` because it round-trips
    /// through the post-DW-506 canonical checkout layout deterministically.
    #[serde(default)]
    repo_id: Option<String>,
    /// `action=repo`: upstream repo identity. Preferred over `repo_id`
    /// because callers typically know the GitHub owner/repo, not the
    /// derived hash, and because it lets the tool look up project
    /// usage without enumerating the entire store.
    #[serde(default)]
    source: Option<RepoSource>,
    /// `action=repo`: report project usage without deleting. Returns
    /// the same shape as a real delete but with `deleted: false`.
    /// Mutually compatible with `force`; `check_only` always wins.
    #[serde(default)]
    check_only: bool,
    /// `action=repo`: explicitly opt in to deleting an index that's
    /// still named by multiple projects. Default false: a multi-project
    /// delete fails with the usage list and the user has to re-run with
    /// `force=true`. Single-project / orphan deletes don't need this
    /// flag.
    #[serde(default)]
    force: bool,
    /// `action=prune`: every repo_id you still want kept. Anything in the
    /// graph not in this list is dropped (subject to the safety guards).
    #[serde(default)]
    known_repo_ids: Option<Vec<String>>,
    /// `action=prune`: restrict candidates to rows whose lineage is one
    /// of these values. When set, callers don't need to compute
    /// `known_repo_ids` themselves. Combined with `known_repo_ids` as
    /// `(lineage in filter)` AND `(id not in known)` — i.e. the lineage
    /// list is the candidate pool, and `known_repo_ids` is the exception
    /// list that protects specific orphans from deletion.
    #[serde(default)]
    lineage_filter: Option<Vec<RepoLineage>>,
    /// `action=prune`: don't actually delete; just report what would go.
    /// Defaults to true.
    #[serde(default = "default_true")]
    dry_run: bool,
    /// `action=prune`: must be true alongside `dry_run=false` to delete.
    #[serde(default)]
    confirm: bool,
    /// `action=prune`: when more than [`PRUNE_ACK_THRESHOLD`] repos would be
    /// removed, this must equal the actual candidate count.
    #[serde(default)]
    acknowledge_count: Option<usize>,
}

fn default_true() -> bool {
    true
}

pub struct KnowledgeDeleteTool {
    service: Arc<KnowledgeService>,
    /// Project store for the multi-project usage check on
    /// `action='repo'`. Optional so the tool keeps working in tests
    /// and standalone deployments that don't carry a project store
    /// — falls back to the unguarded pre-DW-508 behavior in that case.
    store: Option<Arc<dyn ProjectStore>>,
    /// Hub data root, only used to derive shared canonical checkout
    /// paths from `source`. `None` when the caller didn't wire a store
    /// either (the two are paired).
    data_dir: Option<PathBuf>,
}

impl KnowledgeDeleteTool {
    pub fn new(service: Arc<KnowledgeService>) -> Self {
        Self {
            service,
            store: None,
            data_dir: None,
        }
    }

    /// Wire in the project store + data_dir so DW-508's multi-project
    /// usage check can run. The hub passes both at registration time;
    /// tests that only exercise the graph drop path can keep using
    /// `new()`.
    pub fn with_store(
        service: Arc<KnowledgeService>,
        store: Arc<dyn ProjectStore>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            service,
            store: Some(store),
            data_dir: Some(data_dir),
        }
    }

    /// Per-`source` repo_id under the post-DW-506 shared layout. Same
    /// derivation `MaintenanceRunner::pull_all` applies: the indexer's
    /// canonical path through `repo_dir` round-tripped through the
    /// `KnowledgeService::repo_id_for` hash.
    fn repo_id_for_source(data_dir: &std::path::Path, source: &RepoSource) -> String {
        InnerKS::repo_id_for(&index_layout::repo_dir(data_dir, source))
    }

    /// Walk the project store and find the unique upstream source
    /// whose canonical post-DW-506 repo_id matches `repo_id`. Returns
    /// the canonical slug on hit and back-fills `source_out` so the
    /// rest of the pipeline (the JSON output, downstream gating) sees
    /// the same data it would have if the caller had passed `source`.
    /// Returns `None` for repo_ids that don't reverse-map — true
    /// orphans, or entries indexed under the pre-DW-506 layout still
    /// awaiting a `prune` cleanup. Cost is one `list_all` + one hash
    /// per unique slug; project lists are small.
    fn reverse_map_repo_id(
        &self,
        store: &dyn ProjectStore,
        data_dir: &std::path::Path,
        repo_id: &str,
        source_out: &mut Option<RepoSource>,
    ) -> Option<String> {
        let projects = match store.list_all() {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    repo_id = %repo_id, error = %e,
                    "knowledge.delete: store walk for repo_id reverse-map failed",
                );
                return None;
            }
        };
        let mut seen = std::collections::HashSet::new();
        for project in &projects {
            for repo in &project.repos {
                let slug = repo.source.canonical_slug();
                if !seen.insert(slug.clone()) {
                    continue;
                }
                if Self::repo_id_for_source(data_dir, &repo.source) == repo_id {
                    *source_out = Some(repo.source.clone());
                    return Some(slug);
                }
            }
        }
        None
    }

    /// Process the `action='repo'` branch: resolve identity, look up
    /// project usage, gate on `check_only` / `force`, then invoke
    /// `drop_repo_everywhere`. Returns the same envelope shape across
    /// every code path so callers don't have to branch on success vs.
    /// the new "would-affect-N-projects" pre-check failure.
    async fn handle_repo_action(&self, input: Input, start: Instant) -> Result<ToolResult> {
        let elapsed = || start.elapsed().as_millis() as u64;

        // Seed identity from whichever the caller supplied. `source`
        // is preferred — it lets us look up project usage directly.
        // Bare `repo_id` falls through to the reverse-map step below.
        let (mut slug, repo_id, mut source_for_output) = match (&input.source, &input.repo_id) {
            (Some(source), _) => match self.data_dir.as_deref() {
                Some(data_dir) => (
                    Some(source.canonical_slug()),
                    Self::repo_id_for_source(data_dir, source),
                    Some(source.clone()),
                ),
                None => {
                    // No data_dir wired: degrade to "treat source as
                    // identity-only" — no usage check possible. Falls
                    // through to drop without DW-508 gating.
                    let repo_id = source.canonical_slug();
                    (Some(repo_id.clone()), repo_id, Some(source.clone()))
                }
            },
            (None, Some(repo_id)) => (None, repo_id.clone(), None),
            (None, None) => {
                return Ok(ToolResult::failure(
                    "action='repo' requires `source` (preferred) or `repo_id`".to_string(),
                    elapsed(),
                ));
            }
        };

        // Reverse-map: when the caller only supplied `repo_id`, walk
        // the project store and find the unique source whose canonical
        // post-DW-506 path hashes to that id. Without this step a
        // legacy `repo_id` call bypasses the DW-508 force gate
        // entirely (slug stays `None` → `used_by` empty → multi-
        // project guard inert), which defeats the invariant the gate
        // was added to enforce. Repo_ids that don't reverse-map (true
        // orphans / pre-DW-506 leftovers) keep flowing through
        // ungated — that's the intentional escape hatch.
        if slug.is_none() {
            if let (Some(store), Some(data_dir)) = (&self.store, self.data_dir.as_deref()) {
                slug = self.reverse_map_repo_id(
                    store.as_ref(),
                    data_dir,
                    &repo_id,
                    &mut source_for_output,
                );
            }
        }

        // Pull project usage from the store when both pieces are
        // available. Without the store we report `used_by: []` and
        // skip the force gate — same semantic as a hub run that has
        // no project store at all.
        let used_by: Vec<ProjectReference> = match (&self.store, &slug) {
            (Some(store), Some(slug)) => match project_references_by_slug(store.as_ref()) {
                Ok(map) => map.get(slug).cloned().unwrap_or_default(),
                Err(e) => {
                    warn!(
                        repo_id = %repo_id, error = %e,
                        "knowledge.delete: project usage lookup failed; treating as no-usage",
                    );
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };

        let source_json = source_for_output.as_ref().map(|s| match s {
            RepoSource::Github { owner, repo } => json!({
                "kind": "github",
                "owner": owner,
                "repo": repo,
            }),
        });

        if input.check_only {
            return Ok(ToolResult::success(
                json!({
                    "repo_id": repo_id,
                    "source": source_json,
                    "used_by": used_by,
                    "deleted": false,
                    "reason": "check_only",
                }),
                elapsed(),
            ));
        }

        // Multi-project guard: refuse to drop an index named by more
        // than one project unless the caller explicitly opted in via
        // `force=true`. This is the DW-508 invariant — the pre-DW-506
        // tool would silently break sibling projects, the post-DW-508
        // tool surfaces the conflict and waits for confirmation.
        if used_by.len() > 1 && !input.force {
            return Ok(ToolResult::failure(
                format!(
                    "index '{}' is referenced by {} projects; pass force=true to delete anyway",
                    repo_id,
                    used_by.len()
                ),
                elapsed(),
            ));
        }

        let dropped = drop_repo_everywhere(&self.service, &repo_id).await;
        Ok(ToolResult::success(
            json!({
                "repo_id": repo_id,
                "source": source_json,
                "used_by": used_by,
                "deleted": true,
                "dropped": dropped,
            }),
            elapsed(),
        ))
    }
}

/// Drop graph + vector data for one repo. Non-fatal: any layer that
/// errors logs and continues so a Weaviate or graph blip doesn't leave
/// callers with an unrecoverable state. Returns a JSON receipt the
/// callers (knowledge.delete, projects.{remove_repo, delete}) embed in
/// their tool output.
pub async fn drop_repo_everywhere(ks: &KnowledgeService, repo_id: &str) -> serde_json::Value {
    let graph_dropped = match ks.graph().drop_repo(repo_id).await {
        Ok(n) => Some(n),
        Err(e) => {
            warn!(repo_id = %repo_id, error = %e, "graph drop_repo failed");
            None
        }
    };

    let mut vector_entities = None;
    let mut vector_summaries = None;
    if let Some(vs) = ks.vector_store() {
        match vs.delete_repo_entities(repo_id).await {
            Ok(o) => vector_entities = Some(o),
            Err(e) => warn!(
                repo_id = %repo_id, error = %e,
                "vector entity purge failed (non-fatal)",
            ),
        }
        match vs.delete_repo_summaries(repo_id).await {
            Ok(o) => vector_summaries = Some(o),
            Err(e) => warn!(
                repo_id = %repo_id, error = %e,
                "vector summary purge failed (non-fatal)",
            ),
        }
    }

    serde_json::json!({
        "repo_id": repo_id,
        "graph_entities_dropped": graph_dropped,
        "vector_entities": vector_entities,
        "vector_summaries": vector_summaries,
    })
}

#[async_trait]
impl Tool for KnowledgeDeleteTool {
    fn name(&self) -> &str {
        "knowledge.delete"
    }

    fn description(&self) -> &str {
        "Drop an indexed repo (graph + vector store). action='repo' \
        drops one by `source` (preferred: `{kind, owner, repo}`) or \
        legacy `repo_id`. Pass `check_only=true` to inspect project \
        usage without deleting; pass `force=true` to delete an index \
        still referenced by multiple projects (refused otherwise). \
        action='prune' drops every repo not in `known_repo_ids` — \
        defaults to dry_run; pass `confirm=true` and (when over 10) \
        `acknowledge_count=<n>` to actually delete. Refuses prune with \
        empty `known_repo_ids`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["repo", "prune"] },
                "source": {
                    "type": "object",
                    "description": "action='repo'. Upstream repo identity; preferred over `repo_id`.",
                    "properties": {
                        "kind": { "type": "string", "enum": ["github"] },
                        "owner": { "type": "string" },
                        "repo": { "type": "string" }
                    },
                    "required": ["kind", "owner", "repo"]
                },
                "repo_id": { "type": "string", "description": "action='repo'. Legacy fallback when `source` isn't provided." },
                "check_only": { "type": "boolean", "description": "action='repo'. Report usage without deleting." },
                "force": { "type": "boolean", "description": "action='repo'. Required to delete an index used by multiple projects." },
                "known_repo_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "action='prune'. Repos NOT in this list are removed. Optional when `lineage_filter` is set."
                },
                "lineage_filter": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["canonical", "migration_orphan", "unknown"] },
                    "description": "action='prune'. Restrict candidates to rows whose lineage is in this list. Prefer this over `known_repo_ids` for post-DW-506 cleanup — use [\"migration_orphan\"] to drop pre-DW-506 leftovers without enumerating ids."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "action='prune' only. Defaults to true. Returns candidates without deleting."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "action='prune' only. Required alongside dry_run=false."
                },
                "acknowledge_count": {
                    "type": "integer",
                    "description": "action='prune' only. Required when more than 10 repos would be removed; must equal the candidate count."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {}", e),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match input.action.as_str() {
            "repo" => self.handle_repo_action(input, start).await,
            "prune" => {
                // Allow `lineage_filter`-only callers (no `known_repo_ids`).
                // When neither is supplied we'd be asking the tool to drop
                // every repo with no filter — refuse, same as before.
                if input.known_repo_ids.is_none() && input.lineage_filter.is_none() {
                    return Ok(ToolResult::failure(
                        "action='prune' requires `known_repo_ids` or `lineage_filter`".to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }
                let known = input.known_repo_ids.unwrap_or_default();

                if known.is_empty() && input.lineage_filter.is_none() && !input.dry_run {
                    return Ok(ToolResult::failure(
                        "refusing to prune ALL repos: pass at least one entry \
                         in `known_repo_ids`, or use action='repo' per-id"
                            .to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }

                let known_set: std::collections::HashSet<&str> =
                    known.iter().map(|s| s.as_str()).collect();
                let candidates: Vec<String> = match input.lineage_filter.as_ref() {
                    Some(filter) => {
                        let filter_set: std::collections::HashSet<RepoLineage> =
                            filter.iter().copied().collect();
                        self.service
                            .graph()
                            .list_repositories()
                            .await?
                            .into_iter()
                            .filter(|r| {
                                filter_set.contains(&r.lineage)
                                    && !known_set.contains(r.id.as_str())
                            })
                            .map(|r| r.id)
                            .collect()
                    }
                    None => self
                        .service
                        .graph()
                        .list_repos()
                        .await?
                        .into_iter()
                        .filter(|id| !known_set.contains(id.as_str()))
                        .collect(),
                };

                if input.dry_run {
                    return Ok(ToolResult::success(
                        serde_json::json!({
                            "dry_run": true,
                            "candidate_count": candidates.len(),
                            "candidates": candidates,
                        }),
                        start.elapsed().as_millis() as u64,
                    ));
                }

                if !input.confirm {
                    return Ok(ToolResult::failure(
                        format!(
                            "{} repos would be deleted; pass confirm=true to proceed",
                            candidates.len()
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }

                if candidates.len() > PRUNE_ACK_THRESHOLD {
                    let acked = input.acknowledge_count.unwrap_or(0);
                    if acked != candidates.len() {
                        return Ok(ToolResult::failure(
                            format!(
                                "{} repos would be deleted (over threshold {}); \
                                 pass acknowledge_count={} to proceed",
                                candidates.len(),
                                PRUNE_ACK_THRESHOLD,
                                candidates.len()
                            ),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }

                let mut dropped = Vec::with_capacity(candidates.len());
                for repo_id in &candidates {
                    dropped.push(drop_repo_everywhere(&self.service, repo_id).await);
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "dry_run": false,
                        "dropped_count": dropped.len(),
                        "dropped": dropped,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            other => Ok(ToolResult::failure(
                format!("Unknown action '{}': expected 'repo' or 'prune'", other),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    async fn fixture() -> Arc<KnowledgeService> {
        let svc = KnowledgeService::in_memory(PathBuf::from("/tmp/wf-test-ws"))
            .expect("in-memory KnowledgeService");
        for id in &["alpha-aaaaaaaa", "bravo-bbbbbbbb", "charlie-cccccccc"] {
            svc.graph().register_repo(id, id, "/tmp").await.unwrap();
        }
        Arc::new(svc)
    }

    #[tokio::test]
    async fn repo_drops_a_single_repo() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({ "action": "repo", "repo_id": "bravo-bbbbbbbb" }))
            .await
            .unwrap();
        assert!(res.success);

        let remaining = svc.graph().list_repos().await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(!remaining.contains(&"bravo-bbbbbbbb".to_string()));
    }

    #[tokio::test]
    async fn repo_action_requires_source_or_repo_id() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool.execute(json!({ "action": "repo" })).await.unwrap();
        assert!(!res.success);
        assert!(res
            .error
            .unwrap()
            .contains("requires `source` (preferred) or `repo_id`"));
    }

    #[tokio::test]
    async fn prune_dry_run_lists_orphans() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({
                "action": "prune",
                "known_repo_ids": ["alpha-aaaaaaaa"],
            }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["dry_run"], json!(true));
        assert_eq!(res.output["candidate_count"], json!(2));
        assert_eq!(svc.graph().list_repos().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn prune_requires_confirm_to_delete() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({
                "action": "prune",
                "known_repo_ids": ["alpha-aaaaaaaa"],
                "dry_run": false,
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert_eq!(svc.graph().list_repos().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn prune_refuses_empty_known_with_confirm() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({
                "action": "prune",
                "known_repo_ids": [],
                "dry_run": false,
                "confirm": true,
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert_eq!(svc.graph().list_repos().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn prune_with_lineage_filter_drops_only_orphans() {
        let svc = fixture().await;
        svc.graph()
            .set_repo_lineage("alpha-aaaaaaaa", RepoLineage::Canonical)
            .await
            .unwrap();
        svc.graph()
            .set_repo_lineage("bravo-bbbbbbbb", RepoLineage::MigrationOrphan)
            .await
            .unwrap();
        svc.graph()
            .set_repo_lineage("charlie-cccccccc", RepoLineage::MigrationOrphan)
            .await
            .unwrap();
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({
                "action": "prune",
                "lineage_filter": ["migration_orphan"],
                "dry_run": false,
                "confirm": true,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        let remaining = svc.graph().list_repos().await.unwrap();
        assert_eq!(remaining, vec!["alpha-aaaaaaaa".to_string()]);
    }

    #[tokio::test]
    async fn prune_lineage_filter_dry_run_lists_candidates() {
        let svc = fixture().await;
        svc.graph()
            .set_repo_lineage("alpha-aaaaaaaa", RepoLineage::Canonical)
            .await
            .unwrap();
        svc.graph()
            .set_repo_lineage("bravo-bbbbbbbb", RepoLineage::MigrationOrphan)
            .await
            .unwrap();
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({
                "action": "prune",
                "lineage_filter": ["migration_orphan"],
            }))
            .await
            .unwrap();
        assert!(res.success);
        assert_eq!(res.output["candidate_count"], 1);
        assert_eq!(
            res.output["candidates"],
            json!(["bravo-bbbbbbbb".to_string()])
        );
    }

    #[tokio::test]
    async fn prune_requires_known_or_lineage_filter() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool.execute(json!({ "action": "prune" })).await.unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("known_repo_ids"));
        assert!(err.contains("lineage_filter"));
    }

    #[tokio::test]
    async fn prune_with_confirm_deletes() {
        let svc = fixture().await;
        let tool = KnowledgeDeleteTool::new(Arc::clone(&svc));
        let res = tool
            .execute(json!({
                "action": "prune",
                "known_repo_ids": ["alpha-aaaaaaaa", "bravo-bbbbbbbb"],
                "dry_run": false,
                "confirm": true,
            }))
            .await
            .unwrap();
        assert!(res.success);
        let remaining = svc.graph().list_repos().await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(!remaining.contains(&"charlie-cccccccc".to_string()));
    }

    // ─── DW-508: source-keyed delete + multi-project usage gate ──────

    use tempfile::TempDir;
    use workforce_projects::sqlite::SqliteProjectStore;
    use workforce_projects::{Project, ProjectRepo};

    fn project_store_with(refs: &[(&str, &str, &str, &str, &str)]) -> Arc<dyn ProjectStore> {
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        for (agent, project, alias, owner, repo) in refs {
            let mut p = store
                .get(agent, project)
                .unwrap()
                .unwrap_or_else(|| Project::new(*agent, *project));
            p.repos.push(ProjectRepo {
                alias: (*alias).into(),
                source: RepoSource::Github {
                    owner: (*owner).into(),
                    repo: (*repo).into(),
                },
                default_branch: None,
                setup: None,
            });
            store.put(&p).unwrap();
        }
        store
    }

    /// Build a knowledge service whose graph has one repo registered
    /// at the post-DW-506 derived id for `(owner, repo)` under
    /// `tmp/data_dir`, so the delete tool's `source` path round-trips
    /// against a real graph entry.
    async fn ks_with_repo_at_canonical(
        data_dir: &std::path::Path,
        owner: &str,
        repo: &str,
    ) -> (Arc<KnowledgeService>, String) {
        let svc = Arc::new(
            KnowledgeService::in_memory(data_dir.to_path_buf())
                .expect("in-memory KnowledgeService"),
        );
        let source = RepoSource::Github {
            owner: owner.into(),
            repo: repo.into(),
        };
        let path = index_layout::repo_dir(data_dir, &source);
        let id = InnerKS::repo_id_for(&path);
        svc.graph()
            .register_repo(&id, repo, &path.to_string_lossy())
            .await
            .unwrap();
        (svc, id)
    }

    #[tokio::test]
    async fn repo_delete_via_source_round_trips_through_canonical_layout() {
        let tmp = TempDir::new().unwrap();
        let (svc, repo_id) = ks_with_repo_at_canonical(tmp.path(), "acme", "shared").await;
        let store = project_store_with(&[("allan", "p", "shared", "acme", "shared")]);
        let tool = KnowledgeDeleteTool::with_store(
            Arc::clone(&svc),
            Arc::clone(&store),
            tmp.path().to_path_buf(),
        );

        let res = tool
            .execute(json!({
                "action": "repo",
                "source": { "kind": "github", "owner": "acme", "repo": "shared" },
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["repo_id"].as_str(), Some(repo_id.as_str()));
        assert_eq!(res.output["deleted"], json!(true));
        assert!(!svc.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn repo_delete_check_only_returns_used_by_without_dropping() {
        let tmp = TempDir::new().unwrap();
        let (svc, repo_id) = ks_with_repo_at_canonical(tmp.path(), "acme", "shared").await;
        let store = project_store_with(&[
            ("allan", "alpha", "rails", "acme", "shared"),
            ("rita", "beta", "the-tool", "acme", "shared"),
        ]);
        let tool =
            KnowledgeDeleteTool::with_store(Arc::clone(&svc), store, tmp.path().to_path_buf());

        let res = tool
            .execute(json!({
                "action": "repo",
                "source": { "kind": "github", "owner": "acme", "repo": "shared" },
                "check_only": true,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert_eq!(res.output["deleted"], json!(false));
        assert_eq!(res.output["reason"], json!("check_only"));
        let used_by = res.output["used_by"].as_array().unwrap();
        assert_eq!(used_by.len(), 2);
        // Repo still in graph because check_only never drops.
        assert!(svc.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn repo_delete_refuses_multi_project_without_force() {
        let tmp = TempDir::new().unwrap();
        let (svc, repo_id) = ks_with_repo_at_canonical(tmp.path(), "acme", "shared").await;
        let store = project_store_with(&[
            ("allan", "alpha", "rails", "acme", "shared"),
            ("rita", "beta", "tool", "acme", "shared"),
        ]);
        let tool =
            KnowledgeDeleteTool::with_store(Arc::clone(&svc), store, tmp.path().to_path_buf());

        let res = tool
            .execute(json!({
                "action": "repo",
                "source": { "kind": "github", "owner": "acme", "repo": "shared" },
            }))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap().contains("force=true"));
        assert!(svc.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn repo_delete_force_bypasses_multi_project_guard() {
        let tmp = TempDir::new().unwrap();
        let (svc, repo_id) = ks_with_repo_at_canonical(tmp.path(), "acme", "shared").await;
        let store = project_store_with(&[
            ("allan", "alpha", "rails", "acme", "shared"),
            ("rita", "beta", "tool", "acme", "shared"),
        ]);
        let tool =
            KnowledgeDeleteTool::with_store(Arc::clone(&svc), store, tmp.path().to_path_buf());

        let res = tool
            .execute(json!({
                "action": "repo",
                "source": { "kind": "github", "owner": "acme", "repo": "shared" },
                "force": true,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert!(!svc.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    #[tokio::test]
    async fn repo_delete_single_project_does_not_need_force() {
        let tmp = TempDir::new().unwrap();
        let (svc, repo_id) = ks_with_repo_at_canonical(tmp.path(), "acme", "lonely").await;
        let store = project_store_with(&[("allan", "p", "x", "acme", "lonely")]);
        let tool =
            KnowledgeDeleteTool::with_store(Arc::clone(&svc), store, tmp.path().to_path_buf());

        let res = tool
            .execute(json!({
                "action": "repo",
                "source": { "kind": "github", "owner": "acme", "repo": "lonely" },
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert!(!svc.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    /// Closes the bypass: a caller who already knows the canonical
    /// `repo_id` (e.g. cached from `knowledge.list_repos`) and passes
    /// it as the legacy identifier must still hit the multi-project
    /// force gate. Without the reverse-map step the slug stays
    /// `None`, `used_by` is empty, and the gate is silently inert.
    #[tokio::test]
    async fn repo_delete_via_legacy_repo_id_still_hits_force_gate() {
        let tmp = TempDir::new().unwrap();
        let (svc, repo_id) = ks_with_repo_at_canonical(tmp.path(), "acme", "shared").await;
        let store = project_store_with(&[
            ("allan", "alpha", "rails", "acme", "shared"),
            ("rita", "beta", "tool", "acme", "shared"),
        ]);
        let tool =
            KnowledgeDeleteTool::with_store(Arc::clone(&svc), store, tmp.path().to_path_buf());

        let res = tool
            .execute(json!({
                "action": "repo",
                "repo_id": repo_id,
            }))
            .await
            .unwrap();
        assert!(
            !res.success,
            "force gate must fire for multi-project repo_id"
        );
        assert!(res.error.unwrap().contains("force=true"));
        assert!(svc.graph().list_repos().await.unwrap().contains(&repo_id));
    }

    /// Reverse-map only fires when the repo_id corresponds to a
    /// known canonical source. A stale repo_id (orphan from a
    /// pre-DW-506 entry, or one the indexer mistakenly produced)
    /// flows through ungated — that's the intentional escape hatch
    /// for cleanup ops.
    #[tokio::test]
    async fn repo_delete_via_orphan_repo_id_bypasses_gate_intentionally() {
        let tmp = TempDir::new().unwrap();
        let (svc, _) = ks_with_repo_at_canonical(tmp.path(), "acme", "orphan").await;
        // Project store has nothing referencing `acme/orphan`, so the
        // reverse-map yields no slug; force gate stays inert.
        let store = project_store_with(&[]);
        let tool =
            KnowledgeDeleteTool::with_store(Arc::clone(&svc), store, tmp.path().to_path_buf());

        let orphan_id = InnerKS::repo_id_for(&index_layout::repo_dir(
            tmp.path(),
            &RepoSource::Github {
                owner: "acme".into(),
                repo: "orphan".into(),
            },
        ));
        let res = tool
            .execute(json!({
                "action": "repo",
                "repo_id": orphan_id,
            }))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert!(!svc.graph().list_repos().await.unwrap().contains(&orphan_id));
    }
}
