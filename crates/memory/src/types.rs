use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_core::types::AgentId;

/// Canonical identifier for a human user / paired-device principal (DW-525).
///
/// Distinct from [`AgentId`] in the type system so cross-user visibility
/// checks can require a user subject rather than collapsing every member of
/// an agent to "the agent's identity." Wraps the same underlying string the
/// rest of the codebase already uses (Slack identity UUID, paired-device id,
/// system sentinels like `system:webhook`); the newtype is purely for the
/// access-control surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

impl UserId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for UserId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for UserId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A memory entry stored in the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "MemoryDeser")]
pub struct Memory {
    /// Unique memory identifier.
    pub id: MemoryId,
    /// The content of the memory.
    pub content: String,
    /// Which layer this memory belongs to.
    pub layer: MemoryLayer,
    /// Who created this memory.
    pub owner: AgentId,
    /// Who can see this memory.
    pub visibility: Visibility,
    /// Relevance/importance score (0.0 to 1.0).
    pub importance: f32,
    /// Arbitrary metadata tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// How the memory was produced.
    #[serde(default)]
    pub source: MemorySource,
    /// Canonical identity UUID of the person/entity that created this memory.
    #[serde(default)]
    pub identity_id: Option<String>,
    /// Session that produced this memory, when it was extracted from conversation.
    /// Enables drill-down back to the original transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    /// Transcript message IDs within `source_session_id` covering this memory's turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_message_ids: Vec<String>,
    /// When this memory was created (i.e. the wallclock at write time).
    ///
    /// Use [`Memory::event_time`] when you want the time of the underlying
    /// event — `created_at` and event time diverge whenever extraction is
    /// lazy, e.g. session-scanner backfilling old turns: the memory is
    /// created today but describes something that happened a week ago.
    pub created_at: DateTime<Utc>,
    /// When the depicted event actually happened.
    ///
    /// For passively-extracted memories this is the timestamp of the latest
    /// source message in the turn. For curated/agent-written memories it
    /// equals `created_at`. Filters that ask "what happened on day X" use
    /// this field, not `created_at`.
    ///
    /// Legacy JSON without this field falls back to `created_at` (see
    /// [`MemoryDeser`]), matching the SQL/Weaviate row parsers — never to
    /// "now", which would make old content look freshly minted.
    pub event_time: DateTime<Utc>,
    /// When this memory was last accessed.
    pub last_accessed: DateTime<Utc>,
    /// How many times this memory has been recalled.
    #[serde(default)]
    pub access_count: u32,
    /// How many times this memory has been re-extracted as a near-duplicate.
    ///
    /// The original write does not increment this; only subsequent matches via
    /// `MemoryStore::record_recurrence` do. Lets a long-running situation
    /// (e.g. the user repeatedly hitting the same wall) collapse to one
    /// memory whose recurrence count expresses the urgency, instead of
    /// polluting the store with N near-identical episodic fragments.
    #[serde(default)]
    pub recurrence_count: u32,
    /// When this memory most recently recurred. `None` until a recurrence
    /// is recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recurrence_at: Option<DateTime<Utc>>,
    /// Whether this memory has been soft-deleted.
    #[serde(default)]
    pub deleted: bool,
}

/// Deserialization shadow for [`Memory`]: identical shape except
/// `event_time` is optional so legacy JSON (from before the field existed)
/// can fall back to `created_at` instead of a `Utc::now()` default that
/// re-dates old content to the moment of parsing.
#[derive(Deserialize)]
struct MemoryDeser {
    id: MemoryId,
    content: String,
    layer: MemoryLayer,
    owner: AgentId,
    visibility: Visibility,
    importance: f32,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    source: MemorySource,
    #[serde(default)]
    identity_id: Option<String>,
    #[serde(default)]
    source_session_id: Option<String>,
    #[serde(default)]
    source_message_ids: Vec<String>,
    created_at: DateTime<Utc>,
    #[serde(default)]
    event_time: Option<DateTime<Utc>>,
    last_accessed: DateTime<Utc>,
    #[serde(default)]
    access_count: u32,
    #[serde(default)]
    recurrence_count: u32,
    #[serde(default)]
    last_recurrence_at: Option<DateTime<Utc>>,
    #[serde(default)]
    deleted: bool,
}

impl From<MemoryDeser> for Memory {
    fn from(de: MemoryDeser) -> Self {
        Self {
            id: de.id,
            content: de.content,
            layer: de.layer,
            owner: de.owner,
            visibility: de.visibility,
            importance: de.importance,
            tags: de.tags,
            source: de.source,
            identity_id: de.identity_id,
            source_session_id: de.source_session_id,
            source_message_ids: de.source_message_ids,
            created_at: de.created_at,
            event_time: de.event_time.unwrap_or(de.created_at),
            last_accessed: de.last_accessed,
            access_count: de.access_count,
            recurrence_count: de.recurrence_count,
            last_recurrence_at: de.last_recurrence_at,
            deleted: de.deleted,
        }
    }
}

/// How a memory was produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// Origin unknown (pre-migration memories).
    #[default]
    Unknown,
    /// Written from human conversation via notes tool.
    Human,
    /// Technical/compaction log.
    Technical,
    /// Curated long-term knowledge (append_memory).
    Curated,
    /// Generated by the compaction pipeline (end-of-context summarisation).
    Compaction,
    /// Written directly by an agent tool.
    Agent,
    /// Extracted by the per-turn passive memory pipeline.
    Passive,
    /// Trigger-conditioned behaviour-changing note extracted from
    /// failure→recovery / user-correction / repeated-retry turns. Lessons live
    /// at `MemoryLayer::Semantic` and are filtered separately from generic
    /// recall when `lesson_inject` is on.
    Lesson,
    /// Durable user-stated preference (style, tone, format, workflow). Lives at
    /// `MemoryLayer::Semantic`, is strictly per-user (`identity_id`), and is
    /// always injected for the user it belongs to — filtered out of generic
    /// recall to avoid double-rendering.
    Preference,
}

impl MemorySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Human => "human",
            Self::Technical => "technical",
            Self::Curated => "curated",
            Self::Compaction => "compaction",
            Self::Agent => "agent",
            Self::Passive => "passive",
            Self::Lesson => "lesson",
            Self::Preference => "preference",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "human" => Self::Human,
            "technical" => Self::Technical,
            "curated" => Self::Curated,
            "compaction" => Self::Compaction,
            "agent" => Self::Agent,
            "passive" => Self::Passive,
            "lesson" => Self::Lesson,
            "preference" => Self::Preference,
            _ => Self::Unknown,
        }
    }
}

/// Unique memory identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

impl MemoryId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Memory layers as defined in the architecture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayer {
    /// Single conversation context (session lifetime).
    Working,
    /// Per-agent long-term experiences.
    Episodic,
    /// Domain knowledge, codebase understanding.
    Semantic,
    /// Shared across a team.
    Team,
    /// Global organisational knowledge.
    Organisation,
}

/// Who can access a memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Only the owner agent.
    #[default]
    Private,
    /// All members of a specific team.
    Team(String),
    /// Everyone in the organisation.
    Global,
}

/// A search result with relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub memory: Memory,
    /// Similarity/relevance score from the vector search (0.0 to 1.0).
    pub score: f32,
}

/// Filters for memory search.
#[derive(Debug, Clone, Default)]
pub struct MemoryFilter {
    /// Filter by memory layer.
    pub layer: Option<MemoryLayer>,
    /// Filter by owner.
    pub owner: Option<AgentId>,
    /// Filter by visibility.
    pub visibility: Option<Visibility>,
    /// Filter by tags (any match).
    pub tags: Option<Vec<String>>,
    /// Tags whose presence on a memory should exclude it from results.
    ///
    /// Applied AFTER the underlying recall: the store fetches a small
    /// over-pull of candidates, then drops any whose tag set intersects
    /// with this list. Default-empty. The store layer additionally applies
    /// audit-pollution exclusion automatically (see `include_polluted`)
    /// — this field is for arbitrary caller-defined extra exclusions.
    pub excluded_tags: Vec<String>,
    /// When `false` (the default), the store layer transparently appends
    /// the audit-pollution kind tags (`kind:recap_artifact`,
    /// `kind:consolidation_meta`) to the in-flight exclusion set so
    /// recall and list never surface memories the audit has flagged as
    /// polluted. Forensic callers (audit tooling, explicit "show me the
    /// quarantined data" queries) opt out by setting this to `true`.
    ///
    /// Default-safe: any new caller using `..Default::default()` is
    /// automatically protected without having to know about the kind tags.
    pub include_polluted: bool,
    /// Minimum importance score.
    pub min_importance: Option<f32>,
    /// Filter by memory source.
    pub source: Option<MemorySource>,
    /// Filter by canonical identity UUID.
    pub identity_id: Option<String>,
    /// Explicit opt-in for system queries that legitimately span all users
    /// (consolidation-side merges, audit tooling, migration jobs). When
    /// `false` and `identity_id` is `None`, the store returns no results —
    /// the fail-closed posture introduced by DW-518 to prevent unscoped
    /// recall when an entry path forgets to set `CURRENT_USER_ID`.
    ///
    /// **Do not flip this on lightly.** Any site setting `true` is opting
    /// out of per-user isolation and must be reviewable as such.
    pub system_unscoped: bool,
    /// Include team-visible memories for this team ID.
    /// When set alongside `identity_id`, recall returns the user's own memories
    /// plus memories with `Visibility::Team(team_id)`.
    pub team_id: Option<String>,
    /// Only return memories created at or after this time.
    pub created_after: Option<DateTime<Utc>>,
    /// Only return memories created at or before this time.
    pub created_before: Option<DateTime<Utc>>,
    /// Only return memories whose `event_time` is at or after this time.
    /// Use this (not `created_after`) for "what happened on day X" queries.
    pub event_after: Option<DateTime<Utc>>,
    /// Only return memories whose `event_time` is at or before this time.
    pub event_before: Option<DateTime<Utc>>,
    /// Maximum number of results.
    pub limit: Option<usize>,
    /// Minimum semantic-search certainty for `recall` results (cosine
    /// similarity in `[0, 1]`). `None` returns the top-N regardless of
    /// relevance — the legacy behaviour. `Some(0.7)` filters weak matches
    /// Weaviate-side so the agent sees only signal. The threshold is
    /// plumbed through to Weaviate's `nearText { certainty: X }`.
    pub min_certainty: Option<f32>,
    /// When `Some(alpha)`, `recall` uses Weaviate **hybrid** search (BM25 +
    /// vector) instead of pure `nearText`, with this `alpha` (0 = keyword
    /// only, 1 = vector only). Hybrid surfaces lessons that share keywords
    /// with the query even when the embedding has drifted — the fix for
    /// trigger-conditioned lessons that semantic-only recall misses. Result
    /// scores then come from `_additional.score` (relative-fusion, ~[0,1])
    /// rather than cosine `certainty`. `None` (default) preserves the legacy
    /// pure-vector path. Only the Weaviate backend honours this; others ignore it.
    pub hybrid_alpha: Option<f32>,
}

impl MemoryFilter {
    /// Construct a filter that explicitly opts out of per-user identity
    /// scoping — for system jobs (consolidation, audit, migration) that
    /// legitimately read across users. Prefer setting `identity_id`
    /// whenever the caller is acting on behalf of a specific user.
    pub fn system_unscoped() -> Self {
        Self {
            system_unscoped: true,
            ..Self::default()
        }
    }
}

impl Memory {
    /// Create a new memory entry.
    pub fn new(content: impl Into<String>, layer: MemoryLayer, owner: AgentId) -> Self {
        let now = Utc::now();
        Self {
            id: MemoryId::new(),
            content: content.into(),
            layer,
            owner,
            visibility: Visibility::default(),
            importance: 0.5,
            tags: Vec::new(),
            source: MemorySource::default(),
            identity_id: None,
            source_session_id: None,
            source_message_ids: Vec::new(),
            created_at: now,
            event_time: now,
            last_accessed: now,
            access_count: 0,
            recurrence_count: 0,
            last_recurrence_at: None,
            deleted: false,
        }
    }

    /// Override the event time. Use this for passively-extracted memories
    /// where the depicted event happened earlier than the write.
    pub fn with_event_time(mut self, event_time: DateTime<Utc>) -> Self {
        self.event_time = event_time;
        self
    }

    /// Set source.
    pub fn with_source(mut self, source: MemorySource) -> Self {
        self.source = source;
        self
    }

    /// Set identity ID.
    pub fn with_identity_id(mut self, identity_id: impl Into<String>) -> Self {
        self.identity_id = Some(identity_id.into());
        self
    }

    /// Set visibility.
    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Set importance.
    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = importance.clamp(0.0, 1.0);
        self
    }

    /// Add tags.
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Link this memory back to the session that produced it.
    pub fn with_source_session(
        mut self,
        session_id: impl Into<String>,
        message_ids: Vec<String>,
    ) -> Self {
        self.source_session_id = Some(session_id.into());
        self.source_message_ids = message_ids;
        self
    }

    /// The attribution note for a memory surfacing in another agent's
    /// context: `Some("from agent \"sarah\"")` when `current_agent` is not
    /// the owner, `None` for the agent's own memories. Every injection
    /// surface renders this one phrase (placement varies per surface) so a
    /// reading agent never absorbs a teammate's memory as its own.
    pub fn origin_label(&self, current_agent: &AgentId) -> Option<String> {
        (self.owner != *current_agent).then(|| format!("from agent \"{}\"", self.owner))
    }

    /// Record an access.
    pub fn record_access(&mut self) {
        self.last_accessed = Utc::now();
        self.access_count += 1;
    }

    /// Soft delete.
    pub fn soft_delete(&mut self) {
        self.deleted = true;
    }

    /// Recency score using exponential decay from `event_time`.
    ///
    /// Anchored on event time (when the depicted event happened), not
    /// `created_at` (when the record was written) — so a session-scanner
    /// backfill of last week's turn correctly reads as old.
    ///
    /// Returns 1.0 for brand new memories, approaching 0.0 for very old ones.
    /// The `decay_rate` parameter (λ) controls the speed of decay:
    /// - Default 0.005 gives a half-life of ~5.8 days
    /// - Higher values = faster decay
    pub fn recency_score(&self, decay_rate: f64) -> f32 {
        let hours = (Utc::now() - self.event_time).num_seconds() as f64 / 3600.0;
        let hours = hours.max(0.0);
        ((-decay_rate * hours).exp()) as f32
    }

    /// Human-readable age label anchored on `event_time` (see `recency_score`).
    ///
    /// Examples: "just now", "3 minutes ago", "2 hours ago", "5 days ago"
    pub fn age_label(&self) -> String {
        humanize_age(Utc::now() - self.event_time)
    }
}

/// Convert a `chrono::Duration` into a human-readable age string.
/// Kept as an inline helper to avoid cross-crate dependency on `workforce_common`.
fn humanize_age(duration: chrono::Duration) -> String {
    let total_seconds = duration.num_seconds().max(0);

    if total_seconds < 60 {
        return "just now".to_string();
    }

    let minutes = total_seconds / 60;
    if minutes < 60 {
        return if minutes == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{} minutes ago", minutes)
        };
    }

    let hours = minutes / 60;
    if hours < 24 {
        return if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{} hours ago", hours)
        };
    }

    let days = hours / 24;
    if days < 14 {
        return if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{} days ago", days)
        };
    }

    let weeks = days / 7;
    if weeks == 1 {
        "1 week ago".to_string()
    } else {
        format!("{} weeks ago", weeks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy JSON predates `event_time`; deserialisation must anchor it to
    /// `created_at`, never to the wallclock at parse time — a "now" default
    /// re-dates months-old content to the moment a consolidation/import job
    /// happens to read it.
    #[test]
    fn legacy_json_event_time_falls_back_to_created_at() {
        use chrono::Duration;
        let mut mem = Memory::new(
            "bedtime story",
            MemoryLayer::Episodic,
            AgentId::new("allan"),
        );
        mem.created_at = Utc::now() - Duration::days(60);
        let mut json = serde_json::to_value(&mem).unwrap();
        json.as_object_mut().unwrap().remove("event_time");
        let back: Memory = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.event_time, mem.created_at,
            "must anchor to created_at, not deserialisation wallclock"
        );
    }

    #[test]
    fn explicit_event_time_survives_deserialisation() {
        use chrono::Duration;
        let mut mem = Memory::new("m", MemoryLayer::Episodic, AgentId::new("allan"));
        mem.event_time = Utc::now() - Duration::days(30);
        let json = serde_json::to_string(&mem).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_time, mem.event_time);
        assert_eq!(back.created_at, mem.created_at);
    }

    #[test]
    fn create_memory() {
        let mem = Memory::new(
            "The database schema uses UUID primary keys",
            MemoryLayer::Semantic,
            AgentId::new("dev-1"),
        )
        .with_importance(0.8)
        .with_tags(vec!["database".to_string(), "schema".to_string()])
        .with_visibility(Visibility::Team("backend".to_string()));

        assert_eq!(mem.layer, MemoryLayer::Semantic);
        assert_eq!(mem.importance, 0.8);
        assert_eq!(mem.tags.len(), 2);
        assert!(!mem.deleted);
        assert_eq!(mem.access_count, 0);
    }

    #[test]
    fn importance_clamped() {
        let mem = Memory::new("test", MemoryLayer::Working, AgentId::new("a")).with_importance(1.5);
        assert_eq!(mem.importance, 1.0);

        let mem =
            Memory::new("test", MemoryLayer::Working, AgentId::new("a")).with_importance(-0.5);
        assert_eq!(mem.importance, 0.0);
    }

    #[test]
    fn record_access() {
        let mut mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        assert_eq!(mem.access_count, 0);
        mem.record_access();
        assert_eq!(mem.access_count, 1);
        mem.record_access();
        assert_eq!(mem.access_count, 2);
    }

    #[test]
    fn serialization_roundtrip() {
        let mem = Memory::new("important fact", MemoryLayer::Team, AgentId::new("dev"))
            .with_visibility(Visibility::Global);
        let json = serde_json::to_string(&mem).unwrap();
        let deserialized: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.content, "important fact");
        assert_eq!(deserialized.visibility, Visibility::Global);
    }

    #[test]
    fn serialization_roundtrip_with_source_link() {
        let mem = Memory::new(
            "we talked about it",
            MemoryLayer::Episodic,
            AgentId::new("a"),
        )
        .with_source_session("sess-1", vec!["m-1".into(), "m-2".into()]);
        let json = serde_json::to_string(&mem).unwrap();
        let deserialized: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.source_session_id.as_deref(), Some("sess-1"));
        assert_eq!(deserialized.source_message_ids, vec!["m-1", "m-2"]);
    }

    #[test]
    fn deserialises_legacy_memory_without_source_fields() {
        // Pre-source-link payload: source_session_id / source_message_ids absent.
        let json = r#"{
            "id": "mem-legacy",
            "content": "old memory",
            "layer": "episodic",
            "owner": "a",
            "visibility": "private",
            "importance": 0.5,
            "created_at": "2026-01-01T00:00:00Z",
            "last_accessed": "2026-01-01T00:00:00Z"
        }"#;
        let mem: Memory = serde_json::from_str(json).expect("legacy payload should deserialise");
        assert_eq!(mem.content, "old memory");
        assert!(mem.source_session_id.is_none());
        assert!(mem.source_message_ids.is_empty());
    }

    #[test]
    fn source_link_fields_omitted_when_empty() {
        let mem = Memory::new("x", MemoryLayer::Episodic, AgentId::new("a"));
        let json = serde_json::to_string(&mem).unwrap();
        assert!(!json.contains("source_session_id"));
        assert!(!json.contains("source_message_ids"));
    }

    #[test]
    fn recency_score_brand_new() {
        let mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        let score = mem.recency_score(0.005);
        // Brand new memory should have score very close to 1.0
        assert!(score > 0.99, "Expected > 0.99, got {}", score);
    }

    #[test]
    fn recency_score_decays() {
        use chrono::Duration;
        let mut mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        mem.event_time = Utc::now() - Duration::days(7);
        let score = mem.recency_score(0.005);
        // After 7 days at λ=0.005: e^(-0.005 * 168) ≈ 0.43
        assert!(score < 0.5, "Expected < 0.5, got {}", score);
        assert!(score > 0.3, "Expected > 0.3, got {}", score);
    }

    #[test]
    fn recency_score_custom_decay() {
        use chrono::Duration;
        let mut mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        mem.event_time = Utc::now() - Duration::days(1);
        let fast_decay = mem.recency_score(0.1);
        let slow_decay = mem.recency_score(0.001);
        assert!(
            fast_decay < slow_decay,
            "Fast decay should produce lower score"
        );
    }

    #[test]
    fn age_label_recent() {
        let mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        assert_eq!(mem.age_label(), "just now");
    }

    #[test]
    fn age_label_old() {
        use chrono::Duration;
        let mut mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        mem.event_time = Utc::now() - Duration::days(3);
        assert_eq!(mem.age_label(), "3 days ago");
    }

    #[test]
    fn recency_anchors_on_event_time_not_created_at() {
        // A session-scanner backfill writes today (created_at = now) but
        // the depicted event happened a week ago (event_time = -7d). The
        // memory should read as a week-old, not brand new.
        use chrono::Duration;
        let mut mem = Memory::new("test", MemoryLayer::Episodic, AgentId::new("a"));
        mem.created_at = Utc::now();
        mem.event_time = Utc::now() - Duration::days(7);
        assert!(mem.age_label().contains("week") || mem.age_label().contains("day"));
        assert!(mem.recency_score(0.005) < 0.5);
    }
}
