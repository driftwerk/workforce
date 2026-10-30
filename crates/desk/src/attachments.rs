//! Files in Desk conversations. A file is an artifact in the store of the
//! agent whose conversation it is in; Desk shows it with what is cheap to
//! know about it, and downloads its bytes from that agent's hub by id.
//!
//! A DM's files are kept here by artifact: an agent's, as its turn
//! delivered them, and the person's, as they uploaded them. Once a turn
//! ends, the files it delivered ride on its answer, and the person's on the
//! message that carried them; an agent's file its turn never answered with
//! is a message of its own. A room's files ride on its messages, as Desk
//! posts them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio_postgres::Row;

use crate::store::DeskStore;

/// The most files one message carries.
pub const MAX_PER_MESSAGE: usize = 20;

/// A file as Desk shows it: in a DM's history, on a room message, and as an
/// upload answers. Its bytes are at
/// `GET /api/agents/{agent}/attachments/{artifact_id}` on the hub whose
/// agent's store holds them: a DM's agent on the hub that serves the DM, or
/// the `hub` and `agent` a room message's file names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub artifact_id: String,
    pub media_type: String,
    pub filename: String,
    /// Bytes.
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Seconds, for audio and video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// A small picture of it (a PDF's first page), on the same hub as the
    /// file; null when there is none.
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// The hub whose store holds it, as Desk names its hubs: set on room
    /// messages, which may hold files from any hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<String>,
    /// The agent whose store holds it: set on room messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// A file an agent delivered, as its conversation's message says it: on
/// the hub and in the store of that conversation's agent.
impl From<&workforce_core::desk::DeskFile> for Attachment {
    fn from(file: &workforce_core::desk::DeskFile) -> Self {
        Self {
            artifact_id: file.artifact_id.clone(),
            media_type: file.media_type.clone(),
            filename: file.filename.clone(),
            size: file.size,
            width: file.width,
            height: file.height,
            duration: None,
            thumbnail_url: None,
            hub: None,
            agent: None,
        }
    }
}

impl Attachment {
    /// Why this can't be kept on a message, if it can't.
    pub fn invalid(&self) -> Option<String> {
        let id = self.artifact_id.trim();
        if id.is_empty() || id.len() > 128 || id.contains('/') {
            return Some("an attachment needs its artifact_id".into());
        }
        if self.filename.trim().is_empty() || self.filename.len() > 512 {
            return Some(format!("attachment {id} needs a filename"));
        }
        if self.media_type.trim().is_empty() || self.media_type.len() > 255 {
            return Some(format!("attachment {id} needs a media_type"));
        }
        None
    }
}

pub use workforce_core::desk::download_path;

/// Who put a file in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// An agent's turn delivered it.
    Agent,
    /// The person uploaded it.
    You,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Agent => "agent",
            Origin::You => "you",
        }
    }
}

/// A file of a DM or room session, as kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub artifact_id: String,
    /// Whose store holds it.
    pub agent_id: String,
    pub channel: String,
    /// The session's owner: the person (or their paired device).
    pub owner: String,
    pub session_id: String,
    pub origin: Origin,
    /// The message it rides on, once known.
    pub message_id: Option<String>,
    pub media_type: String,
    pub filename: String,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<f64>,
    /// The artifact holding its thumbnail, in the same store.
    pub thumbnail_id: Option<String>,
    /// The line its producer gave it.
    pub caption: Option<String>,
    pub at: DateTime<Utc>,
}

impl Kept {
    /// How Desk shows it.
    pub fn shown(&self) -> Attachment {
        Attachment {
            artifact_id: self.artifact_id.clone(),
            media_type: self.media_type.clone(),
            filename: self.filename.clone(),
            size: self.size,
            width: self.width,
            height: self.height,
            duration: self.duration,
            thumbnail_url: self
                .thumbnail_id
                .as_deref()
                .map(|t| download_path(&self.agent_id, t)),
            hub: None,
            agent: None,
        }
    }
}

fn kept(row: &Row) -> Result<Kept> {
    let origin: String = row.try_get("origin")?;
    Ok(Kept {
        artifact_id: row.try_get("artifact_id")?,
        agent_id: row.try_get("agent_id")?,
        channel: row.try_get("channel")?,
        owner: row.try_get("owner")?,
        session_id: row.try_get("session_id")?,
        origin: if origin == "you" {
            Origin::You
        } else {
            Origin::Agent
        },
        message_id: row.try_get("message_id")?,
        media_type: row.try_get("media_type")?,
        filename: row.try_get("filename")?,
        size: row.try_get::<_, i64>("size")?.max(0) as u64,
        width: row
            .try_get::<_, Option<i32>>("width")?
            .map(|w| w.max(0) as u32),
        height: row
            .try_get::<_, Option<i32>>("height")?
            .map(|h| h.max(0) as u32),
        duration: row.try_get("duration")?,
        thumbnail_id: row.try_get("thumbnail_id")?,
        caption: row.try_get("caption")?,
        at: row.try_get("at")?,
    })
}

const COLUMNS: &str = "artifact_id, agent_id, channel, owner, session_id, origin, message_id, \
                       media_type, filename, size, width, height, duration, thumbnail_id, caption, at";

fn dimension(v: Option<u32>) -> Option<i32> {
    v.map(|v| v.min(i32::MAX as u32) as i32)
}

impl DeskStore {
    /// Keeps a file of a conversation. An artifact is one file: keeping it
    /// again changes nothing.
    pub async fn put_attachment(&self, k: &Kept) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO attachments ({COLUMNS}) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) \
                     ON CONFLICT (artifact_id) DO NOTHING"
                ),
                &[
                    &k.artifact_id,
                    &k.agent_id,
                    &k.channel,
                    &k.owner,
                    &k.session_id,
                    &k.origin.as_str(),
                    &k.message_id,
                    &k.media_type,
                    &k.filename,
                    &(k.size.min(i64::MAX as u64) as i64),
                    &dimension(k.width),
                    &dimension(k.height),
                    &k.duration,
                    &k.thumbnail_id,
                    &k.caption,
                    &k.at,
                ],
            )
            .await
            .context("keeping a conversation's file")?;
        Ok(())
    }

    /// The files kept for session `session_id` are `owner`'s, on
    /// `channel`: the session's conversation moved to them.
    pub async fn move_attachments(
        &self,
        session_id: &str,
        owner: &str,
        channel: &str,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE attachments SET owner = $2, channel = $3 WHERE session_id = $1",
                &[&session_id, &owner, &channel],
            )
            .await
            .context("moving a conversation's files")?;
        Ok(())
    }

    /// Forget the files kept for session `session_id`: it is gone.
    pub async fn forget_attachments(&self, session_id: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM attachments WHERE session_id = $1",
                &[&session_id],
            )
            .await
            .context("forgetting a conversation's files")?;
        Ok(())
    }

    /// What was learnt about a kept file after it was kept: its size on
    /// screen, its length, its thumbnail. `None` leaves a field as it is.
    pub async fn describe_attachment(
        &self,
        artifact_id: &str,
        width: Option<u32>,
        height: Option<u32>,
        duration: Option<f64>,
        thumbnail_id: Option<&str>,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE attachments SET width = COALESCE($2, width), \
                 height = COALESCE($3, height), duration = COALESCE($4, duration), \
                 thumbnail_id = COALESCE($5, thumbnail_id) WHERE artifact_id = $1",
                &[
                    &artifact_id,
                    &dimension(width),
                    &dimension(height),
                    &duration,
                    &thumbnail_id,
                ],
            )
            .await
            .context("describing a conversation's file")?;
        Ok(())
    }

    pub async fn attachment(&self, artifact_id: &str) -> Result<Option<Kept>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM attachments WHERE artifact_id = $1"),
                &[&artifact_id],
            )
            .await?
            .as_ref()
            .map(kept)
            .transpose()
    }

    /// A turn of `agent_id`'s in `sessions` (the session it ran in, and the
    /// one it rolled over to) ended. The files it delivered since `since`
    /// ride on its `answer`, when it gave one; the person's files ride on
    /// the messages that carried them.
    pub async fn bind_turn(
        &self,
        agent_id: &str,
        sessions: &[String],
        since: DateTime<Utc>,
        answer: Option<&str>,
        carried: &[(String, Vec<String>)],
    ) -> Result<()> {
        let client = self.pool().get().await?;
        if let Some(answer) = answer {
            client
                .execute(
                    "UPDATE attachments SET message_id = $1 WHERE agent_id = $2 \
                     AND session_id = ANY($3) AND origin = 'agent' AND message_id IS NULL \
                     AND at >= $4",
                    &[&answer, &agent_id, &sessions, &since],
                )
                .await
                .context("putting a turn's files on its answer")?;
        }
        for (message, ids) in carried {
            client
                .execute(
                    "UPDATE attachments SET message_id = $1 WHERE agent_id = $2 \
                     AND artifact_id = ANY($3) AND origin = 'you' AND message_id IS NULL",
                    &[message, &agent_id, ids],
                )
                .await
                .context("putting the person's files on their message")?;
        }
        Ok(())
    }

    /// The files on whichever of `ids` carry some, oldest first.
    pub async fn attachments_on(&self, ids: &[String]) -> Result<HashMap<String, Vec<Kept>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM attachments WHERE message_id = ANY($1) \
                     ORDER BY at, artifact_id"
                ),
                &[&ids],
            )
            .await?;
        let mut on: HashMap<String, Vec<Kept>> = HashMap::new();
        for row in &rows {
            let k = kept(row)?;
            if let Some(m) = k.message_id.clone() {
                on.entry(m).or_default().push(k);
            }
        }
        Ok(on)
    }

    /// The agent's files of `owner`'s conversation on `channel` that ride
    /// on no message, delivered after `after` and before `before`, oldest
    /// first: each is a message of its own.
    pub async fn loose_attachments(
        &self,
        agent_id: &str,
        channel: &str,
        owner: &str,
        after: Option<DateTime<Utc>>,
        before: Option<DateTime<Utc>>,
    ) -> Result<Vec<Kept>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM attachments WHERE agent_id = $1 AND channel = $2 \
                     AND owner = $3 AND origin = 'agent' AND message_id IS NULL \
                     AND ($4::timestamptz IS NULL OR at >= $4) \
                     AND ($5::timestamptz IS NULL OR at < $5) ORDER BY at, artifact_id"
                ),
                &[&agent_id, &channel, &owner, &after, &before],
            )
            .await?;
        rows.iter().map(kept).collect()
    }

    /// The files in `owner`'s conversation on `channel` with `agent_id`,
    /// newest first, at most `limit`: every file the agent delivered, and
    /// the person's once they sent them.
    pub async fn conversation_files(
        &self,
        agent_id: &str,
        channel: &str,
        owner: &str,
        limit: i64,
    ) -> Result<Vec<Kept>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM attachments WHERE agent_id = $1 AND channel = $2 \
                     AND owner = $3 AND (origin = 'agent' OR message_id IS NOT NULL) \
                     ORDER BY at DESC, artifact_id LIMIT $4"
                ),
                &[&agent_id, &channel, &owner, &limit],
            )
            .await?;
        rows.iter().map(kept).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    fn file(id: &str, origin: Origin, session: &str, at: DateTime<Utc>) -> Kept {
        Kept {
            artifact_id: id.into(),
            agent_id: "scout".into(),
            channel: "desk".into(),
            owner: "identity-1".into(),
            session_id: session.into(),
            origin,
            message_id: None,
            media_type: "application/pdf".into(),
            filename: format!("{id}.pdf"),
            size: 1234,
            width: None,
            height: None,
            duration: None,
            thumbnail_id: None,
            caption: None,
            at,
        }
    }

    #[test]
    fn an_attachment_reads_as_the_brief_names_it() {
        let mut k = file("art_1", Origin::Agent, "s1", Utc::now());
        k.thumbnail_id = Some("art_2".into());
        let shown = serde_json::to_value(k.shown()).unwrap();
        assert_eq!(
            shown,
            serde_json::json!({
                "artifact_id": "art_1",
                "media_type": "application/pdf",
                "filename": "art_1.pdf",
                "size": 1234,
                "thumbnail_url": "/api/agents/scout/attachments/art_2",
            })
        );
        let plain = file("art_3", Origin::Agent, "s1", Utc::now()).shown();
        assert_eq!(
            serde_json::to_value(plain).unwrap()["thumbnail_url"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_room_attachment_must_name_its_file() {
        let ok: Attachment = serde_json::from_value(serde_json::json!({
            "artifact_id": "art_1", "media_type": "image/png", "filename": "a.png",
            "hub": "local", "agent": "scout",
        }))
        .unwrap();
        assert_eq!(ok.invalid(), None);
        assert_eq!(ok.size, 0);
        let mut bad = ok.clone();
        bad.artifact_id = " ".into();
        assert!(bad.invalid().is_some());
        let mut bad = ok;
        bad.filename = String::new();
        assert!(bad.invalid().is_some());
    }

    #[tokio::test]
    async fn a_turns_files_ride_on_its_answer_and_the_rest_stand_alone() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let t0 = Utc::now() - chrono::Duration::minutes(10);
            let at = |m: i64| t0 + chrono::Duration::minutes(m);
            // Delivered before the turn: stays loose.
            store
                .put_attachment(&file("early", Origin::Agent, "s1", at(0)))
                .await
                .unwrap();
            // Delivered in the turn.
            store
                .put_attachment(&file("report", Origin::Agent, "s1", at(2)))
                .await
                .unwrap();
            // Uploaded, then sent; and uploaded but never sent.
            store
                .put_attachment(&file("mine", Origin::You, "s1", at(1)))
                .await
                .unwrap();
            store
                .put_attachment(&file("unsent", Origin::You, "s1", at(1)))
                .await
                .unwrap();
            // Keeping it again changes nothing.
            store
                .put_attachment(&file("report", Origin::Agent, "s2", at(3)))
                .await
                .unwrap();

            store
                .bind_turn(
                    "scout",
                    &["s1".to_string()],
                    at(1),
                    Some("answer-1"),
                    &[("question-1".to_string(), vec!["mine".to_string()])],
                )
                .await
                .unwrap();
            let on = store
                .attachments_on(&["answer-1".into(), "question-1".into()])
                .await
                .unwrap();
            assert_eq!(on["answer-1"].len(), 1);
            assert_eq!(on["answer-1"][0].artifact_id, "report");
            assert_eq!(on["answer-1"][0].session_id, "s1");
            assert_eq!(on["question-1"][0].artifact_id, "mine");

            let loose = store
                .loose_attachments("scout", "desk", "identity-1", None, None)
                .await
                .unwrap();
            assert_eq!(
                loose
                    .iter()
                    .map(|k| k.artifact_id.as_str())
                    .collect::<Vec<_>>(),
                ["early"]
            );
            assert!(store
                .loose_attachments("scout", "desk", "identity-1", Some(at(1)), None)
                .await
                .unwrap()
                .is_empty());
            assert!(store
                .loose_attachments("scout", "desk", "identity-2", None, None)
                .await
                .unwrap()
                .is_empty());

            let files = store
                .conversation_files("scout", "desk", "identity-1", 50)
                .await
                .unwrap();
            assert_eq!(
                files
                    .iter()
                    .map(|k| k.artifact_id.as_str())
                    .collect::<Vec<_>>(),
                ["report", "mine", "early"],
                "newest first, never a file the person didn't send"
            );

            store
                .move_attachments("s1", "identity-2", "desk:moved")
                .await
                .unwrap();
            let moved = store.attachment("early").await.unwrap().unwrap();
            assert_eq!(
                (moved.owner.as_str(), moved.channel.as_str()),
                ("identity-2", "desk:moved")
            );
            store.forget_attachments("s1").await.unwrap();
            assert!(store.attachment("early").await.unwrap().is_none());
            assert!(
                store.attachment("report").await.unwrap().is_none(),
                "it was kept for s1 first"
            );
            store
                .put_attachment(&file("report", Origin::Agent, "s2", at(3)))
                .await
                .unwrap();
            store
                .describe_attachment("report", Some(640), Some(480), Some(1.5), Some("thumb"))
                .await
                .unwrap();
            let report = store.attachment("report").await.unwrap().unwrap();
            assert_eq!((report.width, report.height), (Some(640), Some(480)));
            assert_eq!(report.duration, Some(1.5));
            assert_eq!(
                report.shown().thumbnail_url.as_deref(),
                Some("/api/agents/scout/attachments/thumb")
            );
        })
        .await;
    }
}
