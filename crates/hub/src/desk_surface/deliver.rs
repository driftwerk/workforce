//! Delivering to a Desk conversation outside a live turn: a reminder, a job's
//! answer, a workflow's step, a check-in, an approval. The session is the
//! outbox: the message is appended to the conversation's head session first,
//! then pushed to whoever watches it and signalled to the person's Desk hub
//! links, which drain it by cursor whenever they connect. A dropped socket, a
//! closed app or a hub restart loses nothing.

use uuid::Uuid;
use workforce_common::channels::desk::{
    DeliveryKind, DeskDelivered, DeskMark, DeskPlace, DeskTarget, DESK_CHANNEL,
};
use workforce_common::session::{
    estimate_tokens, MessageRole, Session, SessionAttachment, SessionStore,
};

use crate::serve::AgentService;

/// Session metadata a room's conversation is stamped with once the room is
/// gone: `{"at", "name", "copied": [message ids]}`.
pub(crate) const CLOSED_KEY: &str = "desk_closed";

/// One message to deliver.
#[derive(Debug, Clone)]
pub(crate) struct Delivery {
    pub kind: DeliveryKind,
    pub text: String,
    pub attachments: Vec<SessionAttachment>,
    pub request_id: Option<String>,
}

impl Delivery {
    pub(crate) fn text(kind: DeliveryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            attachments: Vec::new(),
            request_id: None,
        }
    }
}

/// Where a delivery was saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Delivered {
    pub session_id: String,
    pub message_id: String,
}

/// Where a delivery for `target` lands, and the note its text opens with: a
/// room that was deleted (`closed` names it) or one an older Desk knew only
/// by its slug sends it to the DM, saying which room it was for.
pub(crate) fn routed(target: &DeskTarget, closed: Option<&str>) -> (DeskTarget, Option<String>) {
    match (&target.place, closed) {
        (_, Some(name)) => (
            target.dm(),
            Some(format!("(for #{name}, which was deleted)")),
        ),
        (DeskPlace::LegacyRoom { slug }, None) => (target.dm(), Some(format!("(from #{slug})"))),
        _ => (target.clone(), None),
    }
}

/// The name of the deleted room whose conversation is `channel`, if it was
/// closed.
pub(crate) fn closed_room(
    sessions: &dyn SessionStore,
    channel: &str,
    owner: &str,
) -> eyre::Result<Option<String>> {
    if channel == DESK_CHANNEL {
        return Ok(None);
    }
    Ok(sessions.find(channel, owner)?.and_then(|head| {
        head.metadata
            .get(CLOSED_KEY)
            .and_then(|closed| closed.get("name"))
            .and_then(|name| name.as_str())
            .map(str::to_string)
    }))
}

/// Deliver `delivery` to `owner`'s conversation with `service`'s agent at
/// `target`. Once the message is saved it is durable, so a failed push is
/// logged rather than returned: the person's Desk drains it from the session.
pub(crate) async fn deliver(
    service: &AgentService,
    owner: &str,
    target: &DeskTarget,
    delivery: Delivery,
) -> eyre::Result<Delivered> {
    let sessions = service.sessions.as_ref();
    let closed = closed_room(sessions, &target.channel, owner)?;
    let (target, note) = routed(target, closed.as_deref());
    let text = workforce_common::redact::redact_secrets(&match note {
        Some(note) => format!("{note} {}", delivery.text),
        None => delivery.text,
    });
    let mut session = sessions.get_or_create(&target.channel, owner, service.agent_id.as_str())?;
    let message_id = Uuid::new_v4().to_string();
    session.add_assistant_message_with_id(message_id.clone(), &text, estimate_tokens(&text));
    let Some(message) = session.messages.last_mut() else {
        eyre::bail!("a delivery was not appended to its session");
    };
    message.desk = Some(DeskMark {
        delivery: Some(delivery.kind),
        request_id: delivery.request_id,
        ..target.place.mark()
    });
    message.attachments = (!delivery.attachments.is_empty()).then_some(delivery.attachments);
    let message = message.clone();
    sessions.save(&session)?;
    if let Err(e) = sessions.append_to_archive(&session.id, &message) {
        tracing::warn!(session_id = %session.id, error = %e, "archiving a Desk delivery");
    }
    let delivered = Delivered {
        session_id: session.id,
        message_id,
    };
    announce(service, owner, &target, &delivered, &text, delivery.kind).await;
    Ok(delivered)
}

/// Tell whoever watches the conversation that `delivered` landed in it, and
/// the person's Desk hub links on every replica. Best effort: the message
/// is saved already, and a Desk drains it whenever it connects.
pub(crate) async fn announce(
    service: &AgentService,
    owner: &str,
    target: &DeskTarget,
    delivered: &Delivered,
    text: &str,
    kind: DeliveryKind,
) {
    let Some(push) = crate::session_push::installed() else {
        return;
    };
    push.message(
        &delivered.session_id,
        &super::shown(text),
        Some(&delivered.message_id),
    )
    .await;
    push.delivered(
        owner,
        DeskDelivered {
            agent_id: service.agent_id.to_string(),
            channel: target.channel.clone(),
            session_id: delivered.session_id.clone(),
            message_id: delivered.message_id.clone(),
            kind,
            place: target.place.clone(),
            spoken: target.spoken,
        },
    )
    .await;
}

/// Hand `answer`, what a turn re-run after the fact in `session` said (an
/// approval's resume, a recovered turn), to the Desk conversation the
/// session answers to. A Desk conversation's own session holds the answer
/// already, so it is announced; any other (a job's run) delivers it there.
/// `false` when the session answers to no Desk conversation.
pub(crate) async fn resumed(
    service: &AgentService,
    session: &Session,
    owner: &str,
    answer: &str,
) -> eyre::Result<bool> {
    let Some(target) = session.desk_origin() else {
        return Ok(false);
    };
    if session.channel != target.channel {
        deliver(
            service,
            owner,
            &target,
            Delivery::text(DeliveryKind::Resumed, answer),
        )
        .await?;
        return Ok(true);
    }
    let saved = service.sessions.get(&session.id)?.and_then(|now| {
        now.messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
            .map(|m| (m.id.clone(), m.content.clone()))
    });
    if let Some((message_id, text)) = saved {
        let delivered = Delivered {
            session_id: session.id.clone(),
            message_id,
        };
        announce(
            service,
            owner,
            &target,
            &delivered,
            &text,
            DeliveryKind::Resumed,
        )
        .await;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> DeskTarget {
        DeskTarget {
            place: DeskPlace::Room {
                room: Uuid::from_u128(1),
                name: "Planning".into(),
                thread: Some(Uuid::from_u128(2)),
            },
            channel: format!("desk:{}", Uuid::from_u128(1)),
            device: Some("device-1".into()),
            spoken: true,
        }
    }

    /// A delivery lands in the conversation's head as it is, marked with its
    /// place and kind, and is pushed to the session's watchers and signalled
    /// to its person's hub links. A deleted room's goes to the DM, named.
    #[tokio::test]
    async fn a_delivery_is_saved_marked_pushed_and_signalled() {
        let tmp = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(tmp.path()).await;
        let mut rx = crate::session_push::subscribe_for_tests();
        let mut stale = service
            .sessions
            .get_or_create(&room().channel, "person-1", service.agent_id.as_str())
            .unwrap();
        stale.updated_at -= chrono::Duration::days(30);
        service.sessions.save(&stale).unwrap();

        let delivered = deliver(
            &service,
            "person-1",
            &room(),
            Delivery {
                request_id: Some("r1".into()),
                ..Delivery::text(DeliveryKind::Reminder, "stand-up in five")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            delivered.session_id, stale.id,
            "the head, never a rolled successor"
        );
        let head = service.sessions.get(&stale.id).unwrap().unwrap();
        let saved = head.messages.last().unwrap();
        assert_eq!(saved.id, delivered.message_id);
        assert_eq!(saved.content, "stand-up in five");
        assert_eq!(
            saved.desk,
            Some(DeskMark {
                delivery: Some(DeliveryKind::Reminder),
                request_id: Some("r1".into()),
                ..room().place.mark()
            })
        );
        let frames: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|f| f.session_id == stale.id || f.delivered.is_some())
            .collect();
        let message = frames.iter().find(|f| f.msg_type == "message").unwrap();
        assert_eq!(
            (message.session_id.as_str(), message.id.as_deref()),
            (stale.id.as_str(), Some(delivered.message_id.as_str()))
        );
        let signal = frames
            .iter()
            .filter(|f| f.target_identity.as_deref() == Some("person-1"))
            .find_map(|f| f.delivered.clone().filter(|d| d.session_id == stale.id))
            .expect("the hub links are told");
        assert_eq!(
            (signal.channel.as_str(), signal.kind, signal.spoken),
            (room().channel.as_str(), DeliveryKind::Reminder, true)
        );

        let mut patch = serde_json::Map::new();
        patch.insert(CLOSED_KEY.into(), serde_json::json!({"name": "Planning"}));
        service
            .sessions
            .merge_metadata(&stale.id, patch, &[])
            .unwrap();
        let moved = deliver(
            &service,
            "person-1",
            &room(),
            Delivery::text(DeliveryKind::Job, "done"),
        )
        .await
        .unwrap();
        let dm = service
            .sessions
            .find(DESK_CHANNEL, "person-1")
            .unwrap()
            .unwrap();
        assert_eq!(moved.session_id, dm.id);
        let note = dm.messages.last().unwrap();
        assert_eq!(note.content, "(for #Planning, which was deleted) done");
        assert_eq!(note.desk.as_ref().and_then(|m| m.room), None);
    }

    #[test]
    fn a_live_room_gets_its_delivery_and_a_gone_one_sends_it_to_the_dm() {
        assert_eq!(routed(&room(), None), (room(), None));
        assert_eq!(
            routed(&room(), Some("Planning")),
            (
                room().dm(),
                Some("(for #Planning, which was deleted)".to_string())
            )
        );
        let legacy = DeskTarget {
            place: DeskPlace::LegacyRoom {
                slug: "planning".into(),
            },
            channel: DESK_CHANNEL.into(),
            device: None,
            spoken: false,
        };
        assert_eq!(
            routed(&legacy, None),
            (legacy.dm(), Some("(from #planning)".to_string()))
        );
    }
}
