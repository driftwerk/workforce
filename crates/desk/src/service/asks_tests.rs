//! What a Claude Code session asks the person, on the floor: each ask is
//! the call that raised it, ends with what names that call, and never
//! outlives it, a hub restart included.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use workforce_postgres::test_support::with_test_pool;

use chrono::Utc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::tests::{fake_cli, logged_desk, logs, options, post, Logged};
use super::{Desk, Floor};
use crate::floor::{self, Answer, Outcome, Place, Subject, Turn, TurnOp, Wait};
use crate::model::{
    Approvals, Engine, NewSession, Origin, SessionRecord, SessionState, SessionView,
};
use crate::store::{build_pool, tests::record_fixture, DeskStore};

/// A session whose terminal answers what it asks.
async fn asking_session(d: &Desk, dir: &Path) -> SessionView {
    d.create(
        NewSession {
            fresh_worktree: false,
            engine: Engine::Claude,
            view: None,
            name: "orphans".into(),
            cwd: dir.to_string_lossy().into_owned(),
            goal: "tidy the asks".into(),
            worktree: None,
            prompt: None,
            approvals: Some(Approvals::Terminal),
            approve_for_me: false,
            fast: false,
            account: None,
            env: Default::default(),
            group: None,
        },
        None,
    )
    .await
    .unwrap()
}

/// The floor's log as it reads now.
fn turns(logged: &Logged) -> Vec<Turn> {
    logged.0.lock().iter().fold(Vec::new(), floor::apply)
}

/// What the floor lists as needing the person: as the hub reads it, only
/// what a session still asks.
fn needs(d: &Desk, logged: &Logged) -> Vec<floor::Need> {
    floor::open_needs(&floor::backed(turns(logged), &d.ask_keys()))
}

fn bash(id: &str, command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": id,
        "tool_input": {"command": command}})
}

fn permission(command: &str) -> Value {
    json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
        "tool_input": {"command": command}})
}

/// Waits until the floor's log alone, unfiltered, lists `n` needs: what
/// it keeps, not only what the hub shows of it.
async fn lists(logged: &Arc<Logged>, n: usize) -> bool {
    logs(logged, |ops| {
        floor::open_needs(&ops.iter().fold(Vec::new(), floor::apply)).len() == n
    })
    .await
}

/// The key of the one wait the floor lists.
fn listed(d: &Desk, logged: &Logged) -> Vec<String> {
    needs(d, logged)
        .into_iter()
        .map(|n| n.id.rsplit(':').next().unwrap_or_default().to_string())
        .collect()
}

/// A session at work on a prompt, asking to run `command` as `call`.
async fn asked(d: &Desk, s: Uuid, call: &str, command: &str) {
    post(d, s, bash(call, command)).await;
    post(d, s, permission(command)).await;
}

#[tokio::test]
async fn a_turn_ended_on_an_api_error_ends_and_takes_its_asks_with_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "deploy it"}),
        )
        .await;
        post(&d, s.id, bash("toolu_1", "make deploy")).await;
        post(&d, s.id, permission("make deploy")).await;
        assert!(lists(&logged, 1).await, "{:?}", needs(&d, &logged));

        post(
            &d,
            s.id,
            json!({"hook_event_name": "StopFailure", "error": "rate_limit",
            "last_assistant_message": "API Error: Rate limit reached"}),
        )
        .await;
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Done, None));
        assert!(
            logs(&logged, |ops| ops.iter().any(|op| matches!(
                op,
                TurnOp::End {
                    outcome: Outcome::Limit | Outcome::Failed,
                    ..
                }
            )))
            .await,
            "its turn ends, on the error"
        );
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

async fn prompted(d: &Desk, s: Uuid, prompt: &str) {
    post(
        d,
        s,
        json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt}),
    )
    .await;
}

#[tokio::test]
async fn an_ask_is_its_call_and_ends_with_that_calls_end() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "deploy it").await;
        asked(&d, s.id, "toolu_1", "make deploy").await;
        let p = d.view(s.id).unwrap().pending.unwrap();
        assert_eq!(p.call.as_deref(), Some("toolu_1"), "its call, as its thread last started it");
        let key = p.id.to_string();
        assert!(lists(&logged, 1).await);
        assert_eq!(listed(&d, &logged), std::slice::from_ref(&key));

        // Another call run beside it ends: it still waits.
        post(&d, s.id, json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "tool_use_id": "toolu_0"})).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
        assert_eq!(listed(&d, &logged), std::slice::from_ref(&key));

        post(&d, s.id, json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "toolu_1"})).await;
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Working, None));
        assert!(
            logs(&logged, |ops| ops.iter().any(
                |op| matches!(op, TurnOp::Unwait { key: k, .. } if *k == key)
            ))
            .await
        );
        assert!(lists(&logged, 0).await);
        assert!(listed(&d, &logged).is_empty());
    })
    .await;
}

fn start(id: &str, tool: &str, input: Value) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": tool, "tool_use_id": id, "tool_input": input})
}

fn ended(id: &str, tool: &str) -> Value {
    json!({"hook_event_name": "PostToolUse", "tool_name": tool, "tool_use_id": id})
}

fn asks_for(tool: &str, input: Value) -> Value {
    json!({"hook_event_name": "PermissionRequest", "tool_name": tool, "tool_input": input})
}

#[tokio::test]
async fn calls_run_beside_an_ask_never_end_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "read up on it").await;
        // Run side by side: a fetch that needs permission, a read that
        // doesn't, started after it.
        let fetch = json!({"url": "https://example.com/a"});
        post(&d, s.id, start("toolu_A", "WebFetch", fetch.clone())).await;
        post(
            &d,
            s.id,
            start("toolu_B", "Read", json!({"file_path": "/etc/hosts"})),
        )
        .await;
        post(&d, s.id, asks_for("WebFetch", fetch)).await;
        let p = d.view(s.id).unwrap().pending.unwrap();
        assert_eq!(
            p.call.as_deref(),
            Some("toolu_A"),
            "its own call, not the last started"
        );
        assert!(lists(&logged, 1).await);
        // Another starts beside it, and the read ends: its dialog is up yet.
        post(
            &d,
            s.id,
            start(
                "toolu_C",
                "WebFetch",
                json!({"url": "https://example.com/c"}),
            ),
        )
        .await;
        post(&d, s.id, ended("toolu_B", "Read")).await;
        post(&d, s.id, ended("toolu_C", "WebFetch")).await;
        let v = d.view(s.id).unwrap();
        assert_eq!(v.state, SessionState::Blocked);
        assert_eq!(v.pending.map(|p| p.id), Some(p.id));
        assert_eq!(listed(&d, &logged), std::slice::from_ref(&p.id.to_string()));
        // Its own end ends it.
        post(&d, s.id, ended("toolu_A", "WebFetch")).await;
        assert_eq!(d.view(s.id).unwrap().pending, None);
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

#[tokio::test]
async fn an_ask_whose_call_cant_be_told_waits_while_its_thread_runs_any() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "look twice").await;
        // Two alike side by side: which one asks isn't known.
        let ls = json!({"command": "ls /srv"});
        post(&d, s.id, start("toolu_A", "Bash", ls.clone())).await;
        post(&d, s.id, start("toolu_B", "Bash", ls.clone())).await;
        post(&d, s.id, asks_for("Bash", ls)).await;
        assert_eq!(d.view(s.id).unwrap().pending.unwrap().call, None);
        assert!(lists(&logged, 1).await);
        post(&d, s.id, ended("toolu_B", "Bash")).await;
        assert_eq!(
            d.view(s.id).unwrap().state,
            SessionState::Blocked,
            "the other may be it"
        );
        post(&d, s.id, ended("toolu_A", "Bash")).await;
        assert_eq!(d.view(s.id).unwrap().pending, None);
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

#[tokio::test]
async fn a_second_ask_waits_behind_the_first_and_shows_when_it_ends() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "fetch both").await;
        let (a, b) = (
            json!({"url": "https://example.com/a"}),
            json!({"url": "https://example.com/b"}),
        );
        post(&d, s.id, start("toolu_A", "WebFetch", a.clone())).await;
        post(&d, s.id, start("toolu_B", "WebFetch", b.clone())).await;
        post(&d, s.id, asks_for("WebFetch", a)).await;
        post(&d, s.id, asks_for("WebFetch", b)).await;
        let first = d.view(s.id).unwrap().pending.unwrap();
        assert_eq!(
            first.call.as_deref(),
            Some("toolu_A"),
            "the first still shows"
        );
        assert!(lists(&logged, 1).await);
        assert_eq!(d.ask_keys()[&s.id].len(), 2, "and the second is kept");
        post(&d, s.id, ended("toolu_A", "WebFetch")).await;
        let second = d.view(s.id).unwrap().pending.unwrap();
        assert_eq!(second.call.as_deref(), Some("toolu_B"));
        assert!(
            logs(&logged, |ops| {
                let needs = floor::open_needs(&ops.iter().fold(Vec::new(), floor::apply));
                needs.len() == 1 && needs[0].id.ends_with(&second.id.to_string())
            })
            .await,
            "{:?}",
            needs(&d, &logged)
        );
        post(&d, s.id, ended("toolu_B", "WebFetch")).await;
        assert_eq!(d.view(s.id).unwrap().pending, None);
        assert!(lists(&logged, 0).await);
    })
    .await;
}

#[tokio::test]
async fn a_sub_agents_ask_ends_when_it_does() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "clean up").await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "SubagentStart", "agent_id": "a1"}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_use_id": "toolu_s1", "tool_input": {"command": "rm -rf build"}}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PermissionRequest", "agent_id": "a1", "tool_name": "Bash",
            "tool_input": {"command": "rm -rf build"}}),
        )
        .await;
        assert!(lists(&logged, 1).await);
        // Another sub-agent's end leaves it be.
        post(
            &d,
            s.id,
            json!({"hook_event_name": "SubagentStop", "agent_id": "a2"}),
        )
        .await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
        post(
            &d,
            s.id,
            json!({"hook_event_name": "SubagentStop", "agent_id": "a1"}),
        )
        .await;
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Working, None));
        assert!(
            !d.0.state.lock().live[&s.id]
                .flights
                .contains_key(&Some("a1".to_string())),
            "nor does it run anything"
        );
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

#[tokio::test]
async fn an_ask_left_by_a_turn_that_waits_on_its_background_work_is_closed() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "build it").await;
        asked(&d, s.id, "toolu_1", "make all").await;
        assert!(lists(&logged, 1).await);
        // Its turn stops with a shell still running: the turn stays open on
        // the floor, its ask doesn't.
        post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Building in the background.",
            "background_tasks": [{"id": "b1", "type": "shell", "status": "running"}]})).await;
        assert_eq!(d.view(s.id).unwrap().pending, None);
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

/// A turn of `r`'s open since `at`, waiting on `key` since then.
fn waiting(r: &SessionRecord, key: &str, at: chrono::DateTime<Utc>) -> Turn {
    Turn {
        id: Uuid::new_v4(),
        owner: None,
        subject: Subject::Session {
            id: r.id,
            engine: r.engine,
        },
        name: r.name.clone(),
        place: Place::Session,
        task: Some("check the deploy".into()),
        doing: None,
        started_at: at,
        heard_at: at,
        ended_at: None,
        outcome: None,
        answer: None,
        question: None,
        settled_at: None,
        waits: vec![Wait {
            key: key.into(),
            from: at,
            to: None,
            ask: "Run make deploy?".into(),
            answer: Answer::Decide {
                at: floor::DecideRef::Session {
                    session_id: r.id,
                    approval_id: Uuid::new_v4(),
                },
            },
            options: None,
        }],
        idle: vec![],
        token: None,
        instance: None,
    }
}

#[tokio::test]
async fn the_upgrade_closes_waits_left_open_before_desk_kept_its_asks() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // At work across the restart, in another terminal that still runs.
        let alive = SessionRecord {
            origin: Origin::External,
            external_pid: Some(std::process::id() as i32),
            ..record_fixture("deploy")
        };
        let at = Utc::now() - chrono::Duration::hours(3);
        let orphan = waiting(&alive, &Uuid::new_v4().to_string(), at);
        let logged = Arc::new(Logged::default());
        logged.0.lock().push(TurnOp::Begin {
            turn: Box::new(orphan.clone()),
            by_person: true,
        });
        assert_eq!(floor::open_needs(&turns(&logged)).len(), 1, "the orphan, before");
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        store.put(&alive).await.unwrap();

        let d = Desk::start(
            store,
            options(dir.path()),
            Floor {
                turns: Some(logged.clone()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(d.open_turns(), std::collections::HashSet::from([orphan.id]));
        assert!(
            logs(&logged, |ops| ops.iter().any(|op| matches!(
                op,
                TurnOp::Unwait { turn, key, .. } if *turn == orphan.id && *key == orphan.waits[0].key
            )))
            .await,
            "nothing asks it: it is closed"
        );
        assert!(floor::open_needs(&turns(&logged)).is_empty());
        assert!(needs(&d, &logged).is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_background_sub_agents_ask_ends_with_its_own_call() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "open the PR").await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "SubagentStart", "agent_id": "a1"}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_use_id": "toolu_s1", "tool_input": {"command": "git status"}}),
        )
        .await;
        // The turn is done, its sub-agent works on in the background.
        post(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "The PR is on its way.",
            "background_tasks": [{"id": "a1", "type": "agent", "status": "running"}]}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_use_id": "toolu_s9", "tool_input": {"command": "git push"}}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PermissionRequest", "agent_id": "a1", "tool_name": "Bash",
            "tool_input": {"command": "git push"}}),
        )
        .await;
        assert_eq!(
            d.view(s.id).unwrap().pending.unwrap().call.as_deref(),
            Some("toolu_s9"),
            "the call it started last, even while the turn was done"
        );
        assert!(lists(&logged, 1).await, "{:?}", needs(&d, &logged));
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PostToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_use_id": "toolu_s9"}),
        )
        .await;
        let v = d.view(s.id).unwrap();
        assert_ne!(v.state, SessionState::Blocked);
        assert_eq!(v.pending, None);
        assert!(needs(&d, &logged).is_empty(), "{:?}", needs(&d, &logged));
    })
    .await;
}

#[tokio::test]
async fn an_ask_turned_down_in_the_terminal_ends_when_the_turn_goes_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, "").unwrap();
        post(&d, s.id, json!({"hook_event_name": "SessionStart", "session_id": "cc-1",
            "transcript_path": transcript.to_string_lossy()})).await;
        prompted(&d, s.id, "clean up").await;
        asked(&d, s.id, "toolu_A", "rm -rf build").await;
        assert!(lists(&logged, 1).await);
        // A call started beside it, before its dialog is answered: it waits.
        post(&d, s.id, start("toolu_B", "Read", json!({"file_path": "/etc/hosts"}))).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
        // "No, and tell Claude what to do differently": no hook says so,
        // the transcript has its result, and the turn goes on.
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A","is_error":true,"content":"The user doesn't want to proceed with this tool use."}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        post(&d, s.id, start("toolu_C", "Bash", json!({"command": "ls build"}))).await;
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Working, None));
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

#[tokio::test]
async fn a_running_sub_agents_ask_outlives_the_main_turn() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "ship it").await;
        asked(&d, s.id, "toolu_A", "make deploy").await;
        // A sub-agent in the background asks too, behind it.
        post(&d, s.id, json!({"hook_event_name": "SubagentStart", "agent_id": "a1"})).await;
        post(&d, s.id, json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_use_id": "toolu_s1", "tool_input": {"command": "git push"}})).await;
        post(&d, s.id, json!({"hook_event_name": "PermissionRequest", "agent_id": "a1", "tool_name": "Bash",
            "tool_input": {"command": "git push"}})).await;
        assert_eq!(d.view(s.id).unwrap().pending.unwrap().call.as_deref(), Some("toolu_A"));
        // The main turn ends, the sub-agent runs on: its ask shows.
        post(&d, s.id, json!({"hook_event_name": "Stop", "last_assistant_message": "Deploying in the background.",
            "background_tasks": [{"id": "a1", "type": "agent", "status": "running"}]})).await;
        let v = d.view(s.id).unwrap();
        assert_eq!(v.state, SessionState::Blocked);
        let p = v.pending.unwrap();
        assert_eq!((p.call.as_deref(), p.agent.as_deref()), (Some("toolu_s1"), Some("a1")));
        assert!(
            logs(&logged, |ops| {
                let needs = floor::open_needs(&ops.iter().fold(Vec::new(), floor::apply));
                needs.len() == 1 && needs[0].id.ends_with(&p.id.to_string())
            })
            .await,
            "{:?}",
            needs(&d, &logged)
        );
        // A sub-agent that ended doesn't keep its ask past the turn.
        post(&d, s.id, json!({"hook_event_name": "SubagentStop", "agent_id": "a1"})).await;
        assert_eq!(d.view(s.id).unwrap().pending, None);
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
    })
    .await;
}

#[tokio::test]
async fn another_threads_call_never_ends_a_sub_agents_ask_whose_call_isnt_known() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "tidy").await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "SubagentStart", "agent_id": "a1"}),
        )
        .await;
        // Its call came without an id: which it asks about isn't known, and
        // it has nothing known running.
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_input": {"command": "rm -rf tmp"}}),
        )
        .await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PermissionRequest", "agent_id": "a1", "tool_name": "Bash",
            "tool_input": {"command": "rm -rf tmp"}}),
        )
        .await;
        assert_eq!(d.view(s.id).unwrap().pending.unwrap().call, None);
        assert!(lists(&logged, 1).await);
        // The main thread's call runs and ends.
        post(
            &d,
            s.id,
            start("toolu_M", "Read", json!({"file_path": "/etc/hosts"})),
        )
        .await;
        post(&d, s.id, ended("toolu_M", "Read")).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Blocked);
        assert_eq!(needs(&d, &logged).len(), 1);
    })
    .await;
}

#[test]
fn claudes_and_devins_asks_outlive_the_hub_and_codexs_do_not() {
    use super::asks::asks_outlive_the_hub;
    assert!(asks_outlive_the_hub(Engine::Claude));
    assert!(asks_outlive_the_hub(Engine::Devin));
    assert!(!asks_outlive_the_hub(Engine::Codex));
}
