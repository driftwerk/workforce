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
        let (taken_tx, basis_taken) = tokio::sync::oneshot::channel::<()>();
        let early = {
            let crew = Arc::clone(&crew);
            let action = nudge(s.id, "Correction to my 10:25 message: production read-only.", None);
            tokio::spawn(async move {
                crew.on_picture(async {
                    let _ = taken_tx.send(());
                    let _ = sent.await;
                    let by = crew.by_overseer("Production stays read-only.").unwrap();
                    crew.perform(&action, by).await
                })
                .await
            })
        };
        basis_taken.await.unwrap();
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

/// Below Act fully, a session's reports to its boss are never refused:
/// past the boss's turns for the hour they wait, gathered, for the next
/// (20 Nov: Legacy removal's 02:32 and 03:24 reports were refused by the
/// line's message limit and lost).
#[tokio::test(flavor = "multi_thread")]
async fn reports_past_the_bosss_turns_for_the_hour_wait_for_the_next_and_none_is_lost() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Noted.",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
             work = { max_turns_per_hour = 1 }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        d.take_on(
            s.id,
            &workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer)),
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        let channel = super::super::line::line_channel(s.id);
        let turns = || {
            overseer
                .sessions
                .find(&channel, PERSON)
                .unwrap()
                .map_or(0, |l| {
                    l.messages
                        .iter()
                        .filter(|m| m.content.starts_with(super::super::line::LINE))
                        .count()
                })
        };
        let tell = |text: &'static str| {
            let d = d.clone();
            async move {
                d.line_call(s.id, "tell", &json!({"to": "desk", "text": text}), None)
                    .await
                    .unwrap();
            }
        };
        tell("Inventory done: 14 legacy services on staging.").await;
        eventually("its first report taken", async || {
            (turns() == 1).then_some(())
        })
        .await;
        tell("Step 12 done: the queue is drained.").await;
        tell("Step 13 done: the old workers are stopped.").await;
        let waiting = || async { d.store().line_waiting_for_bosses().await.unwrap().len() };
        eventually("both kept waiting for the next turn", async || {
            (waiting().await == 2).then_some(())
        })
        .await;
        assert_eq!(turns(), 1, "the hour's one turn is spent");
        let next = eventually("the next hour's turn", async || {
            overseer
                .sessions
                .find(&channel, PERSON)
                .unwrap()?
                .messages
                .into_iter()
                .filter(|m| m.content.starts_with(super::super::line::LINE))
                .nth(1)
        })
        .await;
        assert!(
            next.content.contains("the queue is drained")
                && next.content.contains("the old workers are stopped"),
            "both reports, gathered as one turn: {}",
            next.content
        );
        // Taken once that turn is over.
        eventually("taken by that turn", async || {
            (waiting().await == 0).then_some(())
        })
        .await;
    })
    .await;
}

/// Past its boss's turns for the hour, what a session it runs asks it
/// still gets a turn at once: only reports wait.
#[tokio::test(flavor = "multi_thread")]
async fn past_the_hours_turns_a_question_to_the_boss_that_runs_it_is_taken_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Yes, drop it.",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
             work = { max_turns_per_hour = 1 }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        d.take_on(
            s.id,
            &workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer)),
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        assert!(crew.spend_work_turn(s.id), "the hour's one turn");
        assert!(!crew.spend_work_turn(s.id), "spent");
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "Drop the legacy queue table?", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        let q = eventually("its question taken", async || {
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked != Some(workforce_desk::line::Asked::Open))
        })
        .await;
        assert_eq!(
            q.asked,
            Some(workforce_desk::line::Asked::Answered),
            "{q:?}"
        );
    })
    .await;
}

/// Only a boss's turn that would run waits on its turns for the hour: a
/// question Desk gives the person without one (Desk only watches) reaches
/// them at once, however many of the hour's turns are spent.
#[tokio::test(flavor = "multi_thread")]
async fn past_the_hours_turns_a_question_desk_only_watches_still_reaches_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Noted.",
            "[oversight]\nenabled = true\nlevel = \"watch\"\nwork = { max_turns_per_hour = 1 }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        assert!(crew.spend_work_turn(s.id), "the hour's one turn");
        assert!(!crew.spend_work_turn(s.id), "spent");
        let asked = d
            .line_call(
                s.id,
                "ask",
                &json!({"to": "desk", "text": "Drop the legacy queue table?", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
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
            "given to the person, never held for the boss's next turn: {q:?}"
        );
    })
    .await;
}

/// A session the boss runs, its turns for the hour spent.
async fn paced_session(
    max: u32,
    reply: &str,
    pool: workforce_postgres::PostgresPool,
    dir: &std::path::Path,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, uuid::Uuid) {
    paced_session_at(max, reply, pool, dir, FAST).await
}

/// [`paced_session`], its crew keeping `timing`.
async fn paced_session_at(
    max: u32,
    reply: &str,
    pool: workforce_postgres::PostgresPool,
    dir: &std::path::Path,
    timing: super::super::Timing,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, uuid::Uuid) {
    let (overseer, crew, d, _llm) =
        overseeing_at(dir, pool, reply, &paced_config(max), &[], timing).await;
    let s = new_session(&d, dir, "Legacy removal").await;
    d.take_on(
        s.id,
        &workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer)),
        workforce_desk::charge::How::Handed,
        &Default::default(),
        &By::you(),
    )
    .await
    .unwrap();
    (overseer, crew, d, s.id)
}

/// The boss's config for [`paced_session`]: `max` work turns an hour.
fn paced_config(max: u32) -> String {
    format!(
        "[oversight]\nenabled = true\nlevel = \"act\"\nact = {{ nudge = true }}\n\
         work = {{ max_turns_per_hour = {max} }}\n"
    )
}

/// The line turns `overseer` had for session `s`.
fn line_turns(overseer: &AgentService, s: uuid::Uuid) -> usize {
    overseer
        .sessions
        .find(&super::super::line::line_channel(s), PERSON)
        .unwrap()
        .map_or(0, |l| {
            l.messages
                .iter()
                .filter(|m| m.content.starts_with(super::super::line::LINE))
                .count()
        })
}

/// A question from a session whose reports wait for its boss's next hour
/// is never held with them: it is answered at once, the reports with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_behind_reports_waiting_for_the_next_hour_is_answered_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _crew, d, s) = paced_session(1, "Yes, drop it.", pool, dir.path()).await;
        let tell = |text: &'static str| {
            let d = d.clone();
            async move {
                d.line_call(s, "tell", &json!({"to": "desk", "text": text}), None)
                    .await
                    .unwrap();
            }
        };
        tell("Inventory done: 14 legacy services on staging.").await;
        eventually("its first report taken", async || {
            (line_turns(&overseer, s) == 1).then_some(())
        })
        .await;
        tell("Step 12 done: the queue is drained.").await;
        eventually("the next report waiting for the next hour", async || {
            (d.store().line_waiting_for_bosses().await.unwrap().len() == 1).then_some(())
        })
        .await;
        let asked_at = std::time::Instant::now();
        let asked = d
            .line_call(
                s,
                "ask",
                &json!({"to": "desk", "text": "Drop the legacy queue table?", "wait_seconds": 60}),
                Some(std::time::Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        let q = eventually("its question answered", async || {
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked != Some(workforce_desk::line::Asked::Open))
        })
        .await;
        assert_eq!(
            q.asked,
            Some(workforce_desk::line::Asked::Answered),
            "{q:?}"
        );
        assert!(
            asked_at.elapsed() < std::time::Duration::from_secs(2),
            "at once, not at the next hour nor the next look: {:?}",
            asked_at.elapsed()
        );
        // The waiting report went with it: taken as the turn that answers
        // runs, which can end after the answer is kept.
        eventually("the waiting report taken with it", async || {
            d.store()
                .line_waiting_for_bosses()
                .await
                .ok()
                .filter(Vec::is_empty)
                .map(drop)
        })
        .await;
    })
    .await;
}

/// Round after round of report and answer between a session and the boss
/// that runs it: every report reaches the boss's turn, however many rounds
/// (the chain never climbs to the hops that hand it to the person).
#[tokio::test(flavor = "multi_thread")]
async fn every_report_in_rounds_with_the_boss_that_runs_it_reaches_its_turn() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, llm) =
            overseeing_at(dir.path(), pool, "Noted.", &paced_config(50), &[], FAST).await;
        let s = new_session(&d, dir.path(), "Legacy removal").await.id;
        d.take_on(
            s,
            &workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer)),
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        let rounds = usize::try_from(workforce_desk::rooms::MAX_HOPS).unwrap() * 3;
        let url = hook_url(dir.path(), s).await;
        for round in 1..=rounds {
            let sent = d
                .line_call(
                    s,
                    "tell",
                    &json!({"to": "desk", "text": format!("Step {round} done.")}),
                    None,
                )
                .await
                .unwrap_or_else(|e| panic!("report {round} refused: {e}"));
            let report: uuid::Uuid = serde_json::from_value(sent["id"].clone()).unwrap();
            // Its turn, which the boss's last answer started, ends.
            hook(&url, json!({"hook_event_name": "Stop", "last_assistant_message": format!("Step {round} done.")})).await;
            eventually("its report taken by a turn", async || {
                let taken = d.store().line_message(report).await.ok()??.delivery == Delivery::Delivered;
                let waits = d.store().line_waiting_for_bosses().await.ok()?;
                (taken && waits.is_empty()).then_some(())
            })
            .await;
            let hop = d.store().line_message(report).await.unwrap().unwrap().hop;
            // The boss's answer, a hop on as its turn's is, taken in.
            let mut by = crew.by_overseer("It reported.").unwrap();
            by.hop = hop + 1;
            crew.perform(
                &nudge(s, &format!("Go on to step {}.", round + 1), None),
                by,
            )
            .await
            .unwrap();
            let next = eventually("the answer on its line", async || {
                d.line_next(s, std::time::Duration::from_millis(200)).await
            })
            .await;
            let id: uuid::Uuid = serde_json::from_value(next["id"].clone()).unwrap();
            let text = next["text"].as_str().unwrap();
            hook(
                &url,
                json!({"hook_event_name": "UserPromptSubmit", "prompt": format!("The desk plugin sent a message: {text}")}),
            )
            .await;
            d.line_ack(s, id, true).await.unwrap();
        }
        let last = format!("Step {rounds} done.");
        eventually("the last report in the boss's turn", async || {
            llm.received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains(&last))
                .then_some(())
        })
        .await;
    })
    .await;
}

/// A report from session `s` to the boss, kept waiting for its next turn.
async fn kept_report(d: &Desk, s: uuid::Uuid, text: &str) -> workforce_desk::line::LineMessage {
    let v = d.view(s).unwrap();
    let mut m = workforce_desk::line::LineMessage::new(
        workforce_desk::crew::Party::session(&v),
        workforce_desk::crew::Party::manager("desk", "Desk"),
        LineKind::Tell,
        text.into(),
        1,
    );
    m.delivery = Delivery::Queued;
    d.store().put_line(&m).await.unwrap();
    m
}

/// What waited for a boss's turn when the hub stopped is its line loop's
/// at start: the report is taken by a turn of its boss's.
#[tokio::test(flavor = "multi_thread")]
async fn reports_waiting_when_the_hub_stopped_are_taken_by_a_turn_when_it_starts() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, s) = paced_session(5, "Noted.", pool, dir.path()).await;
        let report = kept_report(&d, s, "Step 14 done: the old DNS records are gone.").await;
        // Kept straight in the store, no event: the hub starting again, a
        // line loop of its own reads what waits (the crew's has read
        // already), stopped however the test ends.
        let cancel = tokio_util::sync::CancellationToken::new();
        let _stop = cancel.clone().drop_guard();
        tokio::spawn(super::super::line::line_loop(Arc::clone(&crew), cancel));
        let turn = eventually("the waiting report's turn", async || {
            overseer
                .sessions
                .find(&super::super::line::line_channel(s), PERSON)
                .unwrap()?
                .messages
                .into_iter()
                .find(|m| m.content.contains("the old DNS records are gone"))
        })
        .await;
        assert!(turn.content.starts_with(super::super::line::LINE));
        let taken = eventually("the report taken", async || {
            d.store()
                .line_message(report.id)
                .await
                .unwrap()
                .filter(|m| m.delivery == Delivery::Delivered)
        })
        .await;
        assert!(taken.delivered_at.is_some());
        tokio::time::sleep(super::super::line::GATHER * 2).await;
        assert_eq!(line_turns(&overseer, s), 1, "one turn took it");
    })
    .await;
}

/// What the line loop reads again when it fell behind Desk's events: the
/// questions open to a boss and the reports waiting for its turn alike.
#[tokio::test(flavor = "multi_thread")]
async fn a_line_loop_behind_desks_events_reads_open_questions_and_waiting_reports_again() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, s) = paced_session(5, "Noted.", pool, dir.path()).await;
        let report = kept_report(&d, s, "Step 15 done.").await;
        let v = d.view(s).unwrap();
        let mut ask = workforce_desk::line::LineMessage::new(
            workforce_desk::crew::Party::session(&v),
            workforce_desk::crew::Party::manager("desk", "Desk"),
            LineKind::Ask,
            "Drop the legacy queue table?".into(),
            1,
        );
        ask.asked = Some(workforce_desk::line::Asked::Open);
        d.store().put_line(&ask).await.unwrap();
        let kept = crew.line_kept().await;
        let mut ids: Vec<uuid::Uuid> = kept[&s].iter().map(|m| m.id).collect();
        ids.sort();
        let mut want = vec![report.id, ask.id];
        want.sort();
        assert_eq!(ids, want);
    })
    .await;
}

/// Reports waiting for the next hour go as soon as the person raises the
/// boss's turns for it, not when the hour frees one.
#[tokio::test(flavor = "multi_thread")]
async fn reports_waiting_go_as_soon_as_the_hours_turns_are_raised() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // An hour of a minute: well past what a raised cap waits for.
        let timing = super::super::Timing {
            minute: std::time::Duration::from_secs(1),
            ..FAST
        };
        let (overseer, crew, d, s) = paced_session_at(1, "Noted.", pool, dir.path(), timing).await;
        d.line_call(
            s,
            "tell",
            &json!({"to": "desk", "text": "Inventory done."}),
            None,
        )
        .await
        .unwrap();
        eventually("its first report taken", async || {
            (line_turns(&overseer, s) == 1).then_some(())
        })
        .await;
        let first = std::time::Instant::now();
        d.line_call(
            s,
            "tell",
            &json!({"to": "desk", "text": "Step 12 done."}),
            None,
        )
        .await
        .unwrap();
        eventually("the report waiting for the next hour", async || {
            (d.store().line_waiting_for_bosses().await.unwrap().len() == 1).then_some(())
        })
        .await;
        write_config(&overseer, &paced_config(5));
        eventually("its turn once raised", async || {
            (line_turns(&overseer, s) == 2).then_some(())
        })
        .await;
        let hour = crew.timing.minute * 60;
        assert!(
            first.elapsed() < hour,
            "before the hour freed one: {:?} of {hour:?}",
            first.elapsed()
        );
        eventually("taken by it", async || {
            d.store()
                .line_waiting_for_bosses()
                .await
                .unwrap()
                .is_empty()
                .then_some(())
        })
        .await;
        // Taken, it starts no turn again.
        tokio::time::sleep(super::super::line::GATHER * 2).await;
        assert_eq!(line_turns(&overseer, s), 2);
    })
    .await;
}

/// The boss's message went into its session twice and was never shown to
/// have: the boss is woken once to say so, naming the message.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_never_received_wakes_its_boss_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, llm) =
            overseeing_at(dir.path(), pool, "Noted.", &paced_config(50), &[], FAST).await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        d.take_on(
            s.id,
            &workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer)),
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        let by = crew.by_overseer("It should open the PR.").unwrap();
        let mut m = workforce_desk::line::LineMessage::new(
            by.party,
            workforce_desk::crew::Party::session(&s),
            LineKind::Tell,
            "Open the PR for the ledger fix.".into(),
            1,
        );
        m.delivery = Delivery::Unreceived;
        crew.unreceived(&m);
        let woken = eventually("the boss woken", async || {
            Some(super::stalls::woken_by(&llm, "never received").await).filter(|w| !w.is_empty())
        })
        .await;
        assert_eq!(woken.len(), 1, "{woken:?}");
        assert!(woken[0].contains(&m.id.to_string()), "{woken:?}");
        assert!(
            woken[0].contains("Open the PR for the ledger fix."),
            "{woken:?}"
        );
    })
    .await;
}
