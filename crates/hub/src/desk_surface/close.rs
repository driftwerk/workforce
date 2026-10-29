//! A room Desk deleted: its conversation is stamped closed, what was set up
//! to deliver there is pointed at the person's DM, and the messages its Desk
//! never showed are copied there.

use serde_json::{json, Value};
use workforce_common::channels::desk::{DeliveryKind, DeskAnchor, DeskPlace, DeskTarget};
use workforce_common::session::{MessageRole, Session, SessionStore, TranscriptEntry};

use super::deliver::{deliver, Delivery, CLOSED_KEY};
use super::retarget::{retarget, Matches};
use crate::serve::AgentService;

/// What a room is called in a note about it: the name its latest turn gave,
/// else its slug.
fn room_name(head: Option<&Session>, channel: &str) -> String {
    head.and_then(|session| {
        session
            .metadata
            .get(CLOSED_KEY)
            .and_then(|closed| closed.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| match session.desk_origin()?.place {
                DeskPlace::Room { name, .. } => Some(name),
                _ => None,
            })
    })
    .unwrap_or_else(|| channel.trim_start_matches("desk:").to_string())
}

/// The message `id` of the conversation whose head is `head`, wherever in its
/// chain it is: in a session's archive, or in its live messages.
pub(crate) fn message_in_chain(
    store: &dyn SessionStore,
    head: &str,
    id: &str,
) -> eyre::Result<Option<(String, TranscriptEntry)>> {
    let mut session = Some(head.to_string());
    for _ in 0..50 {
        let Some(current) = session else { break };
        if let Some(entry) = store.read_archive_message(&current, id)? {
            return Ok(Some((current, entry)));
        }
        let live = store.get(&current)?;
        if let Some(entry) = live.as_ref().and_then(|s| {
            s.messages
                .iter()
                .find(|m| m.id == id)
                .map(|m| workforce_common::session::message_to_transcript_entry(&current, m))
        }) {
            return Ok(Some((current, entry)));
        }
        session = live.and_then(|s| s.prev_session_id().map(str::to_string));
    }
    Ok(None)
}

/// Whether `channel` is a Desk room's conversation, which can be closed.
pub(crate) fn is_room(channel: &str) -> bool {
    matches!(
        DeskAnchor::of(channel),
        Some(DeskAnchor::Room(_) | DeskAnchor::Slug(_))
    )
}

/// Close `owner`'s conversation on the room `channel` ([`is_room`]) with
/// `service`'s agent: stamp it closed, point what delivers there at the DM,
/// and copy the agent's messages in `undelivered` there. A message already
/// copied is not copied again. Returns how many were copied now.
pub(crate) async fn close(
    service: &AgentService,
    owner: &str,
    channel: &str,
    undelivered: &[String],
) -> eyre::Result<usize> {
    let sessions = service.sessions.as_ref();
    let head = sessions.find(channel, owner)?;
    let name = room_name(head.as_ref(), channel);
    let mut copied: Vec<String> = head
        .as_ref()
        .and_then(|h| h.metadata.get(CLOSED_KEY))
        .and_then(|closed| closed.get("copied"))
        .and_then(|ids| serde_json::from_value(ids.clone()).ok())
        .unwrap_or_default();
    let stamp = |copied: &[String]| {
        serde_json::Map::from_iter([(
            CLOSED_KEY.to_string(),
            json!({ "at": chrono::Utc::now(), "name": name, "copied": copied }),
        )])
    };
    if let Some(head) = &head {
        sessions.merge_metadata(&head.id, stamp(&copied), &[])?;
    }
    retarget(service, owner, Matches::Channel(channel)).await;
    let Some(head) = head else {
        return Ok(0);
    };
    let room = head.desk_origin().unwrap_or_else(|| DeskTarget {
        place: DeskPlace::LegacyRoom {
            slug: channel.trim_start_matches("desk:").to_string(),
        },
        channel: channel.to_string(),
        device: None,
        spoken: false,
    });
    let pending: Vec<&String> = undelivered
        .iter()
        .filter(|id| !copied.contains(id))
        .collect();
    let mut fresh = 0;
    for id in pending {
        let Some((_, entry)) = message_in_chain(sessions, &head.id, id)? else {
            continue;
        };
        if entry.role != MessageRole::Assistant {
            continue;
        }
        deliver(
            service,
            owner,
            &room,
            Delivery::text(DeliveryKind::Resumed, entry.content),
        )
        .await?;
        copied.push(id.clone());
        fresh += 1;
        sessions.merge_metadata(&head.id, stamp(&copied), &[])?;
    }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use workforce_common::channels::desk::DESK_CHANNEL;

    fn room(device: &str) -> DeskTarget {
        DeskTarget {
            place: DeskPlace::Room {
                room: Uuid::from_u128(1),
                name: "Planning".into(),
                thread: None,
            },
            channel: "desk:planning".into(),
            device: Some(device.into()),
            spoken: true,
        }
    }

    #[test]
    fn only_a_room_s_conversation_closes() {
        assert!(is_room("desk:planning"));
        assert!(is_room(&format!("desk:{}", Uuid::from_u128(1))));
        assert!(!is_room(DESK_CHANNEL) && !is_room("desk:tuple") && !is_room("ios"));
    }

    #[test]
    fn a_room_is_named_by_its_latest_turn_or_its_slug() {
        let mut head = Session::new("desk:planning", "p1", "scout");
        assert_eq!(room_name(Some(&head), "desk:planning"), "planning");
        head.set_desk_origin(&room("device-1"));
        assert_eq!(room_name(Some(&head), "desk:planning"), "Planning");
        assert_eq!(room_name(None, "desk:planning"), "planning");
    }
}
