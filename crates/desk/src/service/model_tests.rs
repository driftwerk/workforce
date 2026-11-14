//! A session runs the model it was started with: on its first start and
//! on every resume of it, kept with it, and only ever a model's name on its
//! CLI's command line.

use std::os::unix::fs::PermissionsExt;

use super::tests::options;
use super::*;
use crate::store::{build_pool, tests::record_fixture};
use workforce_postgres::test_support::with_test_pool;

/// A stand-in `claude` that keeps what each session start was given, a
/// line each, and stays up until killed.
fn logging_cli(dir: &Path) -> PathBuf {
    let path = dir.join("logging-claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$*\" in *session-id*|*--resume*) echo \"$@\" >> '{}/starts' ;; esac\nexec sleep 60\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn starts(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("starts"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn logging_desk(pool: workforce_postgres::PostgresPool, dir: &Path) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    Desk::start(
        DeskStore::from_pool(pool),
        options(dir).with_program(Engine::Claude, logging_cli(dir)),
        Default::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn claude_on(dir: &Path, model: Option<&str>) -> NewSession {
    NewSession {
        fresh_worktree: false,
        engine: Engine::Claude,
        view: None,
        name: "auth-fix".into(),
        cwd: dir.to_string_lossy().into_owned(),
        goal: String::new(),
        worktree: None,
        prompt: None,
        approvals: None,
        approve_for_me: false,
        fast: false,
        model: model.map(str::to_string),
        account: None,
        env: Default::default(),
        group: None,
    }
}

async fn until_stopped(d: &Desk, id: Uuid) {
    for _ in 0..200 {
        if !d.view(id).unwrap().live {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {id} never stopped");
}

async fn until_started(dir: &Path, n: usize) -> Vec<String> {
    for _ in 0..200 {
        let s = starts(dir);
        if s.len() >= n {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never started {n} times: {:?}", starts(dir));
}

#[tokio::test]
async fn a_session_runs_its_model_and_keeps_it_through_a_resume() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let d = logging_desk(pool, dir.path()).await;
        let s = d
            .create(claude_on(dir.path(), Some(" opus ")), None)
            .await
            .unwrap();
        assert_eq!(s.model.as_deref(), Some("opus"), "it says it, trimmed");
        let first = until_started(dir.path(), 1).await;
        assert!(first[0].contains("--model opus"), "{first:?}");
        d.persisted().await;
        assert_eq!(
            store.get(s.id).await.unwrap().unwrap().model.as_deref(),
            Some("opus"),
            "kept, for its resume"
        );

        d.stop(s.id, None).unwrap();
        until_stopped(&d, s.id).await;
        let back = d.resume(s.id, None, None).await.unwrap();
        assert_eq!(back.model.as_deref(), Some("opus"));
        let both = until_started(dir.path(), 2).await;
        assert!(both[1].starts_with("--resume"), "{both:?}");
        assert!(both[1].contains("--model opus"), "{both:?}");

        // None given, none passed: the engine's own default.
        let plain = d
            .create(claude_on(dir.path(), Some("  ")), None)
            .await
            .unwrap();
        assert_eq!(plain.model, None);
        let all = until_started(dir.path(), 3).await;
        assert!(!all[2].contains("--model"), "{all:?}");
    })
    .await;
}

#[tokio::test]
async fn a_name_that_isnt_a_models_is_refused_and_nothing_starts() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = logging_desk(pool, dir.path()).await;
        for bad in [
            "--dangerously-skip-permissions",
            "-p",
            "opus --settings={}",
            "opus\n--x",
            "a\"b",
        ] {
            let refused = d.create(claude_on(dir.path(), Some(bad)), None).await;
            assert!(
                matches!(refused, Err(DeskError::Invalid(_))),
                "{bad:?}: {refused:?}"
            );
        }
        assert!(d.list().is_empty(), "nothing was made");
        assert!(starts(dir.path()).is_empty(), "nothing was run");
    })
    .await;
}

/// A record whose model was changed where Desk didn't check it (its row,
/// edited) is refused at its resume: nothing in it reaches the CLI.
#[tokio::test]
async fn a_kept_model_is_checked_again_at_every_start() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let was = SessionRecord {
            cwd: dir.path().to_string_lossy().into_owned(),
            state: SessionState::Stopped,
            cli_session_id: Some(Uuid::new_v4().to_string()),
            model: Some("--dangerously-skip-permissions".into()),
            ..record_fixture("tampered")
        };
        store.put(&was).await.unwrap();
        let d = logging_desk(pool, dir.path()).await;
        let refused = d.resume(was.id, None, None).await;
        assert!(matches!(refused, Err(DeskError::Invalid(_))), "{refused:?}");
        assert!(starts(dir.path()).is_empty(), "nothing was run");
        assert!(!d.view(was.id).unwrap().live);
    })
    .await;
}
