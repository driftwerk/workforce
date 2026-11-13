//! Charges end to end in the desk: taking a session on and letting it go,
//! the person taking the wheel, and the boss's messages waiting for it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, new_session};
use super::*;
use crate::line::{Delivery, Send, Target, Wait, When};
use crate::service::Sender;
use crate::store::build_pool;

async fn hook(d: &Desk, id: Uuid, body: Value) -> Value {
    let token = d.0.state.lock().live[&id].token.clone();
    d.hook(id, &token, body.to_string().as_bytes())
        .await
        .unwrap()
}

/// It starts, and Desk's plugin in it is heard from.
async fn idle(d: &Desk, id: Uuid) {
    hook(
        d,
        id,
        json!({"hook_event_name": "SessionStart", "session_id": id.to_string()}),
    )
    .await;
    d.plugin_heard_from(id);
}

fn sarah() -> Party {
    Party::manager("sarah", "Sarah")
}

/// What the plugin is handed next, said into the session.
async fn take_next(d: &Desk, id: Uuid, wait: Duration) -> Option<Value> {
    let next = d.line_next(id, wait).await?;
    let m: Uuid = serde_json::from_value(next["id"].clone()).unwrap();
    d.line_ack(id, m, true).await.unwrap();
    Some(next)
}

#[tokio::test]
async fn a_persons_prompt_holds_the_bosss_queued_tell_and_hand_back_delivers_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        let c = d
            .take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        assert_eq!((c.boss_name.as_str(), c.wheel), ("Sarah", Wheel::Boss));
        // Its thread says who runs it with the person, and no turn starts.
        assert!(
            d.line_next(s.id, Duration::from_millis(200))
                .await
                .is_none(),
            "the note isn't typed in"
        );
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let note = thread.last().unwrap();
        assert_eq!(note.delivery, Delivery::Noted);
        assert!(note
            .text
            .contains("I'm running this session with the person now"));

        // The person prompts it: theirs now.
        hook(
            &d,
            s.id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "try the other fix"}),
        )
        .await;
        hook(
            &d,
            s.id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Tried it."}),
        )
        .await;
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Person));
        let by = By::manager("sarah", "Sarah", "Next step.");
        let m = d
            .line_send(
                Sender::Overseer(by),
                Send::Tell {
                    to: Target::Session(s.id),
                    text: "Run the tests next.".into(),
                    when: When::Queue,
                },
            )
            .await
            .unwrap();
        assert!(
            d.line_next(s.id, Duration::from_millis(200))
                .await
                .is_none(),
            "held while the person drives"
        );
        let inbox = d.line_call(s.id, "inbox", &json!({}), None).await.unwrap();
        assert_eq!(inbox["messages"], json!([]), "nor read from its inbox");
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let waiting = thread.iter().find(|r| r.id == m.id).unwrap();
        assert_eq!(waiting.waiting, Some(Wait::PersonDriving));
        assert_eq!(
            serde_json::to_value(waiting).unwrap()["waiting"],
            "person_driving"
        );
        // The person's own words still go in.
        let mine = d
            .line_post(s.id, "and the lint".into(), When::Queue)
            .await
            .unwrap();
        let next = take_next(&d, s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["id"], json!(mine.id));

        // Written down once, as Activity says.
        d.charge_settle().await;
        assert_eq!(d.charge_of(s.id).unwrap().wheel, Wheel::Person);

        // The plugin, waiting for a message, gets it once handed back.
        let waiting = {
            let d = d.clone();
            tokio::spawn(async move { d.line_next(s.id, Duration::from_secs(10)).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let back = d.hand_back(s.id).await.unwrap();
        assert_eq!(back.wheel, Wheel::Boss);
        let next = tokio::time::timeout(Duration::from_secs(3), waiting)
            .await
            .expect("handed back, it goes in")
            .unwrap()
            .unwrap();
        assert_eq!(next["id"], json!(m.id));
        assert!(next["text"]
            .as_str()
            .unwrap()
            .contains("tells you: Run the tests next."));
        // Activity is written behind: looked at until it is.
        for _ in 0..200 {
            let kinds: Vec<Kind> = d
                .activity(crew::Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .iter()
                .map(|e| e.kind)
                .collect();
            if [Kind::TakenOn, Kind::Driving, Kind::HandedBack]
                .iter()
                .all(|k| kinds.contains(k))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("taking on, driving and handing back aren't all in Activity");
    })
    .await;
}

/// The person holds a session only while they are active in it: an ask
/// 30 seconds after their last input waits for them, one 3 minutes after
/// is the boss's, and a boss's own hold, as its settings say, counts.
#[tokio::test]
async fn the_persons_hold_lasts_two_minutes_after_their_last_input_unless_the_boss_says() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let typed = |ago: chrono::Duration| {
            d.0.state
                .lock()
                .charges
                .driven
                .insert(s.id, Utc::now() - ago);
        };
        let by = By::manager("sarah", "Sarah", "Answering it.");
        // Taken on an hour ago, so the input below is since.
        d.0.state.lock().charges.rows.get_mut(&s.id).unwrap().since =
            Utc::now() - chrono::Duration::hours(1);

        typed(chrono::Duration::seconds(30));
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Person), "still typing");
        assert!(d.boss_holds(s.id, Some(&by)).is_err());
        // Unbound (Act fully), the person typing holds it back from nothing.
        let unbound = crate::charge::FullGrant {
            unbound: true,
            ..Default::default()
        };
        d.set_full([("sarah".to_string(), unbound)].into());
        assert!(d.boss_holds(s.id, Some(&by)).is_ok(), "unbound");
        d.set_full(std::collections::HashMap::new());
        assert!(d.boss_holds(s.id, Some(&by)).is_err());
        typed(chrono::Duration::minutes(3));
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Boss), "stopped typing");
        assert!(d.boss_holds(s.id, Some(&by)).is_ok());

        // Sarah's settings hold for 5 minutes.
        d.set_holds([("sarah".to_string(), chrono::Duration::minutes(5))].into());
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Person));
        typed(chrono::Duration::minutes(6));
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Boss));
        d.set_holds(Default::default());

        // Written down as it changes hands, in honest words.
        typed(chrono::Duration::seconds(30));
        d.charge_settle().await;
        assert_eq!(d.charge_of(s.id).unwrap().wheel, Wheel::Person);
        {
            let mut st = d.0.state.lock();
            let c = st.charges.rows.get_mut(&s.id).unwrap();
            c.wheel_since = Some(Utc::now() - chrono::Duration::minutes(3));
        }
        typed(chrono::Duration::minutes(3));
        d.charge_settle().await;
        assert_eq!(d.charge_of(s.id).unwrap().wheel, Wheel::Boss);
        for _ in 0..200 {
            let texts: Vec<String> = d
                .activity(crew::Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .iter()
                .map(|e| e.text.clone())
                .collect();
            if texts.iter().any(|t| t.starts_with("is driving"))
                && texts
                    .iter()
                    .any(|t| t.ends_with("back after 2 quiet minutes"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("driving and getting it back after 2 quiet minutes aren't in Activity");
    })
    .await;
}

/// Typing a draft in Desk's composer, unsent, keeps the session the
/// person's as a keystroke in its terminal does: the typing route counts
/// as their input, and an unknown session is refused.
#[tokio::test]
async fn a_draft_typed_in_the_composer_keeps_the_session_the_persons() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.0.state.lock().charges.rows.get_mut(&s.id).unwrap().since =
            Utc::now() - chrono::Duration::hours(1);
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Boss));
        let app = crate::charge_api::api_routes(d.clone(), Arc::new(crate::testing::Bearer));
        let typing = |id: Uuid| {
            Request::builder()
                .method("POST")
                .uri(format!("/api/desk/sessions/{id}/typing"))
                .header("authorization", "Bearer hub-token")
                .body(Body::empty())
                .unwrap()
        };
        let res = app.clone().oneshot(typing(s.id)).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(d.wheel_of(s.id), Some(Wheel::Person), "a draft is input");
        let by = By::manager("sarah", "Sarah", "Answering it.");
        assert!(d.boss_holds(s.id, Some(&by)).is_err());
        let res = app.clone().oneshot(typing(Uuid::new_v4())).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let unauthorized = Request::builder()
            .method("POST")
            .uri(format!("/api/desk/sessions/{}/typing", s.id))
            .body(Body::empty())
            .unwrap();
        assert!(!app
            .oneshot(unauthorized)
            .await
            .unwrap()
            .status()
            .is_success());
        assert!(charge::TYPING_EVERY < charge::HOLD.to_std().unwrap() / 4);
    })
    .await;
}

#[tokio::test]
async fn ill_drive_keeps_the_wheel_and_a_release_keeps_the_row() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Taken, &Default::default(), &By::you())
            .await
            .unwrap();
        assert_eq!(d.charges().len(), 1);
        assert_eq!(d.drive(s.id).await.unwrap().wheel, Wheel::PersonPinned);
        d.charge_settle().await;
        assert_eq!(d.wheel_of(s.id), Some(Wheel::PersonPinned));
        // Passed to another boss, the person keeps driving.
        let mia = Party::manager("mia", "Mia");
        let passed = d
            .take_on(s.id, &mia, How::Taken, &Default::default(), &By::you())
            .await
            .unwrap();
        assert_eq!(
            (passed.boss.as_str(), passed.wheel),
            ("mia", Wheel::PersonPinned)
        );
        d.take_on(s.id, &sarah(), How::Taken, &Default::default(), &By::you())
            .await
            .unwrap();

        let by = By::manager("sarah", "Sarah", "It's done.");
        let released = d.release(s.id, &by).await.unwrap();
        assert!(released.released_at.is_some());
        assert!(d.charges().is_empty());
        assert_eq!(d.wheel_of(s.id), None);
        assert!(matches!(
            d.release(s.id, &by).await,
            Err(DeskError::Conflict(_))
        ));
        // Kept, released, across a restart: never taken on by itself again.
        d.0.state.lock().charges.rows.clear();
        d.charge_load().await;
        let kept = d.charge_of(s.id).unwrap();
        assert!(!kept.live());
        assert!(!charge::takes_automatically(
            true,
            &d.view(s.id).unwrap(),
            Some(&kept),
            &Default::default(),
            &std::collections::HashSet::new()
        ));
        // Handed to it again by the person, it is the boss's again.
        let again = d
            .take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        assert!(again.released_at.is_none());
        assert_eq!(again.wheel, Wheel::Boss);
    })
    .await;
}

#[tokio::test]
async fn an_ended_session_cant_be_taken_on_and_says_why() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        d.0.state.lock().records.get_mut(&s.id).unwrap().state = SessionState::Closed;
        match d
            .take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
        {
            Err(DeskError::Conflict(why)) => assert_eq!(why, "It has ended."),
            other => panic!("{other:?}"),
        }
        assert!(d.charge_of(s.id).is_none());
    })
    .await;
}

fn start_in(dir: &std::path::Path, group: Option<&str>) -> crate::model::NewSession {
    crate::model::NewSession {
        fresh_worktree: false,
        engine: crate::engine::Engine::Claude,
        view: None,
        name: "acme-web".into(),
        cwd: dir.to_string_lossy().into_owned(),
        goal: "Upgrade the date picker".into(),
        worktree: None,
        prompt: None,
        approvals: None,
        approve_for_me: false,
        fast: false,
        account: None,
        env: Default::default(),
        group: group.map(Into::into),
    }
}

/// A session a boss starts is in its charge and its group before it runs:
/// the first the sidebar hears of it, it is already both, and both are
/// kept before it can spend anything.
#[tokio::test]
async fn a_session_a_boss_starts_is_its_and_in_its_group_before_it_runs() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| l.create("g-acme", "Acme", &[], None))
            .await
            .unwrap();
        let (_, mut rx) = d.subscribe();
        let by = By::manager("sarah", "Sarah", "Acme's web upgrade");
        let s = d
            .create_for(start_in(dir.path(), Some("g-acme")), &sarah(), by)
            .await
            .unwrap();
        assert_eq!(s.group.as_ref().map(|g| g.id.as_str()), Some("g-acme"));

        // In the order the sidebar heard it: charged, then placed, and its
        // first appearance already in Acme.
        let mut charged = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                DeskEvent::Charge { charge } if charge.session == s.id => {
                    assert_eq!(
                        (charge.how, charge.boss_name.as_str()),
                        (How::Started, "Sarah")
                    );
                    assert_eq!(charge.group.as_deref(), Some("g-acme"));
                    charged = true;
                }
                DeskEvent::Session { session } if session.id == s.id => {
                    assert!(charged, "charged before it is first seen");
                    assert_eq!(
                        session.group.as_ref().map(|g| g.id.as_str()),
                        Some("g-acme"),
                        "in Acme from the first"
                    );
                    break;
                }
                _ => {}
            }
        }
        // Kept, not only in memory.
        let kept = d.0.store.charges().await.unwrap();
        assert!(kept
            .iter()
            .any(|c| c.session == s.id && c.boss == "sarah" && c.how == How::Started));
        let layout = d.0.store.layout().await.unwrap();
        assert_eq!(
            layout
                .group_of(&super::super::session_item(s.id))
                .map(|g| g.id.as_str()),
            Some("g-acme")
        );
        assert_eq!(d.charges_busy("sarah"), 1, "starting holds a slot");
        // Its thread says who runs it, and no turn starts for it.
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert!(thread
            .last()
            .unwrap()
            .text
            .contains("I started this session for the person"));

        // A group the sidebar doesn't have starts nothing.
        let before = d.list().len();
        match d
            .create_for(
                start_in(dir.path(), Some("g-atlas")),
                &sarah(),
                By::manager("sarah", "Sarah", ""),
            )
            .await
        {
            Err(DeskError::Invalid(why)) => {
                assert_eq!(why, "there's no group g-atlas in the sidebar")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(d.list().len(), before);
        assert_eq!(d.charges().len(), 1);
    })
    .await;
}

/// A hub whose roster calls each agent by the name given here.
struct Roster(Vec<(&'static str, &'static str)>);

#[async_trait::async_trait]
impl crew::Crew for Roster {
    fn overseer(&self) -> Option<crew::Overseer> {
        None
    }
    fn agents(&self) -> Vec<crew::CrewAgent> {
        self.0
            .iter()
            .map(|(id, name)| crew::CrewAgent {
                id: id.to_string(),
                name: name.to_string(),
                live: None,
            })
            .collect()
    }
    fn midnight(&self) -> chrono::DateTime<Utc> {
        Utc::now()
    }
    async fn accept(&self, _: &crew::Suggestion) -> Result<(), String> {
        Ok(())
    }
}

/// A boss is called what the hub's roster calls it now, so its name
/// outlives a restart; one known only by its id is never shown by it.
#[tokio::test]
async fn a_boss_is_called_by_its_name_after_a_restart_and_never_by_its_id() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let name = |d: &Desk| d.charges()[0].boss_name.clone();
        assert_eq!(name(&d), "Sarah");

        // After a restart only the rows are known: not what she was called.
        d.0.state.lock().charges.names.clear();
        assert_eq!(name(&d), UNNAMED_BOSS, "never her id");

        // The hub's roster says.
        let roster: Arc<dyn crew::Crew> = Arc::new(Roster(vec![("sarah", "Sarah")]));
        d.answer_lines_with(&roster);
        assert_eq!(name(&d), "Sarah");
        d.hand_back(s.id).await.unwrap_or_else(|e| panic!("{e:?}"));
        let mut said = String::new();
        for _ in 0..200 {
            let page = d.activity(crew::Filter::All, None, 5).await.unwrap();
            if let Some(e) = page.events.iter().find(|e| e.text.contains(" back to ")) {
                said = e.text.clone();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(said.ends_with("back to Sarah"), "{said}");

        // An agent with no name of its own: its id is no name.
        let roster: Arc<dyn crew::Crew> = Arc::new(Roster(vec![("sarah", "sarah")]));
        d.answer_lines_with(&roster);
        assert_eq!(name(&d), UNNAMED_BOSS);
        d.0.state
            .lock()
            .charges
            .names
            .insert("sarah".into(), "sarah".into());
        assert_eq!(name(&d), UNNAMED_BOSS);
    })
    .await;
}

/// A charge made for a session that then didn't start is let go where
/// Desk shows it, so none is left behind.
#[tokio::test]
async fn a_charge_for_a_start_that_failed_is_let_go_where_desk_shows_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = Uuid::new_v4();
        d.charge_started(id, &sarah(), None).await.unwrap();
        assert_eq!(d.charges().len(), 1);
        let (_, mut rx) = d.subscribe();
        d.charge_forget(id);
        let shown = loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                DeskEvent::Charge { charge } if charge.session == id => break charge,
                _ => {}
            }
        };
        assert!(shown.released_at.is_some(), "{shown:?}");
        assert!(d.charges().is_empty());
        assert!(d.charge_of(id).is_none());
    })
    .await;
}

fn boss_tell(text: &str, to: Uuid) -> Send {
    Send::Tell {
        to: Target::Session(to),
        text: text.into(),
        when: When::Queue,
    }
}

/// Let go while the person drives, what the boss said and the person held
/// is withdrawn, never typed into a session the person now runs alone.
#[tokio::test]
async fn a_release_withdraws_what_the_boss_said_and_starts_no_turn() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.person_drove(s.id);
        let by = By::manager("sarah", "Sarah", "Next.");
        let held = d
            .line_send(Sender::Overseer(by.clone()), boss_tell("Push it.", s.id))
            .await
            .unwrap();
        d.release(s.id, &By::you()).await.unwrap();
        assert!(
            d.line_next(s.id, Duration::from_millis(300))
                .await
                .is_none(),
            "nothing goes in"
        );
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        let withdrawn = thread.iter().find(|m| m.id == held.id).unwrap();
        assert_eq!(withdrawn.delivery, Delivery::Withdrawn);
        assert_eq!(
            serde_json::to_value(withdrawn).unwrap()["delivery"],
            "withdrawn"
        );
        let note = thread.last().unwrap();
        assert_eq!(note.delivery, Delivery::Noted);
        assert!(note.text.contains("I'm no longer running this session"));
    })
    .await;
}

/// A boss's message already handed to Desk's plugin when the person takes
/// the session back is withdrawn when it comes back to the queue, dropped
/// by the plugin or stale, never typed into the session they now run
/// alone; a session no boss ran keeps the overseer's nudge.
#[tokio::test]
async fn a_handed_message_that_comes_back_after_a_release_is_withdrawn() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let by = By::manager("sarah", "Sarah", "Next.");
        let send = |text: &str, to: Uuid| {
            let d = d.clone();
            let by = by.clone();
            let text = text.to_string();
            async move {
                d.line_send(Sender::Overseer(by), boss_tell(&text, to))
                    .await
                    .unwrap()
            }
        };
        let delivery = |id: Uuid| {
            let d = d.clone();
            async move { d.0.store.line_message(id).await.unwrap().unwrap().delivery }
        };

        // Dropped by the plugin after the release.
        let dropped = send("Run the tests.", s.id).await;
        let next = d.line_next(s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["id"], json!(dropped.id));
        assert_eq!(delivery(dropped.id).await, Delivery::Handed);
        d.release(s.id, &By::you()).await.unwrap();
        assert_eq!(
            delivery(dropped.id).await,
            Delivery::Handed,
            "a release leaves what the plugin has"
        );
        d.line_ack(s.id, dropped.id, false).await.unwrap();
        assert_eq!(delivery(dropped.id).await, Delivery::Withdrawn);
        assert!(d
            .line_next(s.id, Duration::from_millis(300))
            .await
            .is_none());

        // Stale, after the session passed to another boss.
        let mia = Party::manager("mia", "Mia");
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let stale = send("Carry on.", s.id).await;
        let next = d.line_next(s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["id"], json!(stale.id));
        d.take_on(s.id, &mia, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let back =
            d.0.store
                .line_requeue(None, Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap();
        assert_eq!(back.len(), 1);
        d.line_requeued(back).await;
        assert_eq!(delivery(stale.id).await, Delivery::Withdrawn);

        // A session no boss ever ran: the nudge comes back and goes in.
        let other = new_session(&d, dir.path()).await;
        idle(&d, other.id).await;
        let nudge = send("How is it going?", other.id).await;
        let next = d.line_next(other.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["id"], json!(nudge.id));
        d.line_ack(other.id, nudge.id, false).await.unwrap();
        assert_eq!(delivery(nudge.id).await, Delivery::Queued);
        let again = take_next(&d, other.id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(again["id"], json!(nudge.id));
        assert_eq!(delivery(nudge.id).await, Delivery::Delivered);
    })
    .await;
}

/// The person's message goes ahead of a boss's that was waiting for the
/// same idle.
#[tokio::test]
async fn the_persons_message_goes_ahead_of_the_bosss_waiting_one() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let by = By::manager("sarah", "Sarah", "Next.");
        let boss = d
            .line_send(Sender::Overseer(by), boss_tell("Run the tests.", s.id))
            .await
            .unwrap();
        let mine = d
            .line_post(s.id, "Stop, try the other fix".into(), When::Queue)
            .await
            .unwrap();
        let next = take_next(&d, s.id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(next["id"], json!(mine.id));
        assert!(row_waits(&d, boss.id).await);
    })
    .await;
}

async fn row_waits(d: &Desk, id: Uuid) -> bool {
    d.0.store.line_message(id).await.unwrap().unwrap().waits()
}

/// Until Desk has read which sessions bosses run, every boss's message to
/// a session waits and nothing is taken on; after a restart, a wheel the
/// person had counts its quiet from then.
#[tokio::test]
async fn not_knowing_who_runs_what_holds_the_bosss_messages_and_a_restart_keeps_the_wheel() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let other = new_session(&d, dir.path()).await;
        idle(&d, other.id).await;
        d.0.state.lock().charges.loaded = false;
        let by = By::manager("sarah", "Sarah", "Next.");
        let held = d
            .line_send(Sender::Overseer(by), boss_tell("Carry on.", other.id))
            .await
            .unwrap();
        assert!(d
            .line_next(other.id, Duration::from_millis(200))
            .await
            .is_none());
        assert!(row_waits(&d, held.id).await);
        assert!(matches!(
            d.take_on(
                other.id,
                &sarah(),
                How::Handed,
                &Default::default(),
                &By::you()
            )
            .await,
            Err(DeskError::Conflict(_))
        ));

        // The person drove before a restart, long enough ago to be quiet.
        let mut c = d.charge_of(s.id).unwrap();
        c.wheel = Wheel::Person;
        c.wheel_since = Some(crew::now() - chrono::Duration::hours(1));
        d.0.store.put_charge(&c).await.unwrap();
        assert!(d.charge_load().await);
        assert_eq!(
            d.wheel_of(s.id),
            Some(Wheel::Person),
            "quiet from the restart"
        );
        let next = take_next(&d, other.id, Duration::from_secs(1)).await;
        assert_eq!(
            next.unwrap()["id"],
            json!(held.id),
            "known again, it goes in"
        );
    })
    .await;
}

/// A session a boss no longer overseeing ran passes to the new one, or is
/// let go once it has ended.
#[tokio::test]
async fn a_session_passes_to_the_new_boss_or_is_let_go_once_ended() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let (a, b) = (
            new_session(&d, dir.path()).await,
            new_session(&d, dir.path()).await,
        );
        for s in [&a, &b] {
            d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
                .await
                .unwrap();
        }
        d.0.state.lock().records.get_mut(&b.id).unwrap().state = SessionState::Closed;
        let mia = Party::manager("mia", "Mia");
        let by = By::manager("mia", "Mia", "Its boss no longer oversees.");
        let passed = d
            .pass_charge(a.id, &mia, &Default::default(), &by)
            .await
            .unwrap();
        assert_eq!((passed.boss.as_str(), passed.how), ("mia", How::Handed));
        let ended = d
            .pass_charge(b.id, &mia, &Default::default(), &by)
            .await
            .unwrap();
        assert!(ended.released_at.is_some());
        assert_eq!(d.charges().len(), 1);
    })
    .await;
}

/// What its session asks, open until answered.
fn ask_permission(d: &Desk, id: Uuid, command: &str) -> tokio::task::JoinHandle<Value> {
    let d = d.clone();
    let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
        "tool_input": {"command": command}});
    tokio::spawn(async move { hook(&d, id, body).await })
}

async fn asked(d: &Desk, id: Uuid, command: &str) -> Uuid {
    for _ in 0..200 {
        if let Some(p) = d.view(id).unwrap().pending.filter(|p| p.detail == command) {
            return p.id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{command} was never asked");
}

/// Desk itself holds a boss to what it may answer on a session it runs:
/// what can't be undone, a session another boss runs or the person
/// drives, and the rest of the session are the person's; an answer after
/// the person's is told it was already answered.
#[tokio::test]
async fn desk_holds_a_boss_to_what_it_may_answer_for_its_session() {
    use crate::model::Decision;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let sarah_by = By::manager("sarah", "Sarah", "It's the plan.");
        let refused = |r: Result<crate::model::SessionView, DeskError>| match r {
            Err(DeskError::Conflict(why) | DeskError::Invalid(why)) => why,
            other => panic!("not refused: {other:?}"),
        };

        // Until the hub says where answers are judged, none is the boss's.
        let tests = ask_permission(&d, s.id, "cargo test");
        let a = asked(&d, s.id, "cargo test").await;
        assert_eq!(
            refused(d.decide(s.id, a, Decision::Allow, Some(sarah_by.clone()))),
            "Desk can't judge requests for a boss yet: it's the person's to allow"
        );
        d.boss_bounds_with(crate::charge::Bounds {
            home: dir.path().to_path_buf(),
            scratch: vec![],
            withheld: vec![],
        });
        assert_eq!(
            refused(d.decide(s.id, a, Decision::AllowForSession, Some(sarah_by.clone()))),
            "only the person answers for the rest of a session"
        );
        let mia = By::manager("mia", "Mia", "Mine now.");
        assert!(
            refused(d.decide(s.id, a, Decision::Allow, Some(mia))).contains("another boss runs")
        );
        d.decide(s.id, a, Decision::Allow, Some(sarah_by.clone()))
            .unwrap();
        assert_eq!(
            tests.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );

        // What can't be undone is the person's; denying it is the boss's.
        let force = ask_permission(&d, s.id, "git push --force origin feat");
        let a = asked(&d, s.id, "git push --force origin feat").await;
        assert_eq!(
            refused(d.decide(s.id, a, Decision::Allow, Some(sarah_by.clone()))),
            "can't be undone: it's the person's to allow"
        );
        d.decide(s.id, a, Decision::Deny, Some(sarah_by.clone()))
            .unwrap();
        assert_eq!(
            force.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );

        // The person answered first.
        let build = ask_permission(&d, s.id, "cargo build");
        let a = asked(&d, s.id, "cargo build").await;
        d.decide(s.id, a, Decision::Allow, Some(By::you())).unwrap();
        build.await.unwrap();
        assert_eq!(
            refused(d.decide(s.id, a, Decision::Allow, Some(sarah_by.clone()))),
            "already answered"
        );
        assert_eq!(
            refused(d.decide(s.id, a, Decision::Allow, Some(By::you()))),
            "already answered"
        );

        // The person drives it: theirs to answer.
        let lint = ask_permission(&d, s.id, "cargo clippy");
        let a = asked(&d, s.id, "cargo clippy").await;
        d.drive(s.id).await.unwrap();
        assert!(
            refused(d.decide(s.id, a, Decision::Allow, Some(sarah_by.clone())))
                .contains("the person is driving")
        );
        assert!(
            refused(d.decide(s.id, a, Decision::Deny, Some(sarah_by.clone())))
                .contains("the person is driving")
        );
        assert!(
            refused(d.interrupt(s.id, Some(sarah_by.clone()))).contains("the person is driving")
        );
        assert!(refused(d.resume(s.id, None, Some(sarah_by.clone())).await)
            .contains("the person is driving"));
        assert!(refused(d.restart(s.id, None, sarah_by.clone()).await)
            .contains("the person is driving"));
        let mia = By::manager("mia", "Mia", "Mine now.");
        assert!(refused(d.interrupt(s.id, Some(mia))).contains("another boss runs"));
        d.decide(s.id, a, Decision::Allow, Some(By::you())).unwrap();
        lint.await.unwrap();
    })
    .await;
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A push is judged by the repository it runs in: the own folder's, on a
/// branch that isn't the default one. From another repository (another
/// cwd, a nested one) the boss can't tell what it pushes.
#[tokio::test]
async fn a_push_is_judged_by_the_repository_it_runs_in() {
    use crate::model::Decision;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let own = base.join("acme-web");
        let other = base.join("acme-api");
        for (repo, branch) in [(&own, "feat/x"), (&other, "main")] {
            std::fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", branch]);
        }
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, &own).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.boss_bounds_with(crate::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let boss = By::manager("sarah", "Sarah", "It's the plan.");
        let ask = |cwd: &std::path::Path| {
            let d = d.clone();
            let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                "tool_input": {"command": "git push origin HEAD"}, "cwd": cwd});
            tokio::spawn(async move { hook(&d, s.id, body).await })
        };
        let open = || async {
            for _ in 0..200 {
                if let Some(p) = d.view(s.id).unwrap().pending {
                    return p.id;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("never asked");
        };

        let elsewhere = ask(&other);
        let a = open().await;
        assert_eq!(
            d.boss_answerable(s.id, a, "sarah").unwrap_err().to_string(),
            "Desk can't tell which branch this pushes"
        );
        d.decide(s.id, a, Decision::Deny, Some(boss.clone()))
            .unwrap();
        elsewhere.await.unwrap();

        let here = ask(&own);
        let a = open().await;
        assert_eq!(d.boss_answerable(s.id, a, "sarah"), Ok(()));
        d.decide(s.id, a, Decision::Allow, Some(boss)).unwrap();
        assert_eq!(
            here.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
    })
    .await;
}

/// A boss Desk's breaker holds (over its own budget) takes no turns, and
/// what it queued for its sessions is withdrawn: it never goes in.
#[tokio::test]
async fn a_held_bosss_queued_messages_are_withdrawn() {
    use workforce_core::budget::{OnExceed, Period, Standing, Status, Unit};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.person_drove(s.id);
        let by = By::manager("sarah", "Sarah", "Next.");
        let queued = d
            .line_send(Sender::Overseer(by), boss_tell("Push it.", s.id))
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let over = Standing {
            budget: Uuid::new_v4(),
            period: Period::Day,
            unit: Unit::Usd,
            used: 12.0,
            of: 11.0,
            status: Status::Over,
            on_exceed: OnExceed::Pause,
            from: now - chrono::Duration::hours(1),
            to: now + chrono::Duration::hours(10),
            shared: false,
        };
        d.agent_stand("sarah", "Sarah", vec![over], false);
        assert!(d.agent_held("sarah").is_some());
        let mut withdrawn = false;
        for _ in 0..50 {
            let thread = d.line_thread(s.id, None, None).await.unwrap();
            if thread
                .iter()
                .any(|m| m.id == queued.id && m.delivery == Delivery::Withdrawn)
            {
                withdrawn = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(withdrawn, "what it queued is withdrawn");
    })
    .await;
}

/// What may change an agent's own settings is told however it is written:
/// what Act fully allows at once must never be one of them. Reading them,
/// and every ordinary command, goes.
#[test]
fn a_command_that_may_change_an_agents_settings_is_told_however_it_is_written() {
    let home = std::path::Path::new("/h");
    let ws = std::path::Path::new("/h/.workforce/agents/desk");
    let settings = vec![
        ws.join("config.toml"),
        ws.join("heartbeats"),
        ws.join(".local"),
    ];
    let elsewhere = std::path::Path::new("/h/dev/acme");
    let may = |c: &str| command_may_touch_settings(c, elsewhere, &settings, home);
    for c in [
        "echo x >> /h/.workforce/agents/desk/config.toml",
        "sed -i s/a/b/ ~/.workforce/agents/desk/config.toml",
        "sed -i s/a/b/ $HOME/.workforce/agents/desk/config.toml",
        "sed -i s/a/b/ ${HOME}/.workforce//agents/desk/./config.toml",
        "cd /h/.workforce/agents/desk && sed -i s/a/b/ config.toml",
        "M=/h/.workforce/agents/desk; echo x >> $M/config.toml",
        "sed -i s/a/b/ $W/config.toml",
        "rm ../../.workforce/agents/*/config.toml",
        "cd ~ && sed -i s/x/y/ .workforce/agents/desk/config.toml",
        "cd /h/.workforce/agents && sed -i s/x/y/ desk/config.toml",
        "find /h/.workforce -name config.toml -exec sed -i s/x/y/ {} +",
        "cp -r /tmp/w/. /h/.workforce/",
        "cd /h/dev && tee ../.workforce/agents/desk/config.toml < x",
        "python3 -c 'open(\"/h/.workforce/agents/desk/config.toml\",\"w\")'",
    ] {
        assert!(may(c), "{c}");
    }
    assert!(
        command_may_touch_settings("sed -i s/a/b/ config.toml", ws, &settings, home),
        "run inside a workspace"
    );
    assert!(
        command_may_touch_settings(
            "sed -i s/x/y/ .workforce/agents/desk/config.toml",
            home,
            &settings,
            home
        ),
        "a relative path from home"
    );
    for c in [
        // The person's own words that day, and ordinary work.
        "export PATH=$HOME/.local/bin:$PATH; cargo test",
        "M=/x/memory; cat $M/a.md | head -80",
        "cat /h/.workforce/agents/desk/config.toml",
        "M=/h/.workforce/agents/desk; sed -n 1,5p $M/config.toml",
        "grep -r budget ~/.workforce/agents/desk/ 2>/dev/null | head",
        "cat config.toml",
        "cargo test -p acme 2>&1 | tail -20",
        "gh pr view 551",
        "cat $REPO/AGENTS.md",
        "ls ~/dev",
        "cd ~ && ls",
    ] {
        assert!(!may(c), "{c}");
    }
}
