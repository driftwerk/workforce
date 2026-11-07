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
            engine: Engine::Claude,
            view: None,
            name: "orphans".into(),
            cwd: dir.to_string_lossy().into_owned(),
            goal: "tidy the asks".into(),
            worktree: None,
            prompt: None,
            approvals: Some(Approvals::Terminal),
            approve_for_me: false,
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

#[tokio::test]
async fn a_newer_call_on_the_same_thread_ends_what_it_asked_before() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = asking_session(&d, dir.path()).await;
        prompted(&d, s.id, "deploy it").await;
        // A sub-agent's ask: the main thread's calls don't end it.
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
        assert_eq!(
            d.view(s.id).unwrap().pending.unwrap().agent.as_deref(),
            Some("a1")
        );
        assert!(lists(&logged, 1).await);
        // Turned down in the terminal, which says nothing: the sub-agent
        // goes on to its next call.
        post(
            &d,
            s.id,
            json!({"hook_event_name": "PreToolUse", "agent_id": "a1", "tool_name": "Bash",
            "tool_use_id": "toolu_s2", "tool_input": {"command": "ls"}}),
        )
        .await;
        assert_eq!(d.view(s.id).unwrap().pending, None);
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));

        // The main thread's ask, the same way.
        asked(&d, s.id, "toolu_1", "make deploy").await;
        assert!(lists(&logged, 1).await);
        post(&d, s.id, bash("toolu_2", "make status")).await;
        let v = d.view(s.id).unwrap();
        assert_eq!((v.state, v.pending), (SessionState::Working, None));
        assert!(lists(&logged, 0).await, "{:?}", needs(&d, &logged));
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
