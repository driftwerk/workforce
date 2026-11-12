//! A ticket's files, for the person: reading a synced ticket's issue whole
//! for its files when it is opened, a file's bytes (kept, or read through
//! the board's sync), and the files dropped on a ticket of their own.

use serde_json::json;
use uuid::Uuid;

use super::{Desk, DeskError, TicketView};
use crate::board::{System, Ticket};
use crate::board_sync::{Remote, RemoteError};
use crate::crew::{By, Kind};
use crate::model::DeskEvent;
use crate::ticket_files::{self, FileRead, FileSource, TicketFile, FILE_MAX};

/// How long a ticket's issue, read whole for its files, is taken as read.
pub const READ_FRESH: chrono::Duration = chrono::Duration::minutes(5);

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

/// A file's bytes, as the route serves them.
#[derive(Debug)]
pub struct FileBytes {
    pub file: TicketFile,
    pub bytes: Vec<u8>,
}

impl Desk {
    /// The ticket `id` and its file `file`, when it is that ticket's.
    pub async fn ticket_file(
        &self,
        id: Uuid,
        file: Uuid,
    ) -> Result<(Ticket, TicketFile), DeskError> {
        let t = self.ticket_of(id).await?;
        let f = self
            .0
            .store
            .ticket_file(id, file)
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)?;
        Ok((t, f))
    }

    /// The bytes kept of `f` (its small picture's, with `thumb`), if any.
    pub async fn kept_file_bytes(
        &self,
        f: &TicketFile,
        thumb: bool,
    ) -> Result<Option<Vec<u8>>, DeskError> {
        self.0.store.file_bytes(f.id, thumb).await.map_err(internal)
    }

    /// `f`'s bytes (or its small picture's) on `t`: kept ones, else read
    /// through `remote`, the route `t`'s board syncs by, and kept.
    pub async fn remote_file_bytes(
        &self,
        t: &Ticket,
        f: TicketFile,
        thumb: bool,
        remote: &dyn Remote,
    ) -> Result<FileBytes, FileRead> {
        let e = t.external.as_ref().ok_or(FileRead::Link)?;
        let thumb = thumb && f.thumb;
        if let Ok(Some(bytes)) = self.kept_file_bytes(&f, thumb).await {
            return Ok(FileBytes { file: f, bytes });
        }
        let bytes = ticket_files::read_remote(remote, e.system, &e.key, &f, thumb).await?;
        if let Err(err) = self.0.store.keep_file_bytes(f.id, thumb, &bytes).await {
            tracing::warn!(file = %f.id, error = format!("{err:#}"), "a ticket's file wasn't kept");
        }
        Ok(FileBytes { file: f, bytes })
    }

    /// Reads ticket `id`'s issue whole, through `remote`, and keeps what it
    /// says of its files and itself: what opening a synced ticket does when
    /// its issue wasn't read in the last [`READ_FRESH`] (or `force`).
    pub async fn read_ticket_issue(
        &self,
        id: Uuid,
        remote: &dyn Remote,
        force: bool,
    ) -> Result<TicketView, RemoteError> {
        let fail = |e: DeskError| RemoteError::Failed(e.to_string());
        let t = self.ticket_of(id).await.map_err(fail)?;
        let Some(e) = t.external.clone() else {
            return self.view_ticket(t).await.map_err(fail);
        };
        let now = crate::crew::now();
        let fresh = e.details.read_at.is_some_and(|at| now - at < READ_FRESH);
        if (fresh && !force) || !e.synced {
            return self.view_ticket(t).await.map_err(fail);
        }
        let (tool, args) = match e.system {
            System::Jira => ("jira.get_issue", json!({ "issue_key": e.key })),
            System::Linear => ("linear.issue_get", json!({ "identifier": e.key })),
        };
        let v = crate::board_sync::call(remote, e.system, tool, args).await?;
        let issue = match e.system {
            System::Jira => crate::board_sync::jira_issue(&v, ""),
            System::Linear => crate::board_sync::linear_issue(v.get("issue").unwrap_or(&v)),
        }
        .filter(|i| i.key == e.key)
        .ok_or_else(|| RemoteError::Failed(format!("{} couldn't be read", e.key)))?;
        let party = crate::board_sync::remote_party(e.system, &e.key);
        if let Some(files) = &issue.files {
            self.0
                .store
                .put_remote_files(id, files, &party)
                .await
                .map_err(|e| fail(internal(e)))?;
        }
        // The rest of what it says comes in by the pull, as it always does;
        // here only what the view shows beside it, and when it was read.
        let mut t = self.ticket_of(id).await.map_err(fail)?;
        if let Some(ext) = t.external.as_mut() {
            let mut details = crate::board_sync::details_of(&issue, Some(&ext.details));
            details.read_at = Some(now);
            ext.details = details;
        }
        self.save_ticket(&mut t).await.map_err(fail)?;
        let v = self.view_ticket(t).await.map_err(fail)?;
        self.announce(DeskEvent::Ticket {
            ticket: Box::new(v.clone()),
        });
        Ok(v)
    }

    /// Whether `t` takes files dropped on it: a ticket of the person's own,
    /// on a board that isn't synced (a synced one's files are its issue's).
    async fn takes_uploads(&self, t: &Ticket) -> Result<(), DeskError> {
        let b = self.board_of(t.board).await?;
        if t.external.is_some() || b.sync.is_some() {
            return Err(DeskError::Invalid(
                "A synced ticket's files are its issue's: add it there.".into(),
            ));
        }
        Ok(())
    }

    /// Drops `bytes`, a file called `name`, on ticket `id`.
    pub async fn add_ticket_upload(
        &self,
        id: Uuid,
        name: &str,
        bytes: &[u8],
        by: &By,
    ) -> Result<TicketFile, DeskError> {
        let t = self.ticket_of(id).await?;
        self.takes_uploads(&t).await?;
        if bytes.is_empty() {
            return Err(DeskError::Invalid("That file is empty.".into()));
        }
        if bytes.len() as u64 > FILE_MAX {
            return Err(DeskError::Invalid(format!(
                "That file is over {} MB.",
                FILE_MAX / (1024 * 1024)
            )));
        }
        let name = crate::crew::one_line(name.trim(), 200);
        let name = match name.is_empty() {
            true => "file".to_string(),
            false => name,
        };
        // What it is, by its bytes; by its name only for a kind Desk never
        // shows in place.
        let media_type =
            ticket_files::sniff(bytes).unwrap_or(match ticket_files::type_of_name(&name) {
                "text/plain" => "text/plain",
                _ => "application/octet-stream",
            });
        let f = self
            .0
            .store
            .add_upload(id, &name, media_type, bytes, &by.party)
            .await
            .map_err(internal)?;
        let f = match f {
            ticket_files::Uploaded::Added(f) => f,
            ticket_files::Uploaded::TicketFull => {
                return Err(DeskError::Invalid(format!(
                    "A ticket holds at most {} files: take one off first.",
                    ticket_files::UPLOADS_PER_TICKET
                )))
            }
            ticket_files::Uploaded::BoardFull => {
                return Err(DeskError::Invalid(
                    "This board's files already take 1 GB, the most a board keeps: take some off first.".into(),
                ))
            }
        };
        let key = self.key_of(&t).await?;
        let text = format!("added {} to {key}", quoted(&f.name));
        self.ticket_changed(t, by, Kind::TicketEdited, None, text)
            .await?;
        Ok(f)
    }

    /// Takes the file `file` off ticket `id`, one dropped on it.
    pub async fn remove_ticket_upload(
        &self,
        id: Uuid,
        file: Uuid,
        by: &By,
    ) -> Result<(), DeskError> {
        let (t, f) = self.ticket_file(id, file).await?;
        if f.source != FileSource::Upload {
            return Err(DeskError::Invalid(
                "That file is its issue's: take it off there.".into(),
            ));
        }
        if !self
            .0
            .store
            .remove_upload(id, file)
            .await
            .map_err(internal)?
        {
            return Err(DeskError::NotFound);
        }
        let key = self.key_of(&t).await?;
        let text = format!("took {} off {key}", quoted(&f.name));
        self.ticket_changed(t, by, Kind::TicketEdited, None, text)
            .await?;
        Ok(())
    }
}

/// A file's name in a history line: on one line, defanged, quoted.
fn quoted(name: &str) -> String {
    format!("“{}”", crate::board_sync::defanged_name(name))
}
