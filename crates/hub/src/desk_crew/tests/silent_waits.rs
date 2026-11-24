//! No session waits on its boss in silence (Cufflink Migration, 19 Nov
//! 2026: its options and "both need your decision" sat six minutes with
//! nothing reaching it, until the person opened it at 10:32 and said "go
//! with 16.16 on staging"). Within the bound the person has the choice;
//! they are reminded until it is answered; and settled anywhere, it is
//! closed and said so in one line.

use workforce_desk::line::LineKind;
use workforce_desk::open_item::{ClosedBy, OpenItem, OpenState};

use super::super::actions::{Action, Target};
use super::*;

/// A minute is 100 ms: the boss's fair chance is 1 s, reminders come 1 s, 3 s, then
/// every 6 s after the last.
const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
                   updates = { enabled = false }\n";

const REPORT: &str =
    "The staging REINDEX is done, but it can't fix the drill. Both need your decision.\n\n\
                      Options:\n(a) Upgrade staging's PostgreSQL to 16.16, then REINDEX again.\n\
                      (b) Make the drill install the host's exact minor.\n(c) Accept the message.\n\
                      I recommend (a) on staging in the day.";

/// A session that ran a turn the person prompted and stopped, past the
/// time their prompt keeps it theirs.
pub(super) async fn stopped(
    d: &Desk,
    dir: &Path,
    name: &str,
) -> (workforce_desk::model::SessionView, String) {
    let s = new_session(d, dir, name).await;
    let url = hook_url(dir, s.id).await;
    hook(
        &url,
        json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
    )
    .await;
    hook(
        &url,
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "Get the staging drill green"}),
    )
    .await;
    hook(
        &url,
        json!({"hook_event_name": "Stop", "last_assistant_message": "Reported to Desk."}),
    )
    .await;
    // Its prompt keeps its questions the person's for 10 minutes (1 s).
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    (s, url)
}

async fn the_wait(d: &Desk, s: uuid::Uuid) -> OpenItem {
    eventually("the wait given to the person", async || {
        d.store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.dedupe_key.starts_with(&format!("waiting:{}:", s)))
    })
    .await
}

fn dm(overseer: &AgentService, has: &str) -> Vec<String> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|dm| {
            dm.messages
                .into_iter()
                .map(|m| m.content)
                .filter(|c| c.contains(has))
                .collect()
        })
        .unwrap_or_default()
}

async fn closed(d: &Desk, id: uuid::Uuid) -> OpenItem {
    eventually("the wait closed", async || {
        d.store()
            .open_item(id)
            .await
            .unwrap()
            .filter(|i| i.state == OpenState::Closed)
    })
    .await
}

/// Its choice laid out, the session stops; no answer reaches it: within
/// the bound the person has it, with its options and recommendation, and
/// is reminded until they answer from it, which goes to the session.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_stopped_on_its_boss_is_the_persons_and_reminded_until_answered() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let (s, _) = stopped(&d, dir.path(), "Cufflink Migration").await;
        let r = d
            .line_call(s.id, "tell", &json!({"to": "desk", "text": REPORT}), None)
            .await
            .unwrap();
        let report: uuid::Uuid = serde_json::from_value(r["id"].clone()).unwrap();

        // The watchdog: no item yet, past the bound, it raises one.
        let wait = the_wait(&d, s.id).await;
        assert_eq!(wait.line_message, Some(report));
        assert_eq!(wait.options.len(), 3, "{:?}", wait.options);
        assert!(
            wait.recommendation
                .as_deref()
                .unwrap_or_default()
                .contains("I recommend (a)"),
            "{wait:?}"
        );
        assert!(
            wait.headline()
                .starts_with("Cufflink Migration waits on a decision"),
            "{}",
            wait.headline()
        );
        assert!(wait
            .cant_decide
            .as_deref()
            .unwrap_or_default()
            .contains("yours to decide"));
        eventually("its card", async || {
            dm(
                &overseer,
                "**Needs you.** Cufflink Migration waits on a decision",
            )
            .pop()
        })
        .await;
        let carded = std::time::Instant::now();
        // Once a wait, one item: looked at again, nothing new.
        crew.look_at_items().await;
        let open: Vec<_> = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .filter(|i| i.dedupe_key.starts_with("waiting:"))
            .collect();
        assert_eq!(open.len(), 1);

        // Reminded after 10 minutes (1 s), not before, saying how long it
        // has waited.
        assert!(
            dm(&overseer, "**Still waiting on you**").is_empty(),
            "not yet"
        );
        let reminded = eventually("the first reminder", async || {
            dm(&overseer, "**Still waiting on you** (").pop()
        })
        .await;
        assert!(
            carded.elapsed() >= std::time::Duration::from_millis(800),
            "on its cadence: {:?}",
            carded.elapsed()
        );
        assert!(
            reminded.contains(" now). Cufflink Migration waits on a decision"),
            "{reminded}"
        );
        let item = d.store().open_item(wait.id).await.unwrap().unwrap();
        assert_eq!(item.said_count, 2, "said, then reminded once");
        assert_eq!(
            d.store().open_item(wait.id).await.unwrap().unwrap().state,
            OpenState::Open,
            "never parked"
        );

        // Answered from its buttons: the session has it, in reply to its
        // report, and the reminders stop.
        assert_eq!(
            crew.answer_item(wait.id, "Go with (b).").await.unwrap(),
            "Sent."
        );
        let done = closed(&d, wait.id).await;
        assert_eq!(done.closed_by, Some(ClosedBy::Person));
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let answer = thread
            .iter()
            .find(|m| m.text == "Go with (b).")
            .expect("on its line");
        assert_eq!(answer.kind, LineKind::Tell);
        assert_eq!(answer.to_session(), Some(s.id));
        assert_eq!(answer.in_reply_to, Some(report));
        // The next would have come 30 minutes (3 s) on: none comes.
        let said = dm(&overseer, "**Still waiting on you**").len();
        tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
        assert_eq!(
            dm(&overseer, "**Still waiting on you**").len(),
            said,
            "none once answered"
        );
    })
    .await;
}

/// 10:32: the person opens the session and settles it there. The item
/// closes, the reminders stop, and one short line says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_wait_settled_in_its_session_is_closed_and_said_in_one_line() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let (s, url) = stopped(&d, dir.path(), "Cufflink Migration").await;
        d.line_call(s.id, "tell", &json!({"to": "desk", "text": REPORT}), None)
            .await
            .unwrap();
        let wait = the_wait(&d, s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "go with 16.16 on staging"}),
        )
        .await;
        let done = closed(&d, wait.id).await;
        assert_eq!(done.closed_by, Some(ClosedBy::Person));
        let ack = eventually("the one line", async || {
            dm(&overseer, "You settled it in Cufflink Migration at ").pop()
        })
        .await;
        assert!(
            ack.ends_with("(“go with 16.16 on staging”); I've closed the question."),
            "{ack}"
        );
        assert!(!ack.contains("Needs you"), "a line, never a card: {ack}");
        // Past the first reminder's 10 minutes (1 s): none comes.
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        assert!(
            dm(&overseer, "**Still waiting on you**").is_empty(),
            "never reminded once settled"
        );
        assert_eq!(dm(&overseer, "You settled it").len(), 1, "said once");
    })
    .await;
}

/// A held session given to the person before it says it holds: its
/// "Understood, holding." still waits, so the question stays theirs.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_sessions_holding_reply_after_its_wait_is_the_persons_leaves_it_open() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let config = format!("{ACT}wake = {{ waiting_after_minutes = 60 }}\n");
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", &config, &[]).await;
        let (s, _) = stopped(&d, dir.path(), "Cufflink Migration").await;
        d.line_call(s.id, "tell", &json!({"to": "desk", "text": REPORT}), None)
            .await
            .unwrap();
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message:
                    "Don't change staging any further until I answer: this is the person's call."
                        .into(),
                reply_to: None,
            },
            crew.by_overseer("It needs a decision.").unwrap(),
        )
        .await
        .unwrap();
        let hold = d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .into_iter()
            .rfind(|m| m.text.contains("until I answer"))
            .unwrap();
        let _ = d.store().line_went_in(hold.id).await;
        // Desk looks before the session replies: the person has it.
        crew.look_for_silent_waits().await;
        let wait = the_wait(&d, s.id).await;
        assert!(
            wait.dedupe_key.ends_with(&hold.id.to_string()),
            "{}",
            wait.dedupe_key
        );
        d.line_call(
            s.id,
            "tell",
            &json!({"to": "desk", "text": "Understood, holding."}),
            None,
        )
        .await
        .unwrap();
        for _ in 0..3 {
            crew.look_at_items().await;
            crew.look_for_silent_waits().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let now = d.store().open_item(wait.id).await.unwrap().unwrap();
        assert_eq!(now.state, OpenState::Open, "still the person's: {now:?}");
    })
    .await;
}

/// The boss holds the session saying it is the person's: the person has it
/// at once; the boss's own decision reaching the session closes it, said in
/// a line.
#[tokio::test(flavor = "multi_thread")]
async fn a_bosss_hold_is_the_persons_at_once_and_its_decision_closes_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // A bound of an hour: only the hold makes it the person's now.
        let config = format!("{ACT}wake = {{ waiting_after_minutes = 60 }}\n");
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", &config, &[]).await;
        let (s, _) = stopped(&d, dir.path(), "Cufflink Migration").await;
        let r = d
            .line_call(s.id, "tell", &json!({"to": "desk", "text": REPORT}), None)
            .await
            .unwrap();
        let report: uuid::Uuid = serde_json::from_value(r["id"].clone()).unwrap();
        let by = || crew.by_overseer("It needs a decision.").unwrap();
        let nudge = |text: &str| Action::Nudge {
            to: Target::Session(s.id),
            message: text.into(),
            reply_to: None,
        };
        crew.perform(
            &nudge("Don't change staging any further until I answer: this is the person's call."),
            by(),
        )
        .await
        .unwrap();
        let hold = d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .into_iter()
            .rfind(|m| m.text.contains("until I answer"))
            .unwrap();
        let _ = d.store().line_went_in(hold.id).await;
        // It says it holds, in words that ask nothing: it waits all the same.
        d.line_call(
            s.id,
            "tell",
            &json!({"to": "desk", "text": "Understood, holding."}),
            None,
        )
        .await
        .unwrap();
        // At once: well within the hour's bound (6 s).
        let wait =
            tokio::time::timeout(std::time::Duration::from_millis(1_500), the_wait(&d, s.id))
                .await
                .expect("at once, not after the bound");
        assert!(
            wait.dedupe_key.ends_with(&hold.id.to_string()),
            "{}",
            wait.dedupe_key
        );
        // In its words that asked, whether Desk saw its reply first or the
        // hold: its report, its options and its recommendation.
        assert!(
            wait.text.contains("Both need your decision"),
            "its own words: {}",
            wait.text
        );
        assert_eq!(wait.line_message, Some(report));
        assert_eq!(wait.options.len(), 3, "{:?}", wait.options);
        assert!(
            wait.recommendation
                .as_deref()
                .unwrap_or_default()
                .contains("I recommend (a)"),
            "{wait:?}"
        );

        // The boss decides within its directives: it reaches the session.
        crew.picture_turn_began("line-1030");
        crew.perform(&nudge("Decision: (b), not (a) today."), by())
            .await
            .unwrap();
        let decided = d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .into_iter()
            .rfind(|m| m.text.starts_with("Decision: (b)"))
            .expect("the decision is sent, never a repeat of the hold");
        let _ = d.store().line_went_in(decided.id).await;
        let done = closed(&d, wait.id).await;
        assert_eq!(done.closed_by, Some(ClosedBy::Boss));
        let ack = eventually("the one line", async || {
            dm(&overseer, "answered Cufflink Migration at ").pop()
        })
        .await;
        assert!(ack.contains("“Decision: (b), not (a) today.”"), "{ack}");
        assert!(ack.ends_with("I've closed the question."), "{ack}");
    })
    .await;
}

/// The boss's own hold, which says nothing of the person, is the boss's to
/// answer: nothing pops up for the person within its fair chance.
#[tokio::test(flavor = "multi_thread")]
async fn a_bosss_own_hold_is_no_pop_up_for_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let config = format!("{ACT}wake = {{ waiting_after_minutes = 60 }}\n");
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", &config, &[]).await;
        let (s, _) = stopped(&d, dir.path(), "Cufflink Migration").await;
        d.line_call(s.id, "tell", &json!({"to": "desk", "text": REPORT}), None)
            .await
            .unwrap();
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: "Don't change staging any further until I answer.".into(),
                reply_to: None,
            },
            crew.by_overseer("It needs a decision.").unwrap(),
        )
        .await
        .unwrap();
        let hold = d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .into_iter()
            .rfind(|m| m.text.contains("until I answer"))
            .unwrap();
        let _ = d.store().line_went_in(hold.id).await;
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        crew.look_at_items().await;
        let open = d.store().items_not_closed(Some("desk")).await.unwrap();
        assert!(
            !open.iter().any(|i| i.dedupe_key.starts_with("waiting:")),
            "{open:?}"
        );
        assert!(dm(&overseer, "Needs you").is_empty());
    })
    .await;
}

/// While the boss's turn is working, what waits on it is never raised to
/// the person, however long past the bound; once its turn is over, it is.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_raised_while_the_bosss_turn_is_working() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let (s, _) = stopped(&d, dir.path(), "Cufflink Migration").await;
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let c = Arc::clone(&crew);
        let holder = tokio::spawn(async move {
            c.with_mind("desk", "a check-in", async move {
                go_rx.await.ok();
            })
            .await
        });
        eventually("the mind held", async || {
            crew.mind_holder("desk").map(|_| ())
        })
        .await;
        d.line_call(s.id, "tell", &json!({"to": "desk", "text": REPORT}), None)
            .await
            .unwrap();
        // Past the boss's fair chance (10 minutes, 1 s).
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
        crew.look_at_items().await;
        let open = d.store().items_not_closed(Some("desk")).await.unwrap();
        assert!(
            !open.iter().any(|i| i.dedupe_key.starts_with("waiting:")),
            "nothing while its turn works: {open:?}"
        );
        go_tx.send(()).unwrap();
        holder.await.unwrap();
        the_wait(&d, s.id).await;
    })
    .await;
}

/// What a session did, listed ("1. …", "CI: green"), waits on no one: no
/// item, however long it sits.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_of_what_was_done_waits_on_no_one() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let (s, _) = stopped(&d, dir.path(), "Cufflink Migration").await;
        let done = "Done.\n1. Updated the migration.\n2. Ran the tests.\nCI: green on all jobs.\nPR: opened #12.";
        d.line_call(s.id, "tell", &json!({"to": "desk", "text": done}), None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
        crew.look_at_items().await;
        let open = d.store().items_not_closed(Some("desk")).await.unwrap();
        assert!(
            !open.iter().any(|i| i.dedupe_key.starts_with("waiting:")),
            "{open:?}"
        );
        assert!(dm(&overseer, "waits on a decision").is_empty());
    })
    .await;
}
