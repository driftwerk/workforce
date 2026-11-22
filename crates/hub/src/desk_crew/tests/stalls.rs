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
use workforce_desk::model::SessionState;
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
        // Gone quiet after the person's prompt stops holding it theirs (10
        // min) and its stall is said: then never said.
        let (_overseer, crew, d, llm) = overseeing(
            dir.path(),
            pool,
            "I'll read its history.",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
             updates = { enabled = false }\nwork = { stall_minutes = 5 }\n\
             wake = { quiet_minutes = 15 }\n",
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
        let woken = async |s: uuid::Uuid| {
            eventually("the boss woken", async || {
                told_of(&llm, s).await.into_iter().next()
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
            told_of(&llm, busy.id).await.is_empty(),
            "none for the session at work"
        );
        assert_eq!(
            row(busy.id).await.delivery,
            workforce_desk::line::Delivery::Queued,
            "still waiting: left to it only for its progress"
        );
        for s in [stuck.id, steered.id] {
            assert_eq!(told_of(&llm, s).await.len(), 1, "one wake a stall");
            assert!(
                prompted(&llm, &format!("session {s}) has been quiet"))
                    .await
                    .is_empty(),
                "nor a quiet after it"
            );
        }
    })
    .await;
}

/// Each thing the boss was told, in any of its calls, of session `s` not
/// taking its message: one a wake.
async fn told_of(llm: &wiremock::MockServer, s: uuid::Uuid) -> Vec<String> {
    let said = format!("session {s}) hasn't taken your message");
    let told: std::collections::BTreeSet<String> = prompted(llm, &said)
        .await
        .iter()
        .flat_map(|b| {
            b.split(&said)
                .skip(1)
                .filter_map(|rest| rest.split(['"', '\\']).next())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    told.into_iter().collect()
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
            (d.view(s.id).unwrap().state == SessionState::Asking).then_some(())
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
            told_of(&llm, s.id).await.into_iter().next()
        })
        .await;
        assert!(
            woke.contains("it ended its turn asking the person something")
                && !woke.contains("crew.restart"),
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

/// A message waiting behind a session's request for approval wakes its
/// boss with that, never a restart, and the approval is still raised.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_behind_an_approval_still_raises_the_approval() {
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
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "Run step 12."}),
        )
        .await;
        let request = {
            let url = url.clone();
            tokio::spawn(async move {
                hook(
                    &url,
                    json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                           "tool_input": {"command": "./migrate --step 12 --env production"}}),
                )
                .await
            })
        };
        eventually("it asks for approval", async || {
            d.view(s.id).unwrap().pending.map(|_| ())
        })
        .await;
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: "Then report the row counts.".into(),
                reply_to: None,
            },
            crew.by_overseer("Its step is next.").unwrap(),
        )
        .await
        .unwrap();
        let woke = eventually("the boss woken", async || {
            told_of(&llm, s.id).await.into_iter().next()
        })
        .await;
        assert!(
            woke.contains("it waits on an approval or a dialog in its terminal")
                && !woke.contains("crew.restart"),
            "{woke}"
        );
        eventually("its approval raised", async || {
            prompted(&llm, "has waited on you").await.pop()
        })
        .await;
        request.abort();
    })
    .await;
}

/// What the `ssh` stand-in says while its build runs on the devbox.
#[derive(Clone, Copy)]
pub(super) enum Says {
    /// rbuild's queue line, once.
    Queued,
    /// Nothing at all.
    Nothing,
    /// A test's progress, every fifth of a second.
    Streams,
}

/// A Claude Code stand-in in `dir`, installed as far as Desk can tell,
/// whose tool shell runs `ssh devbox ~/bin/rbuild-run w cargo test` (the
/// night of the overnight stall) with an `ssh` that says `says`; each
/// `ssh` notes its pid and its build's in `dir`'s `ssh.pid` for
/// [`builds_stop`].
pub(super) fn building_cli(dir: &Path, says: Says) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pids = dir.join("ssh.pid");
    let runs = match says {
        Says::Queued => {
            "echo 'rbuild: all 3 build slots are busy; waiting for one' >&2\nsleeps"
        }
        Says::Nothing => "sleeps",
        Says::Streams => {
            "n=0\nwhile [ $n -lt 600 ]; do n=$((n+1)); echo \"test step $n of 600\"; sleep 0.2; done"
        }
    };
    workforce_common::executable::write(
        &bin.join("ssh"),
        format!(
            "#!/bin/sh\nsleeps() {{ sleep 120 & echo $! >> '{p}'; wait; }}\necho $$ >> '{p}'\n{runs}\n",
            p = pids.display()
        ),
    )
    .unwrap();
    let path = dir.join("building-claude");
    workforce_common::executable::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in --version) echo '2.1.300 (Claude Code)'; exit 0;; auth) exit 1;; esac\n\
             printf '%s\\n' \"$@\" > \"{dir}/args-$$\"\n\
             /bin/sh -c 'PATH={bin}:$PATH ssh devbox ~/bin/rbuild-run w cargo test; : /h/.claude/shell-snapshots/snapshot-sh-1.sh' &\n\
             exec sleep 60\n",
            dir = dir.display(),
            bin = bin.display(),
        ),
    )
    .unwrap();
    path
}

/// Ends the builds [`building_cli`]'s sessions ran.
pub(super) fn builds_stop(dir: &Path) {
    std::fs::read_to_string(dir.join("ssh.pid"))
        .unwrap_or_default()
        .lines()
        .for_each(|pid| {
            std::process::Command::new("kill").arg(pid).status().ok();
        });
}

/// Session `s` at work on the tests: started, prompted, its Bash call open.
pub(super) async fn at_work(dir: &Path, s: uuid::Uuid) -> String {
    let url = hook_url(dir, s).await;
    for body in [
        json!({"hook_event_name": "SessionStart", "session_id": format!("cc-{s}")}),
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "Run the tests on the devbox."}),
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "~/bin/rbuild /w -- cargo test"}}),
    ] {
        hook(&url, body).await;
    }
    url
}

/// Looks inside every session until the boss's model is woken by a line
/// saying `what`: its newest message, not the history it carries.
pub(super) async fn looked_until_woken(d: &Desk, llm: &wiremock::MockServer, what: &str) {
    eventually("the boss woken by what Desk sees", async || {
        d.sight_pass().await;
        (!woken_by(llm, what).await.is_empty()).then_some(())
    })
    .await;
}

/// The line saying `what` that woke each of the boss's model calls woken
/// by one: in its newest message, past the context Desk gives every turn.
pub(super) async fn woken_by(llm: &wiremock::MockServer, what: &str) -> Vec<String> {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .filter_map(|b| {
            let newest = b["messages"]
                .as_array()?
                .iter()
                .rev()
                .find(|m| m["role"] == "user")?
                .to_string();
            let asked = newest.split("[END SYSTEM CONTEXT]").last()?;
            asked
                .split("\\n")
                .find(|l| l.contains(what))
                .map(str::to_string)
        })
        .collect()
}

/// The first item about session `s` any update posted says who moves.
async fn moved_in_updates(overseer: &AgentService, d: &Desk, s: uuid::Uuid) -> Option<Said> {
    let posted: Vec<String> = overseer
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
        .map(|m| m.id)
        .collect();
    d.store()
        .extras(&posted)
        .await
        .ok()?
        .into_values()
        .filter_map(|e| e.update)
        .flat_map(|u| u.said)
        .find(|i| {
            i.moves.is_some()
                && i.session
                    .as_ref()
                    .is_some_and(|l| l.session_id == s.to_string())
        })
}

/// Act, a stall half a second and quiet as long.
const SIGHTED: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
                       work = { stall_minutes = 5 }\nwake = { quiet_minutes = 5 }\n";

/// Where the overnight session waited: on its terminal, or in the
/// background once its turn ended.
#[derive(Clone, Copy)]
enum Shape {
    Foreground,
    Background,
}

/// The overnight stall of 21 Nov, as `shape`: a session waiting hours on
/// a devbox build slot is seen waiting on the overview, its boss is told
/// once to look inside and move it, and its stall in an update is the
/// queue's, looked at again, never "working on it now".
async fn a_queued_build_is_seen_said_once_and_looked_at_again(shape: Shape) {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let says = match shape {
            Shape::Foreground => Says::Queued,
            Shape::Background => Says::Nothing,
        };
        let d = sighted_desk(pool, &base, building_cli(&base, says)).await;
        let (overseer, crew, d, llm) = overseeing_on(&base, d, "ok", SIGHTED, &[], FAST).await;
        crew.set_update_writer(Arc::new(Down));
        crew.look_at_updates().await;
        let s = new_session(&d, &base, "workforce tests").await;
        let url = at_work(&base, s.id).await;
        if let Shape::Background = shape {
            let cli = format!("cc-{}", s.id);
            let project = base.join("projects").join("-w");
            std::fs::create_dir_all(&project).unwrap();
            let transcript = project.join(format!("{cli}.jsonl"));
            let lines = [
                json!({"type": "assistant", "uuid": "a1", "timestamp": chrono::Utc::now(),
                    "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "t1",
                    "name": "Bash", "input": {"command": "~/bin/rbuild /w -- cargo test",
                    "run_in_background": true}}]}}),
                json!({"type": "user", "uuid": "u1", "timestamp": chrono::Utc::now(),
                    "message": {"role": "user", "content": [{"type": "tool_result",
                    "tool_use_id": "t1", "content": "Command running in background with ID: b1"}]}}),
                json!({"type": "assistant", "uuid": "a2", "timestamp": chrono::Utc::now(),
                    "message": {"role": "assistant", "content": [{"type": "text",
                    "text": "The tests run in the background."}]}}),
            ];
            std::fs::write(
                &transcript,
                lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
            )
            .unwrap();
            let tasks = base.join("tasks").join("-w").join(&cli).join("tasks");
            std::fs::create_dir_all(&tasks).unwrap();
            std::fs::write(
                tasks.join("b1.output"),
                "rbuild: all 3 build slots are busy; waiting for one\n",
            )
            .unwrap();
            hook(
                &url,
                json!({"hook_event_name": "Stop", "transcript_path": transcript,
                    "last_assistant_message": "The tests run in the background.",
                    "background_tasks": [{"id": "b1", "type": "shell", "status": "running",
                        "command": "~/bin/rbuild /w -- cargo test"}]}),
            )
            .await;
            eventually("its turn done, its tests in the background", async || {
                d.view(s.id)
                    .ok()
                    .filter(|v| v.state == SessionState::Done && v.waiting_on() == 1)
            })
            .await;
        }
        looked_until_woken(&d, &llm, "for a build slot. Look inside (crew.inside)").await;
        let v = d.view(s.id).unwrap();
        assert_eq!(v.waits.unwrap().words, "waiting for a build slot");
        let overview = overseer
            .tools
            .get("crew.overview")
            .unwrap()
            .execute(json!({}))
            .await
            .unwrap()
            .output
            .to_string();
        assert!(
            overview.contains("waiting for a build slot, "),
            "{overview}"
        );

        account(
            &d,
            s.id,
            "Run the workforce tests",
            &["The test run is blocked, waiting for a build slot on the devbox."],
            None,
        )
        .await;
        let item = eventually("its stall in an update", async || {
            moved_in_updates(&overseer, &d, s.id).await
        })
        .await;
        let Some(Mover::Outside { on, .. }) = item.moves.clone() else {
            panic!("outside: {item:?}");
        };
        assert!(on.starts_with("a build slot (waited "), "{on}");
        assert!(!item.body.contains("working on it now"), "{item:?}");

        for _ in 0..5 {
            d.sight_pass().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let woke = woken_by(&llm, "for a build slot. Look inside (crew.inside)").await;
        assert_eq!(woke.len(), 1, "said once: {woke:?}");
        builds_stop(&base);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_queued_at_its_terminal_is_seen_said_once_and_looked_at_again() {
    a_queued_build_is_seen_said_once_and_looked_at_again(Shape::Foreground).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_queued_in_the_background_is_seen_said_once_and_looked_at_again() {
    a_queued_build_is_seen_said_once_and_looked_at_again(Shape::Background).await;
}

/// The same night with nothing printed: nothing moves under it, so the
/// stall is found from what Desk sees alone, its ssh named.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_build_over_ssh_is_said_still_from_its_evidence_alone() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let d = sighted_desk(pool, &base, building_cli(&base, Says::Nothing)).await;
        let (_overseer, _crew, d, llm) = overseeing_on(&base, d, "ok", SIGHTED, &[], FAST).await;
        let s = new_session(&d, &base, "workforce tests").await;
        at_work(&base, s.id).await;
        looked_until_woken(&d, &llm, "its ssh command may still run on devbox").await;
        let woke = prompted(&llm, "its ssh command may still run on devbox").await;
        assert!(
            woke.iter()
                .any(|b| b.contains("nothing has moved for ")
                    && b.contains("Look inside (crew.inside)")),
            "{woke:?}"
        );
        builds_stop(&base);
    })
    .await;
}

/// A build that keeps printing is never a stall, however long it runs: its
/// quiet is said with what Desk sees, and nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn a_build_that_keeps_printing_is_never_said_stalled() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let d = sighted_desk(pool, &base, building_cli(&base, Says::Streams)).await;
        let (_overseer, _crew, d, llm) = overseeing_on(&base, d, "ok", SIGHTED, &[], FAST).await;
        let s = new_session(&d, &base, "workforce tests").await;
        at_work(&base, s.id).await;
        looked_until_woken(&d, &llm, "— what Desk sees: ").await;
        for _ in 0..15 {
            d.sight_pass().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stalled = woken_by(&llm, "Look inside (crew.inside)").await;
        assert!(stalled.is_empty(), "{stalled:?}");
        let quiet = woken_by(&llm, "— what Desk sees: ").await;
        assert_eq!(quiet.len(), 1, "once a quiet spell: {quiet:?}");
        builds_stop(&base);
    })
    .await;
}
