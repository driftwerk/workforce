use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::mpsc;
use workforce_common::settings::{is_removed_provider, REMOVED_PROVIDER_POINTER};
use workforce_common::subscription::SubscriptionRoute;

use super::anthropic::AnthropicClient;
use super::bedrock::{self, BedrockClient};
use super::bedrock_catalog::BedrockCatalogStore;
use super::bedrock_credentials::{CredentialResolver, CHAIN_SENTINEL};
use super::cerebras::CerebrasClient;
use super::gemini::GeminiClient;
use super::grok::GrokClient;
use super::ollama::OllamaClient;
use super::openrouter::OpenRouterClient;
use super::streaming::StreamEvent;
use super::types::{LlmMessage, LlmResponse, LlmToolDef};
use super::vertex::VertexAnthropicClient;
use super::vertex_auth::{LazyGcpAuthTokenSource, VertexTokenSource};
use super::vertex_credentials::{self, VertexCredentialSource};
use super::vllm::VllmClient;
use super::zai::ZaiClient;

static BEDROCK_CATALOG: OnceLock<Arc<BedrockCatalogStore>> = OnceLock::new();

/// Process-wide Bedrock catalog. First call lazily constructs the store using
/// the AWS default credential chain; subsequent calls return the cached `Arc`.
/// Returned to both the hub (validation, listing) and [`create_provider`]
/// (attached to each [`BedrockClient`] for 400-error enrichment).
pub fn bedrock_catalog() -> Arc<BedrockCatalogStore> {
    BEDROCK_CATALOG
        .get_or_init(|| BedrockCatalogStore::new(CredentialResolver::from_packed(CHAIN_SENTINEL)))
        .clone()
}

tokio::task_local! {
    /// When set, reasoning-capable providers drop extended `reasoning` /
    /// `effort` for every LLM call made within the scope.
    ///
    /// Set by background utility calls — passive memory & lesson extraction,
    /// classifiers, summaries — which want a direct JSON/short answer, not a
    /// chain of thought. Without it, some reasoning models (notably
    /// `deepseek-v3.2` via OpenRouter, routed through Novita/Friendli/etc.)
    /// spend the entire `max_tokens` budget on hidden reasoning and return a
    /// truncated or empty `content`, so the extraction silently yields nothing
    /// (incident 2026-05-30: passive/lesson capture went dark hub-wide after
    /// OpenRouter re-routed deepseek-v3.2 to providers that honour `effort`;
    /// a prod A/B confirmed reason=OFF extracts valid JSON 3/3 where reason=ON
    /// truncated 2/3 at `max_tokens=1024`).
    ///
    /// Modelled on `CURRENT_USER_ID`: an ambient flag that must reach deep
    /// into the provider stack (through the pool + failover wrappers) without
    /// threading a parameter through every `create_message` signature.
    /// Task-locals propagate across `.await` within the same task, and the
    /// provider stack never re-`spawn`s between caller and HTTP request, so the
    /// flag reliably reaches the request builder.
    static SUPPRESS_REASONING: bool;

    /// When set, replaces the client's configured `thinking_budget_tokens` for
    /// every LLM call made within the scope.
    ///
    /// The budget is otherwise baked into each provider client at construction
    /// (`with_thinking_budget`), so there is no other way to vary reasoning
    /// depth per turn. Set by the voice path, where time-to-first-word is the
    /// whole point and a deep chain of thought is dead air:
    /// `[llm] voice_thinking_budget_tokens`.
    ///
    /// `0` reads as "don't think" everywhere, via each provider's existing
    /// mapping — `minimal` on gemini-3.x, `none` on OpenAI-shaped `effort`,
    /// and below Anthropic's 1024-token floor, which disables thinking
    /// outright.
    ///
    /// Same ambient-flag rationale as [`SUPPRESS_REASONING`], with one extra
    /// hazard: the runner spawns the *streaming* call onto its own task
    /// (`runner.rs`, `create_message_stream`), and task-locals do not cross
    /// `tokio::spawn`. That spawn re-establishes this scope by hand — without
    /// it the override would vanish on the streaming path, which is the one
    /// voice actually uses.
    static THINKING_BUDGET_OVERRIDE: u32;

    /// Number of leading messages that recur byte-identically on the next
    /// turn (the replayed history). Set by the runner for round 1 of a turn
    /// with history so providers place the message breakpoint at the end of
    /// that prefix instead of on the varying task message: a cache read only
    /// lands on a position an earlier request wrote a breakpoint at, and the
    /// end of the replayed history is the one boundary the next turn sends
    /// again. Carried across the streaming spawn by hand, like
    /// [`THINKING_BUDGET_OVERRIDE`].
    static CACHED_PREFIX_LEN: usize;

    /// The request's catalogued tools, for a wire that defers them natively:
    /// every other wire sends only the declared `tools` it is handed. Set by
    /// the runner for every round of a turn that loads tools on demand, and
    /// carried across the streaming spawn by hand like [`CACHED_PREFIX_LEN`].
    static TOOL_DEFERRAL: ToolDeferral;
}

/// Catalogued tools a native wire declares deferred: those not yet loaded,
/// and which of the declared tools were loaded during the turn (declared
/// deferred too, so the tools array stays the same for the whole turn).
#[derive(Debug, Clone, Default)]
pub struct ToolDeferral {
    pub deferred: Arc<Vec<LlmToolDef>>,
    pub loaded: Arc<std::collections::BTreeSet<String>>,
}

/// The catalogued tools in scope, if the round defers any. See
/// [`TOOL_DEFERRAL`].
pub fn tool_deferral() -> Option<ToolDeferral> {
    TOOL_DEFERRAL.try_with(Clone::clone).ok()
}

/// Run `fut` with `deferral` in scope; `None` is a plain passthrough.
pub async fn with_tool_deferral<F>(deferral: Option<ToolDeferral>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    match deferral {
        Some(d) => TOOL_DEFERRAL.scope(d, fut).await,
        None => fut.await,
    }
}

/// True when the current task is running inside [`with_reasoning_suppressed`].
/// Reasoning-capable provider request builders consult this to drop `effort`.
pub fn reasoning_suppressed() -> bool {
    SUPPRESS_REASONING.try_with(|v| *v).unwrap_or(false)
}

/// Run `fut` with extended reasoning suppressed for every LLM call it makes.
/// See [`SUPPRESS_REASONING`]. Use for background extraction/summary calls
/// that want a direct answer rather than a chain of thought.
pub async fn with_reasoning_suppressed<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    SUPPRESS_REASONING.scope(true, fut).await
}

/// The per-turn thinking budget in scope, if any. See
/// [`THINKING_BUDGET_OVERRIDE`].
pub fn thinking_budget_override() -> Option<u32> {
    THINKING_BUDGET_OVERRIDE.try_with(|v| *v).ok()
}

/// Run `fut` with `budget` replacing the client's configured thinking budget.
/// `None` is a plain passthrough — no scope is established, so the common case
/// is byte-for-byte the behaviour without this feature.
pub async fn with_thinking_budget_override<F>(budget: Option<u32>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    match budget {
        Some(b) => THINKING_BUDGET_OVERRIDE.scope(b, fut).await,
        None => fut.await,
    }
}

/// The recurring message prefix declared for the round in scope, if any. See
/// [`CACHED_PREFIX_LEN`].
pub fn cached_prefix_len() -> Option<usize> {
    CACHED_PREFIX_LEN.try_with(|v| *v).ok()
}

/// Run `fut` with `len` leading messages declared as the recurring prefix.
/// `None` is a plain passthrough — no scope is established, so providers
/// keep the breakpoint on the last message.
pub async fn with_cached_prefix<F>(len: Option<usize>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    match len {
        Some(n) => CACHED_PREFIX_LEN.scope(n, fut).await,
        None => fut.await,
    }
}

/// The thinking budget a request builder should actually use, given the budget
/// baked into its client. Every provider routes its `thinking_budget_tokens`
/// read through this so the per-turn override reaches all of them alike.
///
/// Precedence: an active [`with_reasoning_suppressed`] scope wins, then the
/// per-turn override, then the client's own budget. Suppression comes first
/// because it exists to stop reasoning-happy models burning `max_tokens` on
/// hidden thought and returning nothing — a voice turn that kicks off a
/// passive extraction must not hand that call its reasoning back. In both
/// non-override cases this returns `configured` unchanged.
pub fn effective_thinking_budget(configured: Option<u32>) -> Option<u32> {
    match thinking_budget_override() {
        Some(_) if reasoning_suppressed() => configured,
        Some(budget) => Some(budget),
        None => configured,
    }
}

/// Unified LLM provider interface.
///
/// Abstracts over different LLM backends (Anthropic, OpenRouter, etc.)
/// so the agent runner doesn't need to know which one is being used.
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait LlmProvider: Send + Sync {
    /// Send a message and get a complete response (non-streaming).
    ///
    /// The `enable_cache` parameter controls whether prompt caching is enabled
    /// (currently only implemented for Anthropic).
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse>;

    /// Stream a message response, sending [`StreamEvent`]s to the provided channel.
    ///
    /// The `enable_cache` parameter controls whether prompt caching is enabled.
    /// The default implementation falls back to the non-streaming `create_message`
    /// and synthesises stream events from the complete response.
    /// Verify the provider credential is authentic.
    ///
    /// This is intentionally narrower than `create_message`: it must
    /// only confirm "the API key authenticates," not "a model can
    /// complete a request right now." Providers SHOULD override this
    /// to hit an auth-only endpoint (e.g. `GET /v1/models`) so a
    /// credential isn't rejected when an unrelated upstream is flaky.
    ///
    /// Default implementation falls back to a 1-token completion, which
    /// validates the key end-to-end but conflates auth with inference
    /// availability — fine for providers without a lighter endpoint
    /// (Bedrock, Vertex, Ollama, vLLM), wrong for SaaS providers with
    /// downstream routing (OpenRouter).
    /// The smallest context window a request for `model` can reach: a
    /// provider that fails over to other models reports the smallest of them.
    async fn context_floor(&self, model: &str) -> Option<u32> {
        super::model_limits::context_for(model)
    }

    async fn verify_credentials(&self) -> Result<()> {
        let test_msg = LlmMessage {
            role: super::types::Role::User,
            content: vec![super::types::ContentBlock::Text { text: "hi".into() }],
        };
        self.create_message(None, &[test_msg], &[], None, Some(1), false)
            .await
            .map(|_| ())
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
        // Default: non-streaming fallback — call create_message and emit synthetic events
        let response = self
            .create_message(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
            )
            .await?;

        use super::types::ContentBlock;
        for (i, block) in response.content.iter().enumerate() {
            match block {
                ContentBlock::Text { text } => {
                    let _ = tx.send(StreamEvent::ContentStart { index: i }).await;
                    let _ = tx
                        .send(StreamEvent::TextDelta {
                            index: i,
                            text: text.clone(),
                        })
                        .await;
                    let _ = tx.send(StreamEvent::ContentStop { index: i }).await;
                }
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    signature,
                } => {
                    let _ = tx
                        .send(StreamEvent::ToolUseStart {
                            index: i,
                            id: id.clone(),
                            name: name.clone(),
                            signature: signature.clone(),
                        })
                        .await;
                    let json = serde_json::to_string(input).unwrap_or_default();
                    let _ = tx
                        .send(StreamEvent::InputJsonDelta {
                            index: i,
                            partial: json,
                        })
                        .await;
                    let _ = tx.send(StreamEvent::ContentStop { index: i }).await;
                }
                ContentBlock::Thinking { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Document { .. } => {}
            }
        }

        let _ = tx
            .send(StreamEvent::MessageDone {
                stop_reason: response.stop_reason,
                usage: response.usage,
            })
            .await;

        Ok(())
    }
}

/// Decorator applying the liveness watchdog to any [`LlmProvider`].
///
/// The hub never needs it — its stack always contains a
/// [`PooledProvider`](super::pool::PooledProvider), which enforces the same
/// windows on every token attempt. Binaries that build a bare concrete
/// client (the CLI) wrap it in this so a hung provider still yields a
/// classifiable timeout error instead of an eternal silent await.
pub struct WatchdogProvider {
    inner: Box<dyn LlmProvider>,
    timeouts: super::failover::LlmTimeouts,
}

impl WatchdogProvider {
    pub fn new(inner: Box<dyn LlmProvider>, timeouts: super::failover::LlmTimeouts) -> Self {
        Self { inner, timeouts }
    }
}

#[async_trait]
impl LlmProvider for WatchdogProvider {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let model_str = model.unwrap_or("");
        super::streaming::call_with_request_cap(
            self.timeouts.request_timeout_for(model_str),
            model_str,
            self.inner.create_message(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
            ),
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
        let model_str = model.unwrap_or("");
        super::streaming::watchdogged_stream(
            self.timeouts.stream_timeouts_for(model_str),
            model_str,
            tx,
            |inner_tx| {
                self.inner.create_message_stream(
                    system_prompt,
                    messages,
                    tools,
                    model,
                    max_tokens,
                    enable_cache,
                    inner_tx,
                )
            },
        )
        .await
    }

    /// Delegate untouched: providers override this with lighter auth-only
    /// probes, and the default 1-token completion would otherwise run
    /// through the request cap twice.
    async fn verify_credentials(&self) -> Result<()> {
        self.inner.verify_credentials().await
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        self.inner.context_floor(model).await
    }
}

#[async_trait]
impl LlmProvider for AnthropicClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(
            system_prompt,
            messages,
            tools,
            model,
            max_tokens,
            enable_cache,
        )
        .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.verify_credentials_models_probe().await
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
        self.create_message_stream(
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
}

#[async_trait]
impl LlmProvider for OpenRouterClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(
            system_prompt,
            messages,
            tools,
            model,
            max_tokens,
            enable_cache,
        )
        .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.verify_credentials().await
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
        self.create_message_stream(
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
}

#[async_trait]
impl LlmProvider for GrokClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool, // Grok doesn't support caching
    ) -> Result<LlmResponse> {
        self.create_message(system_prompt, messages, tools, model, max_tokens)
            .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.verify_credentials().await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.create_message_stream(system_prompt, messages, tools, model, max_tokens, tx)
            .await
    }
}

#[async_trait]
impl LlmProvider for OllamaClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool, // Ollama doesn't support caching
    ) -> Result<LlmResponse> {
        self.create_message(system_prompt, messages, tools, model, max_tokens)
            .await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.create_message_stream(system_prompt, messages, tools, model, max_tokens, tx)
            .await
    }
}

#[async_trait]
impl LlmProvider for GeminiClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(
            system_prompt,
            messages,
            tools,
            model,
            max_tokens,
            enable_cache,
        )
        .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.verify_credentials().await
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
        self.create_message_stream(
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
}

#[async_trait]
impl LlmProvider for VllmClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool, // vLLM doesn't support Anthropic-style caching
    ) -> Result<LlmResponse> {
        self.create_message(system_prompt, messages, tools, model, max_tokens)
            .await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.create_message_stream(system_prompt, messages, tools, model, max_tokens, tx)
            .await
    }
}

#[async_trait]
impl LlmProvider for CerebrasClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(system_prompt, messages, tools, model, max_tokens)
            .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.verify_credentials().await
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        self.create_message_stream(system_prompt, messages, tools, model, max_tokens, tx)
            .await
    }
}

#[async_trait]
impl LlmProvider for ZaiClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(
            system_prompt,
            messages,
            tools,
            model,
            max_tokens,
            enable_cache,
        )
        .await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.verify_credentials().await
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
        self.create_message_stream(
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
}

#[async_trait]
impl LlmProvider for BedrockClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(
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
        self.create_message_stream(
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
}

#[async_trait]
impl LlmProvider for VertexAnthropicClient {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        self.create_message(
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
        self.create_message_stream(
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
}

/// Which LLM provider to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Standard Anthropic API key (per-token billing).
    Anthropic,
    /// OpenRouter proxy.
    OpenRouter,
    /// xAI Grok — frontier model with tool calling support.
    Grok,
    /// Ollama — local LLM inference (free, OpenAI-compatible).
    /// Used primarily by the Codebase Knowledge Engine for summary generation.
    Ollama,
    /// Google Gemini — frontier model via OpenAI-compatible API.
    Gemini,
    /// Self-hosted vLLM — OpenAI-compatible API on RunPod or similar.
    Vllm,
    /// Cerebras — ultra-fast inference (OpenAI-compatible).
    Cerebras,
    /// Z.ai — direct GLM API (OpenAI-compatible).
    Zai,
    /// AWS Bedrock — Anthropic Messages (Invoke) for Claude, Converse for everything else.
    Bedrock,
    /// Anthropic Claude on GCP Vertex AI — billed against GCP credits/quotas.
    VertexAnthropic,
    /// The ChatGPT backend on a user's ChatGPT login (Responses API).
    ChatGpt,
    /// The `claude` CLI on a user's Claude login.
    ClaudeCode,
    /// The `codex` CLI on a user's ChatGPT login.
    Codex,
}

impl ProviderKind {
    /// Every provider variant in declaration order. Use this rather than
    /// hand-maintaining provider lists at other layers (tool schemas,
    /// docs, etc.) so a new variant flows everywhere automatically.
    pub const ALL: &'static [Self] = &[
        Self::Anthropic,
        Self::OpenRouter,
        Self::Grok,
        Self::Ollama,
        Self::Gemini,
        Self::Vllm,
        Self::Cerebras,
        Self::Zai,
        Self::Bedrock,
        Self::VertexAnthropic,
        Self::ChatGpt,
        Self::ClaudeCode,
        Self::Codex,
    ];

    /// Canonical lowercase identifier, matched first by [`Self::parse`].
    /// Exhaustive on every variant by design — the compiler will fail to
    /// build when a new provider lands without picking a name. A
    /// subscription kind is named by its [`SubscriptionRoute`], the prefix
    /// its calls carry in the cost ledger.
    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
            Self::Grok => "grok",
            Self::Ollama => "ollama",
            Self::Gemini => "gemini",
            Self::Vllm => "vllm",
            Self::Cerebras => "cerebras",
            Self::Zai => "zai",
            Self::Bedrock => "bedrock",
            Self::VertexAnthropic => "vertex-anthropic",
            Self::ChatGpt => SubscriptionRoute::ChatGpt.label(),
            Self::ClaudeCode => SubscriptionRoute::ClaudeCode.label(),
            Self::Codex => SubscriptionRoute::Codex.label(),
        }
    }

    /// Parse a provider name or alias (case-insensitive).
    pub fn parse(s: &str) -> Result<Self, ProviderParseError> {
        if is_removed_provider(s) {
            return Err(ProviderParseError::Removed {
                name: s.trim().to_string(),
            });
        }
        let lower = s.to_lowercase();
        if let Some(kind) = Self::ALL.iter().find(|k| k.canonical_name() == lower) {
            return Ok(*kind);
        }
        match lower.as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openrouter" | "open-router" | "or" => Ok(Self::OpenRouter),
            "grok" | "xai" | "x-ai" => Ok(Self::Grok),
            "ollama" | "local" => Ok(Self::Ollama),
            "gemini" | "google" | "google-ai" => Ok(Self::Gemini),
            "vllm" | "vllm-server" | "self-hosted" | "runpod" => Ok(Self::Vllm),
            "cerebras" | "cerebras-ai" => Ok(Self::Cerebras),
            "zai" | "z-ai" | "zhipu" | "bigmodel" => Ok(Self::Zai),
            "bedrock" | "aws" | "aws-bedrock" => Ok(Self::Bedrock),
            "vertex-anthropic" | "vertex-claude" | "gcp-claude" | "vertex" => {
                Ok(Self::VertexAnthropic)
            }
            "claude_code" => Ok(Self::ClaudeCode),
            _ => Err(ProviderParseError::Unknown(s.to_string())),
        }
    }

    /// Parse from a string (case-insensitive); `None` for anything
    /// [`Self::parse`] refuses.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        Self::parse(s).ok()
    }

    /// Whether the provider runs on a user's subscription login rather than
    /// an API credential. Only the hub can build these: a login needs its
    /// vault write-back.
    pub const fn is_subscription(self) -> bool {
        self.subscription_route().is_some()
    }

    /// Whether the provider authenticates with a bearer API key. The rest
    /// carry a login, a URL in the key's place (Ollama, vLLM) or a credential
    /// chain (Bedrock, Vertex).
    pub const fn takes_api_key(self) -> bool {
        !self.is_subscription()
            && !matches!(
                self,
                Self::Ollama | Self::Vllm | Self::Bedrock | Self::VertexAnthropic
            )
    }

    /// Whether a completion on the provider can be shown an image to
    /// describe. Codex takes images on its turns alone: its completions are
    /// text.
    pub const fn describes_images(self) -> bool {
        self.carries_images() && !matches!(self, Self::Codex)
    }

    /// Whether the provider's turns send image blocks. The rest send a
    /// message's text alone, so a model behind them is never shown an image.
    pub const fn carries_images(self) -> bool {
        match self {
            Self::Anthropic
            | Self::OpenRouter
            | Self::Gemini
            | Self::Vllm
            | Self::Bedrock
            | Self::VertexAnthropic
            | Self::ChatGpt
            | Self::ClaudeCode
            | Self::Codex => true,
            Self::Grok | Self::Ollama | Self::Cerebras | Self::Zai => false,
        }
    }

    /// The subscription route the provider's calls run on, if they run on
    /// one.
    pub const fn subscription_route(self) -> Option<SubscriptionRoute> {
        match self {
            Self::ChatGpt => Some(SubscriptionRoute::ChatGpt),
            Self::ClaudeCode => Some(SubscriptionRoute::ClaudeCode),
            Self::Codex => Some(SubscriptionRoute::Codex),
            Self::Anthropic
            | Self::OpenRouter
            | Self::Grok
            | Self::Ollama
            | Self::Gemini
            | Self::Vllm
            | Self::Cerebras
            | Self::Zai
            | Self::Bedrock
            | Self::VertexAnthropic => None,
        }
    }
}

/// A provider name that names no provider.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderParseError {
    #[error("unknown provider '{0}'")]
    Unknown(String),
    #[error("provider '{name}' was removed: {}", REMOVED_PROVIDER_POINTER)]
    Removed { name: String },
}

/// Why [`create_provider`] could not build a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "provider '{}' runs on a subscription login, which only the hub can use",
        .0.canonical_name()
    )]
    HubOnly(ProviderKind),
}

/// For providers whose credentials are sourced outside of env vars or a
/// secret vault (Bedrock's AWS credential chain, Vertex's Google ADC chain),
/// returns the sentinel `api_key` value that triggers chain resolution at
/// request time. Returns `None` for every other provider.
pub fn chain_credential_fallback(kind: ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Bedrock => Some(super::bedrock_credentials::CHAIN_SENTINEL.to_string()),
        ProviderKind::VertexAnthropic => Some(vertex_credentials::CHAIN_SENTINEL.to_string()),
        _ => None,
    }
}

/// Resolve a provider's primary credential from the process environment.
///
/// Each provider has a canonical env var; some accept legacy aliases
/// (`GEMINI_API_KEY` ← `GOOGLE_API_KEY`).
/// Ollama and Vllm always succeed because they default to a local URL when
/// no env var is set. Callers typically chain this with
/// [`chain_credential_fallback`] for Bedrock/Vertex's ADC paths.
pub fn provider_env_key(kind: ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Anthropic => std::env::var("ANTHROPIC_API_KEY").ok(),
        ProviderKind::OpenRouter => std::env::var("OPENROUTER_API_KEY").ok(),
        ProviderKind::Grok => std::env::var("XAI_API_KEY").ok(),
        ProviderKind::Ollama => std::env::var("OLLAMA_URL")
            .ok()
            .or_else(|| Some("http://localhost:11434".to_string())),
        ProviderKind::Gemini => std::env::var("GEMINI_API_KEY")
            .ok()
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok()),
        ProviderKind::Vllm => std::env::var("VLLM_URL")
            .ok()
            .or_else(|| Some("http://localhost:8000".to_string())),
        ProviderKind::Cerebras => std::env::var("CEREBRAS_API_KEY").ok(),
        ProviderKind::Zai => std::env::var("ZAI_API_KEY").ok(),
        ProviderKind::Bedrock => std::env::var("AWS_ACCESS_KEY_ID").ok(),
        ProviderKind::VertexAnthropic => std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok(),
        ProviderKind::ChatGpt | ProviderKind::ClaudeCode | ProviderKind::Codex => None,
    }
}

/// Canonical env var name for each provider — used in user-facing error
/// messages telling the caller which variable to set.
pub const fn provider_env_var_hint(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
        ProviderKind::Grok => "XAI_API_KEY",
        ProviderKind::Ollama => "OLLAMA_URL",
        ProviderKind::Gemini => "GEMINI_API_KEY",
        ProviderKind::Vllm => "VLLM_URL",
        ProviderKind::Cerebras => "CEREBRAS_API_KEY",
        ProviderKind::Zai => "ZAI_API_KEY",
        ProviderKind::Bedrock => "AWS_ACCESS_KEY_ID",
        ProviderKind::VertexAnthropic => "GOOGLE_APPLICATION_CREDENTIALS",
        ProviderKind::ChatGpt | ProviderKind::Codex => "codex/auth_json (ChatGPT login)",
        ProviderKind::ClaudeCode => "claude_code/oauth_token (Claude login)",
    }
}

/// Create an LLM provider from configuration.
///
/// For `Anthropic`, `api_key` is a standard API key; a Claude subscription
/// token is refused. Subscription kinds ([`ProviderKind::is_subscription`])
/// fail with [`ProviderError::HubOnly`].
pub fn create_provider(
    kind: ProviderKind,
    api_key: &str,
    model: Option<&str>,
    thinking_budget: Option<u32>,
    zai_subscription: bool,
) -> Result<Box<dyn LlmProvider>> {
    Ok(match kind {
        ProviderKind::Anthropic => {
            let client = AnthropicClient::new(api_key)?;
            let client = model.into_iter().fold(client, |c, m| c.with_model(m));
            let client = thinking_budget
                .into_iter()
                .fold(client, |c, b| c.with_thinking_budget(b));
            Box::new(client)
        }
        ProviderKind::OpenRouter => {
            let mut client = OpenRouterClient::new(api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            if let Some(budget) = thinking_budget {
                client = client.with_thinking_budget(budget);
            }
            Box::new(client)
        }
        ProviderKind::Grok => {
            let mut client = GrokClient::new(api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            Box::new(client)
        }
        ProviderKind::Ollama => {
            // For Ollama, api_key is used as the base URL (no auth needed).
            // If empty, defaults to http://localhost:11434.
            let base_url = if api_key.is_empty() || api_key == "none" {
                std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".to_string())
            } else {
                api_key.to_string()
            };
            let ollama_api_key = std::env::var("OLLAMA_API_KEY").unwrap_or_default();
            let mut client = OllamaClient::new(base_url).with_api_key(ollama_api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            Box::new(client)
        }
        ProviderKind::Gemini => {
            let mut client = GeminiClient::new(api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            if let Some(budget) = thinking_budget {
                client = client.with_thinking_budget(budget);
            }
            Box::new(client)
        }
        ProviderKind::Vllm => {
            // For vLLM, api_key field is used as the base URL (not auth).
            // If empty, defaults to http://localhost:8000.
            // Authentication is via VLLM_API_KEY env var → Bearer token.
            let base_url = if api_key.is_empty() || api_key == "none" {
                std::env::var("VLLM_URL").unwrap_or_else(|_| "http://localhost:8000".to_string())
            } else {
                api_key.to_string()
            };
            let vision_url = std::env::var("VLLM_VISION_URL").unwrap_or_default();
            let vllm_api_key = std::env::var("VLLM_API_KEY").unwrap_or_default();
            let mut client = VllmClient::new(base_url)
                .with_vision_url(vision_url)
                .with_api_key(vllm_api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            Box::new(client)
        }
        ProviderKind::Cerebras => {
            let mut client = CerebrasClient::new(api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            if let Some(budget) = thinking_budget {
                client = client.with_thinking_budget(budget);
            }
            Box::new(client)
        }
        ProviderKind::Zai => {
            let mut client = ZaiClient::new(api_key);
            if let Some(m) = model {
                client = client.with_model(m);
            }
            if let Some(budget) = thinking_budget {
                client = client.with_thinking_budget(budget);
            }
            if zai_subscription || std::env::var("ZAI_CODING").is_ok() {
                client = client.with_coding_endpoint(true);
            }
            Box::new(client)
        }
        ProviderKind::Bedrock => {
            let resolver = CredentialResolver::from_packed(api_key);
            let region = bedrock::resolve_region(None);
            let client = BedrockClient::new(resolver, region).with_catalog(bedrock_catalog());
            let client = model.into_iter().fold(client, |c, m| c.with_model(m));
            let client = thinking_budget
                .into_iter()
                .fold(client, |c, b| c.with_thinking_budget(b));
            Box::new(client)
        }
        ProviderKind::VertexAnthropic => {
            let source = vertex_credentials::resolve_source(api_key)?;
            let project = vertex_credentials::resolve_project(None)?;
            let location = vertex_credentials::resolve_location(None)?;
            let token_source: Arc<dyn VertexTokenSource> = match source {
                VertexCredentialSource::Adc => Arc::new(LazyGcpAuthTokenSource::adc()),
                VertexCredentialSource::ServiceAccountFile(p) => {
                    Arc::new(LazyGcpAuthTokenSource::from_service_account_file(p))
                }
            };
            let client = VertexAnthropicClient::new(token_source, project, location);
            let client = model.into_iter().fold(client, |c, m| c.with_model(m));
            let client = thinking_budget
                .into_iter()
                .fold(client, |c, b| c.with_thinking_budget(b));
            Box::new(client)
        }
        ProviderKind::ChatGpt | ProviderKind::ClaudeCode | ProviderKind::Codex => {
            return Err(ProviderError::HubOnly(kind).into());
        }
    })
}

/// Create a provider (async variant for consistency with `create_named_provider_async`).
pub async fn create_provider_async(
    kind: ProviderKind,
    api_key: &str,
    model: Option<&str>,
    thinking_budget: Option<u32>,
    zai_subscription: bool,
) -> eyre::Result<Box<dyn LlmProvider>> {
    create_named_provider_async(
        kind,
        "default",
        api_key,
        model,
        thinking_budget,
        zai_subscription,
    )
    .await
}

/// Create a provider with a specific name for pool identification.
///
/// Each name identifies a token in the pool.
pub async fn create_named_provider_async(
    kind: ProviderKind,
    _name: &str,
    api_key: &str,
    model: Option<&str>,
    thinking_budget: Option<u32>,
    zai_subscription: bool,
) -> eyre::Result<Box<dyn LlmProvider>> {
    create_provider(kind, api_key, model, thinking_budget, zai_subscription)
}

#[cfg(test)]
mod verify_credentials_default_tests {
    //! The trait-level default impl falls back to a 1-token completion.
    //! Providers without a cheaper auth-only endpoint (Bedrock, Vertex,
    //! Ollama, vLLM) rely on this — these tests guard the fallback so
    //! a future refactor doesn't silently break verification for them.
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crate::llm::types::{ContentBlock, Role, StopReason, TokenUsage};

    #[derive(Default)]
    struct ProbeProvider {
        create_calls: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait]
    impl LlmProvider for ProbeProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            tools: &[LlmToolDef],
            _model: Option<&str>,
            max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            // Default verify contract: 1 user msg, no tools, 1 max token.
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role, Role::User);
            assert!(tools.is_empty());
            assert_eq!(max_tokens, Some(1));
            if self.fail {
                Err(eyre::eyre!("simulated failure"))
            } else {
                Ok(LlmResponse {
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    model: "probe-model".into(),
                })
            }
        }
    }

    #[tokio::test]
    async fn default_verify_calls_create_message_once() {
        let probe = ProbeProvider::default();
        let calls = Arc::clone(&probe.create_calls);
        probe.verify_credentials().await.expect("verify ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn default_verify_surfaces_create_message_error() {
        let probe = ProbeProvider {
            create_calls: Arc::new(AtomicUsize::new(0)),
            fail: true,
        };
        let err = probe.verify_credentials().await.expect_err("must fail");
        assert!(err.to_string().contains("simulated failure"));
    }
}

#[cfg(test)]
mod provider_kind_tests {
    use super::{create_provider, ProviderKind, ProviderParseError};
    use workforce_common::settings::{REMOVED_PROVIDER_ALIASES, REMOVED_PROVIDER_POINTER};

    #[test]
    fn removed_provider_names_are_refused_with_the_pointer() {
        for name in REMOVED_PROVIDER_ALIASES
            .iter()
            .copied()
            .chain(["Claude-Max", " max "])
        {
            let err = ProviderKind::parse(name).unwrap_err();
            assert_eq!(
                err,
                ProviderParseError::Removed {
                    name: name.trim().to_string()
                }
            );
            assert!(err.to_string().contains(REMOVED_PROVIDER_POINTER), "{err}");
            assert_eq!(ProviderKind::from_str_loose(name), None);
        }
    }

    #[test]
    fn oat_key_under_anthropic_provider_rejected() {
        let err = create_provider(
            ProviderKind::Anthropic,
            concat!("sk-ant", "-oat01-abcdefghijklmnop"),
            None,
            None,
            false,
        )
        .err()
        .expect("a Claude subscription token is not an API key")
        .to_string();
        assert!(err.contains(REMOVED_PROVIDER_POINTER), "{err}");
        create_provider(
            ProviderKind::Anthropic,
            concat!("sk-ant", "-api03-abcdefghijklmnop"),
            None,
            None,
            false,
        )
        .expect("an API key builds");
    }

    #[test]
    fn all_round_trips_through_from_str_loose() {
        for kind in ProviderKind::ALL {
            let name = kind.canonical_name();
            let parsed = ProviderKind::from_str_loose(name)
                .unwrap_or_else(|| panic!("canonical name `{name}` does not parse back"));
            assert_eq!(parsed, *kind);
        }
    }

    #[test]
    fn all_names_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for kind in ProviderKind::ALL {
            let name = kind.canonical_name();
            assert_eq!(name, name.to_lowercase(), "name `{name}` must be lowercase");
            assert!(seen.insert(name), "duplicate canonical name `{name}`");
        }
    }

    #[test]
    fn every_variant_is_in_all() {
        let coverage = |k: ProviderKind| match k {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::Grok => "grok",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Vllm => "vllm",
            ProviderKind::Cerebras => "cerebras",
            ProviderKind::Zai => "zai",
            ProviderKind::Bedrock => "bedrock",
            ProviderKind::VertexAnthropic => "vertex-anthropic",
            ProviderKind::ChatGpt => "chatgpt",
            ProviderKind::ClaudeCode => "claude-code",
            ProviderKind::Codex => "codex",
        };
        for kind in ProviderKind::ALL {
            assert_eq!(coverage(*kind), kind.canonical_name());
        }
    }

    #[test]
    fn subscription_kinds_parse_and_are_the_only_subscriptions() {
        for (name, kind) in [
            ("chatgpt", ProviderKind::ChatGpt),
            ("ChatGPT", ProviderKind::ChatGpt),
            ("claude-code", ProviderKind::ClaudeCode),
            ("claude_code", ProviderKind::ClaudeCode),
            ("codex", ProviderKind::Codex),
        ] {
            assert_eq!(ProviderKind::parse(name), Ok(kind), "{name}");
        }
        let subscriptions: Vec<ProviderKind> = ProviderKind::ALL
            .iter()
            .copied()
            .filter(|k| k.is_subscription())
            .collect();
        assert_eq!(
            subscriptions,
            [
                ProviderKind::ChatGpt,
                ProviderKind::ClaudeCode,
                ProviderKind::Codex
            ]
        );
    }

    #[test]
    fn unknown_provider_is_a_typed_parse_error() {
        assert_eq!(
            ProviderKind::parse("claude-cli"),
            Err(super::ProviderParseError::Unknown("claude-cli".into()))
        );
        assert_eq!(ProviderKind::from_str_loose("claude-cli"), None);
    }

    #[test]
    fn subscription_kinds_are_hub_only() {
        for kind in ProviderKind::ALL.iter().filter(|k| k.is_subscription()) {
            let err = super::create_provider(*kind, "token", None, None, false)
                .err()
                .unwrap_or_else(|| panic!("{kind:?} built outside the hub"));
            assert_eq!(
                err.downcast_ref::<super::ProviderError>(),
                Some(&super::ProviderError::HubOnly(*kind))
            );
        }
    }

    #[test]
    fn vertex_anthropic_aliases_parse() {
        for alias in [
            "vertex-anthropic",
            "vertex-claude",
            "gcp-claude",
            "vertex",
            "VERTEX-ANTHROPIC",
        ] {
            assert_eq!(
                ProviderKind::from_str_loose(alias),
                Some(ProviderKind::VertexAnthropic),
                "alias {alias} failed to parse"
            );
        }
    }
}

#[cfg(test)]
mod chain_credential_fallback_tests {
    use super::{chain_credential_fallback, ProviderKind};
    use crate::llm::bedrock_credentials::CHAIN_SENTINEL as BEDROCK_SENTINEL;
    use crate::llm::vertex_credentials::CHAIN_SENTINEL as VERTEX_SENTINEL;

    #[test]
    fn bedrock_returns_chain_sentinel() {
        assert_eq!(
            chain_credential_fallback(ProviderKind::Bedrock).as_deref(),
            Some(BEDROCK_SENTINEL)
        );
    }

    #[test]
    fn vertex_anthropic_returns_chain_sentinel() {
        assert_eq!(
            chain_credential_fallback(ProviderKind::VertexAnthropic).as_deref(),
            Some(VERTEX_SENTINEL)
        );
    }

    #[test]
    fn other_providers_return_none() {
        for kind in ProviderKind::ALL {
            if matches!(kind, ProviderKind::Bedrock | ProviderKind::VertexAnthropic) {
                continue;
            }
            assert!(
                chain_credential_fallback(*kind).is_none(),
                "{kind:?} unexpectedly produced a chain fallback"
            );
        }
    }
}

#[cfg(test)]
mod provider_env_key_tests {
    use super::{provider_env_key, provider_env_var_hint, ProviderKind};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(value) => std::env::set_var(&k, value),
                None => std::env::remove_var(&k),
            }
        }
    }

    #[test]
    fn gemini_prefers_gemini_api_key_over_google_api_key() {
        with_env(
            &[
                ("GEMINI_API_KEY", Some("from-gemini")),
                ("GOOGLE_API_KEY", Some("from-google")),
            ],
            || {
                assert_eq!(
                    provider_env_key(ProviderKind::Gemini).as_deref(),
                    Some("from-gemini")
                );
            },
        );
    }

    #[test]
    fn gemini_falls_back_to_google_api_key() {
        with_env(
            &[
                ("GEMINI_API_KEY", None),
                ("GOOGLE_API_KEY", Some("from-google")),
            ],
            || {
                assert_eq!(
                    provider_env_key(ProviderKind::Gemini).as_deref(),
                    Some("from-google")
                );
            },
        );
    }

    #[test]
    fn gemini_returns_none_when_neither_set() {
        with_env(
            &[("GEMINI_API_KEY", None), ("GOOGLE_API_KEY", None)],
            || {
                assert!(provider_env_key(ProviderKind::Gemini).is_none());
            },
        );
    }

    #[test]
    fn ollama_and_vllm_default_to_localhost_when_unset() {
        with_env(&[("OLLAMA_URL", None), ("VLLM_URL", None)], || {
            assert_eq!(
                provider_env_key(ProviderKind::Ollama).as_deref(),
                Some("http://localhost:11434")
            );
            assert_eq!(
                provider_env_key(ProviderKind::Vllm).as_deref(),
                Some("http://localhost:8000")
            );
        });
    }

    #[test]
    fn env_var_hint_is_canonical_primary() {
        assert_eq!(
            provider_env_var_hint(ProviderKind::Gemini),
            "GEMINI_API_KEY"
        );
        assert_eq!(
            provider_env_var_hint(ProviderKind::Anthropic),
            "ANTHROPIC_API_KEY"
        );
        for kind in ProviderKind::ALL {
            assert!(!provider_env_var_hint(*kind).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn subscription_kinds_have_no_env_credential() {
        with_env(&[("ANTHROPIC_API_KEY", Some("ak"))], || {
            for kind in ProviderKind::ALL.iter().filter(|k| k.is_subscription()) {
                assert_eq!(provider_env_key(*kind), None, "{kind:?}");
            }
        });
        assert_eq!(
            provider_env_var_hint(ProviderKind::ChatGpt),
            "codex/auth_json (ChatGPT login)"
        );
        assert_eq!(
            provider_env_var_hint(ProviderKind::ClaudeCode),
            "claude_code/oauth_token (Claude login)"
        );
    }
}

#[cfg(test)]
mod reasoning_scope_tests {
    use super::{reasoning_suppressed, with_reasoning_suppressed};

    #[tokio::test]
    async fn suppressed_only_within_scope() {
        // Default (no scope active): reasoning is on — providers send `effort`.
        assert!(!reasoning_suppressed());

        // Inside the scope: request builders drop reasoning. The flag is
        // readable across `.await` points within the scoped future.
        let inner = with_reasoning_suppressed(async {
            let before = reasoning_suppressed();
            tokio::task::yield_now().await;
            before && reasoning_suppressed()
        })
        .await;
        assert!(inner, "reasoning must read as suppressed within the scope");

        // The scope does not leak back out to the surrounding task.
        assert!(!reasoning_suppressed());
    }
}

#[cfg(test)]
mod thinking_budget_scope_tests {
    use super::{
        effective_thinking_budget, thinking_budget_override, with_reasoning_suppressed,
        with_thinking_budget_override,
    };

    #[tokio::test]
    async fn no_scope_is_a_passthrough_of_the_configured_budget() {
        assert_eq!(thinking_budget_override(), None);
        assert_eq!(effective_thinking_budget(Some(10_000)), Some(10_000));
        assert_eq!(effective_thinking_budget(None), None);

        // `None` establishes no scope at all, so the configured budget stands.
        let inner =
            with_thinking_budget_override(None, async { effective_thinking_budget(Some(10_000)) })
                .await;
        assert_eq!(inner, Some(10_000));
    }

    #[tokio::test]
    async fn override_replaces_the_configured_budget_within_scope() {
        let inner = with_thinking_budget_override(Some(0), async {
            let before = effective_thinking_budget(Some(10_000));
            tokio::task::yield_now().await;
            (before, effective_thinking_budget(Some(10_000)))
        })
        .await;
        // Zero survives as zero — providers map it to "don't think", so it
        // must not be confused with "no override".
        assert_eq!(inner, (Some(0), Some(0)));

        // The scope does not leak back out.
        assert_eq!(effective_thinking_budget(Some(10_000)), Some(10_000));
    }

    #[tokio::test]
    async fn suppression_beats_the_override() {
        // A background extraction that suppressed reasoning must not have it
        // handed back by a voice turn's override — the configured budget
        // stands, exactly as it does today.
        let inner = with_thinking_budget_override(Some(8_000), async {
            with_reasoning_suppressed(async { effective_thinking_budget(Some(10_000)) }).await
        })
        .await;
        assert_eq!(inner, Some(10_000));
    }
}

#[cfg(test)]
mod cached_prefix_scope_tests {
    use super::{cached_prefix_len, with_cached_prefix};

    #[tokio::test]
    async fn cached_prefix_is_none_outside_a_scope_and_visible_inside() {
        assert_eq!(cached_prefix_len(), None);
        assert_eq!(
            with_cached_prefix(None, async { cached_prefix_len() }).await,
            None
        );

        let inner = with_cached_prefix(Some(3), async {
            let before = cached_prefix_len();
            tokio::task::yield_now().await;
            (before, cached_prefix_len())
        })
        .await;
        assert_eq!(inner, (Some(3), Some(3)));

        assert_eq!(cached_prefix_len(), None, "the scope does not leak out");
    }
}
