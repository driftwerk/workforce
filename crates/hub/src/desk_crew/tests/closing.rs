//! The boss closes the sessions it runs once their work is done
//! (`closing`), as the person asked on 11/15: "after the Desk agent starts
//! up sessions to do work, they just hang around forever".
//!
//! - One it started whose work is done is closed at Act and removed, as on
//!   11/18 ("I'm having lots of sessions I have to go close by hand"): off
//!   the sidebar and the closed list, its conversation and folder kept,
//!   each close a decision whose Undo brings it back and reopens it.
//! - Asked to close what is finished, at Act fully it closes and removes
//!   its own (`crew.close_session`), never one with work left in its
//!   folder, and only suggests one that isn't its own.
//! - One the person handed it is only suggested at Act.
//! - One with uncommitted work, or one the person typed into just now,
//!   stays open; the first is said once.
//! - One whose close was undone is never closed again.
//! - With the setting off, nothing is closed.
//! - At Act fully a request the boss decides itself is cancelled, as its
//!   own decision, so the session closes.
//! - A session reviewing a ticket's pull request never supersedes the
//!   session whose work it reviews.

use super::act_fully::repo;
use super::*;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::charge::How;
use workforce_desk::crew::Party;
use workforce_desk::decision::{Undo, UndoPlan};
use workforce_desk::model::{SessionState, SessionView};
use workforce_desk::update::{self, ClosedKind, ClosedLine};
use workforce_desk::{AssignTo, NewBoard, NewTicket};

use super::decides_at_once::{asked, behavior};
use super::unbound::UNBOUND;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\n";

/// A session in `dir` that `boss` runs (as `how` says), whose turn ended
/// saying it shipped, judged done by its boss.
async fn finished(
    d: &Desk,
    crew: &CrewHub,
    base: &Path,
    dir: &Path,
    name: &str,
    boss: &Party,
    how: How,
) -> SessionView {
    let s = new_session(d, dir, name).await;
    let url = hook_url(base, s.id).await;
    hook(
        &url,
        json!({"hook_event_name": "SessionStart", "session_id": format!("cc-{name}")}),
    )
    .await;
    d.take_on(s.id, boss, how, &Default::default(), &By::you())
        .await
        .unwrap();
    hook(
        &url,
        json!({"hook_event_name": "Stop", "last_assistant_message": "Shipped it."}),
    )
    .await;
    let v = eventually("its turn to end", async || {
        d.view(s.id).ok().filter(|v| v.state == SessionState::Done)
    })
    .await;
    crew.said_done(&v, "the release is out");
    v
}

/// Every finished-sessions line the boss's updates in the DM have said.
async fn said(overseer: &AgentService, d: &Desk) -> Vec<ClosedLine> {
    let ids: Vec<String> = overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|s| s.messages)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.desk
                .as_ref()
                .and_then(|d| d.delivery)
                .is_some_and(|k| k == DeliveryKind::Update)
        })
        .map(|m| m.id)
        .collect();
    let extras = d.store().extras(&ids).await.unwrap();
    ids.iter()
        .filter_map(|id| extras.get(id).and_then(|e| e.update.clone()))
        .flat_map(|u| u.closed)
        .collect()
}

fn state(d: &Desk, id: uuid::Uuid) -> SessionState {
    d.view(id).map(|v| v.state).unwrap_or(SessionState::Closed)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boss_closes_the_finished_sessions_it_started_and_never_what_holds_work_or_is_the_persons(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", ACT, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let since = chrono::Utc::now();

        let plain = base.join("notes");
        std::fs::create_dir_all(&plain).unwrap();
        let clean = base.join("acme-web");
        repo(&clean, "git@github.com:acme/acme-web.git");
        let dirty = base.join("acme-api");
        repo(&dirty, "git@github.com:acme/acme-api.git");

        let notes = finished(&d, &crew, &base, &plain, "notes", &boss, How::Started).await;
        let web = finished(&d, &crew, &base, &clean, "web", &boss, How::Started).await;
        let api = finished(&d, &crew, &base, &dirty, "api", &boss, How::Started).await;
        std::fs::write(dirty.join("half-done.rs"), "fn main() {}").unwrap();
        let handed = finished(&d, &crew, &base, &plain, "handed", &boss, How::Handed).await;
        // The person prompted this one just now: theirs.
        let typed = finished(&d, &crew, &base, &plain, "typed", &boss, How::Started).await;
        let typed_url = hook_url(&base, typed.id).await;
        hook(
            &typed_url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "one more thing"}),
        )
        .await;
        hook(
            &typed_url,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Done that too."}),
        )
        .await;
        let typed = eventually("its turn to end again", async || {
            d.view(typed.id)
                .ok()
                .filter(|v| v.state == SessionState::Done && v.prompts > typed.prompts)
        })
        .await;
        crew.said_done(&typed, "it did the one more thing");

        // Rested: the boss looks.
        *crew.close_settle.lock() = chrono::Duration::zero();
        eventually("what it started and finished to close", async || {
            (state(&d, notes.id) == SessionState::Closed
                && state(&d, web.id) == SessionState::Closed)
                .then_some(())
        })
        .await;
        let suggested = eventually("the handed one suggested", async || {
            d.store()
                .suggestions(50)
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.action["input"]["session_id"] == handed.id.to_string())
        })
        .await;
        assert_eq!(suggested.action["tool"], super::super::closing::TOOL);
        assert_eq!(suggested.state, SuggestionState::Open);
        // Its updates say what it closed, would close and left open.
        let of = |lines: &[ClosedLine], kind: ClosedKind| -> Vec<(String, Option<String>)> {
            lines
                .iter()
                .filter(|l| l.kind == kind)
                .flat_map(|l| {
                    l.sessions
                        .iter()
                        .map(|s| (s.session_id.clone(), s.why.clone()))
                })
                .collect()
        };
        let lines = eventually("its updates to say all three", async || {
            let lines = said(&overseer, &d).await;
            (of(&lines, ClosedKind::Closed).len() == 2
                && !of(&lines, ClosedKind::Suggested).is_empty()
                && !of(&lines, ClosedKind::KeptOpen).is_empty())
            .then_some(lines)
        })
        .await;
        let suggested_line = of(&lines, ClosedKind::Suggested);
        assert_eq!(suggested_line.len(), 1, "{suggested_line:?}");
        assert_eq!(suggested_line[0].0, handed.id.to_string());
        assert!(
            suggested_line[0]
                .1
                .as_deref()
                .is_some_and(|w| w.contains("the release is out")),
            "{suggested_line:?}"
        );
        assert_eq!(
            of(&lines, ClosedKind::KeptOpen),
            vec![(api.id.to_string(), Some("1 uncommitted file".into()))]
        );
        // Looked at again and again: nothing more is closed, said or
        // suggested.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        for (s, why) in [
            (&api, "uncommitted work"),
            (&handed, "handed, at Act"),
            (&typed, "the person in it"),
        ] {
            assert_ne!(state(&d, s.id), SessionState::Closed, "{} ({why})", s.name);
        }
        let again = said(&overseer, &d).await;
        for kind in [
            ClosedKind::Closed,
            ClosedKind::Suggested,
            ClosedKind::KeptOpen,
        ] {
            assert_eq!(of(&again, kind), of(&lines, kind), "said once: {kind:?}");
        }
        assert!(crew.closing_notes("desk", chrono::Utc::now()).is_empty());
        let rows: Vec<_> = d
            .store()
            .decisions_since("desk", since, 50)
            .await
            .unwrap()
            .into_iter()
            .filter(update::is_close)
            .collect();
        assert_eq!(rows.len(), 2, "each closed once: {rows:?}");
        for r in &rows {
            assert_eq!(r.why, "It finished its work: the release is out.");
            assert_eq!(r.undo, Undo::Reverse, "its conversation resumes");
        }
        let line = update::closed_line(&rows).unwrap();
        // Newest first, as its decisions are.
        let (a, b) = (notes.label(), web.label());
        assert!(
            [
                format!("Closed and removed 2 finished sessions: {a}, {b}."),
                format!("Closed and removed 2 finished sessions: {b}, {a}.")
            ]
            .contains(&line.text),
            "{}",
            line.text
        );
        // Removed from Desk as the person's Remove does: off the closed
        // list, its conversation never adopted again, its folder kept.
        for (s, folder) in [(&notes, &plain), (&web, &clean)] {
            let cli = s.cli_session_id.clone().unwrap();
            eventually("it to be removed", async || {
                (d.store().get(s.id).await.unwrap().is_none()
                    && d.hidden_sessions()
                        .await
                        .unwrap()
                        .iter()
                        .any(|h| h.cli_session_id == cli))
                .then_some(())
            })
            .await;
            assert!(d.view(s.id).is_err(), "{} still shown", s.name);
            assert!(!d
                .closed_sessions()
                .await
                .unwrap()
                .iter()
                .any(|c| c.id == s.id));
            assert!(folder.is_dir(), "{}'s folder stays", s.name);
        }
        // Its history stays: Undo brings it back and reopens it.
        let undo = rows
            .iter()
            .find(
                |r| matches!(r.plan(), Some(UndoPlan::Reopen { session, .. }) if session == web.id),
            )
            .unwrap();
        crew.undo(undo.id).await.unwrap();
        eventually("web reopened", async || {
            (state(&d, web.id) != SessionState::Closed).then_some(())
        })
        .await;
        let web_cli = web.cli_session_id.clone().unwrap();
        eventually("web back in Desk", async || {
            (d.store().get(web.id).await.unwrap().is_some()
                && !d
                    .hidden_sessions()
                    .await
                    .unwrap()
                    .iter()
                    .any(|h| h.cli_session_id == web_cli))
            .then_some(())
        })
        .await;
        assert_eq!(d.view(web.id).unwrap().name, web.name);
        // Reopened, it is the person's: never closed as finished again.
        assert!(d.store().close_undone(web.id).await.unwrap());
        assert!(!d.store().close_undone(notes.id).await.unwrap());

        // Off: what finishes stays.
        write_config(
            &overseer,
            &format!("{ACT}[oversight.work]\nclose_finished = false\n"),
        );
        let left = finished(&d, &crew, &base, &plain, "left", &boss, How::Started).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(state(&d, left.id), SessionState::Done);
    })
    .await;
}

/// Act fully, its closer off: only the closes it is asked for.
const FULL: &str =
    "[oversight]\nenabled = true\nlevel = \"full\"\n[oversight.work]\nclose_finished = false\n";

#[tokio::test(flavor = "multi_thread")]
async fn asked_to_close_what_is_finished_a_boss_closes_and_removes_its_own_and_never_what_holds_work(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", FULL, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let since = chrono::Utc::now();
        let clean = base.join("acme-web");
        repo(&clean, "git@github.com:acme/acme-web.git");
        let dirty = base.join("acme-api");
        repo(&dirty, "git@github.com:acme/acme-api.git");
        let web = finished(&d, &crew, &base, &clean, "web", &boss, How::Started).await;
        let api = finished(&d, &crew, &base, &dirty, "api", &boss, How::Started).await;
        std::fs::write(dirty.join("half-done.rs"), "fn main() {}").unwrap();
        // The person's own, never taken on.
        let theirs = new_session(&d, &clean, "theirs").await;
        // Just finished: it rests before any close of the boss's.
        let r = overseer
            .tools
            .get(super::super::closing::TOOL)
            .unwrap()
            .execute(json!({ "session_id": web.id.to_string(), "reason": "Finished." }))
            .await
            .unwrap();
        assert!(!r.success, "{r:?}");
        assert!(d.view(web.id).is_ok());
        *crew.close_settle.lock() = chrono::Duration::zero();

        let close = overseer
            .tools
            .get(super::super::closing::TOOL)
            .unwrap();
        let ask = |s: &SessionView| {
            json!({ "session_id": s.id.to_string(), "reason": "The person asked to close the finished sessions." })
        };
        let r = close.execute(ask(&web)).await.unwrap();
        assert!(r.success, "{r:?}");
        let cli = web.cli_session_id.clone().unwrap();
        eventually("web removed", async || {
            (d.store().get(web.id).await.unwrap().is_none()
                && d.hidden_sessions()
                    .await
                    .unwrap()
                    .iter()
                    .any(|h| h.cli_session_id == cli))
            .then_some(())
        })
        .await;
        assert!(d.view(web.id).is_err());
        assert!(clean.is_dir());
        // Work in its folder a close would leave behind: it stays, and why.
        let r = close.execute(ask(&api)).await.unwrap();
        assert!(!r.success, "{r:?}");
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("1 uncommitted file")),
            "{r:?}"
        );
        assert_eq!(state(&d, api.id), SessionState::Done);
        // Not its own: the person's to close, with one click.
        let r = close.execute(ask(&theirs)).await.unwrap();
        assert!(r.output["suggested"].is_string(), "{r:?}");
        assert_ne!(state(&d, theirs.id), SessionState::Closed);

        // One line in its update; Undo brings it back and resumes it.
        let rows: Vec<_> = d
            .store()
            .decisions_since("desk", since, 50)
            .await
            .unwrap()
            .into_iter()
            .filter(update::is_close)
            .collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(matches!(
            rows[0].plan(),
            Some(UndoPlan::Reopen { removed: Some(_), .. })
        ));
        assert_eq!(rows[0].undo, Undo::Reverse);
        assert_eq!(
            update::closed_line(&rows).unwrap().text,
            format!("Closed and removed 1 finished session: {}.", web.label())
        );
        crew.undo(rows[0].id).await.unwrap();
        eventually("web back and reopened", async || {
            (d.view(web.id)
                .is_ok_and(|v| v.state != SessionState::Closed && v.name == web.name)
                && !d
                    .hidden_sessions()
                    .await
                    .unwrap()
                    .iter()
                    .any(|h| h.cli_session_id == cli))
            .then_some(())
        })
        .await;
        assert!(d.store().close_undone(web.id).await.unwrap());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn asked_to_close_a_boss_ends_what_it_let_go_as_done_and_never_what_it_no_longer_runs_or_the_person_is_in(
) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", FULL, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let clean = base.join("acme-web");
        repo(&clean, "git@github.com:acme/acme-web.git");
        let by_boss = By::manager("desk", &boss.name, "Its work is done.");
        let release = overseer.tools.get("crew.release").unwrap();
        let mut done = Vec::new();
        for (name, how) in [("started", How::Started), ("handed", How::Handed)] {
            let s = finished(&d, &crew, &base, &clean, name, &boss, how).await;
            let r = release
                .execute(json!({ "session_id": s.id.to_string(), "reason": "Finished.", "done": true }))
                .await
                .unwrap();
            assert!(r.success, "{r:?}");
            done.push(s);
        }
        let by_desk = finished(&d, &crew, &base, &clean, "desk", &boss, How::Started).await;
        let r = release
            .execute(json!({ "session_id": by_desk.id.to_string(), "reason": "Yours now." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        let theirs = finished(&d, &crew, &base, &clean, "theirs", &boss, How::Started).await;
        d.release(theirs.id, &By::you()).await.unwrap();
        let typing = finished(&d, &crew, &base, &clean, "typing", &boss, How::Started).await;
        let url = hook_url(&base, typing.id).await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "one more thing"}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Done that too."}),
        )
        .await;
        let typing = eventually("its turn to end again", async || {
            d.view(typing.id)
                .ok()
                .filter(|v| v.state == SessionState::Done && v.prompts > typing.prompts)
        })
        .await;
        d.release_done(typing.id, &by_boss).await.unwrap();
        *crew.close_settle.lock() = chrono::Duration::zero();
        assert!(d.charges().iter().all(|c| c.boss != "desk"), "none live");

        let close = overseer.tools.get(super::super::closing::TOOL).unwrap();
        let ask = |s: &SessionView| {
            json!({ "session_id": s.id.to_string(), "reason": "The person asked to close the finished sessions." })
        };
        let r = close.execute(ask(&by_desk)).await.unwrap();
        assert!(r.success && r.output["suggested"].is_string(), "{r:?}");
        let r = close.execute(ask(&theirs)).await.unwrap();
        assert!(!r.success, "{r:?}");
        assert!(
            r.error.as_deref().is_some_and(|e| e.contains("took theirs back")),
            "{r:?}"
        );
        for s in [&by_desk, &theirs] {
            assert_ne!(state(&d, s.id), SessionState::Closed);
        }
        let r = close.execute(ask(&typing)).await.unwrap();
        assert!(!r.success, "{r:?}");
        assert!(
            r.error.as_deref().is_some_and(|e| e.contains("in it now")),
            "{r:?}"
        );
        assert_ne!(state(&d, typing.id), SessionState::Closed);
        for s in &done {
            let r = close.execute(ask(s)).await.unwrap();
            assert!(r.success, "{r:?}");
            eventually("it is closed and removed", async || {
                d.view(s.id).is_err().then_some(())
            })
            .await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_never_closes_a_session_its_boss_let_go() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", ACT, &[]).await;
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let plain = base.join("notes");
        std::fs::create_dir_all(&plain).unwrap();
        let let_go = finished(&d, &crew, &base, &plain, "let-go", &boss, How::Started).await;
        let running = finished(&d, &crew, &base, &plain, "running", &boss, How::Started).await;
        d.release_done(let_go.id, &By::manager("desk", &boss.name, "Done."))
            .await
            .unwrap();
        *crew.close_settle.lock() = chrono::Duration::zero();
        crew.look_at_finished_sessions().await;
        eventually("the one it still runs closed", async || {
            crew.look_at_finished_sessions().await;
            (state(&d, running.id) == SessionState::Closed).then_some(())
        })
        .await;
        assert_eq!(state(&d, let_go.id), SessionState::Done);
    })
    .await;
}

/// At Act fully a boss closes a session whose request it decides itself is
/// still waiting on its model: the request is cancelled as the boss's own
/// decision, and the session is closed.
#[tokio::test(flavor = "multi_thread")]
async fn at_act_fully_a_boss_cancels_the_request_it_decides_and_closes_the_session() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let unbound = format!("{UNBOUND}[oversight.work]\nclose_finished = false\n");
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", &unbound, &[]).await;
        crew.read_spent_today(&crew.overseers()).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        // Its model takes its time over the request.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(super::super::decide::MARK))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(600)))
            .with_priority(1)
            .mount(&llm)
            .await;
        let web = base.join("acme-web");
        repo(&web, "git@github.com:acme/acme-web.git");
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        let s = finished(&d, &crew, &base, &web, "web", &boss, How::Started).await;
        let since = chrono::Utc::now();
        let waiting = asked(&hook_url(&base, s.id).await, "cargo build --release");
        let v = eventually("its request to wait on the boss", async || {
            d.view(s.id).ok().filter(|v| v.pending.is_some())
        })
        .await;

        let r = overseer
            .tools
            .get(super::super::closing::TOOL)
            .unwrap()
            .execute(json!({ "session_id": s.id.to_string(), "reason": "Its ticket is done." }))
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!(behavior(&waiting.await.unwrap()), "deny");
        eventually("it closed", async || {
            d.view(s.id)
                .map_or(true, |v| v.state == SessionState::Closed)
                .then_some(())
        })
        .await;
        let cancelled = format!("cancelled {}'s request to close it", v.label());
        let decided = d.store().decisions_since("desk", since, 50).await.unwrap();
        let x = decided
            .iter()
            .find(|x| x.what == cancelled)
            .unwrap_or_else(|| panic!("{cancelled}: {decided:?}"));
        assert_eq!(x.session, Some(s.id));
        assert_eq!(
            x.need_ref.as_deref(),
            v.pending.as_ref().map(|p| p.id.to_string()).as_deref()
        );
    })
    .await;
}

/// A session started to review a ticket's pull request carries none of
/// its work: handed the ticket, it never supersedes the session whose work
/// it reviews, while a newer session handed it does.
#[tokio::test(flavor = "multi_thread")]
async fn a_review_session_on_its_ticket_never_supersedes_the_session_it_reviews() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let work = new_session(&d, dir.path(), "web").await;
        let review = new_session(&d, dir.path(), "web-review").await;
        let newer = new_session(&d, dir.path(), "web-again").await;
        d.store()
            .pr_review_link(
                review.id,
                "desk",
                &workforce_desk::plan::PrRef::new("acme", "web", 7),
            )
            .await
            .unwrap();
        let board = d
            .create_board(NewBoard {
                name: "Acme".into(),
                group: None,
                prefix: None,
            })
            .await
            .unwrap()
            .id;
        let ticket = d
            .create_ticket(
                NewTicket {
                    board,
                    title: "The date picker".into(),
                    body: String::new(),
                    state: Some(workforce_desk::board::TicketState::Todo),
                    needs: vec![],
                },
                &By::you(),
            )
            .await
            .unwrap()
            .ticket
            .id;
        let you = By::you();
        let give = |s: uuid::Uuid| d.assign_ticket(ticket, Some(AssignTo::Session(s)), &you);
        give(work.id).await.unwrap();
        assert!(!crew.superseded(work.id).await, "its own ticket");
        give(review.id).await.unwrap();
        assert!(
            !crew.superseded(work.id).await,
            "a review carries none of its work"
        );
        give(newer.id).await.unwrap();
        assert!(crew.superseded(work.id).await, "a newer session carries it");
    })
    .await;
}
