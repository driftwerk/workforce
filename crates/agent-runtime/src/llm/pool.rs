//! Token pool provider — wraps multiple LLM providers for token-level failover.
//!
//! Rate limits are account-level: all models on the same token are
//! rate-limited simultaneously. This provider maintains a pool
//! of providers (each backed by a different token/proxy) and rotates between them
//! on 429 errors.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use futures::future::join_all;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use super::failover::auth_is_definitive;
use super::failover::auth_is_model_scoped;
use super::failover::classify_error;
use super::failover::ErrorKind;
use super::failover::LlmTimeouts;
use super::provider::LlmProvider;
use super::streaming::{call_with_request_cap, watchdogged_stream, StreamEvent};
use super::tiers::{EquivalencesCell, TierRegistry};
use super::types::{LlmMessage, LlmResponse, LlmToolDef};
use workforce_common::session::estimate_tokens;
use workforce_common::text::truncate_str;

/// Maximum pool cooldown (5 minutes). The FailoverProvider handles longer outages
/// by switching models — the pool only needs to avoid immediate re-hits.
const MAX_COOLDOWN_SECS: u64 = 300;

/// Cap on stored cooldown errors so a pathological provider body can't
/// bloat pool state or the cached-skip message it is replayed into.
const COOLDOWN_ERROR_MAX_BYTES: usize = 240;

/// Marker embedded in the cached-skip error for an auth-armed cooldown.
/// `classify_error` keys on it, so the replay classifies `Auth` regardless
/// of what status the wrapped arming error carries.
pub const POOL_AUTH_SKIP_MARKER: &str = "entry auth-failed (401/403, cached)";

/// Marker embedded in the cached-skip error for a rate-limit cooldown;
/// keeps the replay classifying `RateLimited`.
pub const POOL_RATE_LIMIT_SKIP_MARKER: &str = "entry rate-limited (429, cached)";

/// Marker embedded in the cached-skip error for a model-scoped auth
/// cooldown; classifies `Auth` so the FailoverProvider switches to the
/// next model in its chain — which is exactly what a model-access denial
/// needs, since the credential itself still works.
pub const POOL_MODEL_AUTH_SKIP_MARKER: &str = "model auth-failed (401/403, cached)";

// ─── Cooldown Reason ─────────────────────────────────────────────────────────

/// Why a pool entry was placed on cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownReason {
    /// API returned 429 — token is rate-limited. Will recover after the
    /// retry-after window (could be minutes, hours, or days).
    RateLimited,
    /// API returned 401/403 — token is invalid or expired.
    /// Will NOT recover on its own; needs a fresh token.
    AuthFailed,
}

// ─── Pool Entry ──────────────────────────────────────────────────────────────

/// One armed cooldown — entry-wide (credential) or scoped to a model.
#[derive(Debug, Clone)]
struct Cooldown {
    until: Instant,
    /// The original retry-after value from the API (before capping).
    original_retry_after_secs: u64,
    reason: CooldownReason,
    /// The (truncated) error that armed the cooldown — replayed in the
    /// cached-skip error so callers see the actual failure, not just that
    /// the entry is skipped.
    error: String,
}

impl Cooldown {
    /// Arm from a provider error: honour an embedded retry-after hint,
    /// cap to [`MAX_COOLDOWN_SECS`], truncate the stored error.
    fn from_error(error_msg: &str, reason: CooldownReason, default_secs: u64) -> Self {
        let raw = parse_retry_after(error_msg).unwrap_or(default_secs);
        if raw > MAX_COOLDOWN_SECS {
            info!(
                raw_retry_after = raw,
                capped_to = MAX_COOLDOWN_SECS,
                "Capped pool cooldown (retry-after exceeds max)"
            );
        }
        Self {
            until: Instant::now() + Duration::from_secs(raw.min(MAX_COOLDOWN_SECS)),
            original_retry_after_secs: raw,
            reason,
            error: truncate_str(error_msg, COOLDOWN_ERROR_MAX_BYTES).to_string(),
        }
    }

    fn active(&self, now: Instant) -> bool {
        now < self.until
    }

    fn remaining_secs(&self, now: Instant) -> u64 {
        self.until.saturating_duration_since(now).as_secs()
    }

    fn label(reason: CooldownReason) -> &'static str {
        match reason {
            CooldownReason::RateLimited => "rate-limited",
            CooldownReason::AuthFailed => "auth-failed",
        }
    }
}

struct PoolEntry {
    name: String,
    provider: Arc<dyn LlmProvider>,
    /// Entry-wide cooldown — the credential itself is limited or dead.
    cooldown: Option<Cooldown>,
    /// Cooldowns keyed by effective model id — the credential works but
    /// the named model is denied (e.g. no marketplace subscription).
    /// Expired keys are pruned on each arm.
    model_cooldowns: HashMap<String, Cooldown>,
    /// Rolling window of admitted token reservations for proactive TPM
    /// pacing: `(admission instant, estimated tokens)`. Pruned past
    /// [`PACING_WINDOW`]; reservations for paced requests are stamped at
    /// their future send time so concurrent admissions see them.
    usage_window: VecDeque<(Instant, u32)>,
}

impl PoolEntry {
    fn entry_cooldown(&self, now: Instant) -> Option<&Cooldown> {
        self.cooldown.as_ref().filter(|c| c.active(now))
    }

    fn model_cooldown(&self, model: &str, now: Instant) -> Option<&Cooldown> {
        self.model_cooldowns.get(model).filter(|c| c.active(now))
    }

    /// The cooldown blocking `model` on this entry, entry-wide first — a
    /// dead credential is the more actionable report than a denied model.
    fn blocked_for<'a>(
        &'a self,
        model: &'a str,
        now: Instant,
    ) -> Option<(&'a Cooldown, Option<&'a str>)> {
        self.entry_cooldown(now)
            .map(|c| (c, None))
            .or_else(|| self.model_cooldown(model, now).map(|c| (c, Some(model))))
    }
}

/// Public-facing status of a pool entry.
#[derive(Debug, Clone)]
pub struct PoolEntryStatus {
    pub name: String,
    pub is_rate_limited: bool,
    pub rate_limited_secs_remaining: Option<u64>,
    /// The original retry-after from the API (before pool capping).
    /// Useful for agents to report the real rate-limit window to users.
    pub original_retry_after_secs: Option<u64>,
    /// Why this entry is on cooldown (`None` if available).
    pub cooldown_reason: Option<CooldownReason>,
    /// The (truncated) error that armed the cooldown (`None` if available).
    pub cooldown_error: Option<String>,
    /// Cooldowns scoped to individual models on this entry (unexpired
    /// only). The entry itself stays available for other models.
    pub model_cooldowns: Vec<ModelCooldownStatus>,
}

/// Public-facing status of one model-scoped cooldown.
#[derive(Debug, Clone)]
pub struct ModelCooldownStatus {
    pub model: String,
    pub secs_remaining: u64,
    pub reason: CooldownReason,
    pub error: String,
}

// ─── Pooled Provider ─────────────────────────────────────────────────────────

/// A provider that maintains a pool of underlying providers and rotates
/// between them when one hits a rate limit.
///
/// For single-token setups (pool size = 1), this is a transparent passthrough
/// with minimal overhead.
pub struct PooledProvider {
    entries: RwLock<Vec<PoolEntry>>,
    /// How long to mark a provider as rate-limited after a 429 (seconds).
    cooldown_secs: u64,
    /// Tier-name resolver wired in by the hub. The pool is the final
    /// chokepoint before a model id leaves for the wire, so resolving
    /// here is the defence-in-depth against every upstream bypass —
    /// vault loads, admin HTTP writes, workflow step overrides, future
    /// code paths we haven't audited.
    tier_resolver: Option<TierResolver>,
    /// Proactive TPM pacing; `None` = gate disabled. Behind a lock so
    /// `AgentService::reload_config` can hot-swap a per-agent `[llm.pacing]`
    /// edit into the live pool without a hub restart (the pool is held via
    /// `Arc`, so the policy must be interior-mutable like `tier_key`).
    pacing: parking_lot::RwLock<Option<PacingPolicy>>,
    /// Liveness watchdog windows. The pool sits in every provider stack —
    /// including installs with no failover chain — so enforcing here means
    /// a hung provider always produces an error instead of an eternal
    /// silent turn. Interior-mutable for the same hot-reload reason as
    /// `pacing`.
    timeouts: parking_lot::RwLock<LlmTimeouts>,
}

/// Cheap-to-clone handle the hub hands to [`PooledProvider`] so it can
/// look up the current tier key and tier registry without rebuilding
/// state on every call. `tier_key` is `Arc<RwLock<String>>` because
/// `system.set_provider` flips it at runtime.
#[derive(Clone)]
pub struct TierResolver {
    pub tier_key: Arc<parking_lot::RwLock<String>>,
    pub config_equivalences: EquivalencesCell,
}

/// Rolling-window span TPM quotas are measured over.
const PACING_WINDOW: Duration = Duration::from_secs(60);

/// Proactive TPM pacing policy, resolved from `[llm.pacing]`.
#[derive(Debug, Clone)]
pub struct PacingPolicy {
    pub effective_limit: u32,
    pub max_wait: Duration,
    pub output_allowance: u32,
}

impl PacingPolicy {
    /// `None` when pacing is disabled — the pool gate becomes a no-op.
    pub fn from_config(cfg: &workforce_common::settings::PacingConfig) -> Option<Self> {
        cfg.enabled.then(|| Self {
            effective_limit: (f64::from(cfg.tpm_limit) * cfg.headroom.clamp(0.1, 1.0)) as u32,
            max_wait: Duration::from_secs(cfg.max_wait_secs),
            output_allowance: cfg.output_allowance,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaceDecision {
    Proceed,
    Wait(Duration),
    Decline(Duration),
}

/// Admission decision for `estimate` tokens against a pruned rolling window.
///
/// A single request bigger than the whole budget is admitted — no wait can
/// ever fit it, and declining would trap it in permanent failover; the
/// reactive 429 retry is the backstop. Otherwise: fits now → `Proceed`;
/// fits once enough of the window ages out within `max_wait` → `Wait`
/// (exact shortfall, not the provider's punitive full-window reset);
/// longer → `Decline` carrying the projected wait.
fn pacing_decision(
    window: &VecDeque<(Instant, u32)>,
    now: Instant,
    estimate: u32,
    limit: u32,
    max_wait: Duration,
) -> PaceDecision {
    if estimate >= limit {
        return PaceDecision::Proceed;
    }
    let used: u64 = window.iter().map(|(_, amount)| u64::from(*amount)).sum();
    if used + u64::from(estimate) <= u64::from(limit) {
        return PaceDecision::Proceed;
    }
    let mut freed: u64 = 0;
    for (admitted_at, amount) in window {
        freed += u64::from(*amount);
        if used - freed + u64::from(estimate) <= u64::from(limit) {
            let wait = (*admitted_at + PACING_WINDOW).saturating_duration_since(now);
            return if wait <= max_wait {
                PaceDecision::Wait(wait)
            } else {
                PaceDecision::Decline(wait)
            };
        }
    }
    PaceDecision::Decline(PACING_WINDOW)
}

/// Drop window entries older than [`PACING_WINDOW`]. Future-stamped
/// reservations (paced requests) sit at the back and survive.
fn prune_window(window: &mut VecDeque<(Instant, u32)>, now: Instant) {
    while let Some((admitted_at, _)) = window.front() {
        if now.saturating_duration_since(*admitted_at) >= PACING_WINDOW {
            window.pop_front();
        } else {
            break;
        }
    }
}

/// Estimate the tokens a request will charge against the provider quota:
/// full prompt (cached tokens count toward TPM) + the declared tools + a
/// completion allowance.
fn estimate_request_tokens(
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    output_allowance: u32,
) -> u32 {
    let messages_json = serde_json::to_string(messages).unwrap_or_default();
    estimate_tokens(system_prompt.unwrap_or(""))
        .saturating_add(estimate_tokens(&messages_json))
        .saturating_add(
            tools
                .iter()
                .map(crate::tools::loading::definition_tokens)
                .sum::<u32>(),
        )
        .saturating_add(output_allowance)
}

impl Default for PooledProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PooledProvider {
    /// Create a new pool with default cooldown (60 seconds).
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            cooldown_secs: 60,
            tier_resolver: None,
            pacing: parking_lot::RwLock::new(None),
            timeouts: parking_lot::RwLock::new(LlmTimeouts::default()),
        }
    }

    /// Create a new pool with a custom cooldown.
    pub fn with_cooldown(cooldown_secs: u64) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            cooldown_secs,
            tier_resolver: None,
            pacing: parking_lot::RwLock::new(None),
            timeouts: parking_lot::RwLock::new(LlmTimeouts::default()),
        }
    }

    /// Attach a tier resolver. After this, every model id passed to
    /// `create_message` / `create_message_stream` is run through
    /// [`TierRegistry::resolve_model`] using the current tier key before
    /// reaching the underlying provider. Tier names that slipped past
    /// every other defence still self-correct here.
    pub fn with_tier_resolver(mut self, resolver: TierResolver) -> Self {
        self.tier_resolver = Some(resolver);
        self
    }

    /// Attach a proactive TPM pacing policy; `None` leaves the gate off.
    pub fn with_pacing(mut self, pacing: Option<PacingPolicy>) -> Self {
        self.pacing = parking_lot::RwLock::new(pacing);
        self
    }

    /// Hot-swap the pacing policy on a live pool (held via `Arc`). `None`
    /// disables the gate. Called by `AgentService::reload_config` so a
    /// per-agent `[llm.pacing]` edit takes effect without a restart.
    pub fn set_pacing(&self, pacing: Option<PacingPolicy>) {
        *self.pacing.write() = pacing;
    }

    /// Hot-swap the liveness watchdog windows on a live pool (held via
    /// `Arc`). Called at wiring time and by `AgentService::reload_config`
    /// so a `[llm.retry]` timeout edit takes effect without a restart.
    pub fn set_llm_timeouts(&self, timeouts: LlmTimeouts) {
        *self.timeouts.write() = timeouts;
    }

    /// Admit a request of `estimate` tokens against the entry's rolling
    /// window. `Proceed`/`Wait` record the reservation (paced requests are
    /// stamped at their future send time so concurrent admissions see
    /// them); `Decline` puts the entry on cooldown for the projected wait
    /// so other callers skip it from cache.
    async fn pace_admit(&self, index: usize, estimate: u32) -> PaceDecision {
        let policy = self.pacing.read().clone();
        let Some(policy) = policy else {
            return PaceDecision::Proceed;
        };
        let now = Instant::now();
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get_mut(index) else {
            return PaceDecision::Proceed;
        };
        prune_window(&mut entry.usage_window, now);
        let decision = pacing_decision(
            &entry.usage_window,
            now,
            estimate,
            policy.effective_limit,
            policy.max_wait,
        );
        match decision {
            PaceDecision::Proceed => entry.usage_window.push_back((now, estimate)),
            PaceDecision::Wait(wait) => entry.usage_window.push_back((now + wait, estimate)),
            PaceDecision::Decline(wait) => {
                let capped = wait.min(Duration::from_secs(MAX_COOLDOWN_SECS));
                entry.cooldown = Some(Cooldown {
                    until: now + capped,
                    original_retry_after_secs: wait.as_secs(),
                    reason: CooldownReason::RateLimited,
                    error: format!(
                        "proactive TPM pacing declined the request (projected wait {}s exceeds cap)",
                        wait.as_secs()
                    ),
                });
                warn!(
                    name = entry.name.as_str(),
                    estimate,
                    projected_wait_secs = wait.as_secs(),
                    "Pacing declined request — projected TPM wait exceeds cap"
                );
            }
        }
        if let PaceDecision::Wait(wait) = decision {
            info!(
                name = entry.name.as_str(),
                estimate,
                wait_ms = wait.as_millis() as u64,
                "Pacing request to stay under provider TPM"
            );
        }
        decision
    }

    fn normalize_model(&self, model: Option<&str>) -> Option<String> {
        let Some(raw) = model else {
            tracing::info!(
                resolver_attached = self.tier_resolver.is_some(),
                "pool.normalize_model: no model arg"
            );
            return None;
        };
        let Some(resolver) = self.tier_resolver.as_ref() else {
            tracing::info!(
                raw = %raw,
                "pool.normalize_model: no tier resolver attached — passthrough"
            );
            return None;
        };
        let tier_key = resolver.tier_key.read().clone();
        let tiers = TierRegistry::from_cell(&resolver.config_equivalences);
        let resolved = tiers.resolve_model(raw, &tier_key);
        tracing::info!(
            raw = %raw,
            resolved = %resolved,
            tier_key = %tier_key,
            rewritten = resolved != raw,
            "pool.normalize_model: ran"
        );
        if resolved != raw {
            warn!(
                stored = %raw,
                resolved = %resolved,
                tier_key = %tier_key,
                "Tier name reached PooledProvider; resolved before forwarding"
            );
        }
        Some(resolved)
    }

    /// Add a provider to the pool.
    pub async fn add(&self, name: &str, provider: Arc<dyn LlmProvider>) {
        let mut entries = self.entries.write().await;
        // Remove existing entry with same name (replacement)
        entries.retain(|e| e.name != name);
        entries.push(PoolEntry {
            name: name.to_string(),
            provider,
            cooldown: None,
            model_cooldowns: HashMap::new(),
            usage_window: VecDeque::new(),
        });
        info!(
            name = name,
            pool_size = entries.len(),
            "Provider added to pool"
        );
    }

    /// Remove a provider from the pool by name. Returns true if found.
    pub async fn remove(&self, name: &str) -> bool {
        let mut entries = self.entries.write().await;
        let before = entries.len();
        entries.retain(|e| e.name != name);
        let removed = entries.len() < before;
        if removed {
            info!(
                name = name,
                pool_size = entries.len(),
                "Provider removed from pool"
            );
        }
        removed
    }

    /// Get status of all pool entries.
    pub async fn list(&self) -> Vec<PoolEntryStatus> {
        let entries = self.entries.read().await;
        let now = Instant::now();
        entries
            .iter()
            .map(|e| {
                let active = e.entry_cooldown(now);
                PoolEntryStatus {
                    name: e.name.clone(),
                    is_rate_limited: active.is_some(),
                    rate_limited_secs_remaining: active.map(|c| c.remaining_secs(now)),
                    original_retry_after_secs: active.map(|c| c.original_retry_after_secs),
                    cooldown_reason: active.map(|c| c.reason),
                    cooldown_error: active.map(|c| c.error.clone()),
                    model_cooldowns: e
                        .model_cooldowns
                        .iter()
                        .filter(|(_, c)| c.active(now))
                        .map(|(model, c)| ModelCooldownStatus {
                            model: model.clone(),
                            secs_remaining: c.remaining_secs(now),
                            reason: c.reason,
                            error: c.error.clone(),
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// Get the number of providers in the pool.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Check if the pool is empty.
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// Check if a named entry exists in the pool.
    pub async fn has(&self, name: &str) -> bool {
        self.entries.read().await.iter().any(|e| e.name == name)
    }

    /// Verify credentials on every entry in parallel. Any entry whose probe
    /// returns an auth-failed error is immediately moved to cooldown using
    /// the existing rate-limit machinery, so the first real request doesn't
    /// burn round-trip time on a doomed token. Non-auth probe errors are
    /// logged but left to the normal request-time failure path — they may be
    /// transient. Quietly noops on an empty pool.
    pub async fn probe_all_entries(&self) {
        let snapshot: Vec<(usize, String, Arc<dyn LlmProvider>)> = {
            let entries = self.entries.read().await;
            entries
                .iter()
                .enumerate()
                .map(|(idx, e)| (idx, e.name.clone(), Arc::clone(&e.provider)))
                .collect()
        };
        if snapshot.is_empty() {
            return;
        }
        let results = join_all(
            snapshot
                .into_iter()
                .map(|(idx, name, provider)| async move {
                    (idx, name, provider.verify_credentials().await)
                }),
        )
        .await;
        for (idx, name, result) in results {
            match result {
                Ok(()) => {
                    info!(name = name.as_str(), "Pool entry credentials probe ok");
                }
                Err(e) => {
                    let err_str = e.to_string();
                    match classify_error(&err_str) {
                        ErrorKind::Auth if !auth_is_model_scoped(&err_str) => {
                            warn!(
                                name = name.as_str(),
                                error = %e,
                                "Pool entry credential probe failed (auth) — marking cooldown"
                            );
                            self.arm_cooldown(idx, None, &err_str, CooldownReason::AuthFailed)
                                .await;
                        }
                        // A model-access denial says nothing about the
                        // credential, and the probe has no model key to
                        // scope a cooldown to — leave the entry admitted
                        // and let the request path arm per model.
                        ErrorKind::Auth => {
                            warn!(
                                name = name.as_str(),
                                error = %e,
                                "Pool entry credential probe failed (model-scoped auth) — leaving admitted"
                            );
                        }
                        _ => {
                            warn!(
                                name = name.as_str(),
                                error = %e,
                                "Pool entry credential probe failed (non-auth) — leaving admitted"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Absorb a failed provider call: rate limits and definitive auth
    /// failures arm a cooldown; client errors and heuristic
    /// (non-definitive) auth classifications just move on to the next entry
    /// — one wrongly-guessed auth error must not poison every model behind
    /// the credential. A definitive auth failure whose body names the model
    /// as the denied resource cools down only `(entry, model)`, so other
    /// models keep flowing through a healthy credential. Returns false when
    /// the error must propagate for the FailoverProvider to handle model
    /// switching.
    async fn absorb_failure(&self, index: usize, model: &str, error_str: &str) -> bool {
        match classify_error(error_str) {
            ErrorKind::RateLimited => {
                self.arm_cooldown(index, None, error_str, CooldownReason::RateLimited)
                    .await;
                true
            }
            ErrorKind::Auth if auth_is_definitive(error_str) => {
                let scope = (auth_is_model_scoped(error_str) && !model.is_empty()).then_some(model);
                self.arm_cooldown(index, scope, error_str, CooldownReason::AuthFailed)
                    .await;
                true
            }
            ErrorKind::Auth | ErrorKind::ClientError => true,
            _ => false,
        }
    }

    /// Arm a cooldown from a provider error — scoped to `model` when given,
    /// entry-wide otherwise. Retry-after hints and the `MAX_COOLDOWN_SECS`
    /// cap are honoured by [`Cooldown::from_error`]; expired model keys are
    /// pruned while the write lock is held.
    async fn arm_cooldown(
        &self,
        index: usize,
        model: Option<&str>,
        error_msg: &str,
        reason: CooldownReason,
    ) {
        let cooldown = Cooldown::from_error(error_msg, reason, self.cooldown_secs);
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get_mut(index) else {
            return;
        };
        let now = Instant::now();
        entry.model_cooldowns.retain(|_, c| c.active(now));
        warn!(
            name = entry.name.as_str(),
            model = model.unwrap_or(""),
            cooldown_secs = cooldown.original_retry_after_secs.min(MAX_COOLDOWN_SECS),
            reason = Cooldown::label(reason),
            "Provider marked as cooldown"
        );
        match model {
            Some(m) => {
                entry.model_cooldowns.insert(m.to_string(), cooldown);
            }
            None => entry.cooldown = Some(cooldown),
        }
    }

    /// Build the cached-skip error for `model` on the entry at `index`, or
    /// `None` when neither the entry nor the model is on cooldown.
    /// Entry-wide wins ([`PoolEntry::blocked_for`]). The message replays
    /// the error that armed the cooldown behind a reason marker that
    /// `classify_error` checks before anything in the wrapped text, so the
    /// replay always classifies as the cooldown reason: the auth markers
    /// fail over immediately, and the rate-limit marker carries a fresh
    /// `[retry-after: Ns]` hint (remaining cooldown, rounded up) that the
    /// FailoverProvider's gated retry sleeps on. The fresh hint precedes
    /// the replayed error because `parse_retry_after` takes the first
    /// match — arming 429s usually embed the provider's own hint, which is
    /// stale by now and may be mangled by truncation.
    async fn cooldown_skip_error(&self, index: usize, model: &str) -> Option<eyre::Error> {
        let entries = self.entries.read().await;
        let now = Instant::now();
        let (cooldown, scope) = entries.get(index)?.blocked_for(model, now)?;
        let last = &cooldown.error;
        Some(match (cooldown.reason, scope) {
            (CooldownReason::AuthFailed, Some(m)) => {
                eyre::eyre!("LLM pool: {POOL_MODEL_AUTH_SKIP_MARKER} for '{m}': {last}")
            }
            (CooldownReason::AuthFailed, None) => {
                eyre::eyre!("LLM pool: {POOL_AUTH_SKIP_MARKER}: {last}")
            }
            (CooldownReason::RateLimited, _) => {
                let remaining = cooldown.remaining_secs(now) + 1;
                eyre::eyre!(
                    "LLM pool: {POOL_RATE_LIMIT_SKIP_MARKER} [retry-after: {remaining}s]: {last}"
                )
            }
        })
    }

    /// A success on `model` proves the credential works: clear the
    /// entry-wide cooldown and the model's own. Other models' cooldowns
    /// stand — a healthy Sonnet must not resurrect a denied Opus.
    async fn clear_cooldowns(&self, index: usize, model: &str) {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(index) {
            entry.cooldown = None;
            entry.model_cooldowns.remove(model);
        }
    }

    /// Get a clone of the provider Arc at a given index.
    async fn get_provider(&self, index: usize) -> Option<(String, Arc<dyn LlmProvider>)> {
        let entries = self.entries.read().await;
        entries
            .get(index)
            .map(|e| (e.name.clone(), Arc::clone(&e.provider)))
    }

    /// Get all provider indices to try for `model`, starting from the best
    /// pick: entries not blocked for this model first, then index order.
    async fn try_order(&self, model: &str) -> Vec<usize> {
        let entries = self.entries.read().await;
        let now = Instant::now();
        let mut indices: Vec<usize> = (0..entries.len()).collect();
        indices.sort_by_key(|&i| (entries[i].blocked_for(model, now).is_some(), i));
        indices
    }
}

#[async_trait]
impl LlmProvider for PooledProvider {
    /// The smallest window among the pool's providers: any of them may take
    /// the request.
    async fn context_floor(&self, model: &str) -> Option<u32> {
        let providers: Vec<Arc<dyn LlmProvider>> = self
            .entries
            .read()
            .await
            .iter()
            .map(|entry| Arc::clone(&entry.provider))
            .collect();
        if providers.is_empty() {
            return super::model_limits::context_for(model);
        }
        futures::future::join_all(providers.iter().map(|p| p.context_floor(model)))
            .await
            .into_iter()
            .flatten()
            .min()
    }

    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let normalized = self.normalize_model(model);
        let effective_model = normalized.as_deref().or(model);
        let model_key = effective_model.unwrap_or("");
        let order = self.try_order(model_key).await;
        if order.is_empty() {
            return Err(eyre::eyre!("No providers in pool"));
        }

        let estimate =
            self.pacing.read().as_ref().map(|p| {
                estimate_request_tokens(system_prompt, messages, tools, p.output_allowance)
            });
        let mut last_error: Option<eyre::Error> = None;

        for &idx in &order {
            // Skip entries already on cooldown — avoids wasting ~2s on a
            // doomed API call. The FailoverProvider will move to the next
            // model in the chain.
            if let Some(skip) = self.cooldown_skip_error(idx, model_key).await {
                last_error = Some(skip);
                continue;
            }

            if let Some(estimate) = estimate {
                match self.pace_admit(idx, estimate).await {
                    PaceDecision::Proceed => {}
                    PaceDecision::Wait(wait) => tokio::time::sleep(wait).await,
                    PaceDecision::Decline(wait) => {
                        last_error = Some(eyre::eyre!(
                            "pool pacing: projected TPM rate limit wait exceeds cap [retry-after: {}s]",
                            wait.as_secs().max(1)
                        ));
                        continue;
                    }
                }
            }

            let Some((_name, provider)) = self.get_provider(idx).await else {
                continue;
            };

            // Bind before awaiting — the lock guard must not live that long.
            let request_timeout = self.timeouts.read().request_timeout_for(model_key);
            let call = provider.create_message(
                system_prompt,
                messages,
                tools,
                effective_model,
                max_tokens,
                enable_cache,
            );
            match call_with_request_cap(request_timeout, model_key, call).await {
                Ok(response) => {
                    self.clear_cooldowns(idx, model_key).await;
                    return Ok(response);
                }
                Err(e) => {
                    if self.absorb_failure(idx, model_key, &format!("{e}")).await {
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| eyre::eyre!("All providers in pool are exhausted")))
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let normalized = self.normalize_model(model);
        let effective_model = normalized.as_deref().or(model);
        let model_key = effective_model.unwrap_or("");
        // Iterate all tokens in the pool, same as create_message().
        // 429 errors happen at the HTTP level before any streaming response,
        // so rotating to the next token is safe — no partial events have been sent.
        let order = self.try_order(model_key).await;
        if order.is_empty() {
            return Err(eyre::eyre!("No providers in pool"));
        }

        let estimate =
            self.pacing.read().as_ref().map(|p| {
                estimate_request_tokens(system_prompt, messages, tools, p.output_allowance)
            });
        let mut last_error: Option<eyre::Error> = None;

        for &idx in &order {
            // Skip entries already on cooldown — avoids wasting ~2s on a
            // doomed API call. The FailoverProvider will move to the next
            // model in the chain.
            if let Some(skip) = self.cooldown_skip_error(idx, model_key).await {
                last_error = Some(skip);
                continue;
            }

            if let Some(estimate) = estimate {
                match self.pace_admit(idx, estimate).await {
                    PaceDecision::Proceed => {}
                    PaceDecision::Wait(wait) => {
                        // attempt: 0 marks proactive pacing (vs reactive
                        // retry) — renderers show "pacing" copy for it.
                        let _ = tx
                            .send(StreamEvent::RateLimitWait {
                                wait_secs: wait.as_secs().max(1),
                                attempt: 0,
                            })
                            .await;
                        tokio::time::sleep(wait).await;
                    }
                    PaceDecision::Decline(wait) => {
                        last_error = Some(eyre::eyre!(
                            "pool pacing: projected TPM rate limit wait exceeds cap [retry-after: {}s]",
                            wait.as_secs().max(1)
                        ));
                        continue;
                    }
                }
            }

            let Some((_name, provider)) = self.get_provider(idx).await else {
                continue;
            };

            let limits = self.timeouts.read().stream_timeouts_for(model_key);
            let watched = watchdogged_stream(limits, model_key, tx.clone(), |inner_tx| {
                provider.create_message_stream(
                    system_prompt,
                    messages,
                    tools,
                    effective_model,
                    max_tokens,
                    enable_cache,
                    inner_tx,
                )
            });
            match watched.await {
                Ok(()) => {
                    self.clear_cooldowns(idx, model_key).await;
                    return Ok(());
                }
                Err(e) => {
                    if self.absorb_failure(idx, model_key, &format!("{e}")).await {
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        // All providers exhausted (rate-limited or auth-failed)
        Err(last_error.unwrap_or_else(|| eyre::eyre!("All providers in pool are exhausted")))
    }
}

/// Parse a retry-after hint from an error message. Accepts both delimiter
/// styles providers embed: `[retry-after: Ns]` (Anthropic, pool cached-skip)
/// and `(retry-after: Ns)` (Cerebras) — the bracket-only version silently
/// never matched Cerebras, so its pool cooldown always fell back to the
/// 60s default regardless of what the API asked for.
/// Returns `None` if the pattern is not found or cannot be parsed.
pub(crate) fn parse_retry_after(error: &str) -> Option<u64> {
    let start = error.find("retry-after: ")?;
    let rest = &error[start + 13..];
    let end = rest.find('s')?;
    rest[..end].parse().ok()
}

/// Allow `Arc<PooledProvider>` to be used directly as an `LlmProvider`.
///
/// This enables the pool to be shared via `Arc` (for system tools to add/remove
/// tokens at runtime) while also being used as the inner provider for
/// `FailoverProvider`.
#[async_trait]
impl LlmProvider for Arc<PooledProvider> {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        (**self)
            .create_message(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
            )
            .await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        (**self)
            .create_message_stream(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
                tx,
            )
            .await
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        (**self).context_floor(model).await
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ContentBlock, StopReason, TokenUsage};
    use std::collections::HashMap;

    /// Mock provider that always succeeds.
    struct MockOk {
        label: String,
    }

    #[async_trait]
    impl LlmProvider for MockOk {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.label.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                model: model.unwrap_or("default").to_string(),
            })
        }
    }

    #[tokio::test]
    async fn the_pool_floor_is_its_smallest_providers_window() {
        let pool = PooledProvider::new();
        assert_eq!(
            pool.context_floor("claude-sonnet-4-6").await,
            crate::llm::model_limits::context_for("claude-sonnet-4-6"),
            "an empty pool reports the model's own window"
        );
        pool.add(
            "key",
            Arc::new(MockOk {
                label: "key".into(),
            }),
        )
        .await;
        pool.add(
            "narrow",
            Arc::new(Narrow(MockOk {
                label: "narrow".into(),
            })),
        )
        .await;
        assert_eq!(pool.context_floor("claude-sonnet-4-6").await, Some(20_000));
    }

    /// A provider whose requests fit a window smaller than the model's own.
    struct Narrow(MockOk);

    #[async_trait]
    impl LlmProvider for Narrow {
        async fn create_message(
            &self,
            system_prompt: Option<&str>,
            messages: &[LlmMessage],
            tools: &[LlmToolDef],
            model: Option<&str>,
            max_tokens: Option<u32>,
            enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.0
                .create_message(
                    system_prompt,
                    messages,
                    tools,
                    model,
                    max_tokens,
                    enable_cache,
                )
                .await
        }

        async fn context_floor(&self, _model: &str) -> Option<u32> {
            Some(20_000)
        }
    }

    /// Mock provider that always returns 429.
    struct MockRateLimited;

    #[async_trait]
    impl LlmProvider for MockRateLimited {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!(
                "Anthropic API error (429): rate limit exceeded"
            ))
        }
    }

    /// Provider whose calls hang forever — the watchdog's prey.
    struct MockHang;

    #[async_trait]
    impl LlmProvider for MockHang {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            tokio::time::sleep(Duration::from_secs(100_000)).await;
            Err(eyre::eyre!("unreachable"))
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            tokio::time::sleep(Duration::from_secs(100_000)).await;
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pool_stream_first_token_timeout_returns_marker_error() {
        let pool = PooledProvider::new();
        pool.add("default", Arc::new(MockHang)).await;

        let (tx, _rx) = mpsc::channel(8);
        let err = pool
            .create_message_stream(None, &[], &[], Some("m"), None, true, tx)
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains(super::super::streaming::FIRST_TOKEN_TIMEOUT_MARKER));
    }

    #[tokio::test(start_paused = true)]
    async fn pool_request_timeout_caps_non_streaming_call() {
        let pool = PooledProvider::new();
        pool.add("default", Arc::new(MockHang)).await;

        let err = pool
            .create_message(None, &[], &[], Some("m"), None, true)
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains(super::super::streaming::FIRST_TOKEN_TIMEOUT_MARKER));
    }

    #[tokio::test]
    async fn single_provider_passthrough() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "hello".to_string(),
            }),
        )
        .await;

        let result = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("hello".to_string()));
    }

    #[tokio::test]
    async fn empty_pool_returns_error() {
        let pool = PooledProvider::new();
        let result = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await;
        assert!(result.is_err());
    }

    /// `set_pacing` hot-swaps the live policy (used by config hot-reload) so a
    /// per-agent `[llm.pacing]` edit applies without rebuilding the pool.
    #[test]
    fn set_pacing_hot_swaps_live_policy() {
        let pool = PooledProvider::new();
        assert!(pool.pacing.read().is_none(), "new pool has no pacing");

        pool.set_pacing(Some(PacingPolicy {
            effective_limit: 1_700_000,
            max_wait: Duration::from_secs(30),
            output_allowance: 1024,
        }));
        assert_eq!(
            pool.pacing.read().as_ref().unwrap().effective_limit,
            1_700_000
        );

        // A later edit (e.g. opting an agent out) takes effect in place.
        pool.set_pacing(None);
        assert!(pool.pacing.read().is_none(), "pacing disabled in place");
    }

    /// Bulletproof gate: any tier name reaching the pool — no matter
    /// which upstream defence let it through — is resolved through the
    /// tier registry before the underlying provider sees it.
    #[tokio::test]
    async fn tier_resolver_rewrites_tier_name_at_pool_layer() {
        let tier_key = Arc::new(parking_lot::RwLock::new("bedrock.anthropic".to_string()));
        let pool = PooledProvider::new().with_tier_resolver(TierResolver {
            tier_key,
            config_equivalences: Arc::new(parking_lot::RwLock::new(None)),
        });
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;

        let result = pool
            .create_message(None, &[], &[], Some("standard"), None, true)
            .await
            .unwrap();

        assert_ne!(result.model, "standard", "tier name leaked to provider");
        assert!(
            result.model.starts_with("anthropic.claude-sonnet"),
            "expected bedrock-anthropic standard tier resolution, got {:?}",
            result.model
        );
    }

    #[tokio::test]
    async fn tier_resolver_passes_through_concrete_model_ids() {
        let tier_key = Arc::new(parking_lot::RwLock::new("bedrock.anthropic".to_string()));
        let pool = PooledProvider::new().with_tier_resolver(TierResolver {
            tier_key,
            config_equivalences: Arc::new(parking_lot::RwLock::new(None)),
        });
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;

        let result = pool
            .create_message(
                None,
                &[],
                &[],
                Some("anthropic.claude-opus-4-7"),
                None,
                true,
            )
            .await
            .unwrap();

        assert_eq!(result.model, "anthropic.claude-opus-4-7");
    }

    /// The resolver reads `[llm.equivalences]` from the shared cell on every
    /// call, so a config hot-reload that swaps the cell changes resolution live
    /// without rebuilding the pool.
    #[tokio::test]
    async fn tier_resolver_reads_equivalences_live_from_cell() {
        let tier_key = Arc::new(parking_lot::RwLock::new("anthropic".to_string()));
        let equivalences: EquivalencesCell = Arc::new(parking_lot::RwLock::new(None));
        let pool = PooledProvider::new().with_tier_resolver(TierResolver {
            tier_key,
            config_equivalences: Arc::clone(&equivalences),
        });
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;

        let before = pool
            .create_message(None, &[], &[], Some("powerful"), None, true)
            .await
            .unwrap()
            .model;

        let mut overrides = HashMap::new();
        overrides.insert(
            "powerful".to_string(),
            vec!["claude-live-reload-test".to_string()],
        );
        *equivalences.write() = Some(Arc::new(overrides));

        let after = pool
            .create_message(None, &[], &[], Some("powerful"), None, true)
            .await
            .unwrap()
            .model;

        assert_ne!(before, after, "swapping the cell must change resolution");
        assert!(
            after.contains("claude-live-reload-test"),
            "expected live equivalence override, got {after:?}"
        );
    }

    #[tokio::test]
    async fn tier_resolver_disabled_leaves_input_untouched() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;

        let result = pool
            .create_message(None, &[], &[], Some("standard"), None, true)
            .await
            .unwrap();

        assert_eq!(
            result.model, "standard",
            "without resolver, pool must not modify the model id"
        );
    }

    #[tokio::test]
    async fn failover_on_rate_limit() {
        let pool = PooledProvider::new();
        pool.add("primary", Arc::new(MockRateLimited)).await;
        pool.add(
            "backup",
            Arc::new(MockOk {
                label: "backup-response".to_string(),
            }),
        )
        .await;

        let result = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("backup-response".to_string()));

        // Primary should now be rate-limited
        let statuses = pool.list().await;
        assert!(statuses[0].is_rate_limited);
        assert!(!statuses[1].is_rate_limited);
    }

    #[tokio::test]
    async fn all_rate_limited_returns_error() {
        let pool = PooledProvider::new();
        pool.add("a", Arc::new(MockRateLimited)).await;
        pool.add("b", Arc::new(MockRateLimited)).await;

        let result = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("429"));
    }

    #[tokio::test]
    async fn add_and_remove() {
        let pool = PooledProvider::new();
        pool.add(
            "a",
            Arc::new(MockOk {
                label: "a".to_string(),
            }),
        )
        .await;
        pool.add(
            "b",
            Arc::new(MockOk {
                label: "b".to_string(),
            }),
        )
        .await;

        assert_eq!(pool.len().await, 2);
        assert!(pool.remove("a").await);
        assert_eq!(pool.len().await, 1);
        assert!(!pool.remove("nonexistent").await);
    }

    #[tokio::test]
    async fn replace_existing_name() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "old".to_string(),
            }),
        )
        .await;
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "new".to_string(),
            }),
        )
        .await;

        assert_eq!(pool.len().await, 1);
        let result = pool
            .create_message(None, &[], &[], None, None, true)
            .await
            .unwrap();
        assert_eq!(result.text(), Some("new".to_string()));
    }

    #[tokio::test]
    async fn non_rate_limit_error_not_rotated() {
        let pool = PooledProvider::new();

        struct MockServerError;
        #[async_trait]
        impl LlmProvider for MockServerError {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!("500 Internal Server Error"))
            }
        }

        pool.add("primary", Arc::new(MockServerError)).await;
        pool.add(
            "backup",
            Arc::new(MockOk {
                label: "backup".to_string(),
            }),
        )
        .await;

        // 500 errors should NOT rotate to backup — let FailoverProvider handle those
        let result = pool.create_message(None, &[], &[], None, None, true).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("500"));
    }

    #[tokio::test]
    async fn per_user_limit_arms_no_pool_cooldown() {
        use crate::llm::failover::SUBSCRIPTION_UNAVAILABLE_MARKER;

        struct MockPlanLimit;
        #[async_trait]
        impl LlmProvider for MockPlanLimit {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!(
                    "{SUBSCRIPTION_UNAVAILABLE_MARKER}: Alice's ChatGPT plan is at its limit: \
                     ChatGPT API error (429): usage limit reached"
                ))
            }
        }

        let pool = PooledProvider::new();
        pool.add("chatgpt", Arc::new(MockPlanLimit)).await;

        let err = pool
            .create_message(None, &[], &[], Some("gpt-6.1-sol"), None, true)
            .await
            .expect_err("the plan limit propagates");
        assert!(
            err.to_string().contains(SUBSCRIPTION_UNAVAILABLE_MARKER),
            "the provider's own error propagates unchanged: {err}"
        );
        assert!(
            !pool.list().await[0].is_rate_limited,
            "one user's plan limit must not cool the entry for everyone"
        );
        assert!(pool.cooldown_skip_error(0, "gpt-6.1-sol").await.is_none());
    }

    #[tokio::test]
    async fn auth_error_rotates_to_next_token() {
        let pool = PooledProvider::new();

        struct MockAuthError;
        #[async_trait]
        impl LlmProvider for MockAuthError {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!(
                    "Anthropic API error (401 Unauthorized): Invalid bearer token"
                ))
            }
        }

        pool.add("expired-token", Arc::new(MockAuthError)).await;
        pool.add(
            "fresh-token",
            Arc::new(MockOk {
                label: "fresh-response".to_string(),
            }),
        )
        .await;

        // Auth errors (401) should rotate to the next token in the pool
        let result = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("fresh-response".to_string()));

        // Expired token should be marked as rate-limited (cooldown)
        let statuses = pool.list().await;
        assert!(statuses[0].is_rate_limited);
        assert!(!statuses[1].is_rate_limited);
    }

    #[tokio::test]
    async fn streaming_auth_error_rotates_to_next_token() {
        let pool = PooledProvider::new();

        struct MockAuthError;
        #[async_trait]
        impl LlmProvider for MockAuthError {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!(
                    "Anthropic API error (401 Unauthorized): Invalid bearer token"
                ))
            }
        }

        pool.add("expired-token", Arc::new(MockAuthError)).await;
        pool.add(
            "fresh-token",
            Arc::new(MockOk {
                label: "fresh-stream".to_string(),
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::channel(32);
        let result = pool
            .create_message_stream(None, &[], &[], Some("test"), None, true, tx)
            .await;

        assert!(result.is_ok(), "streaming should succeed via fresh token");

        let mut got_done = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, StreamEvent::MessageDone { .. }) {
                got_done = true;
            }
        }
        assert!(
            got_done,
            "should have received MessageDone from fresh token"
        );

        let statuses = pool.list().await;
        assert!(statuses[0].is_rate_limited);
        assert!(!statuses[1].is_rate_limited);
    }

    #[tokio::test]
    async fn all_auth_errors_returns_last_error() {
        let pool = PooledProvider::new();

        struct MockAuthError;
        #[async_trait]
        impl LlmProvider for MockAuthError {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!(
                    "Anthropic API error (401 Unauthorized): Invalid bearer token"
                ))
            }
        }

        pool.add("a", Arc::new(MockAuthError)).await;
        pool.add("b", Arc::new(MockAuthError)).await;

        let result = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("401"));
    }

    /// An auth-armed cooldown must surface as an auth failure carrying the
    /// arming error — not as the fake "Anthropic API error (429)" that sent
    /// the 2026-07-13 Bedrock Marketplace incident down a rate-limit rabbit
    /// hole.
    #[tokio::test]
    async fn cached_skip_replays_auth_reason_and_arming_error() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(
            0,
            None,
            "Bedrock API error (403): Model access is denied due to IAM user or service role \
             is not authorized to perform the required AWS Marketplace actions \
             (aws-marketplace:ViewSubscriptions, aws-marketplace:Subscribe)",
            CooldownReason::AuthFailed,
        )
        .await;

        let err = pool.cooldown_skip_error(0, "").await.unwrap().to_string();
        assert!(err.contains("auth-failed"));
        assert!(err.contains("AWS Marketplace"));
        assert!(!err.contains("retry-after"));
        assert_eq!(classify_error(&err), ErrorKind::Auth);
    }

    #[tokio::test]
    async fn cached_skip_replays_rate_limit_reason_with_retry_after() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(
            0,
            None,
            "Anthropic API error (429): rate limit exceeded",
            CooldownReason::RateLimited,
        )
        .await;

        let err = pool.cooldown_skip_error(0, "").await.unwrap().to_string();
        assert!(err.contains("rate-limited (429, cached)"));
        assert!(err.contains("rate limit exceeded"));
        assert!(err.contains("[retry-after:"));
        assert_eq!(classify_error(&err), ErrorKind::RateLimited);
    }

    #[tokio::test]
    async fn cached_skip_is_none_when_not_on_cooldown() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        assert!(pool.cooldown_skip_error(0, "").await.is_none());
    }

    #[tokio::test]
    async fn second_call_surfaces_auth_cooldown_not_fake_429() {
        let pool = PooledProvider::new();

        struct MockAuthError;
        #[async_trait]
        impl LlmProvider for MockAuthError {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!(
                    "Anthropic API error (401 Unauthorized): Invalid bearer token"
                ))
            }
        }
        pool.add("expired", Arc::new(MockAuthError)).await;

        let first = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await;
        assert!(first.is_err());

        let second = pool
            .create_message(None, &[], &[], Some("test"), None, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(second.contains("auth-failed"));
        assert!(second.contains("Invalid bearer token"));
        assert_eq!(classify_error(&second), ErrorKind::Auth);
    }

    #[tokio::test]
    async fn cooldown_error_is_truncated_on_char_boundary() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        let long = format!("Bedrock API error (403): {}", "é".repeat(500));
        pool.arm_cooldown(0, None, &long, CooldownReason::AuthFailed)
            .await;

        let stored = pool.list().await.remove(0).cooldown_error.unwrap();
        assert!(stored.len() <= COOLDOWN_ERROR_MAX_BYTES);
        assert!(stored.starts_with("Bedrock API error (403):"));
        assert!(long.starts_with(&stored));
    }

    /// Arming 429s usually embed the provider's own retry-after hint, which
    /// is stale (and possibly truncation-mangled) by the time the cached
    /// skip replays it. `parse_retry_after` takes the first match, so the
    /// skip's fresh remaining-cooldown hint must come first — otherwise the
    /// FailoverProvider's gated retry sleeps on the stale value.
    #[tokio::test]
    async fn cached_skip_retry_after_is_remaining_cooldown_not_stale_arming_hint() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(
            0,
            None,
            "Anthropic API error (429): rate limited [retry-after: 3000s]",
            CooldownReason::RateLimited,
        )
        .await;

        let err = pool.cooldown_skip_error(0, "").await.unwrap().to_string();
        let hint = parse_retry_after(&err).unwrap();
        assert!(
            hint <= MAX_COOLDOWN_SECS + 1,
            "hint must be the remaining pool cooldown, not the stale 3000s: {hint}"
        );
        assert_eq!(classify_error(&err), ErrorKind::RateLimited);
    }

    #[tokio::test]
    async fn recovery_clears_cooldown_error() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(
            0,
            None,
            "Anthropic API error (429): rate limit exceeded",
            CooldownReason::RateLimited,
        )
        .await;
        assert!(pool.list().await[0].cooldown_error.is_some());

        pool.clear_cooldowns(0, "").await;
        let status = pool.list().await.remove(0);
        assert!(!status.is_rate_limited);
        assert!(status.cooldown_error.is_none());
    }

    #[tokio::test]
    async fn list_status() {
        let pool = PooledProvider::with_cooldown(120);
        pool.add(
            "a",
            Arc::new(MockOk {
                label: "a".to_string(),
            }),
        )
        .await;
        pool.add(
            "b",
            Arc::new(MockOk {
                label: "b".to_string(),
            }),
        )
        .await;

        let statuses = pool.list().await;
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].name, "a");
        assert!(!statuses[0].is_rate_limited);
        assert_eq!(statuses[1].name, "b");
        assert!(!statuses[1].is_rate_limited);
    }

    // ── Streaming tests (mirroring non-streaming tests above) ────────

    #[tokio::test]
    async fn streaming_failover_on_rate_limit() {
        let pool = PooledProvider::new();
        pool.add("primary", Arc::new(MockRateLimited)).await;
        pool.add(
            "backup",
            Arc::new(MockOk {
                label: "backup-stream".to_string(),
            }),
        )
        .await;

        let (tx, mut rx) = mpsc::channel(32);
        let result = pool
            .create_message_stream(None, &[], &[], Some("test"), None, true, tx)
            .await;

        assert!(result.is_ok(), "streaming should succeed via backup token");

        // Verify we received a MessageDone event (stream completed)
        let mut got_done = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, StreamEvent::MessageDone { .. }) {
                got_done = true;
            }
        }
        assert!(got_done, "should have received MessageDone from backup");

        // Primary should be rate-limited, backup should not
        let statuses = pool.list().await;
        assert!(statuses[0].is_rate_limited);
        assert!(!statuses[1].is_rate_limited);
    }

    #[tokio::test]
    async fn streaming_all_rate_limited_returns_error() {
        let pool = PooledProvider::new();
        pool.add("a", Arc::new(MockRateLimited)).await;
        pool.add("b", Arc::new(MockRateLimited)).await;

        let (tx, _rx) = mpsc::channel(32);
        let result = pool
            .create_message_stream(None, &[], &[], Some("test"), None, true, tx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("429"));
    }

    #[tokio::test]
    async fn streaming_non_rate_limit_error_not_rotated() {
        let pool = PooledProvider::new();

        struct MockServerError;
        #[async_trait]
        impl LlmProvider for MockServerError {
            async fn create_message(
                &self,
                _: Option<&str>,
                _: &[LlmMessage],
                _: &[LlmToolDef],
                _: Option<&str>,
                _: Option<u32>,
                _: bool,
            ) -> Result<LlmResponse> {
                Err(eyre::eyre!("500 Internal Server Error"))
            }
        }

        pool.add("primary", Arc::new(MockServerError)).await;
        pool.add(
            "backup",
            Arc::new(MockOk {
                label: "backup".to_string(),
            }),
        )
        .await;

        let (tx, _rx) = mpsc::channel(32);
        // 500 errors should NOT rotate to backup — let FailoverProvider handle those
        let result = pool
            .create_message_stream(None, &[], &[], None, None, true, tx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("500"));
    }

    // ── Credential probe tests ──────────────────────────────────────────

    /// Provider whose `verify_credentials` returns the supplied error verbatim.
    struct MockProbeFail {
        error: String,
    }

    #[async_trait]
    impl LlmProvider for MockProbeFail {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("{}", self.error))
        }

        async fn verify_credentials(&self) -> Result<()> {
            Err(eyre::eyre!("{}", self.error))
        }
    }

    #[tokio::test]
    async fn probe_marks_auth_failed_entries_cooldown() {
        let pool = PooledProvider::new();
        pool.add(
            "good",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.add(
            "bad-auth",
            Arc::new(MockProbeFail {
                error: "Anthropic API error (401 Unauthorized): Invalid bearer token".into(),
            }),
        )
        .await;

        pool.probe_all_entries().await;

        let statuses = pool.list().await;
        let good = statuses.iter().find(|s| s.name == "good").unwrap();
        let bad = statuses.iter().find(|s| s.name == "bad-auth").unwrap();
        assert!(!good.is_rate_limited);
        assert!(bad.is_rate_limited);
        assert_eq!(bad.cooldown_reason, Some(CooldownReason::AuthFailed));
    }

    #[tokio::test]
    async fn probe_leaves_non_auth_entries_admitted() {
        let pool = PooledProvider::new();
        pool.add(
            "flaky",
            Arc::new(MockProbeFail {
                error: "Anthropic API error (500): server error".into(),
            }),
        )
        .await;

        pool.probe_all_entries().await;

        let statuses = pool.list().await;
        assert!(!statuses[0].is_rate_limited);
    }

    #[tokio::test]
    async fn probe_model_scoped_auth_leaves_entry_admitted() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockProbeFail {
                error: MARKETPLACE_403.into(),
            }),
        )
        .await;

        pool.probe_all_entries().await;

        let status = &pool.list().await[0];
        assert!(!status.is_rate_limited);
        assert!(status.model_cooldowns.is_empty());
    }

    #[tokio::test]
    async fn probe_empty_pool_is_noop() {
        let pool = PooledProvider::new();
        pool.probe_all_entries().await;
        assert_eq!(pool.len().await, 0);
    }

    #[tokio::test]
    async fn client_error_with_uuid_request_id_does_not_arm_cooldown() {
        // Verbatim 2026-08-18 incident error: the request-id UUID contains
        // "401" and armed a 60s entry-wide auth cooldown from a plain 400.
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockProbeFail {
                error: "Bedrock API error (400 Bad Request): {\"message\":\"The provided model identifier is invalid.\"} [request-id: 0792cccb-8a0b-4015-8f35-2b23025900e4]".into(),
            }),
        )
        .await;

        let result = pool
            .create_message(None, &[], &[], Some("claude-haiku-4-5"), None, false)
            .await;

        assert!(result.is_err());
        let status = &pool.list().await[0];
        assert!(!status.is_rate_limited);
        assert_eq!(status.cooldown_reason, None);
    }

    #[tokio::test]
    async fn definitive_credential_auth_error_arms_entry_cooldown() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockProbeFail {
                error: "Bedrock API error (403 Forbidden): The security token included in the request is expired".into(),
            }),
        )
        .await;

        let result = pool
            .create_message(None, &[], &[], Some("claude-sonnet-5"), None, false)
            .await;

        assert!(result.is_err());
        let status = &pool.list().await[0];
        assert!(status.is_rate_limited);
        assert_eq!(status.cooldown_reason, Some(CooldownReason::AuthFailed));
        assert!(status.model_cooldowns.is_empty());
    }

    // ── Model-scoped cooldown tests ─────────────────────────────────────

    const MARKETPLACE_403: &str = "Bedrock API error (403 Forbidden): {\"message\":\"Model access is denied due to IAM user or service role is not authorized to perform the required AWS Marketplace actions (aws-marketplace:ViewSubscriptions, aws-marketplace:Subscribe)\"}";

    /// Succeeds for every model except `denied`, which gets a marketplace
    /// 403. Counts invocations so replay tests can prove no wire call.
    struct MockModelDenied {
        denied: &'static str,
        invocations: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MockModelDenied {
        fn new(denied: &'static str) -> (Arc<std::sync::atomic::AtomicUsize>, Self) {
            let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Arc::clone(&invocations),
                Self {
                    denied,
                    invocations,
                },
            )
        }

        fn respond(&self, model: Option<&str>) -> Result<LlmResponse> {
            self.invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if model == Some(self.denied) {
                return Err(eyre::eyre!("{MARKETPLACE_403}"));
            }
            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: model.unwrap_or("default").to_string(),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for MockModelDenied {
        async fn create_message(
            &self,
            _: Option<&str>,
            _: &[LlmMessage],
            _: &[LlmToolDef],
            model: Option<&str>,
            _: Option<u32>,
            _: bool,
        ) -> Result<LlmResponse> {
            self.respond(model)
        }
    }

    #[tokio::test]
    async fn model_access_denial_cools_only_that_model() {
        let pool = PooledProvider::new();
        let (invocations, provider) = MockModelDenied::new("opus");
        pool.add("default", Arc::new(provider)).await;

        let denied = pool
            .create_message(None, &[], &[], Some("opus"), None, false)
            .await;
        assert!(denied.is_err());

        let status = &pool.list().await[0];
        assert!(!status.is_rate_limited, "entry must stay available");
        assert_eq!(status.cooldown_reason, None);
        assert_eq!(status.model_cooldowns.len(), 1);
        assert_eq!(status.model_cooldowns[0].model, "opus");
        assert_eq!(status.model_cooldowns[0].reason, CooldownReason::AuthFailed);

        // The 2026-08-21 incident shape: another model through the same
        // entry must invoke the provider and succeed.
        let other = pool
            .create_message(None, &[], &[], Some("sonnet"), None, false)
            .await
            .unwrap();
        assert_eq!(other.text(), Some("ok".to_string()));
        assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn model_scoped_replay_names_model_and_skips_wire_call() {
        let pool = PooledProvider::new();
        let (invocations, provider) = MockModelDenied::new("opus");
        pool.add("default", Arc::new(provider)).await;

        let _ = pool
            .create_message(None, &[], &[], Some("opus"), None, false)
            .await
            .unwrap_err();
        let replay = pool
            .create_message(None, &[], &[], Some("opus"), None, false)
            .await
            .unwrap_err()
            .to_string();

        assert!(replay.contains(POOL_MODEL_AUTH_SKIP_MARKER));
        assert!(replay.contains("'opus'"));
        assert!(replay.contains("AWS Marketplace"));
        assert_eq!(classify_error(&replay), ErrorKind::Auth);
        assert_eq!(
            invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cached skip must not hit the wire"
        );
    }

    #[tokio::test]
    async fn streaming_model_access_denial_cools_only_that_model() {
        let pool = PooledProvider::new();
        let (_, provider) = MockModelDenied::new("opus");
        pool.add("default", Arc::new(provider)).await;

        let (tx, _rx) = mpsc::channel(8);
        let _ = pool
            .create_message_stream(None, &[], &[], Some("opus"), None, false, tx)
            .await
            .unwrap_err();

        let (tx, mut rx) = mpsc::channel(8);
        pool.create_message_stream(None, &[], &[], Some("sonnet"), None, false, tx)
            .await
            .unwrap();
        let mut got_done = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, StreamEvent::MessageDone { .. }) {
                got_done = true;
            }
        }
        assert!(got_done, "sonnet must stream through the same entry");
    }

    #[tokio::test]
    async fn success_clears_own_model_cooldown_but_not_others() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(0, Some("opus"), MARKETPLACE_403, CooldownReason::AuthFailed)
            .await;
        pool.arm_cooldown(
            0,
            Some("haiku"),
            MARKETPLACE_403,
            CooldownReason::AuthFailed,
        )
        .await;
        pool.arm_cooldown(
            0,
            None,
            "Anthropic API error (429): limited",
            CooldownReason::RateLimited,
        )
        .await;

        pool.clear_cooldowns(0, "opus").await;

        let status = &pool.list().await[0];
        assert!(!status.is_rate_limited, "entry cooldown must clear");
        assert_eq!(status.model_cooldowns.len(), 1);
        assert_eq!(status.model_cooldowns[0].model, "haiku");
    }

    #[tokio::test]
    async fn entry_cooldown_wins_over_model_cooldown_in_replay() {
        let pool = PooledProvider::new();
        pool.add(
            "default",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(0, Some("opus"), MARKETPLACE_403, CooldownReason::AuthFailed)
            .await;
        pool.arm_cooldown(
            0,
            None,
            "Anthropic API error (401 Unauthorized): Invalid bearer token",
            CooldownReason::AuthFailed,
        )
        .await;

        let err = pool
            .cooldown_skip_error(0, "opus")
            .await
            .unwrap()
            .to_string();
        assert!(err.contains(POOL_AUTH_SKIP_MARKER));
        assert!(err.contains("Invalid bearer token"));
    }

    #[tokio::test]
    async fn expired_model_cooldowns_unblock_and_are_pruned_on_next_arm() {
        let pool = PooledProvider::with_cooldown(0);
        let (invocations, provider) = MockModelDenied::new("opus");
        pool.add("default", Arc::new(provider)).await;

        pool.arm_cooldown(
            0,
            Some("sonnet"),
            MARKETPLACE_403,
            CooldownReason::AuthFailed,
        )
        .await;
        assert!(
            pool.list().await[0].model_cooldowns.is_empty(),
            "expired model cooldowns must not surface in status"
        );
        let ok = pool
            .create_message(None, &[], &[], Some("sonnet"), None, false)
            .await
            .unwrap();
        assert_eq!(ok.text(), Some("ok".to_string()));
        assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);

        pool.arm_cooldown(
            0,
            Some("haiku"),
            MARKETPLACE_403,
            CooldownReason::AuthFailed,
        )
        .await;
        assert_eq!(
            pool.entries.read().await[0].model_cooldowns.len(),
            1,
            "expired keys must be pruned when a new cooldown is armed"
        );
    }

    #[tokio::test]
    async fn try_order_prefers_entries_clean_for_the_requested_model() {
        let pool = PooledProvider::new();
        pool.add(
            "a",
            Arc::new(MockOk {
                label: "a".to_string(),
            }),
        )
        .await;
        pool.add(
            "b",
            Arc::new(MockOk {
                label: "b".to_string(),
            }),
        )
        .await;
        pool.arm_cooldown(0, Some("opus"), MARKETPLACE_403, CooldownReason::AuthFailed)
            .await;

        assert_eq!(pool.try_order("opus").await, vec![1, 0]);
        assert_eq!(pool.try_order("sonnet").await, vec![0, 1]);
    }

    /// Both provider delimiter styles must parse — Anthropic and the pool's
    /// cached-skip use `[retry-after: Ns]`, Cerebras uses `(retry-after: Ns)`.
    /// The bracket-only parser silently ignored Cerebras hints for months.
    #[test]
    fn parse_retry_after_accepts_both_delimiter_styles() {
        assert_eq!(
            parse_retry_after("Anthropic API error (429): rate limited [retry-after: 30s]"),
            Some(30)
        );
        assert_eq!(
            parse_retry_after("Cerebras API error (429): quota exceeded (retry-after: 12s)"),
            Some(12)
        );
        assert_eq!(parse_retry_after("some other error"), None);
        assert_eq!(parse_retry_after("retry-after: nonsense s"), None);
    }

    fn window_of(now: Instant, entries: &[(u64, u32)]) -> VecDeque<(Instant, u32)> {
        entries
            .iter()
            .map(|(age_secs, amount)| (now - Duration::from_secs(*age_secs), *amount))
            .collect()
    }

    #[test]
    fn pacing_decision_proceeds_when_budget_fits() {
        let now = Instant::now();
        assert_eq!(
            pacing_decision(
                &VecDeque::new(),
                now,
                40_000,
                450_000,
                Duration::from_secs(30)
            ),
            PaceDecision::Proceed
        );
        let window = window_of(now, &[(10, 400_000)]);
        assert_eq!(
            pacing_decision(&window, now, 50_000, 450_000, Duration::from_secs(30)),
            PaceDecision::Proceed
        );
    }

    #[test]
    fn pacing_decision_waits_exact_shortfall() {
        let now = Instant::now();
        // 430k admitted 40s ago → frees at +60s, i.e. 20s from now. A 50k
        // request over a 450k budget must wait exactly that.
        let window = window_of(now, &[(40, 430_000)]);
        match pacing_decision(&window, now, 50_000, 450_000, Duration::from_secs(30)) {
            PaceDecision::Wait(wait) => {
                assert!((19..=20).contains(&wait.as_secs()), "wait was {wait:?}")
            }
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn pacing_decision_declines_waits_over_cap() {
        let now = Instant::now();
        // Freed only 5s before the window edge → 55s projected wait > 30s cap.
        let window = window_of(now, &[(5, 430_000)]);
        match pacing_decision(&window, now, 50_000, 450_000, Duration::from_secs(30)) {
            PaceDecision::Decline(wait) => {
                assert!((54..=55).contains(&wait.as_secs()), "wait was {wait:?}")
            }
            other => panic!("expected Decline, got {other:?}"),
        }
    }

    /// A single request bigger than the whole budget must be admitted — no
    /// wait can ever fit it, and declining traps it in permanent failover.
    /// The reactive 429 retry is the backstop.
    #[test]
    fn pacing_decision_admits_oversized_single_request() {
        let now = Instant::now();
        let window = window_of(now, &[(10, 400_000)]);
        assert_eq!(
            pacing_decision(&window, now, 600_000, 450_000, Duration::from_secs(30)),
            PaceDecision::Proceed
        );
    }

    #[test]
    fn prune_window_drops_aged_entries_keeps_future_stamps() {
        let now = Instant::now();
        let mut window = window_of(now, &[(70, 100), (61, 200), (30, 300)]);
        window.push_back((now + Duration::from_secs(10), 400));
        prune_window(&mut window, now);
        let amounts: Vec<u32> = window.iter().map(|(_, a)| *a).collect();
        assert_eq!(amounts, vec![300, 400]);
    }

    /// The synthetic decline error must flow through the existing failover
    /// policy: classified RateLimited with a parseable retry-after hint.
    #[test]
    fn pacing_decline_error_reuses_rate_limit_contract() {
        let msg = "pool pacing: projected TPM rate limit wait exceeds cap [retry-after: 55s]";
        assert_eq!(classify_error(msg), ErrorKind::RateLimited);
        assert_eq!(parse_retry_after(msg), Some(55));
    }

    #[tokio::test]
    async fn pace_admit_disabled_is_passthrough() {
        let pool = PooledProvider::new();
        pool.add(
            "a",
            Arc::new(MockOk {
                label: "ok".to_string(),
            }),
        )
        .await;
        assert_eq!(pool.pace_admit(0, u32::MAX).await, PaceDecision::Proceed);
    }
}
