//! S3 client factory + the [`S3Backend`] trait that the tool impls call.
//!
//! Splitting the SDK calls behind a trait lets the tool impls be unit-tested
//! with a hand-written mock backend without dragging the SDK's mocking
//! infrastructure in. The `Sdk` impl is the only production path.

use std::collections::HashSet;
use std::num::NonZeroUsize;

use async_trait::async_trait;
use aws_config::{sts::AssumeRoleProvider, BehaviorVersion};
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::error::ProvideErrorMetadata;
use bytes::{Bytes, BytesMut};
use eyre::{eyre, Result};
use lru::LruCache;

use workforce_agent::runner::SessionContext;

const STS_SESSION_NAME_MAX: usize = 64;
const STS_SESSION_ID_CHARS: usize = 16;
const FALLBACK_STS_SESSION_NAME: &str = "workforce-hub";
const SESSION_CACHE_CAPACITY: usize = 256;

/// Build an STS session name from the current session context, sanitised to
/// the STS-allowed charset `[A-Za-z0-9+=,.@-]` and capped at 64 chars.
/// Falls back to a stable label so CloudTrail always carries *something*
/// even when called outside an agent run (background work, tests).
///
/// 16 chars of the session id (vs the original 8) bring collision space to
/// 2^64 for hex/uuid-style ids, so distinct sessions don't share a cache
/// entry — and therefore don't share STS RoleSessionName attribution —
/// even when their ids share a long timestamp-derived prefix (UUIDv7/ULID).
pub fn sts_session_name(ctx: &SessionContext) -> String {
    let agent = ctx
        .agent_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("anon");
    let session_short: String = ctx
        .id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(STS_SESSION_ID_CHARS).collect())
        .unwrap_or_else(|| "nosess".to_string());
    let raw = format!("hub-{agent}-{session_short}");
    let sanitised: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | ',' | '.' | '@' | '-'))
        .collect();
    let trimmed: String = sanitised.chars().take(STS_SESSION_NAME_MAX).collect();
    if trimmed.is_empty() {
        FALLBACK_STS_SESSION_NAME.to_string()
    } else {
        trimmed
    }
}

/// A list-object-v2 page returned to the tool.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ListedKey {
    pub key: String,
    pub size: i64,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ListResponse {
    pub keys: Vec<ListedKey>,
    /// Common prefixes ("subdirectories") returned when the call passes a
    /// delimiter — the efficient one-level enumeration that maps a hierarchy
    /// without paging every leaf key. Empty when no delimiter was supplied.
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HeadResponse {
    pub size: i64,
    pub content_type: Option<String>,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct GetResponse {
    pub body: Bytes,
    pub content_type: Option<String>,
    pub size: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Distinguishes "object missing" from generic SDK errors so the tool can
/// surface a precise message ("object not found") rather than a stack trace.
#[derive(Debug, Clone, thiserror::Error)]
pub enum S3Error {
    #[error("object not found")]
    NotFound,
    #[error("access denied")]
    AccessDenied,
    #[error("object exceeds max_get_bytes ({cap}): aborted at {observed} bytes")]
    TooLarge { observed: u64, cap: u64 },
    #[error("S3 error: {0}")]
    Other(String),
}

/// Abstract over the SDK so tool tests can drive list/head/get with a
/// hand-written stub. The SDK impl is the only production user.
///
/// `get` takes `max_bytes` so the backend can enforce the cap during the
/// body stream rather than after the entire object is materialised. A
/// dishonest `Content-Length` then cannot OOM the hub.
#[async_trait]
pub trait S3Backend: Send + Sync {
    async fn list(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        continuation_token: Option<&str>,
        max_keys: u16,
    ) -> Result<ListResponse, S3Error>;

    async fn head(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        key: &str,
    ) -> Result<HeadResponse, S3Error>;

    async fn get(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        key: &str,
        max_bytes: u64,
    ) -> Result<GetResponse, S3Error>;

    /// Stream an object body straight to `dest` on disk without buffering it in
    /// memory, enforcing `cap` mid-stream (a lying `Content-Length` cannot then
    /// fill the disk past the ceiling). Returns the bytes written. Backs the
    /// large-object download path, which delivers a file instead of refusing.
    async fn get_to_path(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        key: &str,
        dest: &std::path::Path,
        cap: u64,
    ) -> Result<u64, S3Error>;
}

/// Bucket allowlist as a shared `HashSet`. Tools hold an `Arc<Allowlist>`
/// and check `contains` before any backend call. This is the second wall
/// behind the IAM `Resource` scope; either alone would refuse, both
/// together fail-stop on misconfiguration.
#[derive(Debug, Clone)]
pub struct Allowlist(HashSet<String>);

impl Allowlist {
    pub fn new<I: IntoIterator<Item = String>>(buckets: I) -> Self {
        Self(buckets.into_iter().collect())
    }

    pub fn contains(&self, bucket: &str) -> bool {
        self.0.contains(bucket)
    }

    pub fn deny_message(&self, bucket: &str) -> String {
        let mut listed: Vec<&str> = self.0.iter().map(String::as_str).collect();
        listed.sort_unstable();
        format!(
            "bucket '{bucket}' is not allowlisted; allowed: {}",
            listed.join(", ")
        )
    }
}

/// Production backend: real `aws-sdk-s3::Client`, scoped to the temporary
/// credentials minted by `sts:AssumeRole` against [`AwsS3ClientFactory::role_arn`].
pub struct SdkS3Backend {
    factory: AwsS3ClientFactory,
}

impl SdkS3Backend {
    pub fn new(factory: AwsS3ClientFactory) -> Self {
        Self { factory }
    }
}

#[async_trait]
impl S3Backend for SdkS3Backend {
    async fn list(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        continuation_token: Option<&str>,
        max_keys: u16,
    ) -> Result<ListResponse, S3Error> {
        let client = self.factory.client_for(ctx).await.map_err(into_other)?;
        let mut req = client
            .list_objects_v2()
            .bucket(bucket)
            .max_keys(max_keys as i32);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(d) = delimiter {
            req = req.delimiter(d);
        }
        if let Some(t) = continuation_token {
            req = req.continuation_token(t);
        }
        let out = req.send().await.map_err(map_sdk_err)?;
        let keys = out
            .contents()
            .iter()
            .map(|o| ListedKey {
                key: o.key().unwrap_or("").to_string(),
                size: o.size().unwrap_or(0),
                last_modified: o.last_modified().map(|t| t.to_string()),
                etag: o.e_tag().map(|s| s.to_string()),
            })
            .collect();
        let common_prefixes = out
            .common_prefixes()
            .iter()
            .filter_map(|p| p.prefix().map(str::to_string))
            .collect();
        Ok(ListResponse {
            keys,
            common_prefixes,
            is_truncated: out.is_truncated().unwrap_or(false),
            next_continuation_token: out.next_continuation_token().map(|s| s.to_string()),
        })
    }

    async fn head(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        key: &str,
    ) -> Result<HeadResponse, S3Error> {
        let client = self.factory.client_for(ctx).await.map_err(into_other)?;
        let out = client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_err)?;
        Ok(HeadResponse {
            size: out.content_length().unwrap_or(0),
            content_type: out.content_type().map(|s| s.to_string()),
            last_modified: out.last_modified().map(|t| t.to_string()),
            etag: out.e_tag().map(|s| s.to_string()),
            metadata: out
                .metadata()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        })
    }

    async fn get(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        key: &str,
        max_bytes: u64,
    ) -> Result<GetResponse, S3Error> {
        let client = self.factory.client_for(ctx).await.map_err(into_other)?;
        let out = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_err)?;
        let content_type = out.content_type().map(|s| s.to_string());
        let size = out.content_length().unwrap_or(0);
        let etag = out.e_tag().map(|s| s.to_string());
        let last_modified = out.last_modified().map(|t| t.to_string());
        let body = read_capped(out.body, max_bytes).await?;
        Ok(GetResponse {
            body,
            content_type,
            size,
            etag,
            last_modified,
        })
    }

    async fn get_to_path(
        &self,
        ctx: &SessionContext,
        bucket: &str,
        key: &str,
        dest: &std::path::Path,
        cap: u64,
    ) -> Result<u64, S3Error> {
        let client = self.factory.client_for(ctx).await.map_err(into_other)?;
        let out = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_err)?;
        stream_to_file(out.body, dest, cap).await
    }
}

/// Drain the SDK body stream chunk-by-chunk to `dest`, aborting as soon as the
/// running byte count exceeds `cap`, so a streamed download never holds the
/// object in memory nor overruns the ceiling. `cap == 0` disables the ceiling.
async fn stream_to_file(
    mut stream: aws_sdk_s3::primitives::ByteStream,
    dest: &std::path::Path,
    cap: u64,
) -> Result<u64, S3Error> {
    use tokio::io::AsyncWriteExt;
    let unlimited = cap == 0;
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| S3Error::Other(format!("create download file failed: {e}")))?;
    let mut written: u64 = 0;
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|e| S3Error::Other(format!("body read failed: {e}")))?
    {
        written = written.saturating_add(chunk.len() as u64);
        if !unlimited && written > cap {
            return Err(S3Error::TooLarge {
                observed: written,
                cap,
            });
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| S3Error::Other(format!("write download file failed: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| S3Error::Other(format!("flush download file failed: {e}")))?;
    Ok(written)
}

/// Drain the SDK body stream chunk-by-chunk into a buffer, aborting as
/// soon as the running byte count would exceed `max_bytes`. Guarantees no
/// more than `max_bytes` are held in memory, even if the server's
/// `Content-Length` was a lie.
///
/// `max_bytes == 0` means "no cap": the operator has explicitly disabled
/// the ceiling, so the whole body is buffered (and OOM is on them).
async fn read_capped(
    mut stream: aws_sdk_s3::primitives::ByteStream,
    max_bytes: u64,
) -> Result<Bytes, S3Error> {
    let unlimited = max_bytes == 0;
    let cap_usize: usize = max_bytes.try_into().unwrap_or(usize::MAX);
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|e| S3Error::Other(format!("body read failed: {e}")))?
    {
        if !unlimited {
            let observed = (buf.len() as u64).saturating_add(chunk.len() as u64);
            if observed > max_bytes {
                return Err(S3Error::TooLarge {
                    observed,
                    cap: max_bytes,
                });
            }
        }
        let want = if unlimited {
            chunk.len()
        } else {
            cap_usize.saturating_sub(buf.len()).min(chunk.len())
        };
        buf.reserve(want);
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

fn map_sdk_err<E, R>(err: aws_sdk_s3::error::SdkError<E, R>) -> S3Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    if let Some(svc) = err.as_service_error() {
        if let Some(code) = svc.code() {
            return classify_aws_error_code(code);
        }
    }
    classify_error_message(&err.to_string())
}

/// Map a canonical AWS error code (the contract — `NoSuchKey`,
/// `AccessDenied`, etc.) to a typed [`S3Error`]. Service errors that don't
/// match a known code surface as `Other` with the raw code in the message.
pub(crate) fn classify_aws_error_code(code: &str) -> S3Error {
    match code {
        "NoSuchKey" | "NoSuchBucket" | "NotFound" => S3Error::NotFound,
        "AccessDenied" | "Forbidden" | "AllAccessDisabled" => S3Error::AccessDenied,
        other => S3Error::Other(format!("S3 service error: {other}")),
    }
}

/// Fallback classifier for non-service SDK errors (timeouts, DNS, TLS).
/// Anchored on canonical AWS code spellings; bare 3-digit status codes
/// are intentionally NOT matched — `"timeout after 1403ms"` must not be
/// mis-classified as AccessDenied.
pub(crate) fn classify_error_message(msg: &str) -> S3Error {
    let lower = msg.to_lowercase();
    if lower.contains("nosuchkey") || lower.contains("nosuchbucket") {
        S3Error::NotFound
    } else if lower.contains("accessdenied")
        || lower.contains("access denied")
        || lower.contains("forbidden")
    {
        S3Error::AccessDenied
    } else {
        S3Error::Other(msg.to_string())
    }
}

fn into_other(err: eyre::Report) -> S3Error {
    S3Error::Other(err.to_string())
}

/// Mints per-session `SdkConfig`s under temporary credentials from an
/// `sts:AssumeRole` against `role_arn`. The base SdkConfig is loaded once
/// (it picks up the ECS task-role credentials provider via the SDK's
/// default chain); each new STS session name (one per agent session)
/// mints an assume-role-wrapped SdkConfig once and caches it.
///
/// Shared by every AWS tool surface: S3 ([`SdkS3Backend`]) and the infra
/// tools (`SdkInfraBackend`) each build their own typed client from the
/// returned config — constructing a client is cheap, the STS round-trip is
/// not, so the `SdkConfig` (with its inner credential cache) is what we reuse.
///
/// Cache is an LRU bounded at `SESSION_CACHE_CAPACITY` — the oldest entry
/// is evicted on overflow, not the whole cache. Within a session subsequent
/// `config_for` calls reuse the same `SdkConfig` and the inner
/// `AssumeRoleProvider`'s cred cache refreshes transparently. A debug burst
/// of "list, head×3, get" pays exactly one STS round-trip.
pub struct SdkConfigFactory {
    base: aws_config::SdkConfig,
    role_arn: String,
    cache: parking_lot::Mutex<LruCache<String, aws_config::SdkConfig>>,
}

impl SdkConfigFactory {
    /// `region` pins the SDK region (the infra tools set `eu-west-2`
    /// explicitly); `None` keeps the ambient region from the environment
    /// (`AWS_REGION`), which is what the S3 surface relies on.
    pub async fn load(role_arn: String, region: Option<String>) -> Result<Self> {
        let loader = aws_config::defaults(BehaviorVersion::latest());
        let loader = match region {
            Some(r) => loader.region(aws_config::Region::new(r)),
            None => loader,
        };
        let base = loader.load().await;
        let capacity = NonZeroUsize::new(SESSION_CACHE_CAPACITY)
            .expect("SESSION_CACHE_CAPACITY const is non-zero");
        Ok(Self {
            base,
            role_arn,
            cache: parking_lot::Mutex::new(LruCache::new(capacity)),
        })
    }

    pub fn role_arn(&self) -> &str {
        &self.role_arn
    }

    pub async fn config_for(&self, ctx: &SessionContext) -> Result<aws_config::SdkConfig> {
        let session_name = sts_session_name(ctx);
        if let Some(cfg) = self.cache.lock().get(&session_name).cloned() {
            return Ok(cfg);
        }
        let source = self
            .base
            .credentials_provider()
            .ok_or_else(|| eyre!("default AWS credentials provider not available"))?;
        let provider = AssumeRoleProvider::builder(&self.role_arn)
            .session_name(session_name.clone())
            .configure(&self.base)
            .build_from_provider(SharedCredentialsProvider::new(source))
            .await;
        let cfg = self
            .base
            .to_builder()
            .credentials_provider(SharedCredentialsProvider::new(provider))
            .build();
        self.cache.lock().put(session_name, cfg.clone());
        Ok(cfg)
    }
}

/// S3-specific facade over [`SdkConfigFactory`] — builds `aws_sdk_s3::Client`s
/// from the shared per-session `SdkConfig`, keeping the ambient region.
pub struct AwsS3ClientFactory {
    inner: SdkConfigFactory,
}

impl AwsS3ClientFactory {
    pub async fn load(role_arn: String) -> Result<Self> {
        Ok(Self {
            inner: SdkConfigFactory::load(role_arn, None).await?,
        })
    }

    pub fn role_arn(&self) -> &str {
        self.inner.role_arn()
    }

    async fn client_for(&self, ctx: &SessionContext) -> Result<aws_sdk_s3::Client> {
        Ok(aws_sdk_s3::Client::new(&self.inner.config_for(ctx).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(agent: Option<&str>, id: Option<&str>) -> SessionContext {
        SessionContext {
            key: None,
            id: id.map(String::from),
            user_id: None,
            billing_identity: None,
            platform_user_id: None,
            channel_id: None,
            thread_id: None,
            agent_id: agent.map(String::from),
            workspace_id: None,
            voice_channel: None,
            automated_origin: None,
            active_mcp_envs: Default::default(),
            active_environment: None,
            active_skill_instances: Default::default(),
            artifact_ttl: None,
            artifact_store: None,
            last_user_comm: None,
            artifact_scope: None,
            secret_policy: None,
            served: None,
        }
    }

    #[test]
    fn sts_session_name_includes_agent_and_16_char_session_prefix() {
        let s = sts_session_name(&ctx(
            Some("atlas"),
            Some("0123456789abcdef0123456789abcdef"),
        ));
        assert!(s.starts_with("hub-atlas-"));
        assert!(s.contains("0123456789abcdef"));
        assert!(
            !s.contains("0123456789abcdef0"),
            "id is truncated at 16 chars"
        );
    }

    #[test]
    fn sts_session_name_handles_missing_context() {
        let s = sts_session_name(&ctx(None, None));
        assert_eq!(s, "hub-anon-nosess");
    }

    #[test]
    fn sts_session_name_handles_short_session_id() {
        let s = sts_session_name(&ctx(Some("l"), Some("xy")));
        assert_eq!(s, "hub-l-xy");
    }

    #[test]
    fn sts_session_name_strips_disallowed_chars() {
        let s = sts_session_name(&ctx(Some("agent/colon:slash"), Some("ok-123!@")));
        assert!(s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | ',' | '.' | '@' | '-')));
        assert!(s.contains("colonslash"));
    }

    #[test]
    fn sts_session_name_capped_at_64_chars() {
        let long_agent = "a".repeat(100);
        let s = sts_session_name(&ctx(Some(&long_agent), Some("xxxxxxxxyyyy")));
        assert!(s.len() <= STS_SESSION_NAME_MAX);
        assert!(s.starts_with("hub-"));
    }

    #[test]
    fn sts_session_name_handles_unicode_session_id_without_panic() {
        let s = sts_session_name(&ctx(Some("atlas"), Some("café-1234")));
        assert!(s.starts_with("hub-atlas-"));
    }

    #[test]
    fn sts_session_name_distinct_for_ids_sharing_long_prefix() {
        // Two ids that share the first 8 chars (the old window) but differ
        // within the first 16 (the new window) must produce distinct names.
        let a = sts_session_name(&ctx(Some("l"), Some("01234567aaaaaaaa-trailing")));
        let b = sts_session_name(&ctx(Some("l"), Some("01234567bbbbbbbb-trailing")));
        assert_ne!(a, b, "16-char window must distinguish ids past byte 8");
    }

    #[test]
    fn allowlist_contains_is_exact_case_sensitive() {
        let al = Allowlist::new(vec!["my-bucket".to_string(), "other".to_string()]);
        assert!(al.contains("my-bucket"));
        assert!(!al.contains("My-Bucket"));
        assert!(!al.contains("my-buckets"));
        assert!(!al.contains("other-bucket"));
    }

    #[test]
    fn allowlist_deny_message_lists_allowed_buckets() {
        let al = Allowlist::new(vec!["b".to_string(), "a".to_string()]);
        let msg = al.deny_message("c");
        assert!(msg.contains("'c'"));
        assert!(msg.contains("a, b"), "{msg}");
    }

    #[test]
    fn classify_aws_error_code_recognises_canonical_codes() {
        assert!(matches!(
            classify_aws_error_code("NoSuchKey"),
            S3Error::NotFound
        ));
        assert!(matches!(
            classify_aws_error_code("NoSuchBucket"),
            S3Error::NotFound
        ));
        assert!(matches!(
            classify_aws_error_code("NotFound"),
            S3Error::NotFound
        ));
        assert!(matches!(
            classify_aws_error_code("AccessDenied"),
            S3Error::AccessDenied
        ));
        assert!(matches!(
            classify_aws_error_code("Forbidden"),
            S3Error::AccessDenied
        ));
        assert!(matches!(
            classify_aws_error_code("AllAccessDisabled"),
            S3Error::AccessDenied
        ));
        match classify_aws_error_code("InternalError") {
            S3Error::Other(m) => assert!(m.contains("InternalError")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn classify_error_message_recognises_canonical_names_in_fallback_text() {
        for s in [
            "service error: NoSuchKey",
            "operation error GetObject: NoSuchKey",
            "service error: NoSuchBucket",
        ] {
            assert!(
                matches!(classify_error_message(s), S3Error::NotFound),
                "should map to NotFound: {s}",
            );
        }
        for s in [
            "service error: AccessDenied",
            "user is not authorized: access denied",
            "403 Forbidden",
            "wrapped: AccessDenied (status: 403)",
        ] {
            assert!(
                matches!(classify_error_message(s), S3Error::AccessDenied),
                "should map to AccessDenied: {s}",
            );
        }
    }

    #[test]
    fn classify_error_message_does_not_match_naked_status_digits() {
        for s in [
            "timeout after 1403ms",
            "connection closed; received 4044 bytes",
            "endpoint resolution failed (latency 403)",
            "dispatch error: connection refused",
        ] {
            assert!(
                matches!(classify_error_message(s), S3Error::Other(_)),
                "naked digits in '{s}' must not classify as a known S3 error",
            );
        }
    }

    #[tokio::test]
    async fn read_capped_aborts_when_body_exceeds_cap() {
        use aws_sdk_s3::primitives::ByteStream;
        let body = ByteStream::from_static(&[0u8; 1024]);
        let err = read_capped(body, 100).await.unwrap_err();
        assert!(
            matches!(err, S3Error::TooLarge { observed, cap } if observed >= 100 && cap == 100),
            "{err:?}",
        );
    }

    #[tokio::test]
    async fn read_capped_returns_body_when_under_cap() {
        use aws_sdk_s3::primitives::ByteStream;
        let body = ByteStream::from_static(b"hello");
        let buf = read_capped(body, 100).await.unwrap();
        assert_eq!(&buf[..], b"hello");
    }

    #[tokio::test]
    async fn read_capped_accepts_body_at_exact_cap() {
        use aws_sdk_s3::primitives::ByteStream;
        let body = ByteStream::from_static(b"abcde");
        let buf = read_capped(body, 5).await.unwrap();
        assert_eq!(&buf[..], b"abcde");
    }

    #[tokio::test]
    async fn read_capped_cap_zero_means_unlimited() {
        use aws_sdk_s3::primitives::ByteStream;
        let body = ByteStream::from_static(&[0u8; 4096]);
        let buf = read_capped(body, 0).await.unwrap();
        assert_eq!(buf.len(), 4096, "cap=0 buffers the whole body, no abort");
    }

    #[tokio::test]
    async fn stream_to_file_writes_body_under_cap() {
        use aws_sdk_s3::primitives::ByteStream;
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        let written = stream_to_file(ByteStream::from_static(b"hello world"), &dest, 100)
            .await
            .unwrap();
        assert_eq!(written, 11);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn stream_to_file_aborts_when_body_exceeds_cap() {
        use aws_sdk_s3::primitives::ByteStream;
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        let err = stream_to_file(ByteStream::from_static(&[0u8; 1024]), &dest, 100)
            .await
            .unwrap_err();
        assert!(
            matches!(err, S3Error::TooLarge { observed, cap } if observed >= 100 && cap == 100),
            "{err:?}",
        );
    }

    #[tokio::test]
    async fn stream_to_file_cap_zero_writes_whole_body() {
        use aws_sdk_s3::primitives::ByteStream;
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("out.bin");
        let written = stream_to_file(ByteStream::from_static(&[7u8; 4096]), &dest, 0)
            .await
            .unwrap();
        assert_eq!(written, 4096);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 4096);
    }
}
