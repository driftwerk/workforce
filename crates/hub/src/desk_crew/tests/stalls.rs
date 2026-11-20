//! No update says work is stuck with nobody on it (19 Nov, 14:51: "The
//! Workforce track 2 PRs (VEL-22) haven't started, because no answers to
//! the plan's open questions are recorded", when the person had answered
//! all eight at 11:57 and the boss held their answers as an order). Every
//! stalled item says who moves it; what the boss holds goes to the session
//! and is never said to be missing; what only the person can settle is an
//! item with its answer, reminded until it is answered.

use super::super::actions::{Action, Target};
use super::super::updates::UpdateWriter;
use super::*;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::account::{Account, Done};
use workforce_desk::crew::PartyKind;
use workforce_desk::open_item::{NewItem, OpenItem, OpenKind, OpenState};
use workforce_desk::update::MentionedUpdate;
use workforce_desk::update_write::{self as write, Mover, Said};

/// A minute is 100 ms; a waiting item is reminded after 1 s.
const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
                   wake = { waiting_remind_minutes = [10, 10, 10] }\n";

/// The writer of 19 Nov: says, of every session, that no answers are
/// recorded and nothing has changed.
struct NoAnswers;

#[async_trait::async_trait]
impl UpdateWriter for NoAnswers {
    async fn write(&self, _: &str, _: &str, _: &str, user: &str) -> eyre::Result<String> {
        let sessions: Vec<serde_json::Value> = user
            .lines()
            .filter_map(|l| l.strip_prefix("Session "))
            .filter_map(|l| l.split_whitespace().next())
            .map(|id| {
                json!({
                    "id": id,
                    "headline": "The Workforce track 2 PRs haven't started, because no answers to the plan's open questions are recorded.",
                    "text": "Nothing on track 2 has changed. I noted the block on the ticket.",
                })
            })
            .collect();
        Ok(json!({ "sessions": sessions }).to_string())
    }
}

/// A writer that is down: written plainly.
struct Down;

#[async_trait::async_trait]
impl UpdateWriter for Down {
    async fn write(&self, _: &str, _: &str, _: &str, _: &str) -> eyre::Result<String> {
        eyre::bail!("the model is down")
    }
}

/// Keeps `s`'s account: `done` just now, waiting on `waiting_on`.
async fn account(d: &Desk, s: uuid::Uuid, purpose: &str, done: &[&str], waiting_on: Option<&str>) {
    let mut a = Account::new(s, "", chrono::Utc::now());
    a.purpose = purpose.into();
    for text in done {
        a.done.push(Done {
            text: (*text).into(),
            refs: vec![],
            at: chrono::Utc::now(),
            restated: false,
        });
    }
    a.waiting_on = waiting_on.map(str::to_string);
    d.store().session_account_put(&a).await.unwrap();
}

/// The newest update posted, and its items.
async fn update(overseer: &AgentService, d: &Desk, nth: usize) -> MentionedUpdate {
    let posted = eventually("the update", async || {
        overseer
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .map(|s| s.messages)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| {
                m.desk
                    .as_ref()
                    .and_then(|x| x.delivery)
                    .is_some_and(|k| k == DeliveryKind::Update)
            })
            .nth(nth)
    })
    .await;
    eventually("its extras", async || {
        d.store()
            .extras(std::slice::from_ref(&posted.id))
            .await
            .unwrap()
            .remove(&posted.id)
            .and_then(|e| e.update)
    })
    .await
}

/// The item of `u` about session `s`.
fn item_of(u: &MentionedUpdate, s: uuid::Uuid) -> Said {
    u.said
        .iter()
        .find(|i| {
            i.session
                .as_ref()
                .is_some_and(|l| l.session_id == s.to_string())
        })
        .cloned()
        .unwrap_or_else(|| panic!("no item about {s}: {:?}", u.said))
}

/// The text of the `nth` update posted: what the chat, a notification and
/// a check-in carry.
fn posted_text(overseer: &AgentService, nth: usize) -> String {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|s| s.messages)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.desk
                .as_ref()
                .and_then(|x| x.delivery)
                .is_some_and(|k| k == DeliveryKind::Update)
        })
        .nth(nth)
        .map(|m| m.content)
        .unwrap_or_default()
}

/// What the boss sent session `s` on its line.
async fn sent(d: &Desk, s: uuid::Uuid) -> Vec<String> {
    d.line_thread(s, None, Some(100))
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.from.kind == PartyKind::Manager && m.to_session() == Some(s))
        .map(|m| m.text)
        .collect()
}

/// The person answered in the DM, and the boss kept it as an order naming
/// the ticket, with no session linked. The session then says no answers
/// are recorded, and the writer says so too: the update instead says the
/// boss has the answers and is passing them on, and the session is sent
/// them, once, however often the update is written again.
#[tokio::test(flavor = "multi_thread")]
async fn held_answers_are_passed_on_and_never_said_to_be_missing() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.set_update_writer(Arc::new(NoAnswers));
        crew.look_at_updates().await;
        let answers = "VEL-22's eight section 7 design questions (cutover window 60-90 min, bge-small-en-v1.5 at 384 dims): Kingsley, 19 Nov 11:57: \"ok yes to all to approve the design\".";
        d.store()
            .item_open(&NewItem::order("desk", answers, vec![], None))
            .await
            .unwrap();
        let s = new_session(&d, dir.path(), "VEL-22").await;
        account(
            &d,
            s.id,
            "Carry out the track 2 Workforce PRs, once the owner has answered the plan's section 7 questions",
            &[
                "Read the handoff and the plan; found no recorded answers to the section 7 questions.",
                "Commented on VEL-22 that it is blocked on the owner's answers.",
            ],
            None,
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        let item = item_of(&u, s.id);
        assert!(
            !write::says_missing(&item.headline) && !write::says_missing(&item.body),
            "never that what the boss holds is missing: {item:?}"
        );
        assert!(item.headline.starts_with("I have your answers for"), "{item:?}");
        assert!(
            matches!(&item.moves, Some(Mover::Boss { doing }) if doing.contains("passing them")),
            "{item:?}"
        );
        assert_eq!(write::breaches(&item), vec![], "{item:?}");
        let text = posted_text(&overseer, 0);
        assert!(!write::says_missing(&text), "the text sent says so too: {text}");
        assert!(text.contains("I have your answers for"), "{text}");
        let to_it = eventually("the answers sent to the session", async || {
            let s = sent(&d, s.id).await;
            (!s.is_empty()).then_some(s)
        })
        .await;
        assert_eq!(to_it.len(), 1, "{to_it:?}");
        assert!(to_it[0].contains("ok yes to all to approve the design"), "{to_it:?}");

        // Said again: not sent again.
        account(
            &d,
            s.id,
            "Carry out the track 2 Workforce PRs, once the owner has answered the plan's section 7 questions",
            &["Still no recorded answers to the section 7 questions; blocked."],
            None,
        )
        .await;
        // What the next update says of it (what the boss sent, within the
        // cadence) never says it is missing.
        let next = update(&overseer, &d, 1).await;
        for i in next.said.iter().filter(|i| {
            i.session
                .as_ref()
                .is_some_and(|l| l.session_id == s.id.to_string())
        }) {
            assert!(
                !write::says_missing(&i.headline) && !write::says_missing(&i.body),
                "{i:?}"
            );
            assert_eq!(write::breaches(i), vec![], "{i:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(sent(&d, s.id).await.len(), 1, "given once");
    })
    .await;
}

/// A blocked item with nobody on it, nothing held and nothing it waits on,
/// is never said so: the boss's full turn is handed it, and the item says
/// that.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_item_always_says_who_moves_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.set_update_writer(Arc::new(Down));
        crew.look_at_updates().await;
        let s = new_session(&d, dir.path(), "legacy-removal").await;
        account(
            &d,
            s.id,
            "Remove the legacy services from staging",
            &["Legacy removal is blocked until the partner API docs move to Cufflink."],
            None,
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        for i in &u.said {
            assert!(!write::unowned(i, &[]), "an item with nobody on it: {i:?}");
        }
        let item = item_of(&u, s.id);
        assert!(item.moves.is_some(), "{item:?}");
        assert!(!item.body.is_empty());
    })
    .await;
}

/// What only the person can settle is theirs as an item: "Yours: …" in the
/// update, the item's card answering straight to the session, reminded
/// until it is; their answer reaches the session and closes it in one line.
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_only_the_person_can_settle_is_their_item_until_answered() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.set_update_writer(Arc::new(Down));
        crew.look_at_updates().await;
        let s = new_session(&d, dir.path(), "cutover").await;
        account(
            &d,
            s.id,
            "Cut the search index over to the new store",
            &["Blocked: the cutover window needs the owner's decision."],
            Some("your decision on the cutover window"),
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        let item = item_of(&u, s.id);
        let Some(Mover::Person { question, item: id }) = item.moves.clone() else {
            panic!("the person's: {item:?}");
        };
        assert!(item.body.starts_with("Yours:"), "{item:?}");
        assert!(question.contains("cutover window"), "{question}");
        let open: OpenItem = d.store().open_item(id).await.unwrap().expect("its item");
        assert!(open.dedupe_key.starts_with("waiting:"), "{open:?}");
        assert_eq!(open.session, Some(s.id));
        // Reminded until it is answered.
        let reminded = eventually("a reminder", async || {
            let i = d.store().open_item(id).await.unwrap()?;
            (i.said_count > 1).then_some(i)
        })
        .await;
        assert_eq!(reminded.state, OpenState::Open);
        // Answered from its card: to the session, and closed in one line.
        crew.answer_wait(&open, "Take the 60 minute window tonight")
            .await
            .unwrap();
        let closed = d.store().open_item(id).await.unwrap().unwrap();
        assert_eq!(closed.state, OpenState::Closed);
        assert_eq!(
            closed.closed_why.as_deref(),
            Some("You answered: Take the 60 minute window tonight")
        );
        let thread = d.line_thread(s.id, None, Some(50)).await.unwrap();
        assert!(
            thread
                .iter()
                .any(|m| m.text.contains("Take the 60 minute window tonight")),
            "{thread:?}"
        );
    })
    .await;
}

/// What waits outside the crew is looked at again, and when is said: a
/// promise kept, which the minute's look closes once due.
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_on_something_outside_is_checked_again_when_said() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.set_update_writer(Arc::new(Down));
        crew.look_at_updates().await;
        let s = new_session(&d, dir.path(), "release").await;
        account(
            &d,
            s.id,
            "Ship the v0.39.0 release",
            &["The release is blocked on the wasmtime audit exception."],
            Some("CI on the wasmtime pull request"),
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        let item = item_of(&u, s.id);
        let Some(Mover::Outside { check_at, .. }) = item.moves.clone() else {
            panic!("outside: {item:?}");
        };
        assert!(item.body.contains("I'll check again at"), "{item:?}");
        let recheck = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.dedupe_key.starts_with("recheck:"))
            .expect("the re-check kept");
        assert_eq!(recheck.kind, OpenKind::Promise);
        assert_eq!(
            super::super::stalls::recheck_of(&recheck.dedupe_key),
            Some((s.id, check_at))
        );
        // Due, it is looked at and closed.
        eventually("the re-check looked at", async || {
            crew.look_at_items().await;
            let i = d.store().open_item(recheck.id).await.unwrap()?;
            (i.state == OpenState::Closed).then_some(())
        })
        .await;
    })
    .await;
}

/// What a session says it waits on outside the crew stays outside, however
/// it is worded ("awaiting the reviewer's approval"): looked at again, never
/// a card for the person.
#[tokio::test(flavor = "multi_thread")]
async fn an_outside_wait_in_words_of_approval_is_still_outside() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.set_update_writer(Arc::new(Down));
        crew.look_at_updates().await;
        // An answer the boss holds for it is no answer to what it awaits.
        d.store()
            .item_open(&NewItem::order(
                "desk",
                "REL-7: the audit plan is approved.",
                vec![],
                None,
            ))
            .await
            .unwrap();
        let s = new_session(&d, dir.path(), "REL-7").await;
        account(
            &d,
            s.id,
            "Ship the v0.39.0 release",
            &["Blocked: awaiting the reviewer's approval on the audit PR."],
            Some("the reviewer's approval on the audit PR"),
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        let item = item_of(&u, s.id);
        assert!(
            matches!(item.moves, Some(Mover::Outside { .. })),
            "{item:?}"
        );
        let open = d.store().items_not_closed(Some("desk")).await.unwrap();
        assert!(
            !open.iter().any(|i| i.dedupe_key.starts_with("waiting:")),
            "no card for the person: {open:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            sent(&d, s.id).await.is_empty(),
            "nothing passed off as its answer"
        );
    })
    .await;
}

/// A boss not let to message its sessions (`act.nudge` off) never sends
/// what it holds itself: the item is the person's, with their own words to
/// send, and nothing reaches the session from the boss.
#[tokio::test(flavor = "multi_thread")]
async fn a_boss_that_may_not_message_leaves_the_held_answers_to_the_person() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let no_nudge = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = false }\n\
                        wake = { waiting_remind_minutes = [10, 10, 10] }\n";
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", no_nudge, &[]).await;
        crew.set_update_writer(Arc::new(NoAnswers));
        crew.look_at_updates().await;
        let answers = "VEL-22's section 7 design questions: Kingsley, 19 Nov 11:57: \"ok yes to all to approve the design\".";
        d.store()
            .item_open(&NewItem::order("desk", answers, vec![], None))
            .await
            .unwrap();
        let s = new_session(&d, dir.path(), "VEL-22").await;
        account(
            &d,
            s.id,
            "Carry out the track 2 Workforce PRs",
            &["Found no recorded answers to the section 7 questions; blocked."],
            None,
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        let item = item_of(&u, s.id);
        assert!(
            matches!(&item.moves, Some(Mover::Person { question, .. }) if question.contains("approve the design")),
            "{item:?}"
        );
        assert!(!write::says_missing(&item.headline), "{item:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(sent(&d, s.id).await.is_empty(), "the boss sent nothing");
    })
    .await;
}

/// What the boss holds for a stopped session waits on its line, and a boss
/// not let to start sessions again leaves that to the person: theirs, as
/// an item saying it has stopped, never that the answers reach it by
/// themselves.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_session_the_boss_may_not_start_is_the_persons_to_resume() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let no_resume = "[oversight]\nenabled = true\nlevel = \"act\"\n\
                         act = { nudge = true, resume_restart = false }\n\
                         wake = { waiting_remind_minutes = [10, 10, 10] }\n";
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", no_resume, &[]).await;
        crew.set_update_writer(Arc::new(NoAnswers));
        crew.look_at_updates().await;
        let answers = "VEL-22's section 7 design questions: Kingsley, 19 Nov 11:57: \"ok yes to all to approve the design\".";
        d.store()
            .item_open(&NewItem::order("desk", answers, vec![], None))
            .await
            .unwrap();
        let s = new_session(&d, dir.path(), "VEL-22").await;
        d.stop(s.id, None).unwrap();
        eventually("it to stop", async || {
            (!d.view(s.id).unwrap().live).then_some(())
        })
        .await;
        account(
            &d,
            s.id,
            "Carry out the track 2 Workforce PRs",
            &["Found no recorded answers to the section 7 questions; blocked."],
            None,
        )
        .await;
        let u = update(&overseer, &d, 0).await;
        let item = item_of(&u, s.id);
        assert!(
            matches!(&item.moves, Some(Mover::Person { question, .. }) if question.contains("has stopped")),
            "{item:?}"
        );
        assert!(item.body.starts_with("Yours:"), "{item:?}");
        assert!(!write::says_missing(&item.headline), "{item:?}");
        assert!(!d.view(s.id).unwrap().live, "not started again by the boss");
    })
    .await;
}

/// A stall's card the person set aside is left with them: not asked again
/// until the session says more. One they answered, wherever ("They
/// answered: …" from their boss too), is settled, never read as set aside.
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_set_aside_is_left_and_one_answered_anywhere_is_settled() {
    use super::super::waiting::Stall;
    use workforce_desk::open_item::ClosedBy;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "cutover").await;
        let since = chrono::Utc::now();
        let ask = "The cutover waits on your decision on its window";
        let Stall::Item(i) = crew.give_stall_to_person("desk", &s, ask, "", since).await else {
            panic!("its item");
        };
        d.store()
            .item_close(i.id, ClosedBy::Person, "You dropped it.")
            .await
            .unwrap();
        assert!(matches!(
            crew.give_stall_to_person("desk", &s, ask, "", since).await,
            Stall::SetAside(_)
        ));
        // Said more since: theirs again, and answered through their boss.
        let since = chrono::Utc::now();
        let Stall::Item(i) = crew.give_stall_to_person("desk", &s, ask, "", since).await else {
            panic!("asked again");
        };
        d.store()
            .item_close(
                i.id,
                ClosedBy::Person,
                "They answered: take the 60 minute window",
            )
            .await
            .unwrap();
        assert!(matches!(
            crew.give_stall_to_person("desk", &s, ask, "", since).await,
            Stall::Settled(_)
        ));
    })
    .await;
}

/// A session the boss's message never reached, showing no progress
/// meanwhile (20 Nov: Cufflink Migration held "Go ahead… run step 12 now"
/// from 02:46 until 06:44), wakes its boss once, past `stall_minutes`, with
/// why it waits; one that keeps working on its own is left to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_never_reaches_a_session_wakes_its_boss_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, llm) = overseeing(
            dir.path(),
            pool,
            "I'll read its history.",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
             updates = { enabled = false }\nwork = { stall_minutes = 5 }\n",
            &[],
        )
        .await;
        let working = |name: &'static str, prompt: &'static str| {
            let d = d.clone();
            let dir = dir.path().to_path_buf();
            async move {
                let s = new_session(&d, &dir, name).await;
                let url = hook_url(&dir, s.id).await;
                hook(
                    &url,
                    json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
                )
                .await;
                hook(
                    &url,
                    json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt}),
                )
                .await;
                (s, url)
            }
        };
        let (stuck, _) = working("Cufflink Migration", "Wait for the step 12 timer.").await;
        let (busy, busy_url) = working("Legacy removal", "Remove the legacy services.").await;
        let (steered, steered_url) = working("Ledger audit", "Audit the ledger.").await;
        hook(
            &steered_url,
            json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "./audit --all"}}),
        )
        .await;
        let tell = async |s: uuid::Uuid, message: &str| {
            crew.perform(
                &Action::Nudge {
                    to: Target::Session(s),
                    message: message.into(),
                    reply_to: None,
                },
                crew.by_overseer("Its timer went off.").unwrap(),
            )
            .await
        };
        let since = chrono::Utc::now();
        assert!(tell(stuck.id, "Go ahead: run step 12 now.")
            .await
            .unwrap()
            .starts_with("told Cufflink Migration"));
        tell(busy.id, "Then stop the old workers.").await.unwrap();
        tell(steered.id, "Check the ledger totals.").await.unwrap();
        let row = async |s: uuid::Uuid| {
            d.line_thread(s, None, None)
                .await
                .unwrap()
                .into_iter()
                .rfind(|m| m.from.kind == PartyKind::Manager)
                .unwrap()
        };
        // Sent into its turn, whose next tool call never comes.
        let into_turn = d.line_send_now(row(steered.id).await.id).await.unwrap();
        assert_eq!(
            into_turn.waiting,
            Some(workforce_desk::line::Wait::NextCall)
        );
        let progress = tokio::spawn(async move {
            loop {
                hook(
                    &busy_url,
                    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                    "tool_input": {"command": "systemctl stop legacy-worker"}}),
                )
                .await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        // What the boss was told of session `s`, from its waiting message
        // to what it should do.
        let untaken = |s: uuid::Uuid| format!("session {s}) hasn't taken your message");
        let woken = async |s: uuid::Uuid| {
            let said = untaken(s);
            eventually("the boss woken", async || {
                prompted(&llm, &said).await.first().and_then(|b| {
                    let (_, rest) = b.split_once(&said)?;
                    rest.split_once("can work.")
                        .map(|(said, _)| said.to_string())
                })
            })
            .await
        };
        let stuck_said = woken(stuck.id).await;
        assert!(
            stuck_said.contains("Go ahead: run step 12 now.")
                && stuck_said.contains("its turn hasn't ended")
                && stuck_said.contains("crew.restart"),
            "{stuck_said}"
        );
        let steered_said = woken(steered.id).await;
        assert!(
            steered_said.contains("with its next tool call, and hasn't made one"),
            "{steered_said}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        progress.abort();
        assert!(
            prompted(&llm, &untaken(busy.id)).await.is_empty(),
            "none for the session at work"
        );
        assert_eq!(
            row(busy.id).await.delivery,
            workforce_desk::line::Delivery::Queued,
            "still waiting: left to it only for its progress"
        );
        assert!(
            (1..=2).contains(&d.store().wake_ups_since("desk", since).await.unwrap()),
            "one wake a stall, and none for the session at work"
        );
    })
    .await;
}

/// The boss's model calls whose prompt says `what`.
async fn prompted(llm: &wiremock::MockServer, what: &str) -> Vec<String> {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .filter(|b| b.contains(what))
        .collect()
}

/// A message waiting behind the question a session ended its turn on wakes
/// its boss with why, and the question is still raised as waiting on the
/// person, with its reminders, as any question is.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_behind_a_sessions_question_still_raises_the_question() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, llm) = overseeing(
            dir.path(),
            pool,
            "I'll read its history.",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
             updates = { enabled = false }\nwork = { stall_minutes = 5 }\n\
             wake = { waiting_minutes = 5, hold_minutes = 2, answer_after_minutes = 2 }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Cufflink Migration").await;
        let url = hook_url(dir.path(), s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
        )
        .await;
        hook(&url, json!({"hook_event_name": "UserPromptSubmit", "prompt": "status?"})).await;
        hook(
            &url,
            json!({"hook_event_name": "Stop",
                   "last_assistant_message": "Step 11 is done. Should I run step 12 against production now?"}),
        )
        .await;
        eventually("it asks", async || {
            (d.view(s.id).unwrap().state == workforce_desk::model::SessionState::Asking).then_some(())
        })
        .await;
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: "Go ahead: run step 12 now.".into(),
                reply_to: None,
            },
            crew.by_overseer("Its timer went off.").unwrap(),
        )
        .await
        .unwrap();
        let woke = eventually("the boss woken", async || {
            prompted(&llm, "hasn't taken your message").await.pop()
        })
        .await;
        assert!(
            woke.contains("it ended its turn asking the person something"),
            "{woke}"
        );
        eventually("its question raised", async || {
            prompted(&llm, "has waited on you").await.pop()
        })
        .await;
    })
    .await;
}

/// A message Desk holds on purpose (the person drives the session) wakes
/// no one, however long it waits.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_held_while_the_person_drives_wakes_no_one() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, llm) = overseeing(
            dir.path(),
            pool,
            "I'll read its history.",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
             updates = { enabled = false }\nwork = { stall_minutes = 5 }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "Cufflink Migration").await;
        let url = hook_url(dir.path(), s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
        )
        .await;
        let boss = workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(
            s.id,
            &boss,
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "I'll take step 12 myself."}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Ready when you are."}),
        )
        .await;
        let m = d
            .line_send(
                workforce_desk::LineSender::Overseer(
                    crew.by_overseer("Its timer went off.").unwrap(),
                ),
                workforce_desk::line::Send::Tell {
                    to: workforce_desk::line::Target::Session(s.id),
                    text: "Go ahead: run step 12 now.".into(),
                    when: workforce_desk::line::When::Queue,
                },
            )
            .await
            .unwrap();
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert_eq!(
            thread.iter().find(|r| r.id == m.id).unwrap().waiting,
            Some(workforce_desk::line::Wait::PersonDriving)
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(
            prompted(&llm, "hasn't taken your message").await.is_empty(),
            "no one is woken for it"
        );
    })
    .await;
}
