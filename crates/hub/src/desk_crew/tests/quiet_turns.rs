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
        let rows = finished(&d, 2).await;
        assert!(
            rows.iter().all(|e| e.kind == Kind::Finished),
            "{rows:?}"
        );
    })
    .await;
}
