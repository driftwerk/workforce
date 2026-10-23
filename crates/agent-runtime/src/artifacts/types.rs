use std::time::Duration;

use eyre::{eyre, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Validated artifact identifier. The format `art_<32 hex chars>` is the
/// only shape any `ArtifactStore` will accept, which keeps caller-supplied
/// ids from escaping the artifact directory (no path traversal, no
/// case-insensitive filesystem collisions).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(String);

static ID_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^art_[0-9a-f]{32}$").expect("artifact id regex"));

impl ArtifactId {
    pub fn fresh() -> Self {
        Self(format!("art_{}", Uuid::new_v4().simple()))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if ID_PATTERN.is_match(raw) {
            Ok(Self(raw.to_string()))
        } else {
            Err(eyre!("invalid artifact id: {raw}"))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// MIME-shaped tag covering the artifact bodies the runtime stores.
/// New shapes land here when a new producer needs one — kept narrow so
/// consumer tools can dispatch on a small, exhaustive enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentType {
    ApplicationJson,
    TextCsv,
    OctetStream,
}

impl ContentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ApplicationJson => "application/json",
            Self::TextCsv => "text/csv",
            Self::OctetStream => "application/octet-stream",
        }
    }

    /// File extension (no dot) for naming a stored body on disk or when
    /// shipping it out as a file.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::ApplicationJson => "json",
            Self::TextCsv => "csv",
            Self::OctetStream => "bin",
        }
    }
}

impl std::fmt::Display for ContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Payload variants the store understands. Tabular bodies (JSON / CSV)
/// stay typed so consumer-side shape validation doesn't need to re-parse.
#[derive(Debug, Clone)]
pub enum ArtifactBody {
    Json(serde_json::Value),
    Csv(String),
    Bytes(Vec<u8>),
}

impl ArtifactBody {
    pub fn content_type(&self) -> ContentType {
        match self {
            Self::Json(_) => ContentType::ApplicationJson,
            Self::Csv(_) => ContentType::TextCsv,
            Self::Bytes(_) => ContentType::OctetStream,
        }
    }

    /// Byte size used by the size-gate to decide inline vs store.
    pub fn serialized_size(&self) -> usize {
        match self {
            Self::Json(v) => serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0),
            Self::Csv(s) => s.len(),
            Self::Bytes(b) => b.len(),
        }
    }

    /// Render the body to its wire bytes, consuming it so a large buffer is
    /// moved rather than copied. The single byte-rendering path shared by the
    /// store sidecar writer and any serving route that streams a read body.
    pub fn into_bytes(self) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Json(v) => serde_json::to_vec(&v)?,
            Self::Csv(s) => s.into_bytes(),
            Self::Bytes(b) => b,
        })
    }
}

pub(crate) fn value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub producer: String,
    pub mime: ContentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<usize>,
    /// Seconds until the store may purge the body. Stored as seconds so
    /// the JSON sidecar serialises cleanly.
    pub ttl_seconds: u64,
    pub agent_id: String,
    /// Session that produced the artifact. Model-driven readers-by-id
    /// enforce same-session access (see `resolve::read_session_scoped`);
    /// `None` (legacy / outside a run) is unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Requesting user at production time. Model-driven readers-by-id enforce
    /// same-user access (see `resolve::read_session_scoped`) — this is what
    /// isolates a shared agent's group channel, where one session is shared
    /// across users. `None` (legacy / system / outside a run) is unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Download filename a delivery producer stamped on the artifact. Persisted
    /// so a stateless serving route can set `Content-Disposition` without the
    /// transient delivery marker. `None` (non-delivered bodies / legacy) lets the
    /// route derive one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Concrete MIME type a delivery producer stamped on the artifact (finer than
    /// the coarse [`ContentType`] enum). Persisted for the serving route's
    /// `Content-Type`. `None` (non-delivered bodies / legacy) falls back to `mime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

impl ArtifactDescriptor {
    /// Descriptor stamped with the ambient session/user task-locals and the
    /// default TTL. The standard producer-side constructor; override fields
    /// with struct-update syntax where needed.
    pub fn from_context(
        producer: impl Into<String>,
        mime: ContentType,
        agent_id: impl Into<String>,
    ) -> Self {
        let ambient = |local: &'static tokio::task::LocalKey<String>| {
            local.try_with(|v| v.clone()).ok().filter(|v| !v.is_empty())
        };
        Self {
            producer: producer.into(),
            mime,
            schema: None,
            row_count: None,
            ttl_seconds: super::store::DEFAULT_TTL.as_secs(),
            agent_id: agent_id.into(),
            session_id: ambient(&crate::runner::CURRENT_ARTIFACT_SCOPE)
                .or_else(|| ambient(&crate::runner::CURRENT_SESSION_ID)),
            user_id: ambient(&crate::runner::CURRENT_USER_ID),
            filename: None,
            media_type: None,
        }
    }

    /// Override the body TTL — e.g. uploads use [`super::store::ATTACHMENT_TTL`]
    /// rather than the short default.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl_seconds = ttl.as_secs();
        self
    }

    /// Stamp the download filename and concrete MIME a delivery producer already
    /// computed, so they persist alongside the body for a stateless serving route.
    pub fn with_file_meta(
        mut self,
        filename: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Self {
        self.filename = Some(filename.into());
        self.media_type = Some(media_type.into());
        self
    }

    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_seconds)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactHandle {
    pub id: ArtifactId,
    pub descriptor: ArtifactDescriptor,
    pub preview: serde_json::Value,
}

/// Result of `maybe_store`: either the body is small enough to inline in
/// the tool result, or it was offloaded and consumers should reference
/// it by `ArtifactHandle`.
// Short-lived return value, constructed then immediately destructured at every
// call site — never held or collected — so boxing the larger variant would trade
// a stack slot for a heap allocation on the common path with no benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum MaybeArtifact {
    Inline(ArtifactBody),
    Stored(ArtifactHandle),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_id_matches_expected_shape() {
        let id = ArtifactId::fresh();
        assert!(ID_PATTERN.is_match(id.as_str()), "got {}", id.as_str());
    }

    #[test]
    fn parse_accepts_a_fresh_id() {
        let id = ArtifactId::fresh();
        let round_tripped = ArtifactId::parse(id.as_str()).expect("round-trip");
        assert_eq!(round_tripped, id);
    }

    #[test]
    fn parse_rejects_path_traversal() {
        for bad in &["../etc/passwd", "art_../a", "/abs/path", "art_a/b", "art_"] {
            assert!(ArtifactId::parse(bad).is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn parse_rejects_wrong_case_or_length() {
        let id = ArtifactId::fresh();
        let upper = id.as_str().to_uppercase();
        assert!(ArtifactId::parse(&upper).is_err());
        assert!(ArtifactId::parse("art_abcdef").is_err());
    }

    #[test]
    fn parse_rejects_unknown_prefix() {
        let id = Uuid::new_v4().simple().to_string();
        assert!(ArtifactId::parse(&format!("blob_{id}")).is_err());
    }

    #[test]
    fn from_context_stamps_ambient_session_and_user() {
        let stamped = crate::runner::CURRENT_SESSION_ID.sync_scope("sess-1".to_string(), || {
            crate::runner::CURRENT_USER_ID.sync_scope("user-1".to_string(), || {
                ArtifactDescriptor::from_context("p", ContentType::ApplicationJson, "agent-a")
            })
        });
        assert_eq!(stamped.session_id.as_deref(), Some("sess-1"));
        assert_eq!(stamped.user_id.as_deref(), Some("user-1"));
        assert_eq!(stamped.ttl_seconds, 3600);
        assert_eq!(stamped.agent_id, "agent-a");

        let bare = ArtifactDescriptor::from_context("p", ContentType::ApplicationJson, "agent-a");
        assert!(bare.session_id.is_none());
        assert!(bare.user_id.is_none());
    }

    #[test]
    fn descriptor_without_scope_fields_deserializes() {
        let legacy =
            r#"{"producer":"p","mime":"application-json","ttl_seconds":60,"agent_id":"a"}"#;
        let d: ArtifactDescriptor = serde_json::from_str(legacy).unwrap();
        assert!(d.session_id.is_none());
        assert!(d.user_id.is_none());
        assert!(d.filename.is_none());
        assert!(d.media_type.is_none());
    }

    #[test]
    fn file_meta_round_trips_and_is_omitted_when_unset() {
        let bare = ArtifactDescriptor::from_context("p", ContentType::OctetStream, "a");
        let serialized = serde_json::to_string(&bare).unwrap();
        assert!(!serialized.contains("filename"), "unset fields are skipped");
        assert!(!serialized.contains("media_type"));

        let stamped = bare.with_file_meta("report.csv", "text/csv");
        let json = serde_json::to_string(&stamped).unwrap();
        let restored: ArtifactDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.filename.as_deref(), Some("report.csv"));
        assert_eq!(restored.media_type.as_deref(), Some("text/csv"));
    }

    #[test]
    fn body_serialized_size_tracks_payload() {
        let json = ArtifactBody::Json(serde_json::json!([1, 2, 3]));
        let csv = ArtifactBody::Csv("a,b\n1,2\n".into());
        let bytes = ArtifactBody::Bytes(vec![0; 1024]);
        assert!(json.serialized_size() >= 5);
        assert_eq!(csv.serialized_size(), "a,b\n1,2\n".len());
        assert_eq!(bytes.serialized_size(), 1024);
    }

    #[test]
    fn body_content_type_matches_variant() {
        assert_eq!(
            ArtifactBody::Json(serde_json::json!(null)).content_type(),
            ContentType::ApplicationJson
        );
        assert_eq!(
            ArtifactBody::Csv(String::new()).content_type(),
            ContentType::TextCsv
        );
        assert_eq!(
            ArtifactBody::Bytes(Vec::new()).content_type(),
            ContentType::OctetStream
        );
    }
}
