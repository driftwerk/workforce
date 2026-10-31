//! How far the person has read each place of this Desk: a room's timeline,
//! a thread, a session, Activity, the overseer's brief, the update. Points
//! only move forward, except when the person marks something unread, so
//! several windows writing at once merge in SQL. What is unread is counted
//! here, from the points, by what owns the content.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio_postgres::Row;
use uuid::Uuid;

pub use workforce_core::desk::{Mark, ReadPoint};

use crate::rooms::{RoomSummary, Unread};
use crate::store::DeskStore;

/// A place new content lands for the person, as its read point is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadPlace {
    /// When Desk began keeping points: older content counts as read.
    Floor,
    Room(Uuid),
    Thread {
        room: Uuid,
        thread: Uuid,
    },
    Session(Uuid),
    Activity,
    Brief,
    Update,
}

impl fmt::Display for ReadPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Floor => f.write_str("floor"),
            Self::Room(room) => write!(f, "room:{room}"),
            Self::Thread { room, thread } => write!(f, "thread:{room}/{thread}"),
            Self::Session(id) => write!(f, "session:{id}"),
            Self::Activity => f.write_str("activity"),
            Self::Brief => f.write_str("brief"),
            Self::Update => f.write_str("update"),
        }
    }
}

impl FromStr for ReadPlace {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = |v: &str| Uuid::parse_str(v).map_err(|_| format!("{s} is no place of Desk's"));
        match s.split_once(':') {
            None => match s {
                "floor" => Ok(Self::Floor),
                "activity" => Ok(Self::Activity),
                "brief" => Ok(Self::Brief),
                "update" => Ok(Self::Update),
                _ => Err(format!("{s} is no place of Desk's")),
            },
            Some(("room", room)) => Ok(Self::Room(id(room)?)),
            Some(("session", session)) => Ok(Self::Session(id(session)?)),
            Some(("thread", rest)) => {
                let (room, thread) = rest
                    .split_once('/')
                    .ok_or_else(|| format!("{s} is no place of Desk's"))?;
                Ok(Self::Thread {
                    room: id(room)?,
                    thread: id(thread)?,
                })
            }
            Some(_) => Err(format!("{s} is no place of Desk's")),
        }
    }
}

impl Serialize for ReadPlace {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReadPlace {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// One place's point, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Read {
    pub place: ReadPlace,
    #[serde(flatten)]
    pub point: ReadPoint,
}

/// A write of one place's point.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReadPut {
    pub place: ReadPlace,
    pub point: ReadPoint,
    pub mark: Mark,
}

/// What Mark all read covers: everything, or one room and its threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllScope {
    Everything,
    Room(Uuid),
}

/// How much an unread item asks of the person: something waits on them,
/// something is for them, or something is only new.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Urgency {
    Needs,
    ForYou,
    New,
}

/// A session that came into a state worth reading after its point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionUnread {
    pub id: Uuid,
    pub since: DateTime<Utc>,
    pub urgency: Urgency,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Count {
    pub count: i64,
    pub last_at: Option<DateTime<Utc>>,
}

/// What is unread on this Desk, by place.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadSummary {
    pub floor: DateTime<Utc>,
    pub rooms: Vec<RoomSummary>,
    pub sessions: Vec<SessionUnread>,
    pub activity: Count,
    /// The brief's time, while it is unread.
    pub brief: Option<DateTime<Utc>>,
    /// The update's point, for Desk to compare with its updater.
    pub update: Option<ReadPoint>,
}

/// The point a place is read from: its own, or the floor when it is later.
pub fn effective(point: Option<&ReadPoint>, floor: DateTime<Utc>) -> DateTime<Utc> {
    point.map_or(floor, |p| p.at.max(floor))
}

fn read(row: &Row) -> Result<Read> {
    Ok(Read {
        place: row
            .try_get::<_, String>("place")?
            .parse()
            .map_err(|e: String| eyre::eyre!(e))?,
        point: ReadPoint {
            at: row.try_get("at")?,
            message: row.try_get("message")?,
        },
    })
}

/// What counts as unread in a room or a thread, over `room_messages`
/// unaliased: anyone's message but yours.
pub(crate) const COUNTED: &str = "room_messages.author <> 'you'";

impl DeskStore {
    /// Every point `owner` keeps.
    pub async fn reads(&self, owner: &str) -> Result<Vec<Read>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT place, at, message FROM reads WHERE owner = $1 ORDER BY place",
                &[&owner],
            )
            .await?;
        rows.iter().map(read).collect()
    }

    /// Writes each put, an advance never moving a point back, and answers
    /// the points that changed.
    pub async fn put_reads(&self, owner: &str, puts: &[ReadPut]) -> Result<Vec<Read>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        hold_points(&tx, owner).await?;
        let mut changed = Vec::new();
        for put in puts {
            let only_forward = match put.mark {
                Mark::Advance => " WHERE excluded.at > reads.at",
                Mark::Set => "",
            };
            let row = tx
                .query_opt(
                    &format!(
                        "INSERT INTO reads (owner, place, at, message, updated_at) \
                         VALUES ($1, $2, $3, $4, now()) ON CONFLICT (owner, place) DO UPDATE \
                         SET at = excluded.at, message = excluded.message, updated_at = now()\
                         {only_forward} RETURNING place, at, message"
                    ),
                    &[
                        &owner,
                        &put.place.to_string(),
                        &put.point.at,
                        &put.point.message,
                    ],
                )
                .await?;
            if let Some(row) = row {
                changed.push(read(&row)?);
            }
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// When `owner`'s points began: set the first time it is asked for.
    pub async fn floor(&self, owner: &str) -> Result<DateTime<Utc>> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO reads (owner, place, at, updated_at) VALUES ($1, 'floor', now(), now()) \
                 ON CONFLICT DO NOTHING",
                &[&owner],
            )
            .await?;
        Ok(client
            .query_one(
                "SELECT at FROM reads WHERE owner = $1 AND place = 'floor'",
                &[&owner],
            )
            .await?
            .get(0))
    }

    /// Forgets `owner`'s points of every place whose key starts `prefix`.
    pub async fn forget_reads(&self, owner: &str, prefix: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM reads WHERE owner = $1 AND left(place, length($2)) = $2",
                &[&owner, &prefix],
            )
            .await?;
        Ok(())
    }

    /// Every room's main timeline, with what is unread in it after its
    /// point: rooms with nothing unread are answered too, with zeros.
    pub async fn room_unread(
        &self,
        owner: &str,
        floor: DateTime<Utc>,
    ) -> Result<Vec<(Uuid, Unread)>> {
        let client = self.pool().get().await?;
        let for_you = crate::rooms::FOR_YOU;
        let rows = client
            .query(
                &format!(
                    "SELECT rooms.id AS room_id, u.count, u.for_you, u.first, u.last_at FROM rooms \
                     LEFT JOIN (SELECT room_messages.room_id, count(*) AS count, \
                       count(*) FILTER (WHERE {for_you}) AS for_you, \
                       (array_agg(room_messages.id ORDER BY created_at, room_messages.id))[1] AS first, \
                       max(created_at) AS last_at \
                     FROM room_messages LEFT JOIN reads ON reads.owner = $1 \
                       AND reads.place = 'room:' || room_messages.room_id \
                     WHERE thread_id IS NULL AND {COUNTED} AND created_at > greatest($2, reads.at) \
                     GROUP BY room_messages.room_id) u ON u.room_id = rooms.id \
                     ORDER BY rooms.name"
                ),
                &[&owner, &floor],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("room_id")?,
                    Unread {
                        count: row.try_get::<_, Option<i64>>("count")?.unwrap_or(0),
                        for_you: row.try_get::<_, Option<i64>>("for_you")?.unwrap_or(0),
                        first: row.try_get("first")?,
                        last_at: row.try_get("last_at")?,
                    },
                ))
            })
            .collect()
    }

    /// The newest [`THREADS_SUMMED`] threads with anything unread after
    /// their points: each its room, its first message and what is unread.
    pub async fn thread_unread(
        &self,
        owner: &str,
        floor: DateTime<Utc>,
    ) -> Result<Vec<(Uuid, Uuid, Unread)>> {
        let client = self.pool().get().await?;
        let for_you = crate::rooms::FOR_YOU;
        let rows = client
            .query(
                &format!(
                    "SELECT room_messages.room_id, thread_id, count(*) AS count, \
                       count(*) FILTER (WHERE {for_you}) AS for_you, \
                       (array_agg(room_messages.id ORDER BY created_at, room_messages.id))[1] AS first, \
                       max(created_at) AS last_at \
                     FROM room_messages LEFT JOIN reads ON reads.owner = $1 \
                       AND reads.place = 'thread:' || room_messages.room_id || '/' || thread_id \
                     WHERE thread_id IS NOT NULL AND {COUNTED} \
                       AND created_at > greatest($2, reads.at) \
                     GROUP BY room_messages.room_id, thread_id \
                     ORDER BY max(created_at) DESC LIMIT {THREADS_SUMMED}"
                ),
                &[&owner, &floor],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("room_id")?,
                    row.try_get("thread_id")?,
                    Unread {
                        count: row.try_get("count")?,
                        for_you: row.try_get("for_you")?,
                        first: row.try_get("first")?,
                        last_at: row.try_get("last_at")?,
                    },
                ))
            })
            .collect()
    }

    /// Activity's events after `after`, not your own.
    pub async fn activity_unread(&self, after: DateTime<Utc>) -> Result<Count> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT count(*), max(at) FROM activity WHERE at > $1 AND actor ->> 'kind' <> 'you'",
                &[&after],
            )
            .await?;
        Ok(Count {
            count: row.get(0),
            last_at: row.get(1),
        })
    }

    /// Moves each of `owner`'s places with something unread in `scope` to
    /// its newest item: every room's timeline and thread in it, by SQL, and
    /// `others` (sessions, Activity, the brief) as given, in one
    /// transaction. Answers the points that changed.
    pub async fn mark_all(
        &self,
        owner: &str,
        scope: AllScope,
        floor: DateTime<Utc>,
        others: &[ReadPut],
    ) -> Result<Vec<Read>> {
        let room = match scope {
            AllScope::Everything => None,
            AllScope::Room(room) => Some(room),
        };
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        hold_points(&tx, owner).await?;
        let mut changed = Vec::new();
        for (place, place_of, in_place) in [
            ("'room:' || room_id", "room_id", "thread_id IS NULL"),
            (
                "'thread:' || room_id || '/' || thread_id",
                "room_id, thread_id",
                "thread_id IS NOT NULL",
            ),
        ] {
            let rows = tx
                .query(
                    &format!(
                        "WITH newest AS (SELECT {place} AS place, max(created_at) AS at, \
                           (array_agg(id::text ORDER BY created_at DESC, id DESC))[1] AS message, \
                           max(created_at) FILTER (WHERE {COUNTED}) AS theirs \
                         FROM room_messages WHERE {in_place} AND ($2::uuid IS NULL OR room_id = $2) \
                         GROUP BY {place_of}) \
                         INSERT INTO reads (owner, place, at, message, updated_at) \
                         SELECT $1, n.place, n.at, n.message, now() FROM newest n \
                         LEFT JOIN reads r ON r.owner = $1 AND r.place = n.place \
                         WHERE n.theirs > greatest($3, r.at) \
                         ON CONFLICT (owner, place) DO UPDATE SET at = excluded.at, \
                           message = excluded.message, updated_at = now() \
                         WHERE excluded.at > reads.at RETURNING place, at, message"
                    ),
                    &[&owner, &room, &floor],
                )
                .await?;
            for row in &rows {
                changed.push(read(row)?);
            }
        }
        for put in others {
            let row = tx
                .query_opt(
                    "INSERT INTO reads (owner, place, at, message, updated_at) \
                     VALUES ($1, $2, $3, $4, now()) ON CONFLICT (owner, place) DO UPDATE \
                     SET at = excluded.at, message = excluded.message, updated_at = now() \
                     WHERE excluded.at > reads.at RETURNING place, at, message",
                    &[
                        &owner,
                        &put.place.to_string(),
                        &put.point.at,
                        &put.point.message,
                    ],
                )
                .await?;
            if let Some(row) = row {
                changed.push(read(&row)?);
            }
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Activity's newest event that isn't yours, as a point.
    pub async fn newest_activity(&self) -> Result<Option<ReadPoint>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT at, id FROM activity WHERE actor ->> 'kind' <> 'you' \
                 ORDER BY at DESC, id DESC LIMIT 1",
                &[],
            )
            .await?
            .map(|row| ReadPoint {
                at: row.get(0),
                message: row.get(1),
            }))
    }

    /// When each session came into the state it is in, as kept.
    pub async fn state_times(&self) -> Result<std::collections::HashMap<Uuid, DateTime<Utc>>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT id, state_at FROM sessions WHERE state_at IS NOT NULL",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }
}

/// Holds `owner`'s points for the rest of `tx`. Writes of several points
/// lock their rows in the order they come, so two windows writing the same
/// places in another order, or Mark all read beside them, would deadlock;
/// one writer at a time per owner can't.
async fn hold_points(tx: &tokio_postgres::Transaction<'_>, owner: &str) -> Result<()> {
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended('desk.reads:' || $1, 0))",
        &[&owner],
    )
    .await?;
    Ok(())
}

/// Threads a summary lists at most, the newest.
pub const THREADS_SUMMED: i64 = 50;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::tests::room_fixture;
    use crate::rooms::{RoomMessage, YOU};
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
            .unwrap()
            .to_utc()
    }

    fn at(s: i64) -> DateTime<Utc> {
        t0() + chrono::Duration::seconds(s)
    }

    fn put(place: ReadPlace, s: i64, mark: Mark) -> ReadPut {
        ReadPut {
            place,
            point: ReadPoint {
                at: at(s),
                message: None,
            },
            mark,
        }
    }

    #[test]
    fn a_place_reads_as_its_string_and_back() {
        let room = Uuid::from_u128(1);
        let thread = Uuid::from_u128(2);
        for (place, text) in [
            (ReadPlace::Floor, "floor".to_string()),
            (ReadPlace::Room(room), format!("room:{room}")),
            (
                ReadPlace::Thread { room, thread },
                format!("thread:{room}/{thread}"),
            ),
            (ReadPlace::Session(thread), format!("session:{thread}")),
            (ReadPlace::Activity, "activity".to_string()),
            (ReadPlace::Brief, "brief".to_string()),
            (ReadPlace::Update, "update".to_string()),
        ] {
            assert_eq!(place.to_string(), text);
            assert_eq!(text.parse::<ReadPlace>(), Ok(place));
            assert_eq!(
                serde_json::to_value(place).unwrap(),
                serde_json::json!(text)
            );
        }
        for bad in ["dm:local/allan", "room:atlas", "thread:x", "elsewhere"] {
            assert!(bad.parse::<ReadPlace>().is_err(), "{bad}");
        }
        let put: ReadPut = serde_json::from_value(serde_json::json!({
            "place": format!("room:{room}"),
            "point": { "at": "2026-10-30T10:00:00Z", "message": null },
            "mark": "set",
        }))
        .unwrap();
        assert_eq!((put.place, put.mark), (ReadPlace::Room(room), Mark::Set));
        assert_eq!(
            serde_json::from_value::<AllScope>(serde_json::json!({ "room": room })).unwrap(),
            AllScope::Room(room)
        );
        assert_eq!(
            serde_json::to_value(Read {
                place: ReadPlace::Brief,
                point: ReadPoint {
                    at: t0(),
                    message: Some("m-1".into())
                },
            })
            .unwrap(),
            serde_json::json!({ "place": "brief", "at": "2026-10-30T10:00:00Z", "message": "m-1" })
        );
    }

    #[test]
    fn a_point_is_read_from_the_floor_when_the_floor_is_later() {
        let point = ReadPoint {
            at: at(5),
            message: None,
        };
        assert_eq!(effective(Some(&point), at(1)), at(5));
        assert_eq!(effective(Some(&point), at(9)), at(9));
        assert_eq!(effective(None, at(3)), at(3));
    }

    #[tokio::test]
    async fn an_advance_never_moves_a_point_back_and_a_set_does() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let room = ReadPlace::Room(Uuid::new_v4());
            let changed = store
                .put_reads(YOU, &[put(room, 10, Mark::Advance)])
                .await
                .unwrap();
            assert_eq!(changed.len(), 1);
            assert!(store
                .put_reads(YOU, &[put(room, 5, Mark::Advance)])
                .await
                .unwrap()
                .is_empty());
            assert_eq!(store.reads(YOU).await.unwrap()[0].point.at, at(10));
            let set = store
                .put_reads(YOU, &[put(room, 5, Mark::Set)])
                .await
                .unwrap();
            assert_eq!(set[0].point.at, at(5), "marked unread from here");
            let (later, earlier) = (
                [put(room, 20, Mark::Advance)],
                [put(room, 15, Mark::Advance)],
            );
            let (a, b) = tokio::join!(store.put_reads(YOU, &later), store.put_reads(YOU, &earlier));
            a.unwrap();
            b.unwrap();
            assert_eq!(
                store.reads(YOU).await.unwrap()[0].point.at,
                at(20),
                "the later is kept"
            );
            assert!(
                store.reads("sarah").await.unwrap().is_empty(),
                "points are their owner's"
            );
            let other = ReadPlace::Activity;
            for s in 30..50 {
                let one = [put(room, s, Mark::Advance), put(other, s, Mark::Advance)];
                let another = [put(other, s, Mark::Advance), put(room, s, Mark::Advance)];
                let (a, b) =
                    tokio::join!(store.put_reads(YOU, &one), store.put_reads(YOU, &another));
                a.expect("two windows writing the same places in turn");
                b.expect("two windows writing the same places in turn");
            }
        })
        .await;
    }

    /// A session's state time is kept across restarts: it moves when its
    /// state does, not when anything else about it changes.
    #[tokio::test]
    async fn a_sessions_state_time_moves_with_its_state_alone() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut r = crate::store::tests::record_fixture("acme-web");
            r.updated_at = at(1);
            store.put(&r).await.unwrap();
            assert_eq!(store.state_times().await.unwrap()[&r.id], at(1));
            r.updated_at = at(2);
            r.goal = "ship the launch page".into();
            store.put(&r).await.unwrap();
            assert_eq!(store.state_times().await.unwrap()[&r.id], at(1));
            r.updated_at = at(3);
            r.state = crate::model::SessionState::Stopped;
            store.put(&r).await.unwrap();
            assert_eq!(store.state_times().await.unwrap()[&r.id], at(3));
        })
        .await;
    }

    #[tokio::test]
    async fn the_floor_is_set_once() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let first = store.floor(YOU).await.unwrap();
            assert_eq!(store.floor(YOU).await.unwrap(), first);
        })
        .await;
    }

    fn said(room: &crate::rooms::Room, author: &str, s: i64, thread: Option<Uuid>) -> RoomMessage {
        RoomMessage {
            id: Uuid::new_v4(),
            room_id: room.id,
            thread_id: thread,
            author: author.into(),
            content: "a line".into(),
            hop: 0,
            created_at: at(s),
            attachments: Vec::new(),
            source: None,
            delivery: None,
            request_id: None,
            in_reply_to: None,
            steps: Vec::new(),
            replies: None,
            reading: None,
        }
    }

    #[tokio::test]
    async fn the_summary_counts_what_is_after_each_read_point() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let atlas = room_fixture("atlas launch");
            let quiet = room_fixture("acme-web");
            store.put_room(&atlas).await.unwrap();
            store.put_room(&quiet).await.unwrap();
            let ask = said(&atlas, YOU, 1, None);
            store.add_message(&ask).await.unwrap();
            let answer = RoomMessage {
                in_reply_to: Some(ask.id),
                ..said(&atlas, "dw/allan", 2, None)
            };
            store.add_message(&answer).await.unwrap();
            let handed = RoomMessage {
                hop: 1,
                ..said(&atlas, "local/scout", 3, None)
            };
            store.add_message(&handed).await.unwrap();
            store
                .add_message(&said(&atlas, YOU, 4, None))
                .await
                .unwrap();
            for s in [5, 6] {
                store
                    .add_message(&said(&atlas, "dw/allan", s, Some(ask.id)))
                    .await
                    .unwrap();
            }

            let main = store.room_unread(YOU, t0()).await.unwrap();
            let (_, a) = main.iter().find(|(id, _)| *id == atlas.id).unwrap();
            assert_eq!(
                (a.count, a.for_you, a.first, a.last_at),
                (2, 1, Some(answer.id), Some(at(3))),
                "your own messages aren't counted, a hand-off is only new"
            );
            let (_, q) = main.iter().find(|(id, _)| *id == quiet.id).unwrap();
            assert_eq!(
                (q.count, q.first),
                (0, None),
                "a room with nothing unread, answered"
            );
            let threads = store.thread_unread(YOU, t0()).await.unwrap();
            assert_eq!(threads.len(), 1);
            assert_eq!((threads[0].1, threads[0].2.count), (ask.id, 2));

            store
                .put_reads(
                    YOU,
                    &[
                        put(ReadPlace::Room(atlas.id), 2, Mark::Advance),
                        put(
                            ReadPlace::Thread {
                                room: atlas.id,
                                thread: ask.id,
                            },
                            6,
                            Mark::Advance,
                        ),
                    ],
                )
                .await
                .unwrap();
            let main = store.room_unread(YOU, t0()).await.unwrap();
            let (_, a) = main.iter().find(|(id, _)| *id == atlas.id).unwrap();
            assert_eq!((a.count, a.for_you, a.first), (1, 0, Some(handed.id)));
            assert!(store.thread_unread(YOU, t0()).await.unwrap().is_empty());
            let later = store.room_unread(YOU, at(10)).await.unwrap();
            assert!(
                later.iter().all(|(_, u)| u.count == 0),
                "the floor counts as read"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn marking_a_room_read_covers_its_threads() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let atlas = room_fixture("atlas launch");
            let other = room_fixture("acme-web");
            store.put_room(&atlas).await.unwrap();
            store.put_room(&other).await.unwrap();
            let ask = said(&atlas, YOU, 1, None);
            store.add_message(&ask).await.unwrap();
            store
                .add_message(&said(&atlas, "dw/allan", 2, None))
                .await
                .unwrap();
            let reply = said(&atlas, "dw/allan", 3, Some(ask.id));
            store.add_message(&reply).await.unwrap();
            store
                .add_message(&said(&other, "dw/allan", 4, None))
                .await
                .unwrap();

            let changed = store
                .mark_all(YOU, AllScope::Room(atlas.id), t0(), &[])
                .await
                .unwrap();
            let places: Vec<_> = changed.iter().map(|r| r.place).collect();
            assert!(places.contains(&ReadPlace::Room(atlas.id)));
            assert!(places.contains(&ReadPlace::Thread {
                room: atlas.id,
                thread: ask.id
            }));
            assert!(
                !places.contains(&ReadPlace::Room(other.id)),
                "only the room asked"
            );
            let thread = changed
                .iter()
                .find(|r| {
                    r.place
                        == ReadPlace::Thread {
                            room: atlas.id,
                            thread: ask.id,
                        }
                })
                .unwrap();
            assert_eq!(thread.point.message, Some(reply.id.to_string()));
            assert!(
                store
                    .mark_all(YOU, AllScope::Room(atlas.id), t0(), &[])
                    .await
                    .unwrap()
                    .is_empty(),
                "nothing left unread, nothing changed"
            );
            let all = store
                .mark_all(YOU, AllScope::Everything, t0(), &[])
                .await
                .unwrap();
            assert_eq!(
                all.iter().map(|r| r.place).collect::<Vec<_>>(),
                [ReadPlace::Room(other.id)]
            );
        })
        .await;
    }
}
