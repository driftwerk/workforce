use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eyre::{eyre, Result};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};

use super::types::{
    ArtifactBody, ArtifactDescriptor, ArtifactHandle, ArtifactId, ContentType, MaybeArtifact,
};

/// Default inline-vs-stored threshold (bytes). Tools should accept an
/// override but most call sites use the default.
pub const DEFAULT_INLINE_THRESHOLD: usize = 8_000;

/// Default TTL applied when an `ArtifactDescriptor` doesn't override.
/// One hour gives the agent plenty of time to consume a handle within
/// the same conversation while keeping the disk footprint bounded.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// TTL for artifacts a user is likely to reference much later in the same
/// conversation — uploaded files and files re-fetched via `view_attachment`.
/// A long thread can outlive the 1h default many times over; the re-attach
/// loop the user hit (`data.query` reporting a staged upload "not found or
/// expired" hours later) is mostly this. 24h keeps the disk footprint bounded
/// while covering essentially every interactive session.
pub const ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 3600);

/// Backend-agnostic content-addressed store for large tool outputs. The
/// runtime injects one shared `Arc<dyn ArtifactStore>` so any producer
/// tool can offload a payload and any consumer tool can fetch it by id
/// — neither the agent nor the LLM ever holds the bytes in context.
pub trait ArtifactStore: Send + Sync {
    fn write(
        &self,
        agent_id: &str,
        descriptor: ArtifactDescriptor,
        body: ArtifactBody,
    ) -> Result<ArtifactHandle>;

    fn read(&self, agent_id: &str, id: &ArtifactId) -> Result<ArtifactBody>;

    fn metadata(&self, agent_id: &str, id: &ArtifactId) -> Result<Option<ArtifactDescriptor>>;

    /// Register a file already written to disk as an artifact. The default
    /// reads the file and delegates to [`write`]; [`FsArtifactStore`] overrides
    /// it with a rename so a streamed multi-GB export never buffers in memory.
    /// `descriptor.mime` selects how the bytes are interpreted on later reads.
    fn write_from_path(
        &self,
        agent_id: &str,
        descriptor: ArtifactDescriptor,
        source: &Path,
    ) -> Result<ArtifactHandle> {
        let bytes = fs::read(source)?;
        let body = body_from_bytes(descriptor.mime, bytes)?;
        self.write(agent_id, descriptor, body)
    }

    /// A directory on the same filesystem as the store where a producer may
    /// write a temp file it will then hand to [`write_from_path`], so the move
    /// is a rename rather than a cross-filesystem copy. `None` if the store
    /// can't expose one (callers fall back to the OS temp dir).
    fn staging_dir(&self, _agent_id: &str) -> Option<PathBuf> {
        None
    }

    /// Write the artifact body to `dest` on disk. The default materialises
    /// through [`read`]; [`FsArtifactStore`] overrides it with a file copy so
    /// a multi-GB video never transits memory as one `Vec<u8>`.
    fn read_to_file(&self, agent_id: &str, id: &ArtifactId, dest: &Path) -> Result<u64> {
        let bytes = self.read(agent_id, id)?.into_bytes()?;
        fs::write(dest, &bytes)?;
        Ok(bytes.len() as u64)
    }

    /// Absolute path of the artifact body when the store keeps bodies as
    /// plain files ([`FsArtifactStore`]). `None` for stores that must
    /// materialise bytes — callers fall back to [`read`]. Lets serving
    /// routes stream a large body from disk instead of buffering it whole.
    fn body_file_path(&self, _agent_id: &str, _id: &ArtifactId) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Best-effort immediate removal of an artifact (body + metadata) —
    /// e.g. a multi-file upload rolled back after one file trips the size
    /// cap. Default no-op: TTL-based purging covers stores without cheap
    /// deletes (S3 lifecycle rules own GC there).
    fn delete(&self, _agent_id: &str, _id: &ArtifactId) -> Result<()> {
        Ok(())
    }

    /// Walk every agent directory, delete expired body+meta pairs, and
    /// return the count purged. Idempotent; safe to run on a timer.
    fn purge_expired(&self) -> Result<usize>;

    /// Short-lived presigned GET URL for a stateless redirect, or `None` for
    /// backends that must stream bytes directly (Fs/in-memory). Default `None`.
    fn presigned_get(
        &self,
        _agent_id: &str,
        _id: &ArtifactId,
        _expiry: std::time::Duration,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Sidecar {
    pub(crate) descriptor: ArtifactDescriptor,
    pub(crate) created_at_secs: u64,
}

impl Sidecar {
    pub(crate) fn expired(&self, now: SystemTime) -> bool {
        let created = UNIX_EPOCH + Duration::from_secs(self.created_at_secs);
        match now.duration_since(created) {
            Ok(age) => age >= self.descriptor.ttl(),
            Err(_) => false,
        }
    }
}

/// Filesystem-backed `ArtifactStore`. Layout:
/// `{root}/{agent_id}/{art_id}.body` + `{root}/{agent_id}/{art_id}.meta.json`.
/// Writes are atomic via `tempfile::NamedTempFile::persist` so a reader
/// concurrent with a writer sees either the full pair or nothing.
pub struct FsArtifactStore {
    root: PathBuf,
}

impl FsArtifactStore {
    pub fn new(root: PathBuf) -> Result<Arc<Self>> {
        workforce_common::private_fs::ensure_private_dir(&root)?;
        Ok(Arc::new(Self { root }))
    }

    fn agent_dir(&self, agent_id: &str) -> Result<PathBuf> {
        if agent_id.is_empty()
            || agent_id.contains('/')
            || agent_id.contains('\\')
            || agent_id.contains("..")
        {
            return Err(eyre!("invalid agent_id for artifact path: {agent_id}"));
        }
        Ok(self.root.join(agent_id))
    }

    fn body_path(&self, agent_id: &str, id: &ArtifactId, mime: ContentType) -> Result<PathBuf> {
        let dir = self.agent_dir(agent_id)?;
        Ok(dir.join(format!("{}.{}", id.as_str(), mime.extension())))
    }

    fn meta_path(&self, agent_id: &str, id: &ArtifactId) -> Result<PathBuf> {
        let dir = self.agent_dir(agent_id)?;
        Ok(dir.join(format!("{}.meta.json", id.as_str())))
    }

    fn load_sidecar(&self, agent_id: &str, id: &ArtifactId) -> Result<Option<Sidecar>> {
        let path = self.meta_path(agent_id, id)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reconstruct a typed [`ArtifactBody`] from stored bytes and the descriptor's
/// coarse mime. The single decode path shared by every store backend's read.
pub(crate) fn body_from_bytes(mime: ContentType, bytes: Vec<u8>) -> Result<ArtifactBody> {
    Ok(match mime {
        ContentType::ApplicationJson => ArtifactBody::Json(serde_json::from_slice(&bytes)?),
        ContentType::TextCsv => ArtifactBody::Csv(String::from_utf8(bytes)?),
        ContentType::OctetStream => ArtifactBody::Bytes(bytes),
    })
}

/// Stamp the owning user from the ambient identity when the producer left it
/// unset. Owner is enforced on model-driven reads (`resolve::read_session_scoped`);
/// an owner-less artifact would bypass that check on a shared agent.
pub(crate) fn stamp_user(descriptor: &mut ArtifactDescriptor) {
    if descriptor.user_id.is_none() {
        descriptor.user_id = crate::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty());
    }
}

fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.as_file_mut().write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(dest).map_err(|e| eyre!(e.error))?;
    Ok(())
}

pub(crate) fn render_body(body: &ArtifactBody) -> Result<Vec<u8>> {
    Ok(match body {
        ArtifactBody::Json(v) => serde_json::to_vec(v)?,
        ArtifactBody::Csv(s) => s.as_bytes().to_vec(),
        ArtifactBody::Bytes(b) => b.clone(),
    })
}

/// Rows shown in a staged table's preview, and the byte budget they share.
/// The budget is what makes staging worthwhile: the envelope has to be markedly
/// smaller than the data it replaces, or spilling is pointless.
const PREVIEW_MAX_ROWS: usize = 5;
const PREVIEW_MAX_ROW_BYTES: usize = 2_048;

pub(crate) fn build_preview(
    body: &ArtifactBody,
    descriptor: &ArtifactDescriptor,
) -> serde_json::Value {
    match body {
        ArtifactBody::Json(serde_json::Value::Array(rows)) => {
            // Bounded by BYTES as well as row count. Five rows of a wide record
            // can be the whole payload — a result of exactly five fat rows had
            // its entire content reproduced in the "preview", so staging it saved
            // nothing and the spill was abandoned, leaving the rows unqueryable.
            // Always keep at least one row so the shape is still legible.
            let mut preview: Vec<&serde_json::Value> = Vec::new();
            let mut budget = PREVIEW_MAX_ROW_BYTES;
            for row in rows.iter().take(PREVIEW_MAX_ROWS) {
                let cost = row.to_string().len();
                if !preview.is_empty() && cost > budget {
                    break;
                }
                budget = budget.saturating_sub(cost);
                preview.push(row);
            }
            let truncated = rows.len() > preview.len();
            serde_json::json!({
                "kind": "rows",
                "rows": preview,
                "truncated": truncated,
            })
        }
        ArtifactBody::Json(other) => serde_json::json!({
            "kind": "json",
            "snippet": preview_string(&other.to_string(), 256),
        }),
        ArtifactBody::Csv(s) => serde_json::json!({
            "kind": "csv",
            "snippet": preview_string(s, 512),
        }),
        ArtifactBody::Bytes(b) => serde_json::json!({
            "kind": "bytes",
            "size": b.len(),
            "mime": descriptor.mime.as_str(),
        }),
    }
}

fn preview_string(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let cut: String = input.chars().take(max_chars).collect();
    format!("{cut}…")
}

impl ArtifactStore for FsArtifactStore {
    fn write(
        &self,
        agent_id: &str,
        mut descriptor: ArtifactDescriptor,
        body: ArtifactBody,
    ) -> Result<ArtifactHandle> {
        if descriptor.mime != body.content_type() {
            return Err(eyre!(
                "descriptor mime {} does not match body content type {}",
                descriptor.mime,
                body.content_type()
            ));
        }
        if descriptor.agent_id != agent_id {
            return Err(eyre!(
                "descriptor agent_id `{}` does not match write agent `{}`",
                descriptor.agent_id,
                agent_id
            ));
        }

        stamp_user(&mut descriptor);

        let id = ArtifactId::fresh();
        let dir = self.agent_dir(agent_id)?;
        let body_dest = self.body_path(agent_id, &id, descriptor.mime)?;
        let meta_dest = self.meta_path(agent_id, &id)?;

        let preview = build_preview(&body, &descriptor);
        let body_bytes = render_body(&body)?;
        let sidecar = Sidecar {
            descriptor: descriptor.clone(),
            created_at_secs: now_secs(),
        };
        let meta_bytes = serde_json::to_vec(&sidecar)?;

        write_atomic(&dir, &body_dest, &body_bytes)?;
        if let Err(e) = write_atomic(&dir, &meta_dest, &meta_bytes) {
            let _ = fs::remove_file(&body_dest);
            return Err(e);
        }

        Ok(ArtifactHandle {
            id,
            descriptor,
            preview,
        })
    }

    fn write_from_path(
        &self,
        agent_id: &str,
        mut descriptor: ArtifactDescriptor,
        source: &Path,
    ) -> Result<ArtifactHandle> {
        if descriptor.agent_id != agent_id {
            return Err(eyre!(
                "descriptor agent_id `{}` does not match write agent `{}`",
                descriptor.agent_id,
                agent_id
            ));
        }
        stamp_user(&mut descriptor);

        let id = ArtifactId::fresh();
        let dir = self.agent_dir(agent_id)?;
        fs::create_dir_all(&dir)?;
        let body_dest = self.body_path(agent_id, &id, descriptor.mime)?;
        let size = fs::metadata(source)?.len();
        // Move into the store: a rename when on the same filesystem (the common
        // case — the exporter writes its temp file under the artifact root),
        // falling back to copy+remove across filesystems.
        if fs::rename(source, &body_dest).is_err() {
            fs::copy(source, &body_dest)?;
            let _ = fs::remove_file(source);
        }
        let preview = serde_json::json!({
            "kind": "bytes",
            "size": size,
            "mime": descriptor.mime.as_str(),
        });
        let sidecar = Sidecar {
            descriptor: descriptor.clone(),
            created_at_secs: now_secs(),
        };
        let meta_bytes = serde_json::to_vec(&sidecar)?;
        if let Err(e) = write_atomic(&dir, &self.meta_path(agent_id, &id)?, &meta_bytes) {
            let _ = fs::remove_file(&body_dest);
            return Err(e);
        }
        Ok(ArtifactHandle {
            id,
            descriptor,
            preview,
        })
    }

    fn read(&self, agent_id: &str, id: &ArtifactId) -> Result<ArtifactBody> {
        let Some(sidecar) = self.load_sidecar(agent_id, id)? else {
            return Err(eyre!("artifact {id} not found or expired"));
        };
        if sidecar.expired(SystemTime::now()) {
            return Err(eyre!("artifact {id} not found or expired"));
        }
        let path = self.body_path(agent_id, id, sidecar.descriptor.mime)?;
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(eyre!("artifact {id} not found or expired"));
            }
            Err(e) => return Err(e.into()),
        };
        body_from_bytes(sidecar.descriptor.mime, bytes)
    }

    fn read_to_file(&self, agent_id: &str, id: &ArtifactId, dest: &Path) -> Result<u64> {
        let Some(sidecar) = self.load_sidecar(agent_id, id)? else {
            return Err(eyre!("artifact {id} not found or expired"));
        };
        if sidecar.expired(SystemTime::now()) {
            return Err(eyre!("artifact {id} not found or expired"));
        }
        let path = self.body_path(agent_id, id, sidecar.descriptor.mime)?;
        match fs::copy(&path, dest) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(eyre!("artifact {id} not found or expired"))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn body_file_path(&self, agent_id: &str, id: &ArtifactId) -> Result<Option<PathBuf>> {
        let Some(sidecar) = self.load_sidecar(agent_id, id)? else {
            return Ok(None);
        };
        if sidecar.expired(SystemTime::now()) {
            return Ok(None);
        }
        let path = self.body_path(agent_id, id, sidecar.descriptor.mime)?;
        Ok(path.exists().then_some(path))
    }

    fn metadata(&self, agent_id: &str, id: &ArtifactId) -> Result<Option<ArtifactDescriptor>> {
        Ok(self
            .load_sidecar(agent_id, id)?
            .filter(|s| !s.expired(SystemTime::now()))
            .map(|s| s.descriptor))
    }

    fn staging_dir(&self, agent_id: &str) -> Option<PathBuf> {
        let dir = self.agent_dir(agent_id).ok()?;
        fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    fn delete(&self, agent_id: &str, id: &ArtifactId) -> Result<()> {
        if let Some(sidecar) = self.load_sidecar(agent_id, id)? {
            let _ = fs::remove_file(self.body_path(agent_id, id, sidecar.descriptor.mime)?);
        }
        let _ = fs::remove_file(self.meta_path(agent_id, id)?);
        Ok(())
    }

    fn purge_expired(&self) -> Result<usize> {
        let now = SystemTime::now();
        for agent_dir in self.agent_dirs()? {
            // Spool files (`.upload-*.tmp` / `.slack-dl-*.tmp` / `.byref-*.tmp`)
            // normally remove themselves via drop guards; a crash mid-stream
            // strands them, and nothing else would ever clean a 1 GiB orphan.
            for entry in fs::read_dir(&agent_dir)?.flatten() {
                let path = entry.path();
                let stale = path.extension().and_then(|s| s.to_str()) == Some("tmp")
                    && entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| now.duration_since(t).ok())
                        .is_some_and(|age| age > ATTACHMENT_TTL);
                if stale {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        Ok(self
            .stored()?
            .into_iter()
            .filter(|stored| stored.sidecar.expired(now))
            .filter(|stored| self.remove(stored).is_ok())
            .count())
    }
}

/// One artifact on disk: the agent directory it sits in, its id and its sidecar.
struct Stored {
    agent_id: String,
    id: ArtifactId,
    sidecar: Sidecar,
}

impl FsArtifactStore {
    /// Delete every artifact staged for `user_id`, in any agent's directory.
    /// Returns how many went; a file that cannot be removed fails the call, so
    /// an erasure that left one behind is never reported as done.
    pub fn purge_user(&self, user_id: &str) -> Result<usize> {
        let theirs: Vec<Stored> = self
            .stored()?
            .into_iter()
            .filter(|stored| stored.sidecar.descriptor.user_id.as_deref() == Some(user_id))
            .collect();
        theirs.iter().try_for_each(|stored| self.remove(stored))?;
        Ok(theirs.len())
    }

    fn agent_dirs(&self) -> Result<Vec<PathBuf>> {
        match fs::read_dir(&self.root) {
            Ok(entries) => Ok(entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
                .map(|entry| entry.path())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every artifact whose sidecar reads back; one that does not is skipped.
    fn stored(&self) -> Result<Vec<Stored>> {
        let mut stored = Vec::new();
        for agent_dir in self.agent_dirs()? {
            let Some(agent_id) = agent_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            for entry in fs::read_dir(&agent_dir)?.flatten() {
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".meta.json"))
                    .and_then(|stem| ArtifactId::parse(stem).ok())
                else {
                    continue;
                };
                if let Ok(Some(sidecar)) = self.load_sidecar(agent_id, &id) {
                    stored.push(Stored {
                        agent_id: agent_id.to_string(),
                        id,
                        sidecar,
                    });
                }
            }
        }
        Ok(stored)
    }

    /// Remove an artifact's body, then its sidecar.
    fn remove(&self, stored: &Stored) -> Result<()> {
        let body = self.body_path(&stored.agent_id, &stored.id, stored.sidecar.descriptor.mime)?;
        [body, self.meta_path(&stored.agent_id, &stored.id)?]
            .iter()
            .try_for_each(|path| match fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            })?;
        Ok(())
    }
}

/// Producer-side gate: serialise the body, compare against the inline
/// threshold, store-or-pass-through. The single decision point used by
/// every tool that may emit a large payload.
pub fn maybe_store(
    store: &dyn ArtifactStore,
    agent_id: &str,
    descriptor: ArtifactDescriptor,
    body: ArtifactBody,
    inline_threshold_bytes: usize,
) -> Result<MaybeArtifact> {
    if body.serialized_size() <= inline_threshold_bytes {
        return Ok(MaybeArtifact::Inline(body));
    }
    let handle = store.write(agent_id, descriptor, body)?;
    Ok(MaybeArtifact::Stored(handle))
}

/// Hub-global handle to the configured `ArtifactStore`. Single-set via
/// [`set_global_store`] at hub startup so every tool (across crates)
/// can reach the same store without threading it through each skill
/// constructor.
static GLOBAL_STORE: OnceCell<Arc<dyn ArtifactStore>> = OnceCell::new();

/// Install the hub-wide `ArtifactStore`. Subsequent calls are no-ops —
/// the store is constructed once at startup. Returns `Ok(())` on the
/// first call and on every later call (the runtime keeps the original
/// reference; we don't want to swap stores under tools).
pub fn set_global_store(store: Arc<dyn ArtifactStore>) {
    let _ = GLOBAL_STORE.set(store);
}

/// The `ArtifactStore` for the current work: the one the turn scoped
/// ([`crate::runner::CURRENT_ARTIFACT_STORE`]) when its owner keeps stores
/// apart, else the hub-wide one. Returns `None` if neither is set (tests,
/// CLI tools that don't construct a `HubState`). Tools that get `None` fall
/// back to inline-only behaviour — same shape they emit today.
pub fn global_store() -> Option<Arc<dyn ArtifactStore>> {
    crate::runner::CURRENT_ARTIFACT_STORE
        .try_with(Arc::clone)
        .ok()
        .or_else(|| GLOBAL_STORE.get().cloned())
}

/// Install (or reuse) the process-global store backed by a leaked TempDir.
/// Tests namespace writes by `agent_id` to stay isolated under it.
#[cfg(test)]
pub(crate) fn install_test_global_store() -> Arc<dyn ArtifactStore> {
    use std::sync::LazyLock;
    static DIR: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
        let dir = tempfile::TempDir::new().expect("tempdir");
        set_global_store(FsArtifactStore::new(dir.path().to_path_buf()).expect("artifact store"));
        dir
    });
    LazyLock::force(&DIR);
    global_store().expect("global store installed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn the_store_root_is_closed_to_the_hub() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("artifacts");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o2775)).unwrap();

        FsArtifactStore::new(root.clone()).unwrap();

        assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    }

    fn json_descriptor(agent: &str, ttl: Duration) -> ArtifactDescriptor {
        ArtifactDescriptor {
            producer: "test.producer".into(),
            mime: ContentType::ApplicationJson,
            schema: Some(vec!["a".into(), "b".into()]),
            row_count: Some(2),
            ttl_seconds: ttl.as_secs(),
            agent_id: agent.into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        }
    }

    fn csv_descriptor(agent: &str) -> ArtifactDescriptor {
        ArtifactDescriptor {
            producer: "test.producer".into(),
            mime: ContentType::TextCsv,
            schema: None,
            row_count: None,
            ttl_seconds: DEFAULT_TTL.as_secs(),
            agent_id: agent.into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        }
    }

    fn store() -> (Arc<FsArtifactStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    #[test]
    fn write_then_read_roundtrips_json() {
        let (store, _g) = store();
        let body = ArtifactBody::Json(serde_json::json!([{"a": 1}, {"a": 2}]));
        let handle = store
            .write("agent-a", json_descriptor("agent-a", DEFAULT_TTL), body)
            .unwrap();
        let loaded = store.read("agent-a", &handle.id).unwrap();
        match loaded {
            ArtifactBody::Json(v) => {
                assert_eq!(v, serde_json::json!([{"a": 1}, {"a": 2}]));
            }
            other => panic!("expected json, got {:?}", other),
        }
    }

    #[test]
    fn write_then_read_roundtrips_csv() {
        let (store, _g) = store();
        let body = ArtifactBody::Csv("a,b\n1,2\n".into());
        let handle = store
            .write("agent-a", csv_descriptor("agent-a"), body)
            .unwrap();
        match store.read("agent-a", &handle.id).unwrap() {
            ArtifactBody::Csv(s) => assert_eq!(s, "a,b\n1,2\n"),
            other => panic!("expected csv, got {:?}", other),
        }
    }

    #[test]
    fn write_then_read_roundtrips_bytes() {
        let (store, _g) = store();
        let descriptor = ArtifactDescriptor {
            producer: "test.bytes".into(),
            mime: ContentType::OctetStream,
            schema: None,
            row_count: None,
            ttl_seconds: DEFAULT_TTL.as_secs(),
            agent_id: "agent-a".into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        };
        let body = ArtifactBody::Bytes(vec![1, 2, 3, 4]);
        let handle = store.write("agent-a", descriptor, body).unwrap();
        match store.read("agent-a", &handle.id).unwrap() {
            ArtifactBody::Bytes(b) => assert_eq!(b, vec![1, 2, 3, 4]),
            other => panic!("expected bytes, got {:?}", other),
        }
    }

    #[test]
    fn write_from_path_moves_file_and_roundtrips() {
        let (store, _g) = store();
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("export.csv");
        fs::write(&src, b"a,b\n1,2\n").unwrap();
        let descriptor = ArtifactDescriptor {
            producer: "database.export".into(),
            mime: ContentType::OctetStream,
            schema: None,
            row_count: Some(1),
            ttl_seconds: ATTACHMENT_TTL.as_secs(),
            agent_id: "agent-a".into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        };
        let handle = store.write_from_path("agent-a", descriptor, &src).unwrap();
        assert!(
            !src.exists(),
            "source file should have been moved into the store"
        );
        assert_eq!(handle.descriptor.row_count, Some(1));
        match store.read("agent-a", &handle.id).unwrap() {
            ArtifactBody::Bytes(b) => assert_eq!(b, b"a,b\n1,2\n"),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[test]
    fn write_from_path_rejects_agent_mismatch() {
        let (store, _g) = store();
        let src_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("x.bin");
        fs::write(&src, b"x").unwrap();
        let descriptor = ArtifactDescriptor {
            producer: "database.export".into(),
            mime: ContentType::OctetStream,
            schema: None,
            row_count: None,
            ttl_seconds: DEFAULT_TTL.as_secs(),
            agent_id: "agent-a".into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        };
        let err = store
            .write_from_path("agent-b", descriptor, &src)
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent_id"));
    }

    #[test]
    fn read_unknown_id_returns_error() {
        let (store, _g) = store();
        let fresh = ArtifactId::fresh();
        let err = store.read("agent-a", &fresh).unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[test]
    fn read_expired_id_returns_error_after_ttl() {
        let (store, _g) = store();
        let body = ArtifactBody::Json(serde_json::json!([1]));
        let handle = store
            .write(
                "agent-a",
                json_descriptor("agent-a", Duration::from_secs(0)),
                body,
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let err = store.read("agent-a", &handle.id).unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[test]
    fn purge_expired_removes_only_expired_files() {
        let (store, _g) = store();
        let fresh = store
            .write(
                "agent-a",
                json_descriptor("agent-a", DEFAULT_TTL),
                ArtifactBody::Json(serde_json::json!([1])),
            )
            .unwrap();
        let stale = store
            .write(
                "agent-a",
                json_descriptor("agent-a", Duration::from_secs(0)),
                ArtifactBody::Json(serde_json::json!([2])),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let n = store.purge_expired().unwrap();
        assert_eq!(n, 1);
        assert!(store.read("agent-a", &fresh.id).is_ok());
        assert!(store.read("agent-a", &stale.id).is_err());
    }

    #[test]
    fn a_users_artifacts_are_purged_in_every_agent_and_no_one_elses() {
        let (store, _g) = store();
        let write = |agent: &str, user: Option<&str>| {
            store
                .write(
                    agent,
                    ArtifactDescriptor {
                        user_id: user.map(str::to_string),
                        ..json_descriptor(agent, DEFAULT_TTL)
                    },
                    ArtifactBody::Json(serde_json::json!([1])),
                )
                .unwrap()
                .id
        };
        let theirs = [
            write("agent-a", Some("sub-1")),
            write("agent-b", Some("sub-1")),
        ];
        let neighbour = write("agent-a", Some("sub-10"));
        let unowned = write("agent-b", None);

        assert_eq!(store.purge_user("sub-1").unwrap(), 2);
        assert!(store.read("agent-a", &theirs[0]).is_err());
        assert!(store.read("agent-b", &theirs[1]).is_err());
        assert!(store.read("agent-a", &neighbour).is_ok());
        assert!(store.read("agent-b", &unowned).is_ok());
        assert_eq!(store.purge_user("sub-1").unwrap(), 0);
    }

    #[test]
    fn cross_agent_read_rejected() {
        let (store, _g) = store();
        let handle = store
            .write(
                "agent-a",
                json_descriptor("agent-a", DEFAULT_TTL),
                ArtifactBody::Json(serde_json::json!([1])),
            )
            .unwrap();
        let err = store.read("agent-b", &handle.id).unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[test]
    fn descriptor_agent_mismatch_rejected() {
        let (store, _g) = store();
        let descriptor = json_descriptor("agent-a", DEFAULT_TTL);
        let err = store
            .write(
                "agent-b",
                descriptor,
                ArtifactBody::Json(serde_json::json!([1])),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent_id"));
    }

    #[test]
    fn descriptor_mime_mismatch_rejected() {
        let (store, _g) = store();
        let mut descriptor = json_descriptor("agent-a", DEFAULT_TTL);
        descriptor.mime = ContentType::TextCsv;
        let err = store
            .write(
                "agent-a",
                descriptor,
                ArtifactBody::Json(serde_json::json!([1])),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("mime"));
    }

    #[test]
    fn concurrent_writes_get_unique_ids() {
        let (store, _g) = store();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..16 {
            let handle = store
                .write(
                    "agent-a",
                    json_descriptor("agent-a", DEFAULT_TTL),
                    ArtifactBody::Json(serde_json::json!([1])),
                )
                .unwrap();
            assert!(ids.insert(handle.id), "duplicate id generated");
        }
    }

    #[test]
    fn metadata_returns_none_for_unknown() {
        let (store, _g) = store();
        let id = ArtifactId::fresh();
        assert!(store.metadata("agent-a", &id).unwrap().is_none());
    }

    #[test]
    fn metadata_returns_none_after_expiry() {
        let (store, _g) = store();
        let handle = store
            .write(
                "agent-a",
                json_descriptor("agent-a", Duration::from_secs(0)),
                ArtifactBody::Json(serde_json::json!([1])),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(store.metadata("agent-a", &handle.id).unwrap().is_none());
    }

    #[test]
    fn maybe_store_inline_under_threshold() {
        let (store, _g) = store();
        let descriptor = json_descriptor("agent-a", DEFAULT_TTL);
        let body = ArtifactBody::Json(serde_json::json!({"x": 1}));
        match maybe_store(&*store, "agent-a", descriptor, body, 1024).unwrap() {
            MaybeArtifact::Inline(_) => {}
            MaybeArtifact::Stored(_) => panic!("expected inline"),
        }
    }

    #[test]
    fn maybe_store_stored_over_threshold() {
        let (store, _g) = store();
        let descriptor = json_descriptor("agent-a", DEFAULT_TTL);
        let body = ArtifactBody::Bytes(vec![0u8; 4096]);
        let descriptor = ArtifactDescriptor {
            mime: ContentType::OctetStream,
            ..descriptor
        };
        match maybe_store(&*store, "agent-a", descriptor, body, 1024).unwrap() {
            MaybeArtifact::Stored(handle) => {
                assert!(store.read("agent-a", &handle.id).is_ok());
            }
            MaybeArtifact::Inline(_) => panic!("expected stored"),
        }
    }

    #[test]
    fn agent_id_with_separator_rejected() {
        let (store, _g) = store();
        let descriptor = json_descriptor("..", DEFAULT_TTL);
        let err = store
            .write("..", descriptor, ArtifactBody::Json(serde_json::json!([])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent_id"));
    }
}

#[cfg(test)]
mod scoped_store_tests {
    use super::*;
    use crate::runner::{SessionContext, CURRENT_ARTIFACT_STORE};

    /// A turn that scopes its own store stages into it, in its own task and
    /// in the tool tasks it spawns; everything else keeps the process-wide
    /// store.
    #[tokio::test]
    async fn a_turn_scoped_store_wins_over_the_process_wide_one() {
        let global = install_test_global_store();
        let dir = tempfile::TempDir::new().expect("tempdir");
        let own: Arc<dyn ArtifactStore> =
            FsArtifactStore::new(dir.path().to_path_buf()).expect("artifact store");

        let seen = CURRENT_ARTIFACT_STORE
            .scope(Arc::clone(&own), async { global_store().expect("a store") })
            .await;
        assert!(Arc::ptr_eq(&seen, &own), "inside the scope, the turn's own");

        let carried = CURRENT_ARTIFACT_STORE
            .scope(Arc::clone(&own), async {
                SessionContext::capture()
                    .spawn(async { global_store().expect("a store") })
                    .await
                    .expect("spawned")
            })
            .await;
        assert!(Arc::ptr_eq(&carried, &own), "carried across a spawn");

        let outside = global_store().expect("the process-wide store");
        assert!(
            Arc::ptr_eq(&outside, &global),
            "outside, the process-wide one"
        );
    }
}
