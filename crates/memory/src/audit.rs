//! Audit + classification of polluted memory records.
//!
//! Used by the `workforce memory clean` CLI to identify memories that should
//! be re-tagged so future recalls exclude them. Two classes are detected:
//!
//! * **`recap_artifact`** — memories that were minted from a turn where the
//!   user asked the agent to summarise / recall prior conversation. The
//!   live extraction pipeline now refuses these (see `passive_memory::gate`),
//!   but historical data accumulated thousands of them. Detected
//!   structurally: clusters of ≥3 memories from the same source session
//!   created within a few seconds — the LLM having emitted multiple events
//!   in one extraction call from a single recap turn.
//!
//! * **`consolidation_meta`** — memories minted from the consolidation
//!   agent's own turns ("I performed memory consolidation, creating 3
//!   semantic memories…"). Live extraction now skips this channel, but
//!   historical entries match a stable content signature.
//!
//! The classification operates on raw Weaviate export objects (the same
//! shape produced by `WeaviateStore::export_raw_objects`) so the audit can
//! run without round-tripping through `Memory` parsing — and so the
//! classifier is testable on hand-written fixtures with no live store.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use workforce_common::settings::DerivedFigureVocabulary;

/// Tag applied to memories the audit classifies as a recap-extraction
/// artefact. Recall callers can include this in `MemoryFilter.excluded_tags`
/// to keep polluted records out of normal queries without deleting them.
pub const RECAP_ARTIFACT_KIND: &str = "kind:recap_artifact";
/// Tag applied to memories the audit classifies as a consolidation-agent
/// self-report. Same usage as [`RECAP_ARTIFACT_KIND`].
pub const CONSOLIDATION_META_KIND: &str = "kind:consolidation_meta";
/// Tag applied to memories that store a derived, point-in-time figure (a
/// count, total or metric computed from a query) as if it were a durable
/// fact. Such numbers go stale the moment the data changes; recalling one
/// re-serves a wrong answer forever. Same usage as [`RECAP_ARTIFACT_KIND`].
pub const DERIVED_FIGURE_KIND: &str = "kind:derived_figure";

/// Every `kind:` tag the audit can write. Use as the canonical source of
/// "polluted" markers when building exclusion filters — no caller should
/// hard-code the tags individually.
pub const POLLUTED_KIND_TAGS: &[&str] = &[
    RECAP_ARTIFACT_KIND,
    CONSOLIDATION_META_KIND,
    DERIVED_FIGURE_KIND,
];

/// `POLLUTED_KIND_TAGS` materialised as `Vec<String>` for use with
/// `MemoryFilter.excluded_tags`.
pub fn polluted_kind_tags_owned() -> Vec<String> {
    POLLUTED_KIND_TAGS.iter().map(|s| s.to_string()).collect()
}

/// Minimum cluster size for the recap-artifact heuristic. A turn that
/// extracts a single event into a single memory is a normal turn; recap
/// turns reliably produce many bullets at once.
pub const RECAP_CLUSTER_MIN_SIZE: usize = 3;

/// Time window within which memories from the same session are considered
/// part of the same extraction cluster.
pub const RECAP_CLUSTER_WINDOW: Duration = Duration::seconds(5);

/// A broader detector for retry amplification: when the agent is stuck
/// repeating the same situation across many turns (e.g. user retrying a
/// broken tool, or the agent looping on a failed action), each turn legally
/// produces 1–2 memories, but the resulting cluster is dozens of
/// near-duplicate "the user asked X, I couldn't X" entries spread over a
/// few minutes.
///
/// Picked deliberately conservatively: 10 memories in 5 minutes from one
/// session is hard to hit in normal conversation (≈ one memory every 30 s
/// for five straight minutes), so this threshold catches retry storms while
/// leaving healthy back-and-forth alone. Tag-shared with `RecapArtifact`
/// so the recall path doesn't need to learn a new exclusion.
pub const AMPLIFICATION_CLUSTER_MIN_SIZE: usize = 10;
pub const AMPLIFICATION_CLUSTER_WINDOW: Duration = Duration::minutes(5);

/// Why the audit flagged a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Cluster of ≥`RECAP_CLUSTER_MIN_SIZE` memories from the same session
    /// within `RECAP_CLUSTER_WINDOW` — almost certainly extracted from a
    /// single recap turn.
    RecapArtifact,
    /// Content matches a consolidation-self-report signature.
    ConsolidationMeta,
    /// Content stores a derived, point-in-time figure (a count, total,
    /// percentage or other query-derived metric) as a durable fact.
    DerivedFigure,
}

impl Classification {
    pub fn as_kind_tag(self) -> &'static str {
        match self {
            Self::RecapArtifact => RECAP_ARTIFACT_KIND,
            Self::ConsolidationMeta => CONSOLIDATION_META_KIND,
            Self::DerivedFigure => DERIVED_FIGURE_KIND,
        }
    }
}

/// One classified memory in the audit output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Weaviate object UUID (from the export's top-level `id`).
    pub weaviate_id: String,
    /// `properties.memoryId` — the application-level ID.
    pub memory_id: String,
    /// `properties.sourceSessionId` — None when absent or empty.
    pub source_session_id: Option<String>,
    /// `properties.createdAt`.
    pub created_at: DateTime<Utc>,
    /// Why this memory was flagged.
    pub classification: Classification,
    /// First ~200 chars of content, for the human-readable manifest.
    pub content_preview: String,
}

/// Summary returned alongside `findings`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditSummary {
    pub total_scanned: usize,
    pub recap_artifact_count: usize,
    pub consolidation_meta_count: usize,
    #[serde(default)]
    pub derived_figure_count: usize,
}

/// Classify a slice of raw Weaviate export objects.
///
/// Returns one `Finding` per flagged memory. A memory matching multiple
/// classes (e.g. a consolidation-meta entry that's also part of a recap
/// cluster) is reported once per class — let the caller decide which
/// kind tag to apply (apply both, in fact).
pub fn classify(raw_objects: &[serde_json::Value]) -> (Vec<Finding>, AuditSummary) {
    classify_with(raw_objects, &DEFAULT_DERIVED_FIGURES)
}

/// [`classify`] with a deployment's own derived-figure vocabulary.
pub fn classify_with(
    raw_objects: &[serde_json::Value],
    derived_figures: &DerivedFigureDetector,
) -> (Vec<Finding>, AuditSummary) {
    let mut findings = Vec::new();
    let mut summary = AuditSummary {
        total_scanned: raw_objects.len(),
        ..Default::default()
    };
    let mut already_recap: std::collections::HashSet<String> = std::collections::HashSet::new();

    let by_session = group_by_session(raw_objects);

    // Pass 1a: tight in-burst recap clusters (one extraction call producing
    // many bullets — classic "summarise the past 2 weeks" turn).
    detect_session_clusters(
        &by_session,
        RECAP_CLUSTER_MIN_SIZE,
        RECAP_CLUSTER_WINDOW,
        &mut findings,
        &mut summary.recap_artifact_count,
        &mut already_recap,
    );

    // Pass 1b: retry-amplification clusters spread over minutes (many turns
    // hammering on the same broken tool). Wider window, higher threshold so
    // healthy back-and-forth doesn't trip it.
    detect_session_clusters(
        &by_session,
        AMPLIFICATION_CLUSTER_MIN_SIZE,
        AMPLIFICATION_CLUSTER_WINDOW,
        &mut findings,
        &mut summary.recap_artifact_count,
        &mut already_recap,
    );

    // Pass 2: per-object content match for consolidation-meta detection.
    for obj in raw_objects {
        let content = obj["properties"]["content"].as_str().unwrap_or("");
        if matches_consolidation_meta(content) {
            if let Some(f) = make_finding(obj, Classification::ConsolidationMeta) {
                summary.consolidation_meta_count += 1;
                findings.push(f);
            }
        }
    }

    // Pass 3: per-object content match for stored derived figures — the same
    // detector the live write paths use, so the sweep and the filter can't
    // drift apart.
    for obj in raw_objects {
        let content = obj["properties"]["content"].as_str().unwrap_or("");
        if derived_figures.matches(content) {
            if let Some(f) = make_finding(obj, Classification::DerivedFigure) {
                summary.derived_figure_count += 1;
                findings.push(f);
            }
        }
    }

    (findings, summary)
}

/// Patterns that identify a memory as a self-report from the consolidation
/// agent. Conservative — must be a clear first-person statement of having
/// run consolidation, not just any mention of memory.
static CONSOLIDATION_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        // "I (performed|ran|completed|did) (memory )?consolidation"
        r"(?i)\bI (performed|ran|completed|did|conducted) (the )?(memory )?consolidation\b",
        // "I (created|merged|consolidated) N (episodic|semantic) memories"
        r"(?i)\bI (created|merged|consolidated|combined) (\d+|several|a few|multiple) (episodic|semantic) memor",
        // "memory consolidation (task|run|process|job)" with first-person framing
        r"(?i)\bmemory consolidation (task|run|process|job)\b",
        // "soft-deleted N (redundant )?(episodic )?memories" / "forgot N memories" — agent reporting cleanup.
        r"(?i)\b(soft-deleted|forgot) (\d+|several) (redundant )?(episodic )?(memor|fragment)",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("static consolidation pattern compiles"))
    .collect()
});

fn matches_consolidation_meta(content: &str) -> bool {
    let head: String = content.chars().take(800).collect();
    CONSOLIDATION_PATTERNS.iter().any(|re| re.is_match(&head))
}

/// Status words allowed between a number and its aggregate noun.
const FIGURE_QUALIFIERS: &[&str] = &[
    "open",
    "closed",
    "active",
    "inactive",
    "dormant",
    "available",
    "booked",
    "cancelled",
    "pending",
    "approved",
    "rejected",
    "missing",
    "overdue",
    "expired",
    "new",
    "failed",
    "completed",
    "outstanding",
    "unfulfilled",
    "unpaid",
    "paid",
];

/// Aggregate nouns, as regex fragments covering their plurals.
const FIGURE_NOUNS: &[&str] = &[
    "workers?",
    "employees?",
    "staff",
    "users?",
    "customers?",
    "accounts?",
    "members?",
    "people",
    "records?",
    "rows?",
    "documents?",
    "files?",
    "items?",
    "orders?",
    "invoices?",
    "payments?",
    "transactions?",
    "tickets?",
    "issues?",
    "requests?",
    "bookings?",
    "tasks?",
    "projects?",
    "claims?",
    "expenses?",
    "organisations?",
    "organizations?",
    "departments?",
    "teams?",
    "messages?",
    r"memor(?:y|ies)",
];

/// Keys of a minted aggregate fact line ("count: 23").
const FIGURE_FACT_KEYS: &[&str] = &[
    "count",
    "total",
    "number",
    "figure",
    "sum",
    "percentage",
    "rate",
    "utilisation",
];

/// Recognises memories that carry a derived, point-in-time figure — a count,
/// total, percentage or other metric computed from a query.
///
/// Deliberately aimed at AGGREGATE claims, not the presence of digits: entity
/// IDs (`order:4815162`), absolute dates (`on 2026-07-08`) and names with
/// numbers in them are legitimate memory content and must not match. The
/// discriminator is a number quantifying an aggregate noun, or explicit
/// total/count phrasing, or a minted aggregate fact line.
///
/// The nouns, qualifiers and fact keys are generic; a deployment adds its own
/// domain words through `[memory.derived_figures]`.
#[derive(Debug, Clone)]
pub struct DerivedFigureDetector {
    patterns: Vec<Regex>,
}

impl DerivedFigureDetector {
    pub fn new(vocabulary: &DerivedFigureVocabulary) -> Self {
        let words = |defaults: &[&str], extra: &[String], plural: bool| {
            defaults
                .iter()
                .map(|w| w.to_string())
                .chain(
                    extra
                        .iter()
                        .map(|w| w.trim())
                        .filter(|w| !w.is_empty())
                        .map(|w| {
                            let word = regex::escape(w).replace(' ', r"\s+");
                            if plural {
                                format!("{word}s?")
                            } else {
                                word
                            }
                        }),
                )
                .collect::<Vec<_>>()
                .join("|")
        };
        let qualifiers = words(FIGURE_QUALIFIERS, &vocabulary.qualifiers, false);
        let nouns = words(FIGURE_NOUNS, &vocabulary.nouns, true);
        let fact_keys = words(FIGURE_FACT_KEYS, &vocabulary.fact_keys, false);
        let patterns = [
            // A number immediately quantifying an aggregate noun, allowing up to
            // two known status words between: "197 workers", "23 unfulfilled
            // orders", "12 pending invoices". A curated qualifier list (not \w+)
            // keeps "order 4815162 and worker James" from matching.
            format!(r"(?i)\b\d[\d,]*\s+(?:(?:{qualifiers})\s+){{0,2}}(?:{nouns})\b"),
            // Explicit aggregate phrasing, connector required so an entity label
            // like "catalogue number 4" can't match: "a total of 175", "number of
            // workers: 197".
            r"(?i)\b(total|count|number)\s+of\b[^.\n]{0,40}?\d".to_string(),
            // "the count is 9", "the true total was 197", "total: 42".
            r"(?i)\b(total|count)\s*(is|was|were|are|:|=|came\s+to)\s*\d".to_string(),
            // A minted aggregate fact line: "count:23", "- total: 175". Entity
            // facts ("order:4815162") don't match — the key must itself be an
            // aggregate word.
            format!(r"(?im)^\s*-?\s*(?:{fact_keys})\s*[:=]\s*\d"),
            // Percentages are point-in-time metrics by nature: "85% on time".
            r"(?i)\b\d{1,3}(?:\.\d+)?\s*%".to_string(),
        ];
        Self {
            patterns: patterns
                .iter()
                .map(|p| Regex::new(p).expect("escaped derived-figure pattern compiles"))
                .collect(),
        }
    }

    /// True when `content` stores a derived, point-in-time figure as if it
    /// were a durable fact.
    pub fn matches(&self, content: &str) -> bool {
        let head: String = content.chars().take(1200).collect();
        self.patterns.iter().any(|re| re.is_match(&head))
    }
}

impl Default for DerivedFigureDetector {
    fn default() -> Self {
        Self::new(&DerivedFigureVocabulary::default())
    }
}

static DEFAULT_DERIVED_FIGURES: Lazy<DerivedFigureDetector> =
    Lazy::new(DerivedFigureDetector::default);

/// [`DerivedFigureDetector::matches`] with the generic vocabulary. Shared by
/// the live write-path filters and the audit sweep, so what the filter refuses
/// and what the sweep flags stay the same thing.
pub fn looks_like_derived_figure(content: &str) -> bool {
    DEFAULT_DERIVED_FIGURES.matches(content)
}

fn group_by_session(
    raw_objects: &[serde_json::Value],
) -> BTreeMap<String, Vec<&serde_json::Value>> {
    let mut by_session: BTreeMap<String, Vec<&serde_json::Value>> = BTreeMap::new();
    for obj in raw_objects {
        if let Some(sid) = obj["properties"]["sourceSessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            by_session.entry(sid.to_string()).or_default().push(obj);
        }
    }
    by_session
}

fn created_at_of(obj: &serde_json::Value) -> DateTime<Utc> {
    obj["properties"]["createdAt"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

/// Sweep each session's timeline with the given window, flagging any
/// non-overlapping run of `min_size` or more memories as a recap-artifact
/// cluster. `already_flagged` tracks memory IDs to avoid double-counting
/// when the same memory falls into both the tight and the wide window.
fn detect_session_clusters(
    by_session: &BTreeMap<String, Vec<&serde_json::Value>>,
    min_size: usize,
    window: Duration,
    findings: &mut Vec<Finding>,
    counter: &mut usize,
    already_flagged: &mut std::collections::HashSet<String>,
) {
    for group in by_session.values() {
        let mut sorted: Vec<&serde_json::Value> = group.to_vec();
        sorted.sort_by_key(|o| created_at_of(o));
        let stamps: Vec<DateTime<Utc>> = sorted.iter().map(|o| created_at_of(o)).collect();

        let mut i = 0;
        while i < sorted.len() {
            let mut j = i + 1;
            while j < sorted.len() && stamps[j] - stamps[i] <= window {
                j += 1;
            }
            if j - i >= min_size {
                for obj in &sorted[i..j] {
                    let Some(mid) = obj["properties"]["memoryId"].as_str() else {
                        continue;
                    };
                    if !already_flagged.insert(mid.to_string()) {
                        continue;
                    }
                    if let Some(f) = make_finding(obj, Classification::RecapArtifact) {
                        *counter += 1;
                        findings.push(f);
                    }
                }
            }
            i = j;
        }
    }
}

fn make_finding(obj: &serde_json::Value, classification: Classification) -> Option<Finding> {
    let weaviate_id = obj["id"].as_str()?.to_string();
    let memory_id = obj["properties"]["memoryId"].as_str()?.to_string();
    let source_session_id = obj["properties"]["sourceSessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let created_at = obj["properties"]["createdAt"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let content = obj["properties"]["content"].as_str().unwrap_or("");
    let preview: String = content.chars().take(200).collect();
    Some(Finding {
        weaviate_id,
        memory_id,
        source_session_id,
        created_at,
        classification,
        content_preview: preview,
    })
}

/// Compute the new tags for a memory after applying a classification:
/// returns `None` when the kind tag is already present (no-op), else
/// `Some(new_tags)` ready to PATCH.
pub fn tags_with_kind(existing: &[String], classification: Classification) -> Option<Vec<String>> {
    let kind = classification.as_kind_tag();
    if existing.iter().any(|t| t == kind) {
        return None;
    }
    let mut tags = existing.to_vec();
    tags.push(kind.to_string());
    Some(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(
        id: &str,
        memory_id: &str,
        session: Option<&str>,
        created_at: &str,
        content: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "properties": {
                "memoryId": memory_id,
                "sourceSessionId": session.unwrap_or(""),
                "createdAt": created_at,
                "content": content,
            }
        })
    }

    #[test]
    fn recap_cluster_flags_three_or_more_within_window() {
        let raws = vec![
            obj(
                "w1",
                "m1",
                Some("s-A"),
                "2026-05-02T06:42:40.10Z",
                "first event",
            ),
            obj(
                "w2",
                "m2",
                Some("s-A"),
                "2026-05-02T06:42:40.20Z",
                "second event",
            ),
            obj(
                "w3",
                "m3",
                Some("s-A"),
                "2026-05-02T06:42:40.30Z",
                "third event",
            ),
            obj(
                "w4",
                "m4",
                Some("s-A"),
                "2026-05-02T06:42:40.40Z",
                "fourth event",
            ),
        ];
        let (findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 4);
        assert_eq!(findings.len(), 4);
        assert!(findings
            .iter()
            .all(|f| f.classification == Classification::RecapArtifact));
    }

    #[test]
    fn recap_cluster_does_not_flag_lone_memory() {
        let raws = vec![obj(
            "w1",
            "m1",
            Some("s-A"),
            "2026-05-02T06:42:40Z",
            "a single event",
        )];
        let (findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 0);
        assert!(findings.is_empty());
    }

    #[test]
    fn recap_cluster_does_not_flag_two_close_memories() {
        // Two from the same turn is normal — a turn that covers two distinct
        // topics naturally extracts two events. Threshold is ≥3.
        let raws = vec![
            obj("w1", "m1", Some("s-A"), "2026-05-02T06:42:40.0Z", "a"),
            obj("w2", "m2", Some("s-A"), "2026-05-02T06:42:40.5Z", "b"),
        ];
        let (_findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 0);
    }

    #[test]
    fn recap_cluster_does_not_flag_spaced_memories() {
        // Three memories from the same session, but minutes apart — those
        // are three separate turns in the same conversation, not one recap.
        let raws = vec![
            obj("w1", "m1", Some("s-A"), "2026-05-02T06:00:00Z", "a"),
            obj("w2", "m2", Some("s-A"), "2026-05-02T06:10:00Z", "b"),
            obj("w3", "m3", Some("s-A"), "2026-05-02T06:20:00Z", "c"),
        ];
        let (_findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 0);
    }

    #[test]
    fn amplification_cluster_flags_long_retry_storms() {
        // 12 memories spread evenly over ~4 minutes from one session: too
        // dense for healthy conversation, too spread out for the tight
        // recap-cluster rule. The amplification rule (≥10 in 5 minutes)
        // catches exactly this shape.
        let raws: Vec<_> = (0..12)
            .map(|i| {
                let total_secs = i * 20;
                let stamp = format!(
                    "2026-05-02T03:{:02}:{:02}Z",
                    total_secs / 60,
                    total_secs % 60
                );
                obj(
                    &format!("w{i}"),
                    &format!("m{i}"),
                    Some("s-A"),
                    &stamp,
                    "user asked me to run knowledge.index, I couldn't",
                )
            })
            .collect();
        let (_findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 12);
    }

    #[test]
    fn amplification_cluster_does_not_double_count_with_recap_cluster() {
        // 10 memories in a tight burst — the tight rule fires first, the
        // wide rule should not re-flag the same memories.
        let raws: Vec<_> = (0..10)
            .map(|i| {
                obj(
                    &format!("w{i}"),
                    &format!("m{i}"),
                    Some("s-A"),
                    &format!("2026-05-02T03:00:00.{:03}Z", i * 100),
                    "x",
                )
            })
            .collect();
        let (findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 10);
        assert_eq!(
            findings.len(),
            10,
            "no duplicate findings across the two cluster rules"
        );
    }

    #[test]
    fn amplification_cluster_does_not_flag_steady_conversation() {
        // 8 memories over 7 minutes — below the amplification threshold
        // (10) and not tight enough for the recap rule. Healthy chat pace.
        let raws: Vec<_> = (0..8)
            .map(|i| {
                obj(
                    &format!("w{i}"),
                    &format!("m{i}"),
                    Some("s-A"),
                    &format!("2026-05-02T03:{:02}:00Z", i),
                    "different topic",
                )
            })
            .collect();
        let (_findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 0);
    }

    #[test]
    fn recap_cluster_does_not_cross_sessions() {
        // Three near-simultaneous memories but from different sessions —
        // could be three concurrent users, never the same recap turn.
        let raws = vec![
            obj("w1", "m1", Some("s-A"), "2026-05-02T06:42:40.0Z", "a"),
            obj("w2", "m2", Some("s-B"), "2026-05-02T06:42:40.1Z", "b"),
            obj("w3", "m3", Some("s-C"), "2026-05-02T06:42:40.2Z", "c"),
        ];
        let (_findings, summary) = classify(&raws);
        assert_eq!(summary.recap_artifact_count, 0);
    }

    #[test]
    fn consolidation_meta_matches_first_person_self_report() {
        let raws = vec![
            obj(
                "w1",
                "m1",
                None,
                "2026-04-24T08:23:14Z",
                "I ran the memory consolidation process as instructed. I found 1,963 memories.",
            ),
            obj(
                "w2",
                "m2",
                None,
                "2026-04-24T09:00:00Z",
                "I performed memory consolidation on the user's memories, creating 3 semantic memories.",
            ),
            obj(
                "w3",
                "m3",
                None,
                "2026-04-24T09:30:00Z",
                "I created 9 episodic memories that were already covered.",
            ),
            obj(
                "w4",
                "m4",
                None,
                "2026-04-24T10:00:00Z",
                "I soft-deleted 9 redundant episodic fragments.",
            ),
        ];
        let (findings, summary) = classify(&raws);
        assert_eq!(summary.consolidation_meta_count, 4);
        // "I found 1,963 memories" also stores a derived figure, so that
        // fixture is legitimately reported once per class.
        assert_eq!(
            findings
                .iter()
                .filter(|f| f.classification == Classification::ConsolidationMeta)
                .count(),
            4
        );
    }

    #[test]
    fn consolidation_meta_does_not_match_normal_user_content() {
        let raws = vec![
            obj(
                "w1",
                "m1",
                None,
                "2026-04-24T08:00:00Z",
                "User mentioned that they want to go for a run later",
            ),
            obj(
                "w2",
                "m2",
                None,
                "2026-04-24T09:00:00Z",
                "We discussed memory architecture for the workforce project",
            ),
        ];
        let (_findings, summary) = classify(&raws);
        assert_eq!(summary.consolidation_meta_count, 0);
    }

    #[test]
    fn derived_figure_matches_aggregate_claims() {
        // The ENG-105 shapes: a carried org count, a wrong order count, a
        // minted aggregate fact, a percentage metric.
        assert!(looks_like_derived_figure(
            "The customer's organisation has 197 workers in total."
        ));
        assert!(looks_like_derived_figure(
            "On 2026-07-14 there were 23 unfulfilled orders at the depot."
        ));
        assert!(looks_like_derived_figure("I told them the count is 9."));
        assert!(looks_like_derived_figure(
            "Asked about capacity.\n\nFacts:\n- total:175"
        ));
        assert!(looks_like_derived_figure(
            "Their on-time rate was 85% last month."
        ));
        assert!(looks_like_derived_figure(
            "A total of 36 workers are missing a CV."
        ));
    }

    #[test]
    fn derived_figure_spares_entities_dates_and_identity() {
        // Entity IDs, absolute dates and identity facts are exactly what the
        // extraction prompt asks for — none of them may trip the filter.
        assert!(!looks_like_derived_figure(
            "On 2026-07-08 the customer reviewed unfulfilled bulk orders and asked \
             about order 4815162 and worker Sam Carter.\n\nFacts:\n- order:4815162\n- worker:Sam Carter"
        ));
        assert!(!looks_like_derived_figure(
            "The customer is a Staff Engineer at Contoso.\n\nFacts:\n- name:Alex\n- role:Staff Engineer"
        ));
        assert!(!looks_like_derived_figure(
            "They coordinate catalogue number 4 for the east warehouse."
        ));
        assert!(!looks_like_derived_figure(
            "Prefers order lists sorted by start date."
        ));
    }

    #[test]
    fn deployment_vocabulary_extends_the_generic_words() {
        let content = "There were 23 unshipped parcels on Monday.";
        assert!(!looks_like_derived_figure(content));

        let domain = DerivedFigureDetector::new(&DerivedFigureVocabulary {
            nouns: vec!["parcel".into(), "return request".into(), " ".into()],
            qualifiers: vec!["unshipped".into()],
            fact_keys: vec!["on-time-rate".into()],
        });
        assert!(domain.matches(content));
        assert!(domain.matches("We had 4 return  requests."));
        assert!(domain.matches("Facts:\n- on-time-rate: 85"));
        assert!(domain.matches("The customer has 197 workers."));
        assert!(!domain.matches("Parcel 4815162 is unshipped."));
        assert!(!domain.matches("Asked about 3 things."));
    }

    #[test]
    fn classify_flags_stored_derived_figures() {
        let raws = vec![
            obj(
                "w1",
                "m1",
                None,
                "2026-07-14T08:00:00Z",
                "The organisation has 197 workers in total.\n\nFacts:\n- count:197",
            ),
            obj(
                "w2",
                "m2",
                None,
                "2026-07-14T09:00:00Z",
                "The customer works on the east warehouse.",
            ),
        ];
        let (findings, summary) = classify(&raws);
        assert_eq!(summary.derived_figure_count, 1);
        let f = findings
            .iter()
            .find(|f| f.classification == Classification::DerivedFigure)
            .unwrap();
        assert_eq!(f.memory_id, "m1");
        assert_eq!(
            Classification::DerivedFigure.as_kind_tag(),
            DERIVED_FIGURE_KIND
        );
    }

    #[test]
    fn tags_with_kind_returns_none_when_already_tagged() {
        let existing = vec!["foo".to_string(), RECAP_ARTIFACT_KIND.to_string()];
        assert!(tags_with_kind(&existing, Classification::RecapArtifact).is_none());
    }

    #[test]
    fn tags_with_kind_appends_new_kind() {
        let existing = vec!["foo".to_string(), "bar".to_string()];
        let new = tags_with_kind(&existing, Classification::ConsolidationMeta).unwrap();
        assert_eq!(new.len(), 3);
        assert_eq!(new[2], CONSOLIDATION_META_KIND);
    }

    #[test]
    fn tags_with_kind_idempotent_across_classifications() {
        let existing = vec!["x".to_string()];
        let after_recap = tags_with_kind(&existing, Classification::RecapArtifact).unwrap();
        let after_meta = tags_with_kind(&after_recap, Classification::ConsolidationMeta).unwrap();
        assert!(after_meta.contains(&RECAP_ARTIFACT_KIND.to_string()));
        assert!(after_meta.contains(&CONSOLIDATION_META_KIND.to_string()));
    }
}
