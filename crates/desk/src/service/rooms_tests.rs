//! Tests of what rooms hold a message to: what it answers, the steps it
//! carries.

use super::tests::desk;
use super::*;
use crate::rooms::Step;
use crate::store::build_pool;
use workforce_postgres::test_support::with_test_pool;

async fn release(d: &Desk) -> Room {
    d.create_room(NewRoom {
        name: "Atlas launch".into(),
        members: vec![
            Member {
                hub: "dw".into(),
                agent: "allan".into(),
            },
            Member {
                hub: "local".into(),
                agent: "scout".into(),
            },
        ],
        default_member: None,
    })
    .await
    .unwrap()
}

fn said(author: &str, content: &str) -> NewMessage {
    NewMessage {
        author: author.into(),
        content: content.into(),
        ..NewMessage::default()
    }
}

#[tokio::test]
async fn a_post_answers_a_message_of_its_own_room_and_steps_go_on_an_agents_answer() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let room = release(&d).await;
        let other = d
            .create_room(NewRoom {
                name: "acme-web".into(),
                members: vec![],
                default_member: None,
            })
            .await
            .unwrap();
        let ask = d
            .post(room.id, said(YOU, "what's left?"))
            .await
            .unwrap()
            .message;
        let elsewhere = d.post(other.id, said(YOU, "hi")).await.unwrap().message;
        let steps = vec![Step::of("web.search", true, 840)];

        let answer = d
            .post(
                room.id,
                NewMessage {
                    in_reply_to: Some(ask.id),
                    steps: steps.clone(),
                    ..said("dw/allan", "the migration")
                },
            )
            .await
            .unwrap()
            .message;
        assert_eq!((answer.in_reply_to, &answer.steps), (Some(ask.id), &steps));
        assert!(answer.reading.unwrap().for_you);
        assert_eq!(
            d.room_message(room.id, answer.id).await.unwrap().steps,
            steps,
            "kept as posted"
        );

        let refused = |new: NewMessage| {
            let d = d.clone();
            async move { matches!(d.post(room.id, new).await, Err(DeskError::Invalid(_))) }
        };
        assert!(
            refused(NewMessage {
                in_reply_to: Some(elsewhere.id),
                ..said("dw/allan", "x")
            })
            .await,
            "an answer to another room's message"
        );
        assert!(
            refused(NewMessage {
                in_reply_to: Some(Uuid::new_v4()),
                ..said("dw/allan", "x")
            })
            .await,
            "an answer to nothing"
        );
        assert!(
            refused(NewMessage {
                steps: steps.clone(),
                ..said(YOU, "x")
            })
            .await,
            "steps on your message"
        );
        assert!(
            refused(NewMessage {
                steps: steps.clone(),
                delivery: Some(workforce_core::desk::DeliveryKind::Reminder),
                ..said("dw/allan", "x")
            })
            .await,
            "steps on a delivery"
        );
        assert!(
            refused(NewMessage {
                steps: vec![Step::of("web.search", true, 1); crate::rooms::MAX_STEPS + 1],
                ..said("dw/allan", "x")
            })
            .await,
            "too many steps"
        );
        assert!(matches!(
            d.room_message(other.id, ask.id).await,
            Err(DeskError::NotFound)
        ));
    })
    .await;
}

#[tokio::test]
async fn a_page_around_a_message_in_another_place_is_refused() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let room = release(&d).await;
        let ask = d
            .post(room.id, said(YOU, "ship it?"))
            .await
            .unwrap()
            .message;
        let reply = d
            .post(
                room.id,
                NewMessage {
                    thread_id: Some(ask.id),
                    ..said(YOU, "well?")
                },
            )
            .await
            .unwrap()
            .message;
        let around = |thread, target| Page {
            thread,
            span: Span::Around(target),
            limit: 10,
        };
        assert!(matches!(
            d.room_messages(room.id, around(None, reply.id)).await,
            Err(DeskError::Invalid(why)) if why == "that message is in a thread"
        ));
        assert!(matches!(
            d.room_messages(room.id, around(Some(ask.id), ask.id)).await,
            Err(DeskError::Invalid(_))
        ));
        assert!(matches!(
            d.room_messages(room.id, around(None, Uuid::new_v4())).await,
            Err(DeskError::NotFound)
        ));
        let thread = d
            .room_messages(room.id, around(Some(ask.id), reply.id))
            .await
            .unwrap();
        assert_eq!(thread.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_thread_message_is_copied_to_the_room_once_outside_the_loop_guard() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let room = release(&d).await;
        let ask = d
            .post(room.id, said(YOU, "what's left?"))
            .await
            .unwrap()
            .message;
        let steps = vec![Step::of("web.search", true, 840)];
        let answer = d
            .post(
                room.id,
                NewMessage {
                    thread_id: Some(ask.id),
                    in_reply_to: Some(ask.id),
                    steps: steps.clone(),
                    ..said("dw/allan", "@scout the migration is the last thing")
                },
            )
            .await
            .unwrap()
            .message;
        let (_, mut events) = d.subscribe();
        let copy = d.copy_to_room(room.id, answer.id).await.unwrap();
        assert!(copy.created);
        let m = &copy.message;
        assert_eq!(
            (
                m.thread_id,
                m.copy_of,
                m.hop,
                &m.author,
                &m.content,
                &m.steps
            ),
            (
                None,
                Some(answer.id),
                MAX_HOPS,
                &answer.author,
                &answer.content,
                &steps
            )
        );
        assert_eq!(
            (m.source.as_ref(), m.delivery, m.in_reply_to),
            (None, None, None)
        );
        assert!(!m.reading.unwrap().for_you, "a copy is only new");
        assert!(matches!(
            events.try_recv(),
            Ok(DeskEvent::RoomMessage { .. })
        ));
        let again = d.copy_to_room(room.id, answer.id).await.unwrap();
        assert_eq!((again.created, again.message.id), (false, m.id));
        assert!(events.try_recv().is_err(), "kept once, told once");
        assert!(
            d.room_message(room.id, answer.id)
                .await
                .unwrap()
                .reading
                .unwrap()
                .posted_to_room
        );
        assert_eq!(
            d.0.store.agent_run(room.id, None).await.unwrap(),
            0,
            "no agent's turn"
        );
        assert!(matches!(
            d.copy_to_room(room.id, ask.id).await,
            Err(DeskError::Invalid(_))
        ));
        assert!(matches!(
            d.copy_to_room(room.id, m.id).await,
            Err(DeskError::Invalid(_))
        ));

        let reply = d
            .post(
                room.id,
                NewMessage {
                    thread_id: Some(ask.id),
                    also_in_room: true,
                    ..said(YOU, "Let's ship it Friday.")
                },
            )
            .await
            .unwrap()
            .message;
        let main = d
            .room_messages(room.id, Page::newest(None, 50))
            .await
            .unwrap();
        assert_eq!(
            main.last().unwrap().copy_of,
            Some(reply.id),
            "also in the room"
        );
        assert!(matches!(
            d.post(
                room.id,
                NewMessage {
                    also_in_room: true,
                    ..said(YOU, "in the timeline")
                }
            )
            .await,
            Err(DeskError::Invalid(_))
        ));
    })
    .await;
}
