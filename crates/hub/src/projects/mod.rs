//! Hub-side glue for the per-agent project store.
//!
//! `layout` decides where things live on disk; `fetcher`/`updater` do git
//! IO; `tools` exposes the agent-callable surface.

pub mod cc_brief;
pub mod cc_cost;
pub mod cc_quality_gate;
pub mod cc_rebase;
pub mod cc_session;
pub mod fetcher;
mod git_ops;
pub use crate::hub_core::layout;
pub mod sandbox_git;
pub mod setup;
pub mod staleness;
pub mod tools;
pub mod updater;
pub mod usage;
pub mod work_session;

pub use tools::{project_tools, ProjectsToolsContext, TOOL_NAMES};

/// Sentinel file both session sweepers use as a liveness signal. Touched on
/// every `ensure()` call (which fires once per turn or round). Using a
/// sentinel — rather than the dir's own mtime — sidesteps the fact that POSIX
/// dir mtime only updates on add/remove/rename of *direct* children: claude
/// code writes files deep inside per-repo subdirs, which wouldn't bubble up.
pub(crate) const LIVENESS_FILE: &str = ".lastseen";

/// Best-effort: write an empty [`LIVENESS_FILE`] to bump its mtime to now. A
/// failure is non-fatal — the sweeper could then reclaim a live session — so
/// log it to keep the regression observable.
pub(crate) async fn touch_liveness(session_root: &std::path::Path) {
    let path = session_root.join(LIVENESS_FILE);
    if let Err(e) = tokio::fs::write(&path, b"").await {
        tracing::warn!(path = %path.display(), error = %e, "session: failed to touch liveness file");
    }
}

/// Project a coding session falls back to when the caller names none: an
/// open bench rather than a curated repo bundle.
pub const BENCH_PROJECT: &str = "bench";

pub(crate) use crate::hub_core::layout::SCRATCH_SCOPE_PREFIX;

/// Resolve the agent's bench project, creating it on first use. It is a real
/// project row rather than hidden state, so it lists, indexes and purges like
/// any other; an existing project of that name is returned untouched, because
/// a project the user made themselves outranks the convention.
///
/// Sessions never share a working directory: the per-thread scope
/// ([`workspace_scope`]) keys the workspace, so one bench project safely backs
/// every concurrent thread.
pub fn ensure_bench_project(
    store: &dyn workforce_projects::ProjectStore,
    agent_id: &str,
) -> eyre::Result<workforce_projects::Project> {
    if let Some(existing) = store.get(agent_id, BENCH_PROJECT)? {
        return Ok(existing);
    }
    let project = workforce_projects::Project::new(agent_id, BENCH_PROJECT);
    store.put(&project)?;
    Ok(project)
}

/// The scope a Claude Code workspace is materialised under, shared by the
/// `coding.session` driver and `/cc` threads. A repo-backed workspace is keyed
/// on the transcript uuid, so a rotated or lost transcript gets a clean
/// checkout. A scratch workspace *is* the deliverable, so it is keyed on the
/// thread and outlives the transcript — every path that drops a workspace
/// passes a transcript uuid, and a scratch scope is never one.
pub(crate) fn workspace_scope(
    repo_backed: bool,
    thread_key: &str,
    transcript_uuid: &str,
) -> String {
    if repo_backed {
        transcript_uuid.to_string()
    } else {
        format!(
            "{SCRATCH_SCOPE_PREFIX}{}",
            crate::code::worktree::WorktreeManager::scope_id_for_session(thread_key)
        )
    }
}

/// Repo display alias derived from a workdir path. Used by the cc-session
/// helpers (stash, quality gate, rebase) when surfacing per-repo results
/// in Slack — the directory's leaf name is the same as `Repo.alias` for
/// project-bound sessions and a stable proxy for repo-bound ones. Falls
/// back to a generic `"workdir"` for paths with no leaf (root, `..`).
pub(crate) fn repo_alias_from_path(p: &std::path::Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "workdir".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_projects::ProjectStore;

    #[test]
    fn workspace_scope_tracks_the_transcript_only_for_repo_backed_projects() {
        let (a, b) = (
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        );
        assert_eq!(workspace_scope(true, "thread-1", a), a);
        assert_ne!(
            workspace_scope(true, "thread-1", a),
            workspace_scope(true, "thread-1", b)
        );

        // Scratch: stable across transcript rotation, distinct per thread.
        let scratch = workspace_scope(false, "thread-1", a);
        assert_eq!(scratch, workspace_scope(false, "thread-1", b));
        assert_ne!(scratch, workspace_scope(false, "thread-2", a));
        assert!(scratch.starts_with(SCRATCH_SCOPE_PREFIX));
        assert!(!scratch.contains(a));
    }

    #[test]
    fn ensure_bench_project_creates_once_and_never_overwrites() {
        let store = workforce_projects::sqlite::SqliteProjectStore::in_memory().unwrap();

        let first = ensure_bench_project(&store, "allan").unwrap();
        assert_eq!(first.name, BENCH_PROJECT);
        assert!(first.repos.is_empty());
        assert_eq!(store.list("allan").unwrap().len(), 1);

        // Idempotent, and a project the user has since populated is returned
        // as-is rather than reset to an empty bench.
        store
            .add_repo(
                "allan",
                BENCH_PROJECT,
                workforce_projects::ProjectRepo {
                    alias: "rails".into(),
                    source: workforce_projects::RepoSource::Github {
                        owner: "acme".into(),
                        repo: "rails".into(),
                    },
                    default_branch: None,
                    setup: None,
                },
            )
            .unwrap();
        let second = ensure_bench_project(&store, "allan").unwrap();
        assert_eq!(second.repos.len(), 1);
        assert_eq!(store.list("allan").unwrap().len(), 1);

        // Benches are per agent.
        ensure_bench_project(&store, "sam").unwrap();
        assert_eq!(store.list("sam").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_threads_never_share_a_bench_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bench = workforce_projects::Project::new("allan", BENCH_PROJECT);
        let uuid = "11111111-1111-4111-8111-111111111111";

        let alice = work_session::ensure(
            &layout::WorkRoots::persistent(tmp.path()),
            "allan",
            &bench,
            &workspace_scope(false, "slack:T1:C1:100.1", uuid),
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();
        let bob = work_session::ensure(
            &layout::WorkRoots::persistent(tmp.path()),
            "allan",
            &bench,
            &workspace_scope(false, "slack:T1:C2:200.2", uuid),
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        assert_ne!(alice, bob);
        tokio::fs::write(alice.join("secret.rs"), "mine")
            .await
            .unwrap();
        assert!(
            !bob.join("secret.rs").exists(),
            "one thread's files must never appear in another's workspace"
        );

        // The same thread returns to the same workspace.
        let alice_again = work_session::ensure(
            &layout::WorkRoots::persistent(tmp.path()),
            "allan",
            &bench,
            &workspace_scope(false, "slack:T1:C1:100.1", uuid),
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(alice_again, alice);
        assert!(alice_again.join("secret.rs").exists());
    }

    #[test]
    fn repo_alias_from_path_falls_back_for_leafless_paths() {
        assert_eq!(
            repo_alias_from_path(std::path::Path::new("/a/b/rails")),
            "rails"
        );
        assert_eq!(repo_alias_from_path(std::path::Path::new("/")), "workdir");
    }
}
