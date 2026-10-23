//! Vision LLM abstraction — describes images via a configurable secondary
//! provider when the active agent's primary model isn't vision-capable.
//!
//! Construction lives in `crates/hub/src/vision/` (config resolution + provider
//! instantiation); this module owns the trait and the cached adapter the hub
//! returns. Mirrors the summariser split between `workforce-codebase-knowledge`
//! and `hub::knowledge`.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use lru::LruCache;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use workforce_common::secret::KeySlot;
use workforce_common::subscription::Served;

use crate::llm::provider::LlmProvider;
use crate::llm::types::{ContentBlock, ImageSource, LlmMessage, Role, TokenUsage};
use crate::runner::with_served_log;

/// Callback invoked after every successful uncached vision call. Used by the
/// hub to attribute vision-call cost to the active agent's session ledger.
///
/// Cache hits do not fire this callback — the cost was recorded when the
/// description was first generated. The third argument is the subscription
/// that served the call, if one did.
pub type UsageObserver = Arc<dyn Fn(&str, &TokenUsage, Option<&Served>) + Send + Sync>;

/// Generate a textual description of an image. Implementations are typically
/// backed by a vision-capable LLM (Claude / Gemini / Qwen-VL / GLM-V).
#[async_trait]
pub trait VisionLlm: Send + Sync {
    /// `media_type` is e.g. `"image/png"`, `base64_data` is the raw base64
    /// payload, `hint` is an optional caller-supplied snippet (filename,
    /// surrounding text) that focuses the description.
    async fn describe_image(
        &self,
        media_type: &str,
        base64_data: &str,
        hint: Option<&str>,
    ) -> Result<String>;

    /// Concrete model name; used in the substituted text-block byline.
    fn model_name(&self) -> &str;
}

const PROMPT_BASE: &str = "Describe this image in detail. Capture text content (transcribe \
    verbatim if it's a screenshot of UI, code, logs, or a chart with labels), visual \
    structure, and anything relevant a downstream language model would need to answer \
    questions about it.";

/// Adapts a generic [`LlmProvider`] into the [`VisionLlm`] trait, with a
/// bounded LRU cache keyed by SHA-256 of the base64 payload so re-uploaded
/// images don't re-bill the vision provider.
pub struct LlmProviderVisionAdapter {
    provider: Arc<dyn LlmProvider>,
    model: String,
    cache: Option<Mutex<LruCache<[u8; 32], String>>>,
    usage_observer: Option<UsageObserver>,
    payer: Option<KeySlot>,
}

impl LlmProviderVisionAdapter {
    /// `cache_size = 0` disables caching.
    pub fn new(provider: Arc<dyn LlmProvider>, model: String, cache_size: usize) -> Self {
        let cache = NonZeroUsize::new(cache_size).map(|n| Mutex::new(LruCache::new(n)));
        Self {
            provider,
            model,
            cache,
            usage_observer: None,
            payer: None,
        }
    }

    pub fn with_usage_observer(mut self, observer: UsageObserver) -> Self {
        self.usage_observer = Some(observer);
        self
    }

    /// The key that pays for a description. Descriptions are kept apart by
    /// payer, and someone with no key is refused before the cache is read,
    /// so nobody is served what another person's key paid for.
    pub fn with_payer(mut self, payer: KeySlot) -> Self {
        self.payer = Some(payer);
        self
    }

    fn cache_key(&self, base64_data: &str) -> Result<[u8; 32]> {
        let payer = self.payer.as_ref().map(KeySlot::payer).transpose()?;
        Ok(payer
            .iter()
            .fold(Sha256::new(), |hasher, payer| hasher.chain_update(payer))
            .chain_update(base64_data.as_bytes())
            .finalize()
            .into())
    }

    fn cached(&self, key: &[u8; 32]) -> Option<String> {
        self.cache.as_ref()?.lock().get(key).cloned()
    }

    fn store(&self, key: [u8; 32], description: &str) {
        if let Some(cache) = self.cache.as_ref() {
            cache.lock().put(key, description.to_string());
        }
    }
}

fn build_prompt(hint: Option<&str>) -> String {
    match hint {
        Some(h) if !h.is_empty() => format!("{PROMPT_BASE} It was attached as `{h}`."),
        _ => PROMPT_BASE.to_string(),
    }
}

#[async_trait]
impl VisionLlm for LlmProviderVisionAdapter {
    async fn describe_image(
        &self,
        media_type: &str,
        base64_data: &str,
        hint: Option<&str>,
    ) -> Result<String> {
        let key = self.cache_key(base64_data)?;
        if let Some(cached) = self.cached(&key) {
            tracing::debug!(model = %self.model, "Vision cache hit");
            return Ok(cached);
        }

        let messages = vec![LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: media_type.to_string(),
                        data: base64_data.to_string(),
                    },
                },
                ContentBlock::Text {
                    text: build_prompt(hint),
                },
            ],
        }];

        let (response, served) = with_served_log(self.provider.create_message(
            None,
            &messages,
            &[],
            Some(&self.model),
            Some(2048),
            false,
        ))
        .await;
        let response = response?;
        let description = response
            .text()
            .ok_or_else(|| eyre::eyre!("Vision LLM returned no text content"))?;

        if let Some(observer) = self.usage_observer.as_ref() {
            observer(&self.model, &response.usage, served.as_ref());
        }
        if self.cache_key(base64_data).is_ok_and(|after| after == key) {
            self.store(key, &description);
        }

        Ok(description)
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::streaming::StreamEvent;
    use crate::llm::types::{LlmResponse, LlmToolDef, StopReason};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct CountingFakeProvider {
        calls: Arc<AtomicUsize>,
        response_text: String,
    }

    #[async_trait]
    impl LlmProvider for CountingFakeProvider {
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
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.response_text.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "fake".to_string(),
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
            _tx: mpsc::Sender<StreamEvent>,
        ) -> Result<()> {
            unimplemented!()
        }
    }

    fn fake_adapter(
        response_text: &str,
        cache_size: usize,
    ) -> (LlmProviderVisionAdapter, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingFakeProvider {
            calls: Arc::clone(&calls),
            response_text: response_text.to_string(),
        });
        let adapter = LlmProviderVisionAdapter::new(provider, "fake".to_string(), cache_size);
        (adapter, calls)
    }

    #[tokio::test]
    async fn cache_hit_skips_provider_call() {
        let (adapter, calls) = fake_adapter("a red square", 8);
        let first = adapter
            .describe_image("image/png", "abcdef", None)
            .await
            .unwrap();
        let second = adapter
            .describe_image("image/png", "abcdef", None)
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_description_is_served_only_to_the_payer_whose_key_bought_it() {
        use crate::runner::CURRENT_USER_ID;
        use workforce_common::secret::NoKey;

        let (adapter, calls) = fake_adapter("a red square", 8);
        let adapter = adapter.with_payer(KeySlot::new(
            Arc::new(|| {
                let person = CURRENT_USER_ID.try_with(Clone::clone).ok()?;
                match person.as_str() {
                    "alice" => Some("alice-key".to_string()),
                    "bob" | "carol" => Some("shared-key".to_string()),
                    _ => None,
                }
            }),
            "no key for you",
        ));
        let see = |person: &str| {
            CURRENT_USER_ID.scope(
                person.to_string(),
                adapter.describe_image("image/png", "abcdef", None),
            )
        };

        see("alice").await.unwrap();
        see("alice").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "alice pays once");

        let refused = see("dave").await.unwrap_err();
        assert_eq!(
            refused.downcast_ref::<NoKey>(),
            Some(&NoKey("no key for you".into())),
            "someone with no key is refused before the cache is read"
        );

        see("bob").await.unwrap();
        see("carol").await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "another key pays once, for everyone on it"
        );
    }

    #[tokio::test]
    async fn a_description_is_not_kept_when_the_payer_changed_during_the_call() {
        let reads = Arc::new(AtomicUsize::new(0));
        let (adapter, calls) = fake_adapter("a red square", 8);
        let adapter = adapter.with_payer(KeySlot::new(
            {
                let reads = Arc::clone(&reads);
                Arc::new(move || match reads.fetch_add(1, Ordering::SeqCst) {
                    0 => Some("shared-key".to_string()),
                    _ => Some("alices-new-key".to_string()),
                })
            },
            "none",
        ));
        let see = || adapter.describe_image("image/png", "abcdef", None);
        see().await.unwrap();
        see().await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "nothing was filed under the key that did not pay"
        );
        see().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cache_miss_for_different_payload() {
        let (adapter, calls) = fake_adapter("anything", 8);
        adapter
            .describe_image("image/png", "first", None)
            .await
            .unwrap();
        adapter
            .describe_image("image/png", "second", None)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cache_disabled_when_size_zero() {
        let (adapter, calls) = fake_adapter("x", 0);
        adapter
            .describe_image("image/png", "x", None)
            .await
            .unwrap();
        adapter
            .describe_image("image/png", "x", None)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn usage_observer_fires_only_on_uncached_calls() {
        let (adapter, calls) = fake_adapter("ok", 8);
        let observer_calls = Arc::new(AtomicUsize::new(0));
        let observer_clone = Arc::clone(&observer_calls);
        let adapter = adapter.with_usage_observer(Arc::new(move |_model, _usage, _served| {
            observer_clone.fetch_add(1, Ordering::SeqCst);
        }));

        adapter
            .describe_image("image/png", "x", None)
            .await
            .unwrap();
        adapter
            .describe_image("image/png", "x", None)
            .await
            .unwrap();
        adapter
            .describe_image("image/png", "y", None)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(observer_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn build_prompt_includes_hint_when_provided() {
        assert!(build_prompt(Some("screenshot.png")).contains("screenshot.png"));
        assert!(!build_prompt(None).contains("attached as"));
        assert!(!build_prompt(Some("")).contains("attached as"));
    }
}
