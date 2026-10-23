//! Global, indexer-owned checkout layout.
//!
//! Every repo we index lives under one global tree at
//! `{data_dir}/knowledge/checkouts/repos/{owner}/{repo}`, keyed by the
//! upstream `RepoSource`. The same repo referenced by multiple projects
//! (or by the standalone-discovery path AND a project) shares one
//! checkout — it gets pulled once, parsed once, indexed once.
//!
//! Agents never read or write inside this tree. It's the indexer's
//! private working area, so an in-progress agent edit can't trip up the
//! parser and an in-progress index never blocks an agent.
//!
//! The pre-DW-506 layout split this tree into `standalone/` (filesystem-
//! discovered) and `projects/{agent_id}/{project}/{alias}/` (per-agent).
//! That split caused the same upstream repo to be cloned and indexed
//! once per project — wasted disk, wasted parse, and divergent indexes
//! when projects pulled at different times. Old paths under those
//! subtrees are dead after this change; [`classify_lineage`] tags them
//! as `MigrationOrphan` so `knowledge.delete action='prune'
//! lineage_filter=["migration_orphan"]` cleans up the orphaned graph
//! + vector entries without enumerating ids by hand.
//!
//! No migration: indexes are reproducible — re-running the maintenance
//! pass repopulates the graph from the new paths.

use std::path::{Path, PathBuf};

use workforce_codebase_knowledge::graph::RepoLineage;
use workforce_projects::RepoSource;

pub fn knowledge_root(data_dir: &Path) -> PathBuf {
    data_dir.join("knowledge")
}

pub fn checkouts_root(data_dir: &Path) -> PathBuf {
    knowledge_root(data_dir).join("checkouts")
}

/// Root under which every per-repo checkout lives. Layout is
/// `repos/{owner}/{repo}`, mirroring GitHub's natural namespace.
pub fn repos_root(data_dir: &Path) -> PathBuf {
    checkouts_root(data_dir).join("repos")
}

/// Canonical on-disk location for `source`. Same answer regardless of
/// which project (or the standalone-discovery walk) found this repo —
/// that's the point of the dedup.
pub fn repo_dir(data_dir: &Path, source: &RepoSource) -> PathBuf {
    let RepoSource::Github { repo, .. } = source;
    repos_root(data_dir)
        .join(source.canonical_owner())
        .join(repo)
}

/// Classify `path` (the `root_path` of a `repositories` row) against
/// the current layout. The classifier is intentionally exact-shape:
///
/// - Exactly `repos_root(data_dir)/{owner}/{repo}` → `Canonical`.
/// - Anywhere under three documented pre-DW-506 patterns → `MigrationOrphan`.
/// - Anything else (test fixtures, ad-hoc indexes, rows written under
///   an older `data_dir`) → `Unknown`.
///
/// `data_dir` is read by value — a row written when the hub had a
/// different `data_dir` will not match the canonical pattern and falls
/// to `Unknown`. That's the conservative call: never auto-classify a
/// row as orphan when the path predates the current process's view of
/// the world.
pub fn classify_lineage(path: &Path, data_dir: &Path) -> RepoLineage {
    if is_canonical_layout(path, data_dir) {
        return RepoLineage::Canonical;
    }
    if is_pre_dw506_layout(path, data_dir) {
        return RepoLineage::MigrationOrphan;
    }
    RepoLineage::Unknown
}

fn is_canonical_layout(path: &Path, data_dir: &Path) -> bool {
    let canonical_root = repos_root(data_dir);
    let Ok(rel) = path.strip_prefix(&canonical_root) else {
        return false;
    };
    // Canonical paths are exactly `{owner}/{repo}` — two components.
    rel.components().count() == 2
}

fn is_pre_dw506_layout(path: &Path, data_dir: &Path) -> bool {
    let legacy_checkouts_projects = checkouts_root(data_dir).join("projects");
    let legacy_checkouts_standalone = checkouts_root(data_dir).join("standalone");
    let agents_root = data_dir.join("agents");
    path.starts_with(&legacy_checkouts_projects)
        || path.starts_with(&legacy_checkouts_standalone)
        || path.starts_with(&agents_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> PathBuf {
        PathBuf::from("/tmp/wf")
    }

    fn gh(owner: &str, repo: &str) -> RepoSource {
        RepoSource::Github {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    #[test]
    fn checkouts_nest_under_knowledge_root() {
        let kr = knowledge_root(&data_dir());
        let cr = checkouts_root(&data_dir());
        let r = repo_dir(&data_dir(), &gh("acme", "widgets"));

        assert!(cr.starts_with(&kr));
        assert!(r.starts_with(&cr));
    }

    #[test]
    fn same_source_resolves_to_same_dir_regardless_of_caller() {
        // The whole point of the post-DW-506 layout: a repo referenced
        // by N projects still has exactly one on-disk checkout.
        let a = repo_dir(&data_dir(), &gh("acme", "widgets"));
        let b = repo_dir(&data_dir(), &gh("acme", "widgets"));
        assert_eq!(a, b);
    }

    #[test]
    fn different_owners_do_not_collide() {
        let a = repo_dir(&data_dir(), &gh("acme", "widgets"));
        let b = repo_dir(&data_dir(), &gh("contoso", "widgets"));
        assert_ne!(a, b);
    }

    #[test]
    fn owner_case_variants_resolve_to_same_dir() {
        // GitHub owners are case-insensitive: a stray `Owner` checkout must not
        // fork a second index. Owner folds to lowercase; repo case is preserved.
        let upper = repo_dir(&data_dir(), &gh("Harborlabs", "HarborApps"));
        let lower = repo_dir(&data_dir(), &gh("harborlabs", "HarborApps"));
        assert_eq!(upper, lower);
        assert!(upper.ends_with("repos/harborlabs/HarborApps"));
    }

    #[test]
    fn repo_dir_layout_is_owner_then_repo() {
        let p = repo_dir(&data_dir(), &gh("acme", "widgets"));
        assert!(p.ends_with("repos/acme/widgets"));
    }

    #[test]
    fn classify_canonical_path_returns_canonical() {
        let p = repo_dir(&data_dir(), &gh("acme", "widgets"));
        assert_eq!(classify_lineage(&p, &data_dir()), RepoLineage::Canonical);
    }

    #[test]
    fn classify_canonical_with_extra_subdir_returns_unknown() {
        let mut p = repo_dir(&data_dir(), &gh("acme", "widgets"));
        p.push("sub");
        assert_eq!(classify_lineage(&p, &data_dir()), RepoLineage::Unknown);
    }

    #[test]
    fn classify_legacy_checkouts_projects_returns_orphan() {
        let p = data_dir().join("knowledge/checkouts/projects/allan/p/r");
        assert_eq!(
            classify_lineage(&p, &data_dir()),
            RepoLineage::MigrationOrphan
        );
    }

    #[test]
    fn classify_legacy_checkouts_standalone_returns_orphan() {
        let p = data_dir().join("knowledge/checkouts/standalone/owner/repo");
        assert_eq!(
            classify_lineage(&p, &data_dir()),
            RepoLineage::MigrationOrphan
        );
    }

    #[test]
    fn classify_agent_workdir_returns_orphan() {
        let p = data_dir().join("agents/allan/repos/owner/repo");
        assert_eq!(
            classify_lineage(&p, &data_dir()),
            RepoLineage::MigrationOrphan
        );
    }

    #[test]
    fn classify_unknown_path_returns_unknown() {
        let p = PathBuf::from("/tmp/foo/bar");
        assert_eq!(classify_lineage(&p, &data_dir()), RepoLineage::Unknown);
    }

    #[test]
    fn classify_under_different_data_dir_returns_unknown() {
        // Row written when the hub had a different data_dir falls
        // through to Unknown rather than being mis-classified as orphan.
        let other = PathBuf::from("/tmp/old-dd");
        let canonical_under_other = repo_dir(&other, &gh("acme", "widgets"));
        assert_eq!(
            classify_lineage(&canonical_under_other, &data_dir()),
            RepoLineage::Unknown
        );
    }
}
