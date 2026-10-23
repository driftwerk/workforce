//! Re-classify `repositories` rows whose `lineage` column is still
//! `unknown` after a schema migration. Runs once at hub startup
//! (after `KnowledgeService` construction, before tools are wired)
//! so the agent-facing surface sees typed rows from first request.
//!
//! Idempotent: rows that resolve to `Unknown` stay `Unknown`, and a
//! second invocation reports zero updates.

use std::path::Path;
use std::sync::Arc;

use eyre::Result;
use tracing::info;
use workforce_codebase_knowledge::graph::{CodeGraph, RepoLineage};

use super::index_layout::classify_lineage;

/// Walk every row currently tagged `Unknown` and re-classify against
/// the current layout. Returns the number of rows actually updated
/// (rows that would stay `Unknown` are skipped to avoid a write).
pub async fn backfill_lineage(graph: &Arc<CodeGraph>, data_dir: &Path) -> Result<usize> {
    let unknown = graph
        .list_repositories_by_lineage(RepoLineage::Unknown)
        .await?;
    let updates: Vec<(String, RepoLineage)> = unknown
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                classify_lineage(Path::new(&r.root_path), data_dir),
            )
        })
        .filter(|(_, lineage)| *lineage != RepoLineage::Unknown)
        .collect();
    let touched = graph.set_repo_lineage_batch(&updates).await?;
    info!(
        candidates = unknown.len(),
        updated = touched,
        "repositories.lineage backfill complete"
    );
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::knowledge::index_layout::repo_dir;
    use workforce_projects::RepoSource;

    fn gh(owner: &str, repo: &str) -> RepoSource {
        RepoSource::Github {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    async fn seed_repo(graph: &CodeGraph, id: &str, root: &Path) {
        graph
            .register_repo(id, id, &root.to_string_lossy())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn classifies_existing_unknown_rows() {
        let tmp = TempDir::new().unwrap();
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());

        let canonical_path = repo_dir(tmp.path(), &gh("acme", "widgets"));
        let orphan_path = tmp.path().join("knowledge/checkouts/standalone/x/y");
        let unknown_path = std::path::PathBuf::from("/tmp/elsewhere/abc");

        seed_repo(&graph, "canonical-id", &canonical_path).await;
        seed_repo(&graph, "orphan-id", &orphan_path).await;
        seed_repo(&graph, "unknown-id", &unknown_path).await;

        let touched = backfill_lineage(&graph, tmp.path()).await.unwrap();
        assert_eq!(touched, 2);

        let all = graph.list_repositories().await.unwrap();
        let by_id: std::collections::HashMap<_, _> =
            all.iter().map(|r| (r.id.as_str(), r.lineage)).collect();
        assert_eq!(by_id["canonical-id"], RepoLineage::Canonical);
        assert_eq!(by_id["orphan-id"], RepoLineage::MigrationOrphan);
        assert_eq!(by_id["unknown-id"], RepoLineage::Unknown);
    }

    #[tokio::test]
    async fn is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        seed_repo(
            &graph,
            "canonical-id",
            &repo_dir(tmp.path(), &gh("acme", "widgets")),
        )
        .await;
        let first = backfill_lineage(&graph, tmp.path()).await.unwrap();
        let second = backfill_lineage(&graph, tmp.path()).await.unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0);
    }

    #[tokio::test]
    async fn does_not_touch_already_classified_rows() {
        let tmp = TempDir::new().unwrap();
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        let path = repo_dir(tmp.path(), &gh("acme", "widgets"));
        seed_repo(&graph, "canonical-id", &path).await;
        graph
            .set_repo_lineage("canonical-id", RepoLineage::Canonical)
            .await
            .unwrap();
        let touched = backfill_lineage(&graph, tmp.path()).await.unwrap();
        assert_eq!(touched, 0);
    }
}
