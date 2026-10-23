//! Model failover — resilient LLM provider with retry, failover chains, aliases,
//! and model health tracking.
//!
//! Wraps an underlying [`LlmProvider`] and adds:
//! - **Retry** with exponential backoff on transient errors
//! - **Failover chains**: ordered list of models to try when the primary fails
//! - **Aliases**: friendly names mapped to full model identifiers
//! - **Health tracking**: marks models as degraded after repeated failures
//!

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use super::provider::{LlmProvider, ProviderKind};
use super::streaming::{
    call_with_request_cap, watchdogged_stream, StreamEvent, StreamTimeouts,
    FIRST_TOKEN_TIMEOUT_MARKER, STREAM_STALL_MARKER,
};
use super::types::{LlmMessage, LlmResponse, LlmToolDef};

// ─── Configuration ───────────────────────────────────────────────────────────

/// Failover configuration for the LLM provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailoverConfig {
    /// Ordered list of fallback models to try when the primary fails.
    #[serde(default)]
    pub failover: Vec<String>,
    /// Friendly name → full model identifier mapping.
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Retry configuration for transient errors.
    #[serde(default)]
    pub retry: RetryConfig,
    /// Health tracking configuration.
    #[serde(default)]
    pub health: HealthConfig,
}

impl FailoverConfig {
    /// Check if failover is effectively enabled (has fallback models).
    pub fn has_failover(&self) -> bool {
        !self.failover.is_empty()
    }
}

/// Retry policy for transient errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum retries before failing over to the next model.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Initial backoff duration in milliseconds.
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Backoff multiplier (exponential).
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Maximum backoff duration in milliseconds.
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
    /// Retry `Overloaded` (529) on the same model with backoff before failing
    /// over, instead of failing over immediately. Useful when the primary
    /// provider (e.g. Bedrock) sheds load transiently and the fallback is a
    /// metered account we'd rather not spill onto. Defaults to immediate failover.
    #[serde(default = "default_retry_overloaded")]
    pub retry_overloaded: bool,
    /// Retry `RateLimited` (429) on the same model when the provider's
    /// retry-after hint is short, instead of failing over immediately.
    /// Built for per-minute token quotas (e.g. Cerebras TPM): the window
    /// resets in seconds, and failing over swaps models mid-conversation —
    /// a quality cliff when the fallback is a weaker model. Only fires when
    /// the error carries a `retry-after` hint at or under
    /// [`Self::max_rate_limit_wait_secs`]; hint-less or long waits still
    /// fail over. Defaults to immediate failover.
    #[serde(default = "default_retry_rate_limited")]
    pub retry_rate_limited: bool,
    /// Longest retry-after hint (seconds) worth sleeping on before retrying
    /// the same model; anything longer fails over instead. Default 70 covers
    /// a full per-minute quota window plus jitter.
    #[serde(default = "default_max_rate_limit_wait_secs")]
    pub max_rate_limit_wait_secs: u64,
    /// Liveness watchdog windows for LLM calls.
    #[serde(default)]
    pub timeouts: LlmTimeouts,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            initial_backoff_ms: default_initial_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            max_backoff_ms: default_max_backoff_ms(),
            retry_overloaded: default_retry_overloaded(),
            retry_rate_limited: default_retry_rate_limited(),
            max_rate_limit_wait_secs: default_max_rate_limit_wait_secs(),
            timeouts: LlmTimeouts::default(),
        }
    }
}

/// Liveness watchdog windows for LLM calls, with per-model overrides.
/// A value of `0` disables that timer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTimeouts {
    /// Max seconds to wait for the first stream event after dispatching a
    /// streaming request. Firing before any event is forwarded, this is
    /// failover-safe: the chain silently moves to the next model.
    #[serde(default = "default_first_token_timeout_secs")]
    pub first_token_timeout_secs: u64,
    /// Max seconds between stream events once streaming has begun. Firing
    /// after partial content was surfaced, this aborts the turn instead of
    /// failing over.
    #[serde(default = "default_stall_timeout_secs")]
    pub stall_timeout_secs: u64,
    /// Max seconds for a whole non-streaming request.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Per-model overrides, keyed by exact model id or a substring of it
    /// (`"gemini-3.1"` matches every model containing it — keep keys
    /// specific enough to hit only what you mean). When several substring
    /// keys match, the longest wins.
    #[serde(default)]
    pub model_timeouts: HashMap<String, TimeoutOverride>,
}

/// Per-model override entry for [`LlmTimeouts`]; unset fields inherit the
/// global value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutOverride {
    #[serde(default)]
    pub first_token_timeout_secs: Option<u64>,
    #[serde(default)]
    pub stall_timeout_secs: Option<u64>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

impl Default for LlmTimeouts {
    fn default() -> Self {
        Self {
            first_token_timeout_secs: default_first_token_timeout_secs(),
            stall_timeout_secs: default_stall_timeout_secs(),
            request_timeout_secs: default_request_timeout_secs(),
            model_timeouts: HashMap::new(),
        }
    }
}

impl LlmTimeouts {
    /// Exact-match lookup first, then substring with the longest matching
    /// key winning — determinism `effective_max_tokens_for_model`'s
    /// first-hit HashMap scan lacks.
    fn override_for(&self, model: &str) -> Option<&TimeoutOverride> {
        self.model_timeouts.get(model).or_else(|| {
            self.model_timeouts
                .iter()
                .filter(|(key, _)| model.contains(key.as_str()))
                .max_by_key(|(key, _)| key.len())
                .map(|(_, v)| v)
        })
    }

    /// Resolved streaming watchdog windows for `model`.
    pub fn stream_timeouts_for(&self, model: &str) -> StreamTimeouts {
        let over = self.override_for(model);
        let first = over
            .and_then(|o| o.first_token_timeout_secs)
            .unwrap_or(self.first_token_timeout_secs);
        let stall = over
            .and_then(|o| o.stall_timeout_secs)
            .unwrap_or(self.stall_timeout_secs);
        StreamTimeouts {
            first_token: Duration::from_secs(first),
            stall: Duration::from_secs(stall),
        }
    }

    /// Resolved non-streaming request cap for `model`; `None` = disabled.
    pub fn request_timeout_for(&self, model: &str) -> Option<Duration> {
        let secs = self
            .override_for(model)
            .and_then(|o| o.request_timeout_secs)
            .unwrap_or(self.request_timeout_secs);
        (secs > 0).then(|| Duration::from_secs(secs))
    }
}

/// Wire → runtime mapping, owned here so the hub and CLI don't each
/// hand-copy the field list (a missed field there compiles and silently
/// never propagates).
impl From<&workforce_common::settings::RetryConfig> for LlmTimeouts {
    fn from(r: &workforce_common::settings::RetryConfig) -> Self {
        Self {
            first_token_timeout_secs: r.first_token_timeout_secs,
            stall_timeout_secs: r.stall_timeout_secs,
            request_timeout_secs: r.request_timeout_secs,
            model_timeouts: r
                .model_timeouts
                .iter()
                .map(|(model, o)| {
                    (
                        model.clone(),
                        TimeoutOverride {
                            first_token_timeout_secs: o.first_token_timeout_secs,
                            stall_timeout_secs: o.stall_timeout_secs,
                            request_timeout_secs: o.request_timeout_secs,
                        },
                    )
                })
                .collect(),
        }
    }
}

fn default_max_retries() -> u32 {
    3
}
fn default_initial_backoff_ms() -> u64 {
    1000
}
fn default_backoff_multiplier() -> f64 {
    2.0
}
fn default_max_backoff_ms() -> u64 {
    30_000
}
fn default_retry_overloaded() -> bool {
    false
}
fn default_retry_rate_limited() -> bool {
    false
}
fn default_max_rate_limit_wait_secs() -> u64 {
    70
}
fn default_first_token_timeout_secs() -> u64 {
    45
}
fn default_stall_timeout_secs() -> u64 {
    180
}
fn default_request_timeout_secs() -> u64 {
    300
}

/// Health tracking configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    /// Number of failures within `window_secs` to mark a model degraded.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// Time window in seconds for failure counting.
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// Seconds before retrying a degraded model.
    #[serde(default = "default_recovery_secs")]
    pub recovery_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            window_secs: default_window_secs(),
            recovery_secs: default_recovery_secs(),
        }
    }
}

fn default_failure_threshold() -> u32 {
    3
}
fn default_window_secs() -> u64 {
    300
}
fn default_recovery_secs() -> u64 {
    120
}

// ─── Error Classification ────────────────────────────────────────────────────

/// Classification of an LLM error for retry/failover decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// Rate limited (429) — retry with backoff.
    RateLimited,
    /// Server error (500, 502, 503) — retry, then failover.
    ServerError,
    /// Timeout — retry, then failover.
    Timeout,
    /// Streaming watchdog fired before any event arrived — nothing was
    /// surfaced, so failing over is invisible. Skips same-model retries:
    /// another attempt would burn a full watchdog window against a
    /// provider already known to be hanging.
    FirstTokenTimeout,
    /// Streaming watchdog fired after events were forwarded — partial
    /// content already reached the consumer, so abort rather than fail
    /// over (a silent model switch would duplicate surfaced output).
    StreamStalled,
    /// Overloaded (529) — failover immediately.
    Overloaded,
    /// Billing error (402) — abort, no retry or failover.
    Billing,
    /// Authentication error (401, 403) — failover immediately to different token.
    Auth,
    /// Context window exceeded — abort, need compaction not retry.
    ContextOverflow,
    /// Client error (4xx except 401, 402, 403, 429) — don't retry.
    ClientError,
    /// The payer's subscription login can't serve this call (none, at its
    /// limit, invalid, or the model is outside the plan). Fail over at once,
    /// and never count it against the model's health: one user's plan says
    /// nothing about the model for everyone else.
    SubscriptionUnavailable,
    /// The provider can't serve this request's shape at all (a code-mode
    /// model asked for function tools). Every model behind the same route
    /// would refuse it too, so abort.
    Unsupported,
    /// Unknown error — retry once, then failover.
    Unknown,
}

impl ErrorKind {
    /// Should we retry this error (on the same model)?
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::ServerError | Self::Timeout | Self::Unknown
        )
    }

    /// Should we failover to the next model immediately (skip retries)?
    /// Rate-limited (429) also skips retries because the limit window is minutes,
    /// not seconds — retrying with backoff just wastes time.
    /// Auth errors try a different token/provider immediately.
    /// Note: `Overloaded` is config-gated — see [`RetryConfig::retry_overloaded`],
    /// applied in [`failure_action`].
    pub fn should_failover_immediately(&self) -> bool {
        matches!(
            self,
            Self::Overloaded
                | Self::RateLimited
                | Self::Auth
                | Self::FirstTokenTimeout
                | Self::SubscriptionUnavailable
        )
    }

    /// Should we abort entirely (no retry, no failover)?
    /// Billing issues affect all tokens on the same account; a stalled
    /// stream has already surfaced partial content.
    pub fn should_abort(&self) -> bool {
        matches!(
            self,
            Self::Billing | Self::StreamStalled | Self::Unsupported
        )
    }

    /// Whether the failure says something about the model itself, so it
    /// counts toward degrading the model for every caller.
    pub fn is_model_fault(&self) -> bool {
        !matches!(self, Self::SubscriptionUnavailable | Self::Unsupported)
    }

    /// Context overflow needs compaction, not retry or failover.
    pub fn needs_compaction(&self) -> bool {
        matches!(self, Self::ContextOverflow)
    }
}

/// What to do with a failed attempt, given the error and retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureAction {
    /// Propagate immediately — no retry, no failover (billing).
    Abort,
    /// Context overflow — caller decides escalation vs. propagation per path.
    Compact,
    /// Retry the same model after backoff.
    Retry,
    /// Retry the same model after sleeping this many seconds — the
    /// provider's own retry-after hint, not exponential backoff. The sleep
    /// also outlasts the pool's cooldown for the same hint, so the retry
    /// reaches a live entry instead of the cached-skip error.
    RetryAfter(u64),
    /// Give up this model and try the next in the chain.
    Failover,
}

/// Decide the action for a failed attempt. Pure and shared by the streaming and
/// non-streaming paths so the retry/failover policy lives in one place.
///
/// `retry_after_secs` is the provider's parsed retry-after hint, when the
/// error carried one. With [`RetryConfig::retry_rate_limited`] enabled, a 429
/// whose hint is at or under [`RetryConfig::max_rate_limit_wait_secs`] sleeps
/// out the quota window on the same model instead of failing over (per-minute
/// token quotas reset in seconds; failing over swaps models mid-conversation).
/// Hint-less 429s still fail over — with no reset time there is nothing
/// principled to sleep on.
fn failure_action(
    kind: &ErrorKind,
    retry: &RetryConfig,
    attempt: u32,
    retry_after_secs: Option<u64>,
) -> FailureAction {
    if kind.should_abort() {
        return FailureAction::Abort;
    }
    if kind.needs_compaction() {
        return FailureAction::Compact;
    }
    let overloaded_retry = retry.retry_overloaded && *kind == ErrorKind::Overloaded;
    let rate_limit_retry = retry.retry_rate_limited
        && *kind == ErrorKind::RateLimited
        && retry_after_secs.is_some_and(|s| s <= retry.max_rate_limit_wait_secs);
    if (kind.should_failover_immediately() && !overloaded_retry && !rate_limit_retry)
        || *kind == ErrorKind::ClientError
    {
        return FailureAction::Failover;
    }
    if attempt >= retry.max_retries {
        return FailureAction::Failover;
    }
    if rate_limit_retry {
        // +1s so the pool cooldown armed from the same hint has lapsed by
        // the time we retry — otherwise the retry hits the cached skip.
        return FailureAction::RetryAfter(retry_after_secs.unwrap_or(0) + 1);
    }
    if kind.is_retryable() || overloaded_retry {
        FailureAction::Retry
    } else {
        FailureAction::Failover
    }
}

/// Run one streaming attempt through a relay to `tx`, and report whether
/// any content reached it.
async fn relay_stream<F, Fut>(tx: &mpsc::Sender<StreamEvent>, call: F) -> (Result<()>, bool)
where
    F: FnOnce(mpsc::Sender<StreamEvent>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (relay_tx, mut relay_rx) = mpsc::channel::<StreamEvent>(32);
    let relay = async {
        let mut forwarded = false;
        while let Some(event) = relay_rx.recv().await {
            forwarded |= event.carries_content();
            if tx.send(event).await.is_err() {
                break;
            }
        }
        forwarded
    };
    tokio::join!(call(relay_tx), relay)
}

/// Marker carried by every error a subscription provider raises when the
/// payer's login can't serve the call. `classify_error` keys on it.
pub const SUBSCRIPTION_UNAVAILABLE_MARKER: &str = "subscription unavailable";

/// Marker carried by errors for a request the provider can never serve.
/// `classify_error` keys on it.
pub const UNSUPPORTED_REQUEST_MARKER: &str = "request unsupported by provider";

/// Classify an error string into an `ErrorKind`.
///
/// Precedence: watchdog markers, pool cached-skip markers, a status parsed
/// from the providers' canonical `"… error ({status}): body"` format, then
/// textual heuristics. A parsed status is authoritative — free text in the
/// body or appended suffixes (`[request-id: …]` UUIDs) cannot override it.
pub fn classify_error(error: &str) -> ErrorKind {
    let lower = error.to_lowercase();

    // Watchdog markers first — both contain words the generic arms below
    // would misclassify (e.g. "timeout").
    if lower.contains(FIRST_TOKEN_TIMEOUT_MARKER) {
        return ErrorKind::FirstTokenTimeout;
    }
    if lower.contains(STREAM_STALL_MARKER) {
        return ErrorKind::StreamStalled;
    }

    // Pool cached-skip replays wrap the arming error verbatim, so they must
    // classify by their own marker, not by the inner error's status.
    if lower.contains(super::pool::POOL_AUTH_SKIP_MARKER)
        || lower.contains(super::pool::POOL_MODEL_AUTH_SKIP_MARKER)
    {
        return ErrorKind::Auth;
    }
    if lower.contains(super::pool::POOL_RATE_LIMIT_SKIP_MARKER) {
        return ErrorKind::RateLimited;
    }

    // Subscription refusals quote the backend error they came from, whose
    // status ("… error (429): usage limit reached") must not win.
    if lower.contains(SUBSCRIPTION_UNAVAILABLE_MARKER) {
        return ErrorKind::SubscriptionUnavailable;
    }
    if lower.contains(UNSUPPORTED_REQUEST_MARKER) {
        return ErrorKind::Unsupported;
    }

    if let Some(kind) = parse_status_marker(&lower).and_then(|s| classify_status(s, &lower)) {
        return kind;
    }

    // Heuristic fallback for errors without a status marker (transport
    // failures, foreign formats). Bare status codes must be digit-delimited
    // tokens so ids like "8a0b-4015-8f35" never match.
    if contains_status_token(&lower, "429")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
    {
        return ErrorKind::RateLimited;
    }
    if contains_status_token(&lower, "529") || lower.contains("overloaded") {
        return ErrorKind::Overloaded;
    }
    if contains_status_token(&lower, "402")
        || lower.contains("payment required")
        || lower.contains("billing")
    {
        return ErrorKind::Billing;
    }
    if contains_status_token(&lower, "401")
        || contains_status_token(&lower, "403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
        return ErrorKind::Auth;
    }
    // Textual forms too: Bedrock event-stream errors carry the exception
    // message ("Internal Server Error") with no HTTP status code.
    if contains_status_token(&lower, "500")
        || contains_status_token(&lower, "502")
        || contains_status_token(&lower, "503")
        || lower.contains("internal server error")
        || lower.contains("internalserverexception")
        || lower.contains("service unavailable")
        || lower.contains("serviceunavailableexception")
        || lower.contains("modelstreamerror")
    {
        return ErrorKind::ServerError;
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return ErrorKind::Timeout;
    }
    if let Some(kind) = classify_4xx_body(&lower) {
        return kind;
    }
    // Generic 4xx errors (specific ones handled above)
    if contains_status_token(&lower, "400")
        || contains_status_token(&lower, "404")
        || contains_status_token(&lower, "422")
    {
        return ErrorKind::ClientError;
    }

    ErrorKind::Unknown
}

/// Parse the status from the first `" error (NNN"` provider marker, e.g.
/// `"Bedrock API error (400 Bad Request): …"`. Requires exactly three digits
/// after the paren so free text can never masquerade as a status.
fn parse_status_marker(lower: &str) -> Option<u16> {
    lower.match_indices(" error (").find_map(|(idx, marker)| {
        let rest = &lower[idx + marker.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits == 3).then(|| rest[..3].parse().ok()).flatten()
    })
}

/// Map a parsed HTTP status to an `ErrorKind`. Returns `None` outside the
/// 4xx/5xx range so the textual heuristics take over.
fn classify_status(status: u16, lower: &str) -> Option<ErrorKind> {
    match status {
        429 => Some(ErrorKind::RateLimited),
        529 => Some(ErrorKind::Overloaded),
        402 => Some(ErrorKind::Billing),
        401 | 403 => Some(ErrorKind::Auth),
        408 => Some(ErrorKind::Timeout),
        500..=599 => Some(ErrorKind::ServerError),
        400..=499 => Some(classify_4xx_body(lower).unwrap_or(ErrorKind::ClientError)),
        _ => None,
    }
}

/// Distinguish the 4xx bodies that need special handling.
fn classify_4xx_body(lower: &str) -> Option<ErrorKind> {
    // max_tokens exceeds model limit — configuration error, not context overflow.
    // Matches: "max_tokens: 32768 > 32000, which is the maximum allowed"
    if lower.contains("max_tokens") && lower.contains("maximum allowed") {
        return Some(ErrorKind::ClientError);
    }
    // Context overflow — Anthropic returns this as a 400 with specific message.
    // Matches: "context too long", "prompt is too long", "tokens > N maximum", etc.
    if (lower.contains("context") || lower.contains("prompt"))
        && (lower.contains("too long") || lower.contains("overflow") || lower.contains("exceeded"))
    {
        return Some(ErrorKind::ContextOverflow);
    }
    (lower.contains("tokens") && lower.contains("maximum")).then_some(ErrorKind::ContextOverflow)
}

/// True for a status-code substring standing alone as a token — not embedded
/// in a longer digit run or identifier (request-id UUIDs, model names).
fn contains_status_token(lower: &str, code: &str) -> bool {
    let bytes = lower.as_bytes();
    lower.match_indices(code).any(|(i, _)| {
        let before = i.checked_sub(1).map(|p| bytes[p]);
        let after = bytes.get(i + code.len());
        !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

/// True only when an auth classification is structural — a parsed 401/403
/// status marker or an explicit "unauthorized"/"forbidden" — never a bare
/// digit-token match. Gates the pool's entry-wide auth cooldown so a
/// heuristic guess cannot poison every model behind a credential.
pub fn auth_is_definitive(error: &str) -> bool {
    let lower = error.to_lowercase();
    matches!(parse_status_marker(&lower), Some(401 | 403))
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
}

/// True when a definitive auth failure names the *model* as the denied
/// resource rather than the credential — the token/role itself works, so
/// the pool cools down only `(entry, model)` instead of blanketing every
/// model behind the credential. Unknown 401/403 shapes stay
/// credential-scoped: entry-wide is the safe default unless the body says
/// the model is the problem.
pub fn auth_is_model_scoped(error: &str) -> bool {
    let lower = error.to_lowercase();
    [
        "model access is denied",
        "access to the model",
        "aws marketplace",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

// ─── Health Tracking ─────────────────────────────────────────────────────────

/// Health status for a single model.
#[derive(Debug, Clone, Default)]
struct ModelHealth {
    /// Recent failure timestamps (within window).
    failures: Vec<Instant>,
    /// When the model was marked degraded (if at all).
    degraded_at: Option<Instant>,
}

/// Tracks health status for multiple models.
#[derive(Debug)]
pub struct HealthTracker {
    config: HealthConfig,
    models: RwLock<HashMap<String, ModelHealth>>,
}

impl HealthTracker {
    pub fn new(config: HealthConfig) -> Self {
        Self {
            config,
            models: RwLock::new(HashMap::new()),
        }
    }

    /// Record a failure for a model. Returns true if the model is now degraded.
    pub async fn record_failure(&self, model: &str) -> bool {
        let mut models = self.models.write().await;
        let health = models.entry(model.to_string()).or_default();
        let now = Instant::now();
        let window = Duration::from_secs(self.config.window_secs);

        // Add failure and prune old ones
        health.failures.push(now);
        health.failures.retain(|t| now.duration_since(*t) < window);

        // Check if threshold exceeded
        if health.failures.len() >= self.config.failure_threshold as usize
            && health.degraded_at.is_none()
        {
            health.degraded_at = Some(now);
            warn!(
                model = model,
                failures = health.failures.len(),
                window_secs = self.config.window_secs,
                "Model marked as degraded"
            );
            return true;
        }
        false
    }

    /// Record a success for a model (clears degraded status).
    pub async fn record_success(&self, model: &str) {
        let mut models = self.models.write().await;
        if let Some(health) = models.get_mut(model) {
            if health.degraded_at.is_some() {
                info!(model = model, "Model recovered from degraded state");
            }
            health.degraded_at = None;
            health.failures.clear();
        }
    }

    /// Check if a model is currently degraded.
    pub async fn is_degraded(&self, model: &str) -> bool {
        let models = self.models.read().await;
        if let Some(health) = models.get(model) {
            if let Some(degraded_at) = health.degraded_at {
                let recovery = Duration::from_secs(self.config.recovery_secs);
                if Instant::now().duration_since(degraded_at) >= recovery {
                    // Recovery period elapsed — allow retrying (will clear on success)
                    return false;
                }
                return true;
            }
        }
        false
    }

    /// Get a snapshot of all model health statuses.
    pub async fn status(&self) -> HashMap<String, ModelStatus> {
        let models = self.models.read().await;
        let now = Instant::now();
        let window = Duration::from_secs(self.config.window_secs);

        models
            .iter()
            .map(|(name, health)| {
                let recent_failures = health
                    .failures
                    .iter()
                    .filter(|t| now.duration_since(**t) < window)
                    .count() as u32;

                let is_degraded = health.degraded_at.is_some_and(|degraded_at| {
                    now.duration_since(degraded_at) < Duration::from_secs(self.config.recovery_secs)
                });

                (
                    name.clone(),
                    ModelStatus {
                        recent_failures,
                        is_degraded,
                        degraded_at_secs_ago: health
                            .degraded_at
                            .map(|t| now.duration_since(t).as_secs()),
                    },
                )
            })
            .collect()
    }
}

/// Public-facing health status for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStatus {
    pub recent_failures: u32,
    pub is_degraded: bool,
    pub degraded_at_secs_ago: Option<u64>,
}

// ─── Failover Provider ───────────────────────────────────────────────────────

/// A parsed chain entry: model name + optional provider key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSpec {
    /// Full chain string (e.g., "grok:grok-4" or "claude-sonnet-4-5")
    pub raw: String,
    /// Model name to pass to the provider (e.g., "llama-3.3-70b" or "claude-sonnet-4-5")
    pub model: String,
    /// Provider key (e.g., Some("grok")) or None for primary provider.
    pub provider_key: Option<String>,
    /// The context window of this model on the provider that serves it, if
    /// known. Filled by [`FailoverProvider::model_chain`].
    context_limit: Option<u32>,
}

/// Parse a chain entry string. Entries can optionally have a provider prefix:
/// - `"claude-sonnet-4-5"` → primary provider, model `"claude-sonnet-4-5"`
/// - `"grok:grok-4"` → provider `"grok"`, model `"grok-4"`
/// - `"anthropic.claude-haiku-4-5-20251001-v1:0"` → primary provider, the
///   full Bedrock id (its version-suffix colon is not a provider prefix)
fn parse_chain_entry(s: &str) -> ChainSpec {
    let (provider_key, model) = super::tiers::split_routing_prefix(s);
    ChainSpec {
        raw: s.to_string(),
        model: model.to_string(),
        provider_key: provider_key.map(str::to_string),
        context_limit: None,
    }
}

/// The chain entry `spec` resolves to under `tiers`: routed through its
/// provider and with a tier name resolved against that provider's tiers.
pub fn routed_spec(tiers: &super::tiers::TierRegistry, spec: &str) -> ChainSpec {
    let (routing_key, model) = tiers.resolve_routed_model(spec, "default");
    parse_chain_entry(&super::tiers::TierRegistry::format_routed(
        routing_key.as_deref(),
        model,
    ))
}

/// An LLM provider with retry, failover, health tracking, and cross-provider
/// chain support.
///
/// Wraps an underlying provider (for the primary model) and adds resilience:
/// 1. Resolve model aliases
/// 2. Skip degraded models (prefer healthy alternatives)
/// 3. Retry transient errors with exponential backoff
/// 4. Fail over to next model in chain on persistent failures
/// 5. Route chain entries to different providers (e.g., grok, openrouter)
pub struct FailoverProvider {
    inner: Arc<dyn LlmProvider>,
    config: RwLock<FailoverConfig>,
    /// External providers for non-primary chain entries (e.g., "grok" → GrokClient).
    external_providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    health: Arc<HealthTracker>,
    last_used_model: parking_lot::RwLock<Option<String>>,
    tier_registry: super::tiers::TierRegistry,
    /// The hub-level configured default model (workforce.toml `default_model`).
    /// Used as the last-resort fallback after every chain entry has failed —
    /// rescues agents whose per-agent override points at a provider-specific
    /// model id that the active primary provider can't serve.
    hub_default_model: Option<String>,
}

impl FailoverProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, config: FailoverConfig) -> Self {
        let health = Arc::new(HealthTracker::new(config.health.clone()));
        Self {
            inner,
            config: RwLock::new(config),
            external_providers: RwLock::new(HashMap::new()),
            health,
            last_used_model: parking_lot::RwLock::new(None),
            tier_registry: super::tiers::TierRegistry::new(None),
            hub_default_model: None,
        }
    }

    pub fn with_tier_registry(mut self, registry: super::tiers::TierRegistry) -> Self {
        self.tier_registry = registry;
        self
    }

    pub fn with_hub_default_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        if !model.is_empty() {
            self.hub_default_model = Some(model);
        }
        self
    }

    /// The model that was used in the most recent successful LLM call.
    pub fn last_used_model(&self) -> Option<String> {
        self.last_used_model.read().clone()
    }

    /// Get a reference to the health tracker (for status endpoints).
    pub fn health_tracker(&self) -> &Arc<HealthTracker> {
        &self.health
    }

    /// Check if a specific model is currently degraded.
    pub async fn is_model_healthy(&self, model: &str) -> bool {
        !self.health.is_degraded(model).await
    }

    /// Return the first healthy model from the full chain (primary + failover),
    /// or `None` if all are degraded.
    pub async fn first_healthy_model(&self) -> Option<String> {
        let config = self.config.read().await;
        for m in &config.failover {
            let resolved = config
                .aliases
                .get(m.as_str())
                .cloned()
                .unwrap_or_else(|| m.to_string());
            let entry = parse_chain_entry(&resolved);
            if !self.health.is_degraded(&entry.raw).await {
                return Some(entry.raw);
            }
        }
        None
    }

    /// Return the providers that currently have at least one healthy model
    /// in the failover chain, ordered by chain priority.
    pub async fn healthy_providers(&self) -> Vec<super::tiers::Provider> {
        use super::tiers::Provider;
        let chain = self.model_chain(None).await;
        let mut seen = Vec::new();
        for entry in &chain {
            if !self.health.is_degraded(&entry.raw).await {
                let (provider, _) = Provider::from_model_str(&entry.raw);
                if !seen.contains(&provider) {
                    seen.push(provider);
                }
            }
        }
        seen
    }

    /// Resolve a model alias to its full identifier.
    /// Returns the input unchanged if it's not an alias.
    pub async fn resolve_alias(&self, model: &str) -> String {
        let config = self.config.read().await;
        config
            .aliases
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_string())
    }

    // ─── Runtime mutation methods ────────────────────────────────────────

    /// Update the failover chain at runtime.
    pub async fn set_chain(&self, chain: Vec<String>) {
        let mut config = self.config.write().await;
        info!(chain = ?chain, "Failover chain updated");
        config.failover = chain;
    }

    /// Get the current failover chain.
    pub async fn get_chain(&self) -> Vec<String> {
        self.config.read().await.failover.clone()
    }

    /// Update the retry policy at runtime so workspace `[llm.retry]` edits
    /// take effect without a hub restart.
    pub async fn set_retry(&self, retry: RetryConfig) {
        let mut config = self.config.write().await;
        info!(?retry, "Failover retry config updated");
        config.retry = retry;
    }

    /// Get the current retry policy.
    pub async fn get_retry(&self) -> RetryConfig {
        self.config.read().await.retry.clone()
    }

    /// Get the current aliases.
    pub async fn get_aliases(&self) -> HashMap<String, String> {
        self.config.read().await.aliases.clone()
    }

    /// Add or replace an external provider (for non-primary chain entries).
    pub async fn add_external_provider(&self, key: &str, provider: Arc<dyn LlmProvider>) {
        self.external_providers
            .write()
            .await
            .insert(key.to_string(), provider);
        info!(provider = key, "External provider added to failover");
    }

    /// Remove an external provider.
    pub async fn remove_external_provider(&self, key: &str) -> bool {
        let removed = self.external_providers.write().await.remove(key).is_some();
        if removed {
            info!(provider = key, "External provider removed from failover");
        }
        removed
    }

    /// List external provider keys.
    pub async fn list_external_providers(&self) -> Vec<String> {
        self.external_providers
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }

    /// Verify credentials on every external provider in parallel and remove
    /// any whose probe returns an auth-failed error. Non-auth errors are
    /// logged but left in place so transient upstream issues don't strand a
    /// healthy entry. Quietly noops when no external providers are wired.
    /// How long any one provider gets to answer a startup credentials probe.
    ///
    /// Generous for a call that normally takes under a second, because the only
    /// thing being bought is a guarantee that startup finishes.
    pub const PROBE_TIMEOUT_SECS: u64 = 15;

    pub async fn probe_all_external_providers(&self) {
        let snapshot: Vec<(String, Arc<dyn LlmProvider>)> = {
            let providers = self.external_providers.read().await;
            providers
                .iter()
                .map(|(k, p)| (k.clone(), Arc::clone(p)))
                .collect()
        };
        if snapshot.is_empty() {
            return;
        }
        let results =
            futures::future::join_all(snapshot.into_iter().map(|(key, provider)| async move {
                // Per provider, so one unresponsive endpoint cannot hold up the
                // others and so the log names which one was at fault. An unbounded
                // probe here stopped the hub binding its port at all: startup runs
                // this before the listener, so a provider that accepted the
                // connection and then never answered wedged the process, failed the
                // health check, and rolled the deploy back. Nothing could ship
                // until the provider recovered.
                let outcome = tokio::time::timeout(
                    std::time::Duration::from_secs(Self::PROBE_TIMEOUT_SECS),
                    provider.verify_credentials(),
                )
                .await;
                (key, outcome)
            }))
            .await;
        for (key, outcome) in results {
            let Ok(result) = outcome else {
                // Unknown, not bad: a provider that did not answer in time
                // keeps its place rather than being dropped, because the cost
                // of dropping a working provider is worse than one wasted
                // round-trip on the first request that needs it.
                warn!(
                    provider = key.as_str(),
                    timeout_secs = Self::PROBE_TIMEOUT_SECS,
                    "External provider credentials probe timed out; keeping it and moving on"
                );
                continue;
            };
            match result {
                Ok(()) => {
                    info!(
                        provider = key.as_str(),
                        "External provider credentials probe ok"
                    );
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let kind = classify_error(&err_str);
                    if matches!(kind, ErrorKind::Auth) {
                        warn!(
                            provider = key.as_str(),
                            error = %e,
                            "External provider credential probe failed (auth) — removing from failover"
                        );
                        self.external_providers.write().await.remove(&key);
                    } else {
                        warn!(
                            provider = key.as_str(),
                            error = %e,
                            "External provider credential probe failed (non-auth) — leaving in failover"
                        );
                    }
                }
            }
        }
    }

    // ─── Internal helpers ────────────────────────────────────────────────

    /// Return the hub default model only if it isn't already represented in
    /// the chain — used to gate the last-resort fallback so we don't
    /// re-attempt a model that just failed.
    fn hub_default_to_try(&self, chain: &[ChainSpec]) -> Option<&str> {
        let hub_default = self.hub_default_model.as_deref()?;
        let already_present = chain
            .iter()
            .any(|e| e.raw == hub_default || e.model == hub_default);
        (!already_present).then_some(hub_default)
    }

    /// The last-resort hub default's entry, when it is worth trying after
    /// `chain`: not already in it.
    fn hub_default_entry(&self, chain: &[ChainSpec]) -> Option<ChainSpec> {
        self.hub_default_to_try(chain)
            .map(|hub_default| self.routed_entry(hub_default))
    }

    /// The provider serving the last-resort `entry`, when it is routable.
    async fn hub_default_route(
        &self,
        entry: Option<ChainSpec>,
    ) -> Option<(Arc<dyn LlmProvider>, ChainSpec)> {
        let entry = entry?;
        match self.provider_for_entry(&entry).await {
            Ok((provider, _)) => Some((provider, entry)),
            Err(e) => {
                warn!(model = entry.raw.as_str(), error = %e, "Hub default is not routable here");
                None
            }
        }
    }

    /// Get the provider for a chain entry. Returns inner (primary) if no
    /// provider key, or the matching external provider. The flag reports
    /// whether an external provider was actually resolved — externals
    /// bypass the pool and its liveness watchdog, so the failover layer
    /// must apply the watchdog to them itself. An unregistered metered key
    /// falls back to the primary; an unregistered subscription key is
    /// refused, so a subscription route never lands on a metered primary.
    async fn provider_for_entry(&self, entry: &ChainSpec) -> Result<(Arc<dyn LlmProvider>, bool)> {
        let Some(key) = &entry.provider_key else {
            return Ok((Arc::clone(&self.inner), false));
        };
        match self.external_providers.read().await.get(key) {
            Some(external) => Ok((Arc::clone(external), true)),
            None if ProviderKind::from_str_loose(key).is_some_and(ProviderKind::is_subscription) => {
                Err(eyre::eyre!(
                    "{SUBSCRIPTION_UNAVAILABLE_MARKER}: no {key} provider is set up on this agent for {}",
                    entry.raw
                ))
            }
            None => Ok((Arc::clone(&self.inner), false)),
        }
    }

    /// The chain entry `spec` resolves to: routed through its provider and
    /// with a tier name resolved against that provider's tiers.
    fn routed_entry(&self, spec: &str) -> ChainSpec {
        routed_spec(&self.tier_registry, spec)
    }

    /// The chain a request for `requested` walks, in the configured order:
    /// the requested model (alias- and tier-resolved), then each failover
    /// entry, provider-only entries resolved to the requested model's tier,
    /// with repeats dropped. Health does not reorder it.
    pub async fn resolve_chain(&self, requested: Option<&str>) -> Vec<ChainSpec> {
        let config = self.config.read().await;
        // Resolve in two passes: aliases first (e.g., "best" → "anthropic:powerful"),
        // then tier names (e.g., "openrouter:deepseek:powerful" → "openrouter:deepseek/deepseek-v3.2").
        // Idempotent for concrete model IDs — see `tiers::resolve_routed_model`.
        // Incident 2026-05-18: an un-resolved `openrouter:deepseek:powerful` reached
        // the OpenRouter API and 400'd with "deepseek:powerful is not a valid model ID".
        let primary = requested
            .map(|m| self.routed_entry(config.aliases.get(m).map_or(m, String::as_str)))
            .filter(|entry| !entry.raw.is_empty());

        // Failover models — entries can be:
        // - "provider:model" (concrete) e.g. "gemini:gemini-2.5-pro"
        // - "provider" (tier-resolved) e.g. "gemini" → resolves to the same tier as primary
        // - "model" (same provider) e.g. "claude-haiku-4-6"
        let primary_tier = primary
            .as_ref()
            .map_or(super::tiers::ModelTier::Standard, |entry| {
                self.tier_registry.classify(&entry.raw)
            });

        let failover = config.failover.iter().map(|m| {
            let resolved = config
                .aliases
                .get(m.as_str())
                .cloned()
                .unwrap_or_else(|| m.to_string());

            // Detect provider-only entries: "gemini", "grok", "openrouter:deepseek"
            let provider_tier_key = if resolved.contains(':') {
                super::tiers::parse_provider_tier_key(&resolved)
                    .ok()
                    .map(|(_name, key)| key)
            } else {
                super::tiers::Provider::from_name(&resolved).map(|_| resolved.clone())
            };

            provider_tier_key
                .and_then(|tier_key| {
                    let model = self
                        .tier_registry
                        .resolve_for_tier(primary_tier, &tier_key)?;
                    let base_provider = resolved
                        .split_once(':')
                        .map_or(resolved.as_str(), |(base, _)| base);
                    Some(ChainSpec {
                        raw: format!("{resolved}:{model}"),
                        model,
                        provider_key: Some(base_provider.to_string()),
                        context_limit: None,
                    })
                })
                .unwrap_or_else(|| parse_chain_entry(&resolved))
        });

        primary
            .into_iter()
            .chain(failover)
            .fold(Vec::new(), |mut chain: Vec<ChainSpec>, entry| {
                if !chain.iter().any(|e| e.raw == entry.raw) {
                    chain.push(entry);
                }
                chain
            })
    }

    /// Build the ordered list of chain entries to try: [`Self::resolve_chain`]
    /// with degraded models pushed to the end.
    async fn model_chain(&self, requested: Option<&str>) -> Vec<ChainSpec> {
        self.ordered(self.resolve_chain(requested).await).await
    }

    /// `chain` as it is walked: healthy entries first, each keeping its
    /// place, then the degraded ones (better to try a degraded model than
    /// fail), each with the window of the provider that serves it.
    async fn ordered(&self, chain: Vec<ChainSpec>) -> Vec<ChainSpec> {
        let mut entries: Vec<ChainSpec> = Vec::new();
        let mut degraded: Vec<ChainSpec> = Vec::new();
        for entry in chain {
            if self.health.is_degraded(&entry.raw).await {
                degraded.push(entry);
            } else {
                entries.push(entry);
            }
        }
        entries.extend(degraded);
        self.with_context_limits(entries).await
    }

    /// The configured entries `raws` as a provider of their own: a request
    /// walks exactly them — the requested one first when it is among them —
    /// and never the hub default.
    pub fn slice(self: &Arc<Self>, raws: Vec<String>) -> Arc<ChainSlice> {
        Arc::new(ChainSlice {
            chain: Arc::clone(self),
            entries: raws.iter().map(|raw| parse_chain_entry(raw)).collect(),
            answered: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Each entry's window as the provider that serves it reports it, so
    /// context-overflow escalation compares what the providers can take
    /// rather than the models' nominal windows.
    async fn with_context_limits(&self, entries: Vec<ChainSpec>) -> Vec<ChainSpec> {
        futures::future::join_all(entries.into_iter().map(|entry| async move {
            let context_limit = match self.provider_for_entry(&entry).await {
                Ok((provider, _)) => provider.context_floor(&entry.model).await,
                Err(_) => None,
            };
            ChainSpec {
                context_limit,
                ..entry
            }
        }))
        .await
    }

    /// Execute a request with retry and failover logic across `chain`, then
    /// on `hub_default` as a last resort.
    #[allow(clippy::too_many_arguments)]
    async fn with_failover(
        &self,
        chain: Vec<ChainSpec>,
        hub_default: Option<ChainSpec>,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let retry_config = {
            let config = self.config.read().await;
            config.retry.clone()
        };

        let mut last_error: Option<eyre::Error> = None;

        for (model_idx, entry) in chain.iter().enumerate() {
            let (provider, is_external) = match self.provider_for_entry(entry).await {
                Ok(routed) => routed,
                Err(e) => {
                    warn!(model = entry.raw.as_str(), error = %e, "Skipping unroutable chain entry");
                    last_error = Some(e);
                    continue;
                }
            };
            let model_ref = Some(entry.model.as_str());
            let is_last = model_idx == chain.len() - 1;

            // Retry loop for this model
            let max_retries = retry_config.max_retries;
            let mut backoff_ms = retry_config.initial_backoff_ms;

            for attempt in 0..=max_retries {
                let call = provider.create_message(
                    system_prompt,
                    messages,
                    tools,
                    model_ref,
                    max_tokens,
                    enable_cache,
                );
                // Externals bypass the pool's request cap — apply it here.
                let cap = is_external
                    .then(|| retry_config.timeouts.request_timeout_for(&entry.model))
                    .flatten();
                match call_with_request_cap(cap, &entry.model, call).await {
                    Ok(response) => {
                        self.health.record_success(&entry.raw).await;
                        *self.last_used_model.write() = Some(entry.raw.clone());
                        if model_idx > 0 || attempt > 0 {
                            info!(
                                model = entry.raw.as_str(),
                                attempt = attempt,
                                failover_index = model_idx,
                                "Request succeeded after retry/failover"
                            );
                        }
                        return Ok(response);
                    }
                    Err(e) => {
                        let error_str = format!("{}", e);
                        let kind = classify_error(&error_str);

                        warn!(
                            model = entry.raw.as_str(),
                            attempt = attempt,
                            error_kind = ?kind,
                            error = error_str.as_str(),
                            "LLM call failed"
                        );

                        let action = failure_action(
                            &kind,
                            &retry_config,
                            attempt,
                            super::pool::parse_retry_after(&error_str),
                        );
                        // One health failure per model abandonment, not per retry,
                        // so an internal retry burst doesn't prematurely degrade it.
                        if kind.is_model_fault()
                            && !matches!(
                                action,
                                FailureAction::Retry | FailureAction::RetryAfter(_)
                            )
                        {
                            self.health.record_failure(&entry.raw).await;
                        }

                        match action {
                            FailureAction::Abort => return Err(e),
                            FailureAction::Compact => {
                                let current_limit = entry.context_limit.unwrap_or(0);
                                let has_larger_or_unknown_model =
                                    chain.iter().skip(model_idx + 1).any(|next_entry| {
                                        let next_limit = next_entry.context_limit.unwrap_or(0);
                                        next_limit > current_limit
                                            || entry.context_limit.is_none()
                                            || next_entry.context_limit.is_none()
                                    });
                                if is_last || !has_larger_or_unknown_model {
                                    return Err(e);
                                }
                                info!(
                                    model = entry.raw.as_str(),
                                    limit = ?entry.context_limit,
                                    "Context overflow, escalating to next model with larger window"
                                );
                                last_error = Some(e);
                                break;
                            }
                            FailureAction::Retry => {
                                info!(
                                    model = entry.raw.as_str(),
                                    backoff_ms = backoff_ms,
                                    attempt = attempt + 1,
                                    max = max_retries,
                                    "Retrying after backoff"
                                );
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms =
                                    (backoff_ms as f64 * retry_config.backoff_multiplier) as u64;
                                backoff_ms = backoff_ms.min(retry_config.max_backoff_ms);
                                continue;
                            }
                            FailureAction::RetryAfter(wait_secs) => {
                                info!(
                                    model = entry.raw.as_str(),
                                    wait_secs = wait_secs,
                                    attempt = attempt + 1,
                                    max = max_retries,
                                    "Rate-limited — sleeping out provider retry-after before retrying same model"
                                );
                                tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                                continue;
                            }
                            FailureAction::Failover => {
                                last_error = Some(e);
                                if !is_last {
                                    info!(
                                        from_model = entry.raw.as_str(),
                                        to_model = chain
                                            .get(model_idx + 1)
                                            .map(|e| e.raw.as_str())
                                            .unwrap_or("?"),
                                        "Failing over to next model"
                                    );
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        if let Some((provider, entry)) = self.hub_default_route(hub_default).await {
            let hub_default = entry.raw.as_str();
            warn!(
                chain = ?chain.iter().map(|e| e.raw.as_str()).collect::<Vec<_>>(),
                hub_default,
                "Failover chain exhausted; attempting hub default as last-resort"
            );
            match provider
                .create_message(
                    system_prompt,
                    messages,
                    tools,
                    Some(&entry.model),
                    max_tokens,
                    enable_cache,
                )
                .await
            {
                Ok(response) => {
                    *self.last_used_model.write() = Some(hub_default.to_string());
                    info!(
                        model = hub_default,
                        "Last-resort hub default fallback succeeded — investigate stale agent override"
                    );
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        model = hub_default,
                        error = %e,
                        "Last-resort hub default fallback also failed"
                    );
                }
            }
        }

        Err(last_error.unwrap_or_else(|| eyre::eyre!("All models in failover chain exhausted")))
    }

    /// Stream a request with retry and failover logic across `chain`, then
    /// on `hub_default` as a last resort.
    /// 429s happen before streaming starts, so the chain moves on on initial
    /// connection errors and retries with backoff; once streaming has begun
    /// and events are flowing, it can't retry mid-stream.
    #[allow(clippy::too_many_arguments)]
    async fn stream_with_failover(
        &self,
        chain: Vec<ChainSpec>,
        hub_default: Option<ChainSpec>,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let retry_config = {
            let config = self.config.read().await;
            config.retry.clone()
        };

        let mut last_error: Option<eyre::Error> = None;

        for (model_idx, entry) in chain.iter().enumerate() {
            let (provider, is_external) = match self.provider_for_entry(entry).await {
                Ok(routed) => routed,
                Err(e) => {
                    warn!(model = entry.raw.as_str(), error = %e, "Skipping unroutable chain entry");
                    last_error = Some(e);
                    continue;
                }
            };
            let model_ref = Some(entry.model.as_str());
            let is_last = model_idx == chain.len() - 1;

            // Retry loop for this model
            let max_retries = retry_config.max_retries;
            let mut backoff_ms = retry_config.initial_backoff_ms;

            for attempt in 0..=max_retries {
                let provider = &provider;
                let timeouts = retry_config.timeouts.stream_timeouts_for(&entry.model);
                let (result, forwarded) = relay_stream(&tx, |attempt_tx| async move {
                    // Externals bypass the pool's liveness watchdog — wrap here.
                    if is_external {
                        watchdogged_stream(timeouts, &entry.model, attempt_tx, |inner_tx| {
                            provider.create_message_stream(
                                system_prompt,
                                messages,
                                tools,
                                model_ref,
                                max_tokens,
                                enable_cache,
                                inner_tx,
                            )
                        })
                        .await
                    } else {
                        provider
                            .create_message_stream(
                                system_prompt,
                                messages,
                                tools,
                                model_ref,
                                max_tokens,
                                enable_cache,
                                attempt_tx,
                            )
                            .await
                    }
                })
                .await;
                match result {
                    Ok(()) => {
                        self.health.record_success(&entry.raw).await;
                        *self.last_used_model.write() = Some(entry.raw.clone());
                        if model_idx > 0 || attempt > 0 {
                            info!(
                                model = entry.raw.as_str(),
                                attempt = attempt,
                                failover_index = model_idx,
                                "Streaming succeeded after retry/failover"
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        let error_str = format!("{}", e);
                        let kind = classify_error(&error_str);

                        warn!(
                            model = entry.raw.as_str(),
                            attempt = attempt,
                            error_kind = ?kind,
                            error = error_str.as_str(),
                            "Streaming call failed"
                        );

                        // Content already reached `tx`: a retry or another
                        // model would append a second answer to it.
                        let action = if forwarded {
                            FailureAction::Abort
                        } else {
                            failure_action(
                                &kind,
                                &retry_config,
                                attempt,
                                super::pool::parse_retry_after(&error_str),
                            )
                        };
                        // One health failure per model abandonment, not per retry,
                        // so an internal retry burst doesn't prematurely degrade it.
                        if kind.is_model_fault()
                            && !matches!(
                                action,
                                FailureAction::Retry | FailureAction::RetryAfter(_)
                            )
                        {
                            self.health.record_failure(&entry.raw).await;
                        }

                        match action {
                            FailureAction::Abort => return Err(e),
                            FailureAction::Compact => {
                                let current_limit = entry.context_limit.unwrap_or(0);
                                let has_larger_model =
                                    chain.iter().skip(model_idx + 1).any(|next_entry| {
                                        next_entry.context_limit.unwrap_or(0) > current_limit
                                    });
                                if is_last || !has_larger_model {
                                    return Err(e);
                                }
                                info!(
                                    model = entry.raw.as_str(),
                                    limit = ?entry.context_limit,
                                    "Context overflow on stream, escalating to next model with larger window"
                                );
                                last_error = Some(e);
                                break;
                            }
                            FailureAction::Retry => {
                                info!(
                                    model = entry.raw.as_str(),
                                    backoff_ms = backoff_ms,
                                    attempt = attempt + 1,
                                    max = max_retries,
                                    "Retrying streaming after backoff"
                                );
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms =
                                    (backoff_ms as f64 * retry_config.backoff_multiplier) as u64;
                                backoff_ms = backoff_ms.min(retry_config.max_backoff_ms);
                                continue;
                            }
                            FailureAction::RetryAfter(wait_secs) => {
                                info!(
                                    model = entry.raw.as_str(),
                                    wait_secs = wait_secs,
                                    attempt = attempt + 1,
                                    max = max_retries,
                                    "Rate-limited — sleeping out provider retry-after before retrying same model"
                                );
                                let _ = tx
                                    .send(StreamEvent::RateLimitWait {
                                        wait_secs,
                                        attempt: attempt + 1,
                                    })
                                    .await;
                                tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                                continue;
                            }
                            FailureAction::Failover => {
                                // Tell the UI why the card is switching models
                                // instead of leaving a generic "Thinking…"
                                // through a second watchdog window.
                                if kind == ErrorKind::FirstTokenTimeout && !is_last {
                                    let elapsed_secs = retry_config
                                        .timeouts
                                        .stream_timeouts_for(&entry.model)
                                        .first_token
                                        .as_secs();
                                    let _ = tx
                                        .send(StreamEvent::SlowModelFailover {
                                            model: entry.raw.clone(),
                                            elapsed_secs,
                                        })
                                        .await;
                                }
                                last_error = Some(e);
                                if !is_last {
                                    info!(
                                        from_model = entry.raw.as_str(),
                                        to_model = chain
                                            .get(model_idx + 1)
                                            .map(|e| e.raw.as_str())
                                            .unwrap_or("?"),
                                        "Streaming failing over to next model"
                                    );
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        if let Some((provider, entry)) = self.hub_default_route(hub_default).await {
            let hub_default = entry.raw.as_str();
            warn!(
                chain = ?chain.iter().map(|e| e.raw.as_str()).collect::<Vec<_>>(),
                hub_default,
                "Streaming failover chain exhausted; attempting hub default as last-resort"
            );
            match provider
                .create_message_stream(
                    system_prompt,
                    messages,
                    tools,
                    Some(&entry.model),
                    max_tokens,
                    enable_cache,
                    tx.clone(),
                )
                .await
            {
                Ok(()) => {
                    *self.last_used_model.write() = Some(hub_default.to_string());
                    info!(
                        model = hub_default,
                        "Last-resort hub default stream fallback succeeded — investigate stale agent override"
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        model = hub_default,
                        error = %e,
                        "Last-resort hub default stream fallback also failed"
                    );
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| eyre::eyre!("All models in streaming failover chain exhausted")))
    }
}

#[async_trait]
impl LlmProvider for FailoverProvider {
    /// The requested model's window on the provider that serves it: the
    /// external a routed spec names, else the primary. A fallback is reached
    /// only when the primary fails, and a request too large for it fails
    /// over again, so it does not narrow every turn.
    async fn context_floor(&self, model: &str) -> Option<u32> {
        let entry = self.routed_entry(model);
        match self.provider_for_entry(&entry).await {
            Ok((external, true)) => external.context_floor(&entry.model).await,
            _ => self.inner.context_floor(model).await,
        }
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
        let chain = self.model_chain(model).await;
        if chain.is_empty() {
            // No models configured — call inner directly
            return self
                .inner
                .create_message(
                    system_prompt,
                    messages,
                    tools,
                    model,
                    max_tokens,
                    enable_cache,
                )
                .await;
        }
        let hub_default = self.hub_default_entry(&chain);
        self.with_failover(
            chain,
            hub_default,
            system_prompt,
            messages,
            tools,
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
        let chain = self.model_chain(model).await;
        if chain.is_empty() {
            return self
                .inner
                .create_message_stream(
                    system_prompt,
                    messages,
                    tools,
                    model,
                    max_tokens,
                    enable_cache,
                    tx,
                )
                .await;
        }
        let hub_default = self.hub_default_entry(&chain);
        self.stream_with_failover(
            chain,
            hub_default,
            system_prompt,
            messages,
            tools,
            max_tokens,
            enable_cache,
            tx,
        )
        .await
    }
}

/// A run of a [`FailoverProvider`]'s configured entries serving as a
/// provider of its own: one runner route of an agent turn whose chain also
/// holds CLI routes. It walks only its entries, under the chain's retry and
/// health, and never reaches for the hub default.
pub struct ChainSlice {
    chain: Arc<FailoverProvider>,
    entries: Vec<ChainSpec>,
    answered: std::sync::atomic::AtomicBool,
}

impl ChainSlice {
    /// Whether any call on the slice has been answered or streamed content:
    /// until one has, no model output exists, so no tool can have run on it
    /// and nobody has read a word of it.
    pub fn answered(&self) -> bool {
        self.answered.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The entries a request for `requested` walks: the slice's own, the
    /// requested one first when it is among them, healthy before degraded.
    async fn chain_for(&self, requested: Option<&str>) -> Vec<ChainSpec> {
        let (asked, rest): (Vec<ChainSpec>, Vec<ChainSpec>) = self
            .entries
            .iter()
            .cloned()
            .partition(|entry| requested == Some(entry.raw.as_str()));
        self.chain
            .ordered(asked.into_iter().chain(rest).collect())
            .await
    }

    fn note<T>(&self, outcome: Result<T>, forwarded: bool) -> Result<T> {
        if outcome.is_ok() || forwarded {
            self.answered
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        outcome
    }
}

#[async_trait]
impl LlmProvider for ChainSlice {
    async fn context_floor(&self, model: &str) -> Option<u32> {
        self.chain.context_floor(model).await
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
        let chain = self.chain_for(model).await;
        self.note(
            self.chain
                .with_failover(
                    chain,
                    None,
                    system_prompt,
                    messages,
                    tools,
                    max_tokens,
                    enable_cache,
                )
                .await,
            false,
        )
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
        let chain = self.chain_for(model).await;
        let (outcome, forwarded) = relay_stream(&tx, |relayed| {
            self.chain.stream_with_failover(
                chain,
                None,
                system_prompt,
                messages,
                tools,
                max_tokens,
                enable_cache,
                relayed,
            )
        })
        .await;
        self.note(outcome, forwarded)
    }
}

/// Delegate `LlmProvider` from `Arc<FailoverProvider>` so it can be stored in
/// both `Box<dyn LlmProvider>` and `Arc<FailoverProvider>` simultaneously.
#[async_trait]
impl LlmProvider for Arc<FailoverProvider> {
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
    use std::sync::atomic::{AtomicU32, Ordering};

    // ── Error classification tests ───────────────────────────────────────

    #[test]
    fn classify_rate_limit() {
        assert_eq!(
            classify_error("Anthropic API error (429): rate limit exceeded"),
            ErrorKind::RateLimited
        );
        assert_eq!(classify_error("rate limit hit"), ErrorKind::RateLimited);
    }

    #[test]
    fn classify_server_errors() {
        assert_eq!(
            classify_error("Anthropic API error (500): internal server error"),
            ErrorKind::ServerError
        );
        assert_eq!(classify_error("502 Bad Gateway"), ErrorKind::ServerError);
        assert_eq!(
            classify_error("503 Service Unavailable"),
            ErrorKind::ServerError
        );
        // Bedrock mid-stream error events carry exception text, no status code.
        assert_eq!(
            classify_error("Bedrock streaming error: Internal Server Error"),
            ErrorKind::ServerError
        );
        assert_eq!(
            classify_error("Bedrock streaming error: internalServerException"),
            ErrorKind::ServerError
        );
        assert_eq!(
            classify_error("Bedrock streaming error: modelStreamErrorException"),
            ErrorKind::ServerError
        );
        assert_eq!(
            classify_error("Bedrock API error: serviceUnavailableException"),
            ErrorKind::ServerError
        );
    }

    #[test]
    fn classify_overloaded() {
        assert_eq!(classify_error("529 overloaded"), ErrorKind::Overloaded);
    }

    #[test]
    fn classify_gemini_empty_response_is_retryable() {
        // Gemini's empty-[DONE] response is surfaced as a (503) so it retries
        // (then fails over) instead of delivering an empty turn to the user.
        let kind = classify_error(
            "Gemini streaming API error (503): stream completed with [DONE] but emitted no content or tool calls (stop_reason=EndTurn; possible safety/content filter)",
        );
        assert_eq!(kind, ErrorKind::ServerError);
        assert!(kind.is_retryable());
        assert!(!kind.should_failover_immediately());
    }

    #[test]
    fn classify_timeout() {
        assert_eq!(classify_error("request timed out"), ErrorKind::Timeout);
    }

    #[test]
    fn classify_client_error() {
        assert_eq!(
            classify_error("Anthropic API error (400): invalid request"),
            ErrorKind::ClientError
        );
        assert_eq!(
            classify_error("422 Unprocessable Entity"),
            ErrorKind::ClientError
        );
    }

    #[test]
    fn classify_billing() {
        assert_eq!(
            classify_error("Anthropic API error (402): Payment Required"),
            ErrorKind::Billing
        );
        assert_eq!(
            classify_error("billing account suspended"),
            ErrorKind::Billing
        );
    }

    #[test]
    fn classify_auth() {
        assert_eq!(classify_error("401 Unauthorized"), ErrorKind::Auth);
        assert_eq!(classify_error("403 Forbidden"), ErrorKind::Auth);
        assert_eq!(
            classify_error("request unauthorized: invalid API key"),
            ErrorKind::Auth
        );
    }

    #[test]
    fn classify_context_overflow() {
        assert_eq!(
            classify_error("context length exceeded: messages too long"),
            ErrorKind::ContextOverflow
        );
        assert_eq!(
            classify_error("prompt is too long: context overflow"),
            ErrorKind::ContextOverflow
        );
    }

    #[test]
    fn classify_max_tokens_exceeds_model_limit() {
        // This is a config error, not a context overflow
        let err = "Anthropic API error (400 Bad Request): {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"max_tokens: 32768 > 32000, which is the maximum allowed number of output tokens for claude-opus-4-20250514\"}}";
        assert_eq!(classify_error(err), ErrorKind::ClientError);
        assert!(!classify_error(err).needs_compaction());
    }

    #[test]
    fn classify_uuid_request_id_is_not_auth() {
        // Verbatim arming error from the 2026-08-18 incident: the request-id
        // UUID contains "401" ("-4015-") and used to classify Auth, arming a
        // 60s pool-wide auth cooldown from a plain 400.
        let incident = "Bedrock API error (400 Bad Request): {\"message\":\"The provided model identifier is invalid.\"} [request-id: 0792cccb-8a0b-4015-8f35-2b23025900e4]";
        assert_eq!(classify_error(incident), ErrorKind::ClientError);
        assert!(!auth_is_definitive(incident));
    }

    #[test]
    fn classify_status_marker_beats_body_text() {
        assert_eq!(
            classify_error("Bedrock API error (403 Forbidden): {\"message\":\"You don't have access to the model with the specified model ID.\"}"),
            ErrorKind::Auth
        );
        assert_eq!(
            classify_error("Anthropic API error (500): retry after the rate limit window"),
            ErrorKind::ServerError
        );
    }

    #[test]
    fn classify_bare_digit_runs_are_not_statuses() {
        assert_eq!(
            classify_error("connection reset by host pool-4292 (id f4031b)"),
            ErrorKind::Unknown
        );
    }

    #[test]
    fn classify_pool_skip_replays_by_marker() {
        let auth_replay = "LLM pool: entry auth-failed (401/403, cached): Bedrock API error (400 Bad Request): {\"message\":\"The provided model identifier is invalid.\"}";
        assert_eq!(classify_error(auth_replay), ErrorKind::Auth);
        let model_replay = "LLM pool: model auth-failed (401/403, cached) for 'eu.anthropic.claude-opus-4-8': Bedrock API error (429): stale";
        assert_eq!(classify_error(model_replay), ErrorKind::Auth);
        let rate_replay = "LLM pool: entry rate-limited (429, cached) [retry-after: 42s]: Anthropic API error (403): stale";
        assert_eq!(classify_error(rate_replay), ErrorKind::RateLimited);
    }

    #[test]
    fn auth_is_definitive_requires_structural_evidence() {
        assert!(auth_is_definitive("Anthropic API error (401): bad key"));
        assert!(auth_is_definitive(
            "Bedrock API error (403 Forbidden): denied"
        ));
        assert!(auth_is_definitive("request unauthorized: invalid API key"));
        assert!(!auth_is_definitive("weird transport failure 401"));
    }

    #[test]
    fn auth_is_model_scoped_only_for_model_denial_bodies() {
        assert!(auth_is_model_scoped(
            "Bedrock API error (403 Forbidden): {\"message\":\"Model access is denied due to \
             IAM user or service role is not authorized to perform the required AWS \
             Marketplace actions (aws-marketplace:ViewSubscriptions, aws-marketplace:Subscribe)\"}"
        ));
        assert!(auth_is_model_scoped(
            "Bedrock API error (403 Forbidden): {\"message\":\"You don't have access to the \
             model with the specified model ID.\"}"
        ));
        assert!(!auth_is_model_scoped(
            "Anthropic API error (401 Unauthorized): Invalid bearer token"
        ));
        assert!(!auth_is_model_scoped(
            "Bedrock API error (403 Forbidden): The security token included in the request is expired"
        ));
        assert!(!auth_is_model_scoped("request forbidden"));
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify_error("something went wrong"), ErrorKind::Unknown);
    }

    #[test]
    fn retryable_classification() {
        assert!(ErrorKind::RateLimited.is_retryable());
        assert!(ErrorKind::ServerError.is_retryable());
        assert!(ErrorKind::Timeout.is_retryable());
        assert!(ErrorKind::Unknown.is_retryable());
        assert!(!ErrorKind::ClientError.is_retryable());
        assert!(!ErrorKind::Overloaded.is_retryable());
        assert!(!ErrorKind::Billing.is_retryable());
        assert!(!ErrorKind::Auth.is_retryable());
        assert!(!ErrorKind::ContextOverflow.is_retryable());
    }

    #[test]
    fn abort_classification() {
        assert!(ErrorKind::Billing.should_abort());
        assert!(!ErrorKind::ContextOverflow.should_abort()); // handled by runner via needs_compaction()
        assert!(ErrorKind::ContextOverflow.needs_compaction());
        assert!(!ErrorKind::RateLimited.should_abort());
        assert!(!ErrorKind::Auth.should_abort());
        assert!(!ErrorKind::ServerError.should_abort());
        assert!(!ErrorKind::ClientError.should_abort());
    }

    #[test]
    fn failover_immediately_classification() {
        assert!(ErrorKind::Overloaded.should_failover_immediately());
        assert!(ErrorKind::RateLimited.should_failover_immediately());
        assert!(ErrorKind::Auth.should_failover_immediately());
        assert!(!ErrorKind::Billing.should_failover_immediately());
        assert!(!ErrorKind::ServerError.should_failover_immediately());
        assert!(!ErrorKind::ContextOverflow.should_failover_immediately());
    }

    // ── Alias resolution tests ───────────────────────────────────────────

    #[tokio::test]
    async fn resolve_alias_basic() {
        let config = FailoverConfig {
            aliases: [
                ("fast".to_string(), "claude-3-5-haiku-20241022".to_string()),
                ("smart".to_string(), "claude-opus-4-20250514".to_string()),
            ]
            .into(),
            ..Default::default()
        };
        let provider = FailoverProvider::new(Arc::new(MockOkProvider::new("ok")), config);
        assert_eq!(
            provider.resolve_alias("fast").await,
            "claude-3-5-haiku-20241022"
        );
        assert_eq!(
            provider.resolve_alias("smart").await,
            "claude-opus-4-20250514"
        );
        assert_eq!(
            provider.resolve_alias("claude-sonnet-4").await,
            "claude-sonnet-4"
        );
    }

    // ── Health tracker tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn health_tracker_degrades_after_threshold() {
        let config = HealthConfig {
            failure_threshold: 3,
            window_secs: 300,
            recovery_secs: 60,
        };
        let tracker = HealthTracker::new(config);

        assert!(!tracker.is_degraded("model-a").await);
        tracker.record_failure("model-a").await;
        tracker.record_failure("model-a").await;
        assert!(!tracker.is_degraded("model-a").await); // 2 < 3
        tracker.record_failure("model-a").await;
        assert!(tracker.is_degraded("model-a").await); // 3 >= 3
    }

    #[tokio::test]
    async fn health_tracker_success_clears_degraded() {
        let config = HealthConfig {
            failure_threshold: 1,
            window_secs: 300,
            recovery_secs: 300,
        };
        let tracker = HealthTracker::new(config);

        tracker.record_failure("model-a").await;
        assert!(tracker.is_degraded("model-a").await);

        tracker.record_success("model-a").await;
        assert!(!tracker.is_degraded("model-a").await);
    }

    #[tokio::test]
    async fn health_status_snapshot() {
        let config = HealthConfig {
            failure_threshold: 5,
            window_secs: 300,
            recovery_secs: 60,
        };
        let tracker = HealthTracker::new(config);

        tracker.record_failure("model-a").await;
        tracker.record_failure("model-a").await;
        tracker.record_success("model-b").await;

        let status = tracker.status().await;
        assert_eq!(status.get("model-a").unwrap().recent_failures, 2);
        assert!(!status.get("model-a").unwrap().is_degraded);
    }

    // ── Failover provider tests ──────────────────────────────────────────

    /// Mock provider that always succeeds.
    struct MockOkProvider {
        response: String,
    }

    impl MockOkProvider {
        fn new(text: &str) -> Self {
            Self {
                response: text.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockOkProvider {
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
                    text: self.response.clone(),
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

    /// Mock provider that fails N times then succeeds.
    struct MockFailThenSucceed {
        fail_count: AtomicU32,
        target_failures: u32,
        error_message: String,
    }

    impl MockFailThenSucceed {
        fn new(failures: u32, error: &str) -> Self {
            Self {
                fail_count: AtomicU32::new(0),
                target_failures: failures,
                error_message: error.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockFailThenSucceed {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let count = self.fail_count.fetch_add(1, Ordering::SeqCst);
            if count < self.target_failures {
                Err(eyre::eyre!("{}", self.error_message))
            } else {
                Ok(LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "recovered".to_string(),
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
    }

    /// Mock provider that always fails.
    struct MockAlwaysFail {
        error_message: String,
    }

    impl MockAlwaysFail {
        fn new(error: &str) -> Self {
            Self {
                error_message: error.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockAlwaysFail {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("{}", self.error_message))
        }
    }

    /// Mock provider that records which model was used.
    struct MockModelRecorder {
        calls: Arc<RwLock<Vec<String>>>,
    }

    impl MockModelRecorder {
        fn new() -> (Self, Arc<RwLock<Vec<String>>>) {
            let calls = Arc::new(RwLock::new(Vec::new()));
            (
                Self {
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl LlmProvider for MockModelRecorder {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let model_name = model.unwrap_or("none").to_string();
            self.calls.write().await.push(model_name.clone());
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                model: model_name,
            })
        }
    }

    #[tokio::test]
    async fn passthrough_when_no_failover() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("hello")),
            FailoverConfig::default(),
        );

        let result = provider
            .create_message(None, &[], &[], Some("claude-sonnet-4"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("hello".to_string()));
    }

    #[tokio::test]
    async fn retry_succeeds_on_transient_error() {
        let inner = Arc::new(MockFailThenSucceed::new(
            2,
            "Anthropic API error (500): internal server error",
        ));

        let config = FailoverConfig {
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 10, // Fast for tests
                backoff_multiplier: 1.0,
                max_backoff_ms: 100,
                ..Default::default()
            },
            ..Default::default()
        };

        let provider = FailoverProvider::new(inner, config);

        let result = provider
            .create_message(None, &[], &[], Some("test-model"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("recovered".to_string()));
    }

    #[tokio::test]
    async fn failover_to_next_model() {
        let (_recorder, _calls) = MockModelRecorder::new();
        // First call will fail (we'll wrap with a provider that fails for specific models)
        // For simplicity, use MockFailThenSucceed — it fails the first call regardless of model
        let inner = Arc::new(MockFailThenSucceed::new(
            4, // Fail 4 times (exhaust 3 retries on first model, then succeed on second)
            "Anthropic API error (500): server error",
        ));

        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
                max_backoff_ms: 10,
                ..Default::default()
            },
            ..Default::default()
        };

        let provider = FailoverProvider::new(inner, config);

        let result = provider
            .create_message(None, &[], &[], Some("primary-model"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("recovered".to_string()));
    }

    #[tokio::test]
    async fn client_error_not_retried() {
        let inner = Arc::new(MockAlwaysFail::new(
            "Anthropic API error (400): invalid request - messages: text content blocks must be non-empty",
        ));

        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };

        let provider = FailoverProvider::new(inner, config);

        // Should fail immediately without retry or failover
        let result = provider
            .create_message(None, &[], &[], Some("test-model"), None, true)
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("400"));
    }

    #[tokio::test]
    async fn overloaded_skips_retries_and_failover() {
        let inner = Arc::new(MockFailThenSucceed::new(
            1, // Fail first call with 529, second should succeed
            "529 overloaded",
        ));

        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };

        let provider = FailoverProvider::new(inner, config);

        let result = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .unwrap();

        // Should have skipped retries on primary and succeeded on backup
        assert_eq!(result.text(), Some("recovered".to_string()));
    }

    #[test]
    fn failure_action_overloaded_failover_by_default() {
        let retry = RetryConfig::default();
        assert_eq!(
            failure_action(&ErrorKind::Overloaded, &retry, 0, None),
            FailureAction::Failover
        );
    }

    #[test]
    fn failure_action_overloaded_retries_when_enabled() {
        let retry = RetryConfig {
            max_retries: 3,
            retry_overloaded: true,
            ..Default::default()
        };
        assert_eq!(
            failure_action(&ErrorKind::Overloaded, &retry, 0, None),
            FailureAction::Retry
        );
        assert_eq!(
            failure_action(&ErrorKind::Overloaded, &retry, 2, None),
            FailureAction::Retry
        );
        assert_eq!(
            failure_action(&ErrorKind::Overloaded, &retry, 3, None),
            FailureAction::Failover
        );
    }

    #[test]
    fn failure_action_immediate_failover_kinds_ignore_overloaded_flag() {
        let retry = RetryConfig {
            retry_overloaded: true,
            ..Default::default()
        };
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 0, None),
            FailureAction::Failover
        );
        assert_eq!(
            failure_action(&ErrorKind::Auth, &retry, 0, None),
            FailureAction::Failover
        );
        assert_eq!(
            failure_action(&ErrorKind::ClientError, &retry, 0, None),
            FailureAction::Failover
        );
    }

    /// 429 with the gate on: short hint sleeps it out on the same model,
    /// long hint and hint-less both fail over, and max_retries still caps.
    #[test]
    fn failure_action_rate_limited_retries_short_hints_when_enabled() {
        let retry = RetryConfig {
            max_retries: 3,
            retry_rate_limited: true,
            max_rate_limit_wait_secs: 70,
            ..Default::default()
        };
        // Short hint → sleep hint+1s (outlasts the pool cooldown armed from
        // the same hint) and retry the same model.
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 0, Some(14)),
            FailureAction::RetryAfter(15)
        );
        // Hint at the cap still retries.
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 0, Some(70)),
            FailureAction::RetryAfter(71)
        );
        // Hint over the cap → not worth blocking the turn; fail over.
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 0, Some(71)),
            FailureAction::Failover
        );
        // No hint → nothing principled to sleep on; fail over.
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 0, None),
            FailureAction::Failover
        );
        // Retries exhausted → fail over even with a short hint.
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 3, Some(14)),
            FailureAction::Failover
        );
    }

    /// Gate off (default): 429 fails over immediately regardless of hint.
    #[test]
    fn failure_action_rate_limited_failover_by_default() {
        let retry = RetryConfig::default();
        assert_eq!(
            failure_action(&ErrorKind::RateLimited, &retry, 0, Some(5)),
            FailureAction::Failover
        );
    }

    #[test]
    fn failure_action_abort_and_compact() {
        let retry = RetryConfig::default();
        assert_eq!(
            failure_action(&ErrorKind::Billing, &retry, 0, None),
            FailureAction::Abort
        );
        assert_eq!(
            failure_action(&ErrorKind::ContextOverflow, &retry, 0, None),
            FailureAction::Compact
        );
    }

    #[test]
    fn failure_action_server_error_retries_then_failover() {
        let retry = RetryConfig {
            max_retries: 2,
            ..Default::default()
        };
        assert_eq!(
            failure_action(&ErrorKind::ServerError, &retry, 0, None),
            FailureAction::Retry
        );
        assert_eq!(
            failure_action(&ErrorKind::ServerError, &retry, 1, None),
            FailureAction::Retry
        );
        assert_eq!(
            failure_action(&ErrorKind::ServerError, &retry, 2, None),
            FailureAction::Failover
        );
    }

    #[tokio::test]
    async fn retry_overloaded_retries_same_model() {
        let inner = Arc::new(MockFailThenSucceed::new(1, "529 overloaded"));
        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                retry_overloaded: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config);

        let result = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("recovered".to_string()));
        // Retried the primary rather than spilling onto the fallback.
        assert_eq!(provider.last_used_model().as_deref(), Some("primary"));
    }

    #[tokio::test]
    async fn retry_overloaded_retries_same_model_stream() {
        let inner = Arc::new(MockFailThenSucceed::new(1, "529 overloaded"));
        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                retry_overloaded: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config);
        let (tx, mut rx) = mpsc::channel(32);

        provider
            .create_message_stream(None, &[], &[], Some("primary"), None, true, tx)
            .await
            .unwrap();

        let mut saw_done = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, StreamEvent::MessageDone { .. }) {
                saw_done = true;
            }
        }
        assert!(saw_done);
        assert_eq!(provider.last_used_model().as_deref(), Some("primary"));
    }

    /// The rate-limit sleep must be visible: a `RateLimitWait` event goes
    /// out on the stream before the retry, so the runner can surface
    /// `ProgressEvent::RateLimited` instead of a silent "Thinking…" hang.
    #[tokio::test]
    async fn rate_limit_retry_emits_wait_event_on_stream() {
        let inner = Arc::new(MockFailThenSucceed::new(
            1,
            "429 too many requests [retry-after: 0s]",
        ));
        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                retry_rate_limited: true,
                max_rate_limit_wait_secs: 70,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config);
        let (tx, mut rx) = mpsc::channel(32);

        provider
            .create_message_stream(None, &[], &[], Some("primary"), None, true, tx)
            .await
            .unwrap();

        let (mut saw_wait, mut saw_done) = (false, false);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                StreamEvent::RateLimitWait {
                    wait_secs: 1,
                    attempt: 1,
                } => saw_wait = true,
                StreamEvent::MessageDone { .. } => saw_done = true,
                _ => {}
            }
        }
        assert!(saw_wait, "RateLimitWait must precede the retry");
        assert!(saw_done, "retry must complete on the same model");
        assert_eq!(provider.last_used_model().as_deref(), Some("primary"));
    }

    #[tokio::test]
    async fn set_retry_round_trip() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        );
        let next = RetryConfig {
            max_retries: 7,
            initial_backoff_ms: 250,
            backoff_multiplier: 3.0,
            max_backoff_ms: 5000,
            retry_overloaded: true,
            retry_rate_limited: true,
            max_rate_limit_wait_secs: 45,
            timeouts: LlmTimeouts {
                first_token_timeout_secs: 5,
                ..Default::default()
            },
        };
        provider.set_retry(next.clone()).await;
        assert_eq!(provider.get_retry().await, next);
    }

    #[tokio::test]
    async fn set_retry_takes_effect_on_next_request() {
        let inner = Arc::new(MockFailThenSucceed::new(1, "529 overloaded"));
        let config = FailoverConfig {
            failover: vec!["backup-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config);
        assert!(!provider.get_retry().await.retry_overloaded);

        provider
            .set_retry(RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                retry_overloaded: true,
                ..Default::default()
            })
            .await;
        assert!(provider.get_retry().await.retry_overloaded);

        let result = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("recovered".to_string()));
        assert_eq!(provider.last_used_model().as_deref(), Some("primary"));
    }

    #[tokio::test]
    async fn retry_overloaded_records_single_health_failure() {
        let inner = Arc::new(MockAlwaysFail::new("529 overloaded"));
        let config = FailoverConfig {
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                retry_overloaded: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config);

        assert!(provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .is_err());

        let status = provider.health_tracker().status().await;
        assert_eq!(
            status.get("primary").map(|s| s.recent_failures),
            Some(1),
            "an overload burst that exhausts retries must record one health failure, not one per attempt"
        );
    }

    #[tokio::test]
    async fn alias_resolved_in_call() {
        let (recorder, calls) = MockModelRecorder::new();

        let config = FailoverConfig {
            aliases: [("fast".to_string(), "claude-3-5-haiku-20241022".to_string())].into(),
            ..Default::default()
        };

        let provider = FailoverProvider::new(Arc::new(recorder), config);

        let _result = provider
            .create_message(None, &[], &[], Some("fast"), None, true)
            .await
            .unwrap();

        let recorded = calls.read().await;
        assert_eq!(recorded[0], "claude-3-5-haiku-20241022");
    }

    #[tokio::test]
    async fn model_chain_resolves_tier_suffix_in_primary() {
        // Regression: incident 2026-05-18 — an unresolved tier name
        // "openrouter:deepseek:powerful" reached the OpenRouter API and 400'd.
        // The failover layer must run the requested model through tier resolution
        // so callers (lesson_extract_model, passive_model, session overrides, etc.)
        // don't have to remember to pre-resolve.
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        )
        .with_tier_registry(super::super::tiers::TierRegistry::new(None));

        let chain = provider
            .model_chain(Some("openrouter:deepseek:powerful"))
            .await;

        assert!(
            !chain.is_empty(),
            "chain should contain at least the resolved primary"
        );
        let primary = &chain[0];
        assert_ne!(
            primary.model, "deepseek:powerful",
            "tier suffix must be resolved; got raw model {:?}",
            primary.model
        );
        assert_eq!(primary.provider_key.as_deref(), Some("openrouter"));
    }

    #[tokio::test]
    async fn model_chain_passes_concrete_model_through_unchanged() {
        // A concrete model ID must not be mangled by the tier-resolver pass.
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        )
        .with_tier_registry(super::super::tiers::TierRegistry::new(None));

        let chain = provider.model_chain(Some("claude-sonnet-4-5")).await;

        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].model, "claude-sonnet-4-5");
        assert!(chain[0].provider_key.is_none());
    }

    #[tokio::test]
    async fn a_smaller_fallback_does_not_narrow_the_context_floor() {
        let with_small_fallback = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig {
                failover: vec!["zai.glm-5".to_string()],
                ..Default::default()
            },
        );
        assert_eq!(
            with_small_fallback.context_floor("claude-sonnet-4-6").await,
            super::super::model_limits::context_for("claude-sonnet-4-6")
        );
    }

    #[tokio::test]
    async fn degraded_model_pushed_to_end() {
        let config = FailoverConfig {
            failover: vec!["backup".to_string()],
            health: HealthConfig {
                failure_threshold: 1,
                window_secs: 300,
                recovery_secs: 300,
            },
            ..Default::default()
        };

        let provider = FailoverProvider::new(Arc::new(MockOkProvider::new("ok")), config);

        // Degrade the primary
        provider.health.record_failure("primary").await;
        assert!(provider.health.is_degraded("primary").await);

        // Chain should put backup first
        let chain = provider.model_chain(Some("primary")).await;
        let chain_raw: Vec<&str> = chain.iter().map(|e| e.raw.as_str()).collect();
        assert_eq!(chain_raw, vec!["backup", "primary"]);
    }

    #[tokio::test]
    async fn all_models_exhausted_returns_error() {
        let inner = Arc::new(MockAlwaysFail::new(
            "Anthropic API error (500): server error",
        ));

        let config = FailoverConfig {
            failover: vec!["backup1".to_string(), "backup2".to_string()],
            retry: RetryConfig {
                max_retries: 0, // No retries, just failover
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };

        let provider = FailoverProvider::new(inner, config);

        let result = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn failover_config_defaults() {
        let config = FailoverConfig::default();
        assert!(config.failover.is_empty());
        assert!(config.aliases.is_empty());
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.retry.initial_backoff_ms, 1000);
        assert_eq!(config.health.failure_threshold, 3);
        assert_eq!(config.health.window_secs, 300);
        assert_eq!(config.health.recovery_secs, 120);
    }

    #[test]
    fn failover_config_has_failover() {
        assert!(!FailoverConfig::default().has_failover());
        let config = FailoverConfig {
            failover: vec!["backup".to_string()],
            ..Default::default()
        };
        assert!(config.has_failover());
    }

    // ── Subscription and unsupported-request classification ─────────────

    fn subscription_limit_error() -> String {
        format!(
            "{SUBSCRIPTION_UNAVAILABLE_MARKER}: Alice's ChatGPT plan is at its limit: \
             ChatGPT API error (429): usage limit reached"
        )
    }

    #[test]
    fn subscription_marker_beats_429_status_in_classify() {
        assert_eq!(
            classify_error(&subscription_limit_error()),
            ErrorKind::SubscriptionUnavailable
        );
        assert_eq!(
            classify_error(&format!(
                "{UNSUPPORTED_REQUEST_MARKER}: gpt-5.6-sol takes no function tools \
                 (ChatGPT API error (400): bad request)"
            )),
            ErrorKind::Unsupported
        );
        assert_eq!(
            failure_action(
                &ErrorKind::SubscriptionUnavailable,
                &RetryConfig {
                    retry_rate_limited: true,
                    ..Default::default()
                },
                0,
                Some(1)
            ),
            FailureAction::Failover
        );
    }

    #[tokio::test]
    async fn per_user_limit_records_no_model_health_failure() {
        let provider = FailoverProvider::new(
            Arc::new(MockAlwaysFail::new(&subscription_limit_error())),
            FailoverConfig {
                failover: vec!["backup".to_string()],
                ..Default::default()
            },
        );

        assert!(provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .is_err());
        let (tx, _rx) = mpsc::channel(8);
        assert!(provider
            .create_message_stream(None, &[], &[], Some("primary"), None, true, tx)
            .await
            .is_err());

        let status = provider.health_tracker().status().await;
        for model in ["primary", "backup"] {
            assert_eq!(
                status.get(model).map_or(0, |s| s.recent_failures),
                0,
                "{model}: one user's plan limit must not degrade the model"
            );
        }
    }

    #[tokio::test]
    async fn unsupported_aborts_without_failover() {
        let (recorder, calls) = MockModelRecorder::new();
        let provider = FailoverProvider::new(
            Arc::new(MockAlwaysFail::new(&format!(
                "{UNSUPPORTED_REQUEST_MARKER}: claude-code serves tool-less completions"
            ))),
            FailoverConfig {
                failover: vec!["grok:backup".to_string()],
                ..Default::default()
            },
        );
        provider
            .add_external_provider("grok", Arc::new(recorder))
            .await;

        let err = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .expect_err("an unsupported request aborts");
        assert!(err.to_string().contains(UNSUPPORTED_REQUEST_MARKER));
        assert!(
            calls.read().await.is_empty(),
            "no failover after Unsupported"
        );
        assert_eq!(
            provider
                .health_tracker()
                .status()
                .await
                .get("primary")
                .map_or(0, |s| s.recent_failures),
            0
        );
    }

    /// Streams part of an answer, then fails the way a subscription login
    /// at its limit fails mid-stream.
    struct PartialThenUnavailable;

    #[async_trait]
    impl LlmProvider for PartialThenUnavailable {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            unreachable!("streamed only")
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            tx.send(StreamEvent::ContentStart { index: 0 }).await?;
            tx.send(StreamEvent::TextDelta {
                index: 0,
                text: "partial".into(),
            })
            .await?;
            Err(eyre::eyre!(
                "{SUBSCRIPTION_UNAVAILABLE_MARKER}: the login is at its usage limit"
            ))
        }
    }

    #[tokio::test]
    async fn a_stream_failing_after_content_aborts_instead_of_failing_over() {
        let (recorder, calls) = MockModelRecorder::new();
        let provider = FailoverProvider::new(
            Arc::new(recorder),
            FailoverConfig {
                failover: vec!["backup-model".to_string()],
                ..Default::default()
            },
        );
        provider
            .add_external_provider("chatgpt", Arc::new(PartialThenUnavailable))
            .await;

        let (tx, mut rx) = mpsc::channel(32);
        let err = provider
            .create_message_stream(None, &[], &[], Some("chatgpt:gpt-6.1-sol"), None, true, tx)
            .await
            .expect_err("the partial answer can't be completed elsewhere");
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        assert!(
            calls.read().await.is_empty(),
            "no second stream on the same channel"
        );
        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::TextDelta { text, .. } = event {
                texts.push(text);
            }
        }
        assert_eq!(texts, ["partial"]);
    }

    #[tokio::test]
    async fn unregistered_chatgpt_external_never_reroutes_to_primary() {
        let (recorder, calls) = MockModelRecorder::new();
        let provider = FailoverProvider::new(
            Arc::new(recorder),
            FailoverConfig {
                failover: vec!["chatgpt:gpt-5.4".to_string()],
                ..Default::default()
            },
        )
        .with_hub_default_model("chatgpt:gpt-5.2");

        let err = provider
            .create_message(None, &[], &[], Some("chatgpt:gpt-6.1-sol"), None, true)
            .await
            .expect_err("no chatgpt provider is registered");
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        let (tx, _rx) = mpsc::channel(8);
        assert!(provider
            .create_message_stream(None, &[], &[], Some("chatgpt:gpt-6.1-sol"), None, true, tx)
            .await
            .is_err());
        assert!(
            calls.read().await.is_empty(),
            "a chatgpt route must never reach the metered primary"
        );

        let (chatgpt, routed) = MockModelRecorder::new();
        provider
            .add_external_provider("chatgpt", Arc::new(chatgpt))
            .await;
        provider
            .create_message(None, &[], &[], Some("chatgpt:gpt-6.1-sol"), None, true)
            .await
            .unwrap();
        assert_eq!(*routed.read().await, ["gpt-6.1-sol"]);
        assert!(calls.read().await.is_empty());
    }

    // ── Chain entry context_limit tests ──────────────────────────────────

    /// The chain `requested` then `failover` resolves to, primary on a
    /// provider that reports nominal model windows.
    async fn chain_of(requested: &str, failover: &[&str]) -> Vec<ChainSpec> {
        FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig {
                failover: failover.iter().map(|m| m.to_string()).collect(),
                ..Default::default()
            },
        )
        .model_chain(Some(requested))
        .await
    }

    #[tokio::test]
    async fn chain_entry_context_limit_for_known_models() {
        let chain = chain_of("grok:grok-4", &["gemini:gemini-2.5-pro-preview-05-06"]).await;
        assert_eq!(chain[0].model, "grok-4");
        assert_eq!(chain[0].provider_key, Some("grok".to_string()));
        assert_eq!(chain[0].context_limit, Some(131_072));
        assert_eq!(chain[1].context_limit, Some(1_000_000));
        assert_eq!(
            chain_of("claude-opus-4-20250514", &[]).await[0].context_limit,
            Some(200_000)
        );
    }

    #[tokio::test]
    async fn chain_entry_context_limit_none_for_unknown_model() {
        assert!(chain_of("llama-3-70b", &[]).await[0]
            .context_limit
            .is_none());
    }

    #[test]
    fn parse_chain_entry_keeps_bedrock_id_whole() {
        // The "-v1:0" version suffix is not a provider prefix — the 2026-08-18
        // incident split it here and invoked Bedrock with model "0".
        let entry = parse_chain_entry("anthropic.claude-haiku-4-5-20251001-v1:0");
        assert_eq!(entry.provider_key, None);
        assert_eq!(entry.model, "anthropic.claude-haiku-4-5-20251001-v1:0");

        let routed = parse_chain_entry("anthropic:claude-haiku-4-5");
        assert_eq!(routed.provider_key.as_deref(), Some("anthropic"));
        assert_eq!(routed.model, "claude-haiku-4-5");
    }

    #[test]
    fn parse_chain_entry_unknown_prefix_passes_through_whole() {
        let entry = parse_chain_entry("llama3:8b");
        assert_eq!(entry.provider_key, None);
        assert_eq!(entry.model, "llama3:8b");
    }

    #[tokio::test]
    async fn chain_entry_context_limit_ordering() {
        let entries = chain_of(
            "claude-opus-4-20250514",
            &["grok:grok-4", "gemini:gemini-2.5-pro"],
        )
        .await;

        assert_eq!(entries[0].context_limit, Some(200_000));
        assert_eq!(entries[1].context_limit, Some(131_072));
        assert_eq!(entries[2].context_limit, Some(1_000_000));

        // Verify escalation logic: from Grok, Gemini is a valid target
        let grok_limit = entries[1].context_limit.unwrap_or(0);
        let has_larger = entries[2..]
            .iter()
            .any(|e| e.context_limit.unwrap_or(0) > grok_limit);
        assert!(has_larger);
    }

    #[tokio::test]
    async fn no_escalation_from_largest_model() {
        let entries = chain_of("grok:grok-4", &["gemini:gemini-2.5-pro"]).await;

        let gemini_limit = entries[1].context_limit.unwrap_or(0);
        let has_larger = entries[2..]
            .iter()
            .any(|e| e.context_limit.unwrap_or(0) > gemini_limit);
        assert!(!has_larger);
    }

    /// A provider whose window is its own, not the model's nominal one — a
    /// subscription backend serving `gpt-6.1-sol` at 272K where the model table
    /// says 400K.
    struct FlooredProvider(u32);

    #[async_trait]
    impl LlmProvider for FlooredProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("unused"))
        }

        async fn context_floor(&self, _model: &str) -> Option<u32> {
            Some(self.0)
        }
    }

    #[tokio::test]
    async fn chain_context_limit_uses_provider_floor_272k() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig {
                failover: vec!["chatgpt:gpt-6.1-sol".to_string()],
                ..Default::default()
            },
        );
        provider
            .add_external_provider("chatgpt", Arc::new(FlooredProvider(272_000)))
            .await;

        let chain = provider.model_chain(Some("claude-sonnet-4-6")).await;
        assert_eq!(chain[1].raw, "chatgpt:gpt-6.1-sol");
        assert_eq!(chain[1].provider_key.as_deref(), Some("chatgpt"));
        assert_eq!(chain[1].context_limit, Some(272_000));
        assert_eq!(
            chain[0].context_limit,
            super::super::model_limits::context_for("claude-sonnet-4-6")
        );
    }

    #[tokio::test]
    async fn a_routed_spec_takes_its_providers_window() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        );
        provider
            .add_external_provider("chatgpt", Arc::new(FlooredProvider(272_000)))
            .await;

        assert_eq!(
            provider.context_floor("chatgpt:gpt-6.1-sol").await,
            Some(272_000)
        );
        assert_eq!(
            provider.context_floor("claude-sonnet-4-6").await,
            super::super::model_limits::context_for("claude-sonnet-4-6")
        );
        let limit = |model, configured| {
            super::super::model_limits::context_limit(&provider, model, configured)
        };
        assert_eq!(limit("chatgpt:gpt-6.1-sol", 400_000).await, 272_000);
        assert_eq!(limit("chatgpt:gpt-6.1-sol", 200_000).await, 200_000);
        assert_eq!(limit("llama-3-70b", 250_000).await, 250_000);
    }

    // ── Hub default last-resort fallback tests ──────────────────────────

    /// Mock provider that fails for any model except a designated "good" one.
    /// Lets us assert the last-resort fallback picks the hub default after
    /// every chain entry blows up.
    struct MockOnlyOkOnModel {
        good_model: String,
        error: String,
    }

    #[async_trait]
    impl LlmProvider for MockOnlyOkOnModel {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            if model == Some(self.good_model.as_str()) {
                Ok(LlmResponse {
                    content: vec![ContentBlock::Text {
                        text: "hub-default-rescue".to_string(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        ..Default::default()
                    },
                    model: self.good_model.clone(),
                })
            } else {
                Err(eyre::eyre!("{}", self.error))
            }
        }
    }

    #[tokio::test]
    async fn hub_default_rescues_after_chain_exhausted() {
        let inner = Arc::new(MockOnlyOkOnModel {
            good_model: "hub-fallback".to_string(),
            error: "Anthropic API error (500): server error".to_string(),
        });
        let config = FailoverConfig {
            failover: vec!["broken-backup".to_string()],
            retry: RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config).with_hub_default_model("hub-fallback");

        let result = provider
            .create_message(None, &[], &[], Some("broken-primary"), None, true)
            .await
            .unwrap();

        assert_eq!(result.text(), Some("hub-default-rescue".to_string()));
        assert_eq!(provider.last_used_model().as_deref(), Some("hub-fallback"));
    }

    #[tokio::test]
    async fn hub_default_skipped_when_already_in_chain() {
        let inner = Arc::new(MockAlwaysFail::new(
            "Anthropic API error (500): server error",
        ));
        let config = FailoverConfig {
            failover: vec!["chain-entry".to_string()],
            retry: RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        // hub_default = "chain-entry" — already in chain, must not double-up.
        let provider = FailoverProvider::new(inner, config).with_hub_default_model("chain-entry");

        let err = provider
            .create_message(None, &[], &[], Some("chain-entry"), None, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("server error"));
    }

    #[tokio::test]
    async fn hub_default_failure_propagates_original_error() {
        let inner = Arc::new(MockAlwaysFail::new(
            "Anthropic API error (500): server error",
        ));
        let config = FailoverConfig {
            failover: vec!["backup".to_string()],
            retry: RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config).with_hub_default_model("hub-default");

        let err = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("server error"));
    }

    #[tokio::test]
    async fn hub_default_noop_when_unset() {
        let inner = Arc::new(MockAlwaysFail::new(
            "Anthropic API error (500): server error",
        ));
        let config = FailoverConfig {
            failover: vec!["backup".to_string()],
            retry: RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let provider = FailoverProvider::new(inner, config);

        let err = provider
            .create_message(None, &[], &[], Some("primary"), None, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("server error"));
    }

    // ── External-provider credential probe tests ────────────────────────

    /// Provider whose `verify_credentials` returns the supplied error verbatim.
    /// Lets us assert the probe classifies and acts on auth vs non-auth.
    /// Accepts the call and never answers — the shape of the outage that
    /// stopped the hub starting.
    struct MockProbeHangs;

    #[async_trait]
    impl LlmProvider for MockProbeHangs {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("not used"))
        }

        async fn verify_credentials(&self) -> Result<()> {
            std::future::pending::<()>().await;
            Ok(())
        }
    }

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

    /// The failure this timeout exists for: a provider that accepts the
    /// connection and then never answers. Startup runs this before the server
    /// binds, so an unbounded probe stopped the hub coming up at all and rolled
    /// back every deploy until the provider recovered.
    ///
    /// `start_paused` advances the clock as soon as everything is waiting on a
    /// timer, so this proves the timeout is what ends it — the test would hang
    /// forever without one.
    #[tokio::test(start_paused = true)]
    async fn a_provider_that_never_answers_does_not_wedge_startup() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        );
        provider
            .add_external_provider("hangs", Arc::new(MockProbeHangs))
            .await;
        provider
            .add_external_provider("ok-provider", Arc::new(MockOkProvider::new("alive")))
            .await;

        provider.probe_all_external_providers().await;

        // Kept, not dropped: no answer is not the same as a bad key, and
        // dropping a working provider costs more than one wasted round-trip.
        let listed = provider.list_external_providers().await;
        assert!(
            listed.contains(&"hangs".to_string()),
            "a provider that timed out should still be available"
        );
        assert!(listed.contains(&"ok-provider".to_string()));
    }

    #[tokio::test]
    async fn probe_external_drops_auth_failed_providers() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        );
        provider
            .add_external_provider(
                "bad-auth",
                Arc::new(MockProbeFail {
                    error: "Anthropic API error (401 Unauthorized): Invalid bearer token".into(),
                }),
            )
            .await;
        provider
            .add_external_provider("ok-provider", Arc::new(MockOkProvider::new("alive")))
            .await;

        provider.probe_all_external_providers().await;

        let listed = provider.list_external_providers().await;
        assert!(listed.contains(&"ok-provider".to_string()));
        assert!(!listed.contains(&"bad-auth".to_string()));
    }

    #[tokio::test]
    async fn probe_external_keeps_non_auth_failures() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig::default(),
        );
        provider
            .add_external_provider(
                "flaky-upstream",
                Arc::new(MockProbeFail {
                    error: "Anthropic API error (500): server error".into(),
                }),
            )
            .await;

        provider.probe_all_external_providers().await;

        let listed = provider.list_external_providers().await;
        assert!(listed.contains(&"flaky-upstream".to_string()));
    }

    // ─── Liveness watchdog ───────────────────────────────────────────────────

    #[test]
    fn classify_watchdog_markers_before_generic_arms() {
        // Both messages contain words ("timeout", "stream") the generic arms
        // would otherwise claim.
        assert_eq!(
            classify_error("llm first-token timeout: no first token after 45s (model m)"),
            ErrorKind::FirstTokenTimeout
        );
        assert_eq!(
            classify_error("llm stream stalled: no stream events for 180s (model m)"),
            ErrorKind::StreamStalled
        );
        assert!(ErrorKind::FirstTokenTimeout.should_failover_immediately());
        assert!(!ErrorKind::FirstTokenTimeout.is_retryable());
        assert!(ErrorKind::StreamStalled.should_abort());
    }

    #[test]
    fn llm_timeouts_exact_override_beats_substring() {
        let mut t = LlmTimeouts::default();
        t.model_timeouts.insert(
            "gemini".into(),
            TimeoutOverride {
                first_token_timeout_secs: Some(10),
                ..Default::default()
            },
        );
        t.model_timeouts.insert(
            "gemini-3.1-pro".into(),
            TimeoutOverride {
                first_token_timeout_secs: Some(20),
                ..Default::default()
            },
        );
        assert_eq!(
            t.stream_timeouts_for("gemini-3.1-pro").first_token,
            Duration::from_secs(20)
        );
    }

    #[test]
    fn llm_timeouts_substring_override_and_global_fallback() {
        let mut t = LlmTimeouts::default();
        t.model_timeouts.insert(
            "gemini-3.1".into(),
            TimeoutOverride {
                first_token_timeout_secs: Some(10),
                ..Default::default()
            },
        );
        let hit = t.stream_timeouts_for("gemini-3.1-pro-preview");
        assert_eq!(hit.first_token, Duration::from_secs(10));
        // Unset fields inherit the global value.
        assert_eq!(hit.stall, Duration::from_secs(default_stall_timeout_secs()));
        let miss = t.stream_timeouts_for("claude-sonnet-5");
        assert_eq!(
            miss.first_token,
            Duration::from_secs(default_first_token_timeout_secs())
        );
    }

    #[test]
    fn llm_timeouts_zero_disables_request_cap() {
        let t = LlmTimeouts {
            request_timeout_secs: 0,
            ..Default::default()
        };
        assert_eq!(t.request_timeout_for("any"), None);
    }

    /// External stream that never produces a first event.
    struct MockStreamHang {
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for MockStreamHang {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("not used"))
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(100_000)).await;
            Ok(())
        }
    }

    /// External stream that surfaces one event, then goes silent forever.
    struct MockStreamStall;

    #[async_trait]
    impl LlmProvider for MockStreamStall {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Err(eyre::eyre!("not used"))
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            let _ = tx.send(StreamEvent::ContentStart { index: 0 }).await;
            tokio::time::sleep(Duration::from_secs(100_000)).await;
            Ok(())
        }
    }

    /// Inner provider with a real streaming implementation and call counter.
    struct MockStreamOk {
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for MockStreamOk {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LlmResponse {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: model.unwrap_or("fallback").to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(StreamEvent::ContentStart { index: 0 }).await;
            let _ = tx
                .send(StreamEvent::TextDelta {
                    index: 0,
                    text: "ok".into(),
                })
                .await;
            let _ = tx.send(StreamEvent::ContentStop { index: 0 }).await;
            let _ = tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
                .await;
            Ok(())
        }
    }

    fn watchdog_chain_config() -> FailoverConfig {
        FailoverConfig {
            failover: vec!["good-model".to_string()],
            retry: RetryConfig {
                max_retries: 3,
                initial_backoff_ms: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn external_first_token_timeout_fails_over_once_and_reports() {
        let inner = Arc::new(MockStreamOk {
            calls: AtomicU32::new(0),
        });
        let provider = FailoverProvider::new(inner.clone(), watchdog_chain_config());
        let hang = Arc::new(MockStreamHang {
            calls: AtomicU32::new(0),
        });
        provider.add_external_provider("grok", hang.clone()).await;

        let (tx, mut rx) = mpsc::channel(32);
        provider
            .create_message_stream(None, &[], &[], Some("grok:hang-model"), None, false, tx)
            .await
            .unwrap();

        assert_eq!(
            hang.calls.load(Ordering::SeqCst),
            1,
            "first-token timeout must fail over immediately, not retry the hung model"
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        let mut saw_failover_signal = false;
        let mut saw_fallback_text = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                StreamEvent::SlowModelFailover { model, .. } => {
                    assert_eq!(model, "grok:hang-model");
                    saw_failover_signal = true;
                }
                StreamEvent::TextDelta { text, .. } if text == "ok" => {
                    saw_fallback_text = true;
                }
                _ => {}
            }
        }
        assert!(saw_failover_signal, "UI must learn why the model switched");
        assert!(saw_fallback_text, "fallback model's stream must flow");
    }

    /// The common production shape: the stall fires inside the pool (the
    /// chain entry has no provider prefix), and the marker error must
    /// bubble through the failover walk as an abort, not a model switch.
    #[tokio::test(start_paused = true)]
    async fn pool_stall_bubbles_through_failover_as_abort() {
        let pool = Arc::new(crate::llm::pool::PooledProvider::new());
        pool.add("default", Arc::new(MockStreamStall)).await;
        let provider = FailoverProvider::new(pool, watchdog_chain_config());

        let (tx, mut rx) = mpsc::channel(32);
        let err = provider
            .create_message_stream(None, &[], &[], Some("stall-model"), None, false, tx)
            .await
            .unwrap_err();

        assert!(err.to_string().contains(STREAM_STALL_MARKER));
        assert!(
            matches!(rx.try_recv(), Ok(StreamEvent::ContentStart { .. })),
            "the partial event reached the consumer before the stall"
        );
    }

    #[test]
    fn llm_timeouts_longest_substring_key_wins() {
        let mut t = LlmTimeouts::default();
        t.model_timeouts.insert(
            "gemini".into(),
            TimeoutOverride {
                first_token_timeout_secs: Some(99),
                ..Default::default()
            },
        );
        t.model_timeouts.insert(
            "gemini-3.1".into(),
            TimeoutOverride {
                first_token_timeout_secs: Some(10),
                ..Default::default()
            },
        );
        assert_eq!(
            t.stream_timeouts_for("gemini-3.1-pro-preview").first_token,
            Duration::from_secs(10)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn external_stall_after_first_event_aborts_chain() {
        let inner = Arc::new(MockStreamOk {
            calls: AtomicU32::new(0),
        });
        let provider = FailoverProvider::new(inner.clone(), watchdog_chain_config());
        provider
            .add_external_provider("grok", Arc::new(MockStreamStall))
            .await;

        let (tx, mut rx) = mpsc::channel(32);
        let err = provider
            .create_message_stream(None, &[], &[], Some("grok:stall-model"), None, false, tx)
            .await
            .unwrap_err();

        assert!(err.to_string().contains(STREAM_STALL_MARKER));
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            0,
            "a stalled stream already surfaced content — no silent failover"
        );
        assert!(
            matches!(rx.try_recv(), Ok(StreamEvent::ContentStart { .. })),
            "the partial event reached the consumer before the stall"
        );
    }

    /// Non-streaming externals get the request cap instead of the stream
    /// watchdog: a hung `create_message` classifies as first-token timeout
    /// (nothing surfaced) and fails over.
    struct MockCallHang {
        calls: AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for MockCallHang {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(100_000)).await;
            Err(eyre::eyre!("unreachable"))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn external_request_timeout_fails_over_non_streaming() {
        let inner = Arc::new(MockStreamOk {
            calls: AtomicU32::new(0),
        });
        let provider = FailoverProvider::new(inner.clone(), watchdog_chain_config());
        let hang = Arc::new(MockCallHang {
            calls: AtomicU32::new(0),
        });
        provider.add_external_provider("grok", hang.clone()).await;

        let response = provider
            .create_message(None, &[], &[], Some("grok:hang-model"), None, false)
            .await
            .unwrap();

        assert_eq!(response.text(), Some("ok".to_string()));
        assert_eq!(
            hang.calls.load(Ordering::SeqCst),
            1,
            "request timeout must fail over immediately, not retry the hung model"
        );
    }

    #[tokio::test]
    async fn resolve_chain_matches_model_chain_before_health_reorder() {
        let provider = FailoverProvider::new(
            Arc::new(MockOkProvider::new("ok")),
            FailoverConfig {
                failover: vec![
                    "best".to_string(),
                    "gemini".to_string(),
                    "chatgpt:gpt-6.1-sol".to_string(),
                    "anthropic:claude-opus-4-6".to_string(),
                    "claude-code:sonnet".to_string(),
                ],
                aliases: [("best".to_string(), "anthropic:claude-opus-4-6".to_string())].into(),
                retry: RetryConfig {
                    max_retries: 0,
                    ..Default::default()
                },
                health: HealthConfig {
                    failure_threshold: 1,
                    ..Default::default()
                },
            },
        );
        let raws = |chain: Vec<ChainSpec>| chain.into_iter().map(|e| e.raw).collect::<Vec<_>>();
        let resolved = raws(provider.resolve_chain(Some("chatgpt:gpt-6.1-sol")).await);
        assert_eq!(resolved[0], "chatgpt:gpt-6.1-sol");
        assert_eq!(resolved[1], "anthropic:claude-opus-4-6");
        assert!(resolved[2].starts_with("gemini:"), "{resolved:?}");
        assert_eq!(resolved[3], "claude-code:sonnet");
        assert_eq!(resolved.len(), 4, "repeats are dropped: {resolved:?}");
        assert_eq!(
            raws(provider.model_chain(Some("chatgpt:gpt-6.1-sol")).await),
            resolved,
            "with nothing degraded the two agree"
        );

        provider.health.record_failure("chatgpt:gpt-6.1-sol").await;
        assert_eq!(
            raws(provider.resolve_chain(Some("chatgpt:gpt-6.1-sol")).await),
            resolved,
            "health never reorders the configured chain"
        );
        let walked = raws(provider.model_chain(Some("chatgpt:gpt-6.1-sol")).await);
        assert_eq!(
            walked.last().map(String::as_str),
            Some("chatgpt:gpt-6.1-sol")
        );
        assert_eq!(walked[..3], resolved[1..]);
    }

    #[tokio::test]
    async fn runner_slice_never_tries_hub_default_or_cli_entries() {
        let (primary, hub_default_calls) = MockModelRecorder::new();
        let (cli, cli_calls) = MockModelRecorder::new();
        let (openrouter, openrouter_calls) = MockModelRecorder::new();
        let provider = Arc::new(
            FailoverProvider::new(
                Arc::new(primary),
                FailoverConfig {
                    failover: vec![
                        "chatgpt:gpt-6.1-sol".to_string(),
                        "claude-code:sonnet".to_string(),
                        "openrouter:openai/gpt-5".to_string(),
                    ],
                    retry: RetryConfig {
                        max_retries: 0,
                        initial_backoff_ms: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .with_hub_default_model("hub-default"),
        );
        provider
            .add_external_provider(
                "chatgpt",
                Arc::new(MockAlwaysFail::new("OpenAI API error (500): server error")),
            )
            .await;
        provider
            .add_external_provider("claude-code", Arc::new(cli))
            .await;
        provider
            .add_external_provider("openrouter", Arc::new(openrouter))
            .await;

        let chatgpt = provider.slice(vec!["chatgpt:gpt-6.1-sol".to_string()]);
        assert!(chatgpt
            .create_message(None, &[], &[], Some("claude-code:sonnet"), None, true)
            .await
            .is_err());
        let (tx, _rx) = mpsc::channel(8);
        assert!(chatgpt
            .create_message_stream(None, &[], &[], Some("chatgpt:gpt-6.1-sol"), None, true, tx)
            .await
            .is_err());
        assert!(
            !chatgpt.answered(),
            "nothing answered, so the route may hop"
        );
        assert!(
            hub_default_calls.read().await.is_empty(),
            "a slice never reaches for the hub default"
        );
        assert!(cli_calls.read().await.is_empty(), "nor for a CLI entry");

        let runner = provider.slice(vec![
            "chatgpt:gpt-6.1-sol".to_string(),
            "openrouter:openai/gpt-5".to_string(),
        ]);
        runner
            .create_message(None, &[], &[], Some("chatgpt:gpt-6.1-sol"), None, true)
            .await
            .expect("the slice's second entry answers");
        assert!(runner.answered());
        assert_eq!(*openrouter_calls.read().await, ["openai/gpt-5"]);
        assert!(hub_default_calls.read().await.is_empty());
        assert!(cli_calls.read().await.is_empty());
    }
}
