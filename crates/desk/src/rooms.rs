//! Rooms: you and several agents, on any hub, in one timeline. A room's
//! messages live here; each agent's turns stay in its own hub's session.
//! Agents can @-mention each other, so every agent message carries a hop
//! count and the hub refuses chains that run too long.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use tokio_postgres::Row;
use uuid::Uuid;

use workforce_core::desk::{DeliveryKind, DESK_CHANNEL};

pub use workforce_core::desk::DeskStep as Step;
pub use workforce_core::desk::{MAX_STEPS, STEP_TOOL_CHARS};

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
    /// The message this one answers: the ask an agent's answer is to, the
    /// form your answer fills in, the post a hand-off answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<Uuid>,
    /// The tools the agent's turn ran, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// The thread message this main-timeline copy was posted to the room
    /// from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<Uuid>,
    /// On a main-timeline message as read back, its thread's replies.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub replies: Option<Replies>,
    /// What reading it back says of it; never written.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
}

/// What a message is, as read back beside what was kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    pub for_you: bool,
    /// A form you have answered.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub answered: bool,
    /// A thread message with a copy in the room's timeline.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub posted_to_room: bool,
}

/// How many replies a message's thread has, when the last came, and who
/// wrote them, latest first (at most three).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Replies {
    pub reply_count: i64,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub reply_authors: Vec<String>,
}

/// One end of a page: a message, or a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Message(Uuid),
    At(DateTime<Utc>),
}

impl FromStr for Bound {
    type Err = String;

    /// A message's id, else an RFC 3339 time.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(id) = Uuid::parse_str(s) {
            return Ok(Self::Message(id));
        }
        DateTime::parse_from_rfc3339(s)
            .map(|at| Self::At(at.to_utc()))
            .map_err(|_| format!("{s} is neither a message nor a time"))
    }
}

impl<'de> Deserialize<'de> for Bound {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Which side of a bound a page lies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}

impl Bound {
    /// The `(created_at, id)` a moment compares as, on `side` of it: before
    /// a moment is before every message at it, after one is after them all.
    /// A message's own key is the store's to read ([`DeskStore::cursor`]).
    pub fn key(self, side: Side) -> Option<(DateTime<Utc>, Uuid)> {
        match self {
            Self::Message(_) => None,
            Self::At(at) => Some((
                at,
                match side {
                    Side::Before => Uuid::nil(),
                    Side::After => Uuid::max(),
                },
            )),
        }
    }
}

/// Which messages of a place a page holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Newest,
    Before(Bound),
    After(Bound),
    Around(Uuid),
}

impl Span {
    /// How many of `limit` an `around` page takes at or before its target
    /// (the target included), and how many after it.
    pub fn split(limit: i64) -> (i64, i64) {
        let before = (limit + 1) / 2;
        (before, limit - before)
    }
}

/// A page of one place: the main timeline (`thread: None`) or a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub thread: Option<Uuid>,
    pub span: Span,
    pub limit: i64,
}

/// The messages a page holds at most, and when it doesn't say.
pub const PAGE_MAX: i64 = 200;
pub const PAGE_DEFAULT: i64 = 100;

impl Page {
    /// The page a query asks for: one span at most, `limit` held to
    /// 1..=[`PAGE_MAX`].
    pub fn of(
        thread: Option<Uuid>,
        before: Option<Bound>,
        after: Option<Bound>,
        around: Option<Uuid>,
        limit: Option<i64>,
    ) -> Result<Self, String> {
        let span = match (before, after, around) {
            (None, None, None) => Span::Newest,
            (Some(b), None, None) => Span::Before(b),
            (None, Some(a), None) => Span::After(a),
            (None, None, Some(id)) => Span::Around(id),
            _ => return Err("ask for one of before, after or around".into()),
        };
        Ok(Self {
            thread,
            span,
            limit: limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX),
        })
    }

    /// The newest `limit` messages of a place.
    pub fn newest(thread: Option<Uuid>, limit: i64) -> Self {
        Self {
            thread,
            span: Span::Newest,
            limit: limit.clamp(1, PAGE_MAX),
        }
    }
}

/// What is unread in one place of a room, after its read point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unread {
    pub count: i64,
    pub for_you: i64,
    /// The earliest unread message, to jump to when it isn't loaded.
    pub first: Option<Uuid>,
    pub last_at: Option<DateTime<Utc>>,
}

/// A thread with something unread: its first message, and what.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUnread {
    pub parent: RoomMessage,
    pub unread: Unread,
}

/// What is unread in a room: its timeline, and its threads with any.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomSummary {
    pub room_id: Uuid,
    pub main: Unread,
    pub threads: Vec<ThreadUnread>,
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

#[derive(Debug, Clone, Default, Deserialize)]
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
    /// The message of this room it answers.
    #[serde(default)]
    pub in_reply_to: Option<Uuid>,
    /// The tools an agent's turn ran on the way to it.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// A thread reply of yours also posted to the room's timeline.
    #[serde(default)]
    pub also_in_room: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub room_id: Uuid,
    pub message: RoomMessage,
    /// The text around the matches, each match between [`MATCH_START`] and
    /// [`MATCH_END`].
    pub snippet: String,
}

/// What a snippet brackets each match with: private-use characters, so a
/// snippet is never markup and no message can forge one.
pub const MATCH_START: char = '\u{E000}';
pub const MATCH_END: char = '\u{E001}';

/// What a search may narrow by besides its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Has {
    File,
    Approval,
}

/// A search of the rooms: its text, and what it is narrowed to.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilters {
    #[serde(default)]
    pub q: String,
    pub room: Option<Uuid>,
    /// An author key: `you`, `desk` or `hub/agent`.
    pub from: Option<String>,
    #[serde(default, deserialize_with = "comma_list")]
    pub has: Vec<Has>,
    #[serde(default)]
    pub in_thread: bool,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

/// Results a search answers at most, and when it doesn't say.
pub const SEARCH_MAX: i64 = 100;
pub const SEARCH_DEFAULT: i64 = 50;

impl SearchFilters {
    /// No text and nothing to narrow by: a search that finds nothing.
    pub fn is_empty(&self) -> bool {
        self.q.trim().is_empty()
            && self.room.is_none()
            && self.from.is_none()
            && self.has.is_empty()
            && !self.in_thread
            && self.after.is_none()
            && self.before.is_none()
    }

    fn has(&self, h: Has) -> bool {
        self.has.contains(&h)
    }
}

/// `has=file,approval` as a list.
fn comma_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Has>, D::Error> {
    String::deserialize(d)?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            serde_json::from_value(serde_json::Value::String(s.into()))
                .map_err(|_| serde::de::Error::custom(format!("has:{s} is nothing to search by")))
        })
        .collect()
}

/// What Activity records of a post when nothing else will: a delivery with
/// no turn behind it (a local agent's reminder or approval, anything from
/// another hub's agent) and another hub's agent's answer. A local agent's
/// answer, job and workflow come out of turns the crew logs already; your
/// messages, Desk's notes and copies are none of Activity's.
pub fn post_activity(m: &RoomMessage) -> Option<crate::crew::Event> {
    use crate::crew::{Event, Kind, Party};
    if m.copy_of.is_some() {
        return None;
    }
    let member = Member::parse(&m.author)?;
    let local = member.hub == LOCAL_HUB;
    let (kind, text) = match m.delivery {
        Some(kind @ (DeliveryKind::Reminder | DeliveryKind::Approval)) => {
            (Kind::Delivered, delivered(kind))
        }
        Some(kind) if !local => (Kind::Delivered, delivered(kind)),
        None if !local => (Kind::Replied, "replied"),
        _ => return None,
    };
    let party = match local {
        true => Party::agent(&member.agent, &member.agent),
        false => Party::agent(&m.author, &member.agent),
    };
    let event = Event::new(kind, party.clone(), Some(party), text).at_place(
        Some(crate::floor::Place::Room {
            room: m.room_id,
            thread: m.thread_id,
        }),
        Some(m.id),
    );
    Some(match (&m.request_id, kind) {
        (Some(request), Kind::Delivered) => event.approval(request),
        _ => event,
    })
}

/// What a delivery's event says the agent did.
fn delivered(kind: DeliveryKind) -> &'static str {
    match kind {
        DeliveryKind::Reminder => "sent a reminder",
        DeliveryKind::Approval => "asks you to approve",
        DeliveryKind::Job => "sent a job's answer",
        DeliveryKind::Workflow => "sent a workflow's update",
        DeliveryKind::CheckIn => "checked in",
        DeliveryKind::File => "sent a file",
        DeliveryKind::Resumed => "answered after a restart",
        DeliveryKind::Notice | DeliveryKind::Other => "sent a note",
    }
}

/// Why `steps` can't go on a message, when they can't: only an agent's
/// answer from a turn (`turn`) carries them, at most [`MAX_STEPS`], each
/// tool named in at most [`STEP_TOOL_CHARS`].
pub fn steps_refused(steps: &[Step], turn: bool) -> Option<String> {
    if steps.is_empty() {
        None
    } else if !turn {
        Some("only an agent's answer carries the steps its turn ran".into())
    } else if steps.len() > MAX_STEPS {
        Some(format!("a message carries at most {MAX_STEPS} steps"))
    } else if steps
        .iter()
        .any(|s| s.tool.chars().count() > STEP_TOOL_CHARS)
    {
        Some(format!(
            "a step's tool is named in at most {STEP_TOOL_CHARS} characters"
        ))
    } else {
        None
    }
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
        in_reply_to: row.try_get("in_reply_to")?,
        steps: serde_json::from_value(row.try_get("steps")?)?,
        copy_of: row.try_get("copy_of")?,
        replies: match row.try_get::<_, Option<Uuid>>("thread_id")? {
            Some(_) => None,
            None => Some(Replies {
                reply_count: row.try_get("reply_count")?,
                last_reply_at: row.try_get("last_reply_at")?,
                reply_authors: row.try_get("reply_authors")?,
            }),
        },
        reading: Some(Reading {
            for_you: row.try_get("for_you")?,
            answered: row.try_get("answered")?,
            posted_to_room: row.try_get("posted_to_room")?,
        }),
    })
}

fn delivery_name(kind: Option<DeliveryKind>) -> Result<Option<String>> {
    kind.map(|k| match serde_json::to_value(k)? {
        serde_json::Value::String(name) => Ok(name),
        other => eyre::bail!("a delivery kind named itself {other}"),
    })
    .transpose()
}

/// Whether a message is for you, over `room_messages`' columns unqualified:
/// what Desk wrote, and an agent's that answers one of yours or is a
/// delivery addressed to you. An answer from a hub too old to say what it
/// answers is yours when nobody handed it on (hop 0). A copy posted from a
/// thread is none of these.
pub(crate) const FOR_YOU: &str = "(author = 'desk' OR (author <> 'you' AND copy_of IS NULL AND ( \
     COALESCE(delivery IN ('reminder', 'job', 'workflow', 'check_in', 'approval', 'file', 'resumed'), false) \
     OR (delivery IS NULL AND CASE \
       WHEN in_reply_to IS NOT NULL THEN EXISTS (SELECT 1 FROM room_messages q \
         WHERE q.id = room_messages.in_reply_to AND q.author = 'you') \
       ELSE hop = 0 END))))";

/// A message's columns, with its thread's replies counted and what it is
/// read as: read from `room_messages` unaliased (a `RETURNING` included).
fn message_columns() -> String {
    format!(
        "id, room_id, thread_id, author, content, hop, created_at, attachments, source, delivery, \
         request_id, in_reply_to, steps, copy_of, \
         (SELECT count(*) FROM room_messages r WHERE r.thread_id = room_messages.id) AS reply_count, \
         (SELECT max(r.created_at) FROM room_messages r WHERE r.thread_id = room_messages.id) \
           AS last_reply_at, \
         ARRAY(SELECT r.author FROM room_messages r WHERE r.thread_id = room_messages.id \
           GROUP BY r.author ORDER BY max(r.created_at) DESC LIMIT 3) AS reply_authors, \
         {FOR_YOU} AS for_you, \
         EXISTS (SELECT 1 FROM room_messages a WHERE a.in_reply_to = room_messages.id \
           AND a.author = 'you') AS answered, \
         EXISTS (SELECT 1 FROM room_messages c WHERE c.copy_of = room_messages.id) \
           AS posted_to_room"
    )
}

static MESSAGE_COLUMNS: std::sync::LazyLock<String> = std::sync::LazyLock::new(message_columns);

/// What a message is kept once by.
enum Once<'a> {
    Source(&'a str),
    Copy(Uuid),
}

fn once(m: &RoomMessage) -> Option<Once<'_>> {
    m.source
        .as_deref()
        .map(Once::Source)
        .or(m.copy_of.map(Once::Copy))
}

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
        let cols = &*MESSAGE_COLUMNS;
        let inserted = client
            .query_opt(
                &format!(
                    "INSERT INTO room_messages \
                     (id, room_id, thread_id, author, content, hop, created_at, attachments, \
                     source, delivery, request_id, in_reply_to, steps, copy_of) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) \
                     ON CONFLICT DO NOTHING RETURNING {cols}"
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
                    &m.in_reply_to,
                    &serde_json::to_value(&m.steps)?,
                    &m.copy_of,
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
        let kept = match once(m) {
            Some(Once::Source(source)) => self.by_source(source).await?,
            Some(Once::Copy(original)) => self.copy_of(original).await?,
            None => None,
        };
        Ok(Posted {
            message: kept
                .ok_or_else(|| eyre::eyre!("room message {} was neither kept nor found", m.id))?,
            created: false,
        })
    }

    /// The copy in the room's timeline of the thread message `original`.
    pub async fn copy_of(&self, original: Uuid) -> Result<Option<RoomMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "SELECT {} FROM room_messages WHERE copy_of = $1",
                    *MESSAGE_COLUMNS
                ),
                &[&original],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// The message kept under `source`, if any.
    pub async fn by_source(&self, source: &str) -> Result<Option<RoomMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "SELECT {} FROM room_messages WHERE source = $1",
                    *MESSAGE_COLUMNS
                ),
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
                &format!(
                    "SELECT {} FROM room_messages WHERE id = $1",
                    *MESSAGE_COLUMNS
                ),
                &[&id],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// The messages with these ids, wherever they are.
    pub async fn messages_by_id(&self, ids: &[Uuid]) -> Result<Vec<RoomMessage>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {} FROM room_messages WHERE id = ANY($1)",
                    *MESSAGE_COLUMNS
                ),
                &[&ids],
            )
            .await?;
        rows.iter().map(message).collect()
    }

    /// Where `id` sits in `room`'s order, and in which thread: none when
    /// it isn't a message of that room.
    pub async fn cursor(
        &self,
        room: Uuid,
        id: Uuid,
    ) -> Result<Option<(DateTime<Utc>, Uuid, Option<Uuid>)>> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT created_at, thread_id FROM room_messages WHERE room_id = $1 AND id = $2",
                &[&room, &id],
            )
            .await?
            .map(|row| (row.get(0), id, row.get(1))))
    }

    /// The `(created_at, id)` a bound compares as, on `side` of it: none for
    /// a message that isn't the room's.
    async fn key(
        &self,
        room: Uuid,
        bound: Bound,
        side: Side,
    ) -> Result<Option<(DateTime<Utc>, Uuid)>> {
        Ok(match bound {
            Bound::Message(id) => self.cursor(room, id).await?.map(|(at, id, _)| (at, id)),
            at => at.key(side),
        })
    }

    /// One page of a room's main timeline or of one thread, oldest first,
    /// ordered by `(created_at, id)`: none when a bound names a message
    /// that isn't the room's, or an `around` one that is in another place.
    pub async fn messages(&self, room_id: Uuid, page: &Page) -> Result<Option<Vec<RoomMessage>>> {
        let cols = &*MESSAGE_COLUMNS;
        // `=` or `IS NULL`, never `IS NOT DISTINCT FROM`, which no index
        // serves: a page is read along `room_messages_place`.
        let place = match page.thread {
            Some(_) => "room_id = $1 AND thread_id = $2",
            None => "room_id = $1 AND thread_id IS NULL AND $2::uuid IS NULL",
        };
        let older = |cmp: &str| {
            format!(
                "SELECT {cols} FROM room_messages WHERE {place} AND (created_at, id) {cmp} ($3, $4) \
                 ORDER BY created_at DESC, id DESC LIMIT $5"
            )
        };
        let newer = |cmp: &str, limit: &str| {
            format!(
                "SELECT {cols} FROM room_messages WHERE {place} AND (created_at, id) {cmp} ($3, $4) \
                 ORDER BY created_at, id LIMIT {limit}"
            )
        };
        let client = self.pool().get().await?;
        let rows = match page.span {
            Span::Newest => {
                client
                    .query(
                        &format!(
                            "SELECT * FROM (SELECT {cols} FROM room_messages WHERE {place} \
                             ORDER BY created_at DESC, id DESC LIMIT $3) m ORDER BY created_at, id"
                        ),
                        &[&room_id, &page.thread, &page.limit],
                    )
                    .await?
            }
            Span::Before(bound) => {
                let Some((at, id)) = self.key(room_id, bound, Side::Before).await? else {
                    return Ok(None);
                };
                client
                    .query(
                        &format!("SELECT * FROM ({}) m ORDER BY created_at, id", older("<")),
                        &[&room_id, &page.thread, &at, &id, &page.limit],
                    )
                    .await?
            }
            Span::After(bound) => {
                let Some((at, id)) = self.key(room_id, bound, Side::After).await? else {
                    return Ok(None);
                };
                client
                    .query(
                        &newer(">", "$5"),
                        &[&room_id, &page.thread, &at, &id, &page.limit],
                    )
                    .await?
            }
            Span::Around(target) => {
                let Some((at, id, thread)) = self.cursor(room_id, target).await? else {
                    return Ok(None);
                };
                if thread != page.thread {
                    return Ok(None);
                }
                let (before, after) = Span::split(page.limit);
                client
                    .query(
                        &format!(
                            "SELECT * FROM (({}) UNION ALL ({})) m ORDER BY created_at, id",
                            older("<="),
                            newer(">", "$6")
                        ),
                        &[&room_id, &page.thread, &at, &id, &before, &after],
                    )
                    .await?
            }
        };
        rows.iter().map(message).collect::<Result<_>>().map(Some)
    }

    /// Agent messages since you last spoke, in the main timeline or a
    /// thread; deliveries are none of them.
    pub async fn agent_run(&self, room_id: Uuid, thread_id: Option<Uuid>) -> Result<i64> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT count(*) FROM room_messages WHERE room_id = $1 \
                 AND thread_id IS NOT DISTINCT FROM $2 AND author NOT IN ('you', 'desk') \
                 AND delivery IS NULL AND copy_of IS NULL \
                 AND created_at > COALESCE((SELECT max(created_at) FROM room_messages \
                   WHERE room_id = $1 AND thread_id IS NOT DISTINCT FROM $2 AND author = 'you'), \
                   '-infinity'::timestamptz)",
                &[&room_id, &thread_id],
            )
            .await?;
        Ok(row.get(0))
    }

    /// Messages of every room (or one) matching `f`: by rank, then newest,
    /// when it has text, newest alone when it narrows by filters only. Each
    /// with a snippet of its text, matches bracketed.
    pub async fn search(&self, f: &SearchFilters) -> Result<Vec<SearchHit>> {
        let client = self.pool().get().await?;
        let sentinels = format!("{MATCH_START}{MATCH_END}");
        let options = format!(
            "StartSel={MATCH_START}, StopSel={MATCH_END}, MaxFragments=2, MinWords=8, \
             MaxWords=24, FragmentDelimiter=\" … \""
        );
        let q = f.q.trim();
        let limit = f.limit.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX);
        let rows = client
            .query(
                &format!(
                    "SELECT {}, CASE WHEN $1 = '' THEN left(translate(content, $9, ''), 200) \
                       ELSE ts_headline('english', translate(content, $9, ''), \
                         websearch_to_tsquery('english', $1), $10) END AS snippet \
                     FROM room_messages \
                     WHERE ($1 = '' OR search @@ websearch_to_tsquery('english', $1)) \
                     AND ($2::uuid IS NULL OR room_id = $2) \
                     AND ($3::text IS NULL OR author = $3) \
                     AND (NOT $4 OR attachments <> '[]') \
                     AND (NOT $5 OR delivery = 'approval') \
                     AND (NOT $6 OR thread_id IS NOT NULL) \
                     AND ($7::timestamptz IS NULL OR created_at >= $7) \
                     AND ($8::timestamptz IS NULL OR created_at < $8) \
                     ORDER BY CASE WHEN $1 = '' THEN 0 \
                       ELSE ts_rank(search, websearch_to_tsquery('english', $1)) END DESC, \
                       created_at DESC, id DESC \
                     LIMIT $11",
                    *MESSAGE_COLUMNS
                ),
                &[
                    &q,
                    &f.room,
                    &f.from,
                    &f.has(Has::File),
                    &f.has(Has::Approval),
                    &f.in_thread,
                    &f.after,
                    &f.before,
                    &sentinels,
                    &options,
                    &limit,
                ],
            )
            .await?;
        rows.iter()
            .map(|r| {
                let m = message(r)?;
                Ok(SearchHit {
                    room_id: m.room_id,
                    message: m,
                    snippet: r.try_get("snippet")?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
            in_reply_to: None,
            steps: Vec::new(),
            copy_of: None,
            replies: None,
            reading: None,
        }
    }

    /// A message as reading it back gives it.
    fn read(m: &RoomMessage, replies: Option<Replies>, reading: Reading) -> RoomMessage {
        RoomMessage {
            replies,
            reading: Some(reading),
            ..m.clone()
        }
    }

    async fn newest(store: &DeskStore, r: &Room, thread: Option<Uuid>) -> Vec<RoomMessage> {
        store
            .messages(r.id, &Page::newest(thread, 50))
            .await
            .unwrap()
            .unwrap()
    }

    async fn page(
        store: &DeskStore,
        r: &Room,
        thread: Option<Uuid>,
        span: Span,
        limit: i64,
    ) -> Vec<Uuid> {
        store
            .messages(
                r.id,
                &Page {
                    thread,
                    span,
                    limit,
                },
            )
            .await
            .unwrap()
            .expect("the bound is the room's")
            .iter()
            .map(|m| m.id)
            .collect()
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-30T10:00:00Z")
            .unwrap()
            .to_utc()
    }

    #[test]
    fn a_bound_is_a_message_or_a_moment() {
        let id = Uuid::from_u128(9);
        assert_eq!(id.to_string().parse(), Ok(Bound::Message(id)));
        assert_eq!(
            "2026-10-30T10:00:00.5+01:00".parse(),
            Ok(Bound::At(t0() - chrono::Duration::milliseconds(3_599_500)))
        );
        assert!("yesterday".parse::<Bound>().is_err());
        assert_eq!(Bound::At(t0()).key(Side::Before), Some((t0(), Uuid::nil())));
        assert_eq!(Bound::At(t0()).key(Side::After), Some((t0(), Uuid::max())));
        assert_eq!(Bound::Message(id).key(Side::After), None);
    }

    #[test]
    fn a_page_asks_for_one_span_and_holds_its_limit() {
        let id = Uuid::from_u128(9);
        let at = Some(Bound::At(t0()));
        assert!(Page::of(None, at, at, None, None).is_err());
        assert!(Page::of(None, at, None, Some(id), None).is_err());
        assert!(Page::of(None, None, at, Some(id), None).is_err());
        let p = Page::of(Some(id), None, None, Some(id), Some(0)).unwrap();
        assert_eq!((p.thread, p.span, p.limit), (Some(id), Span::Around(id), 1));
        assert_eq!(
            Page::of(None, None, at, None, Some(1_000)).unwrap().limit,
            PAGE_MAX
        );
        let p = Page::of(None, None, None, None, None).unwrap();
        assert_eq!((p.span, p.limit), (Span::Newest, PAGE_DEFAULT));
        assert_eq!(Span::split(100), (50, 50));
        assert_eq!(Span::split(3), (2, 1));
        assert_eq!(Span::split(1), (1, 0), "the target itself, at the least");
    }

    #[test]
    fn steps_go_only_on_an_agents_answer_and_are_held_to_their_limits() {
        let step = |tool: &str| Step::of(tool, true, 5);
        assert_eq!(steps_refused(&[], false), None);
        assert_eq!(steps_refused(&[step("web.search")], true), None);
        assert!(steps_refused(&[step("web.search")], false).is_some());
        assert!(steps_refused(&vec![step("web.search"); MAX_STEPS + 1], true).is_some());
        let long = Step {
            tool: "x".repeat(STEP_TOOL_CHARS + 1),
            ok: true,
            ms: 1,
        };
        assert!(steps_refused(&[long], true).is_some());
    }

    #[tokio::test]
    async fn pages_walk_a_place_without_gaps_or_repeats() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let at = |s: i64| t0() + chrono::Duration::seconds(s);
            let mut kept = Vec::new();
            for s in [0, 1, 1, 1, 2] {
                let m = msg(&r, YOU, "a line", at(s));
                store.add_message(&m).await.unwrap();
                kept.push((m.created_at, m.id));
            }
            kept.sort();
            let order: Vec<Uuid> = kept.iter().map(|(_, id)| *id).collect();

            let mut back = page(&store, &r, None, Span::Newest, 1).await;
            loop {
                let older = page(&store, &r, None, Span::Before(Bound::Message(back[0])), 1).await;
                let Some(&first) = older.first() else { break };
                back.insert(0, first);
            }
            assert_eq!(back, order, "back to the start, one at a time");

            let mut forth = vec![order[0]];
            loop {
                let last = *forth.last().unwrap();
                let newer = page(&store, &r, None, Span::After(Bound::Message(last)), 1).await;
                let Some(&next) = newer.first() else { break };
                forth.push(next);
            }
            assert_eq!(forth, order, "on to the end, one at a time");

            assert_eq!(
                page(&store, &r, None, Span::Before(Bound::At(at(1))), 10).await,
                order[..1],
                "before a moment is before everything at it"
            );
            assert_eq!(
                page(&store, &r, None, Span::After(Bound::At(at(1))), 10).await,
                order[4..],
                "after a moment is after everything at it"
            );
            let elsewhere = room_fixture("other");
            store.put_room(&elsewhere).await.unwrap();
            let stranger = msg(&elsewhere, YOU, "hi", at(0));
            store.add_message(&stranger).await.unwrap();
            let bad = Page {
                thread: None,
                span: Span::Before(Bound::Message(stranger.id)),
                limit: 5,
            };
            assert_eq!(store.messages(r.id, &bad).await.unwrap(), None);
        })
        .await;
    }

    #[tokio::test]
    async fn around_a_message_holds_it_with_what_came_before_and_after() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let mut ids = Vec::new();
            for s in 0..10 {
                let m = msg(&r, YOU, "a line", t0() + chrono::Duration::seconds(s));
                store.add_message(&m).await.unwrap();
                ids.push(m.id);
            }
            assert_eq!(
                page(&store, &r, None, Span::Around(ids[4]), 4).await,
                ids[3..7]
            );
            assert_eq!(
                page(&store, &r, None, Span::Around(ids[0]), 4).await,
                ids[..3],
                "short before it at the start"
            );
            assert_eq!(
                page(&store, &r, None, Span::Around(ids[9]), 4).await,
                ids[8..],
                "short after it at the end"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn around_a_reply_is_refused_on_the_main_timeline() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let ask = msg(&r, YOU, "ship it?", t0());
            store.add_message(&ask).await.unwrap();
            let mut reply = msg(&r, "dw/allan", "yes", t0() + chrono::Duration::seconds(1));
            reply.thread_id = Some(ask.id);
            store.add_message(&reply).await.unwrap();
            let around = |thread| Page {
                thread,
                span: Span::Around(reply.id),
                limit: 10,
            };
            assert_eq!(store.messages(r.id, &around(None)).await.unwrap(), None);
            assert_eq!(
                store.cursor(r.id, reply.id).await.unwrap().map(|c| c.2),
                Some(Some(ask.id)),
                "the cursor says which thread it is in"
            );
            assert_eq!(
                page(&store, &r, Some(ask.id), Span::Around(reply.id), 10).await,
                [reply.id]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_main_message_names_its_latest_three_repliers() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let ask = msg(&r, YOU, "who has seen the launch plan?", t0());
            store.add_message(&ask).await.unwrap();
            for (s, author) in [
                (1, "dw/allan"),
                (2, "local/scout"),
                (3, YOU),
                (4, "dw/sarah"),
                (5, "dw/allan"),
            ] {
                let mut reply = msg(&r, author, "seen", t0() + chrono::Duration::seconds(s));
                reply.thread_id = Some(ask.id);
                store.add_message(&reply).await.unwrap();
            }
            let parent = store.message(ask.id).await.unwrap().unwrap();
            assert_eq!(
                parent.replies.unwrap().reply_authors,
                ["dw/allan", "dw/sarah", "you"]
            );
            assert_eq!(
                serde_json::to_value(&newest(&store, &r, None).await[0]).unwrap()["replyAuthors"],
                serde_json::json!(["dw/allan", "dw/sarah", "you"])
            );
        })
        .await;
    }

    /// What Desk writes, an agent's answer to one of yours, and a delivery
    /// to you are for you; a hand-off and a notice are not. An answer from a
    /// hub too old to say what it answers is yours only when nobody handed
    /// it on.
    #[tokio::test]
    async fn for_you_is_desks_a_reply_to_you_or_a_delivery_to_you() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let yours = msg(&r, YOU, "what's left?", t0());
            store.add_message(&yours).await.unwrap();
            let theirs = msg(&r, "dw/allan", "@scout can you check?", t0());
            store.add_message(&theirs).await.unwrap();
            let agent =
                |hop: i16, in_reply_to: Option<Uuid>, delivery: Option<DeliveryKind>| RoomMessage {
                    hop,
                    in_reply_to,
                    delivery,
                    ..msg(&r, "local/scout", "an answer", t0())
                };
            let mut cases = vec![
                ("Desk's note", msg(&r, DESK, "a note", t0()), true),
                ("your own", msg(&r, YOU, "more", t0()), false),
                ("an answer to you", agent(0, Some(yours.id), None), true),
                (
                    "an answer to you, after a restart",
                    agent(MAX_HOPS, Some(yours.id), None),
                    true,
                ),
                (
                    "a hand-off's answer",
                    agent(1, Some(theirs.id), None),
                    false,
                ),
                ("an older hub's answer", agent(0, None, None), true),
                ("an older hub's hand-off", agent(1, None, None), false),
            ];
            for kind in [
                DeliveryKind::Reminder,
                DeliveryKind::Job,
                DeliveryKind::Workflow,
                DeliveryKind::CheckIn,
                DeliveryKind::Approval,
                DeliveryKind::File,
                DeliveryKind::Resumed,
            ] {
                cases.push(("a delivery to you", agent(0, None, Some(kind)), true));
            }
            cases.push((
                "a notice",
                agent(0, None, Some(DeliveryKind::Notice)),
                false,
            ));
            for (what, m, for_you) in cases {
                let posted = store.add_message(&m).await.unwrap().message;
                assert_eq!(posted.reading.unwrap().for_you, for_you, "{what}");
                let read = store.message(m.id).await.unwrap().unwrap();
                assert_eq!(read.reading.unwrap().for_you, for_you, "{what}, read again");
            }
            assert_eq!(
                serde_json::to_value(store.message(yours.id).await.unwrap().unwrap()).unwrap()
                    ["forYou"],
                serde_json::json!(false)
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_form_is_answered_once_you_reply_to_it() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("release");
            store.put_room(&r).await.unwrap();
            let form = msg(&r, "dw/allan", "```wf-form\n{}\n```", t0());
            store.add_message(&form).await.unwrap();
            let answered = |m: RoomMessage| m.reading.unwrap().answered;
            assert!(!answered(store.message(form.id).await.unwrap().unwrap()));
            let handed = RoomMessage {
                in_reply_to: Some(form.id),
                ..msg(
                    &r,
                    "local/scout",
                    "noted",
                    t0() + chrono::Duration::seconds(1),
                )
            };
            store.add_message(&handed).await.unwrap();
            assert!(
                !answered(store.message(form.id).await.unwrap().unwrap()),
                "an agent's message is no answer of yours"
            );
            let reply = RoomMessage {
                in_reply_to: Some(form.id),
                ..msg(
                    &r,
                    YOU,
                    "{\"ship\": true}",
                    t0() + chrono::Duration::seconds(2),
                )
            };
            store.add_message(&reply).await.unwrap();
            let read = store.message(form.id).await.unwrap().unwrap();
            assert!(answered(read.clone()));
            assert_eq!(
                serde_json::to_value(&read).unwrap()["answered"],
                serde_json::json!(true)
            );
            assert_eq!(
                store.message(reply.id).await.unwrap().unwrap().in_reply_to,
                Some(form.id)
            );
        })
        .await;
    }

    fn search(q: &str) -> SearchFilters {
        SearchFilters {
            q: q.into(),
            ..SearchFilters::default()
        }
    }

    #[tokio::test]
    async fn search_filters_narrow_by_room_author_files_approvals_threads_and_dates() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let atlas = room_fixture("atlas launch");
            let web = room_fixture("acme-web");
            store.put_room(&atlas).await.unwrap();
            store.put_room(&web).await.unwrap();
            let at = |s: i64| t0() + chrono::Duration::seconds(s);
            let ask = msg(&atlas, YOU, "is the launch page ready?", at(0));
            store.add_message(&ask).await.unwrap();
            let mut reply = msg(&atlas, "dw/allan", "the launch page is ready", at(10));
            reply.thread_id = Some(ask.id);
            store.add_message(&reply).await.unwrap();
            let mut approval = msg(&atlas, "dw/allan", "may I publish the launch page?", at(20));
            approval.delivery = Some(DeliveryKind::Approval);
            store.add_message(&approval).await.unwrap();
            let mut file: RoomMessage = msg(&web, "local/scout", "launch page screenshot", at(30));
            file.attachments = serde_json::from_value(serde_json::json!([{
                "hub": "local", "agent": "scout", "artifact_id": "a1", "filename": "page.png",
                "media_type": "image/png", "size": 10
            }]))
            .unwrap();
            store.add_message(&file).await.unwrap();
            let ids =
                |hits: Vec<SearchHit>| hits.into_iter().map(|h| h.message.id).collect::<Vec<_>>();

            assert_eq!(ids(store.search(&search("launch")).await.unwrap()).len(), 4);
            let mut f = search("launch");
            f.room = Some(web.id);
            assert_eq!(ids(store.search(&f).await.unwrap()), [file.id]);
            let mut f = search("launch");
            f.from = Some("dw/allan".into());
            f.in_thread = true;
            assert_eq!(ids(store.search(&f).await.unwrap()), [reply.id]);
            let mut f = search("");
            f.has = vec![Has::Approval];
            assert_eq!(ids(store.search(&f).await.unwrap()), [approval.id]);
            let mut f = search("");
            f.has = vec![Has::File];
            assert_eq!(ids(store.search(&f).await.unwrap()), [file.id]);
            let mut f = search("launch");
            f.after = Some(at(10));
            f.before = Some(at(30));
            assert_eq!(
                ids(store.search(&f).await.unwrap()),
                [approval.id, reply.id]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_snippet_brackets_its_matches_with_the_sentinels() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("atlas launch");
            store.put_room(&r).await.unwrap();
            let forged =
                format!("the migration is done {MATCH_START}not a match{MATCH_END} at last");
            store
                .add_message(&msg(&r, "dw/allan", &forged, t0()))
                .await
                .unwrap();
            let hits = store.search(&search("migration")).await.unwrap();
            assert!(
                hits[0].snippet.contains(&format!(
                    "{MATCH_START}migration{MATCH_END} is done not a match"
                )),
                "{}",
                hits[0].snippet
            );
            assert_eq!(
                hits[0].snippet.matches(MATCH_START).count(),
                1,
                "the forged brackets are stripped"
            );
            assert_eq!(
                serde_json::to_value(&hits[0]).unwrap()["roomId"],
                serde_json::json!(r.id)
            );
        })
        .await;
    }

    #[tokio::test]
    async fn filters_alone_search_newest_first() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let r = room_fixture("atlas launch");
            store.put_room(&r).await.unwrap();
            let long = "x".repeat(300);
            let older = msg(&r, "dw/allan", &long, t0());
            let newer = msg(&r, "dw/allan", "later", t0() + chrono::Duration::seconds(1));
            store.add_message(&older).await.unwrap();
            store.add_message(&newer).await.unwrap();
            store
                .add_message(&msg(&r, YOU, "mine", t0()))
                .await
                .unwrap();
            let mut f = search("");
            f.from = Some("dw/allan".into());
            let hits = store.search(&f).await.unwrap();
            assert_eq!(
                hits.iter().map(|h| h.message.id).collect::<Vec<_>>(),
                [newer.id, older.id]
            );
            assert_eq!(hits[1].snippet.chars().count(), 200);
            assert!(SearchFilters::default().is_empty());
            assert!(!f.is_empty());
        })
        .await;
    }

    #[test]
    fn a_post_is_activity_only_when_no_turn_records_it() {
        use crate::crew::Kind;
        let r = room_fixture("atlas launch");
        let delivered = |author: &str, kind| RoomMessage {
            delivery: Some(kind),
            request_id: Some("req-1".into()),
            ..msg(&r, author, "x", t0())
        };
        let event = post_activity(&delivered("local/scout", DeliveryKind::Approval)).unwrap();
        assert_eq!(
            (
                event.kind,
                event.text.as_str(),
                event.approval_id.as_deref()
            ),
            (Kind::Delivered, "asks you to approve", Some("req-1"))
        );
        assert_eq!(
            (event.actor.id.as_str(), event.actor.name.as_str()),
            ("scout", "scout")
        );
        assert_eq!(
            event.place,
            Some(crate::floor::Place::Room {
                room: r.id,
                thread: None
            })
        );
        assert!(post_activity(&delivered("local/scout", DeliveryKind::Reminder)).is_some());
        assert!(
            post_activity(&delivered("local/scout", DeliveryKind::Job)).is_none(),
            "its turn is logged already"
        );
        let job = post_activity(&delivered("dw/allan", DeliveryKind::Job)).unwrap();
        assert_eq!(
            (job.kind, job.actor.id.as_str()),
            (Kind::Delivered, "dw/allan")
        );
        let mut answer = msg(&r, "dw/allan", "the migration", t0());
        answer.thread_id = Some(Uuid::from_u128(5));
        let replied = post_activity(&answer).unwrap();
        assert_eq!(
            (replied.kind, replied.message),
            (Kind::Replied, Some(answer.id))
        );
        assert_eq!(replied.approval_id, None);
        assert!(post_activity(&msg(&r, "local/scout", "an answer", t0())).is_none());
        assert!(post_activity(&msg(&r, YOU, "mine", t0())).is_none());
        assert!(post_activity(&msg(&r, DESK, "a note", t0())).is_none());
        assert!(post_activity(&RoomMessage {
            copy_of: Some(Uuid::from_u128(5)),
            ..msg(&r, "dw/allan", "copied", t0())
        })
        .is_none());
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
            let main = newest(&store, &r, None).await;
            let replies: Vec<_> = main.iter().map(|m| m.replies.clone()).collect();
            let authors = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert_eq!(
                replies,
                [
                    Some(Replies {
                        reply_count: 3,
                        last_reply_at: Some(at(30)),
                        reply_authors: authors(&["dw/allan"]),
                    }),
                    Some(Replies {
                        reply_count: 1,
                        last_reply_at: Some(at(15)),
                        reply_authors: authors(&["dw/allan"]),
                    }),
                    Some(Replies {
                        reply_count: 0,
                        last_reply_at: None,
                        reply_authors: vec![],
                    }),
                ]
            );
            assert_eq!(
                store.message(busy.id).await.unwrap().unwrap().replies,
                replies[0]
            );
            let thread = newest(&store, &r, Some(busy.id)).await;
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

            let main = newest(&store, &r, None).await;
            assert_eq!(main.len(), 3);
            assert_eq!(main[0].id, ask.id);
            assert_eq!(main[0].replies.as_ref().map(|r| r.reply_count), Some(1));
            assert_eq!(
                newest(&store, &r, Some(ask.id)).await,
                vec![read(
                    &reply,
                    None,
                    Reading {
                        for_you: true,
                        answered: false,
                        posted_to_room: false,
                    }
                )]
            );
            assert_eq!(
                store
                    .messages(r.id, &Page::newest(None, 1))
                    .await
                    .unwrap()
                    .unwrap()
                    .len(),
                1
            );
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
            let sourced = read(
                &sourced,
                Some(Replies {
                    reply_count: 0,
                    last_reply_at: None,
                    reply_authors: vec![],
                }),
                Reading {
                    for_you: true,
                    answered: false,
                    posted_to_room: false,
                },
            );
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

            let hits = store
                .search(&SearchFilters {
                    q: "migrations".into(),
                    ..SearchFilters::default()
                })
                .await
                .unwrap();
            assert_eq!(hits.len(), 2);
            assert!(store.delete_room(r.id).await.unwrap());
            assert!(newest(&store, &r, None).await.is_empty());
        })
        .await;
    }
}
