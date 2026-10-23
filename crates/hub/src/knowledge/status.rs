//! `knowledge.status` — single-call hub-wide indexing overview.
//!
//! Aggregates every knowledge-subsystem signal into one JSON document
//! so the agent never has to compose state from sparse signals:
//! canonical `IndexTarget`s, the typed `repositories.lineage` column,
//! active `index_lease` rows, the in-flight pass snapshot, recent-pass
//! failures, and the typed `vector_backend` health.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::warn;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_codebase_knowledge::completeness::{IndexCompleteness, NextAction};
use workforce_codebase_knowledge::graph::{RepoLineage, RepositoryRow};
use workforce_codebase_knowledge::lease::{now_ms, IndexLease};
use workforce_knowledge_service::KnowledgeService;

use super::global_index::{IndexTarget, MaintenanceRunner};
use super::pass_state::PassStateSnapshot;
use super::vector_health::{BackendHealthProbe, VectorBackendHealth};

/// Tool name registered by [`KnowledgeStatusTool`].
pub const TOOL_NAME: &str = "knowledge.status";

const VECTOR_HEALTH_TTL: Duration = Duration::from_secs(5);

pub fn default_policy() -> workforce_policy::rules::PolicyRule {
    use workforce_policy::rules::{PolicyRule, PolicyScope, RuleAction};
    PolicyRule {
        name: format!("{TOOL_NAME}-allow-default"),
        scope: PolicyScope::Global,
        tool: TOOL_NAME.to_string(),
        action: RuleAction::Allow,
        conditions: Vec::new(),
        priority: 10,
        constitutional: true,
    }
}

pub struct KnowledgeStatusTool {
    service: Arc<KnowledgeService>,
    runner: Arc<MaintenanceRunner>,
    probe: Arc<BackendHealthProbe>,
    backend_cache: Mutex<Option<CachedBackendState>>,
}

struct CachedBackendState {
    captured_at: Instant,
    state: VectorBackendHealth,
}

impl KnowledgeStatusTool {
    pub fn new(
        service: Arc<KnowledgeService>,
        runner: Arc<MaintenanceRunner>,
        probe: Arc<BackendHealthProbe>,
    ) -> Self {
        Self {
            service,
            runner,
            probe,
            backend_cache: Mutex::new(None),
        }
    }

    async fn cached_vector_backend(&self) -> VectorBackendHealth {
        let mut guard = self.backend_cache.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.captured_at.elapsed() < VECTOR_HEALTH_TTL {
                return cached.state.clone();
            }
        }
        let fresh = self.probe.current().await;
        *guard = Some(CachedBackendState {
            captured_at: Instant::now(),
            state: fresh.clone(),
        });
        fresh
    }

    async fn assemble(&self) -> Result<serde_json::Value> {
        let targets = self.runner.enumerate_targets()?;
        let rows = self.service.graph().list_repositories().await?;
        let leases = self.service.graph().list_active_leases().await?;
        let pass = self.runner.snapshot_state().await;
        let recent_failures = self.runner.recent_failures().await;
        let vector_backend = self.cached_vector_backend().await;

        let canonical_ids: std::collections::HashSet<String> = targets
            .iter()
            .map(|t| KnowledgeService::repo_id_for(&t.dest))
            .collect();

        let target_views: Vec<serde_json::Value> =
            futures::future::join_all(targets.iter().map(|t| self.target_view(t)))
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?;

        let stale_rows: Vec<StaleRow> = rows
            .into_iter()
            .filter(|r| !canonical_ids.contains(&r.id))
            .map(classify_stale_row)
            .collect();

        let lease_views: Vec<LeaseView> = leases.iter().map(LeaseView::from_lease).collect();

        Ok(serde_json::json!({
            "targets": target_views,
            "active_leases": lease_views,
            "stale_rows": stale_rows,
            "vector_backend": vector_backend,
            "pass": pass_section(pass.as_ref()),
            "recent_failures": recent_failures,
        }))
    }

    async fn target_view(&self, target: &IndexTarget) -> Result<serde_json::Value> {
        let repo_id = KnowledgeService::repo_id_for(&target.dest);
        let head = self.service.get_head_commit_for(&target.dest).ok();
        let completeness = self.service.completeness(&repo_id, head.as_deref()).await?;

        // `is_current()` collapses "commit indexed" and "summaries complete"
        // into one bool; split them out so a reader can't take summary-lag for
        // unindexed commits. `status_reason` is the field to render.
        let commit_current = completeness.head_commit.is_some()
            && completeness.head_commit == completeness.indexed_commit;
        let summaries = if completeness.generate_summaries {
            serde_json::json!({
                "enabled": true,
                "present": completeness.summaries_present,
                "expected": completeness.summaries_expected,
                "stale": completeness.summaries_stale,
                "missing": completeness.summaries_missing(),
            })
        } else {
            serde_json::json!({ "enabled": false })
        };
        Ok(serde_json::json!({
            "id": target.id,
            "source": target.source,
            "repo_id": repo_id,
            "checkout_path": target.dest.to_string_lossy(),
            "agent_ids": target.agent_ids,
            "indexed_commit": completeness.indexed_commit,
            "head_commit": completeness.head_commit,
            "commit_current": commit_current,
            "status_reason": status_reason(&completeness, now_ms()),
            "is_current": completeness.is_current(),
            "entities": completeness.entities,
            "vector_entities": completeness.vectors,
            "summaries_enabled": completeness.generate_summaries,
            "summaries": summaries,
            "last_full_index_at": completeness.last_full_index_at,
            "last_incremental_at": completeness.last_incremental_at,
        }))
    }
}

/// One-word target state derived from the indexer's own [`NextAction`], so
/// `knowledge.status` can distinguish a stale code graph from one that is
/// current but whose LLM summaries are still catching up (`summaries_pending`).
fn status_reason(c: &IndexCompleteness, now_ms: i64) -> &'static str {
    match c.next_action(now_ms) {
        NextAction::Complete => "current",
        NextAction::AlreadyIndexing(_) => "indexing",
        NextAction::ResumeSummaries => "summaries_pending",
        NextAction::Incremental => "commits_behind",
        NextAction::Full => "never_indexed",
    }
}

#[derive(Debug, Serialize)]
struct StaleRow {
    repo_id: String,
    root_path: String,
    lineage: RepoLineage,
    last_indexed_at: Option<String>,
}

fn classify_stale_row(row: RepositoryRow) -> StaleRow {
    StaleRow {
        repo_id: row.id,
        root_path: row.root_path,
        lineage: row.lineage,
        last_indexed_at: row.last_indexed_at,
    }
}

#[derive(Debug, Serialize)]
struct LeaseView {
    repo_id: String,
    worker_id: String,
    phase: String,
    is_stale: bool,
    acquired_at_ms: i64,
    last_heartbeat_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    entities_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entities_processed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summaries_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summaries_processed: Option<u64>,
}

impl LeaseView {
    fn from_lease(l: &IndexLease) -> Self {
        Self::from_lease_at(l, now_ms())
    }

    fn from_lease_at(l: &IndexLease, now_ms: i64) -> Self {
        Self {
            repo_id: l.repo_id.clone(),
            worker_id: l.worker_id.clone(),
            phase: l.phase.as_str().to_string(),
            is_stale: !l.is_fresh(now_ms),
            acquired_at_ms: l.acquired_at_ms,
            last_heartbeat_at_ms: l.last_heartbeat_at_ms,
            entities_total: l.entities_total,
            entities_processed: l.entities_processed,
            summaries_total: l.summaries_total,
            summaries_processed: l.summaries_processed,
        }
    }
}

fn pass_section(pass: Option<&PassStateSnapshot>) -> serde_json::Value {
    match pass {
        Some(snap) => serde_json::json!({
            "in_flight": true,
            "snapshot": snap,
        }),
        None => serde_json::json!({ "in_flight": false }),
    }
}

#[async_trait]
impl Tool for KnowledgeStatusTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Hub-wide indexing overview: every canonical target with its index state, every active lease with progress, every stale repository row, vector backend health, and current pass info. Use this — not knowledge.index — when you want to see what's happening across all projects. \
         Read each target's `status_reason`: `current` = fully indexed; `commits_behind` or `never_indexed` = the code graph genuinely needs (re)indexing; `summaries_pending` = the code graph IS up to date and only LLM summaries are still generating — do NOT report this as out-of-date or as new/unindexed commits; `indexing` = a pass is running now. When `summaries_enabled` is false, summaries are off by config and missing summaries are expected — never flag them as a problem. `commit_current` reports purely whether the indexed commit matches HEAD, independent of summaries."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        match self.assemble().await {
            Ok(body) => Ok(ToolResult::success(
                body,
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => {
                warn!(error = %e, "knowledge.status assemble failed");
                Ok(ToolResult::failure(
                    format!("knowledge.status failed: {e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use workforce_codebase_knowledge::lease::{IndexPhase, STALE_THRESHOLD};
    use workforce_projects::sqlite::SqliteProjectStore;
    use workforce_projects::ProjectStore;

    fn make_tool(tmp: &TempDir) -> (KnowledgeStatusTool, Arc<KnowledgeService>) {
        let ks = Arc::new(KnowledgeService::in_memory(tmp.path().to_path_buf()).unwrap());
        let store: Arc<dyn ProjectStore> = Arc::new(SqliteProjectStore::in_memory().unwrap());
        let runner = Arc::new(MaintenanceRunner::new(
            Arc::clone(&ks),
            store,
            tmp.path().to_path_buf(),
            tmp.path().join("ws"),
            BTreeMap::new(),
            None,
            1,
        ));
        let probe = Arc::new(BackendHealthProbe::new(ks.vector_store().clone()));
        (KnowledgeStatusTool::new(Arc::clone(&ks), runner, probe), ks)
    }

    fn synthetic_lease(heartbeat_ms: i64) -> IndexLease {
        IndexLease {
            repo_id: "r".into(),
            worker_id: "w".into(),
            target_commit: None,
            phase: IndexPhase::Crawling,
            acquired_at_ms: heartbeat_ms,
            last_heartbeat_at_ms: heartbeat_ms,
            entities_total: None,
            entities_processed: None,
            summaries_total: None,
            summaries_processed: None,
        }
    }

    #[tokio::test]
    async fn empty_hub_returns_empty_arrays() {
        let tmp = TempDir::new().unwrap();
        let (tool, _ks) = make_tool(&tmp);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(res.success, "{res:?}");
        assert!(res.output["targets"].as_array().unwrap().is_empty());
        assert!(res.output["active_leases"].as_array().unwrap().is_empty());
        assert!(res.output["stale_rows"].as_array().unwrap().is_empty());
        assert_eq!(res.output["pass"]["in_flight"], false);
        assert!(res.output["recent_failures"].as_array().unwrap().is_empty());
        assert_eq!(res.output["vector_backend"]["state"], "disabled");
    }

    #[tokio::test]
    async fn stale_row_classified_when_path_not_under_canonical_root() {
        let tmp = TempDir::new().unwrap();
        let (tool, ks) = make_tool(&tmp);
        ks.graph()
            .register_repo("orphan-id", "orphan", "/legacy/path/not/canonical")
            .await
            .unwrap();
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        let stale = res.output["stale_rows"].as_array().unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0]["repo_id"], "orphan-id");
        assert_eq!(stale[0]["lineage"], "unknown");
    }

    #[test]
    fn lease_view_marks_back_dated_lease_as_stale() {
        let now = now_ms();
        let back_dated = now - STALE_THRESHOLD.as_millis() as i64 - 1_000;
        let view = LeaseView::from_lease_at(&synthetic_lease(back_dated), now);
        assert!(view.is_stale);
    }

    #[test]
    fn lease_view_marks_recent_lease_as_fresh() {
        let now = now_ms();
        let view = LeaseView::from_lease_at(&synthetic_lease(now), now);
        assert!(!view.is_stale);
    }

    #[tokio::test]
    async fn live_lease_surfaces_as_fresh_in_status() {
        let tmp = TempDir::new().unwrap();
        let (tool, ks) = make_tool(&tmp);
        ks.graph().register_repo("r", "r", "/tmp").await.unwrap();
        ks.graph()
            .acquire_lease("r", "w", None, IndexPhase::Crawling)
            .await
            .unwrap();
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        let leases = res.output["active_leases"].as_array().unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0]["is_stale"], false);
    }

    #[tokio::test]
    async fn vector_backend_disabled_when_no_store() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = make_tool(&tmp);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(res.output["vector_backend"]["state"], "disabled");
    }

    #[tokio::test]
    async fn vector_backend_cached_within_ttl() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = make_tool(&tmp);
        tool.assemble().await.unwrap();
        let first = tool
            .backend_cache
            .lock()
            .await
            .as_ref()
            .unwrap()
            .captured_at;
        tool.assemble().await.unwrap();
        let second = tool
            .backend_cache
            .lock()
            .await
            .as_ref()
            .unwrap()
            .captured_at;
        assert_eq!(first, second, "second call must hit cache within TTL");
    }

    #[tokio::test]
    async fn pass_section_reports_not_in_flight_outside_pass() {
        let tmp = TempDir::new().unwrap();
        let (tool, _) = make_tool(&tmp);
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(res.output["pass"]["in_flight"], false);
    }

    #[test]
    fn classify_stale_row_preserves_lineage() {
        let row = RepositoryRow {
            id: "x".into(),
            name: "x".into(),
            root_path: "/p".into(),
            last_indexed_commit: None,
            last_indexed_at: Some("2026-01-01T00:00:00Z".into()),
            created_at: "2026-01-01T00:00:00Z".into(),
            lineage: RepoLineage::MigrationOrphan,
        };
        let stale = classify_stale_row(row);
        assert_eq!(stale.lineage, RepoLineage::MigrationOrphan);
        assert_eq!(stale.repo_id, "x");
        assert_eq!(
            stale.last_indexed_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn non_canonical_rows_appear_only_in_stale_rows() {
        let tmp = TempDir::new().unwrap();
        let (tool, ks) = make_tool(&tmp);
        ks.graph()
            .register_repo("non-canonical", "x", "/wherever")
            .await
            .unwrap();
        let res = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(res.output["stale_rows"].as_array().unwrap().len(), 1);
        assert!(res.output["targets"].as_array().unwrap().is_empty());
    }

    /// A repo whose commit is fully indexed and whose summaries are complete.
    fn completeness_at_head() -> IndexCompleteness {
        IndexCompleteness {
            repo_id: "r".into(),
            head_commit: Some("abc".into()),
            indexed_commit: Some("abc".into()),
            last_full_index_at: None,
            last_incremental_at: None,
            entities: 100,
            vectors: 100,
            summaries_expected: 50,
            summaries_present: 50,
            summaries_stale: 0,
            generate_summaries: true,
            lease: None,
        }
    }

    #[test]
    fn status_reason_current_when_at_head_with_summaries_complete() {
        assert_eq!(status_reason(&completeness_at_head(), now_ms()), "current");
    }

    #[test]
    fn status_reason_distinguishes_summary_lag_from_commit_staleness() {
        // Commits match HEAD but the LLM summary set is incomplete. This is the
        // exact state a weak model mislabelled as "new commits since last
        // index"; it must read as summaries_pending, NOT commit staleness.
        let mut c = completeness_at_head();
        c.summaries_present = 30; // 20 missing
        assert_eq!(status_reason(&c, now_ms()), "summaries_pending");

        // Once HEAD actually moves past the indexed commit, it's a real
        // reindex need.
        c.head_commit = Some("def".into());
        assert_eq!(status_reason(&c, now_ms()), "commits_behind");
    }

    #[test]
    fn status_reason_current_when_summaries_disabled_even_with_none_present() {
        // generate_summaries = false (e.g. a production install): zero
        // summaries is expected, so the repo is current, not pending.
        let mut c = completeness_at_head();
        c.generate_summaries = false;
        c.summaries_present = 0;
        assert_eq!(status_reason(&c, now_ms()), "current");
    }

    #[test]
    fn status_reason_never_indexed_when_no_commit_and_no_entities() {
        let mut c = completeness_at_head();
        c.indexed_commit = None;
        c.entities = 0;
        assert_eq!(status_reason(&c, now_ms()), "never_indexed");
    }
}
