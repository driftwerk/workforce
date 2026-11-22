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
use super::tests::{new_session, options, post};
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
        kill_work(&seen);
    })
    .await;
}
