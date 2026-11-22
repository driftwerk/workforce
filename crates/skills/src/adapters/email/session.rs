//! The IMAP commands mail management needs, as one trait: the real session
//! implements it over async-imap, and tests drive the same code with a fake.

use std::fmt;

use async_imap::imap_proto::types::{NameAttribute, Response, ResponseCode, Status, UidSetMember};
use async_trait::async_trait;
use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use super::error::EmailError;
use super::uids::UidSet;

/// One folder as LIST reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub delimiter: Option<String>,
    pub attributes: Vec<String>,
}

/// What SELECT reports about the folder it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selected {
    pub validity: u32,
    pub exists: u32,
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
    async fn messages_in(&mut self, folder: &str) -> Result<u32, EmailError>;
    async fn create(&mut self, folder: &str) -> Result<(), EmailError>;
    async fn rename(&mut self, from: &str, to: &str) -> Result<(), EmailError>;
    async fn delete(&mut self, folder: &str) -> Result<(), EmailError>;
}

fn imap(what: &str) -> impl Fn(async_imap::error::Error) -> EmailError + '_ {
    move |e| EmailError::Imap(format!("{what} failed: {e}"))
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
        validity: mailbox.uid_validity.unwrap_or(0),
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
            .map_err(|e| EmailError::Imap(format!("Failed to select folder {folder}: {e}")))
    }

    async fn examine(&mut self, folder: &str) -> Result<Selected, EmailError> {
        async_imap::Session::examine(self, folder)
            .await
            .map(selected)
            .map_err(|e| EmailError::Imap(format!("Failed to open folder {folder}: {e}")))
    }

    async fn search(&mut self, query: &str) -> Result<UidSet, EmailError> {
        async_imap::Session::uid_search(self, query)
            .await
            .map(|uids| uids.into_iter().collect())
            .map_err(imap("SEARCH"))
    }

    async fn move_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError> {
        landing(self, &format!("UID MOVE {uids} {}", mailbox_arg(to))).await
    }

    async fn copy_uids(&mut self, uids: &UidSet, to: &str) -> Result<Option<Landed>, EmailError> {
        landing(self, &format!("UID COPY {uids} {}", mailbox_arg(to))).await
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

    async fn messages_in(&mut self, folder: &str) -> Result<u32, EmailError> {
        async_imap::Session::status(self, folder, "(MESSAGES)")
            .await
            .map(|m| m.exists)
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

/// The uids a COPYUID response code lists, in its order.
fn ordered(members: &[UidSetMember]) -> Vec<u32> {
    members
        .iter()
        .flat_map(|m| match m {
            UidSetMember::Uid(u) => *u..=*u,
            UidSetMember::UidRange(r) => r.clone(),
        })
        .collect()
}

/// A mailbox name as a command argument.
fn mailbox_arg(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Runs a COPY or MOVE and reads its responses to the tagged end, keeping
/// the COPYUID the server reports: on the tagged OK for a COPY, on an
/// untagged OK for a MOVE (RFC 6851 §4.3). async-imap's own calls drop it.
async fn landing<T>(
    session: &mut async_imap::Session<T>,
    command: &str,
) -> Result<Option<Landed>, EmailError>
where
    T: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    let failed = |e: &dyn fmt::Display| EmailError::MoveFailure {
        detail: format!("{command}: {e}"),
    };
    let tag = session.run_command(command).await.map_err(|e| failed(&e))?;
    let mut landed = None;
    while let Some(response) = session.read_response().await {
        let response = response.map_err(|e| failed(&e))?;
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
            landed = Some(Landed {
                validity: *validity,
                source: ordered(source),
                dest: ordered(dest),
            });
        }
        match (done, status) {
            (Some(true), Status::Ok) => return Ok(landed),
            (Some(true), _) => {
                return Err(failed(
                    &information.as_deref().unwrap_or("refused").to_string(),
                ))
            }
            _ => {}
        }
    }
    Err(failed(&"the connection closed before the server answered"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copyuid_members_keep_their_order() {
        let members = vec![UidSetMember::UidRange(5..=7), UidSetMember::Uid(2)];
        assert_eq!(ordered(&members), vec![5, 6, 7, 2]);
    }

    #[test]
    fn a_mailbox_argument_is_quoted() {
        assert_eq!(mailbox_arg("Clients/\"Acme\""), "\"Clients/\\\"Acme\\\"\"");
    }
}
