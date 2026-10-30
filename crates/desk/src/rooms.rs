//! Rooms: you and several agents, on any hub, in one timeline. A room's
//! messages live here; each agent's turns stay in its own hub's session.
//! Agents can @-mention each other, so every agent message carries a hop
//! count and the hub refuses chains that run too long.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_postgres::Row;
use uuid::Uuid;

use workforce_core::desk::{DeliveryKind, DESK_CHANNEL};

use crate::attachments::Attachment;
use crate::store::DeskStore;

/// Agent-to-agent hand-offs allowed from one message of yours.
pub const MAX_HOPS: i16 = workforce_core::desk::MAX_HOPS as i16;
/// Agent messages allowed in a room (or thread) before you speak again.
pub const TURN_BUDGET: i64 = 12;
/// What Desk calls the hub on its own Mac, in member keys and sources.
pub const LOCAL_HUB: &str = "local";
/// What a message for a thread that has since been deleted opens with, in
/// the timeline it is posted to instead.
pub const THREAD_GONE: &str = "↳ (in a thread that was deleted)";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub hub: String,
    pub agent: String,
}

impl Member {
    pub fn key(&self) -> String {
        format!("{}/{}", self.hub, self.agent)
    }

    /// The member a `hub/agent` key names, when it is well formed.
    pub fn parse(key: &str) -> Option<Self> {
        key.split_once('/')
            .filter(|(hub, agent)| !hub.is_empty() && !agent.is_empty() && !agent.contains('/'))
            .map(|(hub, agent)| Self {
                hub: hub.into(),
                agent: agent.into(),
            })
    }
}

/// The source of a message `agent` on `hub` said in its own session as
/// `message_id`: the key posting it again is known by, wherever it is
/// posted from.
pub fn source_key(hub: &str, agent: &str, message_id: &str) -> String {
    format!("{hub}/{agent}/{message_id}")
}

/// The conversation channel of a new room: its id, which renaming never
/// changes.
pub fn channel_for(id: Uuid) -> String {
    format!("{DESK_CHANNEL}:{id}")
}

/// Which thread of `room` a message for `thread` goes in: that thread
/// while `parent`, its first message as kept, is a message of the room's
/// timeline; else the timeline.
pub fn place_in_timeline(
    room: Uuid,
    thread: Option<Uuid>,
    parent: Option<&RoomMessage>,
) -> Option<Uuid> {
    thread.filter(|_| parent.is_some_and(|p| p.room_id == room && p.thread_id.is_none()))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    /// The channel its agents' sessions are on; never changes.
    pub channel: String,
    pub members: Vec<Member>,
    pub default_member: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Who wrote a message: `you`, `desk`, or a member's `hub/agent` key.
pub const YOU: &str = "you";
pub const DESK: &str = "desk";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMessage {
    pub id: Uuid,
    pub room_id: Uuid,
    pub thread_id: Option<Uuid>,
    pub author: String,
    pub content: String,
    pub hop: i16,
    pub created_at: DateTime<Utc>,
    /// Its files, each naming the hub and agent whose store holds it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// The session message it is ([`source_key`]), for an agent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Why it was delivered outside a turn, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryKind>,
    /// The routed approval an `approval` delivery asks you to decide, on the
    /// hub of its agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// On a main-timeline message as read back, its thread's replies.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub replies: Option<Replies>,
}

/// How many replies a message's thread has, and when the last came.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Replies {
    pub reply_count: i64,
    pub last_reply_at: Option<DateTime<Utc>>,
}

/// A message as posting left it: kept now, or kept before under the same
/// source.
#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    pub message: RoomMessage,
    pub created: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewRoom {
    pub name: String,
    #[serde(default)]
    pub members: Vec<Member>,
    pub default_member: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomPatch {
    pub name: Option<String>,
    pub members: Option<Vec<Member>>,
    #[serde(default, with = "double_option")]
    pub default_member: Option<Option<String>>,
}

/// `null` clears the field; absent leaves it.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Option::<String>::deserialize(d).map(Some)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewMessage {
    pub author: String,
    pub content: String,
    pub thread_id: Option<Uuid>,
    #[serde(default)]
    pub hop: i16,
    /// Files it carries: an agent's answer's, or what you uploaded to the
    /// agents you sent it to. A message with files may have no text.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// The session message it is ([`source_key`]): posting it again
    /// answers with the message already kept.
    #[serde(default)]
    pub source: Option<String>,
    /// Why it was delivered outside a turn: a reminder, a job's answer, …
    #[serde(default)]
    pub delivery: Option<DeliveryKind>,
    /// The routed approval an `approval` delivery asks you to decide.
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub room_id: Uuid,
    pub message: RoomMessage,
}

pub fn slug_for(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    s.chars().take(64).collect()
}

fn room(row: &Row) -> Result<Room> {
    Ok(Room {
        id: row.try_get("id")?,
        slug: row.try_get("slug")?,
        name: row.try_get("name")?,
        channel: row.try_get("channel")?,
        members: serde_json::from_value(row.try_get("members")?)?,
        default_member: row.try_get("default_member")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn message(row: &Row) -> Result<RoomMessage> {
    Ok(RoomMessage {
        id: row.try_get("id")?,
        room_id: row.try_get("room_id")?,
        thread_id: row.try_get("thread_id")?,
        author: row.try_get("author")?,
        content: row.try_get("content")?,
        hop: row.try_get("hop")?,
        created_at: row.try_get("created_at")?,
        attachments: serde_json::from_value(row.try_get("attachments")?)?,
        source: row.try_get("source")?,
        delivery: row
            .try_get::<_, Option<String>>("delivery")?
            .map(|kind| serde_json::from_value(serde_json::Value::String(kind)))
            .transpose()?,
        request_id: row.try_get("request_id")?,
        replies: match row.try_get::<_, Option<Uuid>>("thread_id")? {
            Some(_) => None,
            None => Some(Replies {
                reply_count: row.try_get("reply_count")?,
                last_reply_at: row.try_get("last_reply_at")?,
            }),
        },
    })
}

fn delivery_name(kind: Option<DeliveryKind>) -> Result<Option<String>> {
    kind.map(|k| match serde_json::to_value(k)? {
        serde_json::Value::String(name) => Ok(name),
        other => eyre::bail!("a delivery kind named itself {other}"),
    })
    .transpose()
}

/// A message's columns, with its thread's replies counted: read from
/// `room_messages` unaliased (a `RETURNING` included).
const MESSAGE_COLUMNS: &str =
    "id, room_id, thread_id, author, content, hop, created_at, attachments, source, delivery, \
     request_id, \
     (SELECT count(*) FROM room_messages r WHERE r.thread_id = room_messages.id) AS reply_count, \
     (SELECT max(r.created_at) FROM room_messages r WHERE r.thread_id = room_messages.id) \
       AS last_reply_at";

impl DeskStore {
    pub async fn rooms(&self) -> Result<Vec<Room>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT * FROM rooms ORDER BY name", &[])
            .await?;
        rows.iter().map(room).collect()
    }

    pub async fn room(&self, id: Uuid) -> Result<Option<Room>> {
        let client = self.pool().get().await?;
        client
            .query_opt("SELECT * FROM rooms WHERE id = $1", &[&id])
            .await?
            .as_ref()
            .map(room)
            .transpose()
    }

    pub async fn put_room(&self, r: &Room) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO rooms \
                 (id, slug, name, members, default_member, created_at, updated_at, channel) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO UPDATE SET slug = $2, \
                 name = $3, members = $4, default_member = $5, updated_at = $7",
                &[
                    &r.id,
                    &r.slug,
                    &r.name,
                    &serde_json::to_value(&r.members)?,
                    &r.default_member,
                    &r.created_at,
                    &r.updated_at,
                    &r.channel,
                ],
            )
            .await
            .context("saving room")?;
        Ok(())
    }

    pub async fn slug_taken(&self, slug: &str, except: Option<Uuid>) -> Result<bool> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT count(*) FROM rooms WHERE slug = $1 AND id IS DISTINCT FROM $2",
                &[&slug, &except],
            )
            .await?;
        Ok(row.get::<_, i64>(0) > 0)
    }

    pub async fn delete_room(&self, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute("DELETE FROM rooms WHERE id = $1", &[&id])
            .await?
            > 0)
    }

    /// Keeps `m`, unless a message with its source is kept already, which
    /// is answered instead.
    pub async fn add_message(&self, m: &RoomMessage) -> Result<Posted> {
        let client = self.pool().get().await?;
        let inserted = client
            .query_opt(
                &format!(
                    "INSERT INTO room_messages \
                     (id, room_id, thread_id, author, content, hop, created_at, attachments, \
                     source, delivery, request_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) \
                     ON CONFLICT (source) WHERE source IS NOT NULL DO NOTHING \
                     RETURNING {MESSAGE_COLUMNS}"
                ),
                &[
                    &m.id,
                    &m.room_id,
                    &m.thread_id,
                    &m.author,
                    &m.content,
                    &m.hop,
                    &m.created_at,
                    &serde_json::to_value(&m.attachments)?,
                    &m.source,
                    &delivery_name(m.delivery)?,
                    &m.request_id,
                ],
            )
            .await
            .context("saving room message")?;
        if let Some(row) = inserted {
            return Ok(Posted {
                message: message(&row)?,
                created: true,
            });
        }
        let source = m.source.as_deref().unwrap_or_default();
        let kept = self
            .by_source(source)
            .await?
            .ok_or_else(|| eyre::eyre!("room message {source} was neither kept nor found"))?;
        Ok(Posted {
            message: kept,
            created: false,
        })
    }

    /// The message kept under `source`, if any.
    pub async fn by_source(&self, source: &str) -> Result<Option<RoomMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {MESSAGE_COLUMNS} FROM room_messages WHERE source = $1"),
                &[&source],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    pub async fn message(&self, id: Uuid) -> Result<Option<RoomMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {MESSAGE_COLUMNS} FROM room_messages WHERE id = $1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// The newest `limit` messages of a room's main timeline (or of one
    /// thread), oldest first, optionally before a point.
    pub async fn messages(
        &self,
        room_id: Uuid,
        thread_id: Option<Uuid>,
        before: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<RoomMessage>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT * FROM (SELECT {MESSAGE_COLUMNS} FROM room_messages \
                     WHERE room_id = $1 AND thread_id IS NOT DISTINCT FROM $2 \
                     AND ($3::timestamptz IS NULL OR created_at < $3) \
                     ORDER BY created_at DESC LIMIT $4) m ORDER BY created_at"
                ),
                &[&room_id, &thread_id, &before, &limit],
            )
            .await?;
        rows.iter().map(message).collect()
    }

    /// Agent messages since you last spoke, in the main timeline or a
    /// thread; deliveries are none of them.
    pub async fn agent_run(&self, room_id: Uuid, thread_id: Option<Uuid>) -> Result<i64> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT count(*) FROM room_messages WHERE room_id = $1 \
                 AND thread_id IS NOT DISTINCT FROM $2 AND author NOT IN ('you', 'desk') \
                 AND delivery IS NULL \
                 AND created_at > COALESCE((SELECT max(created_at) FROM room_messages \
                   WHERE room_id = $1 AND thread_id IS NOT DISTINCT FROM $2 AND author = 'you'), \
                   '-infinity'::timestamptz)",
                &[&room_id, &thread_id],
            )
            .await?;
        Ok(row.get(0))
    }

    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM room_messages \
                     WHERE search @@ websearch_to_tsquery('english', $1) \
                     ORDER BY ts_rank(search, websearch_to_tsquery('english', $1)) DESC, created_at DESC \
                     LIMIT $2"
                ),
                &[&query, &limit],
            )
            .await?;
        rows.iter()
            .map(|r| {
                let m = message(r)?;
                Ok(SearchHit {
                    room_id: m.room_id,
                    message: m,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    pub(crate) fn room_fixture(name: &str) -> Room {
        let now = Utc::now();
        let id = Uuid::new_v4();
        Room {
            id,
            channel: channel_for(id),
            slug: slug_for(name),
            name: name.into(),
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
            default_member: Some("dw/allan".into()),
            created_at: now,
            updated_at: now,
        }
    }

    fn msg(room: &Room, author: &str, content: &str, at: DateTime<Utc>) -> RoomMessage {
        RoomMessage {
            id: Uuid::new_v4(),
            room_id: room.id,
            thread_id: None,
            author: author.into(),
            content: content.into(),
            hop: 0,
            created_at: at,
            attachments: Vec::new(),
            source: None,
            delivery: None,
            request_id: None,
            replies: None,
        }
    }

    #[test]
    fn slugs_are_channel_safe() {
        assert_eq!(slug_for("Release 0.9!"), "release-0-9");
    }

    #[test]
    fn a_new_rooms_channel_is_its_id_and_sources_name_hub_agent_and_message() {
        let id = Uuid::from_u128(7);
        assert_eq!(channel_for(id), "desk:00000000-0000-0000-0000-000000000007");
        assert_eq!(
            source_key(LOCAL_HUB, "scout", "m-1"),
            "local/scout/m-1",
            "Desk's drain builds the same key"
        );
        assert_eq!(
            Member::parse("dw/allan"),
            Some(Member {
                hub: "dw".into(),
                agent: "allan".into()
            })
        );
        for bad in ["you", "/allan", "dw/", "dw/allan/x"] {
            assert_eq!(Member::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_message_for_a_thread_that_has_gone_goes_to_the_timeline() {
        let r = room_fixture("release");
        let root = msg(&r, YOU, "ship it?", Utc::now());
        let mut reply = msg(&r, YOU, "yes", Utc::now());
        reply.thread_id = Some(root.id);
        let elsewhere = msg(&room_fixture("other"), YOU, "hi", Utc::now());
        assert_eq!(place_in_timeline(r.id, None, None), None);
        assert_eq!(
            place_in_timeline(r.id, Some(root.id), Some(&root)),
            Some(root.id)
        );
        assert_eq!(place_in_timeline(r.id, Some(root.id), None), None);
        assert_eq!(place_in_timeline(r.id, Some(reply.id), Some(&reply)), None);
        assert_eq!(
            place_in_timeline(r.id, Some(elsewhere.id), Some(&elsewhere)),
            None
        );
    }

    /// Rooms made before channels were frozen keep the slug channel their
    /// agents' sessions are on, except one called `tuple`, which would be
    /// the call's.
    #[tokio::test]
    async fn older_rooms_keep_their_slug_channel_and_tuple_takes_its_id() {
        with_test_pool(build_pool, |pool| async move {
            let client = pool.get().await.unwrap();
            client
                .batch_execute(&format!(
                    "CREATE SCHEMA IF NOT EXISTS desk; SET search_path TO desk, public; {}",
                    [
                        include_str!("../migrations/V001__sessions.sql"),
                        include_str!("../migrations/V002__rooms.sql"),
                        include_str!("../migrations/V003__crew.sql"),
                        include_str!("../migrations/V004__message_authors.sql"),
                        include_str!("../migrations/V005__state.sql"),
                        include_str!("../migrations/V006__terminals.sql"),
                        include_str!("../migrations/V007__session_terminal.sql"),
                        include_str!("../migrations/V008__names_and_groups.sql"),
                        include_str!("../migrations/V009__attachments.sql"),
                    ]
                    .join("\n")
                ))
                .await
                .unwrap();
            let release = Uuid::new_v4();
            let tuple = Uuid::new_v4();
            for (id, slug) in [(release, "release"), (tuple, "tuple")] {
                client
                    .execute(
                        "INSERT INTO rooms (id, slug, name, members, created_at, updated_at) \
                         VALUES ($1, $2, $2, '[]', now(), now())",
                        &[&id, &slug],
                    )
                    .await
                    .unwrap();
            }
            client
                .batch_execute(include_str!("../migrations/V010__surface.sql"))
                .await
                .unwrap();
            let channel = |id: Uuid| {
                let client = &client;
                async move {
                    client
                        .query_one("SELECT channel FROM rooms WHERE id = $1", &[&id])
                        .await
                        .unwrap()
                        .get::<_, String>(0)
                }
            };
            assert_eq!(channel(release).await, "desk:release");
            assert_eq!(channel(tuple).await, channel_for(tuple));
        })
        .await;
    }

    #[tokio::test]
    async fn a_main_message_says_how_many_replies_its_thread_has_and_when_the_last_came() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let t0 = chrono::DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
                .unwrap()
                .to_utc();
            let at = |s: i64| t0 + chrono::Duration::seconds(s);
            let [busy, quiet, untouched] = [1, 2, 3].map(|i| msg(&r, YOU, "a question", at(i)));
            for m in [&busy, &quiet, &untouched] {
                store.add_message(m).await.unwrap();
            }
            for (parent, s) in [(&busy, 10), (&busy, 20), (&quiet, 15), (&busy, 30)] {
                let mut reply = msg(&r, "dw/allan", "a reply", at(s));
                reply.thread_id = Some(parent.id);
                store.add_message(&reply).await.unwrap();
            }
            let main = store.messages(r.id, None, None, 50).await.unwrap();
            let replies: Vec<_> = main.iter().map(|m| m.replies).collect();
            assert_eq!(
                replies,
                [
                    Some(Replies {
                        reply_count: 3,
                        last_reply_at: Some(at(30))
                    }),
                    Some(Replies {
                        reply_count: 1,
                        last_reply_at: Some(at(15))
                    }),
                    Some(Replies {
                        reply_count: 0,
                        last_reply_at: None
                    }),
                ]
            );
            assert_eq!(
                store.message(busy.id).await.unwrap().unwrap().replies,
                replies[0]
            );
            let thread = store.messages(r.id, Some(busy.id), None, 50).await.unwrap();
            assert!(
                thread.iter().all(|m| m.replies.is_none()),
                "a reply has no thread of its own"
            );
            assert_eq!(
                serde_json::to_value(&main[0]).unwrap()["replyCount"],
                serde_json::json!(3)
            );
            assert_eq!(
                serde_json::to_value(&main[0]).unwrap()["lastReplyAt"],
                serde_json::json!("2026-10-30T10:00:30Z")
            );
        })
        .await;
    }

    #[tokio::test]
    async fn timelines_threads_budget_and_search() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            assert_eq!(store.rooms().await.unwrap(), vec![r.clone()]);
            assert!(store.slug_taken("release", None).await.unwrap());
            assert!(!store.slug_taken("release", Some(r.id)).await.unwrap());

            let t0 = Utc::now();
            let ask = msg(&r, YOU, "what is left before we cut 0.9?", t0);
            store.add_message(&ask).await.unwrap();
            for (i, a) in ["dw/allan", "local/scout"].iter().enumerate() {
                store
                    .add_message(&msg(
                        &r,
                        a,
                        "the migration is still open",
                        t0 + chrono::Duration::seconds(i as i64 + 1),
                    ))
                    .await
                    .unwrap();
            }
            let mut reply = msg(
                &r,
                "dw/allan",
                "threaded detail",
                t0 + chrono::Duration::seconds(5),
            );
            reply.thread_id = Some(ask.id);
            store.add_message(&reply).await.unwrap();

            let main = store.messages(r.id, None, None, 50).await.unwrap();
            assert_eq!(main.len(), 3);
            assert_eq!(main[0].id, ask.id);
            assert_eq!(main[0].replies.map(|r| r.reply_count), Some(1));
            assert_eq!(
                store.messages(r.id, Some(ask.id), None, 50).await.unwrap(),
                vec![reply]
            );
            assert_eq!(store.messages(r.id, None, None, 1).await.unwrap().len(), 1);
            assert_eq!(store.agent_run(r.id, None).await.unwrap(), 2);
            assert_eq!(store.agent_run(r.id, Some(ask.id)).await.unwrap(), 1);

            let mut sourced = msg(
                &r,
                "dw/allan",
                "the answer",
                t0 + chrono::Duration::seconds(6),
            );
            sourced.source = Some(source_key("dw", "allan", "m-9"));
            sourced.delivery = Some(DeliveryKind::Approval);
            sourced.request_id = Some("req-1".into());
            let first = store.add_message(&sourced).await.unwrap();
            assert!(first.created);
            let sourced = RoomMessage {
                replies: Some(Replies {
                    reply_count: 0,
                    last_reply_at: None,
                }),
                ..sourced
            };
            assert_eq!(first.message, sourced);
            let again = store
                .add_message(&RoomMessage {
                    id: Uuid::new_v4(),
                    content: "posted twice".into(),
                    ..sourced.clone()
                })
                .await
                .unwrap();
            assert_eq!((again.created, again.message), (false, sourced));

            let hits = store.search("migrations", 10).await.unwrap();
            assert_eq!(hits.len(), 2);
            assert!(store.delete_room(r.id).await.unwrap());
            assert!(store
                .messages(r.id, None, None, 50)
                .await
                .unwrap()
                .is_empty());
        })
        .await;
    }
}
