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
            model: None,
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

/// A sub-agent's call, `agent` running `command` as `call`.
fn sub_bash(agent: &str, call: &str, command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "agent_id": agent, "tool_name": "Bash",
        "tool_use_id": call, "tool_input": {"command": command}})
}

fn sub_ended(agent: &str, call: &str) -> Value {
    json!({"hook_event_name": "PostToolUse", "agent_id": agent, "tool_name": "Bash",
        "tool_use_id": call})
}

fn sub_asks(agent: &str, command: &str) -> Value {
    json!({"hook_event_name": "PermissionRequest", "agent_id": agent, "tool_name": "Bash",
        "tool_input": {"command": command}})
}

/// Holds `body`'s hook open, as Claude Code waits on it: its answer.
fn held(d: &Desk, s: Uuid, body: Value) -> tokio::task::JoinHandle<Value> {
    let d = d.clone();
    tokio::spawn(async move { post(&d, s, body).await })
}

/// Waits for `n` asks Desk keeps for `s`, shown or behind.
async fn keeps(d: &Desk, s: Uuid, n: usize) {
    for _ in 0..200 {
        if d.ask_keys().get(&s).map_or(0, |k| k.len()) == n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("Desk keeps {:?}, not {n}", d.ask_keys().get(&s));
}

/// What Cufflink Migration met: background sub-agents and the main thread
/// asking at once, with other calls of theirs running and ending meanwhile.
/// Every one is held in Desk, comes to the front answerable, and its answer
/// reaches its own hook: none is left to the terminal.
#[tokio::test]
async fn asks_raised_at_once_by_the_main_thread_and_sub_agents_each_come_up_answerable() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = super::tests::desk(pool, dir.path()).await;
        let s = super::tests::new_session(&d, dir.path()).await;
        prompted(&d, s.id, "fix them all").await;
        for a in ["a1", "a2", "a3"] {
            post(
                &d,
                s.id,
                json!({"hook_event_name": "SubagentStart", "agent_id": a}),
            )
            .await;
        }
        // The main turn ends; its sub-agents work on in the background.
        post(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "three agents launched"}),
        )
        .await;
        // A message on its line starts the next turn.
        prompted(&d, s.id, "carry on").await;

        let mut waiting = Vec::new();
        post(
            &d,
            s.id,
            sub_bash("a2", "toolu_a2", "git show main:ci.yml > ci.yml"),
        )
        .await;
        waiting.push(held(
            &d,
            s.id,
            sub_asks("a2", "git show main:ci.yml > ci.yml"),
        ));
        keeps(&d, s.id, 1).await;
        // a1 runs a call that needs no asking, beside it.
        post(&d, s.id, sub_bash("a1", "toolu_a1_0", "sed -n 1,5p a.rs")).await;
        post(&d, s.id, sub_ended("a1", "toolu_a1_0")).await;
        post(
            &d,
            s.id,
            start("toolu_m", "Bash", json!({"command": "gh run view 1"})),
        )
        .await;
        waiting.push(held(
            &d,
            s.id,
            asks_for("Bash", json!({"command": "gh run view 1"})),
        ));
        keeps(&d, s.id, 2).await;
        post(
            &d,
            s.id,
            sub_bash("a3", "toolu_a3", "python3 - <<EOF\nprint(1)\nEOF"),
        )
        .await;
        waiting.push(held(
            &d,
            s.id,
            sub_asks("a3", "python3 - <<EOF\nprint(1)\nEOF"),
        ));
        keeps(&d, s.id, 3).await;
        post(
            &d,
            s.id,
            sub_bash("a1", "toolu_a1_1", "cargo test -p leads"),
        )
        .await;
        waiting.push(held(&d, s.id, sub_asks("a1", "cargo test -p leads")));
        keeps(&d, s.id, 4).await;

        // Each shows in turn, answerable, and is allowed from Desk.
        for n in 0..4 {
            let p = d
                .view(s.id)
                .unwrap()
                .pending
                .unwrap_or_else(|| panic!("ask {n} shows"));
            assert!(p.answerable, "ask {n} ({}) is Desk's to answer", p.tool);
            d.decide(s.id, p.id, crate::model::Decision::Allow, None)
                .unwrap_or_else(|e| panic!("ask {n} ({}) is answered: {e}", p.tool));
            // Its call runs and ends, as Claude Code goes on.
            let (agent, call) = match p.agent.as_deref() {
                Some(a) => (Some(a.to_string()), p.call.clone()),
                None => (None, p.call.clone()),
            };
            if let Some(call) = call {
                match agent {
                    Some(a) => post(&d, s.id, sub_ended(&a, &call)).await,
                    None => post(&d, s.id, ended(&call, "Bash")).await,
                };
            }
        }
        for w in waiting {
            let said = tokio::time::timeout(std::time::Duration::from_secs(5), w)
                .await
                .expect("every hook is answered")
                .unwrap();
            assert_eq!(
                said,
                crate::claude::allow(),
                "allowed, never handed to the terminal"
            );
        }
        assert_eq!(d.view(s.id).unwrap().pending, None);
    })
    .await;
}

/// What stopped Cufflink Migration: a message on its line (or the person's
/// prompt) starting a turn, or the turn ending, while a background
/// sub-agent's request is held. Dropped, its hook was answered with nothing,
/// so Claude Code showed its own dialog, and queued every request after it
/// there. The request still waits, held, and is answered from Desk.
#[tokio::test]
async fn a_main_turn_starting_or_ending_never_drops_a_sub_agents_held_request() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = super::tests::desk(pool, dir.path()).await;
        let s = super::tests::new_session(&d, dir.path()).await;
        prompted(&d, s.id, "fix them all").await;
        for a in ["a1", "a2"] {
            post(
                &d,
                s.id,
                json!({"hook_event_name": "SubagentStart", "agent_id": a}),
            )
            .await;
        }
        post(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "launched"}),
        )
        .await;
        post(&d, s.id, sub_bash("a1", "toolu_a1", "cargo test")).await;
        let first = held(&d, s.id, sub_asks("a1", "cargo test"));
        keeps(&d, s.id, 1).await;
        post(&d, s.id, sub_bash("a2", "toolu_a2", "cargo build")).await;
        let second = held(&d, s.id, sub_asks("a2", "cargo build"));
        keeps(&d, s.id, 2).await;
        // The next turn starts, and ends, while they wait.
        prompted(&d, s.id, "carry on").await;
        post(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "waiting on them"}),
        )
        .await;
        for n in 0..2 {
            let p = d
                .view(s.id)
                .unwrap()
                .pending
                .unwrap_or_else(|| panic!("ask {n} still shows"));
            assert!(p.answerable, "ask {n} is still Desk's");
            d.decide(s.id, p.id, crate::model::Decision::Allow, None)
                .unwrap_or_else(|e| panic!("ask {n} is answered: {e}"));
            let call = p.call.clone().unwrap();
            post(&d, s.id, sub_ended(p.agent.as_deref().unwrap(), &call)).await;
        }
        for w in [first, second] {
            let said = tokio::time::timeout(std::time::Duration::from_secs(5), w)
                .await
                .expect("every hook is answered")
                .unwrap();
            assert_eq!(said, crate::claude::allow());
        }
    })
    .await;
}

/// A prompt only its terminal's dialog shows names no call ("Claude needs
/// your permission"): what it asks is the one call running, said whole,
/// so its boss judges the command, not a notice. With several running,
/// nothing is made up.
#[tokio::test]
async fn a_prompt_only_the_terminal_shows_carries_the_call_it_is_about() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "ship the migration").await;
        let command = "cd services/leads && git push origin cufflink-7";
        post(&d, s.id, sub_bash("a1", "toolu_9", command)).await;
        let notice = json!({"hook_event_name": "Notification", "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash"});
        post(&d, s.id, notice.clone()).await;
        let p = d.view(s.id).unwrap().pending.expect("its dialog waits");
        assert!(!p.answerable);
        assert_eq!(p.tool, "Input", "still a dialog's wait, ended as one");
        assert!(p.detail.starts_with("Claude needs your permission to use Bash"), "{}", p.detail);
        assert!(p.detail.contains("git push origin cufflink-7"), "{}", p.detail);
        let r = p.request.expect("what it asks, whole");
        assert_eq!(r.tool, "Bash");
        assert_eq!(r.input["command"], command);

        // Esc there (the boss clearing it): its wait is over in Desk too.
        d.interrupt(s.id, None).unwrap();
        let v = d.view(s.id).unwrap();
        assert!(v.pending.is_none(), "{:?}", v.pending);

        // Two calls running: which it is about can't be told.
        post(&d, s.id, sub_bash("a2", "toolu_10", "git status")).await;
        post(&d, s.id, notice).await;
        let p = d.view(s.id).unwrap().pending.expect("its dialog waits");
        assert_eq!(p.detail, "Claude needs your permission to use Bash");
        assert!(p.request.is_none());
    })
    .await;
}

/// A prompt only its terminal's dialog shows is cleared (Esc) only by the
/// boss its requests are held for, or by the person: another boss, even
/// one at Act fully, presses nothing there.
#[tokio::test]
async fn only_its_own_boss_clears_a_prompt_its_terminal_shows() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "ship the migration").await;
        // Both bosses at Act fully: which one the session is held for decides.
        let unbound = crate::charge::FullGrant {
            anywhere: true,
            unbound: true,
            ..Default::default()
        };
        d.set_full(std::collections::HashMap::from([
            ("crew".to_string(), unbound.clone()),
            ("rival".to_string(), unbound),
        ]));
        d.set_dialog_answerers(std::collections::HashMap::from([(
            s.id,
            super::DialogAnswerer {
                boss: "crew".into(),
                gap: chrono::Duration::zero(),
            },
        )]));
        let notice = json!({"hook_event_name": "Notification", "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash"});
        post(&d, s.id, notice.clone()).await;
        let p = d.view(s.id).unwrap().pending.expect("its dialog waits");
        assert!(!p.answerable);
        let boss = |id: &str| crate::crew::By::manager(id, id, "clearing it");

        // Another boss: refused, and the dialog still waits.
        d.clear_dialog(s.id, p.id, boss("rival"))
            .expect_err("another boss clears nothing");
        d.interrupt(s.id, Some(boss("rival")))
            .expect_err("told it isn't theirs");
        let v = d.view(s.id).unwrap();
        assert_eq!(v.pending.map(|w| w.id), Some(p.id), "nothing was pressed");
        assert_eq!(v.state, SessionState::Blocked);

        // Nor an agent's, as its own: a boss's alone.
        let agent = crate::crew::By {
            party: crate::crew::Party::agent("crew", "crew"),
            reason: None,
            suggestion_id: None,
            hop: 0,
            preference: None,
        };
        d.interrupt(s.id, Some(agent.clone()))
            .expect_err("an agent clears nothing");
        d.interrupt(s.id, Some(crate::crew::By::desk("for the test")))
            .expect_err("nor Desk");
        assert_eq!(d.view(s.id).unwrap().pending.map(|w| w.id), Some(p.id));
        d.clear_dialog(s.id, p.id, agent)
            .expect_err("only a boss clears a prompt");

        // Its own boss clears it.
        d.clear_dialog(s.id, p.id, boss("crew")).unwrap();
        assert!(d.view(s.id).unwrap().pending.is_none());

        // Its own boss's Esc too, and the person's.
        post(&d, s.id, notice.clone()).await;
        assert!(d.view(s.id).unwrap().pending.is_some());
        d.interrupt(s.id, Some(boss("crew"))).unwrap();
        assert!(d.view(s.id).unwrap().pending.is_none());
        post(&d, s.id, notice).await;
        assert!(d.view(s.id).unwrap().pending.is_some());
        d.interrupt(s.id, Some(crate::crew::By::you())).unwrap();
        assert!(d.view(s.id).unwrap().pending.is_none());
    })
    .await;
}
