//! Turn capture: score a turn and persist it when it carries signal.
//!
//! Shared by the live hook and the periodic scan. The scan owns the per-session
//! cursor; capture only writes the event, which is idempotent on
//! `(session_id, message_id)`, so the two paths never double-count.

use chrono::{DateTime, Utc};
use eyre::Result;
use workforce_common::telemetry::{RelationshipEvent, TelemetryStore};

use crate::relationship::scoring::{score, RelationshipRead};

/// Score one user turn and record it when noteworthy. Returns `true` when a new
/// row was written (`false` for flat chatter or a re-attempted turn).
#[allow(clippy::too_many_arguments)]
pub fn capture_turn(
    telemetry: &dyn TelemetryStore,
    agent_id: &str,
    identity_id: &str,
    session_id: &str,
    message_id: &str,
    event_time: DateTime<Utc>,
    user_text: &str,
) -> Result<bool> {
    let read = score(user_text);
    if !read.is_noteworthy() {
        return Ok(false);
    }
    let event = build_event(
        read,
        agent_id,
        identity_id,
        session_id,
        message_id,
        event_time,
    );
    telemetry.record_relationship_event(&event)
}

fn build_event(
    read: RelationshipRead,
    agent_id: &str,
    identity_id: &str,
    session_id: &str,
    message_id: &str,
    event_time: DateTime<Utc>,
) -> RelationshipEvent {
    RelationshipEvent {
        timestamp: event_time,
        agent_id: agent_id.to_string(),
        identity_id: identity_id.to_string(),
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        valence: read.valence,
        intensity: read.intensity,
        emotion_label: None,
        reaction: read.reaction.as_str().to_string(),
        is_key_moment: read.is_key_moment,
        confidence: read.confidence,
        source: "code".to_string(),
        signals: serde_json::to_string(&read.signals).unwrap_or_else(|_| "{}".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::persistence::SqliteStore;

    #[test]
    fn noteworthy_turn_is_recorded_neutral_is_not() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now();

        let wrote = capture_turn(
            &store,
            "atlas",
            "alice",
            "sess-1",
            "m1",
            now,
            "perfect, thank you!",
        )
        .unwrap();
        assert!(wrote);

        let flat = capture_turn(
            &store,
            "atlas",
            "alice",
            "sess-1",
            "m2",
            now,
            "what is the weather",
        )
        .unwrap();
        assert!(!flat, "flat chatter is not persisted");

        let events = store
            .relationship_events_in_range(
                Some("atlas"),
                Some("alice"),
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reaction, "praise");
    }

    #[test]
    fn re_attempted_turn_is_idempotent() {
        let store = SqliteStore::in_memory().unwrap();
        let now = Utc::now();
        let args = || {
            capture_turn(
                &store,
                "atlas",
                "alice",
                "sess-1",
                "m1",
                now,
                "thanks a lot!",
            )
        };
        assert!(args().unwrap());
        assert!(!args().unwrap(), "second attempt of same turn is a no-op");
    }
}
