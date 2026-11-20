//! A boss's turn on what woke it that did nothing stays out of Activity
//! (20 Nov: dozens of "finished · I sent the session nothing." rows); one
//! that did something is said there as before.

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

#[tokio::test(flavor = "multi_thread")]
async fn a_line_turn_that_did_nothing_is_kept_out_of_activity_and_one_that_acted_is_said() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, llm) =
            overseeing(dir.path(), pool, "I sent the session nothing.", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        d.line_call(
            s.id,
            "tell",
            &json!({"to": "desk", "text": "CI is still running."}),
            None,
        )
        .await
        .unwrap();
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
        assert_eq!(crew.acted_by("desk"), 0, "leaving it alone is no act");
        assert!(
            bosss_turn_rows(&d).await.is_empty(),
            "{:?}",
            bosss_turn_rows(&d).await
        );

        calls_tool_once(
            &llm,
            "CI is green on 4c16e2e.",
            "crew_nudge",
            json!({"session_id": s.id.to_string(), "message": "Merge it now.", "reason": "CI is green."}),
        )
        .await;
        d.line_call(
            s.id,
            "tell",
            &json!({"to": "desk", "text": "CI is green on 4c16e2e."}),
            None,
        )
        .await
        .unwrap();
        eventually("the nudge", async || {
            (crew.acted_by("desk") > 0).then_some(())
        })
        .await;
        let rows = eventually("the turn that acted, said", async || {
            let rows = bosss_turn_rows(&d).await;
            rows.iter()
                .any(|e| e.kind == Kind::Finished)
                .then_some(rows)
        })
        .await;
        assert!(
            rows.iter().all(|e| e.kind == Kind::Finished),
            "its start isn't said: {rows:?}"
        );
    })
    .await;
}
