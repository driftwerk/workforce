//! A ticket's files: what its Jira or Linear issue has attached (and the
//! pictures uploaded into a Linear description), and what the person drops
//! on a ticket of their own. Kept in `ticket_files`, one row each.
//!
//! A synced ticket's files are its issue's: the pull keeps the list in step
//! and their bytes are read on demand, through the route the board syncs
//! by, as the person, then kept here a while ([`CACHE_MAX`]). A local
//! ticket's files are uploads, kept here whole.
//!
//! Every name and title is someone else's words: kept as text, served with
//! a sanitised filename, and fenced wherever a model reads it.

use std::collections::HashMap;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::board::System;
use crate::board_sync::{unfence, Remote, RemoteError};
use crate::crew::Party;
use crate::store::DeskStore;

/// The largest file read or kept: what a model's view takes too.
pub const FILE_MAX: u64 = 20 * 1024 * 1024;
/// The most files a ticket of the person's own holds.
pub const UPLOADS_PER_TICKET: usize = 20;
/// The most of a remote file's bytes kept, across every ticket; past it the
/// least lately read go.
pub const CACHE_MAX: i64 = 256 * 1024 * 1024;
/// The most files of an issue listed.
pub const LISTED_MAX: usize = 50;
/// The largest chunk asked for at once.
const CHUNK: u64 = 512 * 1024;

/// Where a file comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSource {
    /// Dropped on a ticket of the person's own: its bytes are here.
    Upload,
    /// A Jira attachment, by its id.
    Jira,
    /// A file uploaded to Linear, by its `https://uploads.linear.app/` address.
    LinearUpload,
    /// A Linear attachment that is a link (a pull request, a design): no
    /// bytes, opened where it is.
    LinearLink,
}

impl FileSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Jira => "jira",
            Self::LinearUpload => "linear_upload",
            Self::LinearLink => "linear_link",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "upload" => Self::Upload,
            "jira" => Self::Jira,
            "linear_upload" => Self::LinearUpload,
            "linear_link" => Self::LinearLink,
            other => eyre::bail!("unknown file source {other:?}"),
        })
    }

    /// Whether its bytes can be read: everything but a link.
    pub fn has_bytes(self) -> bool {
        self != Self::LinearLink
    }
}

/// A ticket's file as Desk shows it. Its bytes are at
/// `GET /api/desk/tickets/{ticket}/files/{id}` (`?thumb=1` for its small
/// picture, when `thumb`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketFile {
    pub id: Uuid,
    pub source: FileSource,
    /// What its issue calls it (the Jira attachment's id, the Linear
    /// attachment's id or the upload's address): never sent to the page,
    /// never taken from it.
    #[serde(skip)]
    pub remote_id: Option<String>,
    pub name: String,
    /// As its issue says, or as sniffed from an upload's bytes.
    pub media_type: String,
    pub size: Option<u64>,
    /// A link's address (`https`), or a Linear upload's, which its
    /// description shows it by.
    pub url: Option<String>,
    /// Jira keeps a small picture of it.
    pub thumb: bool,
    pub added_by: Party,
    pub added_at: DateTime<Utc>,
}

impl TicketFile {
    /// What it is, as the view draws it.
    pub fn kind(&self) -> &'static str {
        match (self.source, self.media_type.as_str()) {
            (FileSource::LinearLink, _) => "link",
            (_, m) if RASTER.contains(&m) => "image",
            (_, "application/pdf") => "pdf",
            _ => "file",
        }
    }
}

/// A file as an issue's answer lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFile {
    pub source: FileSource,
    pub remote_id: String,
    pub name: String,
    pub media_type: String,
    pub size: Option<u64>,
    pub url: Option<String>,
    pub thumb: bool,
}

// ── Reading issues' answers ──────────────────────────────────────────────

/// A name someone else wrote, on one line, unwrapped and held short.
fn name_of(v: &Value) -> Option<String> {
    let name = unfence(v.as_str()?).0;
    let name = crate::crew::one_line(name.trim(), 200);
    (!name.is_empty()).then_some(name)
}

/// The files a Jira issue's answer lists (`attachments`, as `jira.get_issue`
/// and a search asking for `attachment` give them); `None` when it lists
/// none at all, so an older hub's answer leaves what is known.
pub fn jira_files(issue: &Value) -> Option<Vec<RemoteFile>> {
    let list = issue.get("attachments")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|a| {
                let id = match &a["id"] {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return None,
                };
                if id.is_empty() || id.len() > 20 || !id.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                Some(RemoteFile {
                    source: FileSource::Jira,
                    name: name_of(&a["filename"]).unwrap_or_else(|| format!("attachment-{id}")),
                    remote_id: id,
                    media_type: a["mime_type"]
                        .as_str()
                        .filter(|m| m.len() < 100)
                        .unwrap_or("application/octet-stream")
                        .to_ascii_lowercase(),
                    size: a["size"].as_u64(),
                    url: None,
                    thumb: a["thumbnail"].as_bool() == Some(true),
                })
            })
            .take(LISTED_MAX)
            .collect(),
    )
}

/// The files a Linear issue's answer and description hold: its
/// attachments (links, or files on `uploads.linear.app`) and the uploads its
/// description shows or links to.
pub fn linear_files(issue: &Value, body: &str) -> Vec<RemoteFile> {
    let mut out: Vec<RemoteFile> = vec![];
    let mut seen = std::collections::HashSet::new();
    for (name, url) in description_uploads(body) {
        if seen.insert(url.clone()) {
            out.push(upload_file(name, url));
        }
    }
    for a in issue["attachments"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let Some(url) = a["url"].as_str().map(str::trim) else {
            continue;
        };
        let title = name_of(&a["title"]);
        if is_linear_upload(url) {
            if seen.insert(url.to_string()) {
                out.push(upload_file(title, url.to_string()));
            }
            continue;
        }
        let (Some(id), true) = (a["id"].as_str(), is_https(url)) else {
            continue;
        };
        if id.is_empty() || id.len() > 64 {
            continue;
        }
        out.push(RemoteFile {
            source: FileSource::LinearLink,
            remote_id: id.to_string(),
            name: title
                .or_else(|| url::Url::parse(url).ok()?.host_str().map(str::to_string))
                .unwrap_or_else(|| "Link".into()),
            media_type: "text/uri-list".into(),
            size: None,
            url: Some(url.to_string()),
            thumb: false,
        });
    }
    out.truncate(LISTED_MAX);
    out
}

fn upload_file(name: Option<String>, url: String) -> RemoteFile {
    let tail = url
        .split(['?', '#'])
        .next()
        .and_then(|p| p.rsplit('/').next())
        .filter(|t| !t.is_empty())
        .unwrap_or("upload")
        .to_string();
    RemoteFile {
        source: FileSource::LinearUpload,
        remote_id: url.clone(),
        media_type: type_of_name(&tail).into(),
        name: name.filter(|n| !n.is_empty()).unwrap_or(tail),
        size: None,
        url: Some(url),
        thumb: false,
    }
}

fn is_https(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some())
}

/// Whether `url` is a file Linear keeps for its uploads: the one place
/// Linear's key is sent besides its API (checked again by the tool).
pub fn is_linear_upload(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    u.scheme() == "https"
        && u.host_str() == Some("uploads.linear.app")
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.path().len() > 1
}

/// The uploads a Markdown description shows (`![alt](url)`) or links to
/// (`[name](url)`), in order: their text and address.
pub fn description_uploads(body: &str) -> Vec<(Option<String>, String)> {
    const AT: &str = "](https://uploads.linear.app/";
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = body[from..].find(AT) {
        let close = from + i;
        let start = close + 2;
        let end = body[start..]
            .find(|c: char| c == ')' || c.is_whitespace())
            .map_or(body.len(), |e| start + e);
        let url = &body[start..end];
        let text = body[..close]
            .rfind('[')
            .map(|o| &body[o + 1..close])
            .filter(|t| !t.contains(['[', ']', '\n']))
            .map(|t| crate::crew::one_line(t.trim(), 200))
            .filter(|t| !t.is_empty());
        if is_linear_upload(url) {
            out.push((text, url.to_string()));
        }
        from = end.max(start);
    }
    out
}

/// What a file's name says it is.
pub fn type_of_name(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
}

// ── Serving bytes ────────────────────────────────────────────────────────

/// The pictures Desk draws: never SVG, which can carry script.
pub const RASTER: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// What `bytes` are by their first bytes, never by what anyone said:
/// a raster picture or a PDF; `None` for anything else.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let b = bytes;
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && (&b[8..12] == b"avif" || &b[8..12] == b"avis")
    {
        Some("image/avif")
    } else if b.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else {
        None
    }
}

/// How a file's bytes are served: the type the page is told, and whether
/// it may be shown in place. Only a sniffed raster picture or PDF is
/// either; anything else is a download of unknown bytes.
pub fn served_as(bytes: &[u8]) -> (&'static str, bool) {
    match sniff(bytes) {
        Some(t) => (t, true),
        None => ("application/octet-stream", false),
    }
}

/// `name` as a filename a header can carry: letters, digits, `.`, `_`,
/// `-` and spaces, no leading dot, at most 120 characters.
pub fn safe_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' | ' ' => c,
            _ => '_',
        })
        .take(120)
        .collect();
    let s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

/// Why a remote file wasn't read, in the sync's plain words.
#[derive(Debug, thiserror::Error)]
pub enum FileRead {
    #[error("{0}")]
    Remote(RemoteError),
    #[error("That file is over {} MB.", FILE_MAX / (1024 * 1024))]
    TooBig,
    #[error("{0} sent something that doesn't add up.")]
    Garbled(&'static str),
    #[error("That's a link: open it where it is.")]
    Link,
}

/// The call that reads `range` of `f`, on `system`, for the issue `key`.
fn read_call(
    system: System,
    key: &str,
    f: &TicketFile,
    offset: u64,
    thumb: bool,
) -> Option<(&'static str, Value)> {
    let id = f.remote_id.as_deref()?;
    match (system, f.source) {
        (System::Jira, FileSource::Jira) => Some((
            "jira.attachment_read",
            serde_json::json!({ "issue_key": key, "attachment_id": id, "thumbnail": thumb,
                "offset": offset, "length": CHUNK }),
        )),
        (System::Linear, FileSource::LinearUpload) => Some((
            "linear.upload_read",
            serde_json::json!({ "identifier": key, "url": id, "offset": offset, "length": CHUNK }),
        )),
        _ => None,
    }
}

/// `f`'s bytes (or its small picture), read through `remote` a chunk at a
/// time, refused past [`FILE_MAX`] and stopped by a chunk that doesn't say
/// what was asked.
pub async fn read_remote(
    remote: &dyn Remote,
    system: System,
    key: &str,
    f: &TicketFile,
    thumb: bool,
) -> std::result::Result<Vec<u8>, FileRead> {
    if !f.source.has_bytes() {
        return Err(FileRead::Link);
    }
    if f.size.is_some_and(|n| n > FILE_MAX) && !thumb {
        return Err(FileRead::TooBig);
    }
    let who = system.title();
    let mut out: Vec<u8> = vec![];
    let mut size: Option<u64> = None;
    // Each chunk is at least one byte until the end, so this ends.
    for _ in 0..=(FILE_MAX / CHUNK + 1) {
        let offset = out.len() as u64;
        let (tool, args) =
            read_call(system, key, f, offset, thumb).ok_or(FileRead::Garbled(who))?;
        let answer = crate::board_sync::call(remote, system, tool, args)
            .await
            .map_err(FileRead::Remote)?;
        if answer["kind"].as_str() != Some("file_bytes") {
            return Err(FileRead::Garbled(who));
        }
        let total = answer["size"].as_u64().ok_or(FileRead::Garbled(who))?;
        if total > FILE_MAX {
            return Err(FileRead::TooBig);
        }
        if size.is_some_and(|s| s != total) || answer["offset"].as_u64() != Some(offset) {
            return Err(FileRead::Garbled(who));
        }
        size = Some(total);
        let data = base64::engine::general_purpose::STANDARD
            .decode(answer["data"].as_str().ok_or(FileRead::Garbled(who))?)
            .map_err(|_| FileRead::Garbled(who))?;
        if answer["length"].as_u64() != Some(data.len() as u64)
            || data.len() as u64 > CHUNK
            || offset + data.len() as u64 > total
        {
            return Err(FileRead::Garbled(who));
        }
        if data.is_empty() && offset < total {
            return Err(FileRead::Garbled(who));
        }
        out.extend_from_slice(&data);
        if out.len() as u64 == total {
            return Ok(out);
        }
    }
    Err(FileRead::Garbled(who))
}

// ── In Postgres ──────────────────────────────────────────────────────────

const FILE: &str =
    "id, ticket, source, remote_id, name, media_type, size, url, thumb, added_by, created_at";

fn file(row: &Row) -> Result<(Uuid, TicketFile)> {
    let size: Option<i64> = row.try_get("size")?;
    Ok((
        row.try_get("ticket")?,
        TicketFile {
            id: row.try_get("id")?,
            source: FileSource::parse(row.try_get("source")?)?,
            remote_id: row.try_get("remote_id")?,
            name: row.try_get("name")?,
            media_type: row.try_get("media_type")?,
            size: size.map(|s| s.max(0) as u64),
            url: row.try_get("url")?,
            thumb: row.try_get("thumb")?,
            added_by: serde_json::from_value(row.try_get("added_by")?)
                .context("reading who added a file")?,
            added_at: row.try_get("created_at")?,
        },
    ))
}

impl DeskStore {
    /// The files of each of `tickets`, in the order they were listed.
    pub async fn files_of(&self, tickets: &[Uuid]) -> Result<HashMap<Uuid, Vec<TicketFile>>> {
        let mut out: HashMap<Uuid, Vec<TicketFile>> = HashMap::new();
        if tickets.is_empty() {
            return Ok(out);
        }
        let client = self.pool().get().await?;
        for r in client
            .query(
                &format!(
                    "SELECT {FILE} FROM ticket_files WHERE ticket = ANY($1) \
                     ORDER BY ticket, position, created_at"
                ),
                &[&tickets],
            )
            .await?
        {
            let (t, f) = file(&r)?;
            out.entry(t).or_default().push(f);
        }
        Ok(out)
    }

    /// The file `id` of `ticket`, if it is that ticket's.
    pub async fn ticket_file(&self, ticket: Uuid, id: Uuid) -> Result<Option<TicketFile>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {FILE} FROM ticket_files WHERE ticket = $1 AND id = $2"),
                &[&ticket, &id],
            )
            .await?
            .map(|r| file(&r).map(|(_, f)| f))
            .transpose()
    }

    /// Keeps `ticket`'s remote files as its issue now lists them: new ones
    /// added, changed ones rewritten (their kept bytes forgotten), gone ones
    /// deleted with what was kept of them. Uploads are never touched.
    /// Whether anything changed.
    pub async fn put_remote_files(
        &self,
        ticket: Uuid,
        files: &[RemoteFile],
        by: &Party,
    ) -> Result<bool> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let was: Vec<TicketFile> = tx
            .query(
                &format!(
                    "SELECT {FILE} FROM ticket_files WHERE ticket = $1 AND source <> 'upload'"
                ),
                &[&ticket],
            )
            .await?
            .iter()
            .map(|r| file(r).map(|(_, f)| f))
            .collect::<Result<_>>()?;
        let mut changed = false;
        let now = Utc::now();
        for (pos, f) in files.iter().enumerate() {
            let size = f.size.map(|s| s.min(i64::MAX as u64) as i64);
            let remote_id = Some(f.remote_id.clone());
            let same = was
                .iter()
                .find(|w| w.source == f.source && w.remote_id == remote_id);
            match same {
                Some(w)
                    if w.name == f.name
                        && w.media_type == f.media_type
                        && w.size == f.size
                        && w.url == f.url
                        && w.thumb == f.thumb =>
                {
                    tx.execute(
                        "UPDATE ticket_files SET position = $2 WHERE id = $1",
                        &[&w.id, &(pos as i32)],
                    )
                    .await?;
                }
                Some(w) => {
                    changed = true;
                    tx.execute(
                        "UPDATE ticket_files SET name = $2, media_type = $3, size = $4, url = $5, \
                         thumb = $6, position = $7, bytes = NULL, thumb_bytes = NULL, cached_at = NULL \
                         WHERE id = $1",
                        &[&w.id, &f.name, &f.media_type, &size, &f.url, &f.thumb, &(pos as i32)],
                    )
                    .await?;
                }
                None => {
                    changed = true;
                    tx.execute(
                        "INSERT INTO ticket_files (id, ticket, source, remote_id, name, media_type, \
                         size, url, thumb, position, added_by, created_at) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                        &[
                            &Uuid::new_v4(),
                            &ticket,
                            &f.source.as_str(),
                            &f.remote_id,
                            &f.name,
                            &f.media_type,
                            &size,
                            &f.url,
                            &f.thumb,
                            &(pos as i32),
                            &serde_json::to_value(by)?,
                            &now,
                        ],
                    )
                    .await?;
                }
            }
        }
        let gone: Vec<Uuid> = was
            .iter()
            .filter(|w| {
                !files
                    .iter()
                    .any(|f| f.source == w.source && Some(&f.remote_id) == w.remote_id.as_ref())
            })
            .map(|w| w.id)
            .collect();
        if !gone.is_empty() {
            changed = true;
            tx.execute("DELETE FROM ticket_files WHERE id = ANY($1)", &[&gone])
                .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Adds an upload to `ticket`, its bytes with it, if the ticket holds
    /// fewer than [`UPLOADS_PER_TICKET`]; `None` when it is full.
    pub async fn add_upload(
        &self,
        ticket: Uuid,
        name: &str,
        media_type: &str,
        bytes: &[u8],
        by: &Party,
    ) -> Result<Option<TicketFile>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // One upload at a time per ticket, so the count holds.
        tx.execute("SELECT 1 FROM tickets WHERE id = $1 FOR UPDATE", &[&ticket])
            .await?;
        let held: i64 = tx
            .query_one(
                "SELECT count(*) FROM ticket_files WHERE ticket = $1 AND source = 'upload'",
                &[&ticket],
            )
            .await?
            .try_get(0)?;
        if held as usize >= UPLOADS_PER_TICKET {
            return Ok(None);
        }
        let f = TicketFile {
            id: Uuid::new_v4(),
            source: FileSource::Upload,
            remote_id: None,
            name: name.to_string(),
            media_type: media_type.to_string(),
            size: Some(bytes.len() as u64),
            url: None,
            thumb: false,
            added_by: by.clone(),
            added_at: Utc::now(),
        };
        tx.execute(
            "INSERT INTO ticket_files (id, ticket, source, name, media_type, size, thumb, position, \
             bytes, added_by, created_at) \
             VALUES ($1, $2, 'upload', $3, $4, $5, FALSE, 1000 + $6, $7, $8, $9)",
            &[
                &f.id,
                &ticket,
                &f.name,
                &f.media_type,
                &(bytes.len() as i64),
                &(held as i32),
                &bytes,
                &serde_json::to_value(by)?,
                &f.added_at,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(Some(f))
    }

    /// Takes the upload `id` off `ticket`. Whether there was one.
    pub async fn remove_upload(&self, ticket: Uuid, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "DELETE FROM ticket_files WHERE ticket = $1 AND id = $2 AND source = 'upload'",
                &[&ticket, &id],
            )
            .await?
            > 0)
    }

    /// The bytes kept of file `id` (or of its small picture); a remote
    /// file's are marked as read now, so they are the last to go.
    pub async fn file_bytes(&self, id: Uuid, thumb: bool) -> Result<Option<Vec<u8>>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "UPDATE ticket_files SET cached_at = CASE WHEN source = 'upload' THEN cached_at \
                 ELSE now() END WHERE id = $1 \
                 RETURNING CASE WHEN $2 THEN thumb_bytes ELSE bytes END",
                &[&id, &thumb],
            )
            .await?;
        Ok(row.and_then(|r| r.get::<_, Option<Vec<u8>>>(0)))
    }

    /// Keeps a remote file's bytes (or its small picture's), then lets go
    /// of the least lately read past [`CACHE_MAX`].
    pub async fn keep_file_bytes(&self, id: Uuid, thumb: bool, bytes: &[u8]) -> Result<()> {
        self.keep_file_bytes_within(id, thumb, bytes, CACHE_MAX)
            .await
    }

    async fn keep_file_bytes_within(
        &self,
        id: Uuid,
        thumb: bool,
        bytes: &[u8],
        cap: i64,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        let column = if thumb { "thumb_bytes" } else { "bytes" };
        client
            .execute(
                &format!(
                    "UPDATE ticket_files SET {column} = $2, cached_at = now() \
                     WHERE id = $1 AND source <> 'upload'"
                ),
                &[&id, &bytes],
            )
            .await?;
        client
            .execute(
                "WITH kept AS ( \
                   SELECT id, sum(coalesce(octet_length(bytes), 0) + coalesce(octet_length(thumb_bytes), 0)) \
                     OVER (ORDER BY cached_at DESC, id) AS running \
                   FROM ticket_files WHERE source <> 'upload' AND cached_at IS NOT NULL) \
                 UPDATE ticket_files f SET bytes = NULL, thumb_bytes = NULL, cached_at = NULL \
                 FROM kept WHERE f.id = kept.id AND kept.running > $1",
                &[&cap],
            )
            .await?;
        Ok(())
    }

    /// Forgets every remote file of the tickets of `board` (unlinked): the
    /// issue's files are no longer read.
    pub async fn forget_remote_files(&self, board: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM ticket_files WHERE source <> 'upload' \
                 AND ticket IN (SELECT id FROM tickets WHERE board = $1)",
                &[&board],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "ticket_files_tests.rs"]
mod tests;
