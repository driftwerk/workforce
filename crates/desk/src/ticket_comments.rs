//! A ticket's comments: its Jira or Linear issue's, read when the ticket is
//! opened (never in a sync pass, which would spend its calls on every
//! issue), and Desk's own: the person's on a ticket of their own, and the
//! notes sessions and agents leave, which stay in Desk.
//!
//! Who wrote one is said in this order of trust:
//! 1. `posted_by`, recorded when Desk posted it: the one record of which
//!    person, session or agent it was, and the only thing that makes a
//!    comment Desk's or the person's own;
//! 2. an app (Linear's `botActor`, a Jira author whose account is an app):
//!    an agent there, by the name it goes by;
//! 3. anyone else: a person there, whatever their words say. A remote name
//!    that is one Desk gives its own ("You", "Desk") is said with where it
//!    is from, so no one there passes for either.
//!
//! Every remote comment's body is someone else's words: kept as text,
//! flagged when the hub warned it reads as instructions, and fenced
//! wherever a model reads it.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::board::System;
use crate::board_sync::unfence;
use crate::crew::Party;
use crate::store::DeskStore;

/// The longest comment kept or posted.
pub const BODY_MAX: usize = 10_000;
/// The most of an issue's comments read.
pub const READ_MAX: usize = 100;
/// How long comments read from an issue are taken as current.
pub const FRESH: chrono::Duration = chrono::Duration::seconds(60);

/// The names Desk says its own comments by: a remote writer going by one
/// is said with where they are ("You (Jira)").
const RESERVED_NAMES: [&str; 2] = ["you", "desk"];

/// Who wrote a comment, as the view says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthorKind {
    /// A person, there or here.
    Person,
    /// An agent or app: there (an app's comment), or here (a session's or
    /// an agent's note).
    Agent,
    /// Desk's own, as `posted_by` records: never a remote writer's.
    Desk,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub kind: AuthorKind,
    pub name: String,
    /// Where it is one: "Linear", "Jira", or "Desk".
    pub on: String,
}

/// A comment as Desk shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: Uuid,
    /// What its issue calls it: never sent to the page.
    #[serde(skip)]
    pub remote_id: Option<String>,
    pub author: Author,
    /// Who in Desk posted it, when Desk did: the person, a session, an
    /// agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posted_by: Option<Party>,
    /// Markdown (Linear, Desk) or text (Jira's rendered HTML, taken down to
    /// its words).
    pub body: String,
    /// The hub warned its words read as instructions to a model.
    pub flagged: bool,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<DateTime<Utc>>,
    /// On a synced ticket, it stays in Desk: a session's or an agent's note.
    pub only_here: bool,
}

/// A comment as an issue's answer gives it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteComment {
    pub remote_id: String,
    pub author: Author,
    pub body: String,
    pub flagged: bool,
    pub at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
}

/// `body` held to [`BODY_MAX`].
pub fn held(body: &str) -> String {
    match body.chars().count() > BODY_MAX {
        true => format!("{}…", body.chars().take(BODY_MAX - 1).collect::<String>()),
        false => body.to_string(),
    }
}

fn name_of(v: &Value) -> Option<String> {
    let n = unfence(v.as_str()?).0;
    let n = crate::crew::one_line(n.trim(), 80);
    (!n.is_empty()).then_some(n)
}

/// Who wrote a comment there: an app or a person, never Desk or the person
/// themselves, whatever its words or name say ([`RESERVED_NAMES`]).
fn author_of(system: System, name: Option<String>, app: bool) -> Author {
    let on = system.title().to_string();
    let name = name.map(
        |n| match RESERVED_NAMES.contains(&n.trim().to_lowercase().as_str()) {
            true => format!("{n} ({on})"),
            false => n,
        },
    );
    Author {
        kind: if app {
            AuthorKind::Agent
        } else {
            AuthorKind::Person
        },
        name: name.unwrap_or_else(|| "Someone".into()),
        on,
    }
}

/// An issue's comments as one answer listed them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteRead {
    pub comments: Vec<RemoteComment>,
    /// The ids of comments it listed whose rest couldn't be read (a time
    /// that doesn't parse): there still, so what is kept of each stays as
    /// it was, and none stops a whole read pruning the others.
    pub unread: Vec<String>,
    /// The answer listed every comment the issue has, each by its id: only
    /// then is one it doesn't list taken as deleted there.
    pub complete: bool,
}

/// An answer's `comments`, or why it lists none.
fn listed(answer: &Value) -> Result<&Vec<Value>, String> {
    answer["comments"]
        .as_array()
        .ok_or_else(|| "its answer listed no comments".to_string())
}

/// One entry of an answer: read, there but unreadable past its id, or not
/// even that.
enum Entry {
    Read(RemoteComment),
    Unread(String),
    Unknown,
}

/// What `entries` (an answer's first [`READ_MAX`]) read as. Whole when each
/// of `listed` was read or at least named, and the issue has no more: as
/// `total` counts them, or, where the answer doesn't count, fewer came back
/// than the page asked for ([`READ_MAX`]).
fn read_of(listed: usize, entries: impl Iterator<Item = Entry>, total: Option<u64>) -> RemoteRead {
    let mut read = RemoteRead::default();
    let mut named = 0;
    for entry in entries.take(READ_MAX) {
        match entry {
            Entry::Read(c) => read.comments.push(c),
            Entry::Unread(id) => read.unread.push(id),
            Entry::Unknown => continue,
        }
        named += 1;
    }
    let all_there = match total {
        Some(total) => total <= listed as u64,
        None => listed < READ_MAX,
    };
    read.complete = named == listed && all_there;
    read
}

/// The comments of a `jira.list_comments` answer, asked for [`READ_MAX`]
/// at a time; Jira's `total` says whether that was all of them.
pub fn jira_comments(answer: &Value) -> Result<RemoteRead, String> {
    let all = listed(answer)?;
    let entries = all.iter().map(|c| {
        let id = match &c["id"] {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return Entry::Unknown,
        };
        let Some(at) = crate::board_sync::remote_time(&c["created"]) else {
            return Entry::Unread(id);
        };
        let (html, flagged) = unfence(c["body_html"].as_str().unwrap_or_default());
        let body = held(&crate::board_sync::html_text(&html));
        let app = c["author_account_type"].as_str() == Some("app");
        Entry::Read(RemoteComment {
            author: author_of(System::Jira, name_of(&c["author"]), app),
            remote_id: id,
            flagged,
            at,
            edited_at: crate::board_sync::remote_time(&c["updated"]).filter(|u| *u != at),
            body,
        })
    });
    Ok(read_of(all.len(), entries, answer["total"].as_u64()))
}

/// The comments of a `linear.comment_list` answer, asked for at most
/// [`READ_MAX`].
pub fn linear_comments(answer: &Value) -> Result<RemoteRead, String> {
    let all = listed(answer)?;
    let entries = all.iter().map(|c| {
        let Some(id) = c["id"].as_str().filter(|id| !id.is_empty()) else {
            return Entry::Unknown;
        };
        let Some(at) = crate::board_sync::remote_time(&c["createdAt"]) else {
            return Entry::Unread(id.to_string());
        };
        let (body, flagged) = unfence(c["body"].as_str().unwrap_or_default());
        let body = held(&body);
        let bot = c.get("botActor").filter(|b| b.is_object());
        let (name, app) = match (c["user"]["name"].as_str(), bot) {
            (Some(_), _) => (name_of(&c["user"]["name"]), false),
            (None, Some(b)) => (name_of(&b["name"]), true),
            (None, None) => (None, false),
        };
        Entry::Read(RemoteComment {
            author: author_of(System::Linear, name, app),
            remote_id: id.to_string(),
            flagged,
            at,
            edited_at: crate::board_sync::remote_time(&c["editedAt"]),
            body,
        })
    });
    Ok(read_of(all.len(), entries, None))
}

/// The id of the comment a `jira.add_comment` or `linear.comment_add`
/// answer made.
pub fn posted_id(system: System, answer: &Value) -> Option<String> {
    let v = match system {
        System::Jira => &answer["comment_id"],
        System::Linear => &answer["comment"]["id"],
    };
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The call that lists `key`'s comments.
pub fn list_call(system: System, key: &str) -> (&'static str, Value) {
    match system {
        // A page as large as is read, so `total` says whether it was all
        // (Jira's own page is smaller).
        System::Jira => (
            "jira.list_comments",
            serde_json::json!({ "issue_key": key, "max_results": READ_MAX }),
        ),
        System::Linear => (
            "linear.comment_list",
            serde_json::json!({ "identifier": key, "limit": READ_MAX }),
        ),
    }
}

/// The call that posts `body` on `key`, as the person.
pub fn add_call(system: System, key: &str, body: &str) -> (&'static str, Value) {
    match system {
        System::Jira => (
            "jira.add_comment",
            serde_json::json!({ "issue_key": key, "body": body }),
        ),
        System::Linear => (
            "linear.comment_add",
            serde_json::json!({ "identifier": key, "body": body }),
        ),
    }
}

/// How a comment Desk posted is said: as whoever posted it.
pub fn author_from(party: &Party) -> Author {
    use crate::crew::PartyKind;
    Author {
        kind: match party.kind {
            PartyKind::You | PartyKind::Manager => AuthorKind::Person,
            PartyKind::Desk => AuthorKind::Desk,
            PartyKind::Agent | PartyKind::Session => AuthorKind::Agent,
        },
        name: match party.kind {
            PartyKind::You => "You".into(),
            _ => crate::crew::one_line(&party.name, 80),
        },
        on: "Desk".into(),
    }
}

// ── In Postgres ──────────────────────────────────────────────────────────

const COMMENT: &str = "id, remote_id, author, posted_by, body, flagged, at, edited_at";

fn comment(row: &Row, synced: bool) -> Result<Comment> {
    let remote_id: Option<String> = row.try_get("remote_id")?;
    let posted_by: Option<Value> = row.try_get("posted_by")?;
    let posted_by: Option<Party> = posted_by
        .filter(|v| !v.is_null())
        .map(serde_json::from_value)
        .transpose()
        .context("reading who posted a comment")?;
    let mut author: Author =
        serde_json::from_value(row.try_get("author")?).context("reading a comment's author")?;
    // Desk's record of who posted it outranks what the comment says, and
    // nothing else makes a comment Desk's.
    match &posted_by {
        Some(p) => {
            author = Author {
                on: author.on,
                ..author_from(p)
            }
        }
        None if author.kind == AuthorKind::Desk => author.kind = AuthorKind::Person,
        None => {}
    }
    Ok(Comment {
        id: row.try_get("id")?,
        only_here: synced && remote_id.is_none(),
        remote_id,
        author,
        posted_by,
        body: row.try_get("body")?,
        flagged: row.try_get("flagged")?,
        at: row.try_get("at")?,
        edited_at: row.try_get("edited_at")?,
    })
}

impl DeskStore {
    /// `ticket`'s comments, oldest first; `synced` says whether a Desk-only
    /// one is marked as staying here.
    pub async fn comments_of(&self, ticket: Uuid, synced: bool) -> Result<Vec<Comment>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {COMMENT} FROM ticket_comments WHERE ticket = $1 ORDER BY at, id"),
                &[&ticket],
            )
            .await?
            .iter()
            .map(|r| comment(r, synced))
            .collect()
    }

    /// Keeps `ticket`'s issue's comments as `read` (asked for at `asked_at`)
    /// lists them: new ones added, edited ones rewritten, and, only when the
    /// read is whole, ones it no longer lists deleted, Desk's posts among
    /// them. Who in Desk posted one stays while it's there. One Desk posted
    /// after the read was asked for stays (the answer may predate it), and
    /// so does one listed but unreadable ([`RemoteRead::unread`]); Desk-only
    /// comments are never touched.
    pub async fn put_remote_comments(
        &self,
        ticket: Uuid,
        read: &RemoteRead,
        asked_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        for c in &read.comments {
            tx.execute(
                "INSERT INTO ticket_comments (id, ticket, remote_id, author, body, flagged, at, edited_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (ticket, remote_id) DO UPDATE SET author = EXCLUDED.author, \
                 body = EXCLUDED.body, flagged = EXCLUDED.flagged, at = EXCLUDED.at, \
                 edited_at = EXCLUDED.edited_at",
                &[
                    &Uuid::new_v4(),
                    &ticket,
                    &c.remote_id,
                    &serde_json::to_value(&c.author)?,
                    &c.body,
                    &c.flagged,
                    &c.at,
                    &c.edited_at,
                ],
            )
            .await?;
        }
        if read.complete {
            let there: Vec<&str> = read
                .comments
                .iter()
                .map(|c| c.remote_id.as_str())
                .chain(read.unread.iter().map(String::as_str))
                .collect();
            tx.execute(
                "DELETE FROM ticket_comments WHERE ticket = $1 AND remote_id IS NOT NULL \
                 AND NOT (remote_id = ANY($2)) AND (posted_by IS NULL OR at < $3)",
                &[&ticket, &there, &asked_at],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Adds a comment Desk made: posted to the issue as `remote_id`, or kept
    /// here only (`None`).
    pub async fn add_comment(
        &self,
        ticket: Uuid,
        remote_id: Option<&str>,
        author: &Author,
        posted_by: &Party,
        body: &str,
        at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO ticket_comments (id, ticket, remote_id, author, posted_by, body, flagged, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, FALSE, $7) \
                 ON CONFLICT (ticket, remote_id) DO UPDATE SET posted_by = EXCLUDED.posted_by",
                &[
                    &id,
                    &ticket,
                    &remote_id,
                    &serde_json::to_value(author)?,
                    &serde_json::to_value(posted_by)?,
                    &body,
                    &at,
                ],
            )
            .await?;
        Ok(id)
    }
}

#[cfg(test)]
#[path = "ticket_comments_tests.rs"]
mod tests;
