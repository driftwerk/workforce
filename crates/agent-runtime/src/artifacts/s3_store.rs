//! S3-backed [`ArtifactStore`] for active-active hub deployments.
//!
//! Layout mirrors [`FsArtifactStore`](super::store::FsArtifactStore): two
//! objects per artifact — the body at `{prefix}{agent_id}/{art_id}` and the
//! descriptor sidecar (the same JSON Fs writes to `*.meta.json`) at a companion
//! key `{body_key}.meta.json`. The sidecar is a separate object rather than the
//! body's user-metadata so a wide-schema descriptor never trips the ~2KB S3
//! user-metadata limit (which would fail `PutObject` for exactly the tabular
//! exports that flow through the store). TTL is enforced at read by
//! [`Sidecar::expired`]; S3 lifecycle rules own GC, so `purge_expired` is a
//! no-op.
//!
//! The [`ArtifactStore`] trait is synchronous; `aws-sdk-s3` is async-only. Each
//! trait method bridges with `block_in_place` + `Handle::block_on`, delegating
//! to a private `async fn *_async`. This requires the multi-thread runtime (the
//! hub uses one).

use std::path::Path;
use std::time::{Duration, SystemTime};

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use eyre::{eyre, Result};

use super::store::{body_from_bytes, build_preview, now_secs, render_body, stamp_user, Sidecar};
use super::types::{ArtifactBody, ArtifactDescriptor, ArtifactHandle, ArtifactId};

/// Suffix appended to a body key to form its companion sidecar key, mirroring
/// the Fs store's `*.meta.json`.
const SIDECAR_SUFFIX: &str = ".meta.json";

/// S3-backed content-addressed artifact store. The hub assembles the
/// `aws_sdk_s3::Client` (region + writer identity) at boot and injects it here;
/// this type owns no AWS configuration.
pub struct S3ArtifactStore {
    client: Client,
    bucket: String,
    prefix: String,
}

/// Reject an `agent_id` that could escape the key namespace, mirroring
/// [`FsArtifactStore::agent_dir`](super::store::FsArtifactStore). The `art_` id
/// shape is already guaranteed by the [`ArtifactId`] type.
fn validate_agent_id(agent_id: &str) -> Result<()> {
    if agent_id.is_empty()
        || agent_id.contains('/')
        || agent_id.contains('\\')
        || agent_id.contains("..")
    {
        return Err(eyre!("invalid agent_id for artifact path: {agent_id}"));
    }
    Ok(())
}

/// Build the object key `{prefix}{agent_id}/{art_id}` after validating the
/// agent_id. The single key-construction choke point for every method.
fn object_key(prefix: &str, agent_id: &str, id: &ArtifactId) -> Result<String> {
    validate_agent_id(agent_id)?;
    Ok(format!("{prefix}{agent_id}/{}", id.as_str()))
}

/// Companion sidecar key for a body key.
fn sidecar_key(body_key: &str) -> String {
    format!("{body_key}{SIDECAR_SUFFIX}")
}

/// Serialize a sidecar to its stored bytes — the JSON the companion object
/// holds. Shared by `write` and `metadata`/`read` so encode and decode can't
/// drift.
fn sidecar_bytes(sidecar: &Sidecar) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(sidecar)?)
}

/// Decode a companion object's bytes back into a [`Sidecar`].
fn sidecar_from_bytes(bytes: &[u8]) -> Result<Sidecar> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Producer-side validation shared by `write` (mime + agent) — identical
/// rejections to [`FsArtifactStore::write`](super::store::FsArtifactStore).
fn validate_write(
    agent_id: &str,
    descriptor: &ArtifactDescriptor,
    body: &ArtifactBody,
) -> Result<()> {
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
    Ok(())
}

/// True when a `GetObject` error reports a missing object (`NoSuchKey`, or
/// `NoSuchBucket`). Maps to "absent" so `metadata` returns `None` and `read`
/// returns the same not-found error [`FsArtifactStore`](super::store::FsArtifactStore)
/// does. A missing body OR a missing sidecar means the artifact is gone.
fn is_missing<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    matches!(err.code(), Some("NoSuchKey") | Some("NoSuchBucket"))
}

impl S3ArtifactStore {
    /// Wrap a hub-built client + target bucket/prefix. This type never builds
    /// AWS configuration; the hub assembles the writer identity at boot.
    pub fn new(client: Client, bucket: String, prefix: String) -> Self {
        Self {
            client,
            bucket,
            prefix,
        }
    }

    fn now_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    async fn write_async(
        &self,
        agent_id: &str,
        mut descriptor: ArtifactDescriptor,
        body: ArtifactBody,
    ) -> Result<ArtifactHandle> {
        validate_write(agent_id, &descriptor, &body)?;
        stamp_user(&mut descriptor);
        let id = ArtifactId::fresh();
        let key = object_key(&self.prefix, agent_id, &id)?;
        let preview = build_preview(&body, &descriptor);
        let bytes = render_body(&body)?;
        let sidecar = Sidecar {
            descriptor: descriptor.clone(),
            created_at_secs: now_secs(),
        };
        self.put_keyed(&key, &sidecar, ByteStream::from(bytes))
            .await?;
        Ok(ArtifactHandle {
            id,
            descriptor,
            preview,
        })
    }

    /// Write the body object (with its `Content-Type`) then the companion
    /// sidecar object — body first, mirroring the Fs store's write order. A
    /// failed sidecar write leaves an orphaned body, which is benign: `read`
    /// requires both objects and the S3 lifecycle rule GCs it.
    async fn put_keyed(&self, key: &str, sidecar: &Sidecar, bytes: ByteStream) -> Result<()> {
        let content_type = sidecar
            .descriptor
            .media_type
            .clone()
            .unwrap_or_else(|| sidecar.descriptor.mime.as_str().to_string());
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|e| eyre!("put_object {key} failed: {e}"))?;
        let meta_key = sidecar_key(key);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&meta_key)
            .content_type("application/json")
            .body(ByteStream::from(sidecar_bytes(sidecar)?))
            .send()
            .await
            .map_err(|e| eyre!("put_object {meta_key} failed: {e}"))?;
        Ok(())
    }

    async fn write_from_path_async(
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
        let key = object_key(&self.prefix, agent_id, &id)?;
        let size = std::fs::metadata(source)?.len();
        let body = ByteStream::from_path(source)
            .await
            .map_err(|e| eyre!("stream artifact source {source:?} failed: {e}"))?;
        let preview = serde_json::json!({
            "kind": "bytes",
            "size": size,
            "mime": descriptor.mime.as_str(),
        });
        let sidecar = Sidecar {
            descriptor: descriptor.clone(),
            created_at_secs: now_secs(),
        };
        self.put_keyed(&key, &sidecar, body).await?;
        // Consume the source after a successful upload, matching the Fs
        // backend's move semantics — both callers detach the temp-file drop
        // guard (`.keep()`) and rely on the store to remove it.
        let _ = std::fs::remove_file(source);
        Ok(ArtifactHandle {
            id,
            descriptor,
            preview,
        })
    }

    /// Fetch and decode the companion sidecar object. A missing companion key →
    /// `None` (the artifact is absent or only a body orphan remains).
    async fn load_sidecar(&self, agent_id: &str, id: &ArtifactId) -> Result<Option<Sidecar>> {
        let meta_key = sidecar_key(&object_key(&self.prefix, agent_id, id)?);
        let out = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&meta_key)
            .send()
            .await
        {
            Ok(out) => out,
            Err(e) if is_missing(&e) => return Ok(None),
            Err(e) => return Err(eyre!("get_object {meta_key} failed: {e}")),
        };
        let bytes = out
            .body
            .collect()
            .await
            .map_err(|e| eyre!("read sidecar {meta_key} failed: {e}"))?
            .into_bytes();
        Ok(Some(sidecar_from_bytes(&bytes)?))
    }

    async fn read_async(&self, agent_id: &str, id: &ArtifactId) -> Result<ArtifactBody> {
        // Sidecar first: it carries expiry + the mime, and lets an expired or
        // missing artifact short-circuit before downloading a large body.
        let sidecar = self
            .load_sidecar(agent_id, id)
            .await?
            .filter(|s| !s.expired(SystemTime::now()))
            .ok_or_else(|| eyre!("artifact {id} not found or expired"))?;
        let key = object_key(&self.prefix, agent_id, id)?;
        let out = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(out) => out,
            Err(e) if is_missing(&e) => {
                return Err(eyre!("artifact {id} not found or expired"));
            }
            Err(e) => return Err(eyre!("get_object {key} failed: {e}")),
        };
        let bytes = out
            .body
            .collect()
            .await
            .map_err(|e| eyre!("read artifact {id} body failed: {e}"))?
            .into_bytes()
            .to_vec();
        body_from_bytes(sidecar.descriptor.mime, bytes)
    }

    /// Stream the body to `dest` chunk-by-chunk — the by-ref attachment
    /// path exists so a 1 GiB video never materialises as one `Vec<u8>`,
    /// and the S3 backend must hold that promise too.
    async fn read_to_file_async(
        &self,
        agent_id: &str,
        id: &ArtifactId,
        dest: &Path,
    ) -> Result<u64> {
        use std::io::Write as _;
        self.load_sidecar(agent_id, id)
            .await?
            .filter(|s| !s.expired(SystemTime::now()))
            .ok_or_else(|| eyre!("artifact {id} not found or expired"))?;
        let key = object_key(&self.prefix, agent_id, id)?;
        let out = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(out) => out,
            Err(e) if is_missing(&e) => {
                return Err(eyre!("artifact {id} not found or expired"));
            }
            Err(e) => return Err(eyre!("get_object {key} failed: {e}")),
        };
        let mut file = std::fs::File::create(dest)?;
        let mut body = out.body;
        let mut written: u64 = 0;
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|e| eyre!("read artifact {id} body failed: {e}"))?
        {
            written += chunk.len() as u64;
            file.write_all(&chunk)?;
        }
        file.flush()?;
        Ok(written)
    }

    async fn metadata_async(
        &self,
        agent_id: &str,
        id: &ArtifactId,
    ) -> Result<Option<ArtifactDescriptor>> {
        Ok(self
            .load_sidecar(agent_id, id)
            .await?
            .filter(|s| !s.expired(SystemTime::now()))
            .map(|s| s.descriptor))
    }

    /// Presigned `GET` URL for the artifact, overriding `Content-Type` /
    /// `Content-Disposition` from the persisted descriptor so a redirected
    /// download arrives with the right filename and MIME. The async core
    /// bridged onto the trait by `ArtifactStore::presigned_get`.
    pub async fn presigned_get_async(
        &self,
        agent_id: &str,
        id: &ArtifactId,
        expiry: Duration,
    ) -> Result<String> {
        let key = object_key(&self.prefix, agent_id, id)?;
        let sidecar = self
            .load_sidecar(agent_id, id)
            .await?
            .filter(|s| !s.expired(SystemTime::now()))
            .ok_or_else(|| eyre!("artifact {id} not found or expired"))?;
        let content_type = sidecar
            .descriptor
            .media_type
            .clone()
            .unwrap_or_else(|| sidecar.descriptor.mime.as_str().to_string());
        let mut req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .response_content_type(content_type);
        if let Some(name) = sidecar.descriptor.filename.as_deref() {
            req = req.response_content_disposition(
                crate::artifacts::disposition::content_disposition_attachment(name),
            );
        }
        let presigned = req
            .presigned(PresigningConfig::expires_in(expiry)?)
            .await
            .map_err(|e| eyre!("presign get_object {key} failed: {e}"))?;
        Ok(presigned.uri().to_string())
    }
}

impl super::store::ArtifactStore for S3ArtifactStore {
    fn write(
        &self,
        agent_id: &str,
        descriptor: ArtifactDescriptor,
        body: ArtifactBody,
    ) -> Result<ArtifactHandle> {
        Self::now_block_on(self.write_async(agent_id, descriptor, body))
    }

    fn read(&self, agent_id: &str, id: &ArtifactId) -> Result<ArtifactBody> {
        Self::now_block_on(self.read_async(agent_id, id))
    }

    fn read_to_file(&self, agent_id: &str, id: &ArtifactId, dest: &Path) -> Result<u64> {
        Self::now_block_on(self.read_to_file_async(agent_id, id, dest))
    }

    fn metadata(&self, agent_id: &str, id: &ArtifactId) -> Result<Option<ArtifactDescriptor>> {
        Self::now_block_on(self.metadata_async(agent_id, id))
    }

    fn write_from_path(
        &self,
        agent_id: &str,
        descriptor: ArtifactDescriptor,
        source: &Path,
    ) -> Result<ArtifactHandle> {
        Self::now_block_on(self.write_from_path_async(agent_id, descriptor, source))
    }

    fn staging_dir(&self, _agent_id: &str) -> Option<std::path::PathBuf> {
        None
    }

    /// Rewrites the sidecar with the longer life. A bucket lifecycle rule
    /// still deletes objects on its own schedule, whatever the sidecar says.
    fn retain(
        &self,
        agent_id: &str,
        id: &ArtifactId,
        ttl: Duration,
        session: Option<&str>,
    ) -> Result<bool> {
        Self::now_block_on(async {
            let now = SystemTime::now();
            let Some(sidecar) = self
                .load_sidecar(agent_id, id)
                .await?
                .filter(|s| !s.expired(now))
            else {
                return Ok(false);
            };
            if let Some(kept) = sidecar.retained(now, ttl, session) {
                let meta_key = sidecar_key(&object_key(&self.prefix, agent_id, id)?);
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&meta_key)
                    .content_type("application/json")
                    .body(ByteStream::from(sidecar_bytes(&kept)?))
                    .send()
                    .await
                    .map_err(|e| eyre!("put_object {meta_key} failed: {e}"))?;
            }
            Ok(true)
        })
    }

    /// S3 lifecycle rules own GC; TTL is enforced at read via `Sidecar::expired`.
    fn purge_expired(&self) -> Result<usize> {
        Ok(0)
    }

    fn presigned_get(
        &self,
        agent_id: &str,
        id: &ArtifactId,
        expiry: Duration,
    ) -> Result<Option<String>> {
        Self::now_block_on(self.presigned_get_async(agent_id, id, expiry)).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::ArtifactStore;
    use crate::artifacts::types::ContentType;

    fn descriptor(agent: &str, mime: ContentType) -> ArtifactDescriptor {
        ArtifactDescriptor {
            producer: "test.producer".into(),
            mime,
            schema: None,
            row_count: None,
            ttl_seconds: 3600,
            agent_id: agent.into(),
            session_id: None,
            user_id: None,
            filename: None,
            media_type: None,
        }
    }

    // --- always-on, client-free logic coverage ---

    #[test]
    fn object_key_builds_prefixed_path() {
        let id = ArtifactId::fresh();
        let key = object_key("runtime/", "agent-a", &id).unwrap();
        assert_eq!(key, format!("runtime/agent-a/{}", id.as_str()));
    }

    #[test]
    fn object_key_empty_prefix() {
        let id = ArtifactId::fresh();
        let key = object_key("", "agent-a", &id).unwrap();
        assert_eq!(key, format!("agent-a/{}", id.as_str()));
    }

    #[test]
    fn object_key_rejects_path_escaping_agent_ids() {
        let id = ArtifactId::fresh();
        for bad in &["", "..", "a/b", "a\\b", "../x", "x/.."] {
            let err = object_key("p/", bad, &id).unwrap_err().to_string();
            assert!(err.contains("agent_id"), "should reject `{bad}`: {err}");
        }
    }

    #[test]
    fn validate_write_rejects_mime_mismatch() {
        let mut d = descriptor("agent-a", ContentType::ApplicationJson);
        d.mime = ContentType::TextCsv;
        let body = ArtifactBody::Json(serde_json::json!([1]));
        let err = validate_write("agent-a", &d, &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mime"), "{err}");
    }

    #[test]
    fn validate_write_rejects_agent_mismatch() {
        let d = descriptor("agent-a", ContentType::ApplicationJson);
        let body = ArtifactBody::Json(serde_json::json!([1]));
        let err = validate_write("agent-b", &d, &body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent_id"), "{err}");
    }

    #[test]
    fn validate_write_accepts_matching() {
        let d = descriptor("agent-a", ContentType::ApplicationJson);
        let body = ArtifactBody::Json(serde_json::json!([1]));
        assert!(validate_write("agent-a", &d, &body).is_ok());
    }

    #[test]
    fn sidecar_key_appends_companion_suffix() {
        assert_eq!(sidecar_key("p/agent-a/art_x"), "p/agent-a/art_x.meta.json");
    }

    #[test]
    fn sidecar_bytes_round_trips_descriptor() {
        let sidecar = Sidecar {
            descriptor: descriptor("agent-a", ContentType::TextCsv),
            created_at_secs: now_secs(),
        };
        let decoded = sidecar_from_bytes(&sidecar_bytes(&sidecar).unwrap()).unwrap();
        assert_eq!(decoded.descriptor.agent_id, "agent-a");
        assert_eq!(decoded.descriptor.mime, ContentType::TextCsv);
        assert_eq!(decoded.created_at_secs, sidecar.created_at_secs);
    }

    #[test]
    fn sidecar_bytes_round_trips_wide_schema() {
        // Regression for the ~2KB S3 user-metadata cap that the companion-object
        // layout fixes: a wide tabular export's column list dwarfs that limit.
        // The descriptor is never the constraint — only the metadata transport
        // was — so a 500-column sidecar must round-trip intact.
        let mut d = descriptor("agent-a", ContentType::TextCsv);
        d.schema = Some((0..500).map(|i| format!("column_name_{i}")).collect());
        let sidecar = Sidecar {
            descriptor: d,
            created_at_secs: now_secs(),
        };
        let bytes = sidecar_bytes(&sidecar).unwrap();
        assert!(
            bytes.len() > 2_048,
            "wide schema should exceed the metadata cap"
        );
        let decoded = sidecar_from_bytes(&bytes).unwrap();
        assert_eq!(decoded.descriptor.schema.as_ref().unwrap().len(), 500);
    }

    #[test]
    fn staging_dir_is_none_and_purge_is_noop() {
        // Built without contacting AWS: `new` does no I/O.
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .build();
        let store = S3ArtifactStore::new(Client::from_conf(conf), "b".into(), "p/".into());
        assert!(store.staging_dir("agent-a").is_none());
        assert_eq!(store.purge_expired().unwrap(), 0);
    }

    // --- gated round-trip against localstack / real S3 ---
    //
    // Off by default (no AWS in CI). To run: point at a writable bucket and set
    //   WORKFORCE_S3_TEST_BUCKET=<bucket> [WORKFORCE_S3_TEST_REGION=<region>]
    //   [AWS_ENDPOINT_URL=http://localhost:4566 for localstack]
    // then `cargo test -p workforce-agent --features s3 s3_roundtrip -- --ignored`.

    async fn test_store() -> Option<S3ArtifactStore> {
        let bucket = std::env::var("WORKFORCE_S3_TEST_BUCKET").ok()?;
        let region =
            std::env::var("WORKFORCE_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .load()
            .await;
        let s3conf = aws_sdk_s3::config::Builder::from(&conf)
            .force_path_style(true)
            .build();
        Some(S3ArtifactStore::new(
            Client::from_conf(s3conf),
            bucket,
            format!("wf-test/{}/", uuid::Uuid::new_v4().simple()),
        ))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn s3_roundtrip_write_read_metadata_expire_presign() {
        let Some(store) = test_store().await else {
            eprintln!("WORKFORCE_S3_TEST_BUCKET unset — skipping S3 round-trip");
            return;
        };

        let agent = "agent-rt";
        let mut d = descriptor(agent, ContentType::ApplicationJson);
        d.media_type = Some("application/json".into());
        d.filename = Some("rows.json".into());
        // A wide schema that would overflow the ~2KB user-metadata cap — proves
        // the companion-object layout carries it where user-metadata could not.
        d.schema = Some((0..500).map(|i| format!("column_name_{i}")).collect());
        let body = ArtifactBody::Json(serde_json::json!([{"a": 1}, {"a": 2}]));

        let handle = store.write(agent, d, body).unwrap();

        match store.read(agent, &handle.id).unwrap() {
            ArtifactBody::Json(v) => {
                assert_eq!(v, serde_json::json!([{"a": 1}, {"a": 2}]));
            }
            other => panic!("expected json, got {other:?}"),
        }

        let meta = store.metadata(agent, &handle.id).unwrap().unwrap();
        assert_eq!(meta.filename.as_deref(), Some("rows.json"));
        assert_eq!(meta.schema.as_ref().unwrap().len(), 500);

        let url = store
            .presigned_get_async(agent, &handle.id, Duration::from_secs(120))
            .await
            .unwrap();
        assert!(url.contains(handle.id.as_str()), "presigned url: {url}");

        // Streamed upload from a file.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"x,y\n1,2\n").unwrap();
        let mut fd = descriptor(agent, ContentType::OctetStream);
        fd.filename = Some("export.csv".into());
        let src_path = tmp.path().to_path_buf();
        let fh = store.write_from_path(agent, fd, &src_path).unwrap();
        assert!(
            !src_path.exists(),
            "write_from_path must consume the source (Fs parity); leaving it leaks hub disk"
        );
        match store.read(agent, &fh.id).unwrap() {
            ArtifactBody::Bytes(b) => assert_eq!(b, b"x,y\n1,2\n"),
            other => panic!("expected bytes, got {other:?}"),
        }

        // Expired (ttl 0) → not found on read, None on metadata.
        let mut expd = descriptor(agent, ContentType::ApplicationJson);
        expd.ttl_seconds = 0;
        let eh = store
            .write(agent, expd, ArtifactBody::Json(serde_json::json!([1])))
            .unwrap();
        assert!(store.read(agent, &eh.id).is_err());
        assert!(store.metadata(agent, &eh.id).unwrap().is_none());

        // Unknown id → not found / None.
        let unknown = ArtifactId::fresh();
        assert!(store.read(agent, &unknown).is_err());
        assert!(store.metadata(agent, &unknown).unwrap().is_none());
    }
}
