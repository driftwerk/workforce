//! Code-only periodic scan that walks DM transcripts and scores any turns past
//! each session's cursor. Mirrors the passive-memory backfill but makes no LLM
//! call and persists relationship events.
//!
//! Restricted to DM sessions: per-message author identity is not recorded, so
//! only a 1:1 session attributes unambiguously to one human. Multi-party
//! channels are covered by the live hook, which carries the real platform user.
//!
//! Unlike the live path there is no per-turn token gate — that gate exists only
//! to bound the shared passive-memory LLM call, which this code-only pass never
//! makes; `is_noteworthy` filters flat chatter here.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::Result;
use tracing::{info, warn};

use workforce_common::channels::ConversationVisibility;
use workforce_common::session::SessionContextType;
use workforce_common::telemetry::{RelationshipCursor, TelemetryStore};
use workforce_core::types::AgentId;

use crate::relationship::capture_turn;
use crate::session_scan::{
    enumerate_session_files, iter_turns, load_session_meta, read_jsonl_messages, resolve_agent_ids,
    SessionMeta,
};

#[derive(Debug, Clone, Default)]
pub struct RelationshipScanOptions {
    /// Agent IDs to scan. Empty means "every agent under `data_dir/agents/`".
    pub agent_ids: Vec<AgentId>,
    /// Skip sessions whose `updated_at` is older than this cutoff.
    pub since: Option<DateTime<Utc>>,
    /// When `true`, scoring runs against a throwaway in-memory store.
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RelationshipScanReport {
    pub agents_scanned: usize,
    pub sessions_scanned: usize,
    pub sessions_skipped_by_filter: usize,
    pub sessions_skipped_not_dm: usize,
    pub turns_scanned: usize,
    pub turns_skipped_by_cursor: usize,
    pub events_written: usize,
    pub session_errors: usize,
}

pub fn run(
    data_dir: &Path,
    telemetry: Arc<dyn TelemetryStore>,
    opts: RelationshipScanOptions,
) -> Result<RelationshipScanReport> {
    let agents_root = data_dir.join("agents");
    let agent_ids = resolve_agent_ids(&agents_root, &opts.agent_ids)?;

    let effective: Arc<dyn TelemetryStore> = if opts.dry_run {
        Arc::new(workforce_common::persistence::SqliteStore::in_memory()?)
    } else {
        telemetry
    };

    let mut report = RelationshipScanReport::default();
    for agent_id in agent_ids {
        let sessions_dir = agents_root.join(agent_id.as_str()).join("sessions");
        if !sessions_dir.is_dir() {
            continue;
        }
        report.agents_scanned += 1;
        for session_path in enumerate_session_files(&sessions_dir) {
            if let Err(e) = scan_session(
                &agent_id,
                &session_path,
                &opts,
                effective.as_ref(),
                &mut report,
            ) {
                report.session_errors += 1;
                warn!(
                    agent = %agent_id,
                    session = %session_path.display(),
                    error = %e,
                    "Relationship scan failed for session"
                );
            }
        }
    }
    Ok(report)
}

/// Whether a persisted session is an unambiguous 1:1 DM. Prefers the explicit
/// `context_type` the live path sets; for older sessions that predate it, falls
/// back to the persisted Slack channel id. Conservative: anything we can't
/// affirmatively confirm as a 1:1 room is treated as not-DM and skipped, so a
/// multi-participant channel is never attributed to one identity.
fn is_dm_session(meta: &SessionMeta) -> bool {
    match meta.context_type {
        Some(SessionContextType::Dm) => true,
        Some(_) => false,
        None => meta
            .metadata
            .get("slack_channel_id")
            .and_then(|v| v.as_str())
            .map(|c| ConversationVisibility::from_slack_channel_id(c).is_private())
            .unwrap_or(false),
    }
}

fn scan_session(
    agent_id: &AgentId,
    session_path: &Path,
    opts: &RelationshipScanOptions,
    telemetry: &dyn TelemetryStore,
    report: &mut RelationshipScanReport,
) -> Result<()> {
    let meta = load_session_meta(session_path)?;
    if let Some(cutoff) = opts.since {
        if meta.updated_at < cutoff {
            report.sessions_skipped_by_filter += 1;
            return Ok(());
        }
    }
    if !is_dm_session(&meta) {
        report.sessions_skipped_not_dm += 1;
        return Ok(());
    }
    if meta.owner.trim().is_empty() {
        return Ok(());
    }

    let messages = read_jsonl_messages(session_path)?;
    report.sessions_scanned += 1;
    if messages.is_empty() {
        return Ok(());
    }

    let cursor_ts = telemetry
        .get_relationship_cursor(&meta.id)?
        .map(|c| c.last_message_ts);

    let mut newest: Option<(DateTime<Utc>, String)> = None;
    for turn in iter_turns(&messages) {
        if cursor_ts
            .map(|cts| turn.user.timestamp <= cts)
            .unwrap_or(false)
        {
            report.turns_skipped_by_cursor += 1;
            continue;
        }
        report.turns_scanned += 1;
        let wrote = capture_turn(
            telemetry,
            agent_id.as_str(),
            &meta.owner,
            &meta.id,
            &turn.user.id,
            turn.user.timestamp,
            &turn.user.content,
        )?;
        if wrote {
            report.events_written += 1;
        }
        newest = Some((turn.user.timestamp, turn.user.id.clone()));
    }

    if let Some((ts, id)) = newest {
        telemetry.upsert_relationship_cursor(&RelationshipCursor {
            session_id: meta.id.clone(),
            agent_id: agent_id.as_str().to_string(),
            last_message_ts: ts,
            last_message_id: id,
            updated_at: Utc::now(),
        })?;
    }
    Ok(())
}

/// Format a scan report for stdout. Stable column layout for piping.
pub fn format_report(r: &RelationshipScanReport) -> String {
    format!(
        "agents scanned          : {}\n\
         sessions scanned        : {}\n\
         sessions skipped (old)  : {}\n\
         sessions skipped (chan) : {}\n\
         turns scanned           : {}\n\
         turns skipped (cursor)  : {}\n\
         events written          : {}\n\
         session errors          : {}",
        r.agents_scanned,
        r.sessions_scanned,
        r.sessions_skipped_by_filter,
        r.sessions_skipped_not_dm,
        r.turns_scanned,
        r.turns_skipped_by_cursor,
        r.events_written,
        r.session_errors,
    )
}

pub fn log_report_summary(r: &RelationshipScanReport) {
    info!(
        agents = r.agents_scanned,
        sessions = r.sessions_scanned,
        turns = r.turns_scanned,
        events = r.events_written,
        errors = r.session_errors,
        "Relationship scan complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::persistence::SqliteStore;
    use workforce_common::session::{MessageRole, SessionMessage};

    fn message(id: &str, role: MessageRole, content: &str, ts: DateTime<Utc>) -> SessionMessage {
        SessionMessage {
            id: id.to_string(),
            role,
            content: content.to_string(),
            tool_data: None,
            timestamp: ts,
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
        }
    }

    fn write_session(
        sessions_dir: &Path,
        id: &str,
        owner: &str,
        context_type: Option<&str>,
        slack_channel_id: Option<&str>,
        msgs: &[SessionMessage],
    ) {
        std::fs::create_dir_all(sessions_dir).unwrap();
        let updated = msgs
            .iter()
            .map(|m| m.timestamp)
            .max()
            .unwrap_or_else(Utc::now);
        let metadata = slack_channel_id
            .map(|c| serde_json::json!({ "slack_channel_id": c }))
            .unwrap_or(serde_json::Value::Null);
        let meta = serde_json::json!({
            "id": id,
            "channel": "slack",
            "owner": owner,
            "agent_id": "atlas",
            "kind": "main",
            "context_type": context_type,
            "updated_at": updated.to_rfc3339(),
            "metadata": metadata,
            "total_tokens": 0,
        });
        std::fs::write(
            sessions_dir.join(format!("{id}.meta.json")),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        let jsonl: String = msgs
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(sessions_dir.join(format!("{id}.jsonl")), jsonl).unwrap();
    }

    #[test]
    fn scans_dm_skips_channel_and_is_cursor_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("agents").join("atlas").join("sessions");
        let t0 = Utc::now() - chrono::Duration::hours(2);

        write_session(
            &sessions,
            "dm-1",
            "alice",
            Some("dm"),
            None,
            &[
                message("u1", MessageRole::User, "perfect, thanks!", t0),
                message("a1", MessageRole::Assistant, "glad it helped", t0),
                message(
                    "u2",
                    MessageRole::User,
                    "no, that's wrong",
                    t0 + chrono::Duration::minutes(5),
                ),
                message(
                    "a2",
                    MessageRole::Assistant,
                    "sorry, fixing",
                    t0 + chrono::Duration::minutes(5),
                ),
            ],
        );
        write_session(
            &sessions,
            "chan-1",
            "C0CHANNEL",
            Some("group"),
            None,
            &[
                message("u3", MessageRole::User, "thanks team", t0),
                message("a3", MessageRole::Assistant, "welcome", t0),
            ],
        );

        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let report = run(
            dir.path(),
            store.clone() as Arc<dyn TelemetryStore>,
            RelationshipScanOptions::default(),
        )
        .unwrap();

        assert_eq!(report.sessions_scanned, 1, "only the DM session is scanned");
        assert_eq!(report.sessions_skipped_not_dm, 1);
        assert_eq!(report.turns_scanned, 2);
        assert_eq!(report.events_written, 2);

        let events = store
            .relationship_events_in_range(
                Some("atlas"),
                Some("alice"),
                t0 - chrono::Duration::hours(1),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        // Channel turn must not have been attributed to anyone.
        assert!(store
            .relationship_events_in_range(
                Some("atlas"),
                Some("C0CHANNEL"),
                t0 - chrono::Duration::hours(1),
                Utc::now(),
            )
            .unwrap()
            .is_empty());

        // Second run: the cursor skips every already-scored turn.
        let again = run(
            dir.path(),
            store.clone() as Arc<dyn TelemetryStore>,
            RelationshipScanOptions::default(),
        )
        .unwrap();
        assert_eq!(again.turns_scanned, 0);
        assert_eq!(again.turns_skipped_by_cursor, 2);
        assert_eq!(again.events_written, 0);
    }

    fn meta(
        context_type: Option<SessionContextType>,
        slack_channel_id: Option<&str>,
    ) -> SessionMeta {
        SessionMeta {
            id: "s".into(),
            channel: "slack".into(),
            owner: "u".into(),
            updated_at: Utc::now(),
            context_type,
            metadata: slack_channel_id
                .map(|c| serde_json::json!({ "slack_channel_id": c }))
                .unwrap_or(serde_json::Value::Null),
        }
    }

    #[test]
    fn is_dm_session_prefers_context_type_then_slack_id() {
        assert!(is_dm_session(&meta(Some(SessionContextType::Dm), None)));
        assert!(!is_dm_session(&meta(
            Some(SessionContextType::Group),
            Some("D9")
        )));
        // Pre-context_type sessions fall back to the persisted Slack channel id.
        assert!(is_dm_session(&meta(None, Some("D9"))));
        assert!(!is_dm_session(&meta(None, Some("C9"))));
        // Unknowable → skipped (privacy-safe default).
        assert!(!is_dm_session(&meta(None, None)));
    }

    #[test]
    fn scans_historical_dm_via_persisted_slack_channel_id() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("agents").join("atlas").join("sessions");
        let t0 = Utc::now() - chrono::Duration::hours(2);
        // No context_type (predates the live populate), but a persisted DM channel id.
        write_session(
            &sessions,
            "old-dm",
            "carol",
            None,
            Some("D7HISTORIC"),
            &[
                message("u1", MessageRole::User, "amazing, thank you!", t0),
                message("a1", MessageRole::Assistant, "anytime", t0),
            ],
        );
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let report = run(
            dir.path(),
            store.clone() as Arc<dyn TelemetryStore>,
            RelationshipScanOptions::default(),
        )
        .unwrap();
        assert_eq!(report.sessions_scanned, 1);
        assert_eq!(report.events_written, 1);
        assert_eq!(
            store
                .relationship_events_in_range(
                    Some("atlas"),
                    Some("carol"),
                    t0 - chrono::Duration::hours(1),
                    Utc::now(),
                )
                .unwrap()
                .len(),
            1
        );
    }
}
