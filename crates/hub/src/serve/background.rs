//! The watchers and sweeps that run for as long as the service does.

use super::AgentService;
use std::sync::Arc;

/// The watchers and sweeps that run for as long as the service does.
pub(super) fn start_sweeps(service: &Arc<AgentService>) {
    // FsWatcher: hot-reload config.toml, written in place or replaced by a
    // rename, when provider/model settings change.
    {
        let config_svc = Arc::downgrade(service);
        service
            .watches
            .lock()
            .extend(crate::hub_core::fs_watcher::spawn_on_change(
                "config.toml",
                crate::hub_core::fs_watcher::FsWatcherConfig {
                    paths: vec![service.workspace.to_string_lossy().to_string()],
                    patterns: vec!["config.toml".to_string()],
                    debounce_ms: 1000,
                    recursive: false,
                },
                move || {
                    config_svc.upgrade().map(|svc| async move {
                        match svc.reload_config().await {
                            Ok(deferred) if !deferred.is_empty() => tracing::warn!(
                                fields = ?deferred,
                                "Config hot-reload applied; these fields require a restart"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "Config hot-reload failed"),
                        }
                    })
                },
            ));
    }

    // Background session scanning for memory consolidation
    if service.effective_memory().consolidation {
        let scan_service = Arc::clone(service);
        let scan_interval = service.effective_memory().consolidation_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(scan_interval));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                if !scan_service.leader_gate.is_leader() {
                    continue; // shared stores — one consolidator per fleet
                }
                let model = scan_service
                    .effective_memory()
                    .passive_model
                    .clone()
                    .or_else(|| {
                        scan_service
                            .current_compaction_config()
                            .compaction_model
                            .clone()
                    });
                let Some(ref store) = scan_service.memory_store else {
                    tracing::debug!(
                        "Session scan skipped — no memory store configured for this agent"
                    );
                    continue;
                };
                match crate::memory_consolidation::scan_sessions_and_extract(
                    &scan_service,
                    Arc::clone(store),
                    model,
                )
                .await
                {
                    Ok(report) => {
                        if report.sessions_scanned > 0 {
                            tracing::info!(
                                sessions = report.sessions_scanned,
                                turns = report.turns_processed,
                                stored = report.memories_stored,
                                "Session scan complete"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "Session scan failed"),
                }
            }
        });
    }

    // Background lesson decay — sinks idle lessons so stale gotchas stop
    // being injected as the corpus grows. Daily cadence; `lesson_decay_step`
    // is per-pass, so a lesson idle past the threshold fades over ~weeks.
    if service.effective_memory().lesson_decay {
        let decay_service = Arc::clone(service);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(86_400));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                if !decay_service.leader_gate.is_leader() {
                    continue; // shared corpus — decaying twice doubles the rate
                }
                let Some(ref store) = decay_service.memory_store else {
                    continue;
                };
                let mem = decay_service.effective_memory();
                if !mem.lesson_decay {
                    continue; // honour a runtime disable via config overlay
                }
                let config = crate::lessons::DecayConfig {
                    idle_days: mem.lesson_decay_idle_days,
                    step: mem.lesson_decay_step,
                    prune_floor: mem.lesson_decay_prune_floor,
                };
                if let Err(e) = crate::lessons::decay_pass(
                    store.as_ref(),
                    decay_service.telemetry_store.as_ref(),
                    &decay_service.agent_id,
                    config,
                    chrono::Utc::now(),
                )
                .await
                {
                    tracing::warn!(error = %e, "Lesson decay pass failed");
                }
            }
        });
    }

    // Background session secret redaction — security hygiene, always
    // on, independent of memory consolidation. Runs in-process so the
    // store stays cache-coherent; blocking file I/O is offloaded to a
    // blocking thread so it never stalls the async runtime.
    {
        let redact_service = Arc::clone(service);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(crate::session_redaction::DEFAULT_INTERVAL);
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                let store = Arc::clone(&redact_service.sessions);
                let agent_id = redact_service.agent_id.clone();
                let workspace = redact_service.workspace.clone();
                let report = tokio::task::spawn_blocking(move || {
                    crate::session_redaction::run_scheduled_sweep(
                        &store,
                        agent_id.as_str(),
                        &workspace,
                        crate::session_redaction::SCHEDULED_IDLE,
                    )
                })
                .await
                .unwrap_or_default();
                if report.sessions_redacted > 0 {
                    tracing::info!(
                        target: "sentinel.audit",
                        sessions = report.sessions_redacted,
                        messages = report.messages_redacted,
                        archive = report.archive_messages_redacted,
                        "scheduled session redaction swept"
                    );
                }
            }
        });
    }
}
