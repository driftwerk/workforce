//! A boss's line or work turn that did nothing stays out of Activity; one
//! that did something, by a tool or by its reply answering, is said there.

use super::*;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\nupdates = { enabled = false }\n";

async fn bosss_turn_rows(d: &Desk) -> Vec<workforce_desk::crew::Event> {
    d.activity(Filter::All, None, 100)
        .await
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.actor.id == "desk" && matches!(e.kind, Kind::Started | Kind::Finished))
        .collect()
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
        let (_overseer, _crew, d, llm) = overseeing(dir.path(), pool, "5433", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let tell = async |text: &str| {
            d.line_call(s.id, "tell", &json!({"to": "desk", "text": text}), None)
                .await
                .unwrap();
        };
        tell("CI is still running.").await;
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
        tokio::time::sleep(Duration::from_millis(300)).await;
        let rows = bosss_turn_rows(&d).await;
        assert!(rows.is_empty(), "{rows:?}");

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
        assert_eq!(left_alone().await, quiet, "nor is it kept as sending nothing");

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
        let began = chrono::Utc::now() - chrono::Duration::seconds(5);
        crew.quiet_ends
            .lock()
            .insert(key.clone(), (began, end("finished · the stale one")));
        crew.turn_end_said("desk", &key.1, chrono::Utc::now(), true);
        assert!(crew.quiet_ends.lock().is_empty(), "dropped");
        // This turn's own end is said.
        let since = chrono::Utc::now();
        crew.quiet_ends.lock().insert(
            key.clone(),
            (chrono::Utc::now(), end("finished · this one")),
        );
        crew.turn_end_said("desk", &key.1, since, true);
        let rows = finished(&d, 1).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].text.contains("this one"), "{rows:?}");
    })
    .await;
}
