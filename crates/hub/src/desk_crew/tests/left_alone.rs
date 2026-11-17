//! A session the person took back from their Act fully boss is left alone
//! until they hand it back, as they expected on 11/17 ("I thought if Desk
//! doesn't have the session it would not touch it at all"): its boss had
//! gone on answering its reports, its requests and a pull request's
//! change.
//!
//! - What it asked its boss and still waits on is the person's.
//! - What it tells Desk is kept, and no boss's turn replies to it; it is
//!   told nobody at Desk answers it now.
//! - Its requests wait on the person: no decision call is made for them,
//!   and a boss's answer is refused.
//! - A boss's message to it, whatever woke the boss, is refused, never
//!   suggested.
//! - Handed back, its boss runs it as before.

use super::super::actions::{Action, Gate, Target};
use super::decides_at_once::{calls_about, decides, THEIRS};
use super::*;
use workforce_desk::model::Decision;

#[tokio::test(flavor = "multi_thread")]
async fn a_session_the_person_took_back_gets_nothing_from_its_boss_until_handed_back() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (_overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "ledger-notes", "ALLOW: it only reads its notes").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "Ledger Rebuild").await;
        eventually("it is taken on", async || {
            d.charge_of(s.id).filter(|c| c.live())
        })
        .await;
        // A question to its boss, still open as the person takes it back.
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "Shall I drop the old ledger table?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        d.release(s.id, &By::you()).await.unwrap();
        let q = eventually("its question, the person's", async || {
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked != Some(workforce_desk::line::Asked::Open))
        })
        .await;
        assert_eq!(
            q.asked,
            Some(workforce_desk::line::Asked::Escalated),
            "given to the person, never answered by the boss: {q:?}"
        );
        let (_, o) = crew.overseer_service().unwrap();

        // It reports to Desk: kept, and it is told nobody answers.
        let told = d
            .line_call(
                s.id,
                "tell",
                &json!({"to": "desk", "text": "PR #41 is open and green; shall I merge?"}),
                None,
            )
            .await
            .unwrap();
        assert!(
            told["note"]
                .as_str()
                .is_some_and(|n| n.contains("nobody at Desk answers it")),
            "{told}"
        );

        // It asks to read its notes: the person's, with no decision call.
        let url = hook_url(&base, s.id).await;
        let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
            "tool_input": {"command": "cat ledger-notes.md"}});
        let waits = tokio::spawn(async move { hook(&url, body).await });
        let p = eventually("its request, waiting", async || {
            d.view(s.id).unwrap().pending
        })
        .await;
        tokio::time::sleep(FAST.coalesce * 4 + Duration::from_secs(2)).await;
        assert!(
            d.view(s.id).unwrap().pending.is_some_and(|q| q.id == p.id),
            "still waits on the person"
        );
        assert!(
            calls_about(&llm, "ledger-notes").await.is_empty(),
            "no decision call for it"
        );
        let by = crew.by_overseer("It only reads its notes.").unwrap();
        let approve = Action::Approve {
            session: s.id,
            approval: p.id,
        };
        let refused = crew.perform(&approve, by.clone()).await.unwrap_err();
        assert!(refused.contains("took Ledger Rebuild back"), "{refused}");

        // A pull request's change wakes its boss: its message is refused,
        // not suggested to the person.
        let nudge = Action::Nudge {
            to: Target::Session(s.id),
            message: "PR #41 changed upstream: rebase it.".into(),
            reply_to: None,
        };
        assert!(matches!(crew.gate(&nudge, &o), Gate::No(_)));
        let refused = crew.perform(&nudge, by.clone()).await.unwrap_err();
        assert!(refused.contains("leave it alone"), "{refused}");

        // No boss's turn was spent on its line, and nothing of a boss's
        // reached it but the release's own note.
        let line_turns = llm
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .filter(|b| b.contains(super::super::line::LINE) && b.contains(&s.id.to_string()))
            .count();
        assert_eq!(line_turns, 0, "no line turn for it");
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let from_boss: Vec<_> = thread
            .iter()
            .filter(|m| {
                m.from.kind == PartyKind::Manager
                    && m.delivery != workforce_desk::line::Delivery::Noted
                    && m.delivery != workforce_desk::line::Delivery::Withdrawn
            })
            .map(|m| m.text.clone())
            .collect();
        assert!(from_boss.is_empty(), "{from_boss:?}");

        // The person answers it.
        d.decide(s.id, p.id, Decision::Allow, Some(By::you()))
            .unwrap();
        let answer = waits.await.unwrap();
        assert_eq!(
            answer["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "{answer}"
        );

        // Handed back, its boss runs it again.
        let boss = workforce_desk::crew::Party::manager("desk", &by.party.name);
        d.take_on(
            s.id,
            &boss,
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        assert!(!crew.taken_back(s.id));
        assert!(!matches!(crew.gate(&nudge, &o), Gate::No(_)));
        // Sent (it may wait on its line while the person has the wheel).
        crew.perform(&nudge, by).await.unwrap();
    })
    .await;
}
