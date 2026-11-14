//! A ticket's files, for the person: reading a synced ticket's issue whole
//! for its files when it is opened, a file's bytes (kept, or read through
//! the board's sync), and the files dropped on a ticket of their own.

use uuid::Uuid;

use super::{Desk, DeskError, TicketView};
use crate::board::{System, Ticket};
use crate::board_sync::{Remote, RemoteError};
use crate::crew::{By, Kind};
use crate::model::DeskEvent;
use crate::ticket_dev;
use crate::ticket_files::{self, FileRead, FileSource, TicketFile, FILE_MAX};
use crate::ticket_markdown::AdfFile;

/// How long a ticket's issue, read whole for its files, is taken as read.
pub const READ_FRESH: chrono::Duration = chrono::Duration::minutes(5);

/// The longest one optional part of a ticket's development is waited for
/// when it opens: a slow dev-status never holds the ticket up.
const DEV_READ_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

/// The most pages of an issue's children one read of it takes.
const CHILD_READS: usize = 3;

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
    /// says of its files and itself, and its development (PRs, branches,
    /// commits, links: [`ticket_dev`]): what opening a synced ticket does
    /// when its issue wasn't read in the last [`READ_FRESH`] (or `force`).
    /// A development tool the hub lacks, or one that fails, never fails the
    /// read: that part is left as it was.
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
        // The board's link knows the Jira site's own fields.
        let sync = self
            .board_sync(t.board)
            .await
            .ok()
            .flatten()
            .map(|(_, s)| s);
        let fields = sync.as_ref().and_then(|s| s.fields.as_ref());
        let (tool, mut args) = crate::board_sync::read_issue_call(e.system, &e.key, fields);
        // Its description as Jira stores it, which says more than its HTML.
        if e.system == System::Jira {
            args["description_adf"] = serde_json::json!(true);
        }
        let read = crate::board_sync::call(remote, e.system, tool, args);
        // Its development, beside it: Jira's by tools of its own, at the
        // same time; Linear's comes with the issue.
        let (v, work, links) = match e.system {
            System::Jira => {
                let (v, work, links) = futures::join!(
                    read,
                    self.read_dev_part(id, remote, &e, ticket_dev::dev_status_call(&e)),
                    self.read_dev_part(id, remote, &e, ticket_dev::remote_links_call(&e)),
                );
                (
                    v?,
                    work.as_ref().and_then(ticket_dev::jira_work),
                    links.as_ref().and_then(ticket_dev::jira_links),
                )
            }
            System::Linear => {
                let v = read.await?;
                let dev = ticket_dev::linear_dev(v.get("issue").unwrap_or(&v));
                let (work, links) = dev.map_or((None, None), |(w, l)| (Some(w), Some(l)));
                (v, work, links)
            }
        };
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
        let adf = match e.system {
            // Its site, by Jira's own address of it, else the ticket's.
            System::Jira => {
                let own = v["self"].as_str().unwrap_or(&e.url);
                self.adf_body(id, &v, own).await
            }
            System::Linear => None,
        };
        let mut t = self.ticket_of(id).await.map_err(fail)?;
        if let Some(ext) = t.external.as_mut() {
            let priorities = sync.as_ref().map_or(&[][..], crate::board_sync::priorities);
            let mut details = crate::board_sync::details_of(&issue, Some(&ext.details), priorities);
            details.read_at = Some(now);
            ext.details = details;
            // Held as the body is, while it isn't one edited here.
            if let Some(adf) = adf.filter(|_| t.body == ext.seen.body) {
                // The HTML a pull reads of it, built from the same answer as
                // a pull builds it: while a pull reads that, this stays.
                ext.html_print = issue.body.as_deref().map(crate::board_sync::body_print);
                ext.html_read = crate::board_sync::ADF_READ;
                ext.clipped = adf.clipped;
                ext.flagged = ext.flagged || adf.flagged;
                ext.seen.body = adf.body.clone();
                t.body = adf.body;
            }
            ext.dev = ticket_dev::merged(ext.dev.take(), work, links, now);
        }
        // A Jira issue's sub-tasks say nothing of who has each, and an
        // epic's children aren't on it: one search over its children says.
        // It never fails the read: what was known stays.
        let facts = t.external.as_ref().map(|e| &e.details.facts);
        let parent = facts.is_some_and(|f| {
            e.system == System::Jira && (!f.children.is_empty() || crate::board_sync::is_epic(f))
        });
        if let (true, Some(s)) = (parent, sync.as_ref()) {
            let mut budget = super::board_sync::Budget(CHILD_READS);
            let mut pages = super::board_sync::Budget(CHILD_READS);
            let keys = [e.key.clone()];
            match self
                .read_children(s, remote, &mut budget, &mut pages, &keys)
                .await
            {
                Ok(Some(mut found)) => {
                    let ext = t.external.as_mut().expect("a synced ticket");
                    let epic = crate::board_sync::is_epic(&ext.details.facts);
                    let children = found.of.remove(&e.key).unwrap_or_default();
                    crate::board_sync::set_children(
                        &mut ext.details.facts,
                        children,
                        &found.blocked,
                        epic,
                    );
                }
                Ok(None) | Err(RemoteError::Spent) => {}
                Err(err) => {
                    tracing::info!(ticket = %id, error = %err, "an issue's children weren't read");
                }
            }
        }
        self.save_ticket(&mut t).await.map_err(fail)?;
        let v = self.view_ticket(t).await.map_err(fail)?;
        self.announce(DeskEvent::Ticket {
            ticket: Box::new(v.clone()),
        });
        Ok(v)
    }

    /// The description `answer` (a `jira.get_issue` one) gives as Jira
    /// stores it, as Desk markdown, its pictures found among ticket `id`'s
    /// files and its issue links on the site of `own` (the ticket's
    /// address) chips; none when the hub doesn't give it, it is larger than
    /// [`ADF_MAX`], or it doesn't read as a document.
    async fn adf_body(&self, id: Uuid, answer: &serde_json::Value, own: &str) -> Option<AdfBody> {
        let text = answer.get("description_adf")?.as_str()?;
        if text.len() > ADF_MAX {
            return None;
        }
        let (text, flagged) = crate::board_sync::unfence(text);
        let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
        if doc["type"] != "doc" {
            return None;
        }
        let files = match self.0.store.files_of(&[id]).await {
            Ok(mut f) => f.remove(&id).unwrap_or_default(),
            Err(err) => {
                tracing::warn!(ticket = %id, error = format!("{err:#}"), "a ticket's files weren't read for its description");
                vec![]
            }
        };
        let files: Vec<AdfFile<'_>> = files
            .iter()
            .filter(|f| f.source == FileSource::Jira)
            .map(|f| AdfFile {
                id: f.id,
                name: &f.name,
            })
            .collect();
        let md = crate::ticket_markdown::adf_markdown(&doc, &files, own);
        let (body, clipped) = crate::board_sync::held_body(md);
        Some(AdfBody {
            body,
            clipped,
            flagged,
        })
    }

    /// One optional part of ticket `id`'s development, read through
    /// `remote` by `call`: its answer, or `None` (logged) when the hub
    /// doesn't offer the tool, it failed, or it took over [`DEV_READ_WAIT`],
    /// so that part is left as it was and the ticket still reads. Quiet: an
    /// approval is never asked for it.
    async fn read_dev_part(
        &self,
        id: Uuid,
        remote: &dyn Remote,
        e: &crate::board::External,
        (tool, args): (&'static str, serde_json::Value),
    ) -> Option<serde_json::Value> {
        let read = crate::board_sync::call_quiet(remote, e.system, tool, args);
        let Ok(answer) = tokio::time::timeout(DEV_READ_WAIT, read).await else {
            tracing::warn!(ticket = %id, tool, "a ticket's development took too long to read");
            return None;
        };
        match answer {
            Ok(v) => Some(v),
            Err(RemoteError::Missing(_) | RemoteError::NotAllowed(_)) => {
                tracing::debug!(ticket = %id, tool, "the hub doesn't offer a ticket's development tool");
                None
            }
            Err(err) => {
                tracing::warn!(ticket = %id, tool, error = %err, "a ticket's development wasn't read");
                None
            }
        }
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

/// The most of a description's ADF read: a larger one is read as its HTML.
const ADF_MAX: usize = 2 * 1024 * 1024;

/// A Jira description read as ADF, as Desk holds it.
struct AdfBody {
    body: String,
    clipped: bool,
    flagged: bool,
}

/// A file's name in a history line: on one line, defanged, quoted.
fn quoted(name: &str) -> String {
    format!("“{}”", crate::board_sync::defanged_name(name))
}
