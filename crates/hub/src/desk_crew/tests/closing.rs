//! The boss closes the sessions it runs once their work is done
//! (`closing`), as the person asked on 11/15: "after the Desk agent starts
//! up sessions to do work, they just hang around forever".
//!
//! - One it started whose work is done is closed at Act: off the sidebar,
//!   its conversation kept, each close a decision whose Undo reopens it.
//! - One the person handed it is only suggested at Act.
//! - One with uncommitted work, or one the person typed into just now,
//!   stays open; the first is said once.
//! - One whose close was undone is never closed again.
//! - With the setting off, nothing is closed.

use super::act_fully::repo;
use super::*;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::charge::How;
use workforce_desk::crew::Party;
use workforce_desk::decision::{Undo, UndoPlan};
use workforce_desk::model::{SessionState, SessionView};
use workforce_desk::update::{self, ClosedKind, ClosedLine};

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
                format!("Closed 2 finished sessions: {a}, {b}."),
                format!("Closed 2 finished sessions: {b}, {a}.")
            ]
            .contains(&line.text),
            "{}",
            line.text
        );
        // Its history stays: Undo reopens it.
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
