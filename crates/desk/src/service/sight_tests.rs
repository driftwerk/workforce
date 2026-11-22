//! Desk's sampler on real processes: a working session's tree and what it
//! prints are read in one listing, nothing is listed while no session
//! works, and a wait its open call says, once it has lasted, shows on the
//! overview.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::sight::{redactor_for_tests, Sample};
use super::tests::{fake_cli, new_session, options, post};
use super::{Desk, Floor, Options, Processes, SightOptions};
use crate::model::{SessionState, SessionView};
use crate::procs::is_tool_shell;
use crate::sight::{Kind, Wait};
use crate::store::build_pool;
use crate::store::DeskStore;

const TOOL_SHELL: &str = "/bin/sh -c 'sleep 600; : /h/.claude/shell-snapshots/snapshot-sh-1.sh' &";

/// A stand-in CLI that prints `said`, runs a tool shell with a long `sleep`
/// under it, as Claude Code's Bash tool does, then runs `then`.
fn cli_running(dir: &Path, said: &str, then: &str) -> PathBuf {
    let path = dir.join("working-cli");
    let script = format!("#!/bin/sh\necho '{said}'\n{TOOL_SHELL}\n{then}\n");
    workforce_common::executable::write(&path, &script).unwrap();
    path
}

fn working_cli(dir: &Path, said: &str) -> PathBuf {
    cli_running(dir, said, "exec sleep 60")
}

fn options_with(dir: &Path, cli: PathBuf, sight: SightOptions) -> Options {
    Options {
        programs: crate::testing::programs(cli.clone(), cli),
        sight,
        ..options(dir)
    }
}

async fn desk_with(pool: workforce_postgres::PostgresPool, opts: Options, floor: Floor) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    Desk::start(
        DeskStore::from_pool(pool),
        opts,
        floor,
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// A Desk in `dir` whose CLI is `cli`, sighted as `sight`.
async fn desk_on(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    cli: PathBuf,
    floor: Floor,
    sight: SightOptions,
) -> Desk {
    let opts = Options {
        programs: crate::testing::programs(cli.clone(), cli),
        sight,
        ..options(dir)
    };
    desk_with(pool, opts, floor).await
}

/// Sight that shows a wait at once and reads task output under `dir`.
fn sight_in(dir: &Path) -> SightOptions {
    SightOptions {
        task_root: dir.join("claude-tmp"),
        shown_after: Duration::ZERO,
        ..SightOptions::default()
    }
}

fn redacting() -> Floor {
    Floor {
        redactor: Some(redactor_for_tests()),
        ..Floor::default()
    }
}

/// Gives session `id` a transcript whose newest Bash call, started a
/// minute ago, is still open, and sets it working on it: its task output
/// folder under `sight_in(dir)`, made.
async fn working_on_a_bash_call(d: &Desk, dir: &Path, id: Uuid) -> PathBuf {
    let project = dir.join("projects").join("-p");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join(format!("{id}.jsonl"));
    let since = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let call = json!({"type": "assistant", "timestamp": since, "uuid": "u1",
        "message": {"stop_reason": "tool_use", "content": [{"type": "tool_use",
        "id": "toolu_1", "name": "Bash", "input": {"command": "~/bin/rbuild w -- cargo test"}}]}});
    std::fs::write(&transcript, format!("{call}\n")).unwrap();
    let tasks = dir
        .join("claude-tmp")
        .join("-p")
        .join(id.to_string())
        .join("tasks");
    std::fs::create_dir_all(&tasks).unwrap();
    post(
        d,
        id,
        json!({"hook_event_name": "SessionStart", "session_id": id.to_string(), "transcript_path": transcript}),
    )
    .await;
    post(
        d,
        id,
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "test it"}),
    )
    .await;
    post(
        d,
        id,
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
        "tool_input": {"command": "~/bin/rbuild w -- cargo test"}}),
    )
    .await;
    tasks
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::File::options()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn has_shell(s: &Sample) -> bool {
    s.tree.iter().any(|p| p.proc.args == "sleep 600")
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
        let opts = options_with(dir.path(), cli, SightOptions::default());
        let d = desk_with(pool, opts, Floor::default()).await;
        d.sight_pass().await;
        assert_eq!(d.sight_reads(), 0, "nothing works, so nothing is listed");

        let s = new_session(&d, dir.path()).await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        )
        .await;
        let seen = sampled_until(&d, s.id, has_shell).await;
        assert!(d.sight_reads() >= 1);
        assert_eq!(Some(seen.root), d.process_root(s.id));
        assert_eq!(seen.reading.kind, Kind::Works);
        assert_eq!(seen.reading.words, "running sleep");
        assert!(seen.output.is_empty(), "nothing is read without a redactor");
        assert_eq!(seen.tree[0].proc.pid, seen.root, "root first");
        let super::Processes::Tree(tree) = d.process_tree(s.id).await else {
            panic!("its tree is listed");
        };
        assert_eq!(tree.rows[0].pid, seen.root);
        assert!(tree.rows.iter().all(|r| r.args.is_none()));
        assert_eq!(d.view(s.id).unwrap().waits, None);
        kill_work(&seen);
    })
    .await;
}

#[tokio::test]
async fn only_a_new_line_of_the_open_calls_own_output_says_a_wait_and_it_shows_on_the_overview() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = working_cli(dir.path(), "Now waiting on CI before the merge");
        let opts = options_with(dir.path(), cli, sight_in(dir.path()));
        let d = desk_with(pool, opts, redacting()).await;
        let s = new_session(&d, dir.path()).await;
        let tasks = working_on_a_bash_call(&d, dir.path(), s.id).await;
        let own = tasks.join("b1.output");
        append(&own, "rbuild: all 3 build slots are busy; waiting for one\n");
        let agent = dir.path().join("agent.jsonl");
        append(&agent, "waiting for the reviewer\n");
        std::os::unix::fs::symlink(&agent, tasks.join("a1.output")).unwrap();

        let first = sampled_until(&d, s.id, |s| has_shell(s) && s.open_call.is_some()).await;
        assert_eq!(first.open_call.as_ref().unwrap().id, "toolu_1");
        assert!(
            first.output.iter().any(|l| l.contains("waiting on CI")),
            "{:?}",
            first.output
        );
        let busy = sampled_until(&d, s.id, |s| s.at > first.at).await;
        assert_eq!(
            busy.reading.kind,
            Kind::Works,
            "the screen's prose, a sub-agent's transcript and a line there before Desk looked are no wait"
        );
        assert_eq!(d.view(s.id).unwrap().waits, None);

        append(&agent, "waiting for the reviewer again\n");
        let before = chrono::Utc::now();
        append(&own, "   Compiling x\nrbuild: all 3 build slots are busy; waiting for one\n");
        let seen = sampled_until(&d, s.id, |s| s.waits.is_some()).await;
        assert_eq!(seen.reading.kind, Kind::Waits(Wait::Queue));
        assert_eq!(seen.reading.call.as_deref(), Some("toolu_1"));
        let shell = seen.tree.iter().find(|p| is_tool_shell(&p.proc)).unwrap();
        assert_eq!(seen.reading.pid, Some(shell.proc.pid), "the waiting call's own shell");
        assert!(seen.reading.since >= before, "since the line appeared");
        let v = d.view(s.id).unwrap();
        assert_eq!(v.waits.as_ref().unwrap().words, "waiting for a build slot");
        let tool = v.current_tool.clone().unwrap();
        assert!(tool.starts_with("Bash"), "{tool}");
        let detail = crate::overview::session_item_of(&v).detail.unwrap();
        assert!(
            detail.starts_with(&format!("● {tool} · waiting for a build slot, ")),
            "{detail}"
        );
        let plain = SessionView { waits: None, ..v };
        assert_eq!(
            crate::overview::session_item_of(&plain).detail,
            Some(format!("● {tool}"))
        );

        append(&own, "test a ... ok\n");
        let after = sampled_until(&d, s.id, |s| s.waits.is_none()).await;
        assert_eq!(after.reading.kind, Kind::Works, "a newer line that says no wait ends it");
        kill_work(&after);
    })
    .await;
}

#[tokio::test]
async fn a_background_shells_own_new_line_says_its_wait_once_the_turn_has_ended() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = working_cli(dir.path(), "Started the tests in the background");
        let opts = options_with(dir.path(), cli, sight_in(dir.path()));
        let d = desk_with(pool, opts, redacting()).await;
        let s = new_session(&d, dir.path()).await;
        let tasks = working_on_a_bash_call(&d, dir.path(), s.id).await;
        let transcript = dir
            .path()
            .join("projects")
            .join("-p")
            .join(format!("{}.jsonl", s.id));
        let at = chrono::Utc::now().to_rfc3339();
        let result = json!({"type": "user", "timestamp": at, "uuid": "u2",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1",
            "content": "Command running in background with ID: b9"}]}});
        let ended = json!({"type": "assistant", "timestamp": at, "uuid": "u3",
            "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "Started."}]}});
        append(&transcript, &format!("{result}\n{ended}\n"));
        let shell = tasks.join("b9.output");
        append(&shell, "rbuild: all 3 build slots are busy; waiting for one\n");
        post(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "background_tasks": [
                {"id": "b9", "type": "shell", "status": "running", "command": "rbuild w -- cargo test"}]}),
        )
        .await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);

        let first = sampled_until(&d, s.id, has_shell).await;
        assert_eq!(first.open_call, None, "its turn ended");
        let next = sampled_until(&d, s.id, |s| s.at > first.at).await;
        assert_ne!(
            next.reading.kind,
            Kind::Waits(Wait::Queue),
            "a line there before Desk looked is no wait"
        );

        let before = chrono::Utc::now();
        append(&shell, "   Compiling x\nrbuild: all 3 build slots are busy; waiting for one\n");
        let seen = sampled_until(&d, s.id, |s| s.waits.is_some()).await;
        assert_eq!(seen.reading.kind, Kind::Waits(Wait::Queue));
        assert_eq!(seen.reading.words, "waiting for a build slot");
        assert_eq!(seen.reading.call.as_deref(), Some("b9"));
        assert!(seen.reading.since >= before, "since the line appeared");

        append(&shell, "test a ... ok\n");
        let after = sampled_until(&d, s.id, |s| s.waits.is_none()).await;
        assert_ne!(after.reading.kind, Kind::Waits(Wait::Queue));
        kill_work(&after);
    })
    .await;
}

#[tokio::test]
async fn a_root_ticking_its_screen_is_neither_work_nor_output() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let ticking =
            "i=0; while :; do i=$((i+1)); [ $((i % 5000)) -eq 0 ] && echo \"Running… ($i)\"; done";
        let cli = cli_running(dir.path(), "starting", ticking);
        let opts = options_with(dir.path(), cli, sight_in(dir.path()));
        let d = desk_with(pool, opts, redacting()).await;
        let s = new_session(&d, dir.path()).await;
        working_on_a_bash_call(&d, dir.path(), s.id).await;
        let first = sampled_until(&d, s.id, has_shell).await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let second = sampled_until(&d, s.id, |s| s.cpu_growing.contains(&s.root)).await;
        assert_ne!(first.output, second.output, "its screen ticked");
        assert_eq!(
            second.active_at, first.active_at,
            "only its root used the CPU"
        );
        assert_eq!(
            second.output_at, first.output_at,
            "a screen is no task output"
        );
        kill_work(&second);
        // SAFETY: kill only signals the test's own child process.
        unsafe { libc::kill(i32::try_from(second.root).unwrap(), libc::SIGKILL) };
    })
    .await;
}

#[tokio::test]
async fn an_idle_sessions_processes_are_read_without_a_pass() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = working_cli(dir.path(), "building");
        let opts = options_with(dir.path(), cli, SightOptions::default());
        let d = desk_with(pool, opts, Floor::default()).await;
        let working = new_session(&d, dir.path()).await;
        post(
            &d,
            working.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "build it"}),
        )
        .await;
        let idle = new_session(&d, dir.path()).await;
        post(
            &d,
            idle.id,
            json!({"hook_event_name": "SessionStart", "session_id": idle.id.to_string()}),
        )
        .await;
        assert_eq!(d.view(idle.id).unwrap().state, SessionState::Idle);
        let seen = sampled_until(&d, working.id, has_shell).await;
        let reads = d.sight_reads();
        for _ in 0..3 {
            assert_eq!(d.process_tree(idle.id).await, Processes::NotRunning);
            assert!(matches!(
                d.process_tree(working.id).await,
                Processes::Tree(_)
            ));
        }
        assert_eq!(
            d.sight_reads(),
            reads,
            "nothing is listed again while fresh"
        );
        kill_work(&seen);
    })
    .await;
}

#[tokio::test]
async fn a_session_kept_out_of_desk_is_read_but_shows_none_of_its_lines() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = working_cli(dir.path(), "key SECRET-abc in its own words");
        let mut opts = options_with(dir.path(), cli, sight_in(dir.path()));
        opts.memory.exclude = vec![dir.path().to_string_lossy().into_owned()];
        let d = desk_with(pool, opts, redacting()).await;
        let s = new_session(&d, dir.path()).await;
        let tasks = working_on_a_bash_call(&d, dir.path(), s.id).await;
        append(&tasks.join("b1.output"), "printed\n");
        let seen = sampled_until(&d, s.id, |s| has_shell(s) && s.open_call.is_some()).await;
        assert_eq!(seen.reading.kind, Kind::Works);
        assert_eq!(seen.reading.words, "running sleep");
        assert!(seen.output.is_empty(), "{:?}", seen.output);
        let call = seen.open_call.as_ref().unwrap();
        assert_eq!((call.tool.as_str(), call.input.as_str()), ("Bash", ""));
        let g = d.glance(&d.view(s.id).unwrap());
        let shown = g.render(chrono_tz::UTC, chrono::Utc::now());
        assert!(g.output.is_none(), "{shown}");
        assert!(
            !shown.contains("printed") && !shown.contains("SECRET"),
            "{shown}"
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
        let d = desk_on(pool, home, fake_cli(home), Floor::default(), sight).await;
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
async fn a_sessions_own_log_lines_are_read_from_desks_logs_where_the_person_lets_desk_and_none_without_them(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let store = DeskStore::from_pool(pool.clone());
        DeskStore::migrate(&pool).await.unwrap();
        let r = kept(dir.path(), "web", dir.path());
        let off = kept(dir.path(), "off", &dir.path().join("off"));
        store.put(&r).await.unwrap();
        store.put(&off).await.unwrap();
        store.set_session_memory(off.id, false).await.unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        std::fs::write(
            logs.join("hub.log"),
            format!(
                "{now}  WARN hub: session {} couldn't read SECRET-abc\n{now}  INFO hub: another\n\
                 {now}  INFO hub: session {} read\n",
                r.id, off.id
            ),
        )
        .unwrap();
        let sight = SightOptions {
            logs: Some(logs),
            ..SightOptions::default()
        };
        let d = desk_on(pool, dir.path(), fake_cli(dir.path()), redacting(), sight).await;
        let read = d.session_logs(r.id, None).await.unwrap();
        assert_eq!(read.lines.len(), 1);
        assert!(read.lines[0].text.contains("[secret]"));
        assert_eq!(
            d.session_logs(off.id, None).await.unwrap_err(),
            super::sight::KEPT_OUT
        );
    })
    .await;
}

#[tokio::test]
async fn a_hub_with_no_desk_logs_says_so() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = fake_cli(dir.path());
        let d = desk_on(pool, dir.path(), cli, redacting(), SightOptions::default()).await;
        let s = new_session(&d, dir.path()).await;
        assert_eq!(
            d.session_logs(s.id, None).await.unwrap_err(),
            super::sight::NO_LOGS
        );
    })
    .await;
}
