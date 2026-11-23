//! The IMAP commands mail management needs, as one trait: the real session
//! implements it over async-imap, and tests drive the same code with a fake.

use std::fmt;
use std::time::Duration;

use async_imap::imap_proto::types::{NameAttribute, Response, ResponseCode, Status, UidSetMember};
use async_trait::async_trait;
use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use super::client::ImapClient;
use super::error::{from_imap_error, EmailError};
use super::uids::UidSet;

/// One folder as LIST reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub delimiter: Option<String>,
    pub attributes: Vec<String>,
}

/// What SELECT reports about the folder it opened. `validity` is unknown
/// when the server gave no UIDVALIDITY: its uids may then be renumbered
/// unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selected {
    pub validity: Option<u32>,
    pub exists: u32,
}

/// What STATUS reports about a folder: how many messages it holds, its
/// numbering, and the uid its next message will get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderStatus {
    pub messages: u32,
    pub validity: Option<u32>,
    pub next: Option<u32>,
}

/// Where copied or moved messages landed, from a COPYUID response code
/// (RFC 4315): `source[i]` became `dest[i]` in a folder of `validity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub validity: u32,
    pub source: Vec<u32>,
    pub dest: Vec<u32>,
}

/// One message's sender, subject and date, for a dry run's sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: String,
}

#[async_trait]
pub trait MailSession: Send {
    async fn capabilities(&mut self) -> Result<Vec<String>, EmailError>;
    async fn list(&mut self) -> Result<Vec<Listed>, EmailError>;
    async fn select(&mut self, folder: &str) -> Result<Selected, EmailError>;
    async fn examine(&mut self, folder: &str) -> Result<Selected, EmailError>;
    async fn search(&mut self, query: &str) -> Result<UidSet, EmailError>;
    /// `UID MOVE` (RFC 6851).
    async fn move_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError>;
    /// `UID COPY`.
    async fn copy_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError>;
    /// `UID STORE uids change` with a `.SILENT` change such as `+FLAGS.SILENT (\Seen)`.
    async fn store(&mut self, uids: &UidSet, change: &str) -> Result<(), EmailError>;
    /// `UID EXPUNGE uids` (RFC 4315): only these, never another message
    /// marked deleted.
    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<(), EmailError>;
    /// The raw header block of each message for `fields`, by uid.
    async fn headers(
        &mut self,
        uids: &UidSet,
        fields: &[&str],
    ) -> Result<Vec<(u32, String)>, EmailError>;
    async fn summaries(&mut self, uids: &UidSet) -> Result<Vec<Summary>, EmailError>;
    async fn status(&mut self, folder: &str) -> Result<FolderStatus, EmailError>;
    async fn create(&mut self, folder: &str) -> Result<(), EmailError>;
    async fn rename(&mut self, from: &str, to: &str) -> Result<(), EmailError>;
    async fn delete(&mut self, folder: &str) -> Result<(), EmailError>;
}

fn imap<E: Into<async_imap::error::Error>>(what: &str) -> impl Fn(E) -> EmailError + '_ {
    move |e| from_imap_error(what, e.into())
}

fn attribute_name(attribute: &NameAttribute<'_>) -> String {
    match attribute {
        NameAttribute::NoInferiors => "\\Noinferiors".to_string(),
        NameAttribute::NoSelect => "\\Noselect".to_string(),
        NameAttribute::Marked => "\\Marked".to_string(),
        NameAttribute::Unmarked => "\\Unmarked".to_string(),
        NameAttribute::All => "\\All".to_string(),
        NameAttribute::Archive => "\\Archive".to_string(),
        NameAttribute::Drafts => "\\Drafts".to_string(),
        NameAttribute::Flagged => "\\Flagged".to_string(),
        NameAttribute::Junk => "\\Junk".to_string(),
        NameAttribute::Sent => "\\Sent".to_string(),
        NameAttribute::Trash => "\\Trash".to_string(),
        NameAttribute::Extension(name) => name.to_string(),
        other => format!("{other:?}"),
    }
}

fn selected(mailbox: async_imap::types::Mailbox) -> Selected {
    Selected {
        validity: mailbox.uid_validity,
        exists: mailbox.exists,
    }
}

fn header_text(fetch: &async_imap::types::Fetch) -> String {
    fetch
        .header()
        .map(|h| String::from_utf8_lossy(h).into_owned())
        .unwrap_or_default()
}

fn cow(bytes: Option<&std::borrow::Cow<'_, [u8]>>) -> String {
    bytes
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

#[async_trait]
impl<T> MailSession for async_imap::Session<T>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    async fn capabilities(&mut self) -> Result<Vec<String>, EmailError> {
        let caps = async_imap::Session::capabilities(self)
            .await
            .map_err(imap("CAPABILITY"))?;
        Ok(caps
            .iter()
            .map(|c| match c {
                async_imap::types::Capability::Imap4rev1 => "IMAP4rev1".to_string(),
                async_imap::types::Capability::Auth(a) => format!("AUTH={a}"),
                async_imap::types::Capability::Atom(a) => a.to_string(),
            })
            .collect())
    }

    async fn list(&mut self) -> Result<Vec<Listed>, EmailError> {
        let names: Vec<_> = async_imap::Session::list(self, Some(""), Some("*"))
            .await
            .map_err(imap("LIST"))?
            .try_collect()
            .await
            .map_err(imap("LIST"))?;
        Ok(names
            .iter()
            .map(|n| Listed {
                name: n.name().to_string(),
                delimiter: n.delimiter().map(str::to_string),
                attributes: n.attributes().iter().map(attribute_name).collect(),
            })
            .collect())
    }

    async fn select(&mut self, folder: &str) -> Result<Selected, EmailError> {
        async_imap::Session::select(self, folder)
            .await
            .map(selected)
            .map_err(|e| from_imap_error(&format!("SELECT {folder}"), e))
    }

    async fn examine(&mut self, folder: &str) -> Result<Selected, EmailError> {
        async_imap::Session::examine(self, folder)
            .await
            .map(selected)
            .map_err(|e| from_imap_error(&format!("EXAMINE {folder}"), e))
    }

    async fn search(&mut self, query: &str) -> Result<UidSet, EmailError> {
        async_imap::Session::uid_search(self, query)
            .await
            .map(|uids| uids.into_iter().collect())
            .map_err(imap("SEARCH"))
    }

    async fn move_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError> {
        let command = format!("UID MOVE {uids} {}", mailbox_arg(to)?);
        landing(self, &command, uids.len()).await
    }

    async fn copy_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError> {
        let command = format!("UID COPY {uids} {}", mailbox_arg(to)?);
        landing(self, &command, uids.len()).await
    }

    async fn store(&mut self, uids: &UidSet, change: &str) -> Result<(), EmailError> {
        let _: Vec<_> = async_imap::Session::uid_store(self, uids.to_string(), change)
            .await
            .map_err(imap("STORE"))?
            .try_collect()
            .await
            .map_err(imap("STORE"))?;
        Ok(())
    }

    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<(), EmailError> {
        let _: Vec<_> = async_imap::Session::uid_expunge(self, uids.to_string())
            .await
            .map_err(imap("UID EXPUNGE"))?
            .try_collect()
            .await
            .map_err(imap("UID EXPUNGE"))?;
        Ok(())
    }

    async fn headers(
        &mut self,
        uids: &UidSet,
        fields: &[&str],
    ) -> Result<Vec<(u32, String)>, EmailError> {
        let query = format!("(UID BODY.PEEK[HEADER.FIELDS ({})])", fields.join(" "));
        let fetches: Vec<_> = async_imap::Session::uid_fetch(self, uids.to_string(), query)
            .await
            .map_err(imap("FETCH"))?
            .try_collect()
            .await
            .map_err(imap("FETCH"))?;
        Ok(fetches
            .iter()
            .filter_map(|f| f.uid.map(|uid| (uid, header_text(f))))
            .collect())
    }

    async fn summaries(&mut self, uids: &UidSet) -> Result<Vec<Summary>, EmailError> {
        let fetches: Vec<_> =
            async_imap::Session::uid_fetch(self, uids.to_string(), "(UID ENVELOPE)")
                .await
                .map_err(imap("FETCH"))?
                .try_collect()
                .await
                .map_err(imap("FETCH"))?;
        Ok(fetches
            .iter()
            .filter_map(|f| {
                let envelope = f.envelope()?;
                let from = envelope
                    .from
                    .as_ref()
                    .and_then(|a| a.first())
                    .map(|a| format!("{}@{}", cow(a.mailbox.as_ref()), cow(a.host.as_ref())))
                    .unwrap_or_default();
                Some(Summary {
                    uid: f.uid?,
                    from,
                    subject: cow(envelope.subject.as_ref()),
                    date: cow(envelope.date.as_ref()),
                })
            })
            .collect())
    }

    async fn status(&mut self, folder: &str) -> Result<FolderStatus, EmailError> {
        async_imap::Session::status(self, folder, "(MESSAGES UIDNEXT UIDVALIDITY)")
            .await
            .map(|m| FolderStatus {
                messages: m.exists,
                validity: m.uid_validity,
                next: m.uid_next,
            })
            .map_err(imap("STATUS"))
    }

    async fn create(&mut self, folder: &str) -> Result<(), EmailError> {
        async_imap::Session::create(self, folder)
            .await
            .map_err(imap("CREATE"))
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<(), EmailError> {
        async_imap::Session::rename(self, from, to)
            .await
            .map_err(imap("RENAME"))
    }

    async fn delete(&mut self, folder: &str) -> Result<(), EmailError> {
        async_imap::Session::delete(self, folder)
            .await
            .map_err(imap("DELETE"))
    }
}

/// The uids a COPYUID response code lists, in its order, when it lists no
/// more than `most`: a server's claim is never expanded past the batch.
fn ordered(members: &[UidSetMember], most: usize) -> Option<Vec<u32>> {
    let runs: Vec<(u32, u32)> = members
        .iter()
        .map(|m| match m {
            UidSetMember::Uid(u) => (*u, *u),
            UidSetMember::UidRange(r) => (*r.start().min(r.end()), *r.start().max(r.end())),
        })
        .collect();
    let size: u64 = runs.iter().map(|(a, b)| u64::from(b - a) + 1).sum();
    (size <= most as u64).then(|| runs.into_iter().flat_map(|(a, b)| a..=b).collect())
}

/// Where a COPYUID code says `expect` messages landed, when it accounts for
/// each of them once.
fn landed_of(
    validity: u32,
    source: &[UidSetMember],
    dest: &[UidSetMember],
    expect: usize,
) -> Option<Landed> {
    ordered(source, expect)
        .zip(ordered(dest, expect))
        .filter(|(source, dest)| source.len() == dest.len())
        .map(|(source, dest)| Landed {
            validity,
            source,
            dest,
        })
}

/// A mailbox name as a command argument; never one that could end the
/// command early.
fn mailbox_arg(name: &str) -> Result<String, EmailError> {
    match name.contains(['\r', '\n', '\0']) {
        true => Err(EmailError::Imap(
            "A folder name can't contain a line break or a NUL.".to_string(),
        )),
        false => Ok(format!(
            "\"{}\"",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )),
    }
}

/// Runs a COPY or MOVE of `expect` messages and reads its responses to the
/// tagged end, keeping the COPYUID the server reports: on the tagged OK for
/// a COPY, on an untagged OK for a MOVE (RFC 6851 §4.3). async-imap's own
/// calls drop it.
async fn landing<T>(
    session: &mut async_imap::Session<T>,
    command: &str,
    expect: usize,
) -> Result<Option<Landed>, EmailError>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let tag = session.run_command(command).await.map_err(imap(command))?;
    let mut landed = None;
    while let Some(response) = session.read_response().await {
        let response = response.map_err(imap(command))?;
        let (done, status, code, information) = match response.parsed() {
            Response::Done {
                tag: done,
                status,
                code,
                information,
            } => (Some(done == &tag), status, code, information),
            Response::Data {
                status,
                code,
                information,
            } => (None, status, code, information),
            _ => continue,
        };
        if let Some(ResponseCode::CopyUid(validity, source, dest)) = code {
            landed = landed_of(*validity, source, dest, expect);
        }
        match (done, status) {
            (Some(true), Status::Ok) => return Ok(landed),
            (Some(true), _) => {
                return Err(EmailError::MoveFailure {
                    detail: format!("{command}: {}", information.as_deref().unwrap_or("refused")),
                })
            }
            _ => {}
        }
    }
    Err(EmailError::Disconnected {
        operation: command.to_string(),
        detail: "the connection closed before the server answered".to_string(),
    })
}

/// An account's session whose every command gives up after the account's
/// command timeout, and then drops the connection, which may still carry
/// the late answer: a stalled server never holds the account's lock. A
/// connection that dropped is let go too, so the next use connects afresh.
pub struct Timed<'a> {
    client: &'a mut ImapClient,
    limit: Duration,
}

impl<'a> Timed<'a> {
    /// `client`, whose session was just checked.
    pub fn new(client: &'a mut ImapClient) -> Self {
        let limit = client.command_timeout();
        Self { client, limit }
    }
}

macro_rules! timed {
    ($self:ident, $what:literal, $method:ident($($arg:expr),*)) => {{
        let limit = $self.limit;
        let session = $self.client.current()?;
        match tokio::time::timeout(limit, MailSession::$method(session, $($arg),*)).await {
            Ok(Err(e)) if e.is_transient() => {
                $self.client.forget();
                Err(e)
            }
            Ok(done) => done,
            Err(_) => {
                $self.client.forget();
                Err(EmailError::Timeout {
                    operation: $what.to_string(),
                    duration: limit,
                })
            }
        }
    }};
}

#[async_trait]
impl MailSession for Timed<'_> {
    async fn capabilities(&mut self) -> Result<Vec<String>, EmailError> {
        timed!(self, "CAPABILITY", capabilities())
    }

    async fn list(&mut self) -> Result<Vec<Listed>, EmailError> {
        timed!(self, "LIST", list())
    }

    async fn select(&mut self, folder: &str) -> Result<Selected, EmailError> {
        timed!(self, "SELECT", select(folder))
    }

    async fn examine(&mut self, folder: &str) -> Result<Selected, EmailError> {
        timed!(self, "EXAMINE", examine(folder))
    }

    async fn search(&mut self, query: &str) -> Result<UidSet, EmailError> {
        timed!(self, "SEARCH", search(query))
    }

    async fn move_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError> {
        timed!(self, "UID MOVE", move_uids(uids, to))
    }

    async fn copy_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError> {
        timed!(self, "UID COPY", copy_uids(uids, to))
    }

    async fn store(&mut self, uids: &UidSet, change: &str) -> Result<(), EmailError> {
        timed!(self, "UID STORE", store(uids, change))
    }

    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<(), EmailError> {
        timed!(self, "UID EXPUNGE", expunge_uids(uids))
    }

    async fn headers(
        &mut self,
        uids: &UidSet,
        fields: &[&str],
    ) -> Result<Vec<(u32, String)>, EmailError> {
        timed!(self, "FETCH", headers(uids, fields))
    }

    async fn summaries(&mut self, uids: &UidSet) -> Result<Vec<Summary>, EmailError> {
        timed!(self, "FETCH", summaries(uids))
    }

    async fn status(&mut self, folder: &str) -> Result<FolderStatus, EmailError> {
        timed!(self, "STATUS", status(folder))
    }

    async fn create(&mut self, folder: &str) -> Result<(), EmailError> {
        timed!(self, "CREATE", create(folder))
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<(), EmailError> {
        timed!(self, "RENAME", rename(from, to))
    }

    async fn delete(&mut self, folder: &str) -> Result<(), EmailError> {
        timed!(self, "DELETE", delete(folder))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copyuid_members_keep_their_order() {
        let members = vec![UidSetMember::UidRange(5..=7), UidSetMember::Uid(2)];
        assert_eq!(ordered(&members, 4), Some(vec![5, 6, 7, 2]));
    }

    #[test]
    fn a_copyuid_wider_than_its_batch_is_never_expanded() {
        let all = [UidSetMember::UidRange(1..=u32::MAX)];
        assert_eq!(ordered(&all, 500), None);
        let two = [UidSetMember::UidRange(1..=2)];
        let three = [UidSetMember::UidRange(10..=12)];
        assert_eq!(landed_of(7, &two, &three, 3), None, "not one to one");
        assert_eq!(landed_of(7, &all, &three, 3), None);
        assert_eq!(
            landed_of(7, &two, &[UidSetMember::UidRange(10..=11)], 2),
            Some(Landed {
                validity: 7,
                source: vec![1, 2],
                dest: vec![10, 11]
            })
        );
    }

    #[test]
    fn a_mailbox_argument_is_quoted_and_never_breaks_the_line() {
        assert_eq!(
            mailbox_arg("Clients/\"Acme\"").unwrap(),
            "\"Clients/\\\"Acme\\\"\""
        );
        for bad in ["a\r\nA1 DELETE INBOX", "a\nb", "a\0b"] {
            assert!(mailbox_arg(bad).is_err(), "{bad:?}");
        }
    }
}
