//! At Act fully the boss answers the questions its sessions end a turn on,
//! itself (`answer`), as the person asked on 11/13:
//!
//! - While the person is in control of the session (they prompted it or
//!   typed in it less than `answer_after_minutes` ago, or are typing now),
//!   nothing is said of its question: no call, no answer, no DM.
//! - Once they have been quiet that long, the boss answers it: a short
//!   message on the session's line, a decision with its reason, and no work
//!   turn woken for it.
//! - Only what it can't decide is the person's: the DM asks it in one line.

use super::decides_at_once::{calls_about, decides, said, THEIRS};
use super::*;

/// The person's settings that day, their questions theirs for 20 of the
/// crew's minutes (two seconds here).
fn theirs() -> String {
    THEIRS.replace(
        "quiet_minutes = 30\n",
        "quiet_minutes = 30\nanswer_after_minutes = 20\n",
    )
}

/// What the boss has told session `id` on its line.
async fn told(d: &Desk, id: uuid::Uuid) -> Vec<String> {
    d.store()
        .line_inbox(&id.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.text)
        .collect()
}

/// The person prompts `url`'s session, and its turn ends asking `question`.
async fn asks(url: &str, question: &str) {
    hook(
        url,
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "carry on with the release"}),
    )
    .await;
    hook(
        url,
        json!({"hook_event_name": "Stop", "last_assistant_message": format!("Built it. {question}")}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn act_fully_answers_a_sessions_question_itself_once_the_person_is_quiet_and_never_while_they_type(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", &theirs(), &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(
            &llm,
            "Ship it?",
            "ANSWER: Yes, ship it to staging.\nWHY: it is built and staging is the safe next step",
        )
        .await;
        decides(
            &llm,
            "Run the migration?",
            "ANSWER: Yes, run it.\nWHY: its plan says so",
        )
        .await;
        decides(
            &llm,
            "Which customer gets the first invite?",
            "ASK: which customer should get the first invite?",
        )
        .await;
        let (_, o) = crew.overseer_service().unwrap();
        assert_eq!(o.wake.answer_after_minutes, 20);
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let since = chrono::Utc::now();

        // The person prompted it just now: its question is theirs for now.
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        asks(&url, "Ship it?").await;
        eventually("the question to be asked", async || {
            d.view(s.id).unwrap().asked.is_some().then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            calls_about(&llm, "Ship it?").await.is_empty(),
            "no call while the person is in control"
        );
        assert!(told(&d, s.id).await.is_empty(), "nothing sent");
        assert_eq!(said(&overseer, &["Ship it?"]), 0, "nothing said in the DM");

        // Quiet past the gap: the boss answers it itself, short.
        let answer = eventually("the boss's answer on its line", async || {
            told(&d, s.id).await.into_iter().next()
        })
        .await;
        assert_eq!(answer, "Yes, ship it to staging.");
        assert_eq!(calls_about(&llm, "Ship it?").await.len(), 1, "one call");
        let kept = eventually("its decision", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.into_iter().find(|r| r.session == Some(s.id))
        })
        .await;
        assert_eq!(
            kept.why, "it asked “Ship it?”: it is built and staging is the safe next step",
            "{kept:?}"
        );
        // The person reads it as a decision, never as a question for them.
        assert_eq!(said(&overseer, &["Cufflink Migration asks"]), 0);
        // No work turn was woken for it.
        tokio::time::sleep(FAST.coalesce * 4).await;
        let woken: Vec<String> = llm
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
            .filter(|m| m["role"] == "user")
            .map(|m| m["content"].to_string())
            .filter(|c| {
                !c.contains(super::super::decide::MARK)
                    && c.rsplit("END SYSTEM CONTEXT")
                        .next()
                        .is_some_and(|asked| asked.contains("Ship it?"))
            })
            .collect();
        assert!(
            woken.is_empty(),
            "woken for an answered question: {woken:?}"
        );

        // Typing in it now: silent however long the question waits, then
        // answered once they stop.
        let s2 = new_session(&d, &base, "acme-web").await;
        let url2 = hook_url(&base, s2.id).await;
        asks(&url2, "Run the migration?").await;
        for _ in 0..12 {
            d.person_typing(s2.id).unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        assert!(
            calls_about(&llm, "Run the migration?").await.is_empty(),
            "no call while they type"
        );
        assert!(told(&d, s2.id).await.is_empty());
        let answer = eventually("the answer once they stopped", async || {
            told(&d, s2.id).await.into_iter().next()
        })
        .await;
        assert_eq!(answer, "Yes, run it.");

        // Only what it can't decide is the person's, in one line.
        let s3 = new_session(&d, &base, "invites").await;
        let url3 = hook_url(&base, s3.id).await;
        asks(&url3, "Which customer gets the first invite?").await;
        eventually("the DM to ask the person", async || {
            (said(
                &overseer,
                &["invites asks: which customer should get the first invite?"],
            ) == 1)
                .then_some(())
        })
        .await;
        assert!(told(&d, s3.id).await.is_empty(), "nothing sent to it");
        let left = eventually("its decision to leave it", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.into_iter().find(|r| r.session == Some(s3.id))
        })
        .await;
        assert_eq!(left.why, "which customer should get the first invite?");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            calls_about(&llm, "Which customer gets the first invite?")
                .await
                .len(),
            1,
            "asked once"
        );
        // Asked once; its decision line says it was left to them.
        assert_eq!(said(&overseer, &["invites asks:"]), 1, "said once");
    })
    .await;
}
