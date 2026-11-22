//! Desk's sampler on real processes: a working session's tree and what it
//! prints are read in one listing, nothing is listed while no session
//! works, and a wait that has lasted shows on the overview.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::sight::{redactor_for_tests, Sample};
use super::tests::{new_session, options, post};
use super::{Desk, Floor, Options, SightOptions};
use crate::model::SessionView;
use crate::sight::{Kind, Wait};
use crate::store::build_pool;
use crate::store::DeskStore;

/// A stand-in CLI that prints `said`, then runs a tool shell with a long
/// `sleep` under it, as Claude Code's Bash tool does.
fn working_cli(dir: &Path, said: &str) -> PathBuf {
    let path = dir.join("working-cli");
    let script = format!(
        "#!/bin/sh\necho '{said}'\n/bin/sh -c 'sleep 600; : /h/.claude/shell-snapshots/snapshot-sh-1.sh' &\nexec sleep 60\n"
    );
    workforce_common::executable::write(&path, &script).unwrap();
    path
}

async fn desk_with(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    cli: PathBuf,
    floor: Floor,
    sight: SightOptions,
) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    Desk::start(
        DeskStore::from_pool(pool),
        Options {
            programs: crate::testing::programs(cli.clone(), cli),
            sight,
            ..options(dir)
        },
        floor,
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// Samples until `seen` holds of `id`'s sample.
async fn sampled_until(d: &Desk, id: Uuid, seen: impl Fn(&Sample) -> bool) -> Sample {
    for _ in 0..100 {
        d.sight_pass().await;
        if let Some(s) = d.sample(id).filter(|s| seen(s)) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("never sampled as expected: {:?}", d.sample(id));
}

fn kill_work(s: &Sample) {
    s.tree.iter().skip(1).for_each(|p| {
        if let Ok(pid) = i32::try_from(p.proc.pid) {
            // SAFETY: kill only signals the test's own child processes.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
}

#[tokio::test]
async fn a_working_sessions_tree_is_sampled_and_no_session_working_lists_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = working_cli(dir.path(), "building");
        let d = desk_with(
            pool,
            dir.path(),
            cli,
            Floor::default(),
            SightOptions::default(),
        )
        .await;
        d.sight_pass().await;
        assert_eq!(d.sight_reads(), 0, "nothing works, so nothing is listed");

        let s = new_session(&d, dir.path()).await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        )
        .await;
        let seen = sampled_until(&d, s.id, |s| {
            s.tree.iter().any(|p| p.proc.args == "sleep 600")
        })
        .await;
        assert!(d.sight_reads() >= 1);
        assert_eq!(seen.root, d.process_root(s.id));
        assert_eq!(seen.reading.kind, Kind::Works);
        assert_eq!(seen.reading.words, "running sleep");
        assert!(seen.output.is_empty(), "nothing is read without a redactor");
        assert_eq!(seen.tree[0].proc.pid, seen.root.unwrap(), "root first");
        let super::Processes::Tree(tree) = d.process_tree(s.id).await else {
            panic!("its tree is listed");
        };
        assert_eq!(tree.rows[0].pid, seen.root.unwrap());
        assert!(tree.rows.iter().all(|r| r.args.is_none()));
        assert_eq!(d.view(s.id).unwrap().waits, None);
        kill_work(&seen);
    })
    .await;
}

#[tokio::test]
async fn a_wait_that_has_lasted_shows_on_the_overview_and_none_shows_otherwise() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = working_cli(
            dir.path(),
            "rbuild: all 3 build slots are busy; waiting for one",
        );
        let floor = Floor {
            redactor: Some(redactor_for_tests()),
            ..Floor::default()
        };
        let sight = SightOptions {
            shown_after: Duration::ZERO,
            ..SightOptions::default()
        };
        let d = desk_with(pool, dir.path(), cli, floor, sight).await;
        let s = new_session(&d, dir.path()).await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "test it"}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "~/bin/rbuild w -- cargo test"}}),
        )
        .await;
        let seen = sampled_until(&d, s.id, |s| s.waits.is_some()).await;
        assert_eq!(seen.reading.kind, Kind::Waits(Wait::Queue));
        let v = d.view(s.id).unwrap();
        assert_eq!(v.waits.as_ref().unwrap().words, "waiting for a build slot");
        let detail = crate::overview::session_item_of(&v).detail.unwrap();
        assert!(
            detail.starts_with("● Bash · waiting for a build slot, "),
            "{detail}"
        );
        let plain = SessionView { waits: None, ..v };
        assert_eq!(
            crate::overview::session_item_of(&plain).detail.as_deref(),
            Some("● Bash")
        );
        kill_work(&seen);
    })
    .await;
}
