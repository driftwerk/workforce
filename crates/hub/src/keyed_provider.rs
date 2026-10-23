//! An [`LlmProvider`] whose API key is read on every call.
//!
//! The provider clients take their key when they are built. This one asks a
//! [`KeySlot`] for the key of whoever each call is for and runs the call on a
//! client built with it, so a person's own key pays for their calls, a key
//! rotated in the vault is used on the next one, and a call for someone with
//! no key is refused before anything is sent. Clients are kept per key, so
//! people taking turns reuse theirs.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use lru::LruCache;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::streaming::StreamEvent;
use workforce_agent::llm::types::{LlmMessage, LlmResponse, LlmToolDef};
use workforce_common::secret::{scrub, KeySlot};

/// Clients kept at once: the people whose calls interleave on one agent.
const CLIENTS: NonZeroUsize = NonZeroUsize::new(16).unwrap();

type Build = Box<dyn Fn(&str) -> Result<Arc<dyn LlmProvider>> + Send + Sync>;

pub struct KeyedProvider {
    key: KeySlot,
    build: Build,
    clients: Mutex<LruCache<[u8; 32], Arc<dyn LlmProvider>>>,
}

impl KeyedProvider {
    /// `build` makes the client for a key.
    pub fn new(
        key: KeySlot,
        build: impl Fn(&str) -> Result<Arc<dyn LlmProvider>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            key,
            build: Box::new(build),
            clients: Mutex::new(LruCache::new(CLIENTS)),
        }
    }

    /// The client for whoever is in scope now, and the key it runs on.
    fn client(&self) -> Result<(Arc<dyn LlmProvider>, String)> {
        let (key, payer) = self.key.paid()?;
        if let Some(client) = self.clients.lock().get(&payer) {
            return Ok((Arc::clone(client), key));
        }
        let client = (self.build)(&key)?;
        self.clients.lock().put(payer, Arc::clone(&client));
        Ok((client, key))
    }
}

/// `error` as it was, unless it echoes `key`: then its text with the key
/// taken out.
fn without_key(error: eyre::Report, key: &str) -> eyre::Report {
    let said = format!("{error:#}");
    let clean = scrub(&said, Some(key));
    match clean == said {
        true => error,
        false => eyre::eyre!(clean),
    }
}

#[async_trait]
impl LlmProvider for KeyedProvider {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let (client, key) = self.client()?;
        client
            .create_message(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
            )
            .await
            .map_err(|e| without_key(e, &key))
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        match self.client() {
            Ok((client, _)) => client.context_floor(model).await,
            Err(_) => workforce_agent::llm::model_limits::context_for(model),
        }
    }

    async fn verify_credentials(&self) -> Result<()> {
        let (client, key) = self.client()?;
        client
            .verify_credentials()
            .await
            .map_err(|e| without_key(e, &key))
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
        let (client, key) = self.client()?;
        client
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
            .map_err(|e| without_key(e, &key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_agent::llm::types::{ContentBlock, StopReason, TokenUsage};
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::secret::NoKey;

    /// Answers with the key it was built with.
    struct Echo(String);

    #[async_trait]
    impl LlmProvider for Echo {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.0.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "echo".to_string(),
            })
        }
    }

    async fn ask(provider: &KeyedProvider, person: &str) -> Result<String> {
        CURRENT_USER_ID
            .scope(
                person.to_string(),
                provider.create_message(None, &[], &[], None, None, false),
            )
            .await
            .map(|response| response.text().unwrap_or_default())
    }

    #[tokio::test]
    async fn each_call_runs_on_the_callers_key_and_a_client_is_built_once_per_key() {
        let keys = Arc::new(Mutex::new(std::collections::HashMap::from([
            ("alice".to_string(), "alice-key".to_string()),
            ("bob".to_string(), "bob-key".to_string()),
        ])));
        let source = {
            let keys = Arc::clone(&keys);
            Arc::new(move || {
                let person = CURRENT_USER_ID.try_with(Clone::clone).ok()?;
                keys.lock().get(&person).cloned()
            })
        };
        let built = Arc::new(AtomicUsize::new(0));
        let provider = KeyedProvider::new(KeySlot::new(source, "no key for you"), {
            let built = Arc::clone(&built);
            move |key| {
                built.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(Echo(key.to_string())))
            }
        });

        assert_eq!(ask(&provider, "alice").await.unwrap(), "alice-key");
        assert_eq!(ask(&provider, "bob").await.unwrap(), "bob-key");
        assert_eq!(ask(&provider, "alice").await.unwrap(), "alice-key");
        assert_eq!(built.load(Ordering::SeqCst), 2);

        let refused = ask(&provider, "carol").await.unwrap_err();
        assert_eq!(
            refused.downcast_ref::<NoKey>(),
            Some(&NoKey("no key for you".into())),
            "someone with no key is refused, never served on another's"
        );
        assert!(provider.verify_credentials().await.is_err());

        keys.lock().insert("alice".into(), "rotated".into());
        assert_eq!(ask(&provider, "alice").await.unwrap(), "rotated");
        assert_eq!(built.load(Ordering::SeqCst), 3);

        keys.lock().remove("alice");
        assert!(
            ask(&provider, "alice").await.unwrap_err().is::<NoKey>(),
            "a deleted key is refused although its client is still kept"
        );
    }

    fn echoing(key: KeySlot) -> KeyedProvider {
        KeyedProvider::new(key, |key| Ok(Arc::new(Echo(key.to_string()))))
    }

    #[tokio::test]
    async fn a_key_that_changes_mid_call_never_files_its_client_under_anothers() {
        let reads = Arc::new(AtomicUsize::new(0));
        let provider = echoing(KeySlot::new(
            {
                let reads = Arc::clone(&reads);
                Arc::new(move || {
                    let read = reads.fetch_add(1, Ordering::SeqCst);
                    Some(["shared-key", "alices-new-key"][read % 2].to_string())
                })
            },
            "none",
        ));
        let answers = [
            ask(&provider, "alice").await.unwrap(),
            ask(&provider, "bob").await.unwrap(),
            ask(&provider, "carol").await.unwrap(),
        ];
        assert_eq!(answers, ["shared-key", "alices-new-key", "shared-key"]);
    }

    #[tokio::test]
    async fn an_upstream_error_that_echoes_the_key_is_passed_on_without_it() {
        /// Fails the way a provider does: echoing the key, or not.
        struct Refusing(String);
        #[async_trait]
        impl LlmProvider for Refusing {
            async fn create_message(
                &self,
                _system_prompt: Option<&str>,
                _messages: &[LlmMessage],
                _tools: &[LlmToolDef],
                _model: Option<&str>,
                _max_tokens: Option<u32>,
                _enable_cache: bool,
            ) -> Result<LlmResponse> {
                Err(NoKey(self.0.clone()).into())
            }
        }
        let refusing = |says: &'static str| {
            KeyedProvider::new(KeySlot::fixed("sk-or-alices-secret"), move |_| {
                Ok(Arc::new(Refusing(says.to_string())))
            })
        };
        let echoed = ask(&refusing("401: bad key sk-or-alices-secret"), "alice")
            .await
            .unwrap_err();
        assert_eq!(echoed.to_string(), "401: bad key <redacted>");
        let masked = ask(&refusing("401: bad key sk-or-***********cret"), "alice")
            .await
            .unwrap_err();
        assert_eq!(masked.to_string(), "401: bad key <redacted>");
        let plain = ask(&refusing("529: overloaded"), "alice")
            .await
            .unwrap_err();
        assert!(
            plain.is::<NoKey>(),
            "an error that shows no key keeps its type"
        );
    }

    #[tokio::test]
    async fn more_keys_than_clients_kept_still_run_each_call_on_its_own() {
        let provider = Arc::new(echoing(KeySlot::new(
            Arc::new(|| {
                CURRENT_USER_ID
                    .try_with(Clone::clone)
                    .ok()
                    .map(|p| format!("key-of-{p}"))
            }),
            "none",
        )));
        let calls = (0..CLIENTS.get() * 3).map(|i| {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                let person = format!("p{i}");
                (ask(&provider, &person).await.unwrap(), person)
            })
        });
        for call in calls.collect::<Vec<_>>() {
            let (answer, person) = call.await.unwrap();
            assert_eq!(answer, format!("key-of-{person}"));
        }
        assert_eq!(provider.clients.lock().len(), CLIENTS.get());
    }
}
