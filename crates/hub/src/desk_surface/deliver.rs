//! Delivering to a Desk conversation outside a live turn: a reminder, a job's
//! answer, a workflow's step, a check-in, an approval. The session is the
//! outbox: the message is appended to the conversation's head session first,
//! then pushed to whoever watches it and signalled to the person's Desk hub
//! links, which drain it by cursor whenever they connect. A dropped socket, a
//! closed app or a hub restart loses nothing. A room of this hub's own desk
//! is posted to at once, so it fills while the app is closed.

use uuid::Uuid;
use workforce_common::channels::desk::{
    DeliveryKind, DeskDelivered, DeskFile, DeskMark, DeskPlace, DeskTarget, DESK_CHANNEL,
};
use workforce_common::session::{estimate_tokens, MessageRole, Session, SessionStore};

use crate::desk_attachments::{into_desk_conversation, DeskConversation, StagedFile};
use crate::serve::AgentService;

/// Whose Desk conversations a delivery lands in: an agent, by its sessions.
#[derive(Clone, Copy)]
pub(crate) struct Outbox<'a> {
    pub agent_id: &'a str,
    pub sessions: &'a dyn SessionStore,
}

impl<'a> From<&'a AgentService> for Outbox<'a> {
    fn from(service: &'a AgentService) -> Self {
        Self {
            agent_id: service.agent_id.as_str(),
            sessions: service.sessions.as_ref(),
        }
    }
}

impl<'a> From<&'a std::sync::Arc<AgentService>> for Outbox<'a> {
    fn from(service: &'a std::sync::Arc<AgentService>) -> Self {
        service.as_ref().into()
    }
}

/// Session metadata a room's conversation is stamped with once the room is
/// gone: `{"at", "name", "copied": [message ids]}`.
pub(crate) const CLOSED_KEY: &str = "desk_closed";

/// One message to deliver.
#[derive(Debug, Clone)]
pub(crate) struct Delivery {
    pub kind: DeliveryKind,
    pub text: String,
    pub files: Vec<DeskFile>,
    pub request_id: Option<String>,
}

impl Delivery {
    pub(crate) fn text(kind: DeliveryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            files: Vec::new(),
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

/// Deliver `delivery` to `owner`'s conversation with the agent of `outbox`
/// at `target`. Once the message is saved it is durable, so a failed push is
/// logged rather than returned: the person's Desk drains it from the session.
pub(crate) async fn deliver<'a>(
    outbox: impl Into<Outbox<'a>>,
    owner: &str,
    target: &DeskTarget,
    delivery: Delivery,
) -> eyre::Result<Delivered> {
    let outbox = outbox.into();
    let sessions = outbox.sessions;
    let closed = closed_room(sessions, &target.channel, owner)?;
    let (target, note) = routed(target, closed.as_deref());
    let text = workforce_common::redact::redact_secrets(&match note {
        Some(note) => format!("{note} {}", delivery.text),
        None => delivery.text,
    });
    let mut session = sessions.get_or_create(&target.channel, owner, outbox.agent_id)?;
    let message_id = Uuid::new_v4().to_string();
    session.add_assistant_message_with_id(message_id.clone(), &text, estimate_tokens(&text));
    let Some(message) = session.messages.last_mut() else {
        eyre::bail!("a delivery was not appended to its session");
    };
    message.attachments =
        (!delivery.files.is_empty()).then(|| delivery.files.iter().map(Into::into).collect());
    message.desk = Some(DeskMark {
        delivery: Some(delivery.kind),
        request_id: delivery.request_id,
        files: delivery.files,
        ..target.place.mark()
    });
    let message = message.clone();
    sessions.save(&session)?;
    if let Err(e) = sessions.append_to_archive(&session.id, &message) {
        tracing::warn!(session_id = %session.id, error = %e, "archiving a Desk delivery");
    }
    let delivered = Delivered {
        session_id: session.id,
        message_id,
    };
    let mark = message.desk.unwrap_or_default();
    announce(
        outbox,
        owner,
        &target,
        &delivered,
        &text,
        delivery.kind,
        &mark.files,
    )
    .await;
    #[cfg(feature = "postgres")]
    if let Some(desk) = crate::desk_driver::desk() {
        let posted = into_own_room(desk, outbox.agent_id, &target, &delivered, &text, &mark).await;
        if let Err(e) = posted {
            tracing::warn!(error = %e, channel = %target.channel, "posting a delivery into its room");
        }
    }
    Ok(delivered)
}

/// Posts a delivery, saved with `mark`, into its room when the room is
/// `desk`'s, as Desk's drain would: under the agent's name, from this Mac's
/// hub, keyed by the session message so the drain's own post of it later is
/// a no-op. `false` when the room is not `desk`'s (another Mac's, or a
/// remote hub's agent).
#[cfg(feature = "postgres")]
pub(crate) async fn into_own_room(
    desk: &workforce_desk::Desk,
    agent_id: &str,
    target: &DeskTarget,
    delivered: &Delivered,
    text: &str,
    mark: &DeskMark,
) -> eyre::Result<bool> {
    use workforce_desk::rooms::{source_key, Member, NewMessage, LOCAL_HUB};
    let DeskPlace::Room { room, thread, .. } = &target.place else {
        return Ok(false);
    };
    if desk.room(*room).is_none_or(|r| r.channel != target.channel) {
        return Ok(false);
    }
    let attachments = mark
        .files
        .iter()
        .map(|file| workforce_desk::attachments::Attachment {
            hub: Some(LOCAL_HUB.into()),
            agent: Some(agent_id.into()),
            ..file.into()
        })
        .collect();
    let author = Member {
        hub: LOCAL_HUB.into(),
        agent: agent_id.into(),
    };
    desk.post(
        *room,
        NewMessage {
            author: author.key(),
            content: super::shown(text),
            thread_id: *thread,
            hop: 0,
            attachments,
            source: Some(source_key(LOCAL_HUB, agent_id, &delivered.message_id)),
            delivery: mark.delivery,
            request_id: mark.request_id.clone(),
        },
    )
    .await?;
    Ok(true)
}

/// Deliver `file`, staged in the store of the agent of `outbox`, into
/// `owner`'s conversation at `target` as a message of its own, captioned,
/// and keep it there for the conversation's history.
pub(crate) async fn deliver_file<'a>(
    outbox: impl Into<Outbox<'a>>,
    owner: &str,
    target: &DeskTarget,
    file: StagedFile,
) -> eyre::Result<Delivered> {
    let outbox = outbox.into();
    let delivered = deliver(
        outbox,
        owner,
        target,
        Delivery {
            files: vec![file.delivered(outbox.agent_id)],
            ..Delivery::text(DeliveryKind::File, file.caption.clone().unwrap_or_default())
        },
    )
    .await?;
    into_desk_conversation(
        outbox.sessions,
        &DeskConversation {
            agent_id: outbox.agent_id.to_string(),
            session_id: delivered.session_id.clone(),
            message: Some(delivered.message_id.clone()),
        },
        &[file],
    )
    .await;
    Ok(delivered)
}

/// Tell whoever watches the conversation that `delivered`, `text` with
/// `files`, landed in it, and the person's Desk hub links on every replica.
/// Best effort: the message is saved already, and a Desk drains it whenever
/// it connects.
pub(crate) async fn announce(
    outbox: Outbox<'_>,
    owner: &str,
    target: &DeskTarget,
    delivered: &Delivered,
    text: &str,
    kind: DeliveryKind,
    files: &[DeskFile],
) {
    let Some(push) = crate::session_push::installed() else {
        return;
    };
    push.message(
        &delivered.session_id,
        &super::shown(text),
        Some(&delivered.message_id),
        files,
    )
    .await;
    push.delivered(
        owner,
        DeskDelivered {
            agent_id: outbox.agent_id.to_string(),
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
            service.into(),
            owner,
            &target,
            &delivered,
            &text,
            DeliveryKind::Resumed,
            &[],
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

    /// A delivery for a room of this hub's own desk is posted into it, in
    /// its thread, under the agent's name, with its files and its kind; the
    /// drain posting the same message again is a no-op on its source. A
    /// room this desk doesn't hold is left to the drain.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn a_delivery_for_a_room_of_this_hubs_desk_is_posted_into_its_thread() {
        use workforce_desk::rooms::{source_key, NewMessage, NewRoom};
        use workforce_postgres::test_support::with_test_pool;
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let desk = crate::desk_crew::tests::desk(pool, dir.path()).await;
            let room = desk
                .create_room(NewRoom {
                    name: "Release".into(),
                    members: Vec::new(),
                    default_member: None,
                })
                .await
                .unwrap();
            let ask = desk
                .post(
                    room.id,
                    NewMessage {
                        author: "you".into(),
                        content: "remind me".into(),
                        thread_id: None,
                        hop: 0,
                        attachments: Vec::new(),
                        source: None,
                        delivery: None,
                        request_id: None,
                    },
                )
                .await
                .unwrap()
                .message;
            let target = DeskTarget {
                place: DeskPlace::Room {
                    room: room.id,
                    name: room.name.clone(),
                    thread: Some(ask.id),
                },
                channel: room.channel.clone(),
                device: None,
                spoken: false,
            };
            let delivered = Delivered {
                session_id: "s1".into(),
                message_id: "m1".into(),
            };
            let file = DeskFile {
                artifact_id: "art_1".into(),
                agent_id: "scout".into(),
                filename: "chart.png".into(),
                media_type: "image/png".into(),
                size: 9,
                width: Some(2),
                height: Some(1),
                caption: None,
            };
            let mark = DeskMark {
                delivery: Some(DeliveryKind::Reminder),
                files: vec![file],
                ..target.place.mark()
            };
            for _ in 0..2 {
                let posted = into_own_room(
                    &desk,
                    "scout",
                    &target,
                    &delivered,
                    "🔔 Reminder: ship it",
                    &mark,
                )
                .await
                .unwrap();
                assert!(posted);
            }
            let thread = desk
                .room_messages(room.id, Some(ask.id), None, 50)
                .await
                .unwrap();
            assert_eq!(thread.len(), 1, "posted once");
            let reminder = &thread[0];
            assert_eq!(
                (
                    reminder.author.as_str(),
                    reminder.content.as_str(),
                    reminder.source.as_deref(),
                    reminder.delivery
                ),
                (
                    "local/scout",
                    "🔔 Reminder: ship it",
                    Some(source_key("local", "scout", "m1").as_str()),
                    Some(DeliveryKind::Reminder)
                )
            );
            let shown = &reminder.attachments[0];
            assert_eq!(
                (
                    shown.artifact_id.as_str(),
                    shown.hub.as_deref(),
                    shown.agent.as_deref()
                ),
                ("art_1", Some("local"), Some("scout"))
            );

            let elsewhere = DeskTarget {
                channel: format!("desk:{}", Uuid::new_v4()),
                ..target.clone()
            };
            let approval = DeskMark {
                delivery: Some(DeliveryKind::Approval),
                request_id: Some("req-1".into()),
                ..target.place.mark()
            };
            let asked = Delivered {
                message_id: "m2".into(),
                ..delivered.clone()
            };
            assert!(
                into_own_room(&desk, "scout", &target, &asked, "May I?", &approval)
                    .await
                    .unwrap()
            );
            let thread = desk
                .room_messages(room.id, Some(ask.id), None, 50)
                .await
                .unwrap();
            assert_eq!(thread[1].request_id.as_deref(), Some("req-1"));
            for elsewhere in [elsewhere, target.dm()] {
                assert!(
                    !into_own_room(&desk, "scout", &elsewhere, &delivered, "x", &approval)
                        .await
                        .unwrap()
                );
            }
        })
        .await;
    }
}
