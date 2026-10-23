//! Project-store-derived index usage primitives.
//!
//! Post-DW-506 the canonical knowledge index for an upstream repo
//! lives at one shared on-disk path keyed by `RepoSource::canonical_slug`.
//! Multiple projects can name the same upstream — and several pieces of
//! code need to know "which projects still reference this repo?":
//!
//! - `projects.delete` / `projects.remove_repo` use it to decide
//!   whether their `purge=true` cascade can drop the shared
//!   index/checkout, and to populate DW-509 user-facing warnings about
//!   surviving siblings vs newly orphaned repos.
//! - `knowledge.delete` (DW-508) uses it to fail-loud when a delete
//!   would silently break a project's index.
//!
//! The project store is the source of truth — there is no separate
//! registry file. Walking `store.list_all()` and grouping by canonical
//! slug is cheap (project lists are tiny: dozens, not millions) and
//! free of the schema-versioning / atomic-write concerns a parallel
//! registry would introduce.

use std::collections::HashMap;

use eyre::Result;
use serde::Serialize;
use workforce_projects::ProjectStore;

/// One project's reference to a repo. Surfaces in user-facing warnings
/// so the user can see *which* sibling projects pin a shared index, not
/// just *that* it's pinned.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectReference {
    pub agent_id: String,
    pub project: String,
    pub alias: String,
}

/// Map every `canonical_slug` named by any project to the list of
/// projects (cross-agent) that name it. Empty map when the store is
/// empty. Each list is in `store.list_all()` order — already sorted by
/// `(agent_id, name)` so the output is deterministic.
pub fn project_references_by_slug(
    store: &dyn ProjectStore,
) -> Result<HashMap<String, Vec<ProjectReference>>> {
    let mut out: HashMap<String, Vec<ProjectReference>> = HashMap::new();
    for project in store.list_all()? {
        for repo in &project.repos {
            out.entry(repo.source.canonical_slug())
                .or_default()
                .push(ProjectReference {
                    agent_id: project.agent_id.clone(),
                    project: project.name.clone(),
                    alias: repo.alias.clone(),
                });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use workforce_projects::sqlite::SqliteProjectStore;
    use workforce_projects::{Project, ProjectRepo, RepoSource};

    fn store() -> Arc<dyn ProjectStore> {
        Arc::new(SqliteProjectStore::in_memory().unwrap())
    }

    fn repo(alias: &str, owner: &str, repo: &str) -> ProjectRepo {
        ProjectRepo {
            alias: alias.into(),
            source: RepoSource::Github {
                owner: owner.into(),
                repo: repo.into(),
            },
            default_branch: None,
            setup: None,
        }
    }

    #[test]
    fn references_group_cross_agent_under_one_slug() {
        let s = store();
        let mut a = Project::new("allan", "harbor");
        a.repos.push(repo("rails", "acme", "shared"));
        s.put(&a).unwrap();
        let mut b = Project::new("rita", "fabrikam");
        b.repos.push(repo("the-tool", "acme", "shared"));
        s.put(&b).unwrap();

        let by_slug = project_references_by_slug(s.as_ref()).unwrap();
        let refs = by_slug.get("acme/shared").unwrap();
        assert_eq!(refs.len(), 2);
        // list_all() is sorted by (agent_id, name) so allan < rita.
        assert_eq!(refs[0].agent_id, "allan");
        assert_eq!(refs[0].alias, "rails");
        assert_eq!(refs[1].agent_id, "rita");
        assert_eq!(refs[1].alias, "the-tool");
    }

    #[test]
    fn empty_store_yields_empty_map() {
        let s = store();
        assert!(project_references_by_slug(s.as_ref()).unwrap().is_empty());
    }
}
