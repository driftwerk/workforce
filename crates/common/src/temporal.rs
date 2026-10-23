//! Temporal Perception Layer — gives agents an intuitive sense of time passage.
//!
//! Four components:
//! 1. **Temporal Context Block** — injected at session start showing time since last conversation
//! 2. **Gap Classification** — categorises time gaps (continuation/short/overnight/multi-day/weekly/extended)
//! 3. **Staleness Warnings** — flags stale tasks, long conversation gaps
//! 4. **Sidecar** — file-based last-interaction tracking per user

use std::fmt;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Temporal perception configuration. Stored in per-agent `config.toml`
/// under the `[temporal]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalConfig {
    /// Master toggle for all temporal features.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Exponential decay rate λ for recency scoring.
    /// Higher = faster decay. Default 0.005 gives ~5.8 day half-life.
    #[serde(default = "default_decay_rate")]
    pub decay_rate: f64,

    /// Weight given to importance in combined scoring (0.0–1.0).
    #[serde(default = "default_importance_weight")]
    pub importance_weight: f64,

    /// Weight given to recency in combined scoring (0.0–1.0).
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f64,

    /// Maximum number of days to look back for daily notes on new sessions.
    #[serde(default = "default_max_lookback_days")]
    pub max_lookback_days: u32,

    /// Hours after which an active task is considered stale.
    #[serde(default = "default_stale_task_warning_hours")]
    pub stale_task_warning_hours: u32,

    /// Days after which a conversation gap triggers a warning.
    #[serde(default = "default_conversation_gap_warning_days")]
    pub conversation_gap_warning_days: u32,
}

fn default_true() -> bool {
    true
}
fn default_decay_rate() -> f64 {
    0.005
}
fn default_importance_weight() -> f64 {
    0.4
}
fn default_recency_weight() -> f64 {
    0.6
}
fn default_max_lookback_days() -> u32 {
    7
}
fn default_stale_task_warning_hours() -> u32 {
    24
}
fn default_conversation_gap_warning_days() -> u32 {
    3
}

impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decay_rate: 0.005,
            importance_weight: 0.4,
            recency_weight: 0.6,
            max_lookback_days: 7,
            stale_task_warning_hours: 24,
            conversation_gap_warning_days: 3,
        }
    }
}

// ─── Gap Classification ──────────────────────────────────────────────────────

/// Classification of the time gap since last interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapClassification {
    /// < 5 minutes — ongoing conversation
    Continuation,
    /// < 2 hours — short break
    Short,
    /// < 18 hours — overnight or work-day gap
    Overnight,
    /// < 3 days — multi-day absence
    MultiDay,
    /// < 7 days — weekly absence
    Weekly,
    /// >= 7 days — extended absence
    ExtendedAbsence,
}

impl GapClassification {
    /// Classify a duration into a gap category.
    pub fn classify(duration: Duration) -> Self {
        let minutes = duration.num_minutes();
        if minutes < 5 {
            Self::Continuation
        } else if minutes < 120 {
            Self::Short
        } else if minutes < 18 * 60 {
            Self::Overnight
        } else if minutes < 3 * 24 * 60 {
            Self::MultiDay
        } else if minutes < 7 * 24 * 60 {
            Self::Weekly
        } else {
            Self::ExtendedAbsence
        }
    }

    /// A human-friendly label for the gap.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Continuation => "Continuation",
            Self::Short => "Short break",
            Self::Overnight => "Overnight",
            Self::MultiDay => "Multi-day gap",
            Self::Weekly => "Weekly gap",
            Self::ExtendedAbsence => "Extended absence",
        }
    }

    /// A contextual note for the agent about how to adjust behaviour.
    pub fn context_note(&self) -> &'static str {
        match self {
            Self::Continuation => "Active conversation — context is fresh.",
            Self::Short => "Short break — context is mostly fresh, quick recap may help.",
            Self::Overnight => {
                "Overnight gap — review recent notes before continuing previous work."
            }
            Self::MultiDay => "Multi-day gap — check for stale tasks and catch up on what changed.",
            Self::Weekly => {
                "Weekly gap — significant time passed. Re-orient on priorities and active work."
            }
            Self::ExtendedAbsence => "Extended absence — do a thorough review of all active work, tasks, and any changes that happened while away.",
        }
    }
}

impl fmt::Display for GapClassification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ─── Duration Humanisation ───────────────────────────────────────────────────

/// Convert a `chrono::Duration` into a human-readable string.
///
/// Examples: "3 minutes", "2 hours", "1 day", "5 days", "2 weeks"
pub fn humanize_duration(duration: Duration) -> String {
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

/// Compact duration string without "ago" for inline use.
/// Examples: "3 minutes", "2 hours", "5 days"
pub fn humanize_duration_compact(duration: Duration) -> String {
    let total_seconds = duration.num_seconds().max(0);

    if total_seconds < 60 {
        return "just now".to_string();
    }

    let minutes = total_seconds / 60;
    if minutes < 60 {
        return if minutes == 1 {
            "1 minute".to_string()
        } else {
            format!("{} minutes", minutes)
        };
    }

    let hours = minutes / 60;
    if hours < 24 {
        return if hours == 1 {
            "1 hour".to_string()
        } else {
            format!("{} hours", hours)
        };
    }

    let days = hours / 24;
    if days < 14 {
        return if days == 1 {
            "1 day".to_string()
        } else {
            format!("{} days", days)
        };
    }

    let weeks = days / 7;
    if weeks == 1 {
        "1 week".to_string()
    } else {
        format!("{} weeks", weeks)
    }
}

// ─── Temporal Sidecar ────────────────────────────────────────────────────────

/// Lightweight sidecar tracking the last interaction timestamp per user.
/// Stored at `{workspace}/memory/.temporal/{owner}.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalSidecar {
    /// Timestamp of the last user message.
    pub last_interaction: DateTime<Utc>,
}

impl TemporalSidecar {
    /// Read the sidecar for a given owner. Returns `None` if the file
    /// doesn't exist or can't be parsed (first interaction).
    pub fn read(workspace: &Path, owner: &str) -> Option<Self> {
        let path = Self::path(workspace, owner);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Write the sidecar atomically. Creates the directory if needed.
    pub fn write(workspace: &Path, owner: &str, now: DateTime<Utc>) -> eyre::Result<()> {
        let dir = workspace.join("memory").join(".temporal");
        std::fs::create_dir_all(&dir)?;

        let sidecar = Self {
            last_interaction: now,
        };
        let json = serde_json::to_string_pretty(&sidecar)?;

        // Atomic write: tmp file + rename
        let path = Self::path(workspace, owner);
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, &path)?;

        Ok(())
    }

    /// Filesystem path for the sidecar file.
    fn path(workspace: &Path, owner: &str) -> std::path::PathBuf {
        // Sanitise owner to be filesystem-safe
        let safe_owner = owner.replace(['/', '\\', ':', '\0'], "_");
        workspace
            .join("memory")
            .join(".temporal")
            .join(format!("{}.json", safe_owner))
    }
}

// ─── Context Block Builders ──────────────────────────────────────────────────

/// Build the `## Temporal Context` section for injection into the system prompt.
///
/// Returns `None` if no sidecar exists (first interaction ever).
pub fn build_temporal_context_block(
    workspace: &Path,
    owner: &str,
    _config: &TemporalConfig,
) -> Option<String> {
    let sidecar = TemporalSidecar::read(workspace, owner)?;
    let now = Utc::now();
    let gap = now.signed_duration_since(sidecar.last_interaction);

    // Don't emit context for very short gaps (< 1 minute)
    if gap.num_seconds() < 60 {
        return None;
    }

    let classification = GapClassification::classify(gap);
    let duration_str = humanize_duration_compact(gap);

    Some(format!(
        "## Temporal Context\n\
         Last conversation: {} ago ({})\n\
         {}\n\
         Last interaction: {}",
        duration_str,
        classification.label(),
        classification.context_note(),
        sidecar.last_interaction.format("%Y-%m-%d %H:%M UTC"),
    ))
}

/// Build the `## ⚠️ Staleness Warnings` section.
///
/// Returns `None` if there are no warnings — the section must NOT be injected
/// when clean.
pub fn build_staleness_warnings(
    workspace: &Path,
    session_id: Option<&str>,
    config: &TemporalConfig,
    last_interaction: Option<DateTime<Utc>>,
) -> Option<String> {
    let now = Utc::now();
    let mut warnings: Vec<String> = Vec::new();

    // 1. Check active task staleness
    let active_task_meta = workspace.join("memory").join("active-task.meta.json");
    if let Ok(content) = std::fs::read_to_string(&active_task_meta) {
        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(claimed_at) = meta
                .get("claimed_at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            {
                let age_hours = now
                    .signed_duration_since(claimed_at.with_timezone(&Utc))
                    .num_hours();
                if age_hours >= config.stale_task_warning_hours as i64 {
                    let task_desc = meta
                        .get("task")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown task)");
                    warnings.push(format!(
                        "- **Stale active task** ({}h old): {}",
                        age_hours, task_desc
                    ));
                }
            }
        }
    }

    // Also check active-task.md mtime as fallback
    if warnings.is_empty() {
        let active_task_path = workspace.join("memory").join("active-task.md");
        if active_task_path.exists() {
            if let Ok(meta) = std::fs::metadata(&active_task_path) {
                if let Ok(modified) = meta.modified() {
                    let modified_dt: DateTime<Utc> = modified.into();
                    let age_hours = now.signed_duration_since(modified_dt).num_hours();
                    if age_hours >= config.stale_task_warning_hours as i64 {
                        if let Ok(content) = std::fs::read_to_string(&active_task_path) {
                            let first_line = content.lines().next().unwrap_or("(unknown task)");
                            if !first_line.trim().is_empty() {
                                warnings.push(format!(
                                    "- **Stale active task** ({}h old): {}",
                                    age_hours,
                                    first_line.trim()
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Check conversation gap
    if let Some(last) = last_interaction {
        let gap_days = now.signed_duration_since(last).num_hours() / 24;
        if gap_days >= config.conversation_gap_warning_days as i64 {
            warnings.push(format!(
                "- **Long conversation gap**: {} days since last interaction — review recent changes and priorities",
                gap_days
            ));
        }
    }

    // 3. Check cross-session task staleness
    let sessions_dir = workspace.join("memory").join("sessions");
    if sessions_dir.is_dir() {
        let mut stale_cross_session_tasks: Vec<String> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let sid = entry.file_name().to_string_lossy().to_string();
                // Skip current session
                if session_id == Some(sid.as_str()) {
                    continue;
                }

                let tasks_path = entry.path().join("tasks.json");
                if let Ok(content) = std::fs::read_to_string(&tasks_path) {
                    if let Ok(tasks) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                        for task in &tasks {
                            if task.get("status").and_then(|v| v.as_str()) != Some("active") {
                                continue;
                            }

                            // Check task age via created_at or file mtime
                            let task_age_hours = if let Some(created) =
                                task.get("created_at").and_then(|v| {
                                    v.as_str()
                                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                }) {
                                now.signed_duration_since(created.with_timezone(&Utc))
                                    .num_hours()
                            } else if let Ok(meta) = std::fs::metadata(&tasks_path) {
                                if let Ok(modified) = meta.modified() {
                                    let dt: DateTime<Utc> = modified.into();
                                    now.signed_duration_since(dt).num_hours()
                                } else {
                                    0
                                }
                            } else {
                                0
                            };

                            if task_age_hours >= 48 {
                                if let Some(desc) = task.get("description").and_then(|v| v.as_str())
                                {
                                    stale_cross_session_tasks.push(format!(
                                        "- **Stale cross-session task** ({}h, session `{}`): {}",
                                        task_age_hours,
                                        &sid[..8.min(sid.len())],
                                        desc
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Cap at 5 to avoid prompt bloat
        for warning in stale_cross_session_tasks.into_iter().take(5) {
            warnings.push(warning);
        }
    }

    if warnings.is_empty() {
        return None;
    }

    Some(format!("## ⚠️ Staleness Warnings\n{}", warnings.join("\n")))
}

/// Parse a whole-number duration with one unit — `"30s"`, `"15m"`, `"1h"`,
/// `"2d"` — into a [`std::time::Duration`]. Zero, fractions and any other unit
/// are refused.
pub fn parse_duration(s: &str) -> eyre::Result<std::time::Duration> {
    let s = s.trim();
    let unit = s.chars().last().ok_or_else(|| eyre::eyre!("is empty"))?;
    let magnitude = &s[..s.len() - unit.len_utf8()];
    let n: u64 = magnitude
        .parse()
        .map_err(|_| eyre::eyre!("cannot parse leading number in \"{s}\""))?;
    if n == 0 {
        return Err(eyre::eyre!("must be positive"));
    }
    let per_unit: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        other => {
            return Err(eyre::eyre!(
                "unknown unit \"{other}\" — expected one of s, m, h, d"
            ))
        }
    };
    n.checked_mul(per_unit)
        .map(std::time::Duration::from_secs)
        .ok_or_else(|| eyre::eyre!("\"{s}\" is too long"))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use tempfile::TempDir;

    #[test]
    fn parses_durations() {
        let secs = |s| parse_duration(s).unwrap().as_secs();
        assert_eq!(secs("15m"), 900);
        assert_eq!(secs("1h"), 3600);
        assert_eq!(secs("30s"), 30);
        assert_eq!(secs("2d"), 172_800);
        assert_eq!(secs(" 20h "), 72_000);
    }

    #[test]
    fn rejects_zero_and_unknown_units() {
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("m").is_err());
        assert!(parse_duration("1.5h").is_err());
        assert!(parse_duration("5é").is_err());
        assert!(parse_duration("99999999999999999d").is_err());
    }

    // ── Gap Classification ──

    #[test]
    fn classify_continuation() {
        assert_eq!(
            GapClassification::classify(Duration::seconds(30)),
            GapClassification::Continuation
        );
        assert_eq!(
            GapClassification::classify(Duration::minutes(4)),
            GapClassification::Continuation
        );
    }

    #[test]
    fn classify_short() {
        assert_eq!(
            GapClassification::classify(Duration::minutes(5)),
            GapClassification::Short
        );
        assert_eq!(
            GapClassification::classify(Duration::minutes(119)),
            GapClassification::Short
        );
    }

    #[test]
    fn classify_overnight() {
        assert_eq!(
            GapClassification::classify(Duration::hours(2)),
            GapClassification::Overnight
        );
        assert_eq!(
            GapClassification::classify(Duration::hours(17)),
            GapClassification::Overnight
        );
    }

    #[test]
    fn classify_multi_day() {
        assert_eq!(
            GapClassification::classify(Duration::hours(18)),
            GapClassification::MultiDay
        );
        assert_eq!(
            GapClassification::classify(Duration::days(2)),
            GapClassification::MultiDay
        );
    }

    #[test]
    fn classify_weekly() {
        assert_eq!(
            GapClassification::classify(Duration::days(3)),
            GapClassification::Weekly
        );
        assert_eq!(
            GapClassification::classify(Duration::days(6)),
            GapClassification::Weekly
        );
    }

    #[test]
    fn classify_extended_absence() {
        assert_eq!(
            GapClassification::classify(Duration::days(7)),
            GapClassification::ExtendedAbsence
        );
        assert_eq!(
            GapClassification::classify(Duration::days(30)),
            GapClassification::ExtendedAbsence
        );
    }

    #[test]
    fn classify_display() {
        assert_eq!(GapClassification::Continuation.to_string(), "Continuation");
        assert_eq!(
            GapClassification::ExtendedAbsence.to_string(),
            "Extended absence"
        );
    }

    // ── Duration Humanisation ──

    #[test]
    fn humanize_just_now() {
        assert_eq!(humanize_duration(Duration::seconds(30)), "just now");
    }

    #[test]
    fn humanize_minutes() {
        assert_eq!(humanize_duration(Duration::minutes(1)), "1 minute ago");
        assert_eq!(humanize_duration(Duration::minutes(45)), "45 minutes ago");
    }

    #[test]
    fn humanize_hours() {
        assert_eq!(humanize_duration(Duration::hours(1)), "1 hour ago");
        assert_eq!(humanize_duration(Duration::hours(5)), "5 hours ago");
    }

    #[test]
    fn humanize_days() {
        assert_eq!(humanize_duration(Duration::days(1)), "1 day ago");
        assert_eq!(humanize_duration(Duration::days(10)), "10 days ago");
    }

    #[test]
    fn humanize_weeks() {
        assert_eq!(humanize_duration(Duration::days(14)), "2 weeks ago");
        assert_eq!(humanize_duration(Duration::days(21)), "3 weeks ago");
    }

    #[test]
    fn humanize_negative_clamped() {
        // Negative durations should be clamped to 0
        assert_eq!(humanize_duration(Duration::seconds(-100)), "just now");
    }

    #[test]
    fn humanize_compact() {
        assert_eq!(humanize_duration_compact(Duration::hours(3)), "3 hours");
        assert_eq!(humanize_duration_compact(Duration::days(2)), "2 days");
    }

    // ── Sidecar Read/Write ──

    #[test]
    fn sidecar_roundtrip() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path();
        let owner = "U12345";
        let now = Utc::now();

        // No sidecar yet
        assert!(TemporalSidecar::read(workspace, owner).is_none());

        // Write
        TemporalSidecar::write(workspace, owner, now).unwrap();

        // Read back
        let sidecar = TemporalSidecar::read(workspace, owner).unwrap();
        assert_eq!(sidecar.last_interaction.timestamp(), now.timestamp());
    }

    #[test]
    fn sidecar_missing_file() {
        let dir = TempDir::new().unwrap();
        assert!(TemporalSidecar::read(dir.path(), "nonexistent").is_none());
    }

    #[test]
    fn sidecar_sanitises_owner() {
        let dir = TempDir::new().unwrap();
        let now = Utc::now();
        // Owner with special characters
        TemporalSidecar::write(dir.path(), "user/with:special", now).unwrap();
        let sidecar = TemporalSidecar::read(dir.path(), "user/with:special").unwrap();
        assert_eq!(sidecar.last_interaction.timestamp(), now.timestamp());
    }

    // ── Temporal Context Block ──

    #[test]
    fn context_block_no_sidecar() {
        let dir = TempDir::new().unwrap();
        let config = TemporalConfig::default();
        assert!(build_temporal_context_block(dir.path(), "user", &config).is_none());
    }

    #[test]
    fn context_block_recent_gap_suppressed() {
        let dir = TempDir::new().unwrap();
        let config = TemporalConfig::default();
        // Write sidecar with very recent timestamp
        TemporalSidecar::write(dir.path(), "user", Utc::now()).unwrap();
        // Gap < 1 minute, should return None
        assert!(build_temporal_context_block(dir.path(), "user", &config).is_none());
    }

    #[test]
    fn context_block_with_gap() {
        let dir = TempDir::new().unwrap();
        let config = TemporalConfig::default();
        let two_hours_ago = Utc::now() - Duration::hours(2);
        TemporalSidecar::write(dir.path(), "user", two_hours_ago).unwrap();

        let block = build_temporal_context_block(dir.path(), "user", &config).unwrap();
        assert!(block.contains("## Temporal Context"));
        assert!(block.contains("Overnight")); // 2 hours = overnight classification
    }

    // ── Staleness Warnings ──

    #[test]
    fn staleness_warnings_clean() {
        let dir = TempDir::new().unwrap();
        let config = TemporalConfig::default();
        // No stale tasks, recent interaction
        let result = build_staleness_warnings(dir.path(), None, &config, Some(Utc::now()));
        assert!(result.is_none());
    }

    #[test]
    fn staleness_warnings_long_gap() {
        let dir = TempDir::new().unwrap();
        let config = TemporalConfig::default();
        let five_days_ago = Utc::now() - Duration::days(5);
        let result = build_staleness_warnings(dir.path(), None, &config, Some(five_days_ago));
        assert!(result.is_some());
        let warnings = result.unwrap();
        assert!(warnings.contains("Long conversation gap"));
        assert!(warnings.contains("5 days"));
    }

    #[test]
    fn staleness_warnings_stale_cross_session_task() {
        let dir = TempDir::new().unwrap();
        let config = TemporalConfig::default();

        // Create a stale cross-session task
        let sessions_dir = dir
            .path()
            .join("memory")
            .join("sessions")
            .join("old-session");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let three_days_ago = Utc::now() - Duration::days(3);
        let tasks = serde_json::json!([{
            "id": "task-1",
            "description": "Fix the login bug",
            "status": "active",
            "created_at": three_days_ago.to_rfc3339()
        }]);
        std::fs::write(
            sessions_dir.join("tasks.json"),
            serde_json::to_string(&tasks).unwrap(),
        )
        .unwrap();

        let result = build_staleness_warnings(
            dir.path(),
            Some("current-session"),
            &config,
            Some(Utc::now()),
        );
        assert!(result.is_some());
        let warnings = result.unwrap();
        assert!(warnings.contains("Stale cross-session task"));
        assert!(warnings.contains("Fix the login bug"));
    }

    // ── Config defaults ──

    #[test]
    fn config_defaults() {
        let config = TemporalConfig::default();
        assert!(config.enabled);
        assert!((config.decay_rate - 0.005).abs() < f64::EPSILON);
        assert_eq!(config.max_lookback_days, 7);
        assert_eq!(config.stale_task_warning_hours, 24);
        assert_eq!(config.conversation_gap_warning_days, 3);
    }

    #[test]
    fn config_deserialise_partial() {
        let toml_str = r#"
            enabled = true
            decay_rate = 0.01
        "#;
        let config: TemporalConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert!((config.decay_rate - 0.01).abs() < f64::EPSILON);
        // Defaults for unspecified fields
        assert_eq!(config.max_lookback_days, 7);
    }

    #[test]
    fn config_deserialise_disabled() {
        let toml_str = "enabled = false";
        let config: TemporalConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.enabled);
    }
}
