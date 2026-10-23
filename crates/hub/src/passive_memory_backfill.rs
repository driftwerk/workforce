//! One-shot batch extractor that walks an installation's session JSONL files
//! and re-runs the live passive-memory extractor over historical turns.
//!
//! Why this exists: the regression fixed in commit `8872565` silently
//! dropped per-turn passive-memory extraction for an unknown window. The
//! consolidation backstop missed real user sessions because its 500-row
//! budget was burned on `workflow:*` and `subagent` rows. Forward extraction
//! is now correct, but the long-term store is sparse for the affected
//! period and won't self-heal — every recovered memory must be re-extracted
//! from the raw transcript. This module is that recovery path.
//!
//! Reuses every component the live path uses — the same `gate`,
//! `extract_and_store`, dedup window, and identity scope — so a backfilled
//! memory is indistinguishable from one written at runtime. The dedup
//! collapses near-duplicates against memories that already exist, so a
//! re-run is idempotent (modulo small score variance from the extractor LLM).

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::Result;
use tracing::{info, warn};

use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::runner::CURRENT_USER_ID;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;

use crate::passive_memory::extract_and_store;
use crate::session_scan::{
    enumerate_session_files, iter_turns, load_session_meta, read_jsonl_messages, resolve_agent_ids,
};

/// Caller-supplied knobs for a backfill run.
#[derive(Debug, Clone, Default)]
pub struct BackfillOptions {
    /// Agent IDs to scan. Empty means "every agent under `data_dir/agents/`".
    pub agent_ids: Vec<AgentId>,
    /// Skip sessions whose `updated_at` is older than this cutoff.
    pub since: Option<DateTime<Utc>>,
    /// When `true`, the writable store is bypassed — extraction still runs
    /// (so its gates and parsing are exercised), but nothing reaches the
    /// real backing store.
    pub dry_run: bool,
    /// Optional override for the extraction model. `None` falls through to
    /// each turn's natural model resolution.
    pub model: Option<String>,
}

/// Per-run aggregate counters reported back to the caller.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BackfillReport {
    pub agents_scanned: usize,
    pub sessions_scanned: usize,
    pub sessions_skipped_by_filter: usize,
    pub turns_scanned: usize,
    pub memories_written: usize,
    pub session_errors: usize,
}

/// Run the backfill against an installation's filesystem.
pub async fn run(
    data_dir: &Path,
    llm: Arc<dyn LlmProvider>,
    store: Arc<dyn MemoryStore>,
    opts: BackfillOptions,
) -> Result<BackfillReport> {
    let agents_root = data_dir.join("agents");
    let agent_ids = resolve_agent_ids(&agents_root, &opts.agent_ids)?;

    let effective_store: Arc<dyn MemoryStore> = if opts.dry_run {
        Arc::new(workforce_memory::in_memory::InMemoryStore::new())
    } else {
        store
    };

    let mut report = BackfillReport::default();
    for agent_id in agent_ids {
        let sessions_dir = agents_root.join(agent_id.as_str()).join("sessions");
        if !sessions_dir.is_dir() {
            continue;
        }
        report.agents_scanned += 1;
        scan_agent(
            &agent_id,
            &sessions_dir,
            &opts,
            &mut report,
            Arc::clone(&effective_store),
            Arc::clone(&llm),
        )
        .await;
    }
    Ok(report)
}

async fn scan_agent(
    agent_id: &AgentId,
    sessions_dir: &Path,
    opts: &BackfillOptions,
    report: &mut BackfillReport,
    store_arc: Arc<dyn MemoryStore>,
    llm_arc: Arc<dyn LlmProvider>,
) {
    for session_path in enumerate_session_files(sessions_dir) {
        match scan_session(
            agent_id,
            &session_path,
            opts,
            report,
            Arc::clone(&store_arc),
            Arc::clone(&llm_arc),
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                report.session_errors += 1;
                warn!(
                    agent = %agent_id,
                    session = %session_path.display(),
                    error = %e,
                    "Passive backfill failed for session"
                );
            }
        }
    }
}

async fn scan_session(
    agent_id: &AgentId,
    session_path: &Path,
    opts: &BackfillOptions,
    report: &mut BackfillReport,
    store_arc: Arc<dyn MemoryStore>,
    llm_arc: Arc<dyn LlmProvider>,
) -> Result<()> {
    let meta = load_session_meta(session_path)?;
    if let Some(cutoff) = opts.since {
        if meta.updated_at < cutoff {
            report.sessions_skipped_by_filter += 1;
            return Ok(());
        }
    }

    let messages = read_jsonl_messages(session_path)?;
    report.sessions_scanned += 1;
    if messages.is_empty() {
        return Ok(());
    }

    for turn in iter_turns(&messages) {
        report.turns_scanned += 1;
        let written = CURRENT_USER_ID
            .scope(meta.owner.clone(), {
                let llm = Arc::clone(&llm_arc);
                let store = Arc::clone(&store_arc);
                let agent_id = agent_id.clone();
                let channel = meta.channel.clone();
                let session_id = meta.id.clone();
                let model = opts.model.clone();
                let message_ids = vec![turn.user.id.clone(), turn.assistant.id.clone()];
                let event_time = turn.assistant.timestamp;
                let user_message = turn.user.content.clone();
                let agent_response = turn.assistant.content.clone();
                async move {
                    extract_and_store(
                        llm,
                        store,
                        agent_id,
                        &channel,
                        Some(session_id),
                        message_ids,
                        Some(event_time),
                        &user_message,
                        &agent_response,
                        model,
                        None,
                        None,
                    )
                    .await
                }
            })
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "passive extract_and_store failed during backfill");
                0
            });
        report.memories_written += written;
    }
    Ok(())
}

/// Format a backfill report for stdout. Stable column layout for piping.
pub fn format_report(report: &BackfillReport) -> String {
    format!(
        "agents scanned         : {}\n\
         sessions scanned       : {}\n\
         sessions skipped       : {}\n\
         turns scanned          : {}\n\
         memories written       : {}\n\
         session errors         : {}",
        report.agents_scanned,
        report.sessions_scanned,
        report.sessions_skipped_by_filter,
        report.turns_scanned,
        report.memories_written,
        report.session_errors,
    )
}

/// Log a one-line `info!` summary of the report.
pub fn log_report_summary(report: &BackfillReport) {
    info!(
        agents = report.agents_scanned,
        sessions = report.sessions_scanned,
        turns = report.turns_scanned,
        memories = report.memories_written,
        errors = report.session_errors,
        "Passive-memory backfill complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_report_renders_all_counters() {
        let report = BackfillReport {
            agents_scanned: 1,
            sessions_scanned: 7,
            sessions_skipped_by_filter: 2,
            turns_scanned: 84,
            memories_written: 19,
            session_errors: 0,
        };
        let out = format_report(&report);
        assert!(out.contains("agents scanned         : 1"));
        assert!(out.contains("sessions scanned       : 7"));
        assert!(out.contains("sessions skipped       : 2"));
        assert!(out.contains("turns scanned          : 84"));
        assert!(out.contains("memories written       : 19"));
        assert!(out.contains("session errors         : 0"));
    }
}
