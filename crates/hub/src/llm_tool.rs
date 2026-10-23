//! LLM management tools — let authorized agents manage models and tokens at runtime.
//!
//! These tools allow agents to:
//! - View current model, provider, and token pool status
//! - Switch the active model
//! - Add/remove pooled LLM tokens for failover
//!
//! Access is controlled by the policy engine — `system.*` tools
//! can be restricted to specific agents via policy rules.

use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{info, warn};

use workforce_agent::llm::bedrock_catalog::{BedrockCatalogStore, CatalogError, CatalogState};
use workforce_agent::llm::failover::FailoverProvider;
use workforce_agent::llm::pool::{
    CooldownReason, ModelCooldownStatus, PoolEntryStatus, PooledProvider,
};
use workforce_agent::llm::provider::{
    chain_credential_fallback, create_named_provider_async, provider_env_key, LlmProvider,
    ProviderKind, ProviderParseError,
};
use workforce_agent::llm::responses::ChatGptModel;
use workforce_agent::llm::tiers::{split_routing_prefix, ModelTier};
use workforce_agent::llm::types::{ContentBlock, LlmMessage, Role};
use workforce_agent::runner::with_served_log;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::cost::{CostEntry, CostTracker, TokenCounts};
use workforce_common::settings::{is_claude_oat, oat_refusal, LlmConfig};
use workforce_common::subscription::Origin;
use workforce_core::types::AgentId;

use workforce_common::session::SessionStore;
use workforce_common::settings::{AdmitWorkspace, AgentWorkspaceConfig};

use crate::hub_core::subscriptions::CliKind;
use crate::vault::Vault;

// ─── LLM Tool Context ───────────────────────────────────────────────────────

/// Shared context for all LLM management tools.
#[derive(Clone)]
pub struct LlmToolContext {
    pub vault: Arc<Vault>,
    /// Hub-level shared vault — fallback tier for `llm/accounts/*` lookups so
    /// an account moved to the global tier stays resolvable (see
    /// [`crate::vault::find_account_api_key_layered`]). `None` in stand-alone
    /// or test setups.
    pub global_vault: Option<Arc<Vault>>,
    pub pool: Arc<PooledProvider>,
    pub active_model: Arc<RwLock<Option<String>>>,
    pub active_max_tokens: Arc<RwLock<Option<u32>>>,
    pub active_max_rounds: Arc<RwLock<Option<u32>>>,
    pub provider_kind: Arc<RwLock<ProviderKind>>,
    pub tier_key: Arc<RwLock<String>>,
    pub llm_config: LlmConfig,
    /// Live default model resolved against the current `tier_key`.
    /// See [`crate::serve::AgentService::current_default_model`] — kept
    /// in sync by `reload_provider` so display/fallback reads here
    /// don't return the stale concrete model captured in
    /// `llm_config.default_model` at startup.
    pub current_default_model: Arc<RwLock<String>>,
    /// The agent's *configured* default model spec (tier name or concrete id)
    /// — i.e. `[llm].default_model` before tier resolution. Kept live across
    /// config hot-reloads (shared cell with
    /// [`crate::serve::AgentService::original_default_model`]).
    /// `system.set_provider` re-resolves this against the new `tier_key` so
    /// switching providers re-points the default instead of leaving the
    /// previous provider's model pinned.
    pub original_default_model: Arc<RwLock<String>>,
    pub failover_provider: Option<Arc<FailoverProvider>>,
    pub agent_id: AgentId,
    /// Where a probe call made to validate a model is costed.
    pub cost_tracker: Arc<CostTracker>,
    pub sessions: Arc<dyn SessionStore>,
    pub active_routing: Arc<RwLock<Option<workforce_agent::model_router::RoutingConfig>>>,
    /// Hot-reloaded runtime config, for the `[llm.routing]` baseline behind the
    /// `active_routing` override.
    pub resolved: crate::resolved_config::ConfigCell,
    /// Live `[llm.equivalences]` cell — the agent's runtime-config view carries
    /// its hot-reload handles (this and `resolved`); new tools add theirs here.
    pub config_equivalences: workforce_agent::llm::tiers::EquivalencesCell,
    pub workspace: std::path::PathBuf,
    pub bedrock_catalog: Arc<workforce_agent::llm::bedrock_catalog::BedrockCatalogStore>,
    /// Invoked after `system.add_account` / `system.remove_account` writes to
    /// `llm/accounts/*`, so the image provider rebuilds with the new
    /// credential. Without it, the provider keeps the empty/stale api_key it
    /// captured at the last `[llm.image]` hot-reload — see
    /// [`workforce_agent::tools::image_gen::ReloadImageTool`].
    pub reload_image_tool: workforce_agent::tools::image_gen::ReloadImageTool,
    /// Builds the agent's providers of every kind, as boot does.
    pub providers: Arc<crate::agent_llm::AgentProviders>,
    /// Refuses a config write the agent would not load.
    pub admission: crate::workspace_admission::WorkspaceAdmission,
}

impl LlmToolContext {
    /// Apply `mutate` to the workspace config and save it, if the agent would
    /// load the result.
    fn persist(&self, mutate: impl FnOnce(&mut AgentWorkspaceConfig)) -> eyre::Result<()> {
        AgentWorkspaceConfig::update(&self.workspace, &self.admission, mutate)
    }

    /// Whether the agent would load its workspace config after `mutate`,
    /// without writing it.
    fn admits(&self, mutate: impl FnOnce(&mut AgentWorkspaceConfig)) -> eyre::Result<()> {
        self.admission.admit(&AgentWorkspaceConfig::load_mutated(
            &self.workspace,
            mutate,
        )?)
    }

    pub fn provider_kind(&self) -> ProviderKind {
        *self.provider_kind.read()
    }

    pub fn tier_key(&self) -> String {
        self.tier_key.read().clone()
    }

    /// Effective routing: the runtime override if set, else the hot-reloaded
    /// `[llm.routing]` baseline. Matches `AgentService::current_routing_config`.
    pub fn current_routing(&self) -> workforce_agent::model_router::RoutingConfig {
        self.active_routing
            .read()
            .clone()
            .unwrap_or_else(|| self.resolved.snapshot().routing.clone())
    }

    /// Build a [`TierRegistry`] from the live `[llm.equivalences]` cell, so
    /// model resolution in the LLM tools honours config hot-reloads.
    pub fn tier_registry(&self) -> workforce_agent::llm::tiers::TierRegistry {
        workforce_agent::llm::tiers::TierRegistry::from_cell(&self.config_equivalences)
    }
}

/// Static list of Anthropic Claude models available on Vertex AI Model Garden.
///
/// Vertex doesn't expose a live model-listing API for publisher models the way
/// Bedrock does, so we ship a curated list matching the `[vertex.anthropic]`
/// tier table. Out-of-region models are listed; the actual availability check
/// happens at request time (404 → friendly error).
fn vertex_anthropic_default_models() -> Vec<ModelInfo> {
    [
        // Current-gen Vertex Claude ids are unversioned (publisher versionId
        // "default"); only older snapshots keep the @date suffix.
        ("claude-opus-4-8", "Claude Opus 4.8"),
        ("claude-sonnet-5", "Claude Sonnet 5"),
        ("claude-haiku-4-5@20251001", "Claude Haiku 4.5"),
    ]
    .into_iter()
    .map(|(id, name)| ModelInfo {
        id: id.into(),
        display_name: Some(name.into()),
    })
    .collect()
}

async fn bedrock_catalog_snapshot(store: &BedrockCatalogStore) -> Vec<ModelInfo> {
    let region = workforce_agent::llm::bedrock::resolve_region(None);
    match store.snapshot(&region).await {
        CatalogState::Ready(catalog) => {
            let notice = catalog.notice().map(|n| ModelInfo {
                id: "<bedrock-catalog-notice>".into(),
                display_name: Some(n.to_string()),
            });
            let models = catalog.models().iter().map(|m| ModelInfo {
                id: m.id.clone(),
                display_name: m.name.clone(),
            });
            notice.into_iter().chain(models).collect()
        }
        CatalogState::Unavailable { reason, .. } => vec![ModelInfo {
            id: "<bedrock-catalog-unavailable>".into(),
            display_name: Some(reason.to_string()),
        }],
    }
}

fn is_bedrock_provider_arg(arg: Option<&str>) -> bool {
    let trimmed = match arg {
        Some(s) => s.trim(),
        None => return false,
    };
    let base = trimmed.split(':').next().unwrap_or("");
    base.eq_ignore_ascii_case("bedrock")
        || base.eq_ignore_ascii_case("aws")
        || base.eq_ignore_ascii_case("aws-bedrock")
}

fn filter_and_encode_models(models: &[ModelInfo], filter: Option<&str>) -> Vec<serde_json::Value> {
    let needle = filter.unwrap_or("").to_lowercase();
    models
        .iter()
        .filter(|m| needle.is_empty() || m.id.to_lowercase().contains(&needle))
        .map(|m| {
            let mut entry = serde_json::json!({ "id": m.id });
            if let Some(name) = m.display_name.as_ref() {
                entry["display_name"] = serde_json::json!(name);
            }
            entry
        })
        .collect()
}

fn format_bedrock_suggestions(
    suggestions: &[workforce_agent::llm::bedrock_catalog::BedrockModel],
) -> String {
    if suggestions.is_empty() {
        return "<none — check Bedrock console>".to_string();
    }
    suggestions
        .iter()
        .map(|m| format!("{} ({:?})", m.id, m.kind))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Create all LLM management tools.
pub fn llm_tools(ctx: LlmToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SystemLlmStatusTool::new(ctx.clone())),
        Arc::new(SystemSetModelTool::new(ctx.clone())),
        Arc::new(SystemListModelsTool::new(ctx.clone())),
        Arc::new(SystemSetMaxTokensTool::new(ctx.clone())),
        Arc::new(SystemSetMaxRoundsTool::new(ctx.clone())),
        Arc::new(SystemAddLlmTokenTool::new(ctx.clone())),
        Arc::new(SystemRemoveLlmTokenTool::new(ctx.clone())),
        Arc::new(SystemListLlmTokensTool::new(ctx.clone())),
        Arc::new(SystemSetFailoverChainTool::new(ctx.clone())),
        Arc::new(SystemAddAccountTool::new(ctx.clone())),
        Arc::new(SystemRemoveAccountTool::new(ctx.clone())),
        Arc::new(SystemListAccountsTool::new(ctx.clone())),
        Arc::new(SystemSetSessionModelTool::new(ctx.clone())),
        Arc::new(SystemRoutingStatusTool::new(ctx.clone())),
        Arc::new(SystemSetRoutingTool::new(ctx.clone())),
        Arc::new(SystemSetProviderTool::new(ctx)),
    ]
}

// ─── Pool entry formatting helpers ──────────────────────────────────────────

/// Threshold (in seconds) above which a rate limit is classified as "long-term".
/// 1 hour — anything longer than this likely represents an account-level ban
/// rather than a transient rate limit.
const LONG_TERM_THRESHOLD_SECS: u64 = 3600;

/// Format a duration in seconds as a human-readable string.
///
/// Examples: `"5s"`, `"2m 30s"`, `"1h 30m"`, `"3d 12h"`.
fn format_duration_human(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if days > 0 {
        if hours > 0 {
            format!("{}d {}h", days, hours)
        } else {
            format!("{}d", days)
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}h", hours)
        }
    } else if minutes > 0 {
        if seconds > 0 {
            format!("{}m {}s", minutes, seconds)
        } else {
            format!("{}m", minutes)
        }
    } else {
        format!("{}s", seconds)
    }
}

/// Serialize a `PoolEntryStatus` to JSON with human-readable fields.
fn format_pool_entry(e: &PoolEntryStatus) -> serde_json::Value {
    let (status, reason_str) = match (e.is_rate_limited, e.cooldown_reason) {
        (false, _) => ("available", None),
        (true, Some(CooldownReason::AuthFailed)) => ("auth-failed", Some("auth-failed")),
        (true, _) => ("rate-limited", Some("rate-limited")),
    };

    let is_long_term = e
        .original_retry_after_secs
        .map(|s| s >= LONG_TERM_THRESHOLD_SECS)
        .unwrap_or(false)
        && e.is_rate_limited;

    let mut entry = serde_json::json!({
        "name": e.name,
        "status": status,
        "is_rate_limited": e.is_rate_limited,
    });

    if e.is_rate_limited {
        if let Some(reason) = reason_str {
            entry["reason"] = serde_json::json!(reason);
        }
        if let Some(err) = &e.cooldown_error {
            entry["last_error"] = serde_json::json!(err);
        }

        if let Some(remaining) = e.rate_limited_secs_remaining {
            entry["effective_cooldown_secs"] = serde_json::json!(remaining);
            entry["effective_cooldown_display"] =
                serde_json::json!(format_duration_human(remaining));
        }
        if let Some(original) = e.original_retry_after_secs {
            entry["original_retry_after_secs"] = serde_json::json!(original);
            entry["original_retry_after_display"] =
                serde_json::json!(format_duration_human(original));
        }
        entry["is_long_term"] = serde_json::json!(is_long_term);

        // Provide clear guidance for agents
        entry["note"] = match e.cooldown_reason {
            Some(CooldownReason::AuthFailed) => serde_json::json!(
                "Token is invalid or expired (401). Will NOT recover on its own. \
                 The effective_cooldown is just the internal retry timer — \
                 after it expires the pool will re-test this token, but it will \
                 likely fail again. Use system.remove_llm_token to remove it \
                 or system.add_llm_token with a fresh token to replace it."
            ),
            Some(CooldownReason::RateLimited) => {
                if is_long_term {
                    serde_json::json!(
                        "Token is rate-limited by Anthropic (429). \
                         The effective_cooldown is an internal retry timer \
                         (max 5 min) — it does NOT mean the API limit resets then. \
                         The real API rate limit resets in original_retry_after_display. \
                         This is a long-term limit (7-day rolling window). \
                         Do NOT wait for effective_cooldown — use other tokens or \
                         failover models."
                    )
                } else {
                    serde_json::json!(
                        "Token is rate-limited by Anthropic (429). \
                         The effective_cooldown is an internal retry timer. \
                         The real API limit resets in original_retry_after_display."
                    )
                }
            }
            None => serde_json::json!("Token is on cooldown."),
        };
    }

    if !e.model_cooldowns.is_empty() {
        entry["model_cooldowns"] = e
            .model_cooldowns
            .iter()
            .map(format_model_cooldown)
            .collect();
        // Suppressed while the entry itself is on cooldown — "the
        // credential works" would contradict the entry-level note.
        if !e.is_rate_limited {
            entry["model_cooldowns_note"] = serde_json::json!(
                "These models are denied for this credential (e.g. no marketplace \
                 or model-access grant). The credential itself works — other \
                 models keep flowing. Fix access for the model or avoid routing \
                 to it."
            );
        }
    }

    entry
}

fn format_model_cooldown(m: &ModelCooldownStatus) -> serde_json::Value {
    serde_json::json!({
        "model": m.model,
        "reason": match m.reason {
            CooldownReason::AuthFailed => "auth-failed",
            CooldownReason::RateLimited => "rate-limited",
        },
        "secs_remaining": m.secs_remaining,
        "remaining_display": format_duration_human(m.secs_remaining),
        "last_error": m.error,
    })
}

// ─── system.llm_status ──────────────────────────────────────────────────────

pub struct SystemLlmStatusTool {
    ctx: LlmToolContext,
}

impl SystemLlmStatusTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemLlmStatusTool {
    fn name(&self) -> &str {
        "system.llm_status"
    }

    fn description(&self) -> &str {
        "Show the agent's LLM setup: active model and where it comes from, the \
         provider chain and its health, the token pool, and every provider the \
         agent's settings route to with the credential a call of the current \
         person's would use now: an API key and where it is held, or which of \
         their plan logins (Claude, ChatGPT) serves it and, if none, why. Read \
         `providers` / `credentials_summary` to answer whether someone's \
         Claude Code token or ChatGPT login is set up on this agent. This \
         report is the check: never test a token or login by starting a \
         coding session or any other call."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let active_model = self
            .ctx
            .active_model
            .read()
            .clone()
            .unwrap_or_else(|| self.ctx.current_default_model.read().clone());

        let is_override = self.ctx.active_model.read().is_some();

        let pool_status = self.ctx.pool.list().await;
        let pool_entries: Vec<serde_json::Value> =
            pool_status.iter().map(format_pool_entry).collect();
        let available_count = pool_status.iter().filter(|e| !e.is_rate_limited).count();
        let rate_limited_count = pool_status
            .iter()
            .filter(|e| e.is_rate_limited && e.cooldown_reason != Some(CooldownReason::AuthFailed))
            .count();
        let auth_failed_count = pool_status
            .iter()
            .filter(|e| e.cooldown_reason == Some(CooldownReason::AuthFailed))
            .count();
        let long_term_count = pool_status
            .iter()
            .filter(|e| {
                e.is_rate_limited
                    && e.original_retry_after_secs
                        .map(|s| s >= LONG_TERM_THRESHOLD_SECS)
                        .unwrap_or(false)
            })
            .count();
        let model_cooldown_count: usize = pool_status.iter().map(|e| e.model_cooldowns.len()).sum();

        let provider_name = format!("{:?}", self.ctx.provider_kind());

        // Build chain details with provider routing info
        let (chain, chain_details, external_providers, health_status) =
            if let Some(ref fp) = self.ctx.failover_provider {
                let chain = fp.get_chain().await;
                let details: Vec<serde_json::Value> = chain
                    .iter()
                    .map(|entry| {
                        if let (Some(prefix), model) =
                            workforce_agent::llm::tiers::split_routing_prefix(entry)
                        {
                            serde_json::json!({
                                "entry": entry,
                                "model": model,
                                "provider": prefix,
                            })
                        } else {
                            serde_json::json!({
                                "entry": entry,
                                "model": entry,
                                "provider": &provider_name,
                            })
                        }
                    })
                    .collect();
                let externals = fp.list_external_providers().await;
                let health = fp.health_tracker().status().await;
                let health_entries: Vec<serde_json::Value> = health
                    .iter()
                    .map(|(name, status)| {
                        serde_json::json!({
                            "model": name,
                            "recent_failures": status.recent_failures,
                            "is_degraded": status.is_degraded,
                            "degraded_at_secs_ago": status.degraded_at_secs_ago,
                        })
                    })
                    .collect();
                (chain, details, externals, health_entries)
            } else {
                (
                    self.ctx.llm_config.failover.clone(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
            };

        let max_tokens_override = *self.ctx.active_max_tokens.read();
        let max_rounds_override = *self.ctx.active_max_rounds.read();
        let routing_overridden = self.ctx.active_routing.read().is_some();
        let effective_routing = self.ctx.current_routing();

        let last_used_model = self
            .ctx
            .failover_provider
            .as_ref()
            .and_then(|fp| fp.last_used_model());

        // Active playbook step model and session-level override. The playbook
        // step model wins — this diagnostic mirrors the precedence in
        // `serve::session_model_source` (the source of truth) so the reported
        // effective model matches what a turn would actually run; it reads the
        // two values separately for the JSON fields rather than reusing it.
        let (playbook_model, session_model) = workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|sid| {
                self.ctx
                    .sessions
                    .get(sid)
                    .ok()
                    .flatten()
                    .map(|s| {
                        (
                            s.playbook().and_then(|p| p.step_model),
                            s.model_override.clone(),
                        )
                    })
                    .unwrap_or((None, None))
            })
            .ok()
            .unwrap_or((None, None));

        let providers = crate::provider_status::status(
            &self.ctx.providers,
            self.ctx.provider_kind(),
            &self.ctx.llm_config,
            &self.ctx.resolved.snapshot(),
            &chain,
        );
        let credentials_summary: Vec<&str> = providers.iter().map(|p| p.summary.as_str()).collect();

        let effective_model = playbook_model
            .as_deref()
            .or(session_model.as_deref())
            .unwrap_or(&active_model);
        let model_source = if playbook_model.is_some() {
            "playbook step"
        } else if session_model.is_some() {
            "session"
        } else if is_override {
            "agent"
        } else {
            "config"
        };

        Ok(ToolResult::success(
            serde_json::json!({
                "active_model": active_model,
                "playbook_step_model": playbook_model,
                "session_model_override": session_model,
                "effective_model": effective_model,
                "model_source": model_source,
                "last_used_model": last_used_model,
                "is_model_override": is_override,
                "default_model": self.ctx.current_default_model.read().clone(),
                "max_tokens": max_tokens_override.unwrap_or(self.ctx.llm_config.max_tokens),
                "max_tokens_default": self.ctx.llm_config.max_tokens,
                "is_max_tokens_override": max_tokens_override.is_some(),
                "max_rounds": max_rounds_override.unwrap_or(self.ctx.llm_config.max_rounds),
                "max_rounds_default": self.ctx.llm_config.max_rounds,
                "is_max_rounds_override": max_rounds_override.is_some(),
                "provider": provider_name,
                "providers": providers,
                "credentials_summary": credentials_summary,
                "failover_chain": chain,
                "failover_chain_details": chain_details,
                "aliases": self.ctx.llm_config.aliases,
                "external_providers": external_providers,
                "model_health": health_status,
                "token_pool": {
                    "size": pool_entries.len(),
                    "available": available_count,
                    "rate_limited": rate_limited_count,
                    "auth_failed": auth_failed_count,
                    "long_term_limited": long_term_count,
                    "model_cooldowns": model_cooldown_count,
                    "entries": pool_entries,
                },
                "routing": {
                    "enabled": effective_routing.enable_adaptive_routing,
                    "config": serde_json::to_value(&effective_routing).unwrap_or_default(),
                    "source": if routing_overridden { "override" } else { "config" },
                },
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── system.set_model ────────────────────────────────────────────────────────

pub struct SystemSetModelTool {
    ctx: LlmToolContext,
}

impl SystemSetModelTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }

    /// Whether `model` can serve the agent's turns: a model a CLI's tier
    /// table names for a CLI route, which a probe would run a whole CLI
    /// completion to learn; else a one-token call, costed as any call is.
    async fn validate(&self, model: &str) -> Result<(), String> {
        let (route, name) = split_routing_prefix(model);
        let kind = route
            .and_then(ProviderKind::from_str_loose)
            .unwrap_or_else(|| self.ctx.provider_kind());
        if CliKind::of(kind).is_some() {
            let tiers = self.ctx.tier_registry();
            let known = ModelTier::ALL.into_iter().any(|tier| {
                tiers
                    .resolve_for_tier(tier, kind.canonical_name())
                    .as_deref()
                    == Some(name)
            });
            return match known {
                true => Ok(()),
                false => Err(format!(
                    "{} runs only its tier models",
                    kind.canonical_name()
                )),
            };
        }
        let probe = [LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        let (answered, served) = with_served_log(self.ctx.pool.create_message(
            None,
            &probe,
            &[],
            Some(model),
            Some(1),
            true,
        ))
        .await;
        let response = answered.map_err(|e| format!("test call failed: {e}"))?;
        let identity = crate::user_config::current_billing_identity_id();
        self.ctx.cost_tracker.record(CostEntry {
            task_type: Some("model_probe"),
            identity_id: identity.as_deref(),
            description: Some("system.set_model"),
            served,
            ..CostEntry::new(
                self.ctx.agent_id.as_str(),
                &response.model,
                TokenCounts::from(&response.usage),
                Origin::Interactive,
            )
        });
        Ok(())
    }
}

#[derive(Deserialize)]
struct SetModelInput {
    model: String,
}

#[async_trait]
impl Tool for SystemSetModelTool {
    fn name(&self) -> &str {
        "system.set_model"
    }

    fn description(&self) -> &str {
        "Change the active LLM model at runtime"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Full model identifier or empty string to reset to default"
                }
            },
            "required": ["model"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetModelInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if params.model.is_empty() {
            if let Err(e) = self.ctx.persist(|cfg| {
                if let Some(ref mut llm) = cfg.llm {
                    llm.default_model = None;
                }
            }) {
                return Ok(ToolResult::failure(
                    format!("The default model was not reset: {e}. No changes were made."),
                    duration(),
                ));
            }
            *self.ctx.active_model.write() = None;
            // Legacy vault cleanup
            let _ = self.ctx.vault.delete("llm/active_model");
            info!("Active model reset to default");
            Ok(ToolResult::success(
                serde_json::json!({
                    "model": self.ctx.current_default_model.read().clone(),
                    "is_override": false,
                    "message": "Reset to default model",
                }),
                duration(),
            ))
        } else {
            let set_model = |cfg: &mut AgentWorkspaceConfig| {
                cfg.llm.get_or_insert_with(Default::default).default_model =
                    Some(params.model.clone());
            };
            let not_set = |e: eyre::Report| {
                ToolResult::failure(
                    format!(
                        "Model '{}' was not set: {e}. No changes were made.",
                        params.model
                    ),
                    duration(),
                )
            };
            if let Err(e) = self.ctx.admits(set_model) {
                return Ok(not_set(e));
            }
            let current_tier_key = self.ctx.tier_key();
            let tiers = self.ctx.tier_registry();
            let resolved_model = tiers.resolve_model(&params.model, &current_tier_key);

            if let Err(reason) = self.validate(&resolved_model).await {
                warn!(model = %resolved_model, %reason, "Model validation failed");
                return Ok(ToolResult::failure(
                    format!(
                        "Model '{resolved_model}' is not available — {reason}. \
                         No changes were made. Use system.list_models to see valid model IDs."
                    ),
                    duration(),
                ));
            }

            if let Err(e) = self.ctx.persist(set_model) {
                return Ok(not_set(e));
            }
            *self.ctx.active_model.write() = Some(params.model.clone());
            info!(
                input = %params.model,
                resolved = %resolved_model,
                "Active model changed via system.set_model"
            );
            Ok(ToolResult::success(
                serde_json::json!({
                    "model": params.model,
                    "resolved_model": resolved_model,
                    "is_override": true,
                    "message": "Model validated and updated — takes effect on next LLM call",
                }),
                duration(),
            ))
        }
    }
}

// ─── system.set_session_model ────────────────────────────────────────────────

pub struct SystemSetSessionModelTool {
    ctx: LlmToolContext,
}

impl SystemSetSessionModelTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }

    fn resolve_session_id(&self) -> Option<String> {
        workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
    }
}

#[derive(Deserialize)]
struct SetSessionModelInput {
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Tool for SystemSetSessionModelTool {
    fn name(&self) -> &str {
        "system.set_session_model"
    }

    fn description(&self) -> &str {
        "Set model override for current session only"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Model ID for this session, or empty/omit to clear"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetSessionModelInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let session_id = match self.resolve_session_id() {
            Some(id) => id,
            None => {
                return Ok(ToolResult::failure(
                    "No current session — cannot set session model".to_string(),
                    duration(),
                ));
            }
        };

        let mut session = match self.ctx.sessions.get(&session_id)? {
            Some(s) => s,
            None => {
                return Ok(ToolResult::failure(
                    format!("Session not found: {}", session_id),
                    duration(),
                ));
            }
        };

        let model = params.model.filter(|m| !m.is_empty()).map(|raw| {
            let current_tier_key = self.ctx.tier_key();
            let tiers = self.ctx.tier_registry();
            tiers.resolve_model(&raw, &current_tier_key)
        });

        if let Some(ref model_id) = model {
            // Validate by making a minimal test call
            let test_msg = LlmMessage {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            };
            if let Err(e) = self
                .ctx
                .pool
                .create_message(None, &[test_msg], &[], Some(model_id), Some(1), true)
                .await
            {
                warn!(model = %model_id, error = %e, "Session model validation failed");
                return Ok(ToolResult::failure(
                    format!(
                        "Model '{}' is not available — test call failed: {}. \
                         No changes were made.",
                        model_id, e
                    ),
                    duration(),
                ));
            }
        }

        session.set_model_override(model.clone());
        self.ctx.sessions.save(&session)?;

        let msg = if model.is_some() {
            "Session model set — takes effect on next LLM call for this session"
        } else {
            "Session model cleared — falls back to agent-wide model or default"
        };

        info!(
            session_id = %session_id,
            model = ?model,
            "Session model override updated via system.set_session_model"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "session_id": session_id,
                "session_model": model,
                "is_session_override": model.is_some(),
                "message": msg,
            }),
            duration(),
        ))
    }
}

// ─── system.list_models ─────────────────────────────────────────────────────

pub struct SystemListModelsTool {
    ctx: LlmToolContext,
}

impl SystemListModelsTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }

    /// Resolve the API key for direct HTTP calls to the models endpoint —
    /// same order as the boot-time chain in `serve`.
    fn resolve_api_key(&self) -> Option<String> {
        let kind = self.ctx.provider_kind();
        self.ctx
            .llm_config
            .api_key
            .clone()
            .or_else(|| provider_env_key(kind))
            .or_else(|| {
                crate::vault::find_account_api_key_layered(
                    Some(&self.ctx.vault),
                    self.ctx.global_vault.as_deref(),
                    crate::agent_llm::vault_provider_name(kind),
                )
            })
            .or_else(|| chain_credential_fallback(kind))
    }

    /// The models `kind`'s API lists for `api_key`; a subscription
    /// provider's need no key.
    async fn fetch_models(
        &self,
        kind: ProviderKind,
        api_key: &str,
    ) -> eyre::Result<Vec<ModelInfo>> {
        match kind {
            ProviderKind::Anthropic => self.fetch_anthropic_models(api_key).await,
            ProviderKind::OpenRouter => self.fetch_openrouter_models(api_key).await,
            ProviderKind::Grok => self.fetch_grok_models(api_key).await,
            ProviderKind::Gemini => self.fetch_gemini_models(api_key).await,
            ProviderKind::Ollama => self.fetch_ollama_models(api_key).await,
            ProviderKind::Vllm => self.fetch_vllm_models(api_key).await,
            ProviderKind::Cerebras => Ok(vec![
                ModelInfo {
                    id: "qwen-3-235b-a22b-instruct-2507".into(),
                    display_name: Some("Qwen 3 235B".into()),
                },
                ModelInfo {
                    id: "gpt-oss-120b".into(),
                    display_name: Some("GPT-OSS 120B".into()),
                },
                ModelInfo {
                    id: "llama3.1-8b".into(),
                    display_name: Some("Llama 3.1 8B".into()),
                },
                ModelInfo {
                    id: "zai-glm-4.7".into(),
                    display_name: Some("Z.ai GLM 4.7".into()),
                },
            ]),
            ProviderKind::Zai => Ok(vec![ModelInfo {
                id: "glm-5.1".into(),
                display_name: Some("GLM 5.1 (754B MoE)".into()),
            }]),
            ProviderKind::Bedrock => Ok(bedrock_catalog_snapshot(&self.ctx.bedrock_catalog).await),
            ProviderKind::VertexAnthropic => Ok(vertex_anthropic_default_models()),
            kind @ (ProviderKind::ChatGpt | ProviderKind::ClaudeCode | ProviderKind::Codex) => {
                Ok(self.subscription_models(kind))
            }
        }
    }

    /// The models a subscription provider runs: what the ChatGPT backend
    /// serves, and for the CLIs the models the agent's tiers name.
    fn subscription_models(&self, kind: ProviderKind) -> Vec<ModelInfo> {
        let ids = match kind {
            ProviderKind::ChatGpt => ChatGptModel::ALL
                .iter()
                .map(|model| model.id().to_string())
                .collect(),
            kind => crate::hub_core::subscriptions::tier_models(kind, &self.ctx.tier_registry()),
        };
        ids.into_iter()
            .map(|id| ModelInfo {
                id,
                display_name: None,
            })
            .collect()
    }

    /// Fetch models from the Anthropic API.
    async fn fetch_anthropic_models(&self, api_key: &str) -> eyre::Result<Vec<ModelInfo>> {
        let resp = reqwest::Client::new()
            .get("https://api.anthropic.com/v1/models?limit=100")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Anthropic models API returned {}: {}",
                status,
                workforce_common::text::truncate_str(&body, 200)
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id")?.as_str()?.to_string();
                        let display_name = m
                            .get("display_name")
                            .and_then(|n| n.as_str())
                            .map(String::from);
                        Some(ModelInfo { id, display_name })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    /// Fetch models from the OpenRouter API.
    async fn fetch_openrouter_models(&self, api_key: &str) -> eyre::Result<Vec<ModelInfo>> {
        let client = reqwest::Client::new();
        let resp = client
            .get("https://openrouter.ai/api/v1/models")
            .bearer_auth(api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "OpenRouter models API returned {}: {}",
                status,
                workforce_common::text::truncate_str(&body, 200)
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id")?.as_str()?.to_string();
                        let display_name = m.get("name").and_then(|n| n.as_str()).map(String::from);
                        Some(ModelInfo { id, display_name })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    /// Fetch models from the xAI Grok API.
    async fn fetch_grok_models(&self, api_key: &str) -> eyre::Result<Vec<ModelInfo>> {
        let client = reqwest::Client::new();
        let resp = client
            .get("https://api.x.ai/v1/models")
            .bearer_auth(api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Grok models API returned {}: {}",
                status,
                workforce_common::text::truncate_str(&body, 200)
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id")?.as_str()?.to_string();
                        let display_name =
                            m.get("owned_by").and_then(|n| n.as_str()).map(String::from);
                        Some(ModelInfo { id, display_name })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    /// Fetch available models from Google Gemini.
    ///
    /// Note: The native models list endpoint uses query-param auth (`?key=`),
    /// while the OpenAI-compatible chat completions endpoint uses Bearer auth.
    /// Both accept standard Google AI Studio API keys.
    async fn fetch_gemini_models(&self, api_key: &str) -> eyre::Result<Vec<ModelInfo>> {
        let client = reqwest::Client::new();
        let resp = client
            .get("https://generativelanguage.googleapis.com/v1beta/models")
            .query(&[("key", api_key)])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Gemini models API returned {}: {}",
                status,
                workforce_common::text::truncate_str(&body, 200)
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let models = body
            .get("models")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let name = m.get("name")?.as_str()?;
                        // API returns "models/gemini-2.5-pro" — strip prefix
                        let id = name.strip_prefix("models/").unwrap_or(name).to_string();
                        let display_name = m
                            .get("displayName")
                            .and_then(|n| n.as_str())
                            .map(String::from);
                        Some(ModelInfo { id, display_name })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    /// Fetch available models from Ollama.
    async fn fetch_ollama_models(&self, base_url: &str) -> eyre::Result<Vec<ModelInfo>> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
        let resp = client.get(&url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Ollama models API returned {}: {}",
                status,
                workforce_common::text::truncate_str(&body, 200)
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let models = body
            .get("models")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("name")?.as_str()?.to_string();
                        let size = m.get("size").and_then(|s| s.as_u64());
                        let display_name =
                            size.map(|s| format!("{:.1}GB", s as f64 / 1_073_741_824.0));
                        Some(ModelInfo { id, display_name })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    /// Fetch models from a vLLM server via the OpenAI-compatible models endpoint.
    async fn fetch_vllm_models(&self, base_url: &str) -> eyre::Result<Vec<ModelInfo>> {
        let client = reqwest::Client::new();
        let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
        let resp = client.get(&url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "vLLM models API returned {}: {}",
                status,
                workforce_common::text::truncate_str(&body, 200)
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let models = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id")?.as_str()?.to_string();
                        Some(ModelInfo {
                            id,
                            display_name: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }
}

struct ModelInfo {
    id: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct ListModelsInput {
    #[serde(default)]
    filter: Option<String>,
    /// Inspect a specific provider's catalog without switching the active
    /// provider. Currently only `bedrock` is supported — the Bedrock catalog
    /// is process-wide and queryable without an API key handshake, so it can
    /// be inspected while the agent stays on its current provider.
    #[serde(default)]
    provider: Option<String>,
}

#[async_trait]
impl Tool for SystemListModelsTool {
    fn name(&self) -> &str {
        "system.list_models"
    }

    fn description(&self) -> &str {
        "List available model IDs from the configured LLM provider"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Optional substring to filter model IDs"
                },
                "provider": {
                    "type": "string",
                    "description": "Optional provider to inspect without switching the active provider. Currently only `bedrock` is supported."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ListModelsInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if is_bedrock_provider_arg(params.provider.as_deref()) {
            let models = bedrock_catalog_snapshot(&self.ctx.bedrock_catalog).await;
            let entries = filter_and_encode_models(&models, params.filter.as_deref());
            return Ok(ToolResult::success(
                serde_json::json!({
                    "provider": "Bedrock",
                    "models": entries,
                    "count": entries.len(),
                }),
                duration(),
            ));
        }

        let kind = self.ctx.provider_kind();
        let provider_name = format!("{kind:?}");
        let models = match self.resolve_api_key() {
            Some(api_key) => self.fetch_models(kind, &api_key).await,
            None if kind.is_subscription() => Ok(self.subscription_models(kind)),
            None => {
                return Ok(ToolResult::failure(
                    "No API key available to query models endpoint".to_string(),
                    duration(),
                ));
            }
        };

        let models = match models {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "Failed to fetch models list");
                return Ok(ToolResult::failure(
                    format!("Failed to fetch models from {}: {}", provider_name, e),
                    duration(),
                ));
            }
        };

        let model_entries = filter_and_encode_models(&models, params.filter.as_deref());

        let active_model = self
            .ctx
            .active_model
            .read()
            .clone()
            .unwrap_or_else(|| self.ctx.current_default_model.read().clone());

        Ok(ToolResult::success(
            serde_json::json!({
                "provider": provider_name,
                "models": model_entries,
                "count": model_entries.len(),
                "current_model": active_model,
                "aliases": self.ctx.llm_config.aliases,
            }),
            duration(),
        ))
    }
}

// ─── system.set_max_tokens ───────────────────────────────────────────────────

pub struct SystemSetMaxTokensTool {
    ctx: LlmToolContext,
}

impl SystemSetMaxTokensTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct SetMaxTokensInput {
    value: u32,
    /// Optional: set max_tokens for a specific model only.
    model: Option<String>,
}

#[async_trait]
impl Tool for SystemSetMaxTokensTool {
    fn name(&self) -> &str {
        "system.set_max_tokens"
    }

    fn description(&self) -> &str {
        "Set max tokens per LLM response at runtime"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "integer",
                    "description": "Max tokens per response (1–128000), or 0 to reset to default"
                },
                "model": {
                    "type": "string",
                    "description": "Model name for per-model override"
                }
            },
            "required": ["value"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetMaxTokensInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Per-model override path
        if let Some(ref model) = params.model {
            let previous = self.ctx.llm_config.model_max_tokens.get(model).copied();

            if params.value == 0 {
                // Remove per-model override
                if let Err(e) = self.ctx.persist(|cfg| {
                    let llm = cfg.llm.get_or_insert_with(Default::default);
                    if let Some(ref mut map) = llm.model_max_tokens {
                        map.remove(model);
                        if map.is_empty() {
                            llm.model_max_tokens = None;
                        }
                    }
                }) {
                    warn!(error = %e, "Failed to update workspace config");
                }
                info!(model = %model, "Per-model max_tokens reset");
                return Ok(ToolResult::success(
                    serde_json::json!({
                        "model": model,
                        "max_tokens": null,
                        "previous": previous,
                        "message": format!("Per-model max_tokens for '{}' removed — will use global default", model),
                    }),
                    duration(),
                ));
            } else if params.value > 128_000 {
                return Ok(ToolResult::failure(
                    format!(
                        "max_tokens {} exceeds maximum allowed (128000)",
                        params.value
                    ),
                    duration(),
                ));
            } else {
                // Set per-model override
                if let Err(e) = self.ctx.persist(|cfg| {
                    let llm = cfg.llm.get_or_insert_with(Default::default);
                    let map = llm.model_max_tokens.get_or_insert_with(Default::default);
                    map.insert(model.clone(), params.value);
                }) {
                    warn!(error = %e, "Failed to update workspace config");
                }
                info!(
                    model = %model,
                    max_tokens = params.value,
                    "Per-model max_tokens updated via system.set_max_tokens"
                );
                return Ok(ToolResult::success(
                    serde_json::json!({
                        "model": model,
                        "max_tokens": params.value,
                        "previous": previous,
                        "message": format!("Per-model max_tokens for '{}' set to {} — takes effect on next session", model, params.value),
                    }),
                    duration(),
                ));
            }
        }

        // Global override path (existing behavior)
        let previous = self
            .ctx
            .active_max_tokens
            .read()
            .unwrap_or(self.ctx.llm_config.max_tokens);

        if params.value == 0 {
            *self.ctx.active_max_tokens.write() = None;
            // Remove from workspace config.toml
            if let Err(e) = self.ctx.persist(|cfg| {
                if let Some(ref mut llm) = cfg.llm {
                    llm.max_tokens = None;
                }
            }) {
                warn!(error = %e, "Failed to update workspace config");
            }
            // Legacy vault cleanup
            let _ = self.ctx.vault.delete("llm/max_tokens");
            info!("max_tokens reset to default");
            Ok(ToolResult::success(
                serde_json::json!({
                    "max_tokens": self.ctx.llm_config.max_tokens,
                    "previous": previous,
                    "is_override": false,
                    "message": "Reset to default max_tokens",
                }),
                duration(),
            ))
        } else if params.value > 128_000 {
            Ok(ToolResult::failure(
                format!(
                    "max_tokens {} exceeds maximum allowed (128000)",
                    params.value
                ),
                duration(),
            ))
        } else {
            *self.ctx.active_max_tokens.write() = Some(params.value);
            // Persist to workspace config.toml
            if let Err(e) = self.ctx.persist(|cfg| {
                let llm = cfg.llm.get_or_insert_with(Default::default);
                llm.max_tokens = Some(params.value);
            }) {
                warn!(error = %e, "Failed to update workspace config");
            }
            info!(
                max_tokens = params.value,
                "max_tokens updated via system.set_max_tokens"
            );
            Ok(ToolResult::success(
                serde_json::json!({
                    "max_tokens": params.value,
                    "previous": previous,
                    "is_override": true,
                    "default": self.ctx.llm_config.max_tokens,
                    "message": "max_tokens updated — takes effect on next LLM call",
                }),
                duration(),
            ))
        }
    }
}

// ─── system.set_max_rounds ──────────────────────────────────────────────────

pub struct SystemSetMaxRoundsTool {
    ctx: LlmToolContext,
}

impl SystemSetMaxRoundsTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct SetMaxRoundsInput {
    value: u32,
}

#[async_trait]
impl Tool for SystemSetMaxRoundsTool {
    fn name(&self) -> &str {
        "system.set_max_rounds"
    }

    fn description(&self) -> &str {
        "Set max tool execution rounds at runtime"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "integer",
                    "description": "Max execution rounds (1–500), or 0 to reset to default"
                }
            },
            "required": ["value"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetMaxRoundsInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let previous = self
            .ctx
            .active_max_rounds
            .read()
            .unwrap_or(self.ctx.llm_config.max_rounds);

        if params.value == 0 {
            *self.ctx.active_max_rounds.write() = None;
            // Remove from workspace config.toml
            if let Err(e) = self.ctx.persist(|cfg| {
                if let Some(ref mut llm) = cfg.llm {
                    llm.max_rounds = None;
                }
            }) {
                warn!(error = %e, "Failed to update workspace config");
            }
            // Legacy vault cleanup
            let _ = self.ctx.vault.delete("llm/max_rounds");
            info!("max_rounds reset to default");
            Ok(ToolResult::success(
                serde_json::json!({
                    "max_rounds": self.ctx.llm_config.max_rounds,
                    "previous": previous,
                    "is_override": false,
                    "message": "Reset to default max_rounds",
                }),
                duration(),
            ))
        } else if params.value > 500 {
            Ok(ToolResult::failure(
                format!("max_rounds {} exceeds maximum allowed (500)", params.value),
                duration(),
            ))
        } else {
            *self.ctx.active_max_rounds.write() = Some(params.value);
            // Persist to workspace config.toml
            if let Err(e) = self.ctx.persist(|cfg| {
                let llm = cfg.llm.get_or_insert_with(Default::default);
                llm.max_rounds = Some(params.value);
            }) {
                warn!(error = %e, "Failed to update workspace config");
            }
            info!(
                max_rounds = params.value,
                "max_rounds updated via system.set_max_rounds"
            );
            Ok(ToolResult::success(
                serde_json::json!({
                    "max_rounds": params.value,
                    "previous": previous,
                    "is_override": true,
                    "default": self.ctx.llm_config.max_rounds,
                    "message": "max_rounds updated — takes effect on next message",
                }),
                duration(),
            ))
        }
    }
}

// ─── system.add_llm_token ────────────────────────────────────────────────────

pub struct SystemAddLlmTokenTool {
    ctx: LlmToolContext,
}

impl SystemAddLlmTokenTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AddTokenInput {
    name: String,
    token: String,
}

#[async_trait]
impl Tool for SystemAddLlmTokenTool {
    fn name(&self) -> &str {
        "system.add_llm_token"
    }

    fn description(&self) -> &str {
        "Add an API key for the agent's provider to the token pool for failover"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Friendly name for this token (e.g., 'backup', 'account2')"
                },
                "token": {
                    "type": "string",
                    "description": "The API key"
                }
            },
            "required": ["name", "token"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AddTokenInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if params.name.is_empty() {
            return Ok(ToolResult::failure(
                "Token name cannot be empty".to_string(),
                duration(),
            ));
        }

        if params.token.is_empty() {
            return Ok(ToolResult::failure(
                "Token value cannot be empty".to_string(),
                duration(),
            ));
        }

        if self.ctx.provider_kind().is_subscription() {
            return Ok(ToolResult::failure(
                format!(
                    "The {} provider takes no tokens: {}",
                    self.ctx.provider_kind().canonical_name(),
                    crate::hub_core::subscriptions::LOGINS_ARE_NOT_ACCOUNTS
                ),
                duration(),
            ));
        }

        if is_claude_oat(&params.token) {
            return Ok(ToolResult::failure(oat_refusal("That"), duration()));
        }

        // Store token in vault
        let vault_path = format!("llm/tokens/{}", params.name);
        self.ctx.vault.set(&vault_path, &params.token)?;

        // Start proxy and add provider to pool
        let model = self
            .ctx
            .active_model
            .read()
            .clone()
            .unwrap_or_else(|| self.ctx.current_default_model.read().clone());

        match create_named_provider_async(
            self.ctx.provider_kind(),
            &params.name,
            &params.token,
            Some(&model),
            self.ctx.llm_config.thinking_budget_tokens,
            self.ctx.llm_config.zai_subscription,
        )
        .await
        {
            Ok(provider) => {
                self.ctx.pool.add(&params.name, Arc::from(provider)).await;
                let pool_status = self.ctx.pool.list().await;
                info!(
                    name = %params.name,
                    pool_size = pool_status.len(),
                    "LLM token added via system.add_llm_token"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "added": true,
                        "name": params.name,
                        "pool_size": pool_status.len(),
                        "message": format!("Token '{}' added to pool", params.name),
                    }),
                    duration(),
                ))
            }
            Err(e) => {
                // Clean up vault entry on failure
                let _ = self.ctx.vault.delete(&vault_path);
                Ok(ToolResult::failure(
                    format!(
                        "Failed to start provider for token '{}': {}",
                        params.name, e
                    ),
                    duration(),
                ))
            }
        }
    }
}

// ─── system.remove_llm_token ─────────────────────────────────────────────────

pub struct SystemRemoveLlmTokenTool {
    ctx: LlmToolContext,
}

impl SystemRemoveLlmTokenTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RemoveTokenInput {
    name: String,
}

#[async_trait]
impl Tool for SystemRemoveLlmTokenTool {
    fn name(&self) -> &str {
        "system.remove_llm_token"
    }

    fn description(&self) -> &str {
        "Remove a token from the LLM token pool and vault"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the token to remove"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RemoveTokenInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Don't allow removing the last token
        let pool_size = self.ctx.pool.len().await;
        if pool_size <= 1 {
            return Ok(ToolResult::failure(
                "Cannot remove the last token in the pool".to_string(),
                duration(),
            ));
        }

        // Remove from pool
        let removed = self.ctx.pool.remove(&params.name).await;
        if !removed {
            return Ok(ToolResult::failure(
                format!("Token '{}' not found in pool", params.name),
                duration(),
            ));
        }

        // Remove from vault
        let vault_path = format!("llm/tokens/{}", params.name);
        let _ = self.ctx.vault.delete(&vault_path);

        let new_pool_size = self.ctx.pool.len().await;
        info!(
            name = %params.name,
            pool_size = new_pool_size,
            "LLM token removed via system.remove_llm_token"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "removed": true,
                "name": params.name,
                "pool_size": new_pool_size,
            }),
            duration(),
        ))
    }
}

// ─── system.list_llm_tokens ─────────────────────────────────────────────────

pub struct SystemListLlmTokensTool {
    ctx: LlmToolContext,
}

impl SystemListLlmTokensTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemListLlmTokensTool {
    fn name(&self) -> &str {
        "system.list_llm_tokens"
    }

    fn description(&self) -> &str {
        "List all tokens in the LLM token pool with their health status"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let pool_status = self.ctx.pool.list().await;
        let entries: Vec<serde_json::Value> = pool_status.iter().map(format_pool_entry).collect();
        let available_count = pool_status.iter().filter(|e| !e.is_rate_limited).count();
        let rate_limited_count = pool_status.len() - available_count;

        Ok(ToolResult::success(
            serde_json::json!({
                "tokens": entries,
                "count": entries.len(),
                "available": available_count,
                "rate_limited": rate_limited_count,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── system.set_failover_chain ───────────────────────────────────────────────

pub struct SystemSetFailoverChainTool {
    ctx: LlmToolContext,
}

impl SystemSetFailoverChainTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct SetFailoverChainInput {
    chain: Vec<String>,
}

#[async_trait]
impl Tool for SystemSetFailoverChainTool {
    fn name(&self) -> &str {
        "system.set_failover_chain"
    }

    fn description(&self) -> &str {
        "Configure model failover chain at runtime"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "chain": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ordered list of models to try"
                }
            },
            "required": ["chain"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetFailoverChainInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let fp = match &self.ctx.failover_provider {
            Some(fp) => fp,
            None => {
                return Ok(ToolResult::failure(
                    "Failover is not configured for this agent. \
                     Add a failover section to the agent's LLM config first."
                        .to_string(),
                    duration(),
                ));
            }
        };

        // Validate that external provider prefixes have providers available
        let external_providers = fp.list_external_providers().await;
        let mut missing: Vec<String> = Vec::new();
        for entry in &params.chain {
            if let (Some(prefix), _) = workforce_agent::llm::tiers::split_routing_prefix(entry) {
                if !external_providers.contains(&prefix.to_string()) {
                    missing.push(prefix.to_string());
                }
            }
        }

        if !missing.is_empty() {
            return Ok(ToolResult::failure(
                format!(
                    "External providers not available: {}. \
                     Use system.add_account to add accounts for these providers first.",
                    missing.join(", ")
                ),
                duration(),
            ));
        }

        if let Err(e) = self.ctx.persist(|cfg| {
            let llm = cfg.llm.get_or_insert_with(Default::default);
            llm.failover = Some(params.chain.clone());
        }) {
            return Ok(ToolResult::failure(
                format!("The failover chain was not changed: {e}. No changes were made."),
                duration(),
            ));
        }
        fp.set_chain(params.chain.clone()).await;

        info!(chain = ?params.chain, "Failover chain updated via system.set_failover_chain");

        Ok(ToolResult::success(
            serde_json::json!({
                "chain": params.chain,
                "message": "Failover chain updated — takes effect on next LLM call",
            }),
            duration(),
        ))
    }
}

// ─── system.add_account ─────────────────────────────────────────────────────

pub struct SystemAddAccountTool {
    ctx: LlmToolContext,
}

impl SystemAddAccountTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct AddAccountInput {
    name: String,
    provider: String,
    api_key: String,
}

#[async_trait]
impl Tool for SystemAddAccountTool {
    fn name(&self) -> &str {
        "system.add_account"
    }

    fn description(&self) -> &str {
        "Add a backup LLM provider account"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Friendly name for this account"
                },
                "provider": {
                    "type": "string",
                    "enum": ["anthropic", "openrouter", "grok", "gemini", "ollama", "vllm", "cerebras"],
                    "description": "Provider type for this account"
                },
                "api_key": {
                    "type": "string",
                    "description": "API key or token for this account"
                }
            },
            "required": ["name", "provider", "api_key"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: AddAccountInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if params.name.is_empty() || params.api_key.is_empty() {
            return Ok(ToolResult::failure(
                "Name and api_key cannot be empty".to_string(),
                duration(),
            ));
        }

        let provider_kind = match ProviderKind::parse(&params.provider) {
            Ok(k) => k,
            Err(removed @ ProviderParseError::Removed { .. }) => {
                return Ok(ToolResult::failure(removed.to_string(), duration()));
            }
            Err(ProviderParseError::Unknown(_)) => {
                return Ok(ToolResult::failure(
                    format!(
                        "Unknown provider '{}'. Supported: anthropic, grok, openrouter, ollama, gemini",
                        params.provider
                    ),
                    duration(),
                ));
            }
        };

        if provider_kind.is_subscription() {
            return Ok(ToolResult::failure(
                format!(
                    "'{}' is not an account provider: {}",
                    params.provider,
                    crate::hub_core::subscriptions::LOGINS_ARE_NOT_ACCOUNTS
                ),
                duration(),
            ));
        }

        if is_claude_oat(&params.api_key) {
            return Ok(ToolResult::failure(oat_refusal("That"), duration()));
        }

        let is_primary = crate::agent_llm::AgentProviders::pools_as_primary(
            self.ctx.provider_kind(),
            provider_kind,
        );

        // The provider built here is used only for `verify_credentials`,
        // which doesn't invoke a model — so the per-tier model and the
        // thinking-budget aren't load-bearing for the probe. We still
        // pass the thinking budget to keep the provider's configured state
        // consistent with how the same key will be used once added.
        let provider = match create_named_provider_async(
            provider_kind,
            &params.name,
            &params.api_key,
            None,
            if is_primary {
                self.ctx.llm_config.thinking_budget_tokens
            } else {
                None
            },
            self.ctx.llm_config.zai_subscription,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to create provider for '{}': {}", params.name, e),
                    duration(),
                ));
            }
        };

        // Auth-only credential probe. Providers that expose `/v1/models`
        // (OpenRouter, Anthropic, OpenAI-compat, etc.) check the key
        // without invoking a model, so a flaky upstream behind the
        // provider can't masquerade as an auth failure. Providers
        // without such an endpoint (Bedrock, Vertex, local Ollama/vLLM)
        // fall back to a 1-token completion via the trait's default.
        if let Err(e) = provider.verify_credentials().await {
            warn!(
                name = %params.name,
                provider = %params.provider,
                error = %e,
                "Account validation failed"
            );
            return Ok(ToolResult::failure(
                format!(
                    "Account '{}' validation failed — credential probe returned: {}",
                    params.name, e
                ),
                duration(),
            ));
        }

        // Store in vault
        let vault_path = format!("llm/accounts/{}", params.name);
        let account_data = serde_json::json!({
            "provider": params.provider,
            "api_key": params.api_key,
        });
        self.ctx.vault.set(&vault_path, &account_data.to_string())?;
        // The image provider built at the last `[llm.image]` reload captured
        // whatever credential the vault held at that moment. Rebuild it now so
        // a newly-added Gemini/OpenRouter/Bedrock key flows into image gen
        // without needing a config.toml touch.
        (self.ctx.reload_image_tool)();

        let provider_arc: Arc<dyn LlmProvider> = Arc::from(provider);

        if is_primary {
            // Add to pool for token rotation
            self.ctx
                .pool
                .add(&params.name, Arc::clone(&provider_arc))
                .await;
            let pool_size = self.ctx.pool.len().await;
            info!(
                name = %params.name,
                provider = %params.provider,
                pool_size = pool_size,
                "Primary account added to pool via system.add_account"
            );
            Ok(ToolResult::success(
                serde_json::json!({
                    "added": true,
                    "name": params.name,
                    "provider": params.provider,
                    "type": "primary_pool",
                    "pool_size": pool_size,
                    "message": format!("Account '{}' added to primary token pool (size: {})", params.name, pool_size),
                }),
                duration(),
            ))
        } else {
            // Add as external provider for failover chain
            match &self.ctx.failover_provider {
                Some(fp) => {
                    fp.add_external_provider(&params.provider, provider_arc)
                        .await;
                    info!(
                        name = %params.name,
                        provider = %params.provider,
                        "External provider account added via system.add_account"
                    );
                    Ok(ToolResult::success(
                        serde_json::json!({
                            "added": true,
                            "name": params.name,
                            "provider": params.provider,
                            "type": "external_provider",
                            "message": format!(
                                "Account '{}' added as external provider '{}'. \
                                 Use 'system.set_failover_chain' to include '{}:<model>' in the chain.",
                                params.name, params.provider, params.provider
                            ),
                        }),
                        duration(),
                    ))
                }
                None => {
                    // No failover provider — still store in vault for future use
                    info!(
                        name = %params.name,
                        provider = %params.provider,
                        "External account stored (no failover provider active)"
                    );
                    Ok(ToolResult::success(
                        serde_json::json!({
                            "added": true,
                            "name": params.name,
                            "provider": params.provider,
                            "type": "stored_only",
                            "message": format!(
                                "Account '{}' stored in vault but failover is not configured. \
                                 It will be available when failover is enabled.",
                                params.name
                            ),
                        }),
                        duration(),
                    ))
                }
            }
        }
    }
}

// ─── system.remove_account ──────────────────────────────────────────────────

pub struct SystemRemoveAccountTool {
    ctx: LlmToolContext,
}

impl SystemRemoveAccountTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RemoveAccountInput {
    name: String,
}

#[async_trait]
impl Tool for SystemRemoveAccountTool {
    fn name(&self) -> &str {
        "system.remove_account"
    }

    fn description(&self) -> &str {
        "Remove a backup account by name"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the account to remove"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RemoveAccountInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let mut removed_from = Vec::new();

        // Try removing from pool (but never the last entry)
        let pool_size = self.ctx.pool.len().await;
        if pool_size > 1 && self.ctx.pool.remove(&params.name).await {
            removed_from.push("token_pool");
        } else if pool_size <= 1 && self.ctx.pool.has(&params.name).await {
            return Ok(ToolResult::failure(
                "Cannot remove the last primary account".to_string(),
                duration(),
            ));
        }

        // Try removing from external providers
        if let Some(ref fp) = self.ctx.failover_provider {
            // The account name might be the provider key
            if fp.remove_external_provider(&params.name).await {
                removed_from.push("external_providers");
            }
        }

        // Remove from vault
        let vault_path = format!("llm/accounts/{}", params.name);
        let _ = self.ctx.vault.delete(&vault_path);
        // Also try old token path for backwards compatibility
        let old_vault_path = format!("llm/tokens/{}", params.name);
        let _ = self.ctx.vault.delete(&old_vault_path);
        // Image provider may have been using this account's key — rebuild so
        // the next request resolves against the remaining accounts (or removes
        // the tool when no usable credential is left).
        (self.ctx.reload_image_tool)();

        if removed_from.is_empty() {
            Ok(ToolResult::failure(
                format!(
                    "Account '{}' not found in pool or external providers",
                    params.name
                ),
                duration(),
            ))
        } else {
            info!(
                name = %params.name,
                removed_from = ?removed_from,
                "Account removed via system.remove_account"
            );
            Ok(ToolResult::success(
                serde_json::json!({
                    "removed": true,
                    "name": params.name,
                    "removed_from": removed_from,
                }),
                duration(),
            ))
        }
    }
}

// ─── system.list_accounts ───────────────────────────────────────────────────

pub struct SystemListAccountsTool {
    ctx: LlmToolContext,
}

impl SystemListAccountsTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemListAccountsTool {
    fn name(&self) -> &str {
        "system.list_accounts"
    }

    fn description(&self) -> &str {
        "List all LLM accounts across all providers"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let provider_name = format!("{:?}", self.ctx.provider_kind());

        // Primary pool entries
        let pool_status = self.ctx.pool.list().await;
        let pool_entries: Vec<serde_json::Value> = pool_status
            .iter()
            .map(|e| {
                let mut entry = serde_json::json!({
                    "name": e.name,
                    "provider": &provider_name,
                    "type": "primary_pool",
                    "is_rate_limited": e.is_rate_limited,
                    "rate_limited_secs_remaining": e.rate_limited_secs_remaining,
                });
                if let Some(original) = e.original_retry_after_secs {
                    entry["original_retry_after_secs"] = serde_json::json!(original);
                }
                entry
            })
            .collect();

        // External provider entries
        let external_entries: Vec<serde_json::Value> =
            if let Some(ref fp) = self.ctx.failover_provider {
                fp.list_external_providers()
                    .await
                    .iter()
                    .map(|key| {
                        serde_json::json!({
                            "name": key,
                            "provider": key,
                            "type": "external_provider",
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };

        let total = pool_entries.len() + external_entries.len();

        Ok(ToolResult::success(
            serde_json::json!({
                "primary_accounts": pool_entries,
                "external_accounts": external_entries,
                "total_count": total,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── system.set_routing ─────────────────────────────────────────────────────

pub struct SystemSetRoutingTool {
    ctx: LlmToolContext,
}

impl SystemSetRoutingTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct SetRoutingInput {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    investigation_switch_round: Option<u32>,
    #[serde(default)]
    coding_switch_round: Option<u32>,
    #[serde(default)]
    read_only_threshold: Option<u32>,
    #[serde(default)]
    fast_round_threshold_ms: Option<u64>,
    #[serde(default)]
    escalate_on_haiku_failure: Option<bool>,
    #[serde(default)]
    reset: Option<bool>,
}

#[async_trait]
impl Tool for SystemSetRoutingTool {
    fn name(&self) -> &str {
        "system.set_routing"
    }

    fn description(&self) -> &str {
        "Configure task-type model routing preferences"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "enabled": {
                    "type": "boolean",
                    "description": "Enable or disable adaptive routing"
                },
                "investigation_switch_round": {
                    "type": "integer",
                    "description": "Round at which to switch for investigation tasks (1–50)"
                },
                "coding_switch_round": {
                    "type": "integer",
                    "description": "Round at which to switch for coding tasks (1–100)"
                },
                "read_only_threshold": {
                    "type": "integer",
                    "description": "Consecutive read-only rounds before switching (1–20)"
                },
                "fast_round_threshold_ms": {
                    "type": "integer",
                    "description": "Rounds faster than this (ms) are considered simple"
                },
                "escalate_on_haiku_failure": {
                    "type": "boolean",
                    "description": "Auto-escalate back to Sonnet on Haiku failure"
                },
                "reset": {
                    "type": "boolean",
                    "description": "Reset all routing overrides to defaults"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetRoutingInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        // Reset case
        if params.reset.unwrap_or(false) {
            *self.ctx.active_routing.write() = None;
            // Remove from workspace config.toml
            if let Err(e) = self.ctx.persist(|cfg| {
                if let Some(ref mut llm) = cfg.llm {
                    llm.routing = None;
                }
            }) {
                warn!(error = %e, "Failed to update workspace config");
            }
            // Legacy vault cleanup
            if let Err(e) = self.ctx.vault.delete("llm/active_routing") {
                tracing::debug!("vault delete llm/active_routing: {}", e);
            }
            info!("Routing config reset to defaults");
            let defaults = workforce_agent::model_router::RoutingConfig::default();
            return Ok(ToolResult::success(
                serde_json::json!({
                    "message": "Routing config reset to defaults",
                    "config": serde_json::to_value(&defaults).unwrap_or_default(),
                }),
                duration(),
            ));
        }

        // Validate bounds
        if let Some(v) = params.investigation_switch_round {
            if !(1..=50).contains(&v) {
                return Ok(ToolResult::failure(
                    format!("investigation_switch_round {} out of range (1–50)", v),
                    duration(),
                ));
            }
        }
        if let Some(v) = params.coding_switch_round {
            if !(1..=100).contains(&v) {
                return Ok(ToolResult::failure(
                    format!("coding_switch_round {} out of range (1–100)", v),
                    duration(),
                ));
            }
        }
        if let Some(v) = params.read_only_threshold {
            if !(1..=20).contains(&v) {
                return Ok(ToolResult::failure(
                    format!("read_only_threshold {} out of range (1–20)", v),
                    duration(),
                ));
            }
        }
        if let Some(v) = params.fast_round_threshold_ms {
            if !(5_000..=300_000).contains(&v) {
                return Ok(ToolResult::failure(
                    format!("fast_round_threshold_ms {} out of range (5000–300000)", v),
                    duration(),
                ));
            }
        }

        // Merge partial updates over the effective routing (override if set,
        // else the hot-reloaded `[llm.routing]`) so unspecified fields keep
        // their configured values rather than resetting to defaults.
        let current = self.ctx.current_routing();

        let updated = workforce_agent::model_router::RoutingConfig {
            enable_adaptive_routing: params.enabled.unwrap_or(current.enable_adaptive_routing),
            investigation_switch_round: params
                .investigation_switch_round
                .unwrap_or(current.investigation_switch_round),
            coding_switch_round: params
                .coding_switch_round
                .unwrap_or(current.coding_switch_round),
            read_only_threshold: params
                .read_only_threshold
                .unwrap_or(current.read_only_threshold),
            fast_round_threshold_ms: params
                .fast_round_threshold_ms
                .unwrap_or(current.fast_round_threshold_ms),
            escalate_on_haiku_failure: params
                .escalate_on_haiku_failure
                .unwrap_or(current.escalate_on_haiku_failure),
        };

        // Persist to workspace config.toml
        let routing_for_save = workforce_common::settings::WorkspaceRoutingConfig {
            enable_adaptive_routing: Some(updated.enable_adaptive_routing),
            investigation_switch_round: Some(updated.investigation_switch_round),
            coding_switch_round: Some(updated.coding_switch_round),
            read_only_threshold: Some(updated.read_only_threshold),
            fast_round_threshold_ms: Some(updated.fast_round_threshold_ms),
            escalate_on_haiku_failure: Some(updated.escalate_on_haiku_failure),
        };
        if let Err(e) = self.ctx.persist(|cfg| {
            let llm = cfg.llm.get_or_insert_with(Default::default);
            llm.routing = Some(routing_for_save.clone());
        }) {
            warn!(error = %e, "Failed to update workspace config");
        }

        *self.ctx.active_routing.write() = Some(updated.clone());

        info!("Routing config updated via system.set_routing");
        Ok(ToolResult::success(
            serde_json::json!({
                "message": "Routing config updated — takes effect on next message",
                "config": serde_json::to_value(&updated).unwrap_or_default(),
            }),
            duration(),
        ))
    }
}

// ─── SystemRoutingStatusTool ────────────────────────────────────────────────

/// Tool for reading adaptive routing configuration from workforce.toml.
#[allow(dead_code)]
pub struct SystemRoutingStatusTool {
    ctx: LlmToolContext,
}

impl SystemRoutingStatusTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemRoutingStatusTool {
    fn name(&self) -> &str {
        "system.routing_status"
    }

    fn description(&self) -> &str {
        "Configure task-type model routing preferences"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        // Try multiple possible locations for workforce.toml
        let possible_paths = vec![
            std::path::PathBuf::from("/app/workforce.toml"),
            std::path::PathBuf::from("./workforce.toml"),
            std::env::current_dir()
                .ok()
                .map(|p| p.join("workforce.toml"))
                .unwrap_or_else(|| std::path::PathBuf::from("workforce.toml")),
        ];

        let mut config_content = None;
        for path in &possible_paths {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                config_content = Some(content);
                break;
            }
        }

        let config_str = config_content.ok_or_else(|| {
            eyre::eyre!("Could not find workforce.toml in any of the expected locations")
        })?;

        // Parse TOML to extract routing config
        let toml_value: toml::Value = toml::from_str(&config_str)?;

        // Extract llm.routing section
        let routing = toml_value
            .get("llm")
            .and_then(|llm| llm.get("routing"))
            .ok_or_else(|| eyre::eyre!("[llm.routing] section not found in workforce.toml"))?;

        let enabled = routing
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let enable_escalation = routing
            .get("enable_escalation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let escalation_model = routing
            .get("escalation_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Extract routing rules
        let rules = routing
            .get("rules")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|rule| {
                        Some(serde_json::json!({
                            "task_type": rule.get("task_type")?.as_str()?,
                            "initial_model": rule.get("initial_model")?.as_str()?,
                            "fallback_model": rule.get("fallback_model")?.as_str()?,
                            "fallback_after_round": rule.get("fallback_after_round")?.as_integer()?,
                            "repetition_threshold": rule.get("repetition_threshold")
                                .and_then(|v| v.as_integer()),
                        }))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Collect runtime state from LlmToolContext
        let active_model = self.ctx.active_model.read().clone();
        let active_max_rounds = *self.ctx.active_max_rounds.read();
        let active_max_tokens = *self.ctx.active_max_tokens.read();
        let routing_override = self.ctx.active_routing.read().clone();

        Ok(ToolResult::success(
            serde_json::json!({
                "config": {
                    "enabled": enabled,
                    "rules": rules,
                    "escalation": {
                        "enabled": enable_escalation,
                        "escalation_model": escalation_model,
                    },
                },
                "runtime": {
                    "active_model_override": active_model,
                    "active_max_rounds_override": active_max_rounds,
                    "active_max_tokens_override": active_max_tokens,
                    "routing_override": routing_override.map(|r| serde_json::to_value(&r).unwrap_or_default()),
                    "default_model": self.ctx.current_default_model.read().clone(),
                    "default_max_rounds": self.ctx.llm_config.max_rounds,
                    "default_max_tokens": self.ctx.llm_config.max_tokens,
                },
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── system.set_provider ────────────────────────────────────────────────────

/// JSON-schema for `system.set_provider`. The `provider` enum is the union
/// of every bare [`ProviderKind`] name that
/// [`workforce_agent::llm::tiers::parse_provider_tier_key`] accepts and every
/// `provider:family` combination present in the [`TierRegistry`], filtered
/// through the same parser. Bare names that the parser rejects (`bedrock`,
/// `cerebras`, `openrouter`) are excluded so JSON-Schema-strict clients
/// cannot send a value the backend will refuse.
fn set_provider_input_schema() -> serde_json::Value {
    use std::collections::BTreeSet;
    let registry = workforce_agent::llm::tiers::TierRegistry::new(None);

    let bare = ProviderKind::ALL
        .iter()
        .map(|k| k.canonical_name().to_string())
        .filter(|n| workforce_agent::llm::tiers::parse_provider_tier_key(n).is_ok());

    let families = registry.tier_keys().filter_map(|k| {
        k.split_once('.').and_then(|(base, family)| {
            let spec = format!("{base}:{family}");
            workforce_agent::llm::tiers::parse_provider_tier_key(&spec)
                .ok()
                .map(|_| spec)
        })
    });

    let names: Vec<String> = bare
        .chain(families)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    serde_json::json!({
        "type": "object",
        "properties": {
            "provider": {
                "type": "string",
                "enum": names,
                "description": "Provider to switch to. Multi-family providers (bedrock, cerebras, openrouter) are listed with their family suffix, e.g. `bedrock:anthropic`."
            },
            "default_tier": {
                "type": "string",
                "enum": ["powerful", "standard", "cheap"],
                "description": "Default model tier to use (default: 'standard')"
            }
        },
        "required": ["provider"]
    })
}

pub struct SystemSetProviderTool {
    ctx: LlmToolContext,
}

impl SystemSetProviderTool {
    pub fn new(ctx: LlmToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct SetProviderInput {
    provider: String,
    #[serde(default)]
    default_tier: Option<String>,
}

#[async_trait]
impl Tool for SystemSetProviderTool {
    fn name(&self) -> &str {
        "system.set_provider"
    }

    fn description(&self) -> &str {
        "Switch the primary LLM provider at runtime"
    }

    fn input_schema(&self) -> serde_json::Value {
        set_provider_input_schema()
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: SetProviderInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        let (new_kind, new_tier_key) =
            match workforce_agent::llm::tiers::parse_provider_tier_key(&params.provider) {
                Ok((name, key)) => match ProviderKind::parse(&name) {
                    Ok(kind) => (kind, key),
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!(
                                "{e}. Supported: {}",
                                ProviderKind::ALL
                                    .iter()
                                    .map(|k| k.canonical_name())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            duration(),
                        ));
                    }
                },
                Err(e) => {
                    return Ok(ToolResult::failure(format!("{}", e), duration()));
                }
            };

        let current = self.ctx.provider_kind();
        if current == new_kind {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "provider": params.provider,
                    "status": "already_active",
                }),
                duration(),
            ));
        }

        let tiers = self.ctx.tier_registry();
        let tier = params.default_tier.as_deref().unwrap_or("standard");
        let model = tiers.resolve_model(tier, &new_tier_key);

        let canonical_model = if matches!(new_kind, ProviderKind::Bedrock) {
            let region = workforce_agent::llm::bedrock::resolve_region(None);
            match self
                .ctx
                .bedrock_catalog
                .resolve_and_validate(&model, &region)
                .await
            {
                Ok(canonical) => canonical,
                Err(CatalogError::UnknownModel {
                    id,
                    region,
                    suggestions,
                }) => {
                    let listed = format_bedrock_suggestions(&suggestions);
                    return Ok(ToolResult::failure(
                        format!(
                            "Bedrock model {id:?} is not available in region {region}. \
                             Override [bedrock.{new_tier_key}] in your workspace tiers.toml. \
                             Available alternatives: {listed}"
                        ),
                        duration(),
                    ));
                }
            }
        } else {
            model
        };

        let provider = match self
            .ctx
            .providers
            .provider(
                new_kind,
                Some(&canonical_model),
                self.ctx.llm_config.thinking_budget_tokens,
                self.ctx.llm_config.zai_subscription,
                None,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to create provider: {}", e),
                    duration(),
                ));
            }
        };

        // Switching providers must re-point the agent's default model at the
        // new provider. Historically `system.set_provider` left
        // `current_default_model` (and any `active_model` override) pinned to
        // the previous provider's model — e.g. an `anthropic/claude-opus-4.7`
        // set via `set_model` while on OpenRouter survived a switch to Cerebras
        // and lingered as the "config default", producing a provider/model
        // mismatch in `system.llm_status` (and a hard failure whenever no
        // per-task `model_preferences` entry overrode it). Mirror
        // `AgentService::reload_provider`: re-resolve the agent's configured
        // default against the new `tier_key`. If that default is a concrete
        // model id (a legacy `set_model` write) rather than a tier name,
        // classify it to a tier first so we never pin a foreign provider's
        // model — and persist the resulting tier so it can't regress on the
        // next restart.
        let new_default_tier = {
            let tiers = self.ctx.tier_registry();
            let configured = self.ctx.original_default_model.read().clone();
            workforce_agent::llm::tiers::ModelTier::from_name(&configured)
                .unwrap_or_else(|| tiers.classify(&configured))
                .as_str()
                .to_string()
        };
        // Persisted before the switch, and only if the agent would load the
        // result: a provider the agent could not boot on never goes live.
        if let Err(e) = self.ctx.persist(|cfg| {
            let llm = cfg.llm.get_or_insert_with(Default::default);
            llm.provider = Some(params.provider.clone());
            // Persist the default as a provider-agnostic tier so a restart
            // re-resolves it against the new provider rather than re-seeding
            // a stale concrete model from the previous provider.
            llm.default_model = Some(new_default_tier.clone());
        }) {
            return Ok(ToolResult::failure(
                format!(
                    "Provider not switched to {}: {e}. No changes were made.",
                    params.provider
                ),
                duration(),
            ));
        }

        self.ctx
            .providers
            .swap_primary(
                Some(&self.ctx.pool),
                self.ctx.failover_provider.as_deref(),
                new_kind,
                provider,
                &self.ctx.llm_config,
                &self.ctx.resolved.snapshot(),
            )
            .await;
        *self.ctx.provider_kind.write() = new_kind;
        *self.ctx.tier_key.write() = new_tier_key;
        {
            let tiers = self.ctx.tier_registry();
            let resolved_default = tiers.resolve_model(&new_default_tier, &self.ctx.tier_key());
            *self.ctx.current_default_model.write() = resolved_default.clone();
            // Drop any agent-wide model override; it referenced the old provider.
            *self.ctx.active_model.write() = None;
            let _ = self.ctx.vault.delete("llm/active_model");
            info!(
                tier = %new_default_tier,
                model = %resolved_default,
                "Reset default model to new provider's tier on provider switch"
            );
        }

        info!(
            from = ?current,
            to = ?new_kind,
            model = %canonical_model,
            "Provider switched via system.set_provider"
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "provider": params.provider,
                "model": canonical_model,
                "status": "switched",
                "message": format!("Provider switched to {}. Next message will use {}.", params.provider, canonical_model),
            }),
            duration(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::time::Duration;
    use workforce_agent::llm::bedrock_catalog::{BedrockControlPlane, BedrockModel, ModelKind};
    use workforce_agent::llm::pool::{CooldownReason, PoolEntryStatus};
    use workforce_common::settings::REMOVED_PROVIDER_POINTER;

    #[test]
    fn tool_names_are_correct() {
        let expected = [
            "system.llm_status",
            "system.set_model",
            "system.list_models",
            "system.set_max_tokens",
            "system.set_max_rounds",
            "system.add_llm_token",
            "system.remove_llm_token",
            "system.list_llm_tokens",
            "system.set_failover_chain",
            "system.add_account",
            "system.remove_account",
            "system.list_accounts",
            "system.set_session_model",
            "system.routing_status",
            "system.set_routing",
        ];
        for name in expected {
            assert!(name.starts_with("system."));
        }
    }

    #[tokio::test]
    async fn llm_status_reports_what_each_provider_runs_on_for_the_caller() {
        let mut ctx = test_llm_ctx();
        ctx.llm_config.failover = vec!["claude-code:sonnet".into(), "codex:cheap".into()];
        ctx.vault
            .set(
                "llm/accounts/main",
                r#"{"provider":"anthropic","api_key":"sk-ant-api03-test"}"#,
            )
            .unwrap();
        const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        ctx.vault
            .set(
                &format!("users/{ALICE}/claude_code/oauth_token"),
                &format!("sk-ant-oat01-{}", "a".repeat(80)),
            )
            .unwrap();
        let tool = SystemLlmStatusTool::new(ctx);

        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope(ALICE.to_string(), tool.execute(serde_json::json!({})))
            .await
            .unwrap();
        assert!(result.success, "{result:?}");
        let providers = result.output["providers"].as_array().unwrap();
        let by_name = |name: &str| {
            providers
                .iter()
                .find(|p| p["provider"] == name)
                .unwrap_or_else(|| panic!("{name} in {providers:?}"))
        };

        let anthropic = by_name("anthropic");
        assert_eq!(anthropic["uses"][0]["setting"], "default_model");
        assert_eq!(anthropic["credential"]["kind"], "api_key");
        assert_eq!(anthropic["credential"]["source"], "agent_vault");
        assert!(
            !anthropic.to_string().contains("sk-ant-api03"),
            "no credential value leaves the tool: {anthropic}"
        );

        let claude = by_name("claude-code");
        assert_eq!(claude["uses"][0]["spec"], "claude-code:sonnet");
        assert_eq!(claude["credential"]["kind"], "login");
        assert_eq!(claude["credential"]["plan"], "claude");
        assert!(
            claude["credential"]["serves_you"].is_string(),
            "Alice's own token serves her: {claude}"
        );
        assert!(claude["summary"]
            .as_str()
            .unwrap()
            .contains("your Claude plan login serves it"));

        let codex = by_name("codex");
        assert_eq!(codex["credential"]["plan"], "chatgpt");
        assert!(codex["credential"]["serves_you"].is_null());
        assert!(
            codex["credential"]["misses"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["reason"] == "none stored"),
            "{codex}"
        );
        let summary = result.output["credentials_summary"].as_array().unwrap();
        assert_eq!(summary.len(), 3);
        assert!(summary[2]
            .as_str()
            .unwrap()
            .starts_with("codex: no ChatGPT plan login of yours serves it"));
    }

    struct StaticControlPlane {
        profiles: Vec<BedrockModel>,
        models: Vec<BedrockModel>,
    }

    #[async_trait]
    impl BedrockControlPlane for StaticControlPlane {
        async fn list_inference_profiles(&self, _region: &str) -> eyre::Result<Vec<BedrockModel>> {
            Ok(self.profiles.clone())
        }
        async fn list_foundation_models(&self, _region: &str) -> eyre::Result<Vec<BedrockModel>> {
            Ok(self.models.clone())
        }
    }

    struct UnavailableControlPlane;

    #[async_trait]
    impl BedrockControlPlane for UnavailableControlPlane {
        async fn list_inference_profiles(&self, _region: &str) -> eyre::Result<Vec<BedrockModel>> {
            Err(eyre::eyre!("AccessDenied: bedrock:ListInferenceProfiles"))
        }
        async fn list_foundation_models(&self, _region: &str) -> eyre::Result<Vec<BedrockModel>> {
            Err(eyre::eyre!("AccessDenied: bedrock:ListFoundationModels"))
        }
    }

    #[tokio::test]
    async fn bedrock_snapshot_returns_catalog_entries() {
        let plane = Arc::new(StaticControlPlane {
            profiles: vec![BedrockModel {
                id: "us.anthropic.claude-sonnet-4-5-20250929-v1:0".into(),
                name: Some("Claude Sonnet 4.5".into()),
                kind: ModelKind::InferenceProfile,
            }],
            models: vec![BedrockModel {
                id: "anthropic.claude-sonnet-4-5-20250929-v1:0".into(),
                name: Some("Claude Sonnet 4.5".into()),
                kind: ModelKind::FoundationModel,
            }],
        });
        let store = BedrockCatalogStore::with_control_plane(plane, Duration::from_secs(3600));
        let entries = bedrock_catalog_snapshot(&store).await;
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .any(|e| e.id == "us.anthropic.claude-sonnet-4-5-20250929-v1:0"));
        assert!(entries
            .iter()
            .any(|e| e.id == "anthropic.claude-sonnet-4-5-20250929-v1:0"));
    }

    #[tokio::test]
    async fn bedrock_snapshot_returns_diagnostic_entry_when_unavailable() {
        let store = BedrockCatalogStore::with_control_plane(
            Arc::new(UnavailableControlPlane),
            Duration::from_secs(3600),
        );
        let entries = bedrock_catalog_snapshot(&store).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "<bedrock-catalog-unavailable>");
        assert!(entries[0]
            .display_name
            .as_deref()
            .is_some_and(|d| d.contains("AccessDenied")));
    }

    #[test]
    fn filter_and_encode_models_applies_substring_filter() {
        let entries = [
            ModelInfo {
                id: "anthropic.claude-opus-4-7".into(),
                display_name: Some("Opus 4.7".into()),
            },
            ModelInfo {
                id: "meta.llama3-70b".into(),
                display_name: None,
            },
        ];
        let filtered = filter_and_encode_models(&entries, Some("opus"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["id"], "anthropic.claude-opus-4-7");
        assert_eq!(filtered[0]["display_name"], "Opus 4.7");
    }

    #[test]
    fn filter_and_encode_models_no_filter_returns_all() {
        let entries = [
            ModelInfo {
                id: "a".into(),
                display_name: None,
            },
            ModelInfo {
                id: "b".into(),
                display_name: None,
            },
        ];
        assert_eq!(filter_and_encode_models(&entries, None).len(), 2);
        assert_eq!(filter_and_encode_models(&entries, Some("")).len(), 2);
    }

    #[test]
    fn list_models_input_accepts_provider_field() {
        let parsed: ListModelsInput =
            serde_json::from_value(serde_json::json!({ "provider": "bedrock" })).unwrap();
        assert_eq!(parsed.provider.as_deref(), Some("bedrock"));
        let none: ListModelsInput = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(none.provider.is_none());
    }

    #[test]
    fn is_bedrock_provider_arg_matches_canonical_and_family_forms() {
        assert!(is_bedrock_provider_arg(Some("bedrock")));
        assert!(is_bedrock_provider_arg(Some("Bedrock")));
        assert!(is_bedrock_provider_arg(Some(" bedrock ")));
        assert!(is_bedrock_provider_arg(Some("bedrock:anthropic")));
        assert!(is_bedrock_provider_arg(Some("AWS:zai")));
        assert!(is_bedrock_provider_arg(Some("aws-bedrock:qwen")));
        assert!(!is_bedrock_provider_arg(Some("anthropic")));
        assert!(!is_bedrock_provider_arg(Some("openrouter:bedrock")));
        assert!(!is_bedrock_provider_arg(Some("")));
        assert!(!is_bedrock_provider_arg(None));
    }

    // ── format_duration_human ────────────────────────────────────────

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration_human(0), "0s");
    }

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration_human(45), "45s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        assert_eq!(format_duration_human(150), "2m 30s");
    }

    #[test]
    fn format_duration_exact_minutes() {
        assert_eq!(format_duration_human(300), "5m");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration_human(5400), "1h 30m");
    }

    #[test]
    fn format_duration_exact_hours() {
        assert_eq!(format_duration_human(7200), "2h");
    }

    #[test]
    fn format_duration_days_and_hours() {
        assert_eq!(format_duration_human(302400), "3d 12h");
    }

    #[test]
    fn format_duration_exact_days() {
        assert_eq!(format_duration_human(172800), "2d");
    }

    // ── format_pool_entry ────────────────────────────────────────────

    #[test]
    fn format_pool_entry_available() {
        let entry = PoolEntryStatus {
            name: "default".to_string(),
            is_rate_limited: false,
            rate_limited_secs_remaining: None,
            original_retry_after_secs: None,
            cooldown_reason: None,
            cooldown_error: None,
            model_cooldowns: vec![],
        };
        let json = format_pool_entry(&entry);
        assert_eq!(json["name"], "default");
        assert_eq!(json["status"], "available");
        assert_eq!(json["is_rate_limited"], false);
        assert!(json.get("effective_cooldown_display").is_none());
        assert!(json.get("is_long_term").is_none());
    }

    #[test]
    fn format_pool_entry_rate_limited_short_term() {
        let entry = PoolEntryStatus {
            name: "token-1".to_string(),
            is_rate_limited: true,
            rate_limited_secs_remaining: Some(270),
            original_retry_after_secs: Some(300),
            cooldown_reason: Some(CooldownReason::RateLimited),
            cooldown_error: Some("Anthropic API error (429): rate limit exceeded".to_string()),
            model_cooldowns: vec![],
        };
        let json = format_pool_entry(&entry);
        assert_eq!(json["status"], "rate-limited");
        assert_eq!(json["reason"], "rate-limited");
        assert_eq!(json["effective_cooldown_display"], "4m 30s");
        assert_eq!(json["original_retry_after_display"], "5m");
        assert_eq!(json["is_long_term"], false);
    }

    #[test]
    fn format_pool_entry_rate_limited_long_term() {
        let entry = PoolEntryStatus {
            name: "token-2".to_string(),
            is_rate_limited: true,
            rate_limited_secs_remaining: Some(250),
            original_retry_after_secs: Some(302400), // 3.5 days
            cooldown_reason: Some(CooldownReason::RateLimited),
            cooldown_error: None,
            model_cooldowns: vec![],
        };
        let json = format_pool_entry(&entry);
        assert_eq!(json["status"], "rate-limited");
        assert_eq!(json["reason"], "rate-limited");
        assert_eq!(json["effective_cooldown_display"], "4m 10s");
        assert_eq!(json["original_retry_after_display"], "3d 12h");
        assert_eq!(json["is_long_term"], true);
    }

    #[test]
    fn format_pool_entry_auth_failed() {
        let entry = PoolEntryStatus {
            name: "expired-token".to_string(),
            is_rate_limited: true,
            rate_limited_secs_remaining: Some(55),
            original_retry_after_secs: Some(60),
            cooldown_reason: Some(CooldownReason::AuthFailed),
            cooldown_error: Some("Bedrock API error (403): Model access is denied".to_string()),
            model_cooldowns: vec![],
        };
        let json = format_pool_entry(&entry);
        assert_eq!(json["status"], "auth-failed");
        assert_eq!(json["reason"], "auth-failed");
        assert_eq!(
            json["last_error"],
            "Bedrock API error (403): Model access is denied"
        );
        assert!(json["note"]
            .as_str()
            .unwrap()
            .contains("invalid or expired"));
    }

    #[test]
    fn format_pool_entry_renders_model_cooldowns_on_available_entry() {
        let entry = PoolEntryStatus {
            name: "default".to_string(),
            is_rate_limited: false,
            rate_limited_secs_remaining: None,
            original_retry_after_secs: None,
            cooldown_reason: None,
            cooldown_error: None,
            model_cooldowns: vec![ModelCooldownStatus {
                model: "eu.anthropic.claude-opus-4-8".to_string(),
                secs_remaining: 42,
                reason: CooldownReason::AuthFailed,
                error: "Bedrock API error (403): Model access is denied".to_string(),
            }],
        };
        let json = format_pool_entry(&entry);
        assert_eq!(json["status"], "available");
        let cooldowns = json["model_cooldowns"].as_array().unwrap();
        assert_eq!(cooldowns.len(), 1);
        assert_eq!(cooldowns[0]["model"], "eu.anthropic.claude-opus-4-8");
        assert_eq!(cooldowns[0]["reason"], "auth-failed");
        assert_eq!(cooldowns[0]["remaining_display"], "42s");
        assert!(json["model_cooldowns_note"]
            .as_str()
            .unwrap()
            .contains("other"));
    }

    fn schema_enum() -> Vec<String> {
        set_provider_input_schema()["properties"]["provider"]["enum"]
            .as_array()
            .expect("enum present")
            .iter()
            .map(|v| v.as_str().expect("string enum entry").to_string())
            .collect()
    }

    /// Every value the schema exposes must round-trip through the parser
    /// the backend uses. Catches drift between schema and runtime contract.
    #[test]
    fn set_provider_schema_enum_round_trips_through_parse_provider_tier_key() {
        for spec in schema_enum() {
            workforce_agent::llm::tiers::parse_provider_tier_key(&spec).unwrap_or_else(|e| {
                panic!("schema enum entry {spec:?} fails parse_provider_tier_key: {e}")
            });
        }
    }

    /// The actual regression this PR fixes — Atlas could not switch to
    /// Bedrock because `bedrock:anthropic` was not in the enum.
    #[test]
    fn set_provider_schema_enum_includes_family_specs() {
        let names = schema_enum();
        for required in ["bedrock:anthropic", "cerebras:qwen", "openrouter:deepseek"] {
            assert!(
                names.iter().any(|n| n == required),
                "{required:?} missing from schema: {names:?}"
            );
        }
    }

    /// Bare multi-family names are rejected by `parse_provider_tier_key`,
    /// so they must not appear in the schema — guards against re-introducing
    /// the gap that prompted this fix.
    #[test]
    fn set_provider_schema_enum_excludes_bare_multifamily_providers() {
        let names = schema_enum();
        for forbidden in ["bedrock", "cerebras", "openrouter"] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "{forbidden:?} unexpectedly present in schema: {names:?}"
            );
        }
    }

    #[test]
    fn set_provider_schema_enum_includes_single_family_providers() {
        let names = schema_enum();
        for required in ["anthropic", "gemini", "grok", "ollama", "vllm", "zai"] {
            assert!(
                names.iter().any(|n| n == required),
                "{required:?} missing from schema: {names:?}"
            );
        }
    }

    // ─── Admin-gate adversarial coverage ─────────────────────────────────────

    /// Build a minimal `LlmToolContext`. Admin gating for the
    /// credential-mutating tools is now enforced pre-dispatch by the
    /// `HubCapabilityGate`, not inside `execute()`, so this context carries no
    /// admin policy of its own.
    fn test_llm_ctx() -> LlmToolContext {
        use crate::resolved_config::{ConfigCell, ResolvedConfig};
        use workforce_common::session::InMemorySessionStore;
        use workforce_common::settings::WorkforceConfig;

        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("vault.enc"), "test-key").unwrap());
        std::mem::forget(dir);

        let resolved = ConfigCell::new(ResolvedConfig::resolve(
            None,
            None,
            &WorkforceConfig::default(),
            "anthropic",
            None,
        ));

        let tier_key = Arc::new(RwLock::new("anthropic".to_string()));
        LlmToolContext {
            global_vault: None,
            pool: Arc::new(PooledProvider::new()),
            active_model: Arc::new(RwLock::new(None)),
            active_max_tokens: Arc::new(RwLock::new(None)),
            active_max_rounds: Arc::new(RwLock::new(None)),
            provider_kind: Arc::new(RwLock::new(ProviderKind::Anthropic)),
            admission: crate::workspace_admission::WorkspaceAdmission::new(
                Arc::new(WorkforceConfig::default()),
                "test",
                Arc::clone(&tier_key),
            ),
            tier_key,
            llm_config: LlmConfig::default(),
            current_default_model: Arc::new(RwLock::new("test-model".to_string())),
            original_default_model: Arc::new(RwLock::new("test-model".to_string())),
            agent_id: AgentId::new("test"),
            cost_tracker: Arc::new(CostTracker::new()),
            failover_provider: None,
            sessions: Arc::new(InMemorySessionStore::new()),
            active_routing: Arc::new(RwLock::new(None)),
            resolved,
            config_equivalences: Arc::new(RwLock::new(None)),
            workspace: std::path::PathBuf::from("/tmp"),
            bedrock_catalog: workforce_agent::llm::provider::bedrock_catalog(),
            reload_image_tool: workforce_agent::tools::image_gen::noop_reload_image_tool(),
            providers: Arc::new(crate::agent_llm::AgentProviders::new(
                Some(Arc::clone(&vault)),
                None,
                crate::subscription::PlanLogins::new(
                    crate::subscription::Subscriptions::new(&Default::default()),
                    crate::subscription::CredentialScope::system(
                        Arc::clone(&vault),
                        &Default::default(),
                    ),
                ),
            )),
            vault,
        }
    }

    #[tokio::test]
    async fn set_provider_refused_by_admission_leaves_config_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let body = "[llm]\nfailover = [\"gpt-6-sol\"]\n";
        std::fs::write(dir.path().join("config.toml"), body).unwrap();
        let ctx = LlmToolContext {
            workspace: dir.path().to_path_buf(),
            ..test_llm_ctx()
        };

        let result = SystemSetProviderTool::new(ctx.clone())
            .execute(serde_json::json!({"provider": "codex"}))
            .await
            .unwrap();
        assert!(!result.success, "{result:?}");
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("codex") && error.contains("remote"),
            "codex runs only on the remote worker, which this install lacks: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            body
        );
        assert_eq!(ctx.provider_kind(), ProviderKind::Anthropic);
        assert_eq!(ctx.tier_key(), "anthropic");

        let model = SystemSetModelTool::new(ctx.clone());
        let refused = model
            .execute(serde_json::json!({"model": "codex:gpt-6-astra"}))
            .await
            .unwrap();
        assert!(!refused.success);
        let error = refused.error.unwrap_or_default();
        assert!(
            error.contains("codex") && error.contains("remote"),
            "refused before any test call: {error}"
        );
        assert_eq!(*ctx.active_model.read(), None);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn set_provider_switches_turns_to_claude_code() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LlmToolContext {
            workspace: dir.path().to_path_buf(),
            ..test_llm_ctx()
        };
        ctx.pool
            .add(
                "default",
                Arc::new(crate::hub_core::logins::test_support::Metered::default()),
            )
            .await;

        let result = SystemSetProviderTool::new(ctx.clone())
            .execute(serde_json::json!({"provider": "claude-code"}))
            .await
            .unwrap();

        assert!(result.success, "{result:?}");
        assert_eq!(ctx.provider_kind(), ProviderKind::ClaudeCode);
        assert_eq!(ctx.tier_key(), "claude-code");
        let persisted = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(persisted.contains("claude-code"), "{persisted}");
    }

    #[tokio::test]
    async fn set_model_on_a_cli_route_checks_its_tier_models_without_a_call() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = LlmToolContext {
            workspace: dir.path().to_path_buf(),
            ..test_llm_ctx()
        };
        *ctx.provider_kind.write() = ProviderKind::ClaudeCode;
        *ctx.tier_key.write() = "claude-code".into();
        let probed = Arc::new(crate::hub_core::logins::test_support::Metered::default());
        ctx.pool
            .add("default", Arc::clone(&probed) as Arc<dyn LlmProvider>)
            .await;
        let set = |model: &'static str| {
            let tool = SystemSetModelTool::new(ctx.clone());
            async move {
                tool.execute(serde_json::json!({ "model": model }))
                    .await
                    .unwrap()
            }
        };

        let refused = set("gpt-4o").await;
        let accepted = set("sonnet").await;

        assert!(!refused.success, "{refused:?}");
        assert!(accepted.success, "{accepted:?}");
        assert!(
            probed.0.lock().is_empty(),
            "no CLI completion runs to check a model"
        );
    }

    #[tokio::test]
    async fn list_models_lists_tier_models_for_cli_providers() {
        let listed = |kind: ProviderKind| async move {
            let ctx = test_llm_ctx();
            *ctx.provider_kind.write() = kind;
            let result = SystemListModelsTool::new(ctx)
                .execute(serde_json::json!({}))
                .await
                .unwrap();
            assert!(result.success, "{result:?}");
            result.output["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|model| model["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            listed(ProviderKind::ClaudeCode).await,
            ["opus", "sonnet", "haiku"]
        );
        assert_eq!(
            listed(ProviderKind::Codex).await,
            ["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"]
        );
        assert_eq!(
            listed(ProviderKind::ChatGpt).await,
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna"
            ]
        );
    }

    #[tokio::test]
    async fn subscription_kind_accounts_refused() {
        let ctx = test_llm_ctx();
        let account = SystemAddAccountTool::new(ctx.clone())
            .execute(serde_json::json!({
                "name": "plan",
                "provider": "chatgpt",
                "api_key": "eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl"
            }))
            .await
            .unwrap();
        assert!(!account.success);
        assert!(
            account
                .error
                .as_deref()
                .is_some_and(|e| e.contains("logins aren't accounts")),
            "{:?}",
            account.error
        );
        assert!(ctx.vault.get("llm/accounts/plan").is_none());

        *ctx.provider_kind.write() = ProviderKind::ChatGpt;
        let token = SystemAddLlmTokenTool::new(ctx.clone())
            .execute(serde_json::json!({"name": "backup", "token": "sk-ant-oat01-abc"}))
            .await
            .unwrap();
        assert!(!token.success);
        assert!(
            token
                .error
                .as_deref()
                .is_some_and(|e| e.contains("logins aren't accounts")),
            "{:?}",
            token.error
        );
        assert!(ctx.vault.get("llm/tokens/backup").is_none());
        assert!(ctx.pool.is_empty().await);
    }

    #[tokio::test]
    async fn add_llm_token_refuses_oat_with_pointer() {
        let ctx = test_llm_ctx();
        let oat = concat!("sk-ant", "-oat01-abcdefghijklmnopqrstuvwxyz");
        let token = SystemAddLlmTokenTool::new(ctx.clone())
            .execute(serde_json::json!({"name": "backup", "token": oat}))
            .await
            .unwrap();
        assert!(!token.success);
        assert!(
            token
                .error
                .as_deref()
                .is_some_and(|e| e.contains(REMOVED_PROVIDER_POINTER)),
            "{:?}",
            token.error
        );
        assert!(ctx.vault.get("llm/tokens/backup").is_none());
        assert!(ctx.pool.is_empty().await);

        for (provider, api_key) in [("anthropic", oat), ("claude-max", "sk-ant-api03-abc")] {
            let account = SystemAddAccountTool::new(ctx.clone())
                .execute(serde_json::json!({
                    "name": "plan",
                    "provider": provider,
                    "api_key": api_key
                }))
                .await
                .unwrap();
            assert!(!account.success);
            assert!(
                account
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains(REMOVED_PROVIDER_POINTER)),
                "{provider}: {:?}",
                account.error
            );
        }
        assert!(ctx.vault.get("llm/accounts/plan").is_none());
    }

    // Admin gating for `system.{add,remove}_account` and
    // `system.{add,remove}_llm_token` is now enforced pre-dispatch by the
    // `HubCapabilityGate` (`→ VaultSharedWrite`) off the `tool_capability`
    // registry, not by an inline check here. The member-denied / admin-allowed
    // adversarial coverage lives in `crate::authz` and `crate::cap_gate`; the
    // read-only `system.list_accounts` tool stays unregistered (ungated), as
    // asserted below.

    #[tokio::test]
    async fn read_only_llm_tools_stay_ungated() {
        // `system.list_accounts` is not a registered admin capability, so the
        // gate never denies it — it executes for any caller.
        let tool = SystemListAccountsTool::new(test_llm_ctx());
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success, "read-only list must not be gated");
    }
}
