//! Charges end to end in the desk: taking a session on and letting it go,
//! the person taking the wheel, and the boss's messages waiting for it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, desk_on, new_session, options, went_in};
use super::*;
use crate::computer::boss::PushKind;
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
    went_in(d, id, &next).await;
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
        // Theirs overtook the boss's "Run the tests next.", decided before
        // it: that one never goes in, not even once handed back (boss one
        // picture, point 4). Before, it went in on hand-back. A boss's
        // message after their words, which nothing overtook, still waits
        // for the wheel and goes in then.
        let by = By::manager("sarah", "Sarah", "Next step.");
        let after = d
            .line_send(
                Sender::Overseer(by),
                Send::Tell {
                    to: Target::Session(s.id),
                    text: "Then commit it.".into(),
                    when: When::Queue,
                },
            )
            .await
            .unwrap();

        // Written down once, as Activity says.
        d.charge_settle().await;
        assert_eq!(d.charge_of(s.id).unwrap().wheel, Wheel::Person);

        // The plugin, waiting for a message, gets the one nothing
        // overtook once handed back; the overtaken one is withdrawn.
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
        assert_eq!(next["id"], json!(after.id));
        assert!(next["text"]
            .as_str()
            .unwrap()
            .contains("tells you: Then commit it."));
        assert_eq!(
            d.0.store
                .line_message(m.id)
                .await
                .unwrap()
                .unwrap()
                .delivery,
            Delivery::Withdrawn
        );
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
        model: None,
        account: None,
        env: Default::default(),
        group: group.map(Into::into),
        review_of: None,
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

/// A session Sarah runs whose Desk plugin isn't heard from: what waits for
/// it is typed in at its prompt. Taken on an hour ago, so input since is
/// the person's.
async fn typed_into(d: &Desk, dir: &std::path::Path) -> Uuid {
    let s = new_session(d, dir).await;
    hook(
        d,
        s.id,
        json!({"hook_event_name": "SessionStart", "session_id": s.id.to_string()}),
    )
    .await;
    d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
        .await
        .unwrap();
    d.0.state.lock().charges.rows.get_mut(&s.id).unwrap().since =
        Utc::now() - chrono::Duration::hours(1);
    s.id
}

async fn waiting(d: &Desk, id: Uuid, row: Uuid) -> (Delivery, Option<Wait>) {
    let thread = d.line_thread(id, None, None).await.unwrap();
    let m = thread.iter().find(|m| m.id == row).unwrap();
    (m.delivery, m.waiting)
}

async fn until_in(d: &Desk, id: Uuid, row: Uuid) {
    let end = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match waiting(d, id, row).await.0 {
            Delivery::Delivered | Delivery::Handed => return,
            Delivery::Queued if tokio::time::Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            other => panic!("it never went in: {other:?}"),
        }
    }
}

/// The person's keystroke holds the boss's message only within the
/// boss's hold, and says when that is up: then it goes in, with no prompt
/// of theirs to free it (MAN-835 waited on a stray byte until one).
#[tokio::test]
async fn a_keystroke_holds_the_bosss_message_only_within_the_hold() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = typed_into(&d, dir.path()).await;
        let hold = chrono::Duration::milliseconds(1500);
        d.set_holds([("sarah".to_string(), hold)].into());
        let pty = d.attach(id).unwrap();
        let (viewer, _, _) = pty.attach();
        viewer.claim();
        viewer.input(b"hm").unwrap();
        let k = d.line_keystroke(id).expect("typed within the hold");
        assert_eq!(k.ends - k.at, hold);
        let by = By::manager("sarah", "Sarah", "Next.");
        let m = d
            .line_send(Sender::Overseer(by), boss_tell("Push it.", id))
            .await
            .unwrap();
        assert_eq!(
            waiting(&d, id, m.id).await,
            (Delivery::Queued, Some(Wait::Typing))
        );
        until_in(&d, id, m.id).await;
        assert_eq!(d.line_keystroke(id), None, "its hold is up");
    })
    .await;
}

/// The person's draft, put in its prompt by Desk, is their input for the
/// wheel and holds what Desk would type in as a draft, never as typing,
/// with no end but the next turn's start; after it, the boss's messages go
/// in.
#[tokio::test]
async fn a_draft_holds_the_bosss_message_until_the_next_turn_starts() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = typed_into(&d, dir.path()).await;
        assert_eq!(d.wheel_of(id), Some(Wheel::Boss));
        d.line_draft(id, "About your questions:").unwrap();
        assert_eq!(d.wheel_of(id), Some(Wheel::Person), "a draft is input");
        assert_eq!(d.line_keystroke(id), None);
        let by = By::manager("sarah", "Sarah", "Next.");
        let m = d
            .line_send(Sender::Overseer(by), boss_tell("Push it.", id))
            .await
            .unwrap();
        assert_eq!(
            waiting(&d, id, m.id).await,
            (Delivery::Queued, Some(Wait::Draft))
        );
        // Handed back, the draft still holds it: it has no hold to end.
        d.hand_back(id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            waiting(&d, id, m.id).await,
            (Delivery::Queued, Some(Wait::Draft))
        );
        // They send it: the turn takes the draft (and overtakes what was
        // decided before it), and once it ends and the wheel is back the
        // boss's next message goes in.
        hook(
            &d,
            id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "About your questions: go on"}),
        )
        .await;
        assert_ne!(waiting(&d, id, m.id).await.1, Some(Wait::Draft));
        hook(
            &d,
            id,
            json!({"hook_event_name": "Stop", "last_assistant_message": "Done."}),
        )
        .await;
        d.hand_back(id).await.unwrap();
        let by = By::manager("sarah", "Sarah", "Next.");
        let next = d
            .line_send(Sender::Overseer(by), boss_tell("Then commit it.", id))
            .await
            .unwrap();
        until_in(&d, id, next.id).await;
    })
    .await;
}

/// A desk whose sessions say what they did, then show a prompt their
/// terminal echoes what it is sent at: a draft shows there.
async fn prompting_desk(pool: workforce_postgres::PostgresPool, dir: &std::path::Path) -> Desk {
    let cli = dir.join("prompting-cli");
    workforce_common::executable::write(
        &cli,
        "#!/bin/sh\ni=0\nwhile [ $i -lt 30 ]; do echo \"step $i of the build is done, ok\"; i=$((i+1)); done\nprintf '> '\nexec sleep 60\n",
    )
    .unwrap();
    super::super::tests::logged_desk(pool, dir, cli).await.0
}

/// A draft put in the prompt of a session Sarah runs, her hold short, and
/// her message it holds.
async fn drafted(d: &Desk, dir: &std::path::Path) -> (Uuid, Uuid) {
    let id = typed_into(d, dir).await;
    for _ in 0..100 {
        if d.terminal_text(id, 4000).unwrap().contains("step 29") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    d.set_holds([("sarah".to_string(), chrono::Duration::milliseconds(300))].into());
    d.line_draft(id, "About your questions: keep the old table")
        .unwrap();
    let by = By::manager("sarah", "Sarah", "Next.");
    let m = d
        .line_send(Sender::Overseer(by), boss_tell("Push it.", id))
        .await
        .unwrap();
    for _ in 0..100 {
        if d.terminal_text(id, 4000)
            .unwrap()
            .contains("keep the old table")
        {
            return (id, m.id);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the draft never showed");
}

/// Past the hold, a draft its screen still shows holds on: time alone
/// never lets the boss's message in over it.
#[tokio::test]
async fn a_draft_still_shown_past_the_hold_holds_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = prompting_desk(pool, dir.path()).await;
        let (id, m) = drafted(&d, dir.path()).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            waiting(&d, id, m).await,
            (Delivery::Queued, Some(Wait::Draft))
        );
    })
    .await;
}

/// A draft the person clears by hand (Ctrl-U erases it from the screen)
/// holds no more: the boss's message goes in with no turn started.
#[tokio::test]
async fn a_draft_cleared_by_hand_lets_the_bosss_message_in() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = prompting_desk(pool, dir.path()).await;
        let (id, m) = drafted(&d, dir.path()).await;
        let pty = d.attach(id).unwrap();
        let (viewer, _, _) = pty.attach();
        viewer.claim();
        viewer.input(b"\x15").unwrap();
        until_in(&d, id, m).await;
        assert_eq!(pty.draft_at(), None);
    })
    .await;
}

/// A short draft over two lines holds while its prompt's rows show it, and
/// once the person clears it the boss's message goes in. Its stand-in
/// draws the draft in its prompt as a TUI does, and clears it on Ctrl-U.
#[tokio::test]
async fn a_short_draft_over_two_lines_holds_until_it_is_cleared() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("composer-cli");
        workforce_common::executable::write(
            &cli,
            "#!/bin/sh\nstty raw -echo\ni=0\n\
             while [ $i -lt 30 ]; do printf 'step %s of the build is done, ok\\r\\n' $i; i=$((i+1)); done\n\
             printf '> '\ndrawn=\n\
             while :; do\n\
             c=$(dd bs=1 count=1 2>/dev/null | od -An -tx1 | tr -d ' \\n')\n\
             [ -z \"$c\" ] && exit 0\n\
             if [ \"$c\" = 15 ]; then printf '\\r\\033[2K\\033[1A\\r\\033[2K> '; drawn=\n\
             elif [ -z \"$drawn\" ]; then printf 'Yes.\\r\\n  Go ahead'; drawn=1; fi\n\
             done\n",
        )
        .unwrap();
        let d = super::super::tests::logged_desk(pool, dir.path(), cli).await.0;
        let id = typed_into(&d, dir.path()).await;
        d.set_holds([("sarah".to_string(), chrono::Duration::milliseconds(300))].into());
        let shown = async |text: &str| {
            for _ in 0..100 {
                if d.terminal_text(id, 4000).unwrap().contains(text) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("{text:?} never showed");
        };
        shown("step 29").await;
        d.line_draft(id, "Yes.\nGo ahead").unwrap();
        shown("Go ahead").await;
        let by = By::manager("sarah", "Sarah", "Next.");
        let m = d
            .line_send(Sender::Overseer(by), boss_tell("Push it.", id))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            waiting(&d, id, m.id).await,
            (Delivery::Queued, Some(Wait::Draft))
        );
        let pty = d.attach(id).unwrap();
        let (viewer, _, _) = pty.attach();
        viewer.claim();
        viewer.input(b"\x15").unwrap();
        until_in(&d, id, m.id).await;
    })
    .await;
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

/// Taken back by the person, a session gets nothing from any boss: no
/// message, no interrupt. Desk keeps who let it go across a restart and
/// shows it, until it is handed to a boss again.
#[tokio::test]
async fn a_session_taken_back_is_left_alone_by_every_boss_and_shown_as_the_persons() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.release(s.id, &By::you()).await.unwrap();
        assert!(d.taken_back(s.id));
        let by = By::manager("sarah", "Sarah", "PR 41 changed.");
        let refused = d
            .line_send(Sender::Overseer(by.clone()), boss_tell("Rebase it.", s.id))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("took"), "{refused}");
        let interrupted = d.interrupt(s.id, Some(by.clone())).unwrap_err();
        assert!(
            interrupted.to_string().contains("leave it alone"),
            "{interrupted}"
        );
        let shown = |d: &Desk| {
            d.charges_shown()
                .into_iter()
                .find(|c| c.session == s.id)
                .map(|c| serde_json::to_value(c).unwrap())
        };
        let v = shown(&d).expect("shown though let go");
        assert_eq!(v["releasedBy"], "person", "{v}");
        assert!(v["releasedAt"].is_string(), "{v}");
        assert!(d.charges().iter().all(|c| c.session != s.id), "not live");
        // Kept as it is stored, so a restart reads it again.
        let kept = d.store().charge(s.id).await.unwrap().unwrap();
        assert_eq!(kept.released_by, Some(crate::charge::ReleasedBy::Person));
        // Handed to a boss again, it is its boss's as before.
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        assert!(!d.taken_back(s.id));
        assert_eq!(shown(&d).unwrap()["releasedBy"], Value::Null);
        d.line_send(Sender::Overseer(by), boss_tell("Rebase it.", s.id))
            .await
            .unwrap();
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
                .line_requeue(None, Some(Utc::now() + chrono::Duration::seconds(1)))
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
/// same idle, and the boss's, overtaken by it, is withdrawn.
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
        assert!(d
            .line_next(s.id, Duration::from_millis(300))
            .await
            .is_none());
        d.line_pump(s.id).await;
        // Their words overtook the boss's, decided before them: it is
        // withdrawn, never typed in after (boss one picture, point 4), and
        // the boss decides again on what they said. Before, it kept
        // waiting and went in next.
        assert_eq!(
            d.0.store
                .line_message(boss.id)
                .await
                .unwrap()
                .unwrap()
                .delivery,
            Delivery::Withdrawn
        );
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

/// Where a bare push goes is what git's `@{push}` says, only while its
/// config says it plainly.
#[test]
fn a_bare_push_goes_where_git_says_only_while_its_config_is_plain() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (own, origin) = (base.join("acme-web"), base.join("origin.git"));
    std::fs::create_dir_all(&own).unwrap();
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare"]);
    git(&own, &["init", "-q", "-b", "feat/x"]);
    git(
        &own,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    );
    assert_eq!(push_to(&own), None, "no upstream: git can't say");
    git(
        &own,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ],
    );
    git(&own, &["push", "-q", "-u", "origin", "feat/x"]);
    assert_eq!(push_to(&own).as_deref(), Some("origin/feat/x"));
    git(&own, &["config", "push.default", "matching"]);
    assert_eq!(push_to(&own), None, "matching pushes every branch");
    git(&own, &["config", "push.default", "simple"]);
    git(
        &own,
        &["config", "remote.origin.push", "refs/heads/*:refs/heads/*"],
    );
    assert_eq!(push_to(&own), None, "a remote's own refspecs");
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
        assert_eq!(d.push_kind(s.id, a, "sarah"), Some(PushKind::Unknown));
        d.decide(s.id, a, Decision::Deny, Some(boss.clone()))
            .unwrap();
        elsewhere.await.unwrap();

        let here = ask(&own);
        let a = open().await;
        assert_eq!(d.boss_answerable(s.id, a, "sarah"), Ok(()));
        assert_eq!(d.push_kind(s.id, a, "sarah"), Some(PushKind::OwnBranch));
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
        "env cp /tmp/x /h/.workforce/agents/desk/config.toml",
        "env sed -i s/a/b/ /h/.workforce/agents/desk/config.toml",
        "sed -Ei s/a/b/ /h/.workforce/agents/desk/config.toml",
        "sed --in-place=.bak s/a/b/ /h/.workforce/agents/desk/config.toml",
        "sed -n 's/a/b/w /h/.workforce/agents/desk/config.toml' /tmp/x",
        "awk -i inplace 1 /h/.workforce/agents/desk/config.toml",
        "yq -i .a=1 /h/.workforce/agents/desk/config.toml",
        "sort -o /h/.workforce/agents/desk/config.toml /tmp/x",
        "sed -n '1,$w /h/.workforce/agents/desk/config.toml' /tmp/x",
        "sed -n '/x/!W /h/.workforce/agents/desk/config.toml' /tmp/x",
        "sed 'e cp /tmp/x /h/.workforce/agents/desk/config.toml' /tmp/y",
        "tree -o /h/.workforce/agents/desk/config.toml",
        "sed -n '1,$w/h/.workforce/agents/desk/config.toml' /tmp/x",
        "sed -n --expression='1,$w /h/.workforce/agents/desk/config.toml' /tmp/x",
        "find /h/.workforce -fprintf /h/.workforce/agents/desk/config.toml x",
        "sed -i s/a/b/ /h/.work\"force\"/agents/desk/config.toml",
        "sed -i s/a/b/ /h/.work\\force/agents/desk/config.toml",
        "D=/h/.work; cd ${D}force/agents/desk && sed -i x config.toml",
        "cd /h/.workf*/agents/desk && sed -i x config.toml",
        "{\"path\":\"/h/.workforce/agents/desk/config.toml\",\"content\":\"x\"}",
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
        "sed -n '/window/p' /h/.workforce/agents/desk/config.toml",
        "sed -n 1,5p /h/.workforce/agents/desk/config.toml",
        "ls ~/dev",
        "cd ~ && ls",
    ] {
        assert!(!may(c), "{c}");
    }
}

/// A command is read where it runs: a Devin `exec` whose `workdir` is an
/// agent's settings may change them, though its words name no path there;
/// where it runs not known, any write counts.
#[test]
fn a_command_is_read_for_settings_where_it_runs() {
    let home = std::path::Path::new("/h");
    let ws = std::path::Path::new("/h/.workforce/agents/desk");
    let settings = vec![ws.join("config.toml")];
    let folder = std::path::Path::new("/h/dev/acme");
    let write = "echo x > config.toml";
    assert!(!command_at_may_touch_settings(
        write,
        Some(folder),
        folder,
        &settings,
        home
    ));
    assert!(command_at_may_touch_settings(
        write,
        Some(ws),
        folder,
        &settings,
        home
    ));
    assert!(command_at_may_touch_settings(
        write, None, folder, &settings, home
    ));
    assert!(!command_at_may_touch_settings(
        "ls", None, folder, &settings, home
    ));
    // As a real Devin `exec` reaches it: its `workdir` is where it runs.
    let mut p = serde_json::json!({"hook_event_name": "PermissionRequest", "tool_name": "exec",
        "tool_input": {"command": write, "workdir": ws.to_str().unwrap()}});
    crate::devin::normalize(&mut p, None);
    assert_eq!(p["tool_name"], "Bash");
    let runs_in = std::path::PathBuf::from(p["cwd"].as_str().unwrap());
    assert!(command_at_may_touch_settings(
        p["tool_input"]["command"].as_str().unwrap(),
        Some(&runs_in),
        folder,
        &settings,
        home
    ));
}

#[tokio::test]
async fn a_boss_ends_what_it_started_or_was_handed_only_while_it_runs_it_or_let_it_go_as_done() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let boss = By::manager("sarah", "Sarah", "its work is done");
        let mia = Party::manager("mia", "Mia");
        let charged = |how| {
            let d = d.clone();
            let dir = dir.path().to_path_buf();
            async move {
                let s = new_session(&d, &dir).await;
                d.take_on(s.id, &sarah(), how, &Default::default(), &By::you())
                    .await
                    .unwrap();
                s.id
            }
        };
        let mut ends = Vec::new();
        for how in [How::Started, How::Handed] {
            let done = charged(how).await;
            d.release_done(done, &boss).await.unwrap();
            let by_desk = charged(how).await;
            d.release(
                by_desk,
                &By::manager("sarah", "Sarah", "its group has a boss"),
            )
            .await
            .unwrap();
            let by_person = charged(how).await;
            d.release(by_person, &By::you()).await.unwrap();
            let passed_on = charged(how).await;
            d.take_on(passed_on, &mia, how, &Default::default(), &By::you())
                .await
                .unwrap();
            let running = charged(how).await;
            ends.extend([
                (done, true),
                (by_desk, false),
                (by_person, false),
                (passed_on, false),
                (running, true),
            ]);
        }
        let taken = charged(How::Taken).await;
        d.release_done(taken, &boss).await.unwrap();
        ends.push((taken, false));
        let legacy = charged(How::Started).await;
        d.release(legacy, &boss).await.unwrap();
        let mut row = d.store().charge(legacy).await.unwrap().unwrap();
        row.released_by = None;
        d.store().put_charge(&row).await.unwrap();
        ends.push((legacy, false));
        assert!(d.charge_load().await, "read again, as after a restart");
        for (id, ok) in &ends {
            assert_eq!(d.may_end(*id, Some(&boss)), *ok, "{id}");
        }
        let (done, _) = ends[0];
        d.end(done, Some(boss.clone())).await.unwrap();
        assert_eq!(d.view(done).unwrap().state, SessionState::Closed);
        let (by_desk, _) = ends[1];
        assert!(matches!(
            d.end(by_desk, Some(boss)).await,
            Err(DeskError::Conflict(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn an_unknown_release_reason_is_the_bosss_and_an_unreadable_charge_fails_the_load() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let boss = By::manager("sarah", "Sarah", "its work is done");
        let mut ids = Vec::new();
        for _ in 0..3 {
            let s = new_session(&d, dir.path()).await;
            d.take_on(
                s.id,
                &sarah(),
                How::Started,
                &Default::default(),
                &By::you(),
            )
            .await
            .unwrap();
            d.release_done(s.id, &boss).await.unwrap();
            ids.push(s.id);
        }
        let client = d.store().pool().get().await.unwrap();
        client
            .execute(
                "UPDATE charges SET released_by = 'from-a-newer-desk' WHERE session = $1",
                &[&ids[0]],
            )
            .await
            .unwrap();
        let read = d.store().charges().await.unwrap();
        let unknown = read.iter().find(|c| c.session == ids[0]).unwrap();
        assert_eq!(unknown.released_by, Some(crate::charge::ReleasedBy::Boss));
        assert!(d.charge_load().await, "the load goes on");
        assert!(!d.may_end(ids[0], Some(&boss)), "unknown reason: refused");
        assert!(d.may_end(ids[2], Some(&boss)), "the row it can read stands");
        client
            .execute(
                "UPDATE charges SET how = 'garbled' WHERE session = $1",
                &[&ids[1]],
            )
            .await
            .unwrap();
        drop(client);
        assert!(d.store().charges().await.is_err(), "no row is skipped");
        assert!(!d.charge_load().await, "the load fails, as it would unread");
    })
    .await;
}

/// A boss acting fully cancels the requests it decides itself to close a
/// session, as its own answer; never one left to the person, nor on a
/// session it doesn't run, nor below Act fully.
#[tokio::test]
async fn a_boss_acting_fully_cancels_only_the_requests_it_decides() {
    use crate::model::Decision;
    use crate::open_item::{approval_key, NewItem, OpenKind};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.boss_bounds_with(crate::charge::Bounds {
            home: dir.path().to_path_buf(),
            ..Default::default()
        });
        let boss = By::manager("sarah", "Sarah", "Its ticket is done.");
        let refused = |r: Result<Vec<Uuid>, DeskError>| match r {
            Err(DeskError::Conflict(why) | DeskError::Invalid(why)) => why,
            other => panic!("not refused: {other:?}"),
        };
        let build = ask_permission(&d, s.id, "cargo build");
        let a = asked(&d, s.id, "cargo build").await;
        assert_eq!(
            refused(d.cancel_boss_pending(s.id, &boss).await),
            "only a boss acting fully cancels a request to close a session"
        );
        let fully = crate::charge::FullGrant {
            anywhere: true,
            ..Default::default()
        };
        d.set_full(HashMap::from([
            ("sarah".to_string(), fully.clone()),
            ("mia".to_string(), fully),
        ]));
        assert!(refused(d.cancel_boss_pending(s.id, &By::you()).await).contains("only a boss"));
        let mia = By::manager("mia", "Mia", "Mine now.");
        assert_eq!(
            refused(d.cancel_boss_pending(s.id, &mia).await),
            "it isn't a session this boss runs"
        );

        // Left to the person: theirs, however the boss closes it.
        let mut left = NewItem::ask("sarah", OpenKind::Ask, Some(s.id), "It waits on you.");
        left.dedupe_key = approval_key(&s.id.to_string(), &a.to_string());
        d.store().item_open(&left).await.unwrap();
        assert_eq!(
            refused(d.cancel_boss_pending(s.id, &boss).await),
            "its request was left to the person; it's theirs to answer"
        );
        d.decide(s.id, a, Decision::Deny, Some(By::you())).unwrap();
        build.await.unwrap();

        // One it decides: denied as its answer, and nothing waits after.
        let tests = ask_permission(&d, s.id, "cargo test");
        let a = asked(&d, s.id, "cargo test").await;
        assert_eq!(d.cancel_boss_pending(s.id, &boss).await.unwrap(), [a]);
        assert_eq!(
            tests.await.unwrap()["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
        assert_eq!(
            d.cancel_boss_pending(s.id, &boss).await.unwrap(),
            Vec::<Uuid>::new()
        );
    })
    .await;
}

/// `base/web`, a repository two commits deep: git run in it, and its
/// first commit, the head its pull request is reviewed at.
fn reviewed_repo(base: &std::path::Path) -> (std::path::PathBuf, String) {
    let repo = base.join("web");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "-q", "-b", "main"]);
    git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let head = git_in(&repo, &["rev-parse", "HEAD"]);
    git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "two"]);
    (repo, head)
}

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=d@e", "-c", "user.name=d"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn review_of(head: &str, for_session: Option<Uuid>) -> crate::plan::ReviewOf {
    crate::plan::ReviewOf {
        pr: crate::plan::PrRef::new("acme", "web", 7),
        head_sha: head.into(),
        for_session,
    }
}

/// A review session is asked for its verdict at the head it reviews, and
/// linked to its pull request for the session whose work it is; one named
/// by a short head is refused.
#[tokio::test]
async fn a_review_session_is_asked_for_its_verdict_and_linked_to_its_pull_request() {
    use crate::plan::{review_instruction, ReviewLink};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (repo, head) = reviewed_repo(&base);
        let d = desk(pool, &base).await;
        let by = || By::manager("sarah", "Sarah", "Review it.");
        let mut new = start_in(&repo, None);
        new.prompt = Some("Review pull request 7.".into());
        let work = Uuid::new_v4();
        new.review_of = Some(review_of(&head, Some(work)));
        let s = d.create_for(new, &sarah(), by()).await.unwrap();
        assert_eq!(
            d.view(s.id).unwrap().last_prompt,
            Some(format!(
                "Review pull request 7.\n\n{}",
                review_instruction(&head)
            ))
        );
        assert_eq!(
            d.store().pr_review_of(s.id).await.unwrap(),
            Some(ReviewLink {
                session: s.id,
                boss: sarah().id,
                pr: crate::plan::PrRef::new("acme", "web", 7),
                head_sha: Some(head.clone()),
                for_session: Some(work),
                settled: false,
            })
        );

        let mut plain = start_in(&repo, None);
        plain.prompt = Some("Fix the date picker.".into());
        let p = d.create_for(plain, &sarah(), by()).await.unwrap();
        assert_eq!(
            d.view(p.id).unwrap().last_prompt.as_deref(),
            Some("Fix the date picker.")
        );
        assert_eq!(d.store().pr_review_of(p.id).await.unwrap(), None);

        let mut short = start_in(&repo, None);
        short.review_of = Some(review_of(&head[..12], None));
        assert!(matches!(
            d.create_for(short, &sarah(), by()).await,
            Err(DeskError::Invalid(_))
        ));
    })
    .await;
}

/// A review is read in a worktree of its own, detached at the head it
/// reviews and named for it, whatever worktree its boss names, a ticket's
/// own among them; a second review of that head is refused while the
/// first is open, and may start once it is closed.
#[tokio::test]
async fn a_review_reads_its_head_in_a_detached_worktree_and_one_head_has_one_open_review() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (repo, head) = reviewed_repo(&base);
        let d = desk(pool, &base).await;
        let by = || By::manager("sarah", "Sarah", "Review it.");
        let mut ticket = start_in(&repo, None);
        ticket.worktree = Some("des-1".into());
        let ticket = d.create_for(ticket, &sarah(), by()).await.unwrap();
        let review = || {
            let mut new = start_in(&repo, None);
            new.worktree = Some("des-1".into());
            new.review_of = Some(review_of(&head, Some(ticket.id)));
            new
        };
        let s = d.create_for(review(), &sarah(), by()).await.unwrap();
        let folder = repo.join(format!(".worktrees/review-7-{}", &head[..7]));
        assert_eq!(s.cwd, folder.to_string_lossy(), "never the ticket's own");
        assert_eq!(s.branch, None);
        assert_eq!(git_in(&folder, &["rev-parse", "HEAD"]), head);
        assert_eq!(git_in(&folder, &["branch", "--show-current"]), "");

        let again = d.create_for(review(), &sarah(), by()).await.unwrap_err();
        assert!(
            matches!(&again, DeskError::Conflict(why) if why.contains("at that head already")),
            "{again:?}"
        );
        d.end(s.id, Some(by())).await.unwrap();
        let next = d.create_for(review(), &sarah(), by()).await.unwrap();
        assert_eq!(next.cwd, s.cwd, "its worktree found again at the head");
    })
    .await;
}

/// A review whose session doesn't start leaves nothing behind: no link,
/// and no worktree it made; one it found there already stays.
#[tokio::test]
async fn a_review_that_doesnt_start_leaves_no_link_and_no_worktree_it_made() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (repo, head) = reviewed_repo(&base);
        crate::store::DeskStore::migrate(&pool).await.unwrap();
        let mut opts = super::super::tests::options(&base);
        let nowhere = base.join("no-such-claude");
        opts.programs = crate::testing::programs(nowhere.clone(), nowhere);
        let d = Desk::start(
            crate::store::DeskStore::from_pool(pool),
            opts,
            Default::default(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let by = || By::manager("sarah", "Sarah", "Review it.");
        let review = || {
            let mut new = start_in(&repo, None);
            new.review_of = Some(review_of(&head, None));
            new
        };
        let folder = repo.join(format!(".worktrees/review-7-{}", &head[..7]));
        d.create_for(review(), &sarah(), by()).await.unwrap_err();
        assert!(!folder.exists(), "the worktree it made is removed");
        assert!(d
            .store()
            .pr_reviews_at(
                &sarah().id,
                &crate::plan::PrRef::new("acme", "web", 7),
                &head
            )
            .await
            .unwrap()
            .is_empty());

        git_in(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &folder.to_string_lossy(),
                &head,
            ],
        );
        d.create_for(review(), &sarah(), by()).await.unwrap_err();
        assert!(folder.exists(), "one it found stays");
    })
    .await;
}

/// A session's own merge is refused before it runs while a boss acting
/// fully has it, in a mode that asks nothing too, and whatever else the
/// command runs; not while the person is in it, nor under a boss that
/// doesn't act fully, nor what only reads.
#[tokio::test]
async fn a_sessions_own_merge_is_refused_before_it_runs_under_a_boss_acting_fully() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let call = |command: &str| {
            json!({"hook_event_name": "PreToolUse", "permission_mode": "bypassPermissions",
                "tool_name": "Bash", "tool_use_id": "toolu_1", "tool_input": {"command": command}})
        };
        let refused = |answer: &Value| answer["hookSpecificOutput"]["permissionDecision"] == "deny";
        let s = d.create(start_in(dir.path(), None), None).await.unwrap();
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        let merge = "git push && gh pr merge 7 --squash";
        assert!(
            !refused(&hook(&d, s.id, call(merge)).await),
            "she doesn't act fully"
        );

        d.set_full(HashMap::from([(
            sarah().id,
            charge::FullGrant {
                anywhere: true,
                unbound: true,
                ..Default::default()
            },
        )]));
        let answer = hook(&d, s.id, call(merge)).await;
        assert!(refused(&answer), "{answer}");
        let why = answer["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap_or_default();
        assert!(
            why.contains(crate::plan::OWN_MERGE) && why.contains(crate::plan::OWN_MERGE_INSTEAD),
            "{why}"
        );
        assert!(!refused(&hook(&d, s.id, call("gh pr view 7")).await));
        let token = d.0.state.lock().live[&s.id].token.clone();
        assert_eq!(
            d.gh_asked(s.id, &token, "gh 'pr' 'merge' '7'").await,
            Some(Some(crate::plan::own_merge_refused())),
            "a Codex session's gh is told the same"
        );
        assert_eq!(
            d.gh_asked(s.id, "not-its-token", "gh pr merge 7").await,
            None
        );

        d.person_typing(s.id).unwrap();
        assert!(
            !refused(&hook(&d, s.id, call(merge)).await),
            "the person is in it"
        );
    })
    .await;
}

/// `base/name` on `feat/x` off `main`, its `origin` at `url`; `origin`
/// has `main` already, as its `origin/HEAD`, when `published`.
fn pushing_repo(
    base: &std::path::Path,
    name: &str,
    url: &str,
    published: bool,
) -> std::path::PathBuf {
    let repo = base.join(name);
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "-q", "-b", "main"]);
    git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git_in(&repo, &["remote", "add", "origin", url]);
    if published {
        git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_in(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
    }
    git_in(&repo, &["switch", "-q", "-c", "feat/x"]);
    repo
}

/// A `gh` under `base` whose `gh api <endpoint>` prints what `answers`
/// gives that endpoint and fails for any other, each endpoint it is asked
/// written to `base`'s `asked`; the folder it is in.
fn gh_answering(base: &std::path::Path, answers: &[(&str, &str)]) -> std::path::PathBuf {
    let tools = base.join("tools");
    let said = base.join("github");
    std::fs::create_dir_all(&said).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    for (endpoint, answer) in answers {
        std::fs::write(said.join(endpoint.replace('/', "_")), answer).unwrap();
    }
    workforce_common::executable::write(
        &tools.join("gh"),
        format!(
            "#!/bin/sh\necho \"$2\" >> '{}'\nf='{}'/$(printf %s \"$2\" | tr / _)\n[ \"$1\" = api ] && [ -f \"$f\" ] && exec cat \"$f\"\nexit 1\n",
            base.join("asked").display(),
            said.display()
        ),
    )
    .unwrap();
    tools
}

/// Under a boss acting fully, a push straight to a default branch GitHub
/// requires a pull request for (by a ruleset or its classic protection)
/// is refused before it runs, in a mode that asks nothing, GitHub asked
/// once for a while; one GitHub doesn't protect so, or can't say about, a
/// release branch, a repository that isn't on GitHub, or its first push
/// isn't, nor anything while the person is in the session.
#[tokio::test]
async fn a_push_straight_to_a_default_branch_github_protects_is_refused_before_it_runs() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let tools = gh_answering(
            &base,
            &[
                (
                    "repos/acme/web/rules/branches/main",
                    r#"[{"type":"deletion"},{"type":"pull_request","parameters":{}}]"#,
                ),
                ("repos/acme/classic/rules/branches/main", "[]"),
                (
                    "repos/acme/classic/branches/main/protection",
                    r#"{"required_pull_request_reviews":{"required_approving_review_count":1}}"#,
                ),
                ("repos/acme/checked/rules/branches/main", "[]"),
                (
                    "repos/acme/checked/branches/main/protection",
                    r#"{"required_status_checks":{"strict":true,"contexts":["ci"]}}"#,
                ),
                ("repos/acme/open/rules/branches/main", "[]"),
            ],
        );
        let mut o = options(&base);
        o.env
            .insert("PATH".into(), format!("{}:/usr/bin:/bin", tools.display()));
        let d = desk_on(pool, o).await;
        d.boss_bounds_with(crate::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        d.set_full(HashMap::from([(
            sarah().id,
            charge::FullGrant {
                anywhere: true,
                unbound: true,
                ..Default::default()
            },
        )]));
        let session = async |repo: &std::path::Path| {
            let s = d.create(start_in(repo, None), None).await.unwrap();
            idle(&d, s.id).await;
            d.take_on(s.id, &sarah(), How::Handed, &Default::default(), &By::you())
                .await
                .unwrap();
            s.id
        };
        let refused = async |id: Uuid, command: &str| {
            let call = json!({"hook_event_name": "PreToolUse", "permission_mode": "bypassPermissions",
                "tool_name": "Bash", "tool_use_id": "toolu_1", "tool_input": {"command": command}});
            let answer = hook(&d, id, call).await;
            (answer["hookSpecificOutput"]["permissionDecision"] == "deny").then(|| {
                answer["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
        };
        let github = |name: &str| format!("https://github.com/acme/{name}.git");
        let web = session(&pushing_repo(&base, "web", &github("web"), true)).await;
        for _ in 0..2 {
            assert_eq!(
                refused(web, "git push origin HEAD:main").await,
                Some(crate::plan::default_push_refused())
            );
        }
        let asked = std::fs::read_to_string(base.join("asked")).unwrap();
        assert_eq!(
            asked.lines().collect::<Vec<_>>(),
            ["repos/acme/web/rules/branches/main"],
            "GitHub asked once"
        );
        assert_eq!(
            refused(web, "timeout 60 sh -c 'git -C . push origin main'").await,
            Some(crate::plan::default_push_refused()),
            "however it is run"
        );
        assert_eq!(refused(web, "git push origin feat/x").await, None);
        assert_eq!(
            refused(web, "git push origin HEAD:release-2026").await,
            None,
            "a release branch isn't the default one"
        );
        let local = base.join("origin.git").to_string_lossy().into_owned();
        let elsewhere = session(&pushing_repo(&base, "api", &local, true)).await;
        assert_eq!(
            refused(elsewhere, "git push origin HEAD:main").await,
            None,
            "no pull requests where it isn't on GitHub"
        );
        for (name, refusal, what) in [
            ("classic", Some(crate::plan::default_push_refused()), "classic protection"),
            ("checked", None, "protected without pull requests"),
            ("open", None, "unprotected"),
            ("down", None, "GitHub can't say"),
        ] {
            let id = session(&pushing_repo(&base, name, &github(name), true)).await;
            assert_eq!(refused(id, "git push origin HEAD:main").await, refusal, "{what}");
        }
        let fresh = session(&pushing_repo(&base, "new", &github("new"), false)).await;
        assert_eq!(
            refused(fresh, "git push origin HEAD:main").await,
            None,
            "a new repository's first push"
        );

        d.person_typing(web).unwrap();
        assert_eq!(
            refused(web, "git push origin HEAD:main").await,
            None,
            "the person is in it"
        );
    })
    .await;
}

/// An alias is read whole, a value of many lines too, as `-z` gives it.
#[test]
fn a_git_alias_is_read_whole_however_many_lines_it_has() {
    let aliases = super::aliases_of("alias.st\nstatus\0alias.pf\n!f() {\n  git push -f\n}; f\0");
    assert_eq!(aliases["st"], "status");
    assert_eq!(aliases["pf"], "!f() {\n  git push -f\n}; f");
}

/// Git's command list is read a command a line, whatever surrounds it.
#[test]
fn git_says_its_commands_a_line_each() {
    let listed = super::commands_of("add\npush\n  send-pack \n\nlfs\n");
    assert_eq!(
        listed,
        ["add", "push", "send-pack", "lfs"].map(String::from).into()
    );
}

/// Git says its own commands, and those a session's login shell finds,
/// in its folder: a path its profile adds, or a relative one, included.
#[cfg(unix)]
#[test]
fn git_says_its_own_commands_and_those_the_sessions_shell_finds() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let external = |dir: &std::path::Path, name: &str| {
        std::fs::create_dir_all(dir).unwrap();
        let at = dir.join(name);
        std::fs::write(&at, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    external(&home.path().join("profiled"), "git-profiled");
    external(&work.path().join("bin"), "git-here");
    // Only a login shell reads it.
    std::fs::write(
        home.path().join(".profile"),
        format!("PATH=\"{}/profiled:$PATH\"\n", home.path().display()),
    )
    .unwrap();
    let env = std::collections::BTreeMap::from([
        ("HOME".to_string(), home.path().display().to_string()),
        ("PATH".to_string(), "bin:/usr/bin:/bin".to_string()),
    ]);
    let commands =
        super::git_commands("/bin/sh", work.path(), &env).expect("git says its commands");
    assert!(commands.contains("push") && commands.contains("send-pack"));
    assert!(commands.contains("profiled"), "one its profile's path has");
    assert!(commands.contains("here"), "one a relative path finds there");
    assert_eq!(
        super::git_commands("/nonexistent/sh", work.path(), &env),
        None
    );
}

/// A Codex session a boss starts runs its commands in a plain shell, its
/// `gh` Desk's whatever the profiles say; the person's own in their login
/// shell.
#[tokio::test]
async fn only_a_session_a_boss_starts_runs_its_commands_in_a_plain_shell() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let by = By::manager("sarah", "Sarah", "Acme's web upgrade");
        let started = d
            .create_for(start_in(dir.path(), None), &sarah(), by)
            .await
            .unwrap();
        assert_eq!(
            d.command_shell(started.id),
            crate::codex::CommandShell::Plain
        );
        let own = d.create(start_in(dir.path(), None), None).await.unwrap();
        assert_eq!(d.command_shell(own.id), crate::codex::CommandShell::Login);
    })
    .await;
}

/// Where Desk's environment isn't the login shell's, a session a boss
/// starts keeps the login shell, its commands finding the person's tools;
/// what said so is never passed on.
#[tokio::test]
async fn a_login_environment_desk_couldnt_read_keeps_the_login_shell() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut o = options(dir.path());
        o.env
            .insert(super::super::LOGIN_ENV_UNREAD.into(), "1".into());
        let d = desk_on(pool, o).await;
        assert!(!d.login_env().contains_key(super::super::LOGIN_ENV_UNREAD));
        let by = By::manager("sarah", "Sarah", "Acme's web upgrade");
        let started = d
            .create_for(start_in(dir.path(), None), &sarah(), by)
            .await
            .unwrap();
        assert_eq!(
            d.command_shell(started.id),
            crate::codex::CommandShell::Login
        );
    })
    .await;
}
