//! A boss's line or work turn that did nothing stays out of Activity; one
//! that did something, by a tool or by its reply answering, is said there.

use super::*;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\nupdates = { enabled = false }\n";

async fn bosss_turn_rows(d: &Desk) -> Vec<workforce_desk::crew::Event> {
    bosss_turn_rows_of(d, &[Kind::Started, Kind::Finished]).await
}

async fn bosss_turn_rows_of(d: &Desk, kinds: &[Kind]) -> Vec<workforce_desk::crew::Event> {
    d.activity(Filter::All, None, 100)
        .await
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.actor.id == "desk" && kinds.contains(&e.kind))
        .collect()
}

/// Until the boss's turns have all returned to their callers.
async fn settled(crew: &CrewHub) {
    eventually("the boss's turns returned", async || {
        (crew.quiet_ends.lock().is_empty() && crew.running.lock().is_empty()).then_some(())
    })
    .await;
}

async fn finished(d: &Desk, n: usize) -> Vec<workforce_desk::crew::Event> {
    eventually("the boss's turns said", async || {
        let rows = bosss_turn_rows(d).await;
        (rows.iter().filter(|e| e.kind == Kind::Finished).count() >= n).then_some(rows)
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_turn_that_did_nothing_is_kept_out_of_activity_and_one_that_acted_is_said() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, llm) = overseeing(dir.path(), pool, "5433", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let tell = async |text: &str| {
            d.line_call(s.id, "tell", &json!({"to": "desk", "text": text}), None)
                .await
                .unwrap();
        };
        tell("acme-web, 00:21. CI is still running.").await;
        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        eventually("its decision to send nothing", async || {
            d.store()
                .decisions_since("desk", since, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|x| x.kind == workforce_desk::decision::DecisionKind::LeftAlone)
        })
        .await;
        settled(&crew).await;
        let rows = bosss_turn_rows(&d).await;
        assert!(rows.is_empty(), "{rows:?}");
        // What it told is said without its stamp.
        let told = d.activity(Filter::All, None, 100).await.unwrap().events;
        assert!(
            told.iter()
                .any(|e| e.kind == Kind::Told && e.text.ends_with("“CI is still running.”")),
            "{told:?}"
        );

        // A tool outside the gated actions that keeps something.
        calls_tool_once(
            &llm,
            "CI is green on 4c16e2e.",
            "crew_promise",
            json!({"session_id": s.id.to_string(), "text": "Tell you when acme-web's PR merges", "done_when": "merged"}),
        )
        .await;
        tell("CI is green on 4c16e2e.").await;
        let rows = finished(&d, 1).await;
        assert_eq!(rows.len(), 1, "one end, and no start: {rows:?}");

        // A step the boss takes itself, with a tool that isn't the crew's.
        calls_tool_once(
            &llm,
            "The release notes are missing.",
            "filesystem_write",
            json!({"path": "release-notes.md", "content": "acme-web 4c16e2e: CI green."}),
        )
        .await;
        let left_alone = async || {
            d.store()
                .decisions_since("desk", since, 50)
                .await
                .unwrap()
                .into_iter()
                .filter(|x| x.kind == workforce_desk::decision::DecisionKind::LeftAlone)
                .count()
        };
        let quiet = left_alone().await;
        tell("The release notes are missing.").await;
        let rows = finished(&d, 2).await;
        assert_eq!(rows.len(), 2, "its end is said: {rows:?}");
        assert_eq!(
            left_alone().await,
            quiet + 1,
            "it still sent the session nothing"
        );

        // A reply that answers what the session asked.
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "Which port does the test database use?", "wait_seconds": 30}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(asked["answer"], "5433", "{asked}");
        let rows = finished(&d, 3).await;
        assert!(
            rows.iter().all(|e| e.kind == Kind::Finished),
            "{rows:?}"
        );
    })
    .await;
}

/// A work turn on acme-web's turn end whose one call is `tool` with
/// `arguments`: once it has returned, its decision to let the session wait.
async fn work_turn_calling(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    tool: &str,
    arguments: Value,
) -> (Desk, workforce_desk::decision::BossDecision) {
    let (overseer, crew, d, llm) = overseeing(
        dir,
        pool,
        "CI is still running, so it should wait.",
        "[oversight]\nenabled = true\nlevel = \"act\"\nupdates = { enabled = false }\n",
        &[],
    )
    .await;
    calls_tool_once(&llm, "Pushed; CI is running.", tool, arguments).await;
    let s = new_session(&d, dir, "acme-web").await;
    let url = hook_url(dir, s.id).await;
    hook(
        &url,
        json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
    )
    .await;
    hook(
        &url,
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "Upgrade the date picker"}),
    )
    .await;
    let boss = workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer));
    d.take_on(
        s.id,
        &boss,
        workforce_desk::charge::How::Handed,
        &Default::default(),
        &By::you(),
    )
    .await
    .unwrap();
    hook(
        &url,
        json!({"hook_event_name": "Stop", "last_assistant_message": "Pushed; CI is running."}),
    )
    .await;
    let waited = eventually("its decision to let it wait", async || {
        d.store()
            .decisions_since("desk", chrono::Utc::now() - chrono::Duration::hours(1), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|x| x.need == workforce_desk::decision::Need::TurnEnd)
    })
    .await;
    settled(&crew).await;
    (d, waited)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_work_turn_that_only_looked_lets_the_session_wait_out_of_activity() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, waited) = work_turn_calling(
            pool,
            dir.path(),
            "crew_overview",
            json!({"reason": "see where acme-web stands"}),
        )
        .await;
        assert_eq!(waited.what, "let acme-web wait");
        assert_eq!(waited.button(), Some("undo"));
        let rows = bosss_turn_rows(&d).await;
        assert!(rows.is_empty(), "{rows:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_work_turn_that_took_a_step_itself_is_said_and_still_lets_the_session_wait() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, waited) = work_turn_calling(
            pool,
            dir.path(),
            "filesystem_write",
            json!({"path": "ci-notes.md", "content": "acme-web: CI running."}),
        )
        .await;
        assert_eq!(waited.what, "let acme-web wait", "with its Go ahead");
        assert_eq!(waited.button(), Some("undo"));
        let rows = finished(&d, 1).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_turn_that_failed_is_said_though_it_did_nothing() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, _crew, d, llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains("The deploy is stuck."))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({"error": {"message": "bad request", "type": "invalid_request_error"}}),
            ))
            .with_priority(1)
            .mount(&llm)
            .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        d.line_call(
            s.id,
            "tell",
            &json!({"to": "desk", "text": "The deploy is stuck."}),
            None,
        )
        .await
        .unwrap();
        let rows = eventually("its failure said", async || {
            let rows = bosss_turn_rows_of(&d, &[Kind::Started, Kind::Failed]).await;
            rows.iter().any(|e| e.kind == Kind::Failed).then_some(rows)
        })
        .await;
        assert!(
            rows.iter().all(|e| e.kind == Kind::Failed),
            "no start: {rows:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_end_held_from_an_earlier_turn_is_never_said_for_a_later_one() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let party = workforce_desk::crew::Party::agent("desk", "Desk");
        let end = |text: &str| {
            workforce_desk::crew::Event::new(
                Kind::Finished,
                party.clone(),
                Some(party.clone()),
                text,
            )
        };
        let key = ("desk".to_string(), "desk:line:s".to_string());
        // A turn that finished after its caller had settled it.
        crew.quiet_ends.lock().insert(
            key.clone(),
            (
                0,
                std::time::Instant::now(),
                end("finished · the stale one"),
            ),
        );
        crew.turn_end_said("desk", &key.1, 1, true);
        assert!(crew.quiet_ends.lock().is_empty(), "dropped");
        // Another session's line turn, still running, keeps its own.
        let other = ("desk".to_string(), "desk:line:t".to_string());
        crew.quiet_ends.lock().insert(
            other.clone(),
            (0, std::time::Instant::now(), end("finished · another")),
        );
        crew.turn_end_said("desk", &key.1, 1, true);
        assert!(crew.quiet_ends.lock().contains_key(&other), "kept");
        crew.quiet_ends.lock().remove(&other);
        // This turn's own end is said.
        crew.quiet_ends.lock().insert(
            key.clone(),
            (1, std::time::Instant::now(), end("finished · this one")),
        );
        crew.turn_end_said("desk", &key.1, 1, true);
        let rows = finished(&d, 1).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].text.contains("this one"), "{rows:?}");
    })
    .await;
}
