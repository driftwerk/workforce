//! Re-reading an agent's configuration while it runs: what changed is
//! validated, applied to the live service, and reported.

use std::sync::Arc;

use tracing::{info, warn};

use crate::serve::{to_failover_retry, AgentService};

/// The hub's reconfiguration, as [`crate::hosts::ReconfigureHost`].
pub struct Reconfigurer;

#[async_trait::async_trait]
impl crate::hosts::ReconfigureHost for Reconfigurer {
    async fn apply(&self, service: &AgentService) -> eyre::Result<Vec<String>> {
        apply_config(service).await
    }
}

pub(crate) async fn apply_config(service: &AgentService) -> eyre::Result<Vec<String>> {
    let (workspace_config, config_text) =
        workforce_common::settings::AgentWorkspaceConfig::load_with_text(&service.workspace)?
            .unzip();
    // Nothing the API does removes the file, and falling back to the
    // install defaults would lift whatever it restricted.
    if config_text.is_none() && service.config_applied.read().is_some() {
        eyre::bail!(
            "config.toml disappeared from {}; keeping the running configuration",
            service.workspace.display()
        );
    }
    let agent_config = service.admission.agent_config();

    // Resolve the prospective provider/tier without swapping yet, so the
    // new background-task models resolve against the incoming provider.
    let crate::workspace_admission::Prospective {
        effective,
        next,
        provider_change,
        equivalences: next_equivalences,
    } = service.admission.prospective(workspace_config.as_ref())?;
    crate::subscription::validate_agent_llm(
        &effective,
        &next,
        &crate::subscription::Install::of(&service.config),
    )?;
    let deferred: Vec<String> = service
        .resolved
        .snapshot()
        .structural_diff(&next)
        .into_iter()
        .map(str::to_string)
        .collect();

    let consolidation = |m: &workforce_common::settings::MemoryConfig| {
        (m.consolidation, m.consolidation_interval_secs)
    };
    let rearm = consolidation(&service.resolved.snapshot().memory) != consolidation(&next.memory);
    service.resolved.store(next);
    if rearm {
        crate::memory_consolidation::rearm(service).await;
    }
    // Self-learning is hot-reloadable (excluded from `structural_diff`): if
    // it just flipped on and no store is open, open it now so curation can
    // start without a restart. The runner already no-ops while disabled.
    if service.self_learning_enabled() && service.self_learning_store.read().is_none() {
        crate::self_learning::ensure_store(service).await;
    }
    // Publish the live equivalences before the provider swap so its model
    // re-resolution (via `tier_registry`) sees the new mappings.
    *service.config_equivalences.write() = next_equivalences.map(Arc::new);

    // The configured default model is published before the provider swap
    // re-resolves it, keeping the live spec in sync so a later provider
    // switch resolves against the latest config rather than the value
    // captured at startup.
    *service.original_default_model.write() = effective.default_model.clone();

    service.publish_gate_policies(workspace_config.as_ref(), agent_config);
    if let Some((kind, tier)) = provider_change {
        if let Err(e) = service.reload_provider(kind, &tier).await {
            warn!(agent = %service.agent_id, error = %e, "Config reload: provider swap failed");
        }
    }

    // `reload_provider` only re-points `current_default_model` on an actual
    // provider switch (it early-returns when the provider is unchanged), so
    // a bare `[llm].default_model` edit would otherwise not take effect
    // until a restart. Re-resolve the configured default against the
    // current tier here too. Idempotent when the provider swap above
    // already updated it.
    {
        let tier_key = service.tier_key.read().clone();
        let spec = service.original_default_model.read().clone();
        let resolved = service.tier_registry().resolve_model(&spec, &tier_key);
        let mut cur = service.current_default_model.write();
        if *cur != resolved {
            info!(
                agent = %service.agent_id,
                from = %*cur,
                to = %resolved,
                spec = %spec,
                "Hot-reload: default model re-resolved",
            );
            *cur = resolved;
        }
    }

    if let Some(ref fp) = service.failover_provider {
        let next_retry = effective
            .retry
            .as_ref()
            .map(to_failover_retry)
            .unwrap_or_default();
        if fp.get_retry().await != next_retry {
            fp.set_retry(next_retry).await;
        }
    }

    // Hot-swap proactive TPM pacing so a per-agent `[llm.pacing]` edit takes
    // effect immediately, without a restart (mirrors the retry/image/video
    // hot-reloads above). The pool is held via `Arc`, so we mutate the live
    // policy in place rather than rebuilding the pool.
    if let Some(ref pool) = service.token_pool {
        pool.set_pacing(workforce_agent::llm::pool::PacingPolicy::from_config(
            &effective.pacing,
        ));
        pool.set_llm_timeouts(
            effective
                .retry
                .as_ref()
                .map(workforce_agent::llm::failover::LlmTimeouts::from)
                .unwrap_or_default(),
        );
    }

    // Hot-swap the `generate_image` tool when `[llm.image]` changes, then
    // record the config it is now built from (see `ImageReload::recorded`).
    let next_image = effective.image.clone();
    let mut image_guard = service.image_config.write();
    let outcome = workforce_agent::tools::image_gen::install_image_tool(
        &service.tools,
        service.agent_id.as_str(),
        image_guard.as_ref(),
        next_image.as_ref(),
        crate::credentials::image_keys(next_image.as_ref(), &service.vault, &service.global_vault),
    );
    *image_guard = outcome.recorded(next_image);
    drop(image_guard);
    outcome.log(service.agent_id.as_str());

    // Hot-swap the `generate_video` tool when `[llm.video]` changes.
    let next_video = effective.video.clone();
    let mut video_guard = service.video_config.write();
    let v_outcome = workforce_agent::tools::video_gen::install_video_tool(
        &service.tools,
        service.agent_id.as_str(),
        video_guard.as_ref(),
        next_video.as_ref(),
        crate::credentials::video_keys(next_video.as_ref(), &service.vault, &service.global_vault),
    );
    *video_guard = v_outcome.recorded(next_video);
    drop(video_guard);
    v_outcome.log(service.agent_id.as_str());

    // Hot-swap the `generate_speech` tool when `[llm.speech]` changes.
    let next_speech = effective.speech.clone();
    let mut speech_guard = service.speech_config.write();
    let s_outcome = workforce_agent::tools::speech_gen::install_speech_tool(
        &service.tools,
        service.agent_id.as_str(),
        speech_guard.as_ref(),
        next_speech.as_ref(),
        &service.workspace.join("piper"),
        crate::credentials::speech_keys(
            next_speech.as_ref(),
            &service.vault,
            &service.global_vault,
        ),
        Some(Arc::new(crate::hub_core::formatting::normalize_for_tts)),
        Some(crate::vault::speech_user_voice_resolver(
            service.vault.clone(),
            service.global_vault.clone(),
        )),
    );
    *speech_guard = s_outcome.recorded(next_speech);
    drop(speech_guard);
    s_outcome.log(service.agent_id.as_str());

    service.reload_coding_session();
    info!(agent = %service.agent_id, deferred = ?deferred, "Config hot-reloaded");
    *service.config_applied.write() = config_text
        .as_deref()
        .map(workforce_orchestrator::workflow_store::content_hash);
    Ok(deferred)
}
