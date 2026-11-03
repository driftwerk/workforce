//! Charges end to end in the desk: taking a session on and letting it go,
//! the person taking the wheel, and the boss's messages waiting for it.

use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::{desk, new_session};
use super::*;
use crate::line::{Send, Target, Wait, When};
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
            .take_on(s.id, &sarah(), How::Handed, &HashSet::new(), &By::you())
            .await
            .unwrap();
        assert_eq!((c.boss_name.as_str(), c.wheel), ("Sarah", Wheel::Boss));
        // The session is told who runs it with the person.
        let told = take_next(&d, s.id, Duration::from_secs(1)).await.unwrap();
        assert!(
            told["text"]
                .as_str()
                .unwrap()
                .contains("I'm running this session with the person now"),
            "{told}"
        );

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

#[tokio::test]
async fn ill_drive_keeps_the_wheel_and_a_release_keeps_the_row() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        idle(&d, s.id).await;
        d.take_on(s.id, &sarah(), How::Taken, &HashSet::new(), &By::you())
            .await
            .unwrap();
        assert_eq!(d.charges().len(), 1);
        assert_eq!(d.drive(s.id).await.unwrap().wheel, Wheel::PersonPinned);
        d.charge_settle().await;
        assert_eq!(d.wheel_of(s.id), Some(Wheel::PersonPinned));

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
            &HashSet::new()
        ));
        // Handed to it again by the person, it is the boss's again.
        let again = d
            .take_on(s.id, &sarah(), How::Handed, &HashSet::new(), &By::you())
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
            .take_on(s.id, &sarah(), How::Handed, &HashSet::new(), &By::you())
            .await
        {
            Err(DeskError::Conflict(why)) => assert_eq!(why, "It has ended."),
            other => panic!("{other:?}"),
        }
        assert!(d.charge_of(s.id).is_none());
    })
    .await;
}
