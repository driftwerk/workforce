//! Rooms: you and several agents, on any hub, in one timeline. A room's
//! messages live here; each agent's turns stay in its own hub's session.
//! Agents can @-mention each other, so every agent message carries a hop
//! count and the hub refuses chains that run too long.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::store::DeskStore;

/// Agent-to-agent hand-offs allowed from one message of yours.
pub const MAX_HOPS: i16 = workforce_core::desk::MAX_HOPS as i16;
/// Agent messages allowed in a room (or thread) before you speak again.
pub const TURN_BUDGET: i64 = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub hub: String,
    pub agent: String,
}

impl Member {
    pub fn key(&self) -> String {
        format!("{}/{}", self.hub, self.agent)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
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
    })
}

const MESSAGE_COLUMNS: &str = "id, room_id, thread_id, author, content, hop, created_at";

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
                "INSERT INTO rooms (id, slug, name, members, default_member, created_at, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (id) DO UPDATE SET slug = $2, name = $3, \
                 members = $4, default_member = $5, updated_at = $7",
                &[
                    &r.id,
                    &r.slug,
                    &r.name,
                    &serde_json::to_value(&r.members)?,
                    &r.default_member,
                    &r.created_at,
                    &r.updated_at,
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

    pub async fn add_message(&self, m: &RoomMessage) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO room_messages (id, room_id, thread_id, author, content, hop, created_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7)",
                &[&m.id, &m.room_id, &m.thread_id, &m.author, &m.content, &m.hop, &m.created_at],
            )
            .await
            .context("saving room message")?;
        Ok(())
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

    /// Agent messages since you last spoke, in the main timeline or a thread.
    pub async fn agent_run(&self, room_id: Uuid, thread_id: Option<Uuid>) -> Result<i64> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT count(*) FROM room_messages WHERE room_id = $1 \
                 AND thread_id IS NOT DISTINCT FROM $2 AND author NOT IN ('you', 'desk') \
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
        Room {
            id: Uuid::new_v4(),
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
        }
    }

    #[test]
    fn slugs_are_channel_safe() {
        assert_eq!(slug_for("Release 0.9!"), "release-0-9");
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
            assert_eq!(
                store.messages(r.id, Some(ask.id), None, 50).await.unwrap(),
                vec![reply]
            );
            assert_eq!(store.messages(r.id, None, None, 1).await.unwrap().len(), 1);
            assert_eq!(store.agent_run(r.id, None).await.unwrap(), 2);
            assert_eq!(store.agent_run(r.id, Some(ask.id)).await.unwrap(), 1);

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
