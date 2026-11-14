//! At Act fully the boss keeps its sessions moving (`idle`), as the person
//! asked on 11/13: a session whose turn ends without asking anything is
//! judged at once, in one call.
//!
//! - Work left: a short, decisive next step on its line, with a decision
//!   and its reason; no work turn woken for it.
//! - Done: a decision that it waits, and nothing sent.
//! - Only the person can say: the DM asks it in one line.
//! - A call that fails is tried once more, then the DM says so.
//! - While the person is in control of it, nothing is said.

use super::decides_at_once::{calls_about, decides, said, THEIRS};
use super::*;

/// The person's settings that day, a session theirs for 30 of the crew's
/// minutes (three seconds here) after they gave it input.
fn theirs() -> String {
    THEIRS.replace(
        "quiet_minutes = 30\n",
        "quiet_minutes = 30\nanswer_after_minutes = 30\n",
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

/// `url`'s session ends its turn saying `said`, asking nothing.
async fn ends(url: &str, said: &str) {
    hook(
        url,
        json!({"hook_event_name": "Stop", "last_assistant_message": said}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn act_fully_tells_a_finished_session_its_next_step_itself_and_never_while_the_person_drives_it(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, llm) = overseeing(&base, pool, "ok", &theirs(), &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(
            &llm,
            "the tests aren't run yet",
            "CONTINUE: Run the test suite and fix what fails.\nWHY: the change isn't verified",
        )
        .await;
        decides(&llm, "the release is out", "DONE: the release is out").await;
        decides(
            &llm,
            "Staged the invites",
            "ASK: send the invites to customers today?",
        )
        .await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let since = chrono::Utc::now();
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // The person prompted it just now: its turn's end is theirs for now.
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "write the migration"}),
        )
        .await;
        ends(&url, "Wrote the migration; the tests aren't run yet.").await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            calls_about(&llm, "the tests aren't run yet").await.is_empty(),
            "no call while the person is in control"
        );
        assert!(told(&d, s.id).await.is_empty(), "nothing sent");

        // Quiet past the gap: the boss tells it its next step, short.
        let next = eventually("the boss's next step on its line", async || {
            told(&d, s.id).await.into_iter().next()
        })
        .await;
        assert!(
            next.contains("Run the test suite and fix what fails."),
            "{next}"
        );
        let kept = eventually("its decision", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.into_iter().find(|r| r.session == Some(s.id))
        })
        .await;
        assert!(kept.why.contains("the change isn't verified"), "{kept:?}");
        // Judged once, however many sweeps.
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert_eq!(calls_about(&llm, "the tests aren't run yet").await.len(), 1, "one call");
        assert_eq!(told(&d, s.id).await.len(), 1, "told once");
        // No work turn was woken for it.
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
                        .is_some_and(|asked| asked.contains("the tests aren't run yet"))
            })
            .collect();
        assert!(woken.is_empty(), "woken for a judged turn: {woken:?}");

        // Done: it waits, recorded, and nothing is sent.
        let s2 = new_session(&d, &base, "acme-web").await;
        let url2 = hook_url(&base, s2.id).await;
        ends(&url2, "Merged the PR; the release is out.").await;
        let rows = eventually("its decision", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.iter()
                .any(|r| r.session == Some(s2.id))
                .then_some(rows)
        })
        .await;
        assert!(
            rows.iter().any(|r| r.session == Some(s2.id) && r.what == "let acme-web wait"),
            "{rows:?}"
        );
        assert!(told(&d, s2.id).await.is_empty(), "nothing sent to it");
        assert_eq!(said(&overseer, &["acme-web"]), 0, "nothing said in the DM");

        // Only the person can say: one line in the DM.
        let s3 = new_session(&d, &base, "invites").await;
        let url3 = hook_url(&base, s3.id).await;
        ends(&url3, "Staged the invites for Acme.").await;
        eventually("the DM to ask the person", async || {
            (said(
                &overseer,
                &["invites finished its turn: send the invites to customers today?"],
            ) == 1)
                .then_some(())
        })
        .await;
        assert!(told(&d, s3.id).await.is_empty(), "nothing sent to it");

        // Its model's answer is none of them: tried once more, then the DM.
        let s4 = new_session(&d, &base, "billing").await;
        let url4 = hook_url(&base, s4.id).await;
        ends(&url4, "Refactored the billing module.").await;
        eventually("the DM to say it couldn't decide", async || {
            (said(&overseer, &["billing finished its turn. I couldn't decide what's next"]) == 1)
                .then_some(())
        })
        .await;
        assert_eq!(calls_about(&llm, "Refactored the billing module").await.len(), 2);
        assert!(told(&d, s4.id).await.is_empty());
    })
    .await;
}
