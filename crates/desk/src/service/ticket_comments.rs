//! A ticket's comments, for the person: read from its issue when it is
//! opened and stale, and posted to it as them; a ticket of their own keeps
//! its comments here.

use serde_json::json;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::board_sync::{Remote, RemoteError};
use crate::crew::{self, By, Kind, PartyKind};
use crate::ticket_comments::{self as comments, Comment, FRESH};

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

/// Why a comment wasn't posted.
#[derive(Debug, thiserror::Error)]
pub enum CommentPost {
    #[error(transparent)]
    Desk(#[from] DeskError),
    /// The hub holds the call for the person's approval.
    #[error("waiting for approval")]
    Waiting,
    #[error("{0}")]
    Remote(RemoteError),
    /// It is on the issue, but Desk couldn't keep its copy: it comes back
    /// when the issue's comments are next read. Posting again would post it
    /// twice. The comment as posted, with no id here ([`Uuid::nil`]): there
    /// is no copy to name.
    #[error("posted, but not kept here")]
    NotKept(Box<Comment>),
}

/// Where a synced ticket's comment goes: through `remote`, the board's sync,
/// for the link `link`.
pub struct PostVia<'a> {
    pub remote: &'a dyn Remote,
    pub link: Uuid,
}

impl Desk {
    /// Ticket `id`'s comments, as kept.
    pub async fn ticket_comments(&self, id: Uuid) -> Result<Vec<Comment>, DeskError> {
        let t = self.ticket_of(id).await?;
        let synced = t.external.as_ref().is_some_and(|e| e.synced);
        self.0.store.comments_of(id, synced).await.map_err(internal)
    }

    /// Whether ticket `id`'s issue's comments want reading: never read, read
    /// more than [`FRESH`] ago, or the issue changed since.
    pub async fn comments_stale(&self, id: Uuid) -> Result<bool, DeskError> {
        let t = self.ticket_of(id).await?;
        let Some(e) = t.external.as_ref().filter(|e| e.synced) else {
            return Ok(false);
        };
        Ok(match e.details.comments_at {
            None => true,
            Some(at) => crate::crew::now() - at > FRESH || e.remote_updated_at > at,
        })
    }

    /// Reads ticket `id`'s issue's comments through `remote` and keeps them.
    pub async fn read_ticket_comments(
        &self,
        id: Uuid,
        remote: &dyn Remote,
    ) -> Result<Vec<Comment>, RemoteError> {
        let fail = |e: DeskError| RemoteError::Failed(e.to_string());
        let t = self.ticket_of(id).await.map_err(fail)?;
        let Some(e) = t.external.clone().filter(|e| e.synced) else {
            return self.ticket_comments(id).await.map_err(fail);
        };
        let (tool, args) = comments::list_call(e.system, &e.key);
        // Before the call: a comment Desk posts from here on may postdate
        // its answer, so this read never takes it as deleted.
        let asked_at = crate::crew::now();
        let answer = crate::board_sync::call(remote, e.system, tool, args).await?;
        let read = match e.system {
            crate::board::System::Jira => comments::jira_comments(&answer),
            crate::board::System::Linear => comments::linear_comments(&answer),
        }
        .map_err(|why| {
            RemoteError::Failed(format!(
                "{}'s comments weren't read: {why}",
                e.system.title()
            ))
        })?;
        self.0
            .store
            .put_remote_comments(id, &read, asked_at)
            .await
            .map_err(|e| fail(internal(e)))?;
        // When they were read: a version race only means they're read again.
        let mut t = self.ticket_of(id).await.map_err(fail)?;
        if let Some(ext) = t.external.as_mut() {
            ext.details.comments_at = Some(crate::crew::now());
        }
        if let Err(e) = self.save_ticket(&mut t).await {
            tracing::debug!(ticket = %id, error = %e, "when its comments were read wasn't kept");
        }
        self.ticket_comments(id).await.map_err(fail)
    }

    /// Posts `body` on ticket `id` as `by`: on a synced ticket, to its issue
    /// through `via`, as the person, and only when `by` is the person (their
    /// words go there unprefixed, so no one else's may); on a ticket of
    /// their own, kept here.
    pub async fn post_ticket_comment(
        &self,
        id: Uuid,
        body: &str,
        by: &By,
        via: Option<PostVia<'_>>,
    ) -> Result<Comment, CommentPost> {
        let body = crate::line::typable(body).trim().to_string();
        if body.is_empty() {
            return Err(DeskError::Invalid("Write the comment first.".into()).into());
        }
        if body.chars().count() > comments::BODY_MAX {
            return Err(DeskError::Invalid(format!(
                "A comment is at most {} characters.",
                comments::BODY_MAX
            ))
            .into());
        }
        let t = self.ticket_of(id).await?;
        let key = self.key_of(&t).await?;
        let remote_id = match (&t.external, via) {
            (Some(e), Some(via)) => {
                if by.party.kind != PartyKind::You {
                    return Err(DeskError::Conflict(format!(
                        "Only you post on {key}'s {} issue as yourself.",
                        e.system.title()
                    ))
                    .into());
                }
                if !e.writable_by(via.link) {
                    return Err(DeskError::Conflict(format!(
                        "{key} isn't synced from this board any more: comment on it in {}.",
                        e.system.title()
                    ))
                    .into());
                }
                let (tool, args) = comments::add_call(e.system, &e.key, &body);
                let answer = crate::board_sync::call(via.remote, e.system, tool, args)
                    .await
                    .map_err(|e| match e {
                        RemoteError::Waiting { .. } => CommentPost::Waiting,
                        e => CommentPost::Remote(e),
                    })?;
                Some(comments::posted_id(e.system, &answer).ok_or_else(|| {
                    CommentPost::Remote(RemoteError::Failed(format!(
                        "{} didn't say it took the comment",
                        e.system.title()
                    )))
                })?)
            }
            (Some(e), None) => {
                return Err(DeskError::Conflict(format!(
                    "{key} is a {} issue: its comments go there.",
                    e.system.title()
                ))
                .into())
            }
            (None, _) => None,
        };
        // Once it's on the issue: a read asked for before now may not list
        // it, and never takes it as deleted (`put_remote_comments`).
        let at = crate::crew::now();
        let author = comments::author_from(&by.party);
        let kept = self
            .0
            .store
            .add_comment(id, remote_id.as_deref(), &author, &by.party, &body, at)
            .await;
        let (cid, kept_ok) = match (kept, &remote_id) {
            (Ok(cid), _) => (cid, true),
            (Err(e), None) => return Err(internal(e).into()),
            // On the issue already: said as posted, never as failed.
            (Err(e), Some(_)) => {
                tracing::warn!(ticket = %id, error = %e, "a posted comment wasn't kept here");
                (Uuid::nil(), false)
            }
        };
        let group = self.board_of(t.board).await.ok().and_then(|b| b.group);
        self.note(
            by.event(
                Kind::TicketCommented,
                None,
                format!("commented on {key}: “{}”", crew::clip(&body, 400)),
            )
            .detail(json!({ "comment": body }))
            .about_ticket(t.id, group.as_deref()),
        );
        let comment = Comment {
            id: cid,
            remote_id,
            only_here: false,
            author,
            posted_by: Some(by.party.clone()),
            body,
            flagged: false,
            at,
            edited_at: None,
        };
        match kept_ok {
            true => Ok(comment),
            false => Err(CommentPost::NotKept(Box::new(comment))),
        }
    }
}
