//! Hot-swappable per-agent runtime config.
//!
//! The runtime knobs an agent reads every turn — `[memory]`, `[compaction]`,
//! `[advisor]` — are workspace-overridable via `{workspace}/config.toml`.
//! [`ResolvedConfig`] bundles their merged, tier-resolved values behind a
//! [`ConfigCell`] so an edit to that file takes effect on the next turn
//! without a hub restart (see `AgentService::reload_config`).
//!
//! Out of scope by design: structural config that needs a new connection or
//! re-index (memory backend / `db_path`, knowledge root), and the LLM
//! provider/model, which is hot-swapped separately via
//! `AgentService::reload_provider`.

use std::collections::HashMap;
use std::sync::Arc;

use workforce_agent::llm::failover::LlmTimeouts;
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::model_router::RoutingConfig;
use workforce_common::session::CompactionConfig;
use workforce_common::settings::{
    AdvisorConfig, AgentConfig, AgentWorkspaceConfig, AwsS3Config, ClaimGroundingConfig,
    CodeContextConfig, LoginPolicy, SessionDigestConfig, WorkforceConfig,
};

/// The merged, tier-resolved runtime knobs an agent reads per turn.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub memory: workforce_common::settings::MemoryConfig,
    pub compaction: CompactionConfig,
    pub spill: workforce_common::session::SpillConfig,
    pub advisor: AdvisorConfig,
    /// Adaptive-routing config from `[llm.routing]`. The per-turn baseline; a
    /// runtime `active_routing` override (set via the routing tool) wins over it.
    pub routing: RoutingConfig,
    /// Per-task-type model map from `[llm.model_preferences]`.
    pub model_preferences: Option<HashMap<String, String>>,
    /// `[llm] voice_model` — the model for turns that arrive over voice. Held
    /// as the raw spec and resolved at pick time (like `model_preferences`,
    /// and unlike the background-task models below) so it routes against the
    /// provider live at that moment rather than the one present at reload.
    pub voice_model: Option<String>,
    /// `[llm] voice_thinking_budget_tokens` — the reasoning budget for voice
    /// turns. Resolved the same workspace → agent-section way as
    /// `voice_model`, and applied independently of it.
    pub voice_thinking_budget_tokens: Option<u32>,
    /// `[llm] effort` as written, resolved workspace → agent section like
    /// `voice_model`. Read every turn ([`Self::effort_level`]), so not
    /// structural; admission refuses a level the provider does not take.
    pub effort: Option<String>,
    /// `[aws_s3]` — surfaced here only so [`Self::structural_diff`] can
    /// report when an edit needs a restart. The hub doesn't read this
    /// per-turn (tools take their config at registration time); a
    /// hot-reload of any aws_s3 field is structural in v1.
    pub aws_s3: AwsS3Config,
    /// `[worker_read_sources] enabled`, resolved per-agent (workspace → agent
    /// section → global). Read live each time a worker sub-agent is spawned
    /// (`run_isolated`), so a per-agent `config.toml` edit takes effect on the
    /// next spawned worker without a restart — hence NOT in `structural_diff`.
    pub worker_read_sources_enabled: bool,
    /// `[self_learning]`, resolved per-agent (workspace override → global).
    /// Read live each turn (the runner gate, context injection, and `record`
    /// tool all check `self_learning.enabled`), so a per-agent `config.toml`
    /// edit takes effect on the next tick/turn without a restart — hence NOT
    /// in `structural_diff`.
    pub self_learning: workforce_common::settings::SelfLearningConfig,
    /// `[claim_grounding]`, resolved per-agent (workspace override → global).
    /// Read live each turn (`claim_grounding_block`), so a per-agent `config.toml`
    /// edit takes effect on the next turn without a restart — hence NOT in
    /// `structural_diff`.
    pub claim_grounding: ClaimGroundingConfig,
    /// `[code_context]`, resolved per-agent (workspace override → global). Read
    /// live before each turn's first model call (the answer-ready context
    /// injection gate), so a per-agent `config.toml` edit takes effect on the
    /// next turn without a restart — hence NOT in `structural_diff`. ON by
    /// default.
    pub code_context: CodeContextConfig,
    /// `[sessions]` digest knobs, resolved per-agent (workspace override →
    /// global `digest_*` fields). Read once per session open (the
    /// recent-sessions digest gate), so a per-agent `config.toml` edit takes
    /// effect on the next session without a restart — hence NOT in
    /// `structural_diff`. ON by default.
    pub session_digest: SessionDigestConfig,
    /// `[tools.programs]` from the agent's `config.toml`: whether and how it
    /// runs Sift programs, each field over the install's. Read each turn, so
    /// an edit takes effect on the next turn — hence NOT in `structural_diff`.
    pub programs: Option<workforce_common::settings::AgentProgramsConfig>,
    /// `[tools.compile_gate]` from the agent's `config.toml`, each field over
    /// the install's. Read each turn.
    pub compile_gate: Option<workforce_common::settings::AgentCompileGateConfig>,
    /// `[tools.refactor]` from the agent's `config.toml`, over the install's.
    /// Read each turn.
    pub refactor: Option<workforce_common::settings::AgentRefactorConfig>,
    /// `[know_how]` from the agent's `config.toml`, each field over the
    /// install's. Read each turn.
    pub know_how: Option<workforce_common::settings::AgentKnowHowConfig>,
    /// `[decision]` from the agent's `config.toml`, over the install's kill
    /// switch. Read each time a decision is recorded.
    pub decision: Option<workforce_common::settings::AgentDecisionConfig>,
    /// `[rich]` from the agent's `config.toml`: the rich blocks it may be
    /// taught. Read each turn, so an edit takes effect on the next one.
    pub rich: workforce_common::rich::RichFeatures,
    /// `[scheduler]` from the agent's `config.toml`, over the install's kill
    /// switch. Read each time a job runs.
    pub scheduler: Option<workforce_common::settings::AgentSchedulerConfig>,
    /// `[research]` from the agent's `config.toml`, over the install's kill
    /// switch. Read on each `investigate` call.
    pub research: Option<workforce_common::settings::AgentResearchConfig>,
    /// `[handoff]` from the agent's `config.toml`, over the install's kill
    /// switch. Read on each hand-over.
    pub handoff: Option<workforce_common::settings::AgentHandoffConfig>,
    /// `[attachments.video]` from the agent's `config.toml`, each field over
    /// the install's. Read each turn.
    pub video_frames: Option<workforce_common::settings::AgentVideoFramesConfig>,
    /// `[llm.logins]`, resolved per-agent (workspace → agent section →
    /// global). Read on every subscription login lookup, so a per-agent
    /// `config.toml` edit takes effect on the next call — hence NOT in
    /// `structural_diff`.
    pub logins: LoginPolicy,
    /// `[llm.retry]` watchdog windows, resolved per-agent (workspace → agent
    /// section → global). Read by each one-shot subscription completion to
    /// finish under the chain's request cap — hence NOT in `structural_diff`.
    pub llm_timeouts: LlmTimeouts,
    /// The tier tables the agent's models resolve through, its equivalences
    /// applied.
    pub tiers: TierRegistry,
    /// This agent's own environments, under the install's kill switch. Read
    /// each turn.
    pub environments: crate::hub_core::environments::ResolvedEnvironments,
    /// The agent's `config.toml` as it was read for this config, kept so the
    /// settings report names which values it set from what the runtime
    /// loaded, never from a later state of the file.
    pub workspace: Option<AgentWorkspaceConfig>,
}

impl ResolvedConfig {
    /// Merge global → agent-section → workspace config for the runtime knobs,
    /// then resolve background-task model tiers (`passive_model`,
    /// `consolidation_model`, `lesson_extract_model`, `compaction_model`,
    /// `self_learning.model`, `claim_grounding.model`) against `tier_key`.
    ///
    /// `tier_key` is supplied by the caller (it lives behind the provider
    /// hot-swap lock), so a reload resolves models against the current
    /// provider rather than the one present at startup.
    pub fn resolve(
        agent_config: Option<&AgentConfig>,
        workspace_config: Option<&AgentWorkspaceConfig>,
        global: &WorkforceConfig,
        tier_key: &str,
        equivalences: Option<&HashMap<String, Vec<String>>>,
    ) -> Self {
        let base_memory = agent_config
            .map(|a| a.effective_memory(&global.memory))
            .unwrap_or_else(|| global.memory.clone());
        let mut memory = match workspace_config {
            Some(ws) => ws.merge_memory(&base_memory),
            None => base_memory,
        };

        let base_compaction = agent_config
            .map(|a| a.effective_compaction(&global.compaction))
            .unwrap_or_else(|| global.compaction.clone());
        let mut compaction = match workspace_config {
            Some(ws) => ws.merge_compaction(&base_compaction),
            None => base_compaction,
        };

        let base_advisor = agent_config
            .map(|a| a.effective_advisor(&global.advisor))
            .unwrap_or_else(|| global.advisor.clone());
        let advisor = match workspace_config {
            Some(ws) => ws.merge_advisor(&base_advisor),
            None => base_advisor,
        };

        let tiers = TierRegistry::new(equivalences);
        resolve_routed_field(&mut compaction.compaction_model, &tiers, tier_key);
        resolve_routed_field(&mut memory.passive_model, &tiers, tier_key);
        resolve_routed_field(&mut memory.consolidation_model, &tiers, tier_key);
        resolve_routed_field(&mut memory.lesson_extract_model, &tiers, tier_key);

        let workspace_llm = workspace_config.and_then(|ws| ws.llm.as_ref());
        let routing = workspace_llm
            .and_then(|llm| llm.routing.as_ref())
            .map(RoutingConfig::from)
            .unwrap_or_default();
        let model_preferences = workspace_llm.and_then(|llm| llm.model_preferences.clone());
        let agent_llm = agent_config.and_then(|a| a.llm.as_ref());
        let voice_model = workspace_llm
            .and_then(|llm| llm.voice_model.clone())
            .or_else(|| agent_llm.and_then(|llm| llm.voice_model.clone()));
        let voice_thinking_budget_tokens = workspace_llm
            .and_then(|llm| llm.voice_thinking_budget_tokens)
            .or_else(|| agent_llm.and_then(|llm| llm.voice_thinking_budget_tokens));
        let effort = workspace_llm
            .and_then(|llm| llm.effort.clone())
            .or_else(|| agent_llm.and_then(|llm| llm.effort.clone()))
            .filter(|effort| !effort.trim().is_empty());

        // Per-agent workspace `config.toml` override → global default. (The
        // workspace file is the fs-watched, hot-reloadable per-agent config.)
        let worker_read_sources_enabled = workspace_config
            .and_then(|ws| ws.worker_read_sources.as_ref())
            .map(|c| c.enabled)
            .unwrap_or(global.worker_read_sources.enabled);

        let mut self_learning = workspace_config
            .and_then(|ws| ws.self_learning.clone())
            .unwrap_or_else(|| global.self_learning.clone());
        resolve_routed_field(&mut self_learning.model, &tiers, tier_key);

        let mut claim_grounding = workspace_config
            .and_then(|ws| ws.claim_grounding.clone())
            .unwrap_or_else(|| global.claim_grounding.clone());
        resolve_routed_field(&mut claim_grounding.model, &tiers, tier_key);

        let code_context = workspace_config
            .and_then(|ws| ws.code_context.clone())
            .unwrap_or_else(|| global.code_context.clone());

        let session_digest = workspace_config
            .and_then(|ws| ws.session_digest.clone())
            .unwrap_or_else(|| global.sessions.digest_settings());

        let programs = workspace_config
            .and_then(AgentWorkspaceConfig::programs)
            .cloned();

        let llm = crate::serve::merged_effective_llm(agent_config, workspace_config, &global.llm);
        let llm_timeouts = llm
            .retry
            .as_ref()
            .map(LlmTimeouts::from)
            .unwrap_or_default();

        Self {
            memory,
            compaction,
            spill: global.spill.clone(),
            advisor,
            routing,
            model_preferences,
            voice_model,
            voice_thinking_budget_tokens,
            effort,
            aws_s3: global.aws_s3.clone(),
            worker_read_sources_enabled,
            self_learning,
            claim_grounding,
            code_context,
            session_digest,
            programs,
            compile_gate: workspace_config
                .and_then(AgentWorkspaceConfig::compile_gate)
                .cloned(),
            refactor: workspace_config
                .and_then(AgentWorkspaceConfig::refactor)
                .cloned(),
            know_how: workspace_config.and_then(|w| w.know_how.clone()),
            decision: workspace_config.and_then(|w| w.decision),
            rich: workspace_config.and_then(|w| w.rich).unwrap_or_default(),
            scheduler: workspace_config.and_then(|w| w.scheduler),
            research: workspace_config.and_then(|w| w.research.clone()),
            handoff: workspace_config.and_then(|w| w.handoff.clone()),
            video_frames: workspace_config
                .and_then(AgentWorkspaceConfig::video_frames)
                .cloned(),
            logins: llm.logins,
            llm_timeouts,
            tiers,
            environments: crate::hub_core::environments::ResolvedEnvironments::resolve(
                &global.environments,
                workspace_config.and_then(|w| w.environments.as_ref()),
            ),
            workspace: workspace_config.cloned(),
        }
    }

    /// Names of fields whose change cannot be hot-applied because they are
    /// baked into a resource built at startup (the memory store connection),
    /// so a restart is required. Consolidation is not among them: its job is
    /// re-armed and its scan reads the live config. Returned
    /// so the operator can be told which parts of their edit are deferred.
    /// The `[llm] effort` level the agent's turns run at, when it names one.
    pub fn effort_level(&self) -> Option<workforce_agent::llm::anthropic::Effort> {
        self.effort
            .as_deref()
            .and_then(workforce_agent::llm::anthropic::Effort::parse)
    }

    pub fn structural_diff(&self, next: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.memory.backend != next.memory.backend {
            changed.push("memory.backend");
        }
        if self.memory.db_path != next.memory.db_path {
            changed.push("memory.db_path");
        }
        // Applied to the store at construction (`with_min_certainty`).
        if self.memory.recall_min_certainty != next.memory.recall_min_certainty {
            changed.push("memory.recall_min_certainty");
        }
        if self.aws_s3.enabled != next.aws_s3.enabled {
            changed.push("aws_s3.enabled");
        }
        if self.aws_s3.bucket_allowlist != next.aws_s3.bucket_allowlist {
            changed.push("aws_s3.bucket_allowlist");
        }
        if self.aws_s3.assume_role_arn != next.aws_s3.assume_role_arn {
            changed.push("aws_s3.assume_role_arn");
        }
        if self.aws_s3.max_get_bytes != next.aws_s3.max_get_bytes {
            changed.push("aws_s3.max_get_bytes");
        }
        if self.aws_s3.require_approval_buckets != next.aws_s3.require_approval_buckets {
            changed.push("aws_s3.require_approval_buckets");
        }
        changed
    }
}

pub fn resolve_routed_field(field: &mut Option<String>, tiers: &TierRegistry, tier_key: &str) {
    if let Some(val) = field.as_deref() {
        let (routing_key, model) = tiers.resolve_routed_model(val, tier_key);
        *field = Some(TierRegistry::format_routed(routing_key.as_deref(), model));
    }
}

/// A hot-swappable cell holding an agent's current [`ResolvedConfig`].
///
/// Reads take the lock only long enough to clone the inner `Arc`, returning a
/// pinned snapshot so a turn sees one consistent config even if a reload lands
/// mid-turn. Cloning the cell shares the same underlying slot, so tools handed
/// a clone observe reloads live.
#[derive(Clone)]
pub struct ConfigCell(Arc<parking_lot::RwLock<Arc<ResolvedConfig>>>);

impl ConfigCell {
    pub fn new(config: ResolvedConfig) -> Self {
        Self(Arc::new(parking_lot::RwLock::new(Arc::new(config))))
    }

    /// Pin the current config for the duration of a turn.
    pub fn snapshot(&self) -> Arc<ResolvedConfig> {
        self.0.read().clone()
    }

    /// Atomically replace the current config.
    pub fn store(&self, config: ResolvedConfig) {
        *self.0.write() = Arc::new(config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::settings::{AgentRecallScope, MemoryConfig};

    fn resolved(scope: AgentRecallScope, backend: &str) -> ResolvedConfig {
        ResolvedConfig {
            memory: MemoryConfig {
                agent_recall_scope: scope,
                backend: backend.to_string(),
                ..Default::default()
            },
            compaction: CompactionConfig::default(),
            spill: workforce_common::session::SpillConfig::default(),
            advisor: AdvisorConfig::default(),
            routing: RoutingConfig::default(),
            model_preferences: None,
            voice_model: None,
            voice_thinking_budget_tokens: None,
            effort: None,
            aws_s3: AwsS3Config::default(),
            worker_read_sources_enabled: false,
            self_learning: workforce_common::settings::SelfLearningConfig::default(),
            claim_grounding: ClaimGroundingConfig::default(),
            code_context: CodeContextConfig::default(),
            session_digest: SessionDigestConfig::default(),
            programs: None,
            compile_gate: None,
            refactor: None,
            know_how: None,
            decision: None,
            rich: Default::default(),
            scheduler: None,
            research: None,
            handoff: None,
            video_frames: None,
            logins: LoginPolicy::default(),
            llm_timeouts: LlmTimeouts::default(),
            tiers: TierRegistry::new(None),
            environments: Default::default(),
            workspace: None,
        }
    }

    #[test]
    fn resolve_carries_global_memory_when_no_overrides() {
        let mut global = WorkforceConfig::default();
        global.memory.agent_recall_scope = AgentRecallScope::Isolated;
        let cfg = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(cfg.memory.agent_recall_scope, AgentRecallScope::Isolated);
        // No workspace block ⇒ routing defaults, no model preferences.
        assert!(!cfg.routing.enable_adaptive_routing);
        assert!(cfg.model_preferences.is_none());
    }

    #[test]
    fn resolve_reads_routing_and_model_preferences_from_workspace() {
        let ws = AgentWorkspaceConfig {
            llm: Some(workforce_common::settings::AgentLlmConfig {
                routing: Some(workforce_common::settings::WorkspaceRoutingConfig {
                    enable_adaptive_routing: Some(true),
                    coding_switch_round: Some(42),
                    ..Default::default()
                }),
                model_preferences: Some(HashMap::from([(
                    "coding".to_string(),
                    "anthropic:powerful".to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(
            None,
            Some(&ws),
            &WorkforceConfig::default(),
            "anthropic",
            None,
        );
        assert!(cfg.routing.enable_adaptive_routing);
        assert_eq!(cfg.routing.coding_switch_round, 42);
        assert_eq!(
            cfg.model_preferences
                .as_ref()
                .and_then(|m| m.get("coding"))
                .map(String::as_str),
            Some("anthropic:powerful")
        );
    }

    /// `[llm] voice_model` is read from the agent's workspace `config.toml`
    /// first, falling back to the `[[agents]]` section — and is kept as the
    /// raw spec, since `pick_model_source` routes it against the live tier.
    #[test]
    fn resolve_reads_voice_model_from_workspace_then_agent_section() {
        let agent = AgentConfig {
            id: "allan".to_string(),
            llm: Some(workforce_common::settings::AgentLlmConfig {
                voice_model: Some("cheap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let global = WorkforceConfig::default();

        // Agent section alone.
        let cfg = ResolvedConfig::resolve(Some(&agent), None, &global, "anthropic", None);
        assert_eq!(cfg.voice_model.as_deref(), Some("cheap"));

        // Workspace wins over the agent section.
        let ws = AgentWorkspaceConfig {
            llm: Some(workforce_common::settings::AgentLlmConfig {
                voice_model: Some("openrouter:anthropic:cheap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(Some(&agent), Some(&ws), &global, "anthropic", None);
        assert_eq!(
            cfg.voice_model.as_deref(),
            Some("openrouter:anthropic:cheap")
        );

        // Neither set ⇒ no override.
        let cfg = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(cfg.voice_model.is_none());
    }

    /// The voice thinking budget resolves the same way and is independent of
    /// `voice_model` — and `0` must survive as `Some(0)` ("don't think"),
    /// never collapse into `None` ("no override").
    #[test]
    fn resolve_reads_voice_thinking_budget_independently_of_voice_model() {
        let global = WorkforceConfig::default();
        let ws = AgentWorkspaceConfig {
            llm: Some(workforce_common::settings::AgentLlmConfig {
                voice_thinking_budget_tokens: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert_eq!(cfg.voice_thinking_budget_tokens, Some(0));
        assert!(cfg.voice_model.is_none(), "set independently of the model");

        // Agent section is the fallback; workspace wins.
        let agent = AgentConfig {
            id: "allan".to_string(),
            llm: Some(workforce_common::settings::AgentLlmConfig {
                voice_thinking_budget_tokens: Some(4_000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(Some(&agent), None, &global, "anthropic", None);
        assert_eq!(cfg.voice_thinking_budget_tokens, Some(4_000));
        let cfg = ResolvedConfig::resolve(Some(&agent), Some(&ws), &global, "anthropic", None);
        assert_eq!(cfg.voice_thinking_budget_tokens, Some(0));

        let cfg = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(cfg.voice_thinking_budget_tokens.is_none());
    }

    #[test]
    fn structural_diff_flags_only_restart_required_fields() {
        let base = resolved(AgentRecallScope::Shared, "sqlite");

        // Tier-1 knob change is not structural.
        let scope_only = resolved(AgentRecallScope::Isolated, "sqlite");
        assert!(base.structural_diff(&scope_only).is_empty());

        // Backend change requires a restart.
        let backend_changed = resolved(AgentRecallScope::Shared, "weaviate");
        assert_eq!(
            base.structural_diff(&backend_changed),
            vec!["memory.backend"]
        );

        // recall_min_certainty is baked into the store at construction.
        let mut certainty_changed = resolved(AgentRecallScope::Shared, "sqlite");
        certainty_changed.memory.recall_min_certainty = Some(0.7);
        assert_eq!(
            base.structural_diff(&certainty_changed),
            vec!["memory.recall_min_certainty"]
        );
    }

    #[test]
    fn structural_diff_flags_aws_s3_changes_so_operator_is_told_to_restart() {
        let mut base = resolved(AgentRecallScope::Shared, "sqlite");
        let mut next = resolved(AgentRecallScope::Shared, "sqlite");
        next.aws_s3.bucket_allowlist = vec!["b".into()];
        next.aws_s3.assume_role_arn = Some("arn".into());
        next.aws_s3.max_get_bytes = 1;
        next.aws_s3.require_approval_buckets = vec!["b".into()];
        next.aws_s3.enabled = true;
        let mut diff = base.structural_diff(&next);
        diff.sort_unstable();
        assert_eq!(
            diff,
            vec![
                "aws_s3.assume_role_arn",
                "aws_s3.bucket_allowlist",
                "aws_s3.enabled",
                "aws_s3.max_get_bytes",
                "aws_s3.require_approval_buckets",
            ],
        );
        base.aws_s3 = next.aws_s3.clone();
        assert!(base.structural_diff(&next).is_empty());
    }

    #[test]
    fn worker_read_sources_resolves_per_agent_over_global() {
        use workforce_common::settings::WorkerReadSourcesConfig;

        let mut global = WorkforceConfig::default();
        global.worker_read_sources.enabled = false;

        // No override → inherits global (off).
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(!base.worker_read_sources_enabled);

        // Per-agent workspace override turns it on without touching global.
        let ws = AgentWorkspaceConfig {
            worker_read_sources: Some(WorkerReadSourcesConfig { enabled: true }),
            ..Default::default()
        };
        let on = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert!(on.worker_read_sources_enabled);

        // Workspace can also explicitly turn it OFF even when global is on.
        global.worker_read_sources.enabled = true;
        let ws_off = AgentWorkspaceConfig {
            worker_read_sources: Some(WorkerReadSourcesConfig { enabled: false }),
            ..Default::default()
        };
        let off = ResolvedConfig::resolve(None, Some(&ws_off), &global, "anthropic", None);
        assert!(!off.worker_read_sources_enabled);
    }

    #[test]
    fn claim_grounding_resolves_per_agent_over_global() {
        use workforce_common::settings::ClaimGroundingConfig;

        let mut global = WorkforceConfig::default();
        assert!(!global.claim_grounding.enabled);

        // No override → inherits global (off).
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(!base.claim_grounding.enabled);
        assert!(!base.claim_grounding.data_claims);

        // Per-agent workspace override turns the fields on without touching global.
        let ws = AgentWorkspaceConfig {
            claim_grounding: Some(ClaimGroundingConfig {
                enabled: true,
                data_claims: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let on = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert!(on.claim_grounding.enabled);
        assert!(on.claim_grounding.data_claims);

        // The review budget rides the same per-agent resolution. Asserted
        // explicitly because this struct has already shipped once wired to the
        // GLOBAL config only, where a per-agent value was ignored in silence —
        // the disabled path returns without logging, so nothing says so.
        assert_eq!(
            base.claim_grounding.review_budget(),
            ClaimGroundingConfig::default().review_budget(),
            "no override → the documented default budget"
        );
        let tuned = AgentWorkspaceConfig {
            claim_grounding: Some(ClaimGroundingConfig {
                enabled: true,
                review_timeout_secs: Some(120),
                review_synthesis_reserve_secs: Some(25),
                ..Default::default()
            }),
            ..Default::default()
        };
        let tuned = ResolvedConfig::resolve(None, Some(&tuned), &global, "anthropic", None);
        assert_eq!(
            tuned.claim_grounding.review_budget(),
            (
                std::time::Duration::from_secs(120),
                std::time::Duration::from_secs(25)
            ),
            "a per-agent budget must reach the runtime, not be silently ignored"
        );

        // Workspace can also explicitly turn enabled OFF even when global is on.
        global.claim_grounding.enabled = true;
        let ws_off = AgentWorkspaceConfig {
            claim_grounding: Some(ClaimGroundingConfig {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let off = ResolvedConfig::resolve(None, Some(&ws_off), &global, "anthropic", None);
        assert!(!off.claim_grounding.enabled);
    }

    #[test]
    fn code_context_defaults_off_and_resolves_per_agent_over_global() {
        // OFF by default: a fresh global config and a no-override resolve both
        // leave injection disabled — rolled out per-install once validated.
        let global = WorkforceConfig::default();
        assert!(!global.code_context.enabled, "global default is OFF");
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(
            !base.code_context.enabled,
            "no override inherits the OFF global"
        );
        // The grounding thresholds default to the calibrated values, not zero.
        assert_eq!(base.code_context.min_grounding_score, 4.0);
        assert_eq!(base.code_context.bare_word_shape_weight, 0.35);
        assert_eq!(base.code_context.secondary_repo_margin, 0.75);

        // A per-agent workspace override can turn it ON and retune a threshold
        // without touching global.
        let ws_on = AgentWorkspaceConfig {
            code_context: Some(CodeContextConfig {
                enabled: true,
                min_grounding_score: 6.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let on = ResolvedConfig::resolve(None, Some(&ws_on), &global, "anthropic", None);
        assert!(on.code_context.enabled, "workspace override turns it on");
        assert_eq!(
            on.code_context.min_grounding_score, 6.0,
            "workspace override retunes the threshold"
        );
        assert!(
            !global.code_context.enabled,
            "global untouched by the override"
        );
        assert_eq!(global.code_context.min_grounding_score, 4.0);

        // And an override can keep it OFF explicitly.
        let ws_off = AgentWorkspaceConfig {
            code_context: Some(CodeContextConfig {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let off = ResolvedConfig::resolve(None, Some(&ws_off), &global, "anthropic", None);
        assert!(!off.code_context.enabled);
    }

    #[test]
    fn programs_come_from_the_agents_own_config() {
        let global = WorkforceConfig::default();
        let none = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(none.programs, None);
        let on: AgentWorkspaceConfig =
            toml::from_str("[tools.programs]\nenabled = true\nmax_calls = 9\n").unwrap();
        let resolved = ResolvedConfig::resolve(None, Some(&on), &global, "anthropic", None);
        let programs = resolved.programs.expect("the agent's own section");
        assert_eq!(
            (programs.enabled, programs.max_calls),
            (Some(true), Some(9))
        );
    }

    #[test]
    fn the_scheduler_policy_comes_from_the_agents_own_config() {
        let global = WorkforceConfig::default();
        let none = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(none.scheduler, None);
        let off: AgentWorkspaceConfig =
            toml::from_str("[scheduler]\npinned_sessions = false\n").unwrap();
        let resolved = ResolvedConfig::resolve(None, Some(&off), &global, "anthropic", None);
        assert!(
            !global
                .scheduler
                .for_agent(resolved.scheduler.as_ref())
                .pinned_sessions
        );
        assert!(resolved.structural_diff(&none).is_empty(), "read each run");
    }

    #[test]
    fn the_compile_gate_comes_from_the_agents_own_config() {
        let global = WorkforceConfig::default();
        let none = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(none.compile_gate, None);
        let on: AgentWorkspaceConfig =
            toml::from_str("[tools.compile_gate]\nenabled = true\ntimeout_secs = 90\n").unwrap();
        let resolved = ResolvedConfig::resolve(None, Some(&on), &global, "anthropic", None);
        let gate = resolved
            .compile_gate
            .clone()
            .expect("the agent's own section");
        assert_eq!((gate.enabled, gate.timeout_secs), (Some(true), Some(90)));
        assert!(resolved.structural_diff(&none).is_empty(), "read each turn");
    }

    #[test]
    fn structural_editing_comes_from_the_agents_own_config() {
        let global = WorkforceConfig::default();
        let none = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(none.refactor, None);
        let on: AgentWorkspaceConfig =
            toml::from_str("[tools.refactor]\nenabled = true\n").unwrap();
        let resolved = ResolvedConfig::resolve(None, Some(&on), &global, "anthropic", None);
        assert_eq!(
            resolved.refactor.as_ref().and_then(|r| r.enabled),
            Some(true)
        );
        assert!(resolved.structural_diff(&none).is_empty(), "read each turn");
    }

    #[test]
    fn video_frames_come_from_the_agents_own_config() {
        let global = WorkforceConfig::default();
        let none = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(none.video_frames, None);
        let mine: AgentWorkspaceConfig =
            toml::from_str("[attachments.video]\nmax_frames = 4\n").unwrap();
        let resolved = ResolvedConfig::resolve(None, Some(&mine), &global, "anthropic", None);
        let frames = global
            .attachments
            .video
            .for_agent(resolved.video_frames.as_ref())
            .expect("on by default");
        assert_eq!(frames.max_frames, 4);
    }

    #[test]
    fn self_learning_resolves_per_agent_over_global() {
        use workforce_common::settings::SelfLearningConfig;

        let mut global = WorkforceConfig::default();
        global.self_learning.enabled = false;

        // No override → inherits global (off).
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(!base.self_learning.enabled);

        // Per-agent workspace override turns it on without touching global.
        let ws = AgentWorkspaceConfig {
            self_learning: Some(SelfLearningConfig {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let on = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert!(on.self_learning.enabled);

        // Workspace can also explicitly turn it OFF even when global is on.
        global.self_learning.enabled = true;
        let ws_off = AgentWorkspaceConfig {
            self_learning: Some(SelfLearningConfig {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let off = ResolvedConfig::resolve(None, Some(&ws_off), &global, "anthropic", None);
        assert!(!off.self_learning.enabled);
    }

    #[test]
    fn claim_grounding_model_resolves_tier_against_tier_key() {
        use workforce_common::settings::ClaimGroundingConfig;

        let mut global = WorkforceConfig::default();

        // Absent ⇒ stays None so grounding falls back to the agent's model.
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(base.claim_grounding.model.is_none());

        // A tier name resolves to a concrete model for the agent's tier_key.
        global.claim_grounding.model = Some("cheap".into());
        let cheap = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(
            cheap.claim_grounding.model.as_deref(),
            Some("claude-haiku-5-5")
        );

        // A workspace override's model resolves too, and a concrete model id
        // passes through untouched.
        let ws = AgentWorkspaceConfig {
            claim_grounding: Some(ClaimGroundingConfig {
                enabled: true,
                model: Some("claude-sonnet-4-6".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let concrete = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert_eq!(
            concrete.claim_grounding.model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    /// The lesson extractor's model reaches the provider and the cost row as
    /// given, so an unresolved tier was recorded as `gemini:powerful`, a key
    /// no price matches.
    #[test]
    fn lesson_extract_model_resolves_tier_against_tier_key() {
        let mut global = WorkforceConfig::default();
        global.memory.lesson_extract_model = Some("gemini:powerful".into());
        let resolved = ResolvedConfig::resolve(None, None, &global, "gemini", None);
        assert_eq!(
            resolved.memory.lesson_extract_model.as_deref(),
            Some("gemini-3.1-pro-preview")
        );
    }

    #[test]
    fn self_learning_model_resolves_tier_against_tier_key() {
        use workforce_common::settings::SelfLearningConfig;

        let mut global = WorkforceConfig::default();

        // Absent ⇒ stays None so curation falls back to the agent's model.
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(base.self_learning.model.is_none());

        // A tier name resolves to a concrete model for the agent's tier_key.
        global.self_learning.model = Some("cheap".into());
        let cheap = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(
            cheap.self_learning.model.as_deref(),
            Some("claude-haiku-5-5")
        );

        // A `provider:tier` spec routes to that provider and keeps the prefix.
        global.self_learning.model = Some("gemini:cheap".into());
        let routed = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert_eq!(
            routed.self_learning.model.as_deref(),
            Some("gemini:gemini-3.5-flash-lite")
        );

        // A concrete model id passes through untouched.
        let ws = AgentWorkspaceConfig {
            self_learning: Some(SelfLearningConfig {
                model: Some("claude-sonnet-4-6".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let concrete = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert_eq!(
            concrete.self_learning.model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn self_learning_change_is_not_structural() {
        // A self_learning edit must hot-reload, so structural_diff ignores it.
        let base = resolved(AgentRecallScope::Shared, "sqlite");
        let mut next = resolved(AgentRecallScope::Shared, "sqlite");
        next.self_learning.enabled = true;
        next.self_learning.recall_top_k = 99;
        assert!(base.structural_diff(&next).is_empty());
    }

    #[test]
    fn claim_grounding_change_is_not_structural() {
        // A claim_grounding edit must hot-reload, so structural_diff ignores it.
        let base = resolved(AgentRecallScope::Shared, "sqlite");
        let mut next = resolved(AgentRecallScope::Shared, "sqlite");
        next.claim_grounding.enabled = true;
        next.claim_grounding.data_claims = true;
        next.claim_grounding.commitments = true;
        assert!(base.structural_diff(&next).is_empty());
    }

    #[test]
    fn research_change_is_not_structural() {
        let base = resolved(AgentRecallScope::Shared, "sqlite");
        let mut next = resolved(AgentRecallScope::Shared, "sqlite");
        next.research = Some(workforce_common::settings::AgentResearchConfig {
            adversarial: Some(true),
            low_confidence_below: Some(70),
        });
        assert!(base.structural_diff(&next).is_empty());
    }

    #[test]
    fn handoff_change_is_not_structural() {
        let base = resolved(AgentRecallScope::Shared, "sqlite");
        let mut next = resolved(AgentRecallScope::Shared, "sqlite");
        next.handoff = Some(workforce_common::settings::AgentHandoffConfig {
            verify: Some(true),
            max_corrections: Some(2),
            model: Some("cheap".into()),
        });
        assert!(base.structural_diff(&next).is_empty());
    }

    #[test]
    fn resolved_config_workspace_session_digest_override_wins() {
        let mut global = WorkforceConfig::default();
        global.sessions.digest_max_sessions = 5;

        // No override → the global `[sessions]` digest knobs.
        let base = ResolvedConfig::resolve(None, None, &global, "anthropic", None);
        assert!(base.session_digest.enabled);
        assert_eq!(base.session_digest.max_sessions, 5);
        assert_eq!(base.session_digest.window_hours, 12);
        assert_eq!(base.session_digest.max_chars, 1200);

        // A workspace `[session_digest]` block replaces them wholesale.
        let ws = AgentWorkspaceConfig {
            session_digest: Some(SessionDigestConfig {
                enabled: false,
                max_sessions: 1,
                window_hours: 2,
                max_chars: 300,
            }),
            ..Default::default()
        };
        let over = ResolvedConfig::resolve(None, Some(&ws), &global, "anthropic", None);
        assert!(!over.session_digest.enabled);
        assert_eq!(over.session_digest.max_sessions, 1);
        assert_eq!(over.session_digest.window_hours, 2);
        assert_eq!(over.session_digest.max_chars, 300);
        // Global untouched by the override.
        assert_eq!(global.sessions.digest_max_sessions, 5);
    }

    #[test]
    fn structural_diff_ignores_digest_and_boost_fields() {
        // Digest knobs and the recall recency boost hot-reload; they must
        // never be reported as restart-required.
        let base = resolved(AgentRecallScope::Shared, "sqlite");
        let mut next = resolved(AgentRecallScope::Shared, "sqlite");
        next.session_digest.enabled = false;
        next.session_digest.max_sessions = 9;
        next.session_digest.window_hours = 99;
        next.session_digest.max_chars = 9999;
        next.memory.recall_recency_boost = 0.9;
        next.memory.recall_recency_half_life_hours = 0.5;
        assert!(base.structural_diff(&next).is_empty());
    }

    #[test]
    fn config_cell_snapshot_is_stable_across_swap() {
        let cell = ConfigCell::new(resolved(AgentRecallScope::Shared, "sqlite"));
        let pinned = cell.snapshot();
        cell.store(resolved(AgentRecallScope::Isolated, "sqlite"));

        // The pinned snapshot is unchanged; a fresh read sees the new value.
        assert_eq!(pinned.memory.agent_recall_scope, AgentRecallScope::Shared);
        assert_eq!(
            cell.snapshot().memory.agent_recall_scope,
            AgentRecallScope::Isolated
        );
    }
}
