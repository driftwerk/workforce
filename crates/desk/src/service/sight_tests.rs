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
use super::tests::{fake_cli, new_session, options, post};
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

const CLAUDE: &str = include_str!("../../fixtures/ledger/claude.jsonl");

/// A closed session in `cwd` whose transcript is the fixture, in `home`'s
/// Claude Code projects.
fn kept(home: &Path, name: &str, cwd: &Path) -> crate::model::SessionRecord {
    let mut r = crate::store::tests::record_fixture(name);
    let project = home.join(".claude/projects").join(format!("-{name}"));
    std::fs::create_dir_all(&project).unwrap();
    let t = project.join(format!("{}.jsonl", r.cli_session_id.as_deref().unwrap()));
    std::fs::write(&t, CLAUDE).unwrap();
    r.cwd = cwd.to_string_lossy().into_owned();
    r.transcript = Some(t.to_string_lossy().into_owned());
    r.state = crate::model::SessionState::Closed;
    r
}

fn redacting() -> Floor {
    Floor {
        redactor: Some(redactor_for_tests()),
        ..Floor::default()
    }
}

#[tokio::test]
async fn a_transcript_is_paged_where_the_person_lets_desk_read_it_and_refused_in_words_otherwise() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let store = DeskStore::from_pool(pool.clone());
        DeskStore::migrate(&pool).await.unwrap();
        let web = kept(home, "web", &home.join("web"));
        let turn = kept(
            home,
            "turn",
            &home.join("agents/scout/projects/_agent-turn"),
        );
        let secret = kept(home, "secret", &home.join("secret"));
        let off = kept(home, "off", &home.join("off"));
        let gone = kept(home, "gone", &home.join("gone"));
        let mut devin = kept(home, "devin", &home.join("devin"));
        devin.engine = crate::model::Engine::Devin;
        devin.transcript = None;
        for r in [&web, &turn, &secret, &off, &gone, &devin] {
            store.put(r).await.unwrap();
        }
        store.set_session_memory(off.id, false).await.unwrap();
        let mut opts = options(home);
        opts.env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        opts.memory.exclude = vec![home.join("secret").to_string_lossy().into_owned()];
        let d = Desk::start(
            DeskStore::from_pool(pool.clone()),
            opts.clone(),
            redacting(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        d.forget_memory(gone.id).await.unwrap();

        let page = d.transcript_page(web.id, Default::default()).await.unwrap();
        assert!(!page.exchanges.is_empty());
        assert_eq!(page.records[0].name, "main");
        let refused = |id| {
            let d = d.clone();
            async move { d.transcript_page(id, Default::default()).await.unwrap_err() }
        };
        assert_eq!(refused(turn.id).await, super::sight::AGENT_TURN);
        assert_eq!(refused(secret.id).await, super::sight::KEPT_OUT);
        assert_eq!(refused(off.id).await, super::sight::KEPT_OUT);
        assert_eq!(refused(gone.id).await, super::sight::KEPT_OUT);
        assert_eq!(
            refused(devin.id).await,
            "Devin keeps no transcript Desk can read."
        );
        let stale = super::TranscriptAsk {
            before: Some(crate::sight::transcript::cursor("other-file", 10)),
            ..Default::default()
        };
        assert_eq!(
            d.transcript_page(web.id, stale).await.unwrap_err(),
            super::sight::REPLACED
        );
        let unknown = super::TranscriptAsk {
            record: Some("../../etc/passwd".into()),
            ..Default::default()
        };
        assert_eq!(
            d.transcript_page(web.id, unknown).await.unwrap_err(),
            super::sight::NO_SUCH_RECORD
        );
    })
    .await;
}

#[tokio::test]
async fn without_a_redactor_no_transcript_and_no_logs_are_shown() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let store = DeskStore::from_pool(pool.clone());
        DeskStore::migrate(&pool).await.unwrap();
        let web = kept(home, "web", &home.join("web"));
        store.put(&web).await.unwrap();
        let sight = SightOptions {
            logs: Some(home.to_path_buf()),
            ..SightOptions::default()
        };
        let d = desk_with(pool, home, fake_cli(home), Floor::default(), sight).await;
        assert_eq!(
            d.transcript_page(web.id, Default::default())
                .await
                .unwrap_err(),
            super::sight::NO_REDACTOR
        );
        assert_eq!(
            d.session_logs(web.id, None).await.unwrap_err(),
            super::sight::LOGS_NO_REDACTOR
        );
    })
    .await;
}

#[tokio::test]
async fn a_sessions_own_log_lines_are_read_from_desks_logs_and_none_without_them() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let store = DeskStore::from_pool(pool.clone());
        DeskStore::migrate(&pool).await.unwrap();
        let r = kept(dir.path(), "web", dir.path());
        store.put(&r).await.unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        std::fs::write(
            logs.join("hub.log"),
            format!(
                "{now}  WARN hub: session {} couldn't read SECRET-abc\n{now}  INFO hub: another\n",
                r.id
            ),
        )
        .unwrap();
        let sight = SightOptions {
            logs: Some(logs),
            ..SightOptions::default()
        };
        let d = desk_with(pool, dir.path(), fake_cli(dir.path()), redacting(), sight).await;
        let read = d.session_logs(r.id, None).await.unwrap();
        assert_eq!(read.lines.len(), 1);
        assert!(read.lines[0].text.contains("[secret]"));
    })
    .await;
}

#[tokio::test]
async fn a_hub_with_no_desk_logs_says_so() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path());
        let d = desk_with(pool, dir.path(), cli, redacting(), SightOptions::default()).await;
        let s = new_session(&d, dir.path()).await;
        assert_eq!(
            d.session_logs(s.id, None).await.unwrap_err(),
            super::sight::NO_LOGS
        );
    })
    .await;
}
