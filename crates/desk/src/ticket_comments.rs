//! A ticket's comments: its Jira or Linear issue's, read when the ticket is
//! opened (never in a sync pass, which would spend its calls on every
//! issue), and Desk's own: the person's on a ticket of their own, and the
//! notes sessions and agents leave, which stay in Desk.
//!
//! Who wrote one is said in this order of trust:
//! 1. `posted_by`, recorded when Desk posted it: the one record of which
//!    person, session or agent it was;
//! 2. Desk's own move notes ("Desk, on my behalf: …"): labelled as what the
//!    comment says, since anyone can type it;
//! 3. an app (Linear's `botActor`, a Jira author whose account is an app):
//!    an agent there, by the name it goes by;
//! 4. anyone else: a person.
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

/// What Desk's own notes on an issue begin with.
pub const DESK_PREFIX: &str = "Desk, on my behalf:";

/// Who wrote a comment, as the view says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthorKind {
    /// A person, there or here.
    Person,
    /// An agent or app: there (an app's comment), or here (a session's or
    /// an agent's note).
    Agent,
    /// Desk's own note on the issue, as the comment says.
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

/// Who wrote a comment whose body is `body`: Desk's note when it says so,
/// else an app or a person there.
fn author_of(system: System, body: &str, name: Option<String>, app: bool) -> Author {
    let on = system.title().to_string();
    if body.trim_start().starts_with(DESK_PREFIX) {
        return Author {
            kind: AuthorKind::Desk,
            name: name.unwrap_or_else(|| "Desk".into()),
            on,
        };
    }
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

/// The comments of a `jira.list_comments` answer.
pub fn jira_comments(answer: &Value) -> Vec<RemoteComment> {
    answer["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let id = match &c["id"] {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            let (html, flagged) = unfence(c["body_html"].as_str().unwrap_or_default());
            let body = held(&crate::board_sync::html_text(&html));
            let app = c["author_account_type"].as_str() == Some("app");
            Some(RemoteComment {
                author: author_of(System::Jira, &body, name_of(&c["author"]), app),
                remote_id: id,
                flagged,
                at: crate::board_sync::remote_time(&c["created"])?,
                edited_at: crate::board_sync::remote_time(&c["updated"])
                    .filter(|u| Some(*u) != crate::board_sync::remote_time(&c["created"])),
                body,
            })
        })
        .take(READ_MAX)
        .collect()
}

/// The comments of a `linear.comment_list` answer.
pub fn linear_comments(answer: &Value) -> Vec<RemoteComment> {
    answer["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let id = c["id"].as_str()?.to_string();
            let (body, flagged) = unfence(c["body"].as_str().unwrap_or_default());
            let body = held(&body);
            let bot = c.get("botActor").filter(|b| b.is_object());
            let (name, app) = match (c["user"]["name"].as_str(), bot) {
                (Some(_), _) => (name_of(&c["user"]["name"]), false),
                (None, Some(b)) => (name_of(&b["name"]), true),
                (None, None) => (None, false),
            };
            let at = crate::board_sync::remote_time(&c["createdAt"])?;
            Some(RemoteComment {
                author: author_of(System::Linear, &body, name, app),
                remote_id: id,
                flagged,
                at,
                edited_at: crate::board_sync::remote_time(&c["editedAt"]),
                body,
            })
        })
        .take(READ_MAX)
        .collect()
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
        System::Jira => (
            "jira.list_comments",
            serde_json::json!({ "issue_key": key }),
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
    // Desk's record of who posted it outranks what the comment says.
    if let Some(p) = &posted_by {
        author = Author {
            on: author.on,
            ..author_from(p)
        };
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

    /// Keeps `ticket`'s issue's comments as it now lists them: new ones
    /// added, edited ones rewritten, gone ones deleted. Who in Desk posted
    /// one stays; Desk-only comments are never touched.
    pub async fn put_remote_comments(
        &self,
        ticket: Uuid,
        comments: &[RemoteComment],
    ) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        for c in comments {
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
        let ids: Vec<&str> = comments.iter().map(|c| c.remote_id.as_str()).collect();
        tx.execute(
            "DELETE FROM ticket_comments WHERE ticket = $1 AND remote_id IS NOT NULL \
             AND NOT (remote_id = ANY($2))",
            &[&ticket, &ids],
        )
        .await?;
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
