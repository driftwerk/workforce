//! Who holds which job (designs/boss-orders-and-serial-turns.md §3),
//! replayed from the night of 17 Nov, anonymised: the client is Harbor,
//! the owner Sam; run numbers, commits and session names changed; what
//! was said, its order and its shape kept.
//!
//! - **Release 4.** At 20:14 the boss's DM turn told one session "you now
//!   own release 4, and only you dispatch it"; at 20:26 a line turn told
//!   another "URGENT, one job: start Harbor production release 4". Both
//!   were working on it at once.
//! - **Release 3.** A focused call told a session to dispatch the release
//!   3 promote at a commit; its DM turn then told it again, in other
//!   words, "start Harbor production release 3 now".

use std::future::Future;
use std::time::Duration;

use workforce_desk::assignment::{short, AssignmentState};
use workforce_desk::line::{Delivery, Send, Target as LineTarget, When};
use workforce_desk::LineSender;

use super::super::actions::{Action, Target};
use super::super::assignments::Order;
use super::decides_at_once::{calls_about, decides, said, THEIRS};
use super::*;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n";

/// The boss's DM turn at 20:14, to the session that had dry-run it.
const OWN_4: &str = "Desk, 20:14. Thanks, and I've checked it: 44383453606 and converge 44388058690 are green on GitHub. You now own release 4, and only you dispatch it. GitHub shows no promote of any kind since release 3";
/// The line turn at 20:26, to another session.
const URGENT_4: &str = "URGENT, one job: start Harbor production release 4 (leads step 2) and watch it through. Sam, the owner, ordered every production release out tonight";
/// The focused call's order for release 3.
const DISPATCH_3: &str = "Dispatch the real release 3 promote now at exactly f40816536a2fa8d947706f7f7a2c3f6d951a0123, with the same workflow and inputs as dry run 44380837640 and dry run off, and send Desk the run id";
/// The DM turn's, after it.
const URGENT_3: &str = "URGENT, one job, from Desk for Sam (owner): start Harbor production release 3 now and watch it through.";

fn nudge(to: uuid::Uuid, message: &str) -> Action {
    Action::Nudge {
        to: Target::Session(to),
        message: message.into(),
        reply_to: None,
    }
}

/// `f` run inside the boss's agent turn on runner session `sid`, as a
/// tool call of that turn runs: its DM (`dm`), a session's line, or a
/// focused call.
async fn in_turn<F: Future>(sid: &str, f: F) -> F::Output {
    workforce_agent::runner::CURRENT_SESSION_ID
        .scope(sid.to_string(), f)
        .await
}

/// The boss's messages on `s`'s line that are there to go in, oldest first.
async fn told(d: &Desk, s: uuid::Uuid) -> Vec<workforce_desk::line::LineMessage> {
    d.line_thread(s, None, None)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| {
            m.from.kind == PartyKind::Manager
                && !matches!(m.delivery, Delivery::Noted | Delivery::Withdrawn)
        })
        .collect()
}

/// `s`'s session in its own folder, `folder` under `dir`: a job is keyed
/// by where it is done.
async fn session_in(
    d: &Desk,
    dir: &std::path::Path,
    folder: &str,
    name: &str,
) -> workforce_desk::model::SessionView {
    let at = dir.join(folder);
    std::fs::create_dir_all(&at).unwrap();
    new_session(d, &at, name).await
}

/// The boss's live assignment held by `s`.
async fn held_by(d: &Desk, s: uuid::Uuid) -> workforce_desk::assignment::Assignment {
    d.store()
        .assignments_of("desk", true)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.session == s)
        .expect("an assignment of its")
}

/// The line message carrying `s`'s assignment goes in, as its hook takes it.
async fn goes_in(d: &Desk, s: uuid::Uuid) {
    let line = held_by(d, s).await.line_message.expect("sent on its line");
    d.store()
        .line_take(line, Delivery::Delivered)
        .await
        .unwrap()
        .expect("still queued");
}

/// `s` writes `text` to its boss, and the boss's picture has it.
async fn reports(d: &Desk, s: uuid::Uuid, text: &str) {
    d.line_send(
        LineSender::Session(s),
        Send::Tell {
            to: LineTarget::Overseer,
            text: text.into(),
            when: When::Queue,
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// A refusal of a second holder: plain, naming who holds it, and what to
/// do instead.
fn refused_for(why: &str, holder: &str) {
    assert!(why.starts_with("Not sent: "), "{why}");
    assert!(why.contains(holder), "it names the holder: {why}");
    assert!(why.contains("replaces: \"a:"), "and how to move it: {why}");
    assert!(
        !why.contains('{') && !why.contains("Rule"),
        "plain words: {why}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn release_4_goes_to_one_session_whichever_turn_sends_it_first() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || {
            crew.by_overseer("Sam ordered every release out tonight.")
                .unwrap()
        };

        // The DM turn first, as that night; then the line turn.
        let owner = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let other = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        crew.picture_turn_began("dm");
        let sent = in_turn("dm", crew.perform(&nudge(owner.id, OWN_4), by()))
            .await
            .unwrap();
        assert!(sent.starts_with("told harbor-release-3"), "{sent}");
        let a = held_by(&d, owner.id).await;
        assert_eq!(a.job_key.as_deref(), Some("harbor/release/4"));
        assert!(a.irreversible && a.claims);
        crew.picture_turn_began("line-auth");
        let why = in_turn("line-auth", crew.perform(&nudge(other.id, URGENT_4), by()))
            .await
            .unwrap_err();
        refused_for(&why, "harbor-release-3");
        assert!(why.contains(&short(a.id)), "{why}");
        assert!(
            why.contains("You now own release 4"),
            "it quotes what the holder was told: {why}"
        );
        assert!(
            told(&d, other.id).await.is_empty(),
            "nothing reached the second session"
        );

        // The line turn first; then the DM turn, on another repository's
        // release 4: refused the same way.
        let first = session_in(&d, dir.path(), "harbor-web", "web-release").await;
        let second = session_in(&d, dir.path(), "harbor-web", "web-hotfix").await;
        crew.picture_turn_began("line-web");
        in_turn("line-web", crew.perform(&nudge(first.id, URGENT_4), by()))
            .await
            .unwrap();
        crew.picture_turn_began("dm");
        let why = in_turn("dm", crew.perform(&nudge(second.id, OWN_4), by()))
            .await
            .unwrap_err();
        refused_for(&why, "web-release");
        assert!(told(&d, second.id).await.is_empty());

        // A check-in turn with the boss's own job named, any wording:
        // the same reservation.
        let named = Order {
            job: Some("Harbor production release 4".into()),
            ..Default::default()
        };
        crew.picture_turn_began("desk:checkin:1");
        let why = in_turn(
            "desk:checkin:1",
            crew.perform_with(
                &nudge(other.id, "Pick up where it stands and watch it through."),
                by(),
                &named,
            ),
        )
        .await
        .unwrap_err();
        refused_for(&why, "harbor-release-3");

        // What isn't an order passes: a status question, an order not to,
        // and an answer.
        let status = Action::parse(
            "crew.ask_session",
            &json!({ "session_id": other.id.to_string(), "question": "Has release 4 started?" }),
        )
        .unwrap();
        crew.perform(&status, by()).await.unwrap();
        crew.perform(
            &nudge(
                other.id,
                "Do not start release 4: harbor-release-3 has it. Carry on with the auth reset.",
            ),
            by(),
        )
        .await
        .unwrap();
        assert_eq!(told(&d, other.id).await.len(), 2);
        let live = d.store().assignments_of("desk", true).await.unwrap();
        assert!(
            live.iter()
                .filter(|l| l.session == other.id)
                .all(|l| !l.claims && !l.irreversible),
            "neither claims the release: {live:?}"
        );

        // Activity says what was refused.
        let refusals = d
            .activity(workforce_desk::crew::Filter::All, None, 100)
            .await
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == workforce_desk::crew::Kind::AssignmentRefused)
            .count();
        assert_eq!(refusals, 3);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn release_3_said_again_before_it_went_in_supersedes_and_after_is_refused() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Sam ordered release 3.").unwrap();
        let s = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let b = session_in(&d, dir.path(), "harbor", "harbor-docs").await;

        // The focused call's order, still queued (its turn runs).
        crew.picture_turn_began("focus-1");
        in_turn("focus-1", crew.perform(&nudge(s.id, DISPATCH_3), by()))
            .await
            .unwrap();
        let first = held_by(&d, s.id).await;
        assert_eq!(first.job_key.as_deref(), Some("harbor/release/3"));
        // The session writes meanwhile, so the picture has moved on.
        reports(&d, s.id, "Dry run 44380837640 is green; waiting on Desk.").await;
        // The DM turn says it again, in other words, before it went in:
        // the new order takes its place, and the session is told once.
        crew.picture_turn_began("dm");
        in_turn("dm", crew.perform(&nudge(s.id, URGENT_3), by()))
            .await
            .unwrap();
        let lines: Vec<String> = told(&d, s.id).await.into_iter().map(|m| m.text).collect();
        assert_eq!(lines, vec![URGENT_3.to_string()], "the first was withdrawn");
        let was = d.store().assignment(first.id).await.unwrap().unwrap();
        assert_eq!(was.state, AssignmentState::Superseded);

        // Once it went in, the same job again to the same session is
        // refused, whichever turn says it.
        goes_in(&d, s.id).await;
        reports(&d, s.id, "Release 3 promote dispatched: run 44391178201.").await;
        crew.picture_turn_began("focus-2");
        // As that night: the same step again, in a focused call's own words.
        let again =
            "Dispatch the release 3 promote now with dry run off, and send Desk the run id.";
        let why = in_turn("focus-2", crew.perform(&nudge(s.id, again), by()))
            .await
            .unwrap_err();
        assert!(
            why.starts_with("Not sent: harbor-release-3 already has this job"),
            "{why}"
        );
        assert!(why.contains("can start it twice"), "{why}");
        assert_eq!(told(&d, s.id).await.len(), 1);
        // And to another session, by the DM turn.
        crew.picture_turn_began("dm");
        let why = in_turn("dm", crew.perform(&nudge(b.id, URGENT_3), by()))
            .await
            .unwrap_err();
        refused_for(&why, "harbor-release-3");
        assert!(
            why.contains("delivered"),
            "it says the holder has it: {why}"
        );

        // A word on it that starts nothing is a follow-up, sent.
        crew.perform(
            &nudge(s.id, "Send Desk the release 3 run id when you have it."),
            by(),
        )
        .await
        .unwrap();
        assert_eq!(told(&d, s.id).await.len(), 2);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moved_release_waits_for_its_holder_to_stand_down_then_goes_checked() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || {
            crew.by_overseer("Sam ordered every release out tonight.")
                .unwrap()
        };
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        crew.perform(&nudge(old.id, OWN_4), by()).await.unwrap();
        goes_in(&d, old.id).await;
        let was = held_by(&d, old.id).await;

        // Moved with replaces: the old holder may have started it, so the
        // new holder's order is held, and the old one told to stand down.
        let moving = Order {
            replaces: Some(short(was.id)),
            ..Default::default()
        };
        crew.picture_turn_began("line-auth");
        let said = in_turn(
            "line-auth",
            crew.perform_with(&nudge(new.id, URGENT_4), by(), &moving),
        )
        .await
        .unwrap();
        assert!(said.starts_with("held a:"), "{said}");
        assert!(said.contains("stand down"), "{said}");
        assert!(
            told(&d, new.id).await.is_empty(),
            "nothing goes to the new holder yet"
        );
        let stand_down = told(&d, old.id)
            .await
            .into_iter()
            .find(|m| m.text.starts_with("Stand down from"))
            .expect("the old holder is told to stand down");
        assert_eq!(stand_down.kind, workforce_desk::line::LineKind::Ask);
        let held = held_by(&d, new.id).await;
        assert_eq!(held.state, AssignmentState::Held);
        assert_eq!(held.stand_down, Some(stand_down.id));
        // Another order for the job meanwhile is refused: it waits.
        let third = session_in(&d, dir.path(), "harbor", "harbor-docs").await;
        let why = crew
            .perform(&nudge(third.id, URGENT_4), by())
            .await
            .unwrap_err();
        refused_for(&why, "harbor-auth-reset");

        // The old holder answers: the order goes out through perform,
        // with what it said and the guard to check first.
        d.line_answer(
            stand_down.id,
            LineSender::Session(old.id),
            "Not started: no release 4 promote dispatched from here.".into(),
        )
        .await
        .unwrap();
        let sent = eventually("the held order to go out", async || {
            told(&d, new.id).await.into_iter().next()
        })
        .await;
        assert!(sent.text.starts_with(URGENT_4), "{}", sent.text);
        assert!(
            sent.text.contains("check it hasn't already been started"),
            "{}",
            sent.text
        );
        assert!(
            sent.text.contains("Not started: no release 4 promote"),
            "{}",
            sent.text
        );
        let now = held_by(&d, new.id).await;
        assert_eq!(now.state, AssignmentState::Open);
        assert_eq!(now.line_message, Some(sent.id));
        assert!(now.answer.is_some());
        let gone = d.store().assignment(was.id).await.unwrap().unwrap();
        assert_eq!(gone.state, AssignmentState::Cancelled);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stand_down_is_answered_by_quoting_its_mark_and_unanswered_is_the_persons() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || {
            crew.by_overseer("Sam ordered every release out tonight.")
                .unwrap()
        };

        // Answered by a message quoting its mark.
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        crew.perform(&nudge(old.id, OWN_4), by()).await.unwrap();
        goes_in(&d, old.id).await;
        let moving = Order {
            replaces: Some(short(held_by(&d, old.id).await.id)),
            ..Default::default()
        };
        crew.perform_with(&nudge(new.id, URGENT_4), by(), &moving)
            .await
            .unwrap();
        let sd = held_by(&d, new.id).await.stand_down.unwrap();
        // A message after it that doesn't answer it holds it still.
        d.line_send(
            LineSender::Session(old.id),
            Send::Tell {
                to: LineTarget::Overseer,
                text: "Dry run 44380837640 is green.".into(),
                when: When::Queue,
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(held_by(&d, new.id).await.state, AssignmentState::Held);
        let mark = &sd.simple().to_string()[..8];
        d.line_send(
            LineSender::Session(old.id),
            Send::Tell {
                to: LineTarget::Overseer,
                text: format!("Ack m:{mark}: standing down, nothing dispatched."),
                when: When::Queue,
            },
        )
        .await
        .unwrap();
        eventually("the held order to go out", async || {
            (!told(&d, new.id).await.is_empty()).then_some(())
        })
        .await;

        // Unanswered past its wait: the person's, and the boss is told.
        let web_old = session_in(&d, dir.path(), "harbor-web", "web-release").await;
        let web_new = session_in(&d, dir.path(), "harbor-web", "web-hotfix").await;
        crew.perform(&nudge(web_old.id, URGENT_4), by())
            .await
            .unwrap();
        goes_in(&d, web_old.id).await;
        let moving = Order {
            replaces: Some(short(held_by(&d, web_old.id).await.id)),
            ..Default::default()
        };
        crew.perform_with(&nudge(web_new.id, OWN_4), by(), &moving)
            .await
            .unwrap();
        let held = held_by(&d, web_new.id).await;
        let item = eventually("the held order to be the person's", async || {
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.text.contains("hasn't answered its stand-down"))
        })
        .await;
        assert!(
            item.text.contains("web-release") && item.text.contains(&short(held.id)),
            "{}",
            item.text
        );
        assert_eq!(
            held_by(&d, web_new.id).await.state,
            AssignmentState::Held,
            "still held, never sent"
        );
        assert!(told(&d, web_new.id).await.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_whose_holder_has_gone_moves_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Its old session closed.").unwrap();
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        crew.perform(&nudge(old.id, OWN_4), by()).await.unwrap();
        goes_in(&d, old.id).await;
        let was = held_by(&d, old.id).await;
        d.remove(old.id, workforce_desk::Removal { transcript: false })
            .await
            .unwrap();
        // The picture block says so.
        let block = crew.assignments_block("desk").await.unwrap();
        assert!(block.contains("holder gone"), "{block}");

        let moving = Order {
            replaces: Some(short(was.id)),
            ..Default::default()
        };
        let said = crew
            .perform_with(&nudge(new.id, URGENT_4), by(), &moving)
            .await
            .unwrap();
        assert!(
            said.starts_with("told harbor-auth-reset"),
            "sent, not held: {said}"
        );
        let sent = told(&d, new.id).await;
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0]
                .text
                .contains("check it hasn't already been started"),
            "{}",
            sent[0].text
        );
        assert_eq!(held_by(&d, new.id).await.state, AssignmentState::Open);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bosss_turns_read_its_open_assignments_and_it_closes_them() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Sam ordered release 4.").unwrap();
        let s = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        crew.perform(&nudge(s.id, URGENT_4), by()).await.unwrap();
        let a = held_by(&d, s.id).await;

        let block = super::super::context::block(&overseer, DESK_CHANNEL, PERSON)
            .await
            .unwrap();
        assert!(block.contains("Your open assignments"), "{block}");
        let line = format!("{} → harbor-release-3 · release 4", short(a.id));
        assert!(block.contains(&line), "{block}");
        assert!(block.contains("can't be undone"), "{block}");

        // Its tools: the list, and closing one, which frees the job.
        let call = async |name: &str, input: Value| {
            let tool = super::super::tools::CrewTool::for_level(&crew, "desk", &Default::default(), false)
                .into_iter()
                .find(|t| workforce_agent::tools::Tool::name(t) == name)
                .unwrap();
            let r = workforce_agent::tools::Tool::execute(&tool, input).await.unwrap();
            format!("{r:?}")
        };
        let listed = call("crew.assignments", json!({})).await;
        assert!(listed.contains(&short(a.id)), "{listed}");
        let closed = call(
            "crew.assignment",
            json!({ "id": short(a.id), "state": "done", "note": "Release 4 is live in production." }),
        )
        .await;
        assert!(closed.contains("closed a:"), "{closed}");
        assert!(crew.assignments_block("desk").await.is_none());
        let other = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        crew.perform(&nudge(other.id, URGENT_4), by()).await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_on_a_release_another_session_holds_is_the_persons_to_allow() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "release=4", "ALLOW: it runs the release Sam ordered").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let holder = new_session(&d, &base, "harbor-release-3").await;
        let other = new_session(&d, &base, "harbor-auth-reset").await;
        let by = crew
            .by_overseer("Sam ordered every release out tonight.")
            .unwrap();
        crew.perform(&nudge(holder.id, URGENT_4), by).await.unwrap();

        let url = hook_url(&base, other.id).await;
        let command = "gh workflow run release.yml -f release=4 -f environment=production";
        let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
            "tool_input": {"command": command}});
        let ask = tokio::spawn(async move { hook(&url, body).await });
        eventually("the DM to say why it is the person's", async || {
            (said(&overseer, &["harbor-release-3 holds", "run it twice"]) > 0).then_some(())
        })
        .await;
        assert_eq!(
            calls_about(&llm, "release=4").await.len(),
            1,
            "the boss was asked"
        );
        let p = d
            .view(other.id)
            .unwrap()
            .pending
            .expect("the person's to answer");
        assert!(!ask.is_finished(), "nothing allowed it");
        d.decide(
            other.id,
            p.id,
            workforce_desk::model::Decision::Deny,
            Some(By::you()),
        )
        .unwrap();
        assert_eq!(
            ask.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_irreversible_orders_that_name_no_job_both_stay_on_the_line() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Its next steps.").unwrap();
        let s = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let other = session_in(&d, dir.path(), "harbor", "harbor-docs").await;
        crew.perform(&nudge(s.id, "Run the deploy."), by())
            .await
            .unwrap();
        crew.perform(&nudge(s.id, "Run the release workflow."), by())
            .await
            .unwrap();
        let lines: Vec<String> = told(&d, s.id).await.into_iter().map(|m| m.text).collect();
        assert_eq!(
            lines,
            vec![
                "Run the deploy.".to_string(),
                "Run the release workflow.".to_string()
            ],
            "nothing says they are one job: neither is withdrawn"
        );
        // Nor does either hold a job from another session.
        crew.perform(&nudge(other.id, "Run the deploy."), by())
            .await
            .unwrap();
        assert_eq!(told(&d, other.id).await.len(), 1);
    })
    .await;
}
