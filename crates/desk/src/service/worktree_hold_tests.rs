//! A worktree is one session's at a time: a start, a resume, a session
//! the terminal keeper still runs, and a folder reached through a link all
//! count; a boss's own earlier session of the same work resting there gives
//! way to its start again.

use std::path::Path;

use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, eventually, options};
use super::*;
use crate::model::{Engine, NewSession};
use crate::service::HolderOptions;
use crate::store::{build_pool, DeskStore};

fn on(repo: &Path, name: &str, branch: Option<&str>) -> NewSession {
    NewSession {
        fresh_worktree: false,
        engine: Engine::Claude,
        view: None,
        name: name.into(),
        cwd: repo.to_string_lossy().into_owned(),
        goal: String::new(),
        worktree: branch.map(Into::into),
        prompt: None,
        approvals: None,
        approve_for_me: false,
        fast: false,
        model: None,
        account: None,
        env: Default::default(),
        group: None,
        review_of: None,
    }
}

fn taken_by(e: DeskError, holder: &str) {
    assert!(
        matches!(&e, DeskError::WorktreeTaken(why) if why.contains(&format!("{holder} still runs in it"))),
        "{e:?}"
    );
}

async fn stopped(d: &Desk, id: Uuid) {
    d.stop(id, None).unwrap();
    eventually("it stopped", async || {
        (!d.record(id).unwrap().state.is_running()).then_some(())
    })
    .await;
}

fn sarah() -> Party {
    Party::manager("sarah", "Sarah")
}

/// A stopped session resumed into its worktree, which a later start has
/// taken since, is refused, naming that start's session.
#[tokio::test]
async fn a_resume_into_a_worktree_another_session_took_is_refused() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let first = d
            .create(on(repo.path(), "first", Some("man-1")), None)
            .await
            .unwrap();
        stopped(&d, first.id).await;
        let second = d
            .create(on(repo.path(), "second", Some("man-1")), None)
            .await
            .unwrap();
        assert_eq!(second.cwd, first.cwd, "the stopped one held it no more");
        taken_by(d.resume(first.id, None, None).await.unwrap_err(), "second");
        stopped(&d, second.id).await;
        d.resume(first.id, None, None).await.unwrap();
    })
    .await;
}

/// A session whose process the terminal keeper still runs, though Desk
/// lost hold of it, holds its worktree.
#[tokio::test]
async fn a_session_the_keeper_still_runs_holds_its_worktree() {
    with_test_pool(build_pool, |pool| async move {
        let (_hdir, socket, holder_cancel) = crate::holder::tests::holder().await;
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let mut o = options(home.path());
        o.holder = Some(HolderOptions {
            socket,
            token: crate::holder::tests::TOKEN.into(),
        });
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            Default::default(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let kept = d
            .create(on(repo.path(), "kept", Some("man-2")), None)
            .await
            .unwrap();
        eventually("its process runs", async || {
            d.attach(kept.id).ok().and_then(|a| a.pid())
        })
        .await;
        d.change(kept.id, |r, _| r.state = SessionState::Interrupted);
        drop(d.0.state.lock().live.remove(&kept.id));
        assert!(d.held_untaken(kept.id).await);
        taken_by(
            d.create(on(repo.path(), "next", Some("man-2")), None)
                .await
                .unwrap_err(),
            "kept",
        );
        holder_cancel.cancel();
    })
    .await;
}

/// A session working in a worktree through a link to it holds it.
#[tokio::test]
async fn a_session_in_a_worktree_through_a_link_holds_it() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let tree = crate::worktree::create(repo.path(), "man-3").await.unwrap();
        let link = home.path().join("linked");
        std::os::unix::fs::symlink(&tree, &link).unwrap();
        d.create(on(&link, "linked", None), None).await.unwrap();
        taken_by(
            d.create(on(repo.path(), "next", Some("man-3")), None)
                .await
                .unwrap_err(),
            "linked",
        );
    })
    .await;
}

/// A boss's own earlier session of the same work, resting in its worktree,
/// is stopped for the boss's start of it again; one at work there isn't,
/// and the start is refused.
#[tokio::test]
async fn a_bosss_own_resting_session_gives_way_to_its_start_again() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let by = || By::manager("sarah", "Sarah", "Its plan starts it.");
        let earlier = d
            .create_for(on(repo.path(), "MAN-4", Some("man-4")), &sarah(), by())
            .await
            .unwrap();
        taken_by(
            d.create_for(on(repo.path(), "MAN-4", Some("man-4")), &sarah(), by())
                .await
                .unwrap_err(),
            "MAN-4",
        );
        d.change(earlier.id, |r, _| r.state = SessionState::Idle);
        let again = d
            .create_for(on(repo.path(), "MAN-4", Some("man-4")), &sarah(), by())
            .await
            .unwrap();
        assert_eq!(again.cwd, earlier.cwd);
        assert!(!d.record(earlier.id).unwrap().state.is_running());
        assert_ne!(d.record(earlier.id).unwrap().state, SessionState::Closed);
    })
    .await;
}
