//! One-shot batch extractor that walks an installation's session JSONL files
//! and runs the live lesson detector + extractor + validator over historical
//! turns. The output is a corpus of lessons synthesised from past mistakes,
//! ready for [`crate::lessons`] injection once it's enabled.
//!
//! Reuses every component the live path uses — `gate`, `detect_learning`,
//! `detect_learning_with_llm_fallback`, `extract_and_store`, and the same
//! `MemoryStore` dedup — so back-extracted lessons are indistinguishable from
//! lessons learned at runtime.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tracing::{debug, info, warn};

use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::runner::CURRENT_USER_ID;
use workforce_common::session::SessionMessage;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;

use crate::lessons::{detect_learning_with_llm_fallback, extract_and_store};
use crate::session_scan::{
    enumerate_session_files, iter_turns, load_session_meta, read_jsonl_messages,
};

/// Caller-supplied knobs for a backfill run.
#[derive(Debug, Clone, Default)]
pub struct BackfillOptions {
    /// Agent IDs to scan. Empty means "every agent under `data_dir/agents/`".
    pub agent_ids: Vec<AgentId>,
    /// Skip sessions whose `updated_at` is older than this cutoff.
    pub since: Option<DateTime<Utc>>,
    /// When `true`, the writable store is bypassed — extraction still runs
    /// (so the validator and detector decide what would have been stored),
    /// but nothing reaches the real backing store.
    pub dry_run: bool,
    /// Optional override for the extraction model. `None` falls through to
    /// each turn's natural model resolution.
    pub model: Option<String>,
    /// When `true`, the report includes the full content + tags of every
    /// lesson written during this run, grouped by owner. Set this for a
    /// human-readable eyeball of dry-run output.
    pub print_lessons: bool,
}

/// Per-run aggregate counters reported back to the caller.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BackfillReport {
    pub agents_scanned: usize,
    pub sessions_scanned: usize,
    pub sessions_skipped_by_filter: usize,
    pub turns_scanned: usize,
    pub turns_gated: usize,
    pub signals_detected: usize,
    pub lessons_written: usize,
    pub session_errors: usize,
    /// Populated only when [`BackfillOptions::print_lessons`] is set.
    /// Lessons written during this run, in storage order, grouped by owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub written_lessons: Vec<WrittenLesson>,
}

/// A lesson captured for human review when `--print-lessons` is set. Carries
/// just enough to eyeball quality without re-querying the store.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WrittenLesson {
    pub owner: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_session_id: Option<String>,
}

/// Run the backfill against an installation's filesystem.
///
/// The caller owns the [`LlmProvider`] and [`MemoryStore`] so this function
/// works equally well against production (real Weaviate) or a dry-run setup
/// (in-memory store).
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

    let run_started_at = Utc::now();
    let mut report = BackfillReport::default();
    for agent_id in agent_ids {
        let sessions_dir = agents_root.join(agent_id.as_str()).join("sessions");
        if !sessions_dir.is_dir() {
            debug!(agent = %agent_id, "No sessions directory; skipping");
            continue;
        }
        report.agents_scanned += 1;
        scan_agent(
            &agent_id,
            &sessions_dir,
            llm.as_ref(),
            effective_store.as_ref(),
            &opts,
            &mut report,
            Arc::clone(&effective_store),
            Arc::clone(&llm),
        )
        .await;
    }
    if opts.print_lessons {
        report.written_lessons =
            collect_written_lessons(effective_store.as_ref(), run_started_at).await;
    }
    Ok(report)
}

/// Pull every Lesson-source memory written since `run_started_at` from the
/// store, sorted by owner then by storage time. Used to populate the
/// printable section of [`BackfillReport`] when `--print-lessons` is set.
///
/// The created_after filter is applied post-query because [`InMemoryStore`]
/// (used by dry-run) doesn't honour `MemoryFilter::created_after`; doing it
/// here keeps the behaviour identical across backends.
async fn collect_written_lessons(
    store: &dyn MemoryStore,
    run_started_at: DateTime<Utc>,
) -> Vec<WrittenLesson> {
    let filter = workforce_memory::types::MemoryFilter {
        source: Some(workforce_memory::types::MemorySource::Lesson),
        system_unscoped: true,
        ..Default::default()
    };
    let memories = match store.list(filter).await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "Failed to list lessons for --print-lessons");
            return Vec::new();
        }
    };
    let mut fresh: Vec<_> = memories
        .into_iter()
        .filter(|m| m.created_at >= run_started_at)
        .collect();
    fresh.sort_by(|a, b| {
        a.owner
            .as_str()
            .cmp(b.owner.as_str())
            .then_with(|| a.created_at.cmp(&b.created_at))
    });
    fresh
        .into_iter()
        .map(|m| WrittenLesson {
            owner: m.owner.as_str().to_string(),
            content: m.content,
            tags: m.tags,
            source_session_id: m.source_session_id,
        })
        .collect()
}

/// Resolve the set of agent IDs to scan. An explicit list short-circuits; an
/// empty list enumerates `data_dir/agents/*`.
fn resolve_agent_ids(agents_root: &Path, explicit: &[AgentId]) -> Result<Vec<AgentId>> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }
    let entries = std::fs::read_dir(agents_root)
        .with_context(|| format!("reading agents dir {}", agents_root.display()))?;
    let mut ids: Vec<AgentId> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|n| AgentId::new(&n))
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(ids)
}

#[allow(clippy::too_many_arguments)]
async fn scan_agent(
    agent_id: &AgentId,
    sessions_dir: &Path,
    llm: &dyn LlmProvider,
    _store: &dyn MemoryStore,
    opts: &BackfillOptions,
    report: &mut BackfillReport,
    store_arc: Arc<dyn MemoryStore>,
    llm_arc: Arc<dyn LlmProvider>,
) {
    let session_paths = enumerate_session_files(sessions_dir);
    for session_path in session_paths {
        match scan_session(
            agent_id,
            &session_path,
            llm,
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
                    "Backfill failed for session"
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn scan_session(
    agent_id: &AgentId,
    session_path: &Path,
    llm: &dyn LlmProvider,
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

    let channel = meta.channel.clone();
    let identity_id = meta.owner.clone();
    let session_id = meta.id.clone();

    for turn in iter_turns(&messages) {
        report.turns_scanned += 1;
        let user_message = turn.user.content.clone();
        let agent_response = turn.assistant.content.clone();
        let turn_messages: Vec<SessionMessage> = turn.between.to_vec();
        let message_ids = vec![turn.user.id.clone(), turn.assistant.id.clone()];
        let event_time = turn.assistant.timestamp;

        let detected = CURRENT_USER_ID
            .scope(
                identity_id.clone(),
                detect_learning_with_llm_fallback(
                    llm,
                    &turn_messages,
                    &user_message,
                    &agent_response,
                    opts.model.as_deref(),
                ),
            )
            .await;
        if detected.is_none() {
            report.turns_gated += 1;
            continue;
        }
        report.signals_detected += 1;

        let llm_for_extract = Arc::clone(&llm_arc);
        let store_for_extract = Arc::clone(&store_arc);
        let agent_id_clone = agent_id.clone();
        let channel_clone = channel.clone();
        let session_id_clone = session_id.clone();
        let model_clone = opts.model.clone();
        let written = CURRENT_USER_ID
            .scope(identity_id.clone(), async move {
                extract_and_store(
                    llm_for_extract,
                    store_for_extract,
                    agent_id_clone,
                    &channel_clone,
                    Some(session_id_clone),
                    message_ids,
                    Some(event_time),
                    &user_message,
                    &agent_response,
                    &turn_messages,
                    model_clone,
                    None,
                    None,
                    // No live injection during historical backfill; recall
                    // hit/miss events it emits carry the original (old) turn
                    // time, so they fall outside recent dashboard windows.
                    &[],
                    workforce_lessons::lessons::ExtractionPolicy::default(),
                )
                .await
            })
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "extract_and_store failed during backfill");
                0
            });
        report.lessons_written += written;
    }
    Ok(())
}

/// Format a backfill report for stdout. Stable column layout for piping.
pub fn format_report(report: &BackfillReport) -> String {
    let mut out = format!(
        "agents scanned         : {}\n\
         sessions scanned       : {}\n\
         sessions skipped       : {}\n\
         turns scanned          : {}\n\
         turns gated            : {}\n\
         signals detected       : {}\n\
         lessons written        : {}\n\
         session errors         : {}",
        report.agents_scanned,
        report.sessions_scanned,
        report.sessions_skipped_by_filter,
        report.turns_scanned,
        report.turns_gated,
        report.signals_detected,
        report.lessons_written,
        report.session_errors,
    );
    if !report.written_lessons.is_empty() {
        out.push_str("\n\n── Lessons written ─────────────────────────────────────────");
        let mut current_owner: Option<&str> = None;
        for (i, lesson) in report.written_lessons.iter().enumerate() {
            if current_owner != Some(lesson.owner.as_str()) {
                out.push_str(&format!("\n\n[{}]", lesson.owner));
                current_owner = Some(lesson.owner.as_str());
            }
            out.push_str(&format!("\n\n  {}. {}", i + 1, lesson.content));
            if !lesson.tags.is_empty() {
                out.push_str(&format!("\n     tags: {}", lesson.tags.join(", ")));
            }
            if let Some(ref sid) = lesson.source_session_id {
                out.push_str(&format!("\n     session: {sid}"));
            }
        }
    }
    out
}

/// Log a one-line `info!` summary of the report.
pub fn log_report_summary(report: &BackfillReport) {
    info!(
        agents = report.agents_scanned,
        sessions = report.sessions_scanned,
        turns = report.turns_scanned,
        gated = report.turns_gated,
        signals = report.signals_detected,
        lessons = report.lessons_written,
        errors = report.session_errors,
        "Lessons backfill complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_ids_returns_explicit_when_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = vec![AgentId::new("allan"), AgentId::new("brian")];
        let resolved = resolve_agent_ids(dir.path(), &explicit).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_agent_ids_enumerates_when_explicit_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("not-a-dir"), "x").unwrap();
        let resolved = resolve_agent_ids(dir.path(), &[]).unwrap();
        let names: Vec<&str> = resolved.iter().map(|a| a.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    fn written(owner: &str, content: &str, tag: &str) -> WrittenLesson {
        WrittenLesson {
            owner: owner.to_string(),
            content: content.to_string(),
            tags: vec![tag.to_string()],
            source_session_id: Some("sess-abc".to_string()),
        }
    }

    #[test]
    fn format_report_omits_lesson_section_when_none_written() {
        let report = BackfillReport {
            agents_scanned: 1,
            ..Default::default()
        };
        let out = format_report(&report);
        assert!(out.contains("agents scanned"));
        assert!(!out.contains("Lessons written"));
    }

    #[test]
    fn format_report_includes_lessons_grouped_by_owner() {
        let report = BackfillReport {
            agents_scanned: 2,
            lessons_written: 3,
            written_lessons: vec![
                written("allan", "When X, do Y.", "trigger:x"),
                written("allan", "If Z then W.", "trigger:z"),
                written("brian", "Whenever Q, prefer R.", "trigger:q"),
            ],
            ..Default::default()
        };
        let out = format_report(&report);
        assert!(out.contains("── Lessons written ─"));
        assert!(out.contains("[allan]"));
        assert!(out.contains("[brian]"));
        assert!(out.contains("1. When X, do Y."));
        assert!(out.contains("2. If Z then W."));
        assert!(out.contains("3. Whenever Q, prefer R."));
        assert!(out.contains("tags: trigger:x"));
        assert!(out.contains("session: sess-abc"));
        let allan_pos = out.find("[allan]").unwrap();
        let brian_pos = out.find("[brian]").unwrap();
        assert!(
            allan_pos < brian_pos,
            "owner grouping should preserve sorted order"
        );
    }

    #[tokio::test]
    async fn collect_written_lessons_filters_by_source_and_start_time() {
        use chrono::Duration;
        use workforce_memory::in_memory::InMemoryStore;
        use workforce_memory::store::MemoryStore;
        use workforce_memory::types::{Memory, MemoryLayer, MemorySource};

        let store = InMemoryStore::new();
        let cutoff = Utc::now();

        let mut pre = Memory::new("old lesson", MemoryLayer::Semantic, AgentId::new("allan"))
            .with_source(MemorySource::Lesson)
            .with_identity_id("u1");
        pre.created_at = cutoff - Duration::seconds(60);
        store.remember(pre).await.unwrap();

        let mut fresh_lesson =
            Memory::new("fresh lesson", MemoryLayer::Semantic, AgentId::new("allan"))
                .with_source(MemorySource::Lesson)
                .with_identity_id("u1");
        fresh_lesson.created_at = cutoff + Duration::seconds(1);
        store.remember(fresh_lesson).await.unwrap();

        let mut fresh_episodic = Memory::new(
            "fresh episodic",
            MemoryLayer::Episodic,
            AgentId::new("allan"),
        )
        .with_source(MemorySource::Agent)
        .with_identity_id("u1");
        fresh_episodic.created_at = cutoff + Duration::seconds(1);
        store.remember(fresh_episodic).await.unwrap();

        let collected = collect_written_lessons(&store, cutoff).await;
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].content, "fresh lesson");
        assert_eq!(collected[0].owner, "allan");
    }
}
