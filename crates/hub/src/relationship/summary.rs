//! Pure roll-up of relationship events into a per-user summary, and the
//! reflective prompt block the agent uses to acknowledge the dynamic.
//!
//! Sentiment decays by wall-clock time (half-life [`HALF_LIFE_DAYS`]) so a rough
//! patch heals even with no new interactions — relationships recover.

use chrono::{DateTime, Utc};
use serde::Serialize;
use workforce_common::telemetry::RelationshipEvent;

pub const HALF_LIFE_DAYS: f64 = 14.0;
const KEY_MOMENTS_SHOWN: usize = 5;
const MIN_INTERACTIONS_TO_SURFACE: usize = 3;
const QUIET_AFTER_DAYS: i64 = 60;

#[derive(Debug, Clone, Serialize)]
pub struct KeyMoment {
    pub timestamp: DateTime<Utc>,
    pub valence: f64,
    pub reaction: String,
    pub emotion_label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationshipSummary {
    pub interactions: usize,
    pub active_days: usize,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub days_since_last: Option<i64>,
    /// Wall-clock-decayed, confidence-weighted mean valence in `[-1, 1]`.
    pub sentiment: f64,
    pub band: String,
    pub gratitude: usize,
    pub praise: usize,
    pub acceptance: usize,
    pub corrections: usize,
    pub retries: usize,
    pub stops: usize,
    pub key_moments: Vec<KeyMoment>,
    pub window_days: i64,
}

pub fn summarise(
    events: &[RelationshipEvent],
    now: DateTime<Utc>,
    window_days: i64,
) -> RelationshipSummary {
    let first_seen = events.iter().map(|e| e.timestamp).min();
    let last_seen = events.iter().map(|e| e.timestamp).max();
    let days_since_last = last_seen.map(|t| days_between(t, now));

    let tau = HALF_LIFE_DAYS / std::f64::consts::LN_2;
    let (wsum, vsum) = events.iter().fold((0.0_f64, 0.0_f64), |(ws, vs), e| {
        let dt_days = ((now - e.timestamp).num_seconds() as f64 / 86_400.0).max(0.0);
        let w = (-dt_days / tau).exp() * e.confidence.max(0.05);
        (ws + w, vs + w * e.valence)
    });
    let sentiment = if wsum > 0.0 { vsum / wsum } else { 0.0 };

    let mut days: Vec<String> = events
        .iter()
        .map(|e| e.timestamp.format("%Y-%m-%d").to_string())
        .collect();
    days.sort();
    days.dedup();

    let count = |reaction: &str| events.iter().filter(|e| e.reaction == reaction).count();

    let mut key_moments: Vec<KeyMoment> = events
        .iter()
        .filter(|e| e.is_key_moment)
        .map(|e| KeyMoment {
            timestamp: e.timestamp,
            valence: e.valence,
            reaction: e.reaction.clone(),
            emotion_label: e.emotion_label.clone(),
        })
        .collect();
    key_moments.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    key_moments.truncate(KEY_MOMENTS_SHOWN);

    RelationshipSummary {
        interactions: events.len(),
        active_days: days.len(),
        first_seen,
        last_seen,
        days_since_last,
        sentiment,
        band: band_for(sentiment, days_since_last).to_string(),
        gratitude: count("gratitude"),
        praise: count("praise"),
        acceptance: count("acceptance"),
        corrections: count("correction"),
        retries: count("retry"),
        stops: count("stop"),
        key_moments,
        window_days,
    }
}

fn days_between(earlier: DateTime<Utc>, later: DateTime<Utc>) -> i64 {
    ((later - earlier).num_seconds() as f64 / 86_400.0).floor() as i64
}

fn band_for(sentiment: f64, days_since_last: Option<i64>) -> &'static str {
    if days_since_last
        .map(|d| d > QUIET_AFTER_DAYS)
        .unwrap_or(true)
    {
        return "quiet";
    }
    if sentiment >= 0.4 {
        "warm"
    } else if sentiment >= 0.1 {
        "positive"
    } else if sentiment > -0.2 {
        "neutral"
    } else {
        "strained"
    }
}

/// The reflective block injected alongside preferences. Empty when there is too
/// little signal to say anything — never a header without content.
pub fn format_relationship_for_prompt(summary: &RelationshipSummary) -> String {
    if summary.interactions < MIN_INTERACTIONS_TO_SURFACE {
        return String::new();
    }
    let recency = match summary.days_since_last {
        Some(0) => " (last spoke today)".to_string(),
        Some(1) => " (last spoke yesterday)".to_string(),
        Some(d) if d > 1 => format!(" (last spoke {d} days ago)"),
        _ => String::new(),
    };
    let mut out = String::from(
        "\n\n## Your Relationship With This User\n\
         _Observed from your past direct conversations. Acknowledge it naturally if it is \
         relevant; don't force it or over-index on it._\n",
    );
    out.push_str(&format!(
        "- Overall: {} — {} exchanges across {} day(s){}\n",
        summary.band, summary.interactions, summary.active_days, recency
    ));
    let appreciative = summary.gratitude + summary.praise + summary.acceptance;
    let frustrated = summary.corrections + summary.retries + summary.stops;
    if appreciative > 0 || frustrated > 0 {
        out.push_str(&format!(
            "- Recent signals: {appreciative} appreciative, {frustrated} frustrated/corrective\n"
        ));
    }
    if let Some(km) = summary.key_moments.first() {
        let label = km.emotion_label.as_deref().unwrap_or(km.reaction.as_str());
        out.push_str(&format!(
            "- Notable: {} on {}\n",
            label,
            km.timestamp.format("%Y-%m-%d")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ts: DateTime<Utc>, valence: f64, reaction: &str, key: bool) -> RelationshipEvent {
        RelationshipEvent {
            timestamp: ts,
            agent_id: "a".into(),
            identity_id: "u".into(),
            session_id: "s".into(),
            message_id: ts.to_rfc3339(),
            valence,
            intensity: 0.7,
            emotion_label: None,
            reaction: reaction.into(),
            is_key_moment: key,
            confidence: 0.8,
            source: "code".into(),
            signals: "{}".into(),
        }
    }

    #[test]
    fn empty_is_quiet() {
        let s = summarise(&[], Utc::now(), 90);
        assert_eq!(s.interactions, 0);
        assert_eq!(s.band, "quiet");
        assert_eq!(s.sentiment, 0.0);
        assert!(s.days_since_last.is_none());
    }

    #[test]
    fn recent_dominates_old_via_decay() {
        let now = Utc::now();
        let old_bad = event(now - chrono::Duration::days(45), -0.9, "correction", true);
        let fresh_good = event(now - chrono::Duration::hours(2), 0.8, "praise", true);
        let s = summarise(&[old_bad, fresh_good], now, 90);
        assert!(
            s.sentiment > 0.3,
            "recent positive must outweigh decayed old negative, got {}",
            s.sentiment
        );
        assert_eq!(s.band, "warm");
    }

    #[test]
    fn absence_heals_then_goes_quiet() {
        let now = Utc::now();
        // A single bad turn long ago: decayed, and beyond the quiet horizon.
        let s = summarise(
            &[event(now - chrono::Duration::days(75), -0.9, "stop", true)],
            now,
            120,
        );
        assert_eq!(s.band, "quiet", "stale relationships read as quiet");
        assert_eq!(s.stops, 1);
    }

    #[test]
    fn counts_and_active_days_dedup() {
        let now = Utc::now();
        let day1 = now - chrono::Duration::days(1);
        let s = summarise(
            &[
                event(day1, 0.6, "gratitude", false),
                event(day1 + chrono::Duration::minutes(5), 0.7, "praise", false),
                event(now, -0.6, "correction", true),
            ],
            now,
            90,
        );
        assert_eq!(s.interactions, 3);
        assert_eq!(s.active_days, 2, "two distinct calendar days");
        assert_eq!(s.gratitude, 1);
        assert_eq!(s.praise, 1);
        assert_eq!(s.corrections, 1);
        assert_eq!(s.key_moments.len(), 1);
    }

    #[test]
    fn prompt_block_empty_below_threshold() {
        let now = Utc::now();
        let s = summarise(&[event(now, 0.6, "gratitude", false)], now, 90);
        assert!(format_relationship_for_prompt(&s).is_empty());
    }

    #[test]
    fn prompt_block_renders_with_enough_signal() {
        let now = Utc::now();
        let evs: Vec<_> = (0..4)
            .map(|i| event(now - chrono::Duration::days(i), 0.6, "gratitude", false))
            .collect();
        let s = summarise(&evs, now, 90);
        let block = format_relationship_for_prompt(&s);
        assert!(block.contains("## Your Relationship With This User"));
        assert!(block.contains("appreciative"));
        assert!(block.contains("Acknowledge it naturally"));
    }
}
