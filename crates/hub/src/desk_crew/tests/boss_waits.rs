//! Cufflink Migration, 19 Nov 2026, 10:25–10:32, replayed: the session's
//! report reached Desk cut short, its reply to Desk's status question was
//! refused ("no question waiting on you"), Desk's decision in reply to its
//! report was refused the same way and then dropped as a repeat of the
//! hold Desk had sent, and the session sat waiting until the person opened
//! it. Every word between a session and its boss reaches whom it answers.

use std::future::Future;

use workforce_desk::line::{Delivery, LineKind};
use workforce_desk::open_item::{OpenKind, OpenState};

use super::super::actions::{Action, Target, STATUS_QUESTION};
use super::super::picture::stale;
use super::*;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n";

/// The session's 10:26:54 report, 2,541 characters in the incident.
fn report() -> String {
    let mut r = String::from(
        "Re m:b6ff0f8c: the staging REINDEX is done, but it can't fix the drill. Both need your decision.\n\n\
         2. Why it doesn't help: 16.16's back-branch fix hard-codes the change in the server's C functions, \
         not in the catalog, because back branches can't change catalogs. ",
    );
    while r.len() < 2_400 {
        r.push_str("Staging's DB host runs 16.15, so its server still writes the flag into every rebuilt index. ");
    }
    r.push_str(
        "\n\nOptions:\n(a) Upgrade staging's PostgreSQL to 16.16, then REINDEX again.\n\
         (b) Make the drill install the host's exact minor.\n(c) Accept the message.\n\
         I recommend (a) on staging in the day.",
    );
    r
}

fn nudge(to: uuid::Uuid, message: &str, reply_to: Option<uuid::Uuid>) -> Action {
    Action::Nudge {
        to: Target::Session(to),
        message: message.into(),
        reply_to,
    }
}

async fn in_turn<F: Future>(sid: &str, f: F) -> F::Output {
    workforce_agent::runner::CURRENT_SESSION_ID
        .scope(sid.to_string(), f)
        .await
}

fn tool(crew: &Arc<CrewHub>, name: &str) -> super::super::tools::CrewTool {
    super::super::tools::CrewTool::for_level(crew, "desk", &Default::default(), false)
        .into_iter()
        .find(|t| workforce_agent::tools::Tool::name(t) == name)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_session_and_its_boss_say_reaches_whom_it_answers_whole() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let raw = pool.clone();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let s = new_session(&d, dir.path(), "Cufflink Migration").await;
        let by = || crew.by_overseer("Its report reached me cut short.").unwrap();

        // 10:26:54, while a boss turn begun before it runs: the report.
        let (go, sent) = tokio::sync::oneshot::channel::<()>();
        let early = {
            let crew = Arc::clone(&crew);
            let action = nudge(s.id, "Correction to my 10:25 message: production read-only.", None);
            tokio::spawn(async move {
                crew.on_picture(async {
                    let _ = sent.await;
                    let by = crew.by_overseer("Production stays read-only.").unwrap();
                    crew.perform(&action, by).await
                })
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let long = report();
        let r = d
            .line_call(s.id, "tell", &json!({"to": "desk", "text": long}), None)
            .await
            .unwrap();
        let report_id: uuid::Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        go.send(()).unwrap();
        // The turn decided before it is told what moved: cut short, and
        // where the whole of it is, never the session's words breaking off.
        let refused = early.await.unwrap().unwrap_err();
        assert!(stale(&refused), "{refused}");
        assert!(refused.contains("Desk cut this short here, not the session"), "{refused}");
        assert!(
            refused.contains(&format!("crew.history, message_id \"{report_id}\"")),
            "{refused}"
        );
        let whole = workforce_agent::tools::Tool::execute(
            &tool(&crew, "crew.history"),
            json!({ "message_id": report_id.to_string() }),
        )
        .await
        .unwrap();
        assert!(whole.success, "{:?}", whole.error);
        let read = whole.output["message"].as_str().unwrap();
        assert!(read.contains("I recommend (a) on staging in the day."), "{read}");
        assert!(read.contains("Both need your decision."), "{read}");

        // 10:27:26: Desk's status question, with a hold in its words. It
        // asks for words only: never "carry on as you were" beside a hold.
        crew.perform(
            &nudge(
                s.id,
                &format!("{STATUS_QUESTION} Resend the rest in short form. Change nothing until Desk answers."),
                None,
            ),
            by(),
        )
        .await
        .unwrap();
        assert!(!STATUS_QUESTION.contains("carry on"), "{STATUS_QUESTION}");
        let status = d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .into_iter()
            .rfind(|m| m.text.starts_with(STATUS_QUESTION))
            .unwrap();
        let promise = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.kind == OpenKind::Promise)
            .expect("the status question's promise");

        // 10:28:04, 40 seconds on: the session's reply to it by its mark.
        raw.get()
            .await
            .unwrap()
            .execute(
                "UPDATE line_messages SET at = at - interval '40 seconds', \
                 delivered_at = delivered_at - interval '40 seconds' WHERE id = $1",
                &[&status.id],
            )
            .await
            .unwrap();
        let mark = status.id.simple().to_string()[..8].to_string();
        let r = d
            .line_call(
                s.id,
                "reply",
                &json!({"message_id": mark, "text": format!("The short form, resent. {long}")}),
                None,
            )
            .await
            .expect("a reply to Desk's status question is never refused");
        let reply_id: uuid::Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let reply = d.store().line_message(reply_id).await.unwrap().unwrap();
        assert_eq!(reply.kind, LineKind::Tell);
        assert_eq!(reply.in_reply_to, Some(status.id));
        assert_eq!(reply.to.kind, PartyKind::Manager);
        assert_eq!(reply.delivery, Delivery::Delivered, "Desk has it");
        assert!(reply.text.ends_with("I recommend (a) on staging in the day."), "whole");
        let closed = eventually("the promise kept by the reply", async || {
            d.store()
                .open_item(promise.id)
                .await
                .unwrap()
                .filter(|i| i.state == OpenState::Closed)
        })
        .await;
        assert!(closed.closed_why.unwrap().contains("replied"));

        // 10:29: Desk's hold, from one turn; 10:30 its decision, from
        // another, in reply to the report (a message, no question): sent as
        // a message, never refused, never dropped as a repeat of the hold.
        crew.picture_turn_began("line-1029");
        let hold = "Send your full answer as a new report. Don't change staging any further until I answer.";
        in_turn("line-1029", crew.perform(&nudge(s.id, hold, None), by()))
            .await
            .unwrap();
        // The same hold again is still a repeat.
        crew.picture_turn_began("line-1029b");
        let again = in_turn("line-1029b", crew.perform(&nudge(s.id, hold, None), by()))
            .await
            .unwrap();
        assert!(!again.starts_with("told"), "{again}");
        crew.picture_turn_began("line-1030");
        let decided = in_turn(
            "line-1030",
            crew.perform(
                &nudge(s.id, "Decision: (b), not (a) today.", Some(report_id)),
                by(),
            ),
        )
        .await
        .unwrap();
        assert!(decided.starts_with("told Cufflink Migration"), "the answer to a hold: {decided}");
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let answer = thread
            .iter()
            .find(|m| m.text == "Decision: (b), not (a) today.")
            .expect("the decision is on its line");
        assert_eq!(answer.kind, LineKind::Tell);
        assert_eq!(answer.from.kind, PartyKind::Manager);
        // A reply can't reach another session through what it cites.
        crew.picture_turn_began("line-1032");
        let other = new_session(&d, dir.path(), "acme-api").await;
        let cited = in_turn(
            "line-1032",
            crew.perform(&nudge(other.id, "Merge it.", Some(report_id)), by()),
        )
        .await
        .unwrap_err();
        assert!(cited.starts_with("Not sent: message"), "{cited}");
    })
    .await;
}

/// A reply to a session's report is a message: with messaging sessions
/// off, citing the report doesn't send it.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_to_a_report_keeps_the_gates_of_a_message() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let config = "[oversight]\nenabled = true\nlevel = \"act\"\n[oversight.act]\nnudge = false\nanswer_sessions = true\n";
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", config, &[]).await;
        let s = new_session(&d, dir.path(), "Cufflink Migration").await;
        let r = d
            .line_call(s.id, "tell", &json!({"to": "desk", "text": report()}), None)
            .await
            .unwrap();
        let report_id: uuid::Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let said = workforce_agent::tools::Tool::execute(
            &tool(&crew, "crew.nudge"),
            json!({
                "session_id": s.id.to_string(),
                "reply_to": report_id.to_string(),
                "message": "Upgrade production now.",
                "reason": "It reported."
            }),
        )
        .await
        .unwrap();
        assert!(
            !said.output.to_string().contains("told Cufflink"),
            "{:?}",
            said
        );
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert!(
            !thread.iter().any(|m| m.text == "Upgrade production now."),
            "never sent"
        );
    })
    .await;
}
