//! On-disk layout for per-agent projects.
//!
//! Two areas — `work/` (agent tools) and `cc-sessions/` (claude-code
//! per-session copies) — sit under one project root keyed by
//! `(agent_id, name)`. The knowledge indexer owns its own checkouts at a
//! global location (see `hub::knowledge::index_layout`); deliberately not
//! per-agent so a single index covers every repo across the hub.
//! Returning paths *without* creating them is deliberate: callers own the
//! IO so this module stays a single, pure source of truth for where
//! things live. Inputs are assumed safe — see `tools::validate_safe_name`
//! for the path-traversal guard at the entry points.

use std::path::{Path, PathBuf};

/// Prefix of a scratch (bench) workspace scope. Such a workspace is the
/// deliverable, so it stays on the persistent data volume however the hot
/// worktree root is configured — see [`layout::WorkRoots`].
pub const SCRATCH_SCOPE_PREFIX: &str = "scratch-";

/// Where a project's `work/` trees live. `data_dir` holds everything
/// persistent — scratch benches, and every work tree when no hot root is
/// configured. `hot`, when set, holds a second copy of each project's work
/// tree (bare bases and session worktrees together) for repo-backed scopes:
/// build output there is dominated by small-file latency, so a network
/// `data_dir` makes every install and build many times slower. Bases move
/// with the worktrees so a git worktree registry is never shared across
/// instances. The hot root may be instance-local, which is why scratch
/// benches, the agent's deliverable, never move there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRoots {
    pub data_dir: PathBuf,
    pub hot: Option<PathBuf>,
}

impl WorkRoots {
    /// Roots from `[hub] data_dir` and the optional `[hub] worktree_root`.
    pub fn new(data_dir: PathBuf, worktree_root: Option<&Path>) -> Self {
        Self {
            data_dir,
            hot: worktree_root.map(|root| root.join("work")),
        }
    }

    pub fn from_hub(hub: &workforce_common::settings::HubConfig) -> Self {
        Self::new(PathBuf::from(&hub.data_dir), hub.worktree_root_path())
    }

    /// Everything under `data_dir`, as before a hot root existed.
    pub fn persistent(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            hot: None,
        }
    }

    fn hot_work_root(&self, agent_id: &str, project: &str) -> Option<PathBuf> {
        self.hot
            .as_ref()
            .map(|hot| hot.join(agent_id).join(project))
    }

    fn persistent_work_root(&self, agent_id: &str, project: &str) -> PathBuf {
        work_root(&self.data_dir, agent_id, project)
    }
}

pub fn is_scratch_scope(scope_id: &str) -> bool {
    scope_id.starts_with(SCRATCH_SCOPE_PREFIX)
}

/// The work tree scope `scope_id` lives in: the hot root's copy for a
/// repo-backed scope when one is configured, the persistent one for a scratch
/// bench or when there is no hot root.
pub fn work_root_for_scope(
    roots: &WorkRoots,
    agent_id: &str,
    project: &str,
    scope_id: &str,
) -> PathBuf {
    roots
        .hot_work_root(agent_id, project)
        .filter(|_| !is_scratch_scope(scope_id))
        .unwrap_or_else(|| roots.persistent_work_root(agent_id, project))
}

/// Every work tree `project` may have — the hot copy first when configured,
/// then the persistent one. Sweepers and orphan cleanup walk both.
pub fn work_trees(roots: &WorkRoots, agent_id: &str, project: &str) -> Vec<PathBuf> {
    roots
        .hot_work_root(agent_id, project)
        .into_iter()
        .chain(std::iter::once(
            roots.persistent_work_root(agent_id, project),
        ))
        .collect()
}

/// Every work tree that may hold `scope_id`: its own, plus the persistent one
/// a scope materialised before a hot root was configured still sits in.
pub fn work_roots_for_teardown(
    roots: &WorkRoots,
    agent_id: &str,
    project: &str,
    scope_id: &str,
) -> Vec<PathBuf> {
    let mut trees = vec![work_root_for_scope(roots, agent_id, project, scope_id)];
    let persistent = roots.persistent_work_root(agent_id, project);
    if persistent != trees[0] {
        trees.push(persistent);
    }
    trees
}

pub fn sessions_dir_of(work_root: &Path) -> PathBuf {
    work_root.join("session")
}

pub fn base_dir_of(work_root: &Path, alias: &str) -> PathBuf {
    work_root.join("bases").join(format!("{alias}.git"))
}

pub fn work_base_dir_for(
    roots: &WorkRoots,
    agent_id: &str,
    project: &str,
    scope_id: &str,
    alias: &str,
) -> PathBuf {
    base_dir_of(
        &work_root_for_scope(roots, agent_id, project, scope_id),
        alias,
    )
}

pub fn work_session_dir(
    roots: &WorkRoots,
    agent_id: &str,
    project: &str,
    scope_id: &str,
) -> PathBuf {
    sessions_dir_of(&work_root_for_scope(roots, agent_id, project, scope_id)).join(scope_id)
}

pub fn work_session_repo_dir(
    roots: &WorkRoots,
    agent_id: &str,
    project: &str,
    scope_id: &str,
    alias: &str,
) -> PathBuf {
    work_session_dir(roots, agent_id, project, scope_id).join(alias)
}

pub fn agent_root(data_dir: &Path, agent_id: &str) -> PathBuf {
    data_dir.join("agents").join(agent_id)
}

/// An agent's workspace: its configured `workspace`, else [`agent_root`].
pub fn agent_workspace(data_dir: &Path, agent_id: &str, configured: Option<&str>) -> PathBuf {
    configured.map_or_else(|| agent_root(data_dir, agent_id), PathBuf::from)
}

pub fn projects_root(data_dir: &Path, agent_id: &str) -> PathBuf {
    agent_root(data_dir, agent_id).join("projects")
}

pub fn project_root(data_dir: &Path, agent_id: &str, project: &str) -> PathBuf {
    projects_root(data_dir, agent_id).join(project)
}

pub fn work_root(data_dir: &Path, agent_id: &str, project: &str) -> PathBuf {
    project_root(data_dir, agent_id, project).join("work")
}

pub fn work_clones_dir(data_dir: &Path, agent_id: &str, project: &str) -> PathBuf {
    work_root(data_dir, agent_id, project).join("clones")
}

pub fn work_clone_dir(data_dir: &Path, agent_id: &str, project: &str, alias: &str) -> PathBuf {
    work_clones_dir(data_dir, agent_id, project).join(alias)
}

pub fn work_bases_dir(data_dir: &Path, agent_id: &str, project: &str) -> PathBuf {
    work_root(data_dir, agent_id, project).join("bases")
}

pub fn work_base_dir(data_dir: &Path, agent_id: &str, project: &str, alias: &str) -> PathBuf {
    base_dir_of(&work_root(data_dir, agent_id, project), alias)
}

pub fn cc_sessions_root(data_dir: &Path, agent_id: &str, project: &str) -> PathBuf {
    project_root(data_dir, agent_id, project).join("cc-sessions")
}

pub fn cc_session_dir(
    data_dir: &Path,
    agent_id: &str,
    project: &str,
    cc_session_uuid: &str,
) -> PathBuf {
    cc_sessions_root(data_dir, agent_id, project).join(cc_session_uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> PathBuf {
        PathBuf::from("/tmp/wf")
    }

    #[test]
    fn project_root_nests_under_agent() {
        let p = project_root(&data_dir(), "allan", "harbor");
        assert!(p.starts_with(agent_root(&data_dir(), "allan")));
    }

    #[test]
    fn work_paths_nest_under_project_root() {
        let pr = project_root(&data_dir(), "allan", "harbor");
        let wr = work_root(&data_dir(), "allan", "harbor");
        let wc = work_clones_dir(&data_dir(), "allan", "harbor");
        let alias = work_clone_dir(&data_dir(), "allan", "harbor", "rails");
        let bases = work_bases_dir(&data_dir(), "allan", "harbor");

        assert!(wr.starts_with(&pr));
        assert!(wc.starts_with(&wr));
        assert!(alias.starts_with(&wc));
        assert!(bases.starts_with(&wr));
    }

    #[test]
    fn work_session_paths_nest_under_work_root_without_a_hot_root() {
        let roots = WorkRoots::persistent(data_dir());
        let wr = work_root(&data_dir(), "allan", "harbor");
        let wsd = work_session_dir(&roots, "allan", "harbor", "wf-1");
        let wsrd = work_session_repo_dir(&roots, "allan", "harbor", "wf-1", "rails");
        assert_eq!(work_trees(&roots, "allan", "harbor"), vec![wr.clone()]);
        assert!(wsd.starts_with(wr.join("session")));
        assert!(wsrd.starts_with(&wsd));
        assert_eq!(
            work_base_dir_for(&roots, "allan", "harbor", "wf-1", "rails"),
            work_base_dir(&data_dir(), "allan", "harbor", "rails")
        );
        assert_eq!(
            work_roots_for_teardown(&roots, "allan", "harbor", "wf-1"),
            vec![wr]
        );
    }

    #[test]
    fn a_hot_root_takes_repo_scopes_and_their_bases_but_never_scratch_benches() {
        let roots = WorkRoots::new(data_dir(), Some(Path::new("/fast")));
        let hot = PathBuf::from("/fast/work/allan/harbor");
        let persistent = work_root(&data_dir(), "allan", "harbor");

        assert_eq!(
            work_session_dir(&roots, "allan", "harbor", "wf-1"),
            hot.join("session").join("wf-1")
        );
        assert_eq!(
            work_session_repo_dir(&roots, "allan", "harbor", "wf-1", "rails"),
            hot.join("session").join("wf-1").join("rails")
        );
        assert_eq!(
            work_base_dir_for(&roots, "allan", "harbor", "wf-1", "rails"),
            hot.join("bases").join("rails.git"),
            "bases move with the worktrees so no registry is shared across instances"
        );
        let scratch = format!("{SCRATCH_SCOPE_PREFIX}abc");
        assert_eq!(
            work_session_dir(&roots, "allan", "harbor", &scratch),
            persistent.join("session").join(&scratch)
        );
        assert_eq!(
            work_trees(&roots, "allan", "harbor"),
            vec![hot.clone(), persistent.clone()]
        );
        assert_eq!(
            work_roots_for_teardown(&roots, "allan", "harbor", "wf-1"),
            vec![hot, persistent.clone()],
            "a scope from before the hot root existed is still found for teardown"
        );
        assert_eq!(
            work_roots_for_teardown(&roots, "allan", "harbor", &scratch),
            vec![persistent]
        );
    }

    #[test]
    fn from_hub_reads_the_data_dir_and_worktree_root() {
        let hub = workforce_common::settings::HubConfig {
            data_dir: "/data".into(),
            worktree_root: Some("/fast".into()),
            ..Default::default()
        };
        assert_eq!(
            WorkRoots::from_hub(&hub),
            WorkRoots::new(PathBuf::from("/data"), Some(Path::new("/fast")))
        );
        assert_eq!(
            WorkRoots::from_hub(&workforce_common::settings::HubConfig {
                data_dir: "/data".into(),
                ..Default::default()
            }),
            WorkRoots::persistent("/data")
        );
    }

    #[test]
    fn work_base_path_uses_alias_dot_git() {
        let b = work_base_dir(&data_dir(), "allan", "harbor", "rails");
        assert!(b.ends_with("bases/rails.git"));
    }

    #[test]
    fn cc_session_paths_nest_under_project_root() {
        let pr = project_root(&data_dir(), "allan", "harbor");
        let cr = cc_sessions_root(&data_dir(), "allan", "harbor");
        let cs = cc_session_dir(&data_dir(), "allan", "harbor", "01662c82");
        assert!(cr.starts_with(&pr));
        assert!(cs.starts_with(&cr));
    }

    #[test]
    fn projects_for_different_agents_dont_collide() {
        let a = project_root(&data_dir(), "allan", "harbor");
        let b = project_root(&data_dir(), "rita", "harbor");
        assert_ne!(a, b);
    }
}
