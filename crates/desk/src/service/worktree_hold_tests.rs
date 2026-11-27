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
        matches!(&e, DeskError::WorktreeTaken { why, holder: Some(_) } if why.contains(&format!("{holder} still runs in it"))),
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
        taken_by(
            d.resume(first.id, None, Some(By::you())).await.unwrap_err(),
            "second",
        );
        stopped(&d, second.id).await;
        d.resume(first.id, None, Some(By::you())).await.unwrap();
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

/// A boss's own earlier session of a plan's ticket, resting in its
/// worktree since the ticket went back to To do, is stopped for its start
/// again, as Desk's stop: never as an end on its own, which would hold its
/// ticket back. One at work there isn't, and the start is refused.
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
        let tree = std::path::PathBuf::from(&earlier.cwd);
        d.free_resting(&tree, "MAN-4", None, &sarah())
            .await
            .unwrap();
        assert!(d.record(earlier.id).unwrap().state.is_running(), "at work");
        taken_by(
            d.create_for(on(repo.path(), "MAN-4", Some("man-4")), &sarah(), by())
                .await
                .unwrap_err(),
            "MAN-4",
        );
        d.change(earlier.id, |r, _| r.state = SessionState::Idle);
        d.free_resting(&tree, "MAN-4", None, &sarah())
            .await
            .unwrap();
        let r = d.record(earlier.id).unwrap();
        assert_eq!(r.state, SessionState::Stopped);
        assert!(!d.0.state.lock().exited.contains_key(&earlier.id));
        assert!(d.view(earlier.id).unwrap().ended_as_started().is_none());
        let again = d
            .create_for(on(repo.path(), "MAN-4", Some("man-4")), &sarah(), by())
            .await
            .unwrap();
        assert_eq!(again.cwd, earlier.cwd);
    })
    .await;
}

/// A resting session in the worktree is left alone, and the start
/// refused, when it isn't the boss's own earlier session of the ticket
/// gone back to To do: another boss's, one the person took back, one in a
/// terminal, one the person spoke to lately, one asking, or one still the
/// ticket's.
#[tokio::test]
async fn a_resting_session_not_left_by_the_tickets_start_is_left_alone() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let by = || By::manager("sarah", "Sarah", "Its plan starts it.");
        type Unlike = fn(&Desk, Uuid) -> (Party, Option<Uuid>);
        let cases: [(&str, Unlike); 7] = [
            ("another boss's", |_, _| {
                (Party::manager("tom", "Tom"), None)
            }),
            ("taken back", |d, id| {
                let mut st = d.0.state.lock();
                let c = st.charges.rows.get_mut(&id).unwrap();
                c.released_at = Some(chrono::Utc::now());
                c.released_by = Some(crate::charge::ReleasedBy::Person);
                (sarah(), None)
            }),
            ("in a terminal", |d, id| {
                d.0.state.lock().live.get_mut(&id).unwrap().terminal = Some(Uuid::new_v4());
                (sarah(), None)
            }),
            ("spoken to lately", |d, id| {
                d.0.state
                    .lock()
                    .charges
                    .driven
                    .insert(id, chrono::Utc::now());
                (sarah(), None)
            }),
            ("at the person's wheel", |d, id| {
                let mut st = d.0.state.lock();
                let c = st.charges.rows.get_mut(&id).unwrap();
                c.wheel = crate::charge::Wheel::Person;
                c.wheel_since = Some(chrono::Utc::now());
                (sarah(), None)
            }),
            ("asking", |d, id| {
                d.change(id, |r, _| r.state = SessionState::Asking);
                (sarah(), None)
            }),
            ("still the ticket's", |_, id| (sarah(), Some(id))),
        ];
        for (n, (what, unlike)) in cases.into_iter().enumerate() {
            let branch = format!("man-{}", 10 + n);
            let s = d
                .create_for(on(repo.path(), "MAN-4", Some(&branch)), &sarah(), by())
                .await
                .unwrap();
            d.change(s.id, |r, _| r.state = SessionState::Idle);
            let (boss, assigned) = unlike(&d, s.id);
            let tree = std::path::PathBuf::from(&s.cwd);
            let assigned = assigned.map(|id| id.to_string());
            d.free_resting(&tree, "MAN-4", assigned.as_deref(), &boss)
                .await
                .unwrap();
            assert!(
                d.record(s.id).unwrap().state.is_running(),
                "{what}: left alone"
            );
            assert!(
                matches!(
                    d.create_for(on(repo.path(), "MAN-4", Some(&branch)), &sarah(), by())
                        .await
                        .unwrap_err(),
                    DeskError::WorktreeTaken { holder: Some(h), .. } if h == s.id
                ),
                "{what}: refused, naming it"
            );
        }
    })
    .await;
}

/// Desk's own restore at its launch brings back sessions that ran together
/// in a worktree; any other resume there, even by no one, is checked.
#[tokio::test]
async fn desks_own_restore_brings_back_sessions_sharing_a_worktree() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let first = d
            .create(on(repo.path(), "first", Some("man-20")), None)
            .await
            .unwrap();
        // Started there by hand, in a folder below it.
        let below = std::path::Path::new(&first.cwd).join("src");
        std::fs::create_dir_all(&below).unwrap();
        stopped(&d, first.id).await;
        let second = d.create(on(&below, "second", None), None).await.unwrap();
        taken_by(
            d.resume(first.id, None, Some(By::you())).await.unwrap_err(),
            "second",
        );
        assert!(matches!(
            d.resume(first.id, None, None).await.unwrap_err(),
            DeskError::WorktreeTaken { .. }
        ));
        d.restore_at_launch(&[first.id]).await;
        assert!(d.record(first.id).unwrap().state.is_running());
        assert!(d.record(second.id).unwrap().state.is_running());
        // Back together, neither is started again over the other: not by a
        // restart, nor by a change of its mode.
        taken_by(
            d.restart(first.id, None, By::you()).await.unwrap_err(),
            "second",
        );
        assert!(!d.record(first.id).unwrap().state.is_running());
        d.restore_at_launch(&[first.id]).await;
        assert!(d.record(first.id).unwrap().state.is_running());
        d.set_mode(first.id, crate::mode::Mode::Plan, By::you())
            .await
            .unwrap();
        assert!(
            !d.record(first.id).unwrap().state.is_running(),
            "not started again"
        );
        assert!(d.record(second.id).unwrap().state.is_running());
    })
    .await;
}

/// A stopped session restarted into its worktree, which a later start has
/// taken since, is refused; alone there, it restarts.
#[tokio::test]
async fn a_restart_into_a_worktree_another_session_took_is_refused() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let first = d
            .create(on(repo.path(), "first", Some("man-21")), None)
            .await
            .unwrap();
        stopped(&d, first.id).await;
        let second = d
            .create(on(repo.path(), "second", Some("man-21")), None)
            .await
            .unwrap();
        taken_by(
            d.restart(first.id, None, By::you()).await.unwrap_err(),
            "second",
        );
        stopped(&d, second.id).await;
        d.restart(first.id, None, By::you()).await.unwrap();
    })
    .await;
}

/// A review reads beside the session working in the worktree: it holds
/// nothing, so the work's start there goes on while it runs.
#[tokio::test]
async fn a_review_in_a_worktree_holds_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let (home, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        crate::worktree::test_repo(repo.path()).await;
        let d = desk(pool, home.path()).await;
        let review = d
            .create(on(repo.path(), "review", Some("man-22")), None)
            .await
            .unwrap();
        let of = crate::plan::ReviewOf {
            pr: crate::plan::PrRef::new("acme", "atlas", 22),
            head_sha: "a".repeat(40),
            for_session: None,
        };
        d.0.store
            .pr_review_link(review.id, "sarah", &of)
            .await
            .unwrap();
        let work = d
            .create(on(repo.path(), "work", Some("man-22")), None)
            .await
            .unwrap();
        assert_eq!(work.cwd, review.cwd);
        assert!(d.record(review.id).unwrap().state.is_running());
    })
    .await;
}
