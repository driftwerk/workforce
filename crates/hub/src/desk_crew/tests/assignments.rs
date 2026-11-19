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
//!
//! Each order declares its job, as the boss's tools now ask
//! (`job: { kind, target, key }`), each its own way.

use std::future::Future;
use std::time::Duration;

use workforce_desk::assignment::{short, AssignmentState};
use workforce_desk::line::{Delivery, Send, Target as LineTarget, When};
use workforce_desk::LineSender;

use super::super::actions::{Action, Target};
use super::super::assignments::{Declared, Order};
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

/// Where the night's releases were.
const HARBOR: &str = "harbor/monorepo";

/// An order that declares `kind` `key` of `target`.
fn job_in(target: &str, kind: &str, key: &str) -> Order {
    Order {
        job: Declared::Job {
            kind: kind.into(),
            target: Some(target.into()),
            key: key.into(),
        },
        ..Default::default()
    }
}

/// An order that declares Harbor's release `key`.
fn release(key: &str) -> Order {
    job_in(HARBOR, "release", key)
}

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
        let sent = in_turn(
            "dm",
            crew.perform_with(&nudge(owner.id, OWN_4), by(), &release("release-4")),
        )
        .await
        .unwrap();
        assert!(sent.starts_with("told harbor-release-3"), "{sent}");
        let a = held_by(&d, owner.id).await;
        assert_eq!(a.job_key.as_deref(), Some("harbor/monorepo/release/4"));
        assert!(a.irreversible && a.claims);
        // The second declares it its own way: the same job.
        crew.picture_turn_began("line-auth");
        let why = in_turn(
            "line-auth",
            crew.perform_with(&nudge(other.id, URGENT_4), by(), &release("Release 4")),
        )
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
        let web = job_in("https://github.com/harbor/web.git", "release", "r4");
        crew.picture_turn_began("line-web");
        in_turn(
            "line-web",
            crew.perform_with(&nudge(first.id, URGENT_4), by(), &web),
        )
        .await
        .unwrap();
        crew.picture_turn_began("dm");
        let web4 = job_in("harbor/web", "release", "4");
        let why = in_turn(
            "dm",
            crew.perform_with(&nudge(second.id, OWN_4), by(), &web4),
        )
        .await
        .unwrap_err();
        refused_for(&why, "web-release");
        assert!(told(&d, second.id).await.is_empty());

        // A check-in turn with the boss's own job named, any wording:
        // the same reservation.
        let named = release("4");
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
            live.iter().all(|l| l.session != other.id),
            "neither holds a job: {live:?}"
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
        in_turn(
            "focus-1",
            crew.perform_with(&nudge(s.id, DISPATCH_3), by(), &release("release-3")),
        )
        .await
        .unwrap();
        let first = held_by(&d, s.id).await;
        assert_eq!(first.job_key.as_deref(), Some("harbor/monorepo/release/3"));
        // The session writes meanwhile, so the picture has moved on.
        reports(&d, s.id, "Dry run 44380837640 is green; waiting on Desk.").await;
        // The DM turn says it again, in other words, before it went in:
        // the new order takes its place, and the session is told once.
        crew.picture_turn_began("dm");
        in_turn(
            "dm",
            crew.perform_with(&nudge(s.id, URGENT_3), by(), &release("3")),
        )
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
        let why = in_turn(
            "focus-2",
            crew.perform_with(&nudge(s.id, again), by(), &release("r3")),
        )
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
        let why = in_turn(
            "dm",
            crew.perform_with(&nudge(b.id, URGENT_3), by(), &release("3")),
        )
        .await
        .unwrap_err();
        refused_for(&why, "harbor-release-3");
        assert!(
            why.contains("delivered"),
            "it says the holder has it: {why}"
        );

        // A word on it that starts nothing is a follow-up, sent.
        crew.perform_with(
            &nudge(s.id, "Send Desk the release 3 run id when you have it."),
            by(),
            &release("3"),
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
        crew.perform_with(&nudge(old.id, OWN_4), by(), &release("4"))
            .await
            .unwrap();
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
            .perform_with(&nudge(third.id, URGENT_4), by(), &release("4"))
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
        crew.perform_with(&nudge(old.id, OWN_4), by(), &release("4"))
            .await
            .unwrap();
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
        crew.perform_with(
            &nudge(web_old.id, URGENT_4),
            by(),
            &job_in("harbor/web", "release", "4"),
        )
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
        crew.perform_with(&nudge(old.id, OWN_4), by(), &release("4"))
            .await
            .unwrap();
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
        crew.perform_with(&nudge(s.id, URGENT_4), by(), &release("4"))
            .await
            .unwrap();
        let a = held_by(&d, s.id).await;

        let block = super::super::context::block(&overseer, DESK_CHANNEL, PERSON)
            .await
            .unwrap();
        assert!(block.contains("Your open assignments"), "{block}");
        let line = format!(
            "{} → harbor-release-3 · release 4 of harbor/monorepo",
            short(a.id)
        );
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
        crew.perform_with(&nudge(other.id, URGENT_4), by(), &release("4"))
            .await
            .unwrap();
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
        crew.perform_with(&nudge(holder.id, URGENT_4), by, &release("4"))
            .await
            .unwrap();

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
async fn an_irreversible_order_that_declares_no_job_is_refused_with_an_example() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Its next steps.").unwrap();
        let s = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let refused_without = |why: &str| {
            assert!(
                why.starts_with(
                    "Not sent: this looks like it starts something that can't be taken back"
                ),
                "{why}"
            );
            assert!(
                why.contains(r#""key": "release-4""#),
                "with an example: {why}"
            );
        };
        // By its words, by the boss's flag, and with job: "none".
        let why = crew
            .perform(&nudge(s.id, "Run the deploy."), by())
            .await
            .unwrap_err();
        refused_without(&why);
        let flagged = Order {
            irreversible: true,
            ..Default::default()
        };
        let why = crew
            .perform_with(
                &nudge(s.id, "Carry on with what we agreed."),
                by(),
                &flagged,
            )
            .await
            .unwrap_err();
        refused_without(&why);
        let none = Order {
            job: Declared::NoJob,
            ..Default::default()
        };
        let why = crew
            .perform_with(&nudge(s.id, DISPATCH_3), by(), &none)
            .await
            .unwrap_err();
        refused_without(&why);
        // A job said as words, not as a job is: what it should be.
        let worded = Order::of(&json!({ "job": "release 3" }));
        let why = crew
            .perform_with(&nudge(s.id, DISPATCH_3), by(), &worded)
            .await
            .unwrap_err();
        assert!(why.contains(r#""kind": "release""#), "{why}");
        assert!(told(&d, s.id).await.is_empty(), "nothing went");
        // With its job, it goes.
        crew.perform_with(
            &nudge(s.id, "Run the deploy."),
            by(),
            &job_in(HARBOR, "deploy", "prod-2026-11-17"),
        )
        .await
        .unwrap();
        assert_eq!(told(&d, s.id).await.len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mention_claims_nothing_and_the_backstop_only_asks_to_confirm() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Its next steps.").unwrap();
        let owner = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let docs = session_in(&d, dir.path(), "harbor", "harbor-docs").await;
        let web = session_in(&d, dir.path(), "harbor", "harbor-web").await;
        // Mentions to two sessions: neither holds anything, both go.
        for s in [docs.id, web.id] {
            crew.perform(&nudge(s, "Review #711 and write the release notes for release 4."), by())
                .await
                .unwrap();
        }
        assert!(d.store().assignments_of("desk", true).await.unwrap().is_empty());

        // Once release 4 is declared by one, words naming it to another
        // ask the boss to say which job it is: not sent, nothing claimed.
        crew.perform_with(&nudge(owner.id, OWN_4), by(), &release("4"))
            .await
            .unwrap();
        let why = crew
            .perform(&nudge(docs.id, "Pick up release 4 and watch it through."), by())
            .await
            .unwrap_err();
        assert!(
            why.starts_with("Not sent: this looks like release 4 of harbor/monorepo, which harbor-release-3 holds"),
            "{why}"
        );
        assert!(why.contains(r#"job: "none""#), "{why}");
        let live = d.store().assignments_of("desk", true).await.unwrap();
        assert_eq!(live.len(), 1, "it claimed nothing: {live:?}");
        // job: "none" sends it, holding nothing; a no never asks.
        let none = Order {
            job: Declared::NoJob,
            ..Default::default()
        };
        crew.perform_with(&nudge(docs.id, "Watch release 4's notes go out."), by(), &none)
            .await
            .unwrap();
        crew.perform(&nudge(web.id, "Don't start release 4: harbor-release-3 has it."), by())
            .await
            .unwrap();
        assert_eq!(d.store().assignments_of("desk", true).await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_moved_before_its_order_went_in_is_withdrawn_and_goes_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let by = || crew.by_overseer("Sam moved release 4.").unwrap();
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        crew.perform_with(&nudge(old.id, OWN_4), by(), &release("4"))
            .await
            .unwrap();
        let was = held_by(&d, old.id).await;
        // Still queued: moved, its message never goes in, and the new
        // holder is sent it now (nothing to stand down from).
        let moving = Order {
            replaces: Some(short(was.id)),
            ..Default::default()
        };
        let said = crew
            .perform_with(&nudge(new.id, URGENT_4), by(), &moving)
            .await
            .unwrap();
        assert!(said.starts_with("told harbor-auth-reset"), "{said}");
        assert!(
            told(&d, old.id).await.is_empty(),
            "withdrawn, and no stand-down"
        );
        let now = held_by(&d, new.id).await;
        assert_eq!(now.state, AssignmentState::Open);
        assert_eq!(now.job_key, was.job_key, "the old one's job, kept");
    })
    .await;
}

/// The stand-down `new`'s held order waits on.
async fn moved_and_held(
    crew: &Arc<CrewHub>,
    d: &Desk,
    old: uuid::Uuid,
    new: uuid::Uuid,
) -> workforce_desk::assignment::Assignment {
    let by = || crew.by_overseer("Sam moved release 4.").unwrap();
    crew.perform_with(&nudge(old, OWN_4), by(), &release("4"))
        .await
        .unwrap();
    goes_in(d, old).await;
    let moving = Order {
        replaces: Some(short(held_by(d, old).await.id)),
        ..Default::default()
    };
    let said = crew
        .perform_with(&nudge(new, URGENT_4), by(), &moving)
        .await
        .unwrap();
    assert!(said.starts_with("held a:"), "{said}");
    held_by(d, new).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_job_is_let_go_by_its_boss_or_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        let held = moved_and_held(&crew, &d, old.id, new.id).await;
        // The boss, once it knows its old holder didn't start it.
        let tool = super::super::tools::CrewTool::for_level(&crew, "desk", &Default::default(), false)
            .into_iter()
            .find(|t| workforce_agent::tools::Tool::name(t) == "crew.assignment")
            .unwrap();
        let r = workforce_agent::tools::Tool::execute(
            &tool,
            json!({ "id": short(held.id), "state": "release", "note": "Its account shows no promote of release 4." }),
        )
        .await
        .unwrap();
        assert!(format!("{r:?}").contains("let a:"), "{r:?}");
        let sent = eventually("the let-go order to go out", async || {
            told(&d, new.id).await.into_iter().next()
        })
        .await;
        assert!(sent.text.contains("Its account shows no promote"), "{}", sent.text);

        // The person, as their release route does.
        let old2 = session_in(&d, dir.path(), "harbor", "harbor-web").await;
        let new2 = session_in(&d, dir.path(), "harbor", "harbor-docs").await;
        let mut job = release("5");
        crew.perform_with(&nudge(old2.id, "Start release 5 now."), crew.by_overseer("Release 5.").unwrap(), &job)
            .await
            .unwrap();
        goes_in(&d, old2.id).await;
        job.replaces = Some(short(held_by(&d, old2.id).await.id));
        crew.perform_with(&nudge(new2.id, "Start release 5 now."), crew.by_overseer("Moved.").unwrap(), &job)
            .await
            .unwrap();
        let held = held_by(&d, new2.id).await;
        assert!(d.store().assignment_let_go(held.id, "I checked: nothing started.").await.unwrap());
        eventually("the person's let-go to go out", async || {
            (!told(&d, new2.id).await.is_empty()).then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stand_down_answer_missed_on_the_bus_is_read_from_the_line() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        let other = session_in(&d, dir.path(), "harbor", "harbor-docs").await;
        let held = moved_and_held(&crew, &d, old.id, new.id).await;
        let sd = held.stand_down.unwrap();
        let answer = |from: &workforce_desk::model::SessionView, text: &str| {
            let mut m = workforce_desk::line::LineMessage::new(
                workforce_desk::crew::Party::session(from),
                workforce_desk::crew::Party::manager("desk", "Desk"),
                workforce_desk::line::LineKind::Answer,
                text.into(),
                1,
            );
            m.in_reply_to = Some(sd);
            m
        };
        // Written straight to the line, never on the bus: from another
        // session first, which never counts.
        d.store()
            .put_line(&answer(&other, "Not started."))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(held_by(&d, new.id).await.state, AssignmentState::Held);
        d.store()
            .put_line(&answer(&old, "Not started: nothing dispatched."))
            .await
            .unwrap();
        let sent = eventually("the held order to go out", async || {
            told(&d, new.id).await.into_iter().next()
        })
        .await;
        assert!(
            sent.text.contains("Not started: nothing dispatched."),
            "{}",
            sent.text
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_dropped_send_left_reserved_is_settled_within_minutes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let s = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let t = session_in(&d, dir.path(), "harbor", "harbor-docs").await;
        let u = session_in(&d, dir.path(), "harbor", "harbor-web").await;
        let reserve = async |session: uuid::Uuid, text: &str, key: &str, tool: Option<&str>| {
            let mut a = workforce_desk::assignment::Assignment::new("desk", session, text);
            let job = workforce_desk::assignment::Job::new("release", HARBOR, key).unwrap();
            a.job_key = Some(job.as_key());
            a.job = Some(job.said());
            a.claims = true;
            a.action = tool.map(|t| json!({ "tool": t, "input": {} }));
            d.store().assignment_reserve(&a, None).await.unwrap();
            a.id
        };
        // Its send dropped before it was kept: one never sent, one whose
        // message went, and a ticket, which carries none.
        let unsent = reserve(s.id, "Start release 6.", "6", None).await;
        let went = reserve(t.id, "Start release 7.", "7", None).await;
        d.line_send(
            LineSender::Overseer(crew.by_overseer("Release 7.").unwrap()),
            Send::Ask {
                to: LineTarget::Session(t.id),
                text: "Start release 7.".into(),
                wait: Duration::ZERO,
                chose: None,
                parts: Default::default(),
            },
        )
        .await
        .unwrap();
        let ticket = reserve(
            u.id,
            "Ticket DW-1 is yours to work on.",
            "8",
            Some("crew.assign_ticket"),
        )
        .await;
        let state = async |id| d.store().assignment(id).await.unwrap().unwrap();
        eventually("the dropped sends to be settled", async || {
            (state(unsent).await.state == AssignmentState::Failed).then_some(())
        })
        .await;
        let w = state(went).await;
        assert_eq!(w.state, AssignmentState::Open);
        assert!(w.line_message.is_some());
        assert_eq!(
            state(ticket).await.state,
            AssignmentState::Open,
            "never dropped for not being seen"
        );
        // Its send ending late still keeps what happened.
        let late = d.store().assignment_sent(unsent, None, true).await.unwrap();
        assert_eq!(late.map(|a| a.state), Some(AssignmentState::Open));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stand_down_while_the_person_drives_is_theirs_and_goes_once_they_close_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let old = session_in(&d, dir.path(), "harbor", "harbor-release-3").await;
        let new = session_in(&d, dir.path(), "harbor", "harbor-auth-reset").await;
        let url = hook_url(dir.path(), old.id).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": "cc-harbor-release-3"}),
        )
        .await;
        let boss = workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(
            old.id,
            &boss,
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        let by = || crew.by_overseer("Sam moved release 4.").unwrap();
        crew.perform_with(&nudge(old.id, OWN_4), by(), &release("4"))
            .await
            .unwrap();
        // A hooked session may have taken it already: either way it went in.
        let line = held_by(&d, old.id).await.line_message.unwrap();
        let _ = d.store().line_take(line, Delivery::Delivered).await;
        eventually("its order to be in", async || {
            held_by(&d, old.id).await.delivered_at.map(|_| ())
        })
        .await;
        // The person types into it: they drive it.
        hook(&url, json!({"hook_event_name": "UserPromptSubmit", "prompt": "I'll check the promote myself."})).await;
        let moving = Order {
            replaces: Some(short(held_by(&d, old.id).await.id)),
            ..Default::default()
        };
        let said = crew
            .perform_with(&nudge(new.id, URGENT_4), by(), &moving)
            .await
            .unwrap();
        assert!(said.starts_with("held a:") && said.contains("the person is driving it"), "{said}");
        assert!(
            !told(&d, old.id).await.iter().any(|m| m.text.starts_with("Stand down")),
            "nothing went into the session they drive"
        );
        let held = held_by(&d, new.id).await;
        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.text.contains("should stand down"))
            .expect("the person's to tell it");
        assert!(item.text.contains(&short(held.id)), "it names the held order: {}", item.text);
        d.store()
            .item_close(item.id, workforce_desk::open_item::ClosedBy::Person, "told it")
            .await
            .unwrap();
        eventually("the held order to go out", async || {
            (!told(&d, new.id).await.is_empty()).then_some(())
        })
        .await;
    })
    .await;
}
