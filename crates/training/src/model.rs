use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Corpus {
    pub id: Uuid,
    pub agent_id: String,
    pub name: String,
    pub scope: CorpusScope,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum CorpusScope {
    Agent,
    Team(String),
    User(String),
}

impl CorpusScope {
    pub fn parts(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Agent => ("agent", None),
            Self::Team(id) => ("team", Some(id.as_str())),
            Self::User(id) => ("user", Some(id.as_str())),
        }
    }

    pub fn from_parts(kind: &str, id: Option<&str>) -> Self {
        match (kind, id) {
            ("team", Some(s)) => Self::Team(s.to_string()),
            ("user", Some(s)) => Self::User(s.to_string()),
            _ => Self::Agent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Document {
    pub id: Uuid,
    pub corpus_id: Uuid,
    pub original_filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub storage_uri: String,
    pub source_kind: SourceKind,
    pub source_ref: Option<String>,
    pub processing_status: ProcessingStatus,
    pub processing_error: Option<String>,
    pub uploaded_at: DateTime<Utc>,
    pub uploaded_by: String,
    pub page_count: Option<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Cli,
    HttpUpload,
    Slack,
    WebUi,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::HttpUpload => "http_upload",
            Self::Slack => "slack",
            Self::WebUi => "web_ui",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "http_upload" => Self::HttpUpload,
            "slack" => Self::Slack,
            "web_ui" => Self::WebUi,
            _ => Self::Cli,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingStatus {
    Pending,
    Processing,
    Ready,
    Failed,
}

impl ProcessingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "processing" => Self::Processing,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Chunk {
    pub id: Uuid,
    pub document_id: Uuid,
    pub chunk_index: i32,
    pub content_hash: String,
    pub memory_id: String,
    pub locator: String,
    pub char_start: i32,
    pub char_end: i32,
}

/// Pure-function identity hash used to detect "this chunk already
/// exists" on re-import. Stable across reloads of the same content.
pub struct ChunkIdentity;

impl ChunkIdentity {
    pub fn hash(corpus_id: Uuid, body: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(corpus_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(body.as_bytes());
        let bytes = hasher.finalize();
        hex_encode(&bytes[..16])
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_identity_is_deterministic() {
        let corpus = Uuid::nil();
        let a = ChunkIdentity::hash(corpus, "hello");
        let b = ChunkIdentity::hash(corpus, "hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn chunk_identity_differs_on_content() {
        let corpus = Uuid::nil();
        assert_ne!(
            ChunkIdentity::hash(corpus, "hello"),
            ChunkIdentity::hash(corpus, "world"),
        );
    }

    #[test]
    fn chunk_identity_differs_per_corpus() {
        let c1 = Uuid::from_u128(1);
        let c2 = Uuid::from_u128(2);
        assert_ne!(
            ChunkIdentity::hash(c1, "hello"),
            ChunkIdentity::hash(c2, "hello"),
        );
    }

    #[test]
    fn scope_round_trip() {
        for s in [
            CorpusScope::Agent,
            CorpusScope::Team("t1".into()),
            CorpusScope::User("u1".into()),
        ] {
            let (k, id) = s.parts();
            assert_eq!(CorpusScope::from_parts(k, id), s);
        }
    }
}
