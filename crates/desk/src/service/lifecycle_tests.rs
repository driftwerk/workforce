//! Tests of a session's life after its work: ending, removing, clearing,
//! and the CLI sessions removed from Desk that adoption leaves alone.

use super::tests::{desk, fake_cli, found, kept_until, logged_desk, logs, new_session, Logged};
use super::*;
use crate::store::{build_pool, tests::record_fixture};
use workforce_postgres::test_support::with_test_pool;

fn listed(found: &[serde_json::Value]) -> Vec<External> {
    adopt::parse(
        serde_json::Value::from(found.to_vec())
            .to_string()
            .as_bytes(),
    )
    .unwrap()
}

/// What Activity says of session `id`.
async fn activity_of(store: &DeskStore, id: Uuid) -> i64 {
    let client = store.pool().get().await.unwrap();
    client
        .query_one(
            "SELECT count(*) FROM activity WHERE subject ->> 'id' = $1 OR actor ->> 'id' = $1",
            &[&id.to_string()],
        )
        .await
        .unwrap()
        .get(0)
}

async fn gone(store: &DeskStore, id: Uuid) {
    for _ in 0..200 {
        if store.get(id).await.unwrap().is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the registry still keeps session {id}");
}

fn forgot(logged: &Logged, id: Uuid) -> bool {
    logged.0.lock().iter().any(|op| {
        matches!(op, TurnOp::Forget { subjects } if subjects.contains(&SubjectKey::session(id)))
    })
}

#[tokio::test]
async fn removing_a_session_forgets_it_everywhere_and_keeps_its_transcript() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let (d, logged) = logged_desk(pool, dir.path(), fake_cli(dir.path())).await;
        let s = new_session(&d, dir.path()).await;
        kept_until(&store, s.id, |_| true).await;
        for _ in 0..200 {
            if activity_of(&store, s.id).await > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            activity_of(&store, s.id).await > 0,
            "starting it was recorded"
        );
        let transcript = dir.path().join("kept.jsonl");
        std::fs::write(&transcript, "{}").unwrap();
        d.change(s.id, |r, _| {
            r.transcript = Some(transcript.to_string_lossy().into_owned())
        });

        let removed = d.remove(s.id, Removal::default()).await.unwrap();
        assert_eq!((removed.removed, removed.transcripts), (1, 0));
        assert!(d.list().is_empty());
        assert!(!d.0.state.lock().live.contains_key(&s.id), "stopped");
        gone(&store, s.id).await;
        assert_eq!(activity_of(&store, s.id).await, 0);
        assert!(forgot(&logged, s.id), "its floor turns go too");
        assert!(
            transcript.is_file(),
            "the engine's transcript is the engine's"
        );
        assert!(matches!(
            d.remove(s.id, Removal::default()).await,
            Err(DeskError::NotFound)
        ));
    })
    .await;
}

/// A stand-in `claude` whose `agents --json` says `listed` (or fails, when
/// it is `None`); anything else stays up until killed.
fn claude_listing(dir: &Path, listed: Option<&[serde_json::Value]>) -> PathBuf {
    let path = dir.join(format!("claude-{}", Uuid::new_v4()));
    let agents = match listed {
        Some(listed) => {
            let json = dir.join(format!("agents-{}.json", Uuid::new_v4()));
            std::fs::write(&json, serde_json::Value::from(listed.to_vec()).to_string()).unwrap();
            format!("cat '{}'", json.display())
        }
        None => "echo 'no agents' >&2; exit 1".into(),
    };
    std::fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = agents ]; then {agents}; exit 0; fi\nexec sleep 60\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    path
}

/// A desk whose home is `home` and whose `claude` is `claude`, holding
/// `records` from before it started.
async fn desk_at_home(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    home: Option<&Path>,
    claude: PathBuf,
    records: &[&SessionRecord],
) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    let store = DeskStore::from_pool(pool);
    for r in records {
        store.put(r).await.unwrap();
    }
    let mut opts = super::tests::options(dir).with_program(Engine::Claude, claude);
    if let Some(home) = home {
        opts.env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
    }
    Desk::start(store, opts, Default::default(), CancellationToken::new())
        .await
        .unwrap()
}

/// A Claude Code transcript of `cli` in `home`, with a sub-agent's beside it.
fn claude_transcript(home: &Path, cli: &str) -> PathBuf {
    let project = home.join(".claude/projects/-Users-k-dev-app");
    std::fs::create_dir_all(project.join(cli).join("subagents")).unwrap();
    std::fs::write(project.join(cli).join("subagents/agent-a.jsonl"), "{}").unwrap();
    std::fs::write(project.join(format!("{cli}.jsonl")), "{}").unwrap();
    project.join(format!("{cli}.jsonl"))
}

fn closed_external(name: &str, cli: &str, transcript: Option<&Path>) -> SessionRecord {
    SessionRecord {
        origin: Origin::External,
        state: SessionState::Closed,
        cli_session_id: Some(cli.into()),
        transcript: transcript.map(|t| t.to_string_lossy().into_owned()),
        external_pid: None,
        terminal_id: None,
        ..record_fixture(name)
    }
}

#[tokio::test]
async fn a_transcript_is_deleted_only_when_asked_and_never_while_it_runs() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let file = claude_transcript(&home, "cc-3");
        let folder = file.with_file_name("cc-3");
        let was = closed_external("app-36", "cc-3", None);
        let claude = claude_listing(dir.path(), Some(&[]));
        let d = desk_at_home(pool, dir.path(), Some(&home), claude, &[&was]).await;
        let store = DeskStore::from_pool(d.0.store.pool().clone());
        let removed = d
            .remove(was.id, Removal { transcript: true })
            .await
            .unwrap();
        assert_eq!((removed.removed, removed.transcripts), (1, 2));
        assert!(!file.exists());
        assert!(!folder.exists(), "its sub-agents' with it");
        // Written in the one transaction, before anything was deleted.
        assert!(store.get(was.id).await.unwrap().is_none());
        assert_eq!(store.hidden().await.unwrap()[0].cli_session_id, "cc-3");

        // While it runs here, its transcript stays and so does it.
        let s = new_session(&d, dir.path()).await;
        assert!(matches!(
            d.remove(s.id, Removal { transcript: true }).await,
            Err(DeskError::Conflict(_))
        ));
        assert_eq!(d.list().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_transcript_running_in_another_terminal_or_unknown_stays() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let file = claude_transcript(&home, "cc-9");
        let was = closed_external("app-40", "cc-9", Some(&file));

        // Resumed in a terminal Desk hasn't looked at since.
        let running = claude_listing(
            dir.path(),
            Some(&[found("cc-9", "/Users/k/dev/app", "app-40")]),
        );
        let d = desk_at_home(pool.clone(), dir.path(), Some(&home), running, &[&was]).await;
        assert!(matches!(
            d.remove(was.id, Removal { transcript: true }).await,
            Err(DeskError::Conflict(_))
        ));
        assert!(file.is_file());
        assert!(d.load(was.id).await.is_ok(), "nor is it removed");

        // `claude agents` can't say: nothing is deleted on a guess.
        let failing = claude_listing(dir.path(), None);
        let d = desk_at_home(pool.clone(), dir.path(), Some(&home), failing, &[]).await;
        assert!(matches!(
            d.remove(was.id, Removal { transcript: true }).await,
            Err(DeskError::Conflict(_))
        ));
        assert!(file.is_file());
        // Removed without it, it goes and the transcript stays.
        d.remove(was.id, Removal::default()).await.unwrap();
        assert!(file.is_file());
    })
    .await;
}

#[tokio::test]
async fn a_transcript_path_a_session_said_is_never_trusted() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let victim = claude_transcript(&home, "victim");
        let outside = dir.path().join("notes.jsonl");
        std::fs::write(&outside, "{}").unwrap();
        // A hook said another session's transcript was its own, or a file
        // outside; another's id climbs to one.
        let pointing = closed_external("app-41", "cc-10", Some(&victim));
        let out = closed_external("app-42", "cc-11", Some(&outside));
        let climbing = closed_external("app-43", "../-Users-k-dev-app/victim", None);
        let claude = claude_listing(dir.path(), Some(&[]));
        let d = desk_at_home(
            pool,
            dir.path(),
            Some(&home),
            claude,
            &[&pointing, &out, &climbing],
        )
        .await;
        for r in [&pointing, &out, &climbing] {
            let removed = d.remove(r.id, Removal { transcript: true }).await.unwrap();
            assert_eq!((removed.removed, removed.transcripts), (1, 0), "{}", r.name);
        }
        assert!(victim.is_file());
        assert!(victim
            .with_file_name("victim/subagents/agent-a.jsonl")
            .is_file());
        assert!(outside.is_file());
    })
    .await;
}

#[tokio::test]
async fn without_a_home_no_transcript_is_deleted() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let file = claude_transcript(&home, "cc-12");
        let was = closed_external("app-44", "cc-12", Some(&file));
        let claude = claude_listing(dir.path(), Some(&[]));
        let d = desk_at_home(pool, dir.path(), None, claude, &[&was]).await;
        let removed = d
            .remove(was.id, Removal { transcript: true })
            .await
            .unwrap();
        assert_eq!((removed.removed, removed.transcripts), (1, 0));
        assert!(file.is_file());
    })
    .await;
}

#[tokio::test]
async fn a_removed_session_changed_elsewhere_is_never_saved_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        let r = d.record(s.id).unwrap();
        d.remove(s.id, Removal::default()).await.unwrap();
        gone(&store, s.id).await;
        // As a resume that failed to launch puts back what it read.
        d.save(r);
        assert!(d.record(s.id).is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(store.get(s.id).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn a_removed_session_is_never_adopted_again_until_tracked_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool.clone(), dir.path()).await;
        let running = listed(&[found("ext-4", "/Users/k/dev/vellev", "vellev-36")]);
        d.reconcile_external(running.clone()).await;
        let s = d.list().pop().unwrap();
        d.remove(s.id, Removal::default()).await.unwrap();
        // Still running in its own terminal: left alone.
        d.reconcile_external(running.clone()).await;
        assert!(d.list().is_empty());
        let hidden = d.hidden_sessions().await;
        // The hide is queued; read it once it has landed.
        let mut hidden = hidden.unwrap();
        for _ in 0..200 {
            if !hidden.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            hidden = d.hidden_sessions().await.unwrap();
        }
        assert_eq!(
            hidden
                .iter()
                .map(|h| (h.cli_session_id.as_str(), h.name.as_str()))
                .collect::<Vec<_>>(),
            [("ext-4", "vellev-36")]
        );

        // A later hub remembers.
        let again = desk(pool.clone(), dir.path()).await;
        again.reconcile_external(running.clone()).await;
        assert!(again.list().is_empty());

        // Tracked again: adopted when next found, as a new record (the old
        // one was removed with everything it said).
        again.unhide(Engine::Claude, "ext-4");
        again.reconcile_external(running).await;
        let back = again.list();
        assert_eq!(back.len(), 1);
        assert_ne!(back[0].id, s.id);
        assert_eq!(back[0].cli_session_id.as_deref(), Some("ext-4"));
    })
    .await;
}

#[tokio::test]
async fn a_hidden_session_the_registry_still_keeps_is_never_taken_back() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let was = SessionRecord {
            origin: Origin::External,
            state: SessionState::Closed,
            cli_session_id: Some("ext-5".into()),
            external_pid: None,
            terminal_id: None,
            ..record_fixture("vellev-37")
        };
        store.put(&was).await.unwrap();
        store
            .hide(&Hidden {
                engine: Engine::Claude,
                cli_session_id: "ext-5".into(),
                name: "vellev-37".into(),
                cwd: "/Users/k/dev/vellev".into(),
                hidden_at: Utc::now(),
            })
            .await
            .unwrap();
        let d = desk(pool, dir.path()).await;
        d.reconcile_external(listed(&[found(
            "ext-5",
            "/Users/k/dev/vellev",
            "vellev-37",
        )]))
        .await;
        assert!(d.list().is_empty());
        assert!(
            !d.0.state.lock().records.contains_key(&was.id),
            "not even recalled"
        );
    })
    .await;
}

#[tokio::test]
async fn a_forgotten_session_is_never_recalled_while_the_registry_catches_up() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let was = SessionRecord {
            origin: Origin::External,
            state: SessionState::Closed,
            cli_session_id: Some("ext-6".into()),
            external_pid: None,
            terminal_id: None,
            ..record_fixture("vellev-38")
        };
        store.put(&was).await.unwrap();
        let d = desk(pool, dir.path()).await;
        d.forget(was.id).await.unwrap();
        gone(&store, was.id).await;
        // As if the queued delete hadn't landed yet.
        store.put(&was).await.unwrap();
        assert_eq!(d.recall(Engine::Claude, "ext-6").await, None);
        assert!(matches!(d.load(was.id).await, Err(DeskError::NotFound)));
        d.reconcile_external(listed(&[found(
            "ext-6",
            "/Users/k/dev/vellev",
            "vellev-38",
        )]))
        .await;
        assert!(d.list().iter().all(|s| s.id != was.id));
    })
    .await;
}

#[tokio::test]
async fn ending_a_session_closes_it_and_it_resumes_from_the_closed_list() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let d = desk(pool.clone(), dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        let ended = d.end(s.id, Some(By::you())).await.unwrap();
        assert_eq!((ended.state, ended.live), (SessionState::Closed, false));
        assert!(d.list().is_empty(), "it leaves the sessions Desk shows");
        kept_until(&store, s.id, |r| r.state == SessionState::Closed).await;

        // A later hub doesn't load it, but lists it, and resumes it.
        let again = desk(pool, dir.path()).await;
        assert!(again.list().is_empty());
        let closed = again.closed_sessions().await.unwrap();
        assert_eq!(closed.iter().map(|c| c.id).collect::<Vec<_>>(), [s.id]);
        let resumed = again.resume(s.id, None, None).await.unwrap();
        assert_eq!(
            (resumed.id, resumed.live, resumed.name.as_str()),
            (s.id, true, "auth-fix")
        );
    })
    .await;
}

#[tokio::test]
async fn a_closed_session_from_another_terminal_resumes_in_desk_as_the_same_session() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let was = SessionRecord {
            origin: Origin::External,
            state: SessionState::Closed,
            cli_session_id: Some("ext-7".into()),
            cwd: dir.path().to_string_lossy().into_owned(),
            display_name: Some("Ledger migration".into()),
            external_pid: None,
            terminal_id: None,
            ..record_fixture("atlas-13")
        };
        store.put(&was).await.unwrap();
        let d = desk(pool, dir.path()).await;
        let s = d.resume(was.id, None, Some(By::you())).await.unwrap();
        assert_eq!((s.id, s.origin, s.live), (was.id, Origin::Desk, true));
        assert_eq!(s.display_name.as_deref(), Some("Ledger migration"));
        assert_eq!(s.cli_session_id.as_deref(), Some("ext-7"));
        let kept = kept_until(&store, was.id, |r| r.origin == Origin::Desk).await;
        assert_eq!(kept.state, SessionState::Starting);
    })
    .await;
}

#[tokio::test]
async fn finished_sessions_are_cleared_by_how_long_ago_they_finished() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let at = |days: i64| Utc::now() - chrono::Duration::days(days);
        let finished = |name: &str, state, days| SessionRecord {
            state,
            cli_session_id: Some(format!("cc-{name}")),
            terminal_id: None,
            updated_at: at(days),
            created_at: at(days + 1),
            ..record_fixture(name)
        };
        let old_stopped = finished("old-stopped", SessionState::Stopped, 10);
        let old_closed = finished("old-closed", SessionState::Closed, 30);
        let recent = finished("recent", SessionState::Stopped, 2);
        for r in [&old_stopped, &old_closed, &recent] {
            store.put(r).await.unwrap();
        }
        let d = desk(pool, dir.path()).await;
        let running = new_session(&d, dir.path()).await;
        let clearing = |dry_run| Clearing {
            older_than_days: 7,
            dry_run,
            transcripts: false,
        };
        assert_eq!(d.clear_finished(clearing(true)).await.unwrap().removed, 2);
        assert_eq!(d.list().len(), 3, "counting removes nothing");
        assert_eq!(d.clear_finished(clearing(false)).await.unwrap().removed, 2);
        let mut left: Vec<Uuid> = d.list().into_iter().map(|s| s.id).collect();
        left.sort();
        let mut want = vec![recent.id, running.id];
        want.sort();
        assert_eq!(left, want);
        gone(&store, old_closed.id).await;
        gone(&store, old_stopped.id).await;
    })
    .await;
}

#[tokio::test]
async fn a_session_finished_again_lately_is_not_cleared_by_when_it_first_finished() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let old = Utc::now() - chrono::Duration::days(20);
        let was = SessionRecord {
            state: SessionState::Stopped,
            cli_session_id: Some("cc-again".into()),
            terminal_id: None,
            updated_at: old,
            created_at: old,
            ..record_fixture("again")
        };
        store.put(&was).await.unwrap();
        let d = desk(pool, dir.path()).await;
        // Resumed and stopped again just now.
        d.change(was.id, |r, _| r.state = SessionState::Idle);
        d.change(was.id, |r, _| r.state = SessionState::Stopped);
        let clearing = |dry_run| Clearing {
            older_than_days: 7,
            dry_run,
            transcripts: false,
        };
        assert_eq!(d.clear_finished(clearing(true)).await.unwrap().removed, 0);
        assert_eq!(d.clear_finished(clearing(false)).await.unwrap().removed, 0);
        assert!(d.record(was.id).is_ok());
        // Nor is one that runs, when its state says it finished.
        let s = new_session(&d, dir.path()).await;
        assert!(matches!(
            d.remove_as(s.id, None, crate::memory_store::Memory::Forget, |st, r| {
                if super::lifecycle::finished_before(st, r, Utc::now()) {
                    Ok(())
                } else {
                    Err(DeskError::Conflict("it started again".into()))
                }
            })
            .await,
            Err(DeskError::Conflict(_))
        ));
        assert_eq!(d.list().iter().filter(|v| v.id == s.id).count(), 1);
    })
    .await;
}

#[tokio::test]
async fn clearing_further_back_than_time_goes_is_refused() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        assert!(matches!(
            d.clear_finished(Clearing {
                older_than_days: u32::MAX,
                dry_run: true,
                transcripts: false,
            })
            .await,
            Err(DeskError::Invalid(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn a_session_ended_as_its_process_exits_stays_closed() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        // Ended (closed) after its exit was on its way but before it was seen.
        d.change(s.id, |r, _| r.state = SessionState::Closed);
        let pty = std::sync::Arc::clone(&d.0.state.lock().live[&s.id].pty);
        pty.kill();
        for _ in 0..250 {
            if !d.0.state.lock().live.contains_key(&s.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(d.record(s.id).unwrap().state, SessionState::Closed);
    })
    .await;
}

#[tokio::test]
async fn a_codex_transcript_goes_whatever_claude_code_can_say() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let day = home.join(".codex/sessions/2026/10/27");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-10-27T17-06-33-th-1.jsonl");
        std::fs::write(&rollout, "{}").unwrap();
        let was = SessionRecord {
            engine: Engine::Codex,
            ..closed_external("codex-1", "th-1", None)
        };
        let failing = claude_listing(dir.path(), None);
        let d = desk_at_home(pool, dir.path(), Some(&home), failing, &[&was]).await;
        let removed = d
            .remove(was.id, Removal { transcript: true })
            .await
            .unwrap();
        assert_eq!((removed.removed, removed.transcripts), (1, 1));
        assert!(!rollout.exists());
    })
    .await;
}

/// A turn of `r`'s that began at `at`, as the floor's log keeps it open.
fn begun(r: &SessionRecord, at: DateTime<Utc>) -> crate::floor::Turn {
    crate::floor::Turn {
        id: Uuid::new_v4(),
        owner: None,
        subject: crate::floor::Subject::Session {
            id: r.id,
            engine: r.engine,
        },
        name: r.name.clone(),
        place: crate::floor::Place::Session,
        task: r.last_prompt.clone(),
        doing: None,
        started_at: at,
        heard_at: at,
        ended_at: None,
        outcome: None,
        answer: None,
        question: None,
        settled_at: None,
        waits: vec![],
        idle: vec![],
        token: None,
        instance: None,
    }
}

fn begin(t: &crate::floor::Turn) -> TurnOp {
    TurnOp::Begin {
        turn: Box::new(t.clone()),
        by_person: true,
    }
}

#[test]
fn a_restarted_desk_carries_on_the_latest_open_turn_of_each_session_at_work() {
    let at = Utc::now() - chrono::Duration::minutes(30);
    let mins = chrono::Duration::minutes;
    let working = record_fixture("api");
    let blocked = SessionRecord {
        state: SessionState::Blocked,
        ..record_fixture("web")
    };
    let lost = SessionRecord {
        state: SessionState::Interrupted,
        ..record_fixture("docs")
    };
    let done = SessionRecord {
        state: SessionState::Done,
        ..record_fixture("infra")
    };
    let records: HashMap<Uuid, SessionRecord> = [&working, &blocked, &lost, &done]
        .into_iter()
        .map(|r| (r.id, r.clone()))
        .collect();
    let older = begun(&working, at);
    let latest = begun(&working, at + mins(5));
    let mut ended = begun(&blocked, at + mins(9));
    ended.ended_at = Some(at + mins(10));
    let waiting = begun(&blocked, at + mins(1));
    let logged = [
        latest.clone(),
        older,
        ended,
        waiting.clone(),
        begun(&lost, at),
        begun(&done, at),
        begun(&record_fixture("forgotten"), at),
    ];
    assert_eq!(
        still_open(&records, &logged),
        HashMap::from([(working.id, latest.id), (blocked.id, waiting.id)]),
        "a lost or finished session's turn, and an ended one, are left to settle"
    );
}

#[tokio::test]
async fn a_session_at_work_across_a_restart_keeps_its_turn_and_one_lost_with_the_hub_does_not() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let dead = ended.id();
        ended.wait().unwrap();
        let alive = SessionRecord {
            origin: Origin::External,
            external_pid: Some(std::process::id() as i32),
            ..record_fixture("api")
        };
        let gone = SessionRecord {
            origin: Origin::External,
            external_pid: Some(dead as i32),
            ..record_fixture("web")
        };
        let at = Utc::now() - chrono::Duration::minutes(10);
        let (kept, lost) = (begun(&alive, at), begun(&gone, at));
        let logged = Arc::new(Logged::default());
        logged.0.lock().extend([begin(&kept), begin(&lost)]);
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        store.put(&alive).await.unwrap();
        store.put(&gone).await.unwrap();

        // The hub comes back over the same registry and log.
        let d = Desk::start(
            store,
            super::tests::options(dir.path()),
            Floor {
                turns: Some(logged.clone()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(d.view(alive.id).unwrap().state, SessionState::Working);
        assert_eq!(d.view(gone.id).unwrap().state, SessionState::Interrupted);
        assert_eq!(
            d.open_turns(),
            HashSet::from([kept.id]),
            "the floor sees the one at work run, as the sidebar does; the lost one settles"
        );

        d.change(alive.id, |r, _| {
            r.state = SessionState::Done;
            r.last_reply = Some("All green.".into());
        });
        assert!(
            logs(&logged, |ops| ops.iter().any(|op| matches!(
                op,
                TurnOp::End { turn, outcome: Outcome::Finished, .. } if *turn == kept.id
            )))
            .await,
            "the turn it was in ends"
        );
        let begins = logged
            .0
            .lock()
            .iter()
            .filter(|op| matches!(op, TurnOp::Begin { .. }))
            .count();
        assert_eq!(begins, 2, "no second turn begun for the same work");
        assert!(d.open_turns().is_empty());
    })
    .await;
}
