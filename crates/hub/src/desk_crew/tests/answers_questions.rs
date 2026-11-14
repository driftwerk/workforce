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

/// The person's settings that day, their questions theirs for 30 of the
/// crew's minutes (three seconds here).
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
        assert_eq!(o.wake.answer_after_minutes, 30);
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

/// A dialog's question (Claude's AskUserQuestion) while the person is in
/// control of the session: nothing is said of it, in the DM or a check-in,
/// until they have been quiet long enough; then the boss raises it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialogs_question_is_left_alone_while_the_person_is_in_control() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, llm) = overseeing(&base, pool, "ok", &theirs(), &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "plan the sends"}),
        )
        .await;
        let asked = "Send the three messages at once or one after another?";
        let answer = hook(
            &url,
            json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "toolu_q",
                "tool_input": {"questions": [{"question": asked, "header": "Sending", "multiSelect": false,
                    "options": [{"label": "At once", "description": "About 5 seconds."},
                                {"label": "One after another", "description": "About 20 seconds."}]}]}}),
        )
        .await;
        assert_eq!(answer, json!({}), "its terminal's dialog asks it");
        assert!(d.asks_in_dialog(s.id), "a dialog's question, not a permission");
        // Within the gap (three seconds), past several of the crew's minutes:
        // no DM, no check-in naming it.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(said(&overseer, &["Cufflink Migration"]), 0, "nothing in the DM");
        let woken = llm
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                String::from_utf8_lossy(&r.body)
                    .rsplit("END SYSTEM CONTEXT")
                    .next()
                    .is_some_and(|b| b.contains("Send the three messages"))
            })
            .count();
        assert_eq!(woken, 0, "no turn woken for it");
        // Quiet past it: the boss says it waits on them.
        eventually("the DM to raise it", async || {
            (said(&overseer, &["Cufflink Migration", "Send the three messages"]) > 0).then_some(())
        })
        .await;
    })
    .await;
}

/// At Act fully a dialog's questions (Claude's AskUserQuestion) are held at
/// their start and answered by the boss in their place, so the dialog
/// never opens; left to the person, or while they are in control, it opens.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_answers_a_dialogs_questions_in_its_place() {
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
            "Send the three messages",
            "ANSWER: Sending: At once. Counts: Yes.\nWHY: it is faster and the counts are read-only",
        )
        .await;
        decides(&llm, "Which customer first", "ASK: which customer gets the first invite?").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let since = chrono::Utc::now();
        let dialog = |q: &str| {
            json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion", "tool_use_id": format!("toolu_{}", q.len()),
                "tool_input": {"questions": [
                    {"question": q, "header": "Sending", "multiSelect": false,
                     "options": [{"label": "At once", "description": "Fast."}, {"label": "One after another", "description": "Slow."}]},
                    {"question": "Run the read-only counts first?", "header": "Counts", "multiSelect": false,
                     "options": [{"label": "Yes", "description": "Before."}, {"label": "No", "description": "Skip."}]}]}})
        };
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        // A sweep tells Desk which sessions' dialogs the boss answers.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Its boss answers every question in the dialog's place.
        let answer = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, dialog("Send the three messages at once or one after another?")),
        )
        .await
        .expect("answered within its hold");
        let out = &answer["hookSpecificOutput"];
        assert_eq!(out["permissionDecision"], "deny", "{answer}");
        let said_to_it = out["permissionDecisionReason"].as_str().unwrap_or_default();
        assert!(
            said_to_it.ends_with("answered this for the person: Sending: At once. Counts: Yes."),
            "{said_to_it}"
        );
        let call = calls_about(&llm, "Send the three messages").await;
        assert_eq!(call.len(), 1);
        assert!(call[0].contains("Run the read-only counts first?"), "every question");
        let v = d.view(s.id).unwrap();
        assert!(v.pending.is_none(), "nothing waits: {:?}", v.pending);
        let kept = eventually("its decision", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            rows.into_iter().find(|r| r.session == Some(s.id))
        })
        .await;
        assert!(
            kept.what.starts_with("answered Cufflink Migration's question “Send the three messages"),
            "{kept:?}"
        );

        // Left to the person: its dialog opens, and the DM asks in one line.
        let answer = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, dialog("Which customer first, Acme or Globex?")),
        )
        .await
        .expect("let go within its hold");
        assert_eq!(answer, json!({}), "its dialog asks the person");
        eventually("the DM to ask the person", async || {
            (said(&overseer, &["Cufflink Migration asks: which customer gets the first invite?"]) == 1)
                .then_some(())
        })
        .await;
        // A request in front of its open dialog, decided: the question shows
        // again as its dialog's alone, and nothing more is said of it.
        decides(&llm, "ls -la ./notes", "ALLOW: it only lists a folder").await;
        let allowed = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                "tool_input": {"command": "ls -la ./notes"}})),
        )
        .await
        .expect("decided");
        assert_eq!(allowed["hookSpecificOutput"]["decision"]["behavior"], "allow", "{allowed}");
        let back = eventually("its question to show again", async || {
            d.view(s.id).unwrap().pending.filter(|p| p.detail.starts_with("Which customer"))
        })
        .await;
        assert!(!back.answerable, "only its dialog answers it now");
        // A few sweeps.
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert_eq!(said(&overseer, &["Cufflink Migration asks: which customer gets the first invite?"]), 1, "asked once");
        assert_eq!(said(&overseer, &["refused"]), 0);
        assert_eq!(said(&overseer, &["Which customer first", "waiting on you"]), 0);
        let call = format!("toolu_{}", "Which customer first, Acme or Globex?".len());
        hook(&url, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": call})).await;

        // The person in control: its dialog opens at once, held for no one.
        hook(&url, json!({"hook_event_name": "UserPromptSubmit", "prompt": "now the sends"})).await;
        let started = std::time::Instant::now();
        let answer = hook(&url, dialog("Send them now or tomorrow morning, then?")).await;
        assert_eq!(answer, json!({}));
        assert!(started.elapsed() < Duration::from_secs(2), "not held");
        assert!(calls_about(&llm, "Send them now or tomorrow").await.is_empty());
    })
    .await;
}

/// At Act fully its boss reviews the plan a session presents: changes it
/// asks for are what the session reads; a plan it approves goes ahead
/// without asking the person; one only the person may approve is theirs.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_reviews_a_sessions_plan_in_its_place() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, llm) = overseeing(&base, pool, "ok", &theirs(), &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "Plan: migrate the invites table", "CHANGES: run it on staging first.").await;
        decides(&llm, "Plan: migrate on staging, then production", "APPROVE: staging goes first now").await;
        decides(&llm, "Plan: drop the legacy invites table", "ASK: drop the legacy table in production tonight?").await;
        decides(&llm, "Plan: shadow the writes", "**APPROVE:** it is the same plan").await;
        decides(&llm, "Plan: read from the replica", "APPROVE: reads go to the replica").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let since = chrono::Utc::now();
        let whole = |plan: &str| format!("# {plan}\n\n1. Write the migration.\n2. Run it.");
        let start = |plan: &str, call: &str| {
            json!({"hook_event_name": "PreToolUse", "tool_name": "ExitPlanMode", "tool_use_id": call,
                "tool_input": {"plan": whole(plan)}})
        };
        // Its permission check, which names no call.
        let check_of = |plan: &str| {
            json!({"hook_event_name": "PermissionRequest", "tool_name": "ExitPlanMode",
                "tool_input": {"plan": whole(plan)}})
        };
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Changes asked for: the session reads them, and no dialog opens.
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, start("Plan: migrate the invites table", "toolu_p1")),
        )
        .await
        .expect("reviewed within its hold");
        let o = &out["hookSpecificOutput"];
        assert_eq!(o["permissionDecision"], "deny", "{out}");
        let reason = o["permissionDecisionReason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("reviewed the plan for the person and wants these changes first: run it on staging first."),
            "{reason}"
        );
        assert!(d.view(s.id).unwrap().pending.is_none());

        // Approved: its start goes on, and its permission check is allowed.
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, start("Plan: migrate on staging, then production", "toolu_p2")),
        )
        .await
        .expect("reviewed within its hold");
        assert_eq!(out, json!({}), "its start goes on");
        let check = hook(&url, check_of("Plan: migrate on staging, then production")).await;
        assert_eq!(check["hookSpecificOutput"]["decision"]["behavior"], "allow", "{check}");
        // Allowed once: the same check again is the person's.
        assert_eq!(hook(&url, check_of("Plan: migrate on staging, then production")).await, json!({}));
        hook(&url, json!({"hook_event_name": "PostToolUse", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_p2"})).await;
        let rows = eventually("both reviews", async || {
            let rows = d.store().decisions_since("desk", since, 50).await.unwrap();
            (rows.iter().filter(|r| r.session == Some(s.id)).count() >= 2).then_some(rows)
        })
        .await;
        assert!(
            rows.iter().any(|r| r.what.starts_with("asked Cufflink Migration to change its plan “Plan: migrate the invites table”")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.what == "approved Cufflink Migration's plan “Plan: migrate on staging, then production”"),
            "{rows:?}"
        );

        // A step only the person may approve: its dialog asks them, and the
        // DM says so in one line; its permission check isn't allowed.
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, start("Plan: drop the legacy invites table", "toolu_p3")),
        )
        .await
        .expect("let go within its hold");
        assert_eq!(out, json!({}));
        let check = hook(&url, check_of("Plan: drop the legacy invites table")).await;
        assert_eq!(check, json!({}), "the person approves it");
        eventually("the DM to ask the person", async || {
            (said(&overseer, &["Cufflink Migration's plan", "waits for you: drop the legacy table in production tonight?"]) == 1)
                .then_some(())
        })
        .await;
        hook(&url, json!({"hook_event_name": "PostToolUse", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_p3"})).await;

        // An approval is for its plan alone: approved, with no permission
        // check after (the session's mode skips it), another plan's check
        // is the person's, and so is that plan's once the person drives.
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, start("Plan: read from the replica", "toolu_p4")),
        )
        .await
        .expect("reviewed within its hold");
        assert_eq!(out, json!({}));
        let other = hook(&url, check_of("Plan: drop the legacy invites table")).await;
        assert_eq!(other, json!({}), "not the plan its boss approved: {other}");
        hook(&url, json!({"hook_event_name": "PostToolUse", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_p4"})).await;
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            hook(&url, start("Plan: shadow the writes", "toolu_p5")),
        )
        .await
        .expect("reviewed within its hold");
        assert_eq!(out, json!({}));
        hook(&url, json!({"hook_event_name": "UserPromptSubmit", "prompt": "hold on, let me look"})).await;
        let theirs = hook(&url, check_of("Plan: shadow the writes")).await;
        assert_eq!(theirs, json!({}), "the person drives it now: {theirs}");
    })
    .await;
}
