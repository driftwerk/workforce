use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hard cap on NATS envelope size, enforced on deserialise. Defense in
/// depth — F13 (2026-05-15) — a compromised broker can't feed the hub
/// arbitrarily large payloads. 1 MiB is comfortably above legitimate
/// chat / control traffic.
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;

/// Default TTL for the replay guard's seen-id cache.
pub const DEFAULT_REPLAY_TTL: Duration = Duration::from_secs(300);

/// A standard envelope wrapping all messages on the NATS bus.
///
/// Every message published to NATS is wrapped in this envelope to provide
/// consistent metadata for tracing, auditing, and replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Unique message ID.
    pub id: Uuid,
    /// The NATS subject this was published to.
    pub subject: String,
    /// Who sent this message (agent_id, "system", or user_id).
    pub sender: String,
    /// When the message was created.
    pub timestamp: DateTime<Utc>,
    /// Optional correlation ID for request/reply tracking.
    pub correlation_id: Option<Uuid>,
    /// The actual message payload.
    pub payload: T,
}

impl<T: Serialize> Envelope<T> {
    /// Create a new envelope wrapping a payload.
    pub fn new(subject: impl Into<String>, sender: impl Into<String>, payload: T) -> Self {
        Self {
            id: Uuid::new_v4(),
            subject: subject.into(),
            sender: sender.into(),
            timestamp: Utc::now(),
            correlation_id: None,
            payload,
        }
    }

    /// Set a correlation ID (for request/reply patterns).
    pub fn with_correlation(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Serialise the envelope to JSON bytes.
    pub fn to_bytes(&self) -> eyre::Result<bytes::Bytes> {
        let json = serde_json::to_vec(self)
            .map_err(|e| eyre::eyre!("Failed to serialise envelope: {}", e))?;
        Ok(bytes::Bytes::from(json))
    }
}

impl<T: for<'de> Deserialize<'de>> Envelope<T> {
    /// Deserialise an envelope from JSON bytes. Rejects payloads above
    /// [`MAX_ENVELOPE_BYTES`] (F13, 2026-05-15) before invoking the JSON
    /// parser — caps memory pressure from a hostile broker.
    pub fn from_bytes(data: &[u8]) -> eyre::Result<Self> {
        if data.len() > MAX_ENVELOPE_BYTES {
            eyre::bail!(
                "envelope rejected: {} bytes exceeds cap of {} bytes",
                data.len(),
                MAX_ENVELOPE_BYTES
            );
        }
        serde_json::from_slice(data)
            .map_err(|e| eyre::eyre!("Failed to deserialise envelope: {}", e))
    }
}

/// In-memory replay guard against a compromised NATS broker re-publishing
/// captured envelopes (F13, 2026-05-15). Consumers hold one alongside
/// their subscription and call `check_and_record` per received envelope.
///
/// Cleanup is amortised: a full `retain` only runs every `ttl/10`, so
/// `check_and_record` is O(1) on the hot path. Memory cost stays
/// proportional to QPS × TTL — at 100 msg/sec with the default 5-min
/// TTL that's roughly 1.2 MB of UUIDs.
pub struct EnvelopeReplayGuard {
    seen: DashMap<Uuid, Instant>,
    ttl: Duration,
    last_cleanup: Mutex<Instant>,
}

impl EnvelopeReplayGuard {
    pub fn new(ttl: Duration) -> Self {
        Self {
            seen: DashMap::new(),
            ttl,
            last_cleanup: Mutex::new(Instant::now()),
        }
    }

    /// True on first sight; false on a replay within the TTL window.
    /// Caller should treat `false` as "drop the message".
    pub fn check_and_record(&self, id: Uuid) -> bool {
        let now = Instant::now();
        self.maybe_sweep_stale(now);

        if let Entry::Vacant(slot) = self.seen.entry(id) {
            slot.insert(now);
            true
        } else {
            false
        }
    }

    /// Amortised cleanup: only sweeps when at least `ttl/10` has elapsed
    /// since the last sweep. Bounds the cost of `retain` regardless of
    /// inbound message rate. A poisoned mutex falls through to "sweep
    /// anyway" — sweeping is idempotent and we'd rather pay it than
    /// silently grow.
    fn maybe_sweep_stale(&self, now: Instant) {
        let interval = self.ttl / 10;
        let should_sweep = match self.last_cleanup.lock() {
            Ok(mut last) => {
                if now.duration_since(*last) >= interval {
                    *last = now;
                    true
                } else {
                    false
                }
            }
            Err(_) => true,
        };
        if should_sweep {
            self.seen.retain(|_, ts| now.duration_since(*ts) < self.ttl);
        }
    }
}

impl Default for EnvelopeReplayGuard {
    fn default() -> Self {
        Self::new(DEFAULT_REPLAY_TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestPayload {
        message: String,
    }

    #[test]
    fn envelope_roundtrip() {
        let envelope = Envelope::new(
            "workforce.agents.dev-1.inbox",
            "orchestrator",
            TestPayload {
                message: "Hello, agent!".to_string(),
            },
        );

        let bytes = envelope.to_bytes().unwrap();
        let parsed: Envelope<TestPayload> = Envelope::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.id, envelope.id);
        assert_eq!(parsed.sender, "orchestrator");
        assert_eq!(parsed.subject, "workforce.agents.dev-1.inbox");
        assert_eq!(parsed.payload.message, "Hello, agent!");
    }

    #[test]
    fn envelope_with_correlation() {
        let corr_id = Uuid::new_v4();
        let envelope = Envelope::new(
            "test.subject",
            "sender",
            TestPayload {
                message: "test".to_string(),
            },
        )
        .with_correlation(corr_id);

        assert_eq!(envelope.correlation_id, Some(corr_id));
    }

    // ── F13: size cap + replay guard ───────────────────────────────────

    #[test]
    fn from_bytes_rejects_payload_over_cap() {
        let oversized = vec![b'A'; MAX_ENVELOPE_BYTES + 1];
        let err: eyre::Report = Envelope::<TestPayload>::from_bytes(&oversized).unwrap_err();
        assert!(format!("{err}").contains("exceeds cap"));
    }

    #[test]
    fn from_bytes_accepts_payload_at_cap() {
        // A modest valid envelope is well under the cap; this just pins
        // that the cap doesn't trigger on legitimate traffic.
        let envelope = Envelope::new(
            "test.subject",
            "sender",
            TestPayload {
                message: "small".to_string(),
            },
        );
        let bytes = envelope.to_bytes().unwrap();
        assert!(bytes.len() < MAX_ENVELOPE_BYTES);
        assert!(Envelope::<TestPayload>::from_bytes(&bytes).is_ok());
    }

    #[test]
    fn replay_guard_first_sight_accepts() {
        let guard = EnvelopeReplayGuard::default();
        assert!(guard.check_and_record(Uuid::new_v4()));
    }

    #[test]
    fn replay_guard_rejects_duplicate_within_ttl() {
        let guard = EnvelopeReplayGuard::new(Duration::from_secs(60));
        let id = Uuid::new_v4();
        assert!(guard.check_and_record(id));
        assert!(!guard.check_and_record(id));
    }

    #[test]
    fn replay_guard_independent_ids_independent() {
        let guard = EnvelopeReplayGuard::default();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        assert!(guard.check_and_record(id1));
        assert!(guard.check_and_record(id2));
        assert!(!guard.check_and_record(id1));
        assert!(!guard.check_and_record(id2));
    }

    #[test]
    fn replay_guard_evicts_after_ttl() {
        let guard = EnvelopeReplayGuard::new(Duration::from_millis(20));
        let id = Uuid::new_v4();
        assert!(guard.check_and_record(id));
        std::thread::sleep(Duration::from_millis(40));
        // After TTL the next check_and_record on any id triggers cleanup,
        // and the same id is treated as fresh.
        assert!(guard.check_and_record(id));
    }
}
