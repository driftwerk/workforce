//! Retroactive secret redaction over stored sessions.
//!
//! Ingress tokenization keeps recognised credentials out of new
//! transcripts, but two gaps remain at rest: secrets written before a
//! pattern existed (catalogue backfill) and any path that wrote before
//! masking. This sweep re-applies the ingress catalogue (`redact_secrets`)
//! to stored messages and archives — at-rest hygiene, not a substitute
//! for `/secret` (it cannot catch unpatternable secrets) and not
//! containment (what already reached the model is gone).
//!
//! Two drivers share one core ([`redact_session_ids`]): a per-agent
//! scheduled sweep ([`run_scheduled_sweep`], incremental + idle-skipped)
//! and a manual operator sweep ([`run_manual_sweep`], all sessions). Both
//! run in the hub process, so the store keeps its cache coherent with
//! disk — no cross-process race.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use workforce_common::session::SessionStore;

/// Default cadence for the per-agent scheduled sweep.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Minimum idle time before the scheduled sweep touches a session. A more
/// recently updated session may have a turn in flight whose clone-and-save
/// would re-persist cleartext after we redact; such sessions are deferred
/// to a later sweep (their `updated_at` stays above the cursor).
pub const SCHEDULED_IDLE: Duration = Duration::from_secs(5 * 60);

/// Upper bound on sessions pulled per sweep — a safety cap, far above any
/// realistic per-agent count updated within one interval.
const MAX_SESSIONS_PER_SWEEP: usize = 100_000;

/// Aggregate outcome of a redaction sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RedactionReport {
    pub sessions_scanned: usize,
    pub sessions_redacted: usize,
    pub messages_redacted: usize,
    pub archive_messages_redacted: usize,
}

/// Redact each session id through the store, aggregating stats. Per-session
/// failures are logged and skipped so one bad session can't abort a sweep.
pub fn redact_session_ids(
    store: &Arc<dyn SessionStore>,
    session_ids: &[String],
    dry_run: bool,
) -> RedactionReport {
    session_ids
        .iter()
        .fold(RedactionReport::default(), |mut report, id| {
            report.sessions_scanned += 1;
            match store.redact_secrets_in_session(id, dry_run) {
                Ok(stats) if stats.changed() => {
                    report.sessions_redacted += 1;
                    report.messages_redacted += stats.messages_redacted;
                    report.archive_messages_redacted += stats.archive_messages_redacted;
                    tracing::info!(
                        target: "sentinel.audit",
                        session_id = %id,
                        dry_run,
                        messages_redacted = stats.messages_redacted,
                        archive_messages_redacted = stats.archive_messages_redacted,
                        "session secrets redacted"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(session_id = %id, error = %e, "session redaction failed")
                }
            }
            report
        })
}

fn cursor_path(workspace: &Path) -> PathBuf {
    workspace.join(".sessions-redaction-cursor")
}

fn read_cursor(workspace: &Path) -> DateTime<Utc> {
    std::fs::read_to_string(cursor_path(workspace))
        .ok()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw.trim()).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn write_cursor(workspace: &Path, at: DateTime<Utc>) {
    let path = cursor_path(workspace);
    if let Err(e) = std::fs::write(&path, at.to_rfc3339()) {
        tracing::warn!(error = %e, path = %path.display(), "failed to persist redaction cursor");
    }
}

/// One incremental scheduled sweep for `agent_id`: redact sessions updated
/// since the cursor that have since gone idle, then advance the cursor to
/// the newest `updated_at` actually processed. A session still too recent
/// to touch is left above the cursor so the next sweep revisits it.
pub fn run_scheduled_sweep(
    store: &Arc<dyn SessionStore>,
    agent_id: &str,
    workspace: &Path,
    idle: Duration,
) -> RedactionReport {
    let cutoff = read_cursor(workspace);
    let sessions = match store.list_since(agent_id, cutoff, MAX_SESSIONS_PER_SWEEP) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "redaction sweep list_since failed");
            return RedactionReport::default();
        }
    };

    let idle = chrono::Duration::from_std(idle).unwrap_or_else(|_| chrono::Duration::minutes(5));
    let now = Utc::now();
    let processed: Vec<(String, DateTime<Utc>)> = sessions
        .into_iter()
        .filter(|s| now.signed_duration_since(s.updated_at) >= idle)
        .map(|s| (s.id, s.updated_at))
        .collect();

    let ids: Vec<String> = processed.iter().map(|(id, _)| id.clone()).collect();
    let report = redact_session_ids(store, &ids, false);

    if let Some(max) = processed.iter().map(|(_, t)| *t).max() {
        write_cursor(workspace, max);
    }
    report
}

/// Redact every session in the store (operator-triggered). Honours
/// `dry_run`; ignores the cursor and idle window.
pub fn run_manual_sweep(store: &Arc<dyn SessionStore>, dry_run: bool) -> RedactionReport {
    let sessions = match store.list(None, MAX_SESSIONS_PER_SWEEP) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "manual redaction sweep list failed");
            return RedactionReport::default();
        }
    };
    let ids: Vec<String> = sessions.into_iter().map(|s| s.id).collect();
    redact_session_ids(store, &ids, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::InMemorySessionStore;

    const GH_SECRET: &str = concat!("ghp_AB", "CDEFGHIJKLMNOPQRSTUVWXYZabcdefghij1234");

    fn store_with_secret(
        owner: &str,
        updated_ago: chrono::Duration,
    ) -> (Arc<dyn SessionStore>, String) {
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let mut s = store.get_or_create("slack", owner, "agent").unwrap();
        s.add_user_message(&format!("token {GH_SECRET}"), None);
        s.updated_at = Utc::now() - updated_ago;
        store.save(&s).unwrap();
        let id = s.id.clone();
        (store, id)
    }

    #[test]
    fn manual_sweep_redacts_all_and_dry_run_does_not_mutate() {
        let (store, id) = store_with_secret("u", chrono::Duration::zero());

        let dry = run_manual_sweep(&store, true);
        assert_eq!(dry.sessions_redacted, 1);
        assert!(store.get(&id).unwrap().unwrap().messages[0]
            .content
            .contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));

        let applied = run_manual_sweep(&store, false);
        assert_eq!(applied.sessions_redacted, 1);
        assert!(!store.get(&id).unwrap().unwrap().messages[0]
            .content
            .contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
    }

    #[test]
    fn scheduled_sweep_skips_recent_and_keeps_cursor_below_skipped() {
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let ws = tempfile::tempdir().unwrap();

        let mut idle = store.get_or_create("slack", "idle", "agent").unwrap();
        idle.add_user_message(&format!("a {GH_SECRET}"), None);
        idle.updated_at = Utc::now() - chrono::Duration::hours(1);
        store.save(&idle).unwrap();

        let mut fresh = store.get_or_create("slack", "fresh", "agent").unwrap();
        fresh.add_user_message(
            concat!("b ghp_ZY", "XWVUTSRQPONMLKJIHGFEDCBA9876543210zzzz"),
            None,
        );
        fresh.updated_at = Utc::now();
        store.save(&fresh).unwrap();

        let report = run_scheduled_sweep(&store, "agent", ws.path(), Duration::from_secs(300));
        assert_eq!(report.sessions_redacted, 1);

        // The fresh session is untouched and stays above the cursor.
        assert!(store.get(&fresh.id).unwrap().unwrap().messages[0]
            .content
            .contains("ZYXWVUTSRQPONMLKJIHGFEDCBA"));
        assert!(read_cursor(ws.path()) < fresh.updated_at);

        // Once it goes idle a later sweep catches it.
        let mut aged = store.get(&fresh.id).unwrap().unwrap();
        aged.updated_at = Utc::now() - chrono::Duration::hours(1);
        store.save(&aged).unwrap();
        let report2 = run_scheduled_sweep(&store, "agent", ws.path(), Duration::from_secs(300));
        assert_eq!(report2.sessions_redacted, 1);
        assert!(!store.get(&fresh.id).unwrap().unwrap().messages[0]
            .content
            .contains("ZYXWVUTSRQPONMLKJIHGFEDCBA"));
    }
}
