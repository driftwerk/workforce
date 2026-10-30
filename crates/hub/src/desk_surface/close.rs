//! A room Desk deleted: its conversation is stamped closed, so whatever was
//! set up to deliver there (a reminder, a job, a check-in, a workflow run)
//! is sent to the person's DM when it fires, naming the room; and the
//! messages its Desk never showed are copied there.

use serde_json::{json, Value};
use workforce_common::channels::desk::{DeliveryKind, DeskAnchor, DeskPlace, DeskTarget};
use workforce_common::session::{MessageRole, Session, SessionStore, TranscriptEntry};

use super::deliver::{deliver, Delivery, CLOSED_KEY};
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

/// The most messages one close copies: what a room's Desk never showed is a
/// handful, and each is looked up through the conversation's chain.
const MOST_COPIED: usize = 200;

/// How often a stamp is saved again when a concurrent save won the session.
const STAMP_ATTEMPTS: usize = 5;

/// Stamps session `id` with `closed` through a versioned save, so a save
/// loaded before it keeps the stamp rather than writing its metadata back;
/// saved again while a concurrent save won.
fn stamp(sessions: &dyn SessionStore, id: &str, closed: &Value) -> eyre::Result<()> {
    for _ in 0..STAMP_ATTEMPTS {
        let Some(mut head) = sessions.get(id)? else {
            eyre::bail!("the room's conversation {id} is gone");
        };
        if head.metadata.get(CLOSED_KEY) == Some(closed) {
            return Ok(());
        }
        match head.metadata.as_object_mut() {
            Some(metadata) => {
                metadata.insert(CLOSED_KEY.to_string(), closed.clone());
            }
            None => head.metadata = json!({ CLOSED_KEY: closed }),
        }
        sessions.save(&head)?;
    }
    eyre::bail!("the room's conversation {id} kept changing while it was closed")
}

/// Close `owner`'s conversation on the room `channel` ([`is_room`]) with
/// `service`'s agent: stamp it closed, which sends what delivers there to the
/// DM from now on, and copy the agent's messages in `undelivered` there, up
/// to [`MOST_COPIED`]. A message already copied is not copied again. Returns
/// how many were copied now.
pub(crate) async fn close(
    service: &AgentService,
    owner: &str,
    channel: &str,
    undelivered: &[String],
) -> eyre::Result<usize> {
    let sessions = service.sessions.as_ref();
    let Some(head) = sessions.find(channel, owner)? else {
        return Ok(0);
    };
    let name = room_name(Some(&head), channel);
    let mut copied: Vec<String> = head
        .metadata
        .get(CLOSED_KEY)
        .and_then(|closed| closed.get("copied"))
        .and_then(|ids| serde_json::from_value(ids.clone()).ok())
        .unwrap_or_default();
    let closed =
        |copied: &[String]| json!({ "at": chrono::Utc::now(), "name": name, "copied": copied });
    stamp(sessions, &head.id, &closed(&copied))?;
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
        .take(MOST_COPIED)
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
        stamp(sessions, &head.id, &closed(&copied))?;
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

    /// A delivery or a turn that loaded the conversation before the room
    /// closed saves after it without writing the stamp away.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_save_loaded_before_the_close_keeps_the_room_closed() {
        use workforce_common::session_postgres::PostgresSessionStore;
        workforce_postgres::test_support::with_test_pool(
            PostgresSessionStore::build_pool,
            |pool| async move {
                let store = PostgresSessionStore::with_pool(pool, "scout")
                    .await
                    .expect("store");
                let store: &dyn SessionStore = store.as_ref();
                let mut head = store.get_or_create("desk:planning", "p1", "scout").unwrap();
                head.add_user_message("hello", None);
                store.save(&head).unwrap();
                let mut loaded = store.get(&head.id).unwrap().unwrap();
                let closed = json!({ "name": "Planning", "copied": [] });
                stamp(store, &head.id, &closed).unwrap();
                loaded.add_assistant_message("delivered", 1);
                store.save(&loaded).unwrap();
                let saved = store.get(&head.id).unwrap().unwrap();
                assert_eq!(saved.metadata.get(CLOSED_KEY), Some(&closed));
                assert_eq!(saved.messages.len(), 2, "the late save's message is kept");
            },
        )
        .await;
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
