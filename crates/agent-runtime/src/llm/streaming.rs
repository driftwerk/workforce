use std::time::Duration;

use eyre::Result;
use tokio::sync::mpsc;

use super::anthropic::AnthropicClient;
use super::types::{ContentBlock, LlmMessage, LlmToolDef, StopReason, TokenUsage};

/// A streaming event from the LLM.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Start of a content block.
    ContentStart { index: usize },
    /// Text delta.
    TextDelta { index: usize, text: String },
    /// Thinking delta (extended thinking).
    ThinkingDelta { index: usize, thinking: String },
    /// Thinking signature delta (extended thinking).
    SignatureDelta { index: usize, signature: String },
    /// Tool use started.
    ToolUseStart {
        index: usize,
        id: String,
        name: String,
        /// Provider integrity token (e.g. Gemini 3.x `thought_signature`)
        /// that must be echoed back when this tool call appears in
        /// subsequent request history. None for providers that don't use it.
        signature: Option<String>,
    },
    /// Tool signature delivered on a chunk after `ToolUseStart`.
    /// Gemini's OAI-compat streaming sometimes emits `extra_content.google.thought_signature`
    /// on a delta that has no `function.name`; without this event the signature is dropped
    /// and the next request 400s with "missing thought_signature in functionCall parts".
    ToolSignatureUpdate { index: usize, signature: String },
    /// Tool input delta (JSON fragment).
    InputJsonDelta { index: usize, partial: String },
    /// Content block finished.
    ContentStop { index: usize },
    /// Message complete.
    MessageDone {
        stop_reason: StopReason,
        usage: TokenUsage,
    },
    /// The failover layer is sleeping out a provider rate-limit before
    /// retrying the same model. Pure UI signal — carries no content; the
    /// collector ignores it and the runner surfaces it as
    /// `ProgressEvent::RateLimited` so cards show the wait instead of a
    /// generic "Thinking…".
    RateLimitWait { wait_secs: u64, attempt: u32 },
    /// The failover layer abandoned a model that produced no first token
    /// within the watchdog window and is moving to the next model in the
    /// chain. Pure UI signal like `RateLimitWait` — the collector ignores
    /// it and the runner surfaces it as progress so cards explain the
    /// switch instead of showing a generic "Thinking…".
    SlowModelFailover { model: String, elapsed_secs: u64 },
    /// A provider whose answer comes only at the end says it is still under
    /// way, and will say so again within `within_secs`. Carries nothing and
    /// is never shown; it only keeps the liveness watchdog waiting.
    KeepAlive { within_secs: u64 },
    /// Error.
    Error { message: String },
}

impl StreamEvent {
    /// Whether the event is part of the answer, not a UI signal about the
    /// call.
    pub fn carries_content(&self) -> bool {
        !matches!(
            self,
            Self::RateLimitWait { .. } | Self::SlowModelFailover { .. } | Self::KeepAlive { .. }
        )
    }
}

/// Marker embedded in errors returned when a stream produced no first event
/// within the watchdog window. `classify_error` keys on it; nothing was
/// forwarded to the consumer, so failing over to another model is invisible.
pub const FIRST_TOKEN_TIMEOUT_MARKER: &str = "llm first-token timeout";

/// Marker embedded in errors returned when a stream went silent after events
/// were already forwarded. `classify_error` keys on it; partial content has
/// reached the consumer, so the turn must abort rather than fail over.
pub const STREAM_STALL_MARKER: &str = "llm stream stalled";

/// Resolved watchdog windows for one streaming call. `Duration::ZERO`
/// disables the corresponding timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTimeouts {
    /// Max wait for the first stream event after dispatching the request.
    pub first_token: Duration,
    /// Max gap between stream events once the first has arrived.
    pub stall: Duration,
}

/// Watch a provider streaming call for liveness.
///
/// `call` is the provider's `create_message_stream` future holding the
/// sending half of `inner_rx`'s channel; events are forwarded to `tx` as
/// they arrive. Until the first event the `first_token` window applies;
/// after it, the `stall` window, re-armed on every event. The future is
/// pinned on this stack rather than spawned, so a timeout drops it and the
/// in-flight request is cancelled with it. A provider's own
/// [`StreamEvent::RateLimitWait`] or [`StreamEvent::KeepAlive`] is forwarded
/// without counting as a first event, and re-arms the current window from
/// the end of the wait it names.
///
/// A `first_token` timeout returns an error carrying
/// [`FIRST_TOKEN_TIMEOUT_MARKER`] with nothing forwarded — the failover
/// chain can silently move to the next model. A `stall` timeout returns
/// [`STREAM_STALL_MARKER`] after partial content was forwarded — callers
/// must treat it as fatal for the attempt chain.
pub async fn stream_with_watchdog(
    call: impl std::future::Future<Output = Result<()>>,
    mut inner_rx: mpsc::Receiver<StreamEvent>,
    tx: mpsc::Sender<StreamEvent>,
    limits: StreamTimeouts,
    model: &str,
) -> Result<()> {
    let started = tokio::time::Instant::now();
    let mut last_event_at = started;
    let mut first_event_seen = false;
    let mut call_result: Option<Result<()>> = None;
    let mut events_closed = false;
    tokio::pin!(call);

    loop {
        if call_result.is_some() && events_closed {
            // The `is_some` guard means take() cannot be None here.
            return call_result.take().unwrap_or(Ok(()));
        }
        let window = if first_event_seen {
            limits.stall
        } else {
            limits.first_token
        };
        // Stays armed while draining after the call resolves: a provider
        // that cloned its sender into a spawned task could otherwise hold
        // the channel open forever. A genuinely dropped sender yields
        // `None` immediately, so an honest drain never meets the deadline.
        let deadline_armed = !window.is_zero();

        tokio::select! {
            res = &mut call, if call_result.is_none() => {
                call_result = Some(res);
            }
            event = inner_rx.recv(), if !events_closed => match event {
                Some(
                    event @ (StreamEvent::RateLimitWait { wait_secs: secs, .. }
                    | StreamEvent::KeepAlive { within_secs: secs }),
                ) => {
                    last_event_at = tokio::time::Instant::now() + Duration::from_secs(secs);
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                }
                Some(event) => {
                    if !first_event_seen {
                        first_event_seen = true;
                        tracing::info!(
                            model,
                            ttft_ms = started.elapsed().as_millis() as u64,
                            "LLM first stream event"
                        );
                    }
                    last_event_at = tokio::time::Instant::now();
                    if tx.send(event).await.is_err() {
                        return Ok(()); // Receiver dropped
                    }
                }
                None => events_closed = true,
            },
            _ = tokio::time::sleep_until(last_event_at + window), if deadline_armed => {
                return Err(if first_event_seen {
                    eyre::eyre!(
                        "{STREAM_STALL_MARKER}: no stream events for {}s (model {model})",
                        window.as_secs()
                    )
                } else {
                    eyre::eyre!(
                        "{FIRST_TOKEN_TIMEOUT_MARKER}: no first token after {}s (model {model})",
                        window.as_secs()
                    )
                });
            }
        }
    }
}

/// Run a provider streaming call under the watchdog: builds the inner
/// channel, hands its sender to `make_call`, and forwards live events to
/// `tx`. The one place the channel-plus-watchdog dance lives — pool,
/// failover externals, and the CLI's [`WatchdogProvider`] all call this.
///
/// [`WatchdogProvider`]: super::provider::WatchdogProvider
pub async fn watchdogged_stream<F, Fut>(
    limits: StreamTimeouts,
    model: &str,
    tx: mpsc::Sender<StreamEvent>,
    make_call: F,
) -> Result<()>
where
    F: FnOnce(mpsc::Sender<StreamEvent>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (inner_tx, inner_rx) = mpsc::channel(32);
    stream_with_watchdog(make_call(inner_tx), inner_rx, tx, limits, model).await
}

/// Cap a non-streaming call at `limit`; `None` runs uncapped. A timeout
/// yields the [`FIRST_TOKEN_TIMEOUT_MARKER`] error — nothing was surfaced,
/// so the failover chain treats it exactly like a hung stream's first token.
pub async fn call_with_request_cap<T>(
    limit: Option<Duration>,
    model: &str,
    call: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match limit {
        Some(limit) => match tokio::time::timeout(limit, call).await {
            Ok(res) => res,
            Err(_) => Err(eyre::eyre!(
                "{FIRST_TOKEN_TIMEOUT_MARKER}: no response after {}s (model {model})",
                limit.as_secs()
            )),
        },
        None => call.await,
    }
}

impl AnthropicClient {
    /// Stream a message response, sending events to the provided channel.
    ///
    /// The `enable_cache` parameter controls whether prompt caching is used.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        use super::types::AnthropicToolDef;

        let model = model.unwrap_or(self.default_model());
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens());
        let messages: &[LlmMessage] = &super::image_cap::anthropic_shaped(messages);

        let anthropic_tools: Option<Vec<AnthropicToolDef>> = if tools.is_empty() {
            None
        } else {
            let mut at: Vec<AnthropicToolDef> = tools.iter().map(AnthropicToolDef::from).collect();
            if enable_cache {
                if let Some(last) = at.last_mut() {
                    last.cache_control = Some(super::anthropic::CacheControl::ephemeral());
                }
            }
            Some(at)
        };

        // Sanitize tool_use.input values — the API requires them to be JSON objects
        // but failover/streaming can produce null or other non-object types.
        let sanitized_messages: Vec<LlmMessage> = messages
            .iter()
            .map(|m| LlmMessage {
                role: m.role.clone(),
                content: m
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::ToolUse {
                            id,
                            name,
                            input,
                            signature,
                        } if !input.is_object() => {
                            tracing::warn!(
                                tool = name.as_str(),
                                "Sanitizing non-object tool_use.input before streaming API call"
                            );
                            ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: serde_json::Value::Object(serde_json::Map::new()),
                                signature: signature.clone(),
                            }
                        }
                        other => other.clone(),
                    })
                    .collect(),
            })
            .collect();

        let mut request = serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "stream": true,
            "messages": super::cache_utils::messages_with_cache_breakpoint(
                &sanitized_messages,
                enable_cache,
            ),
        });

        if let Some(system) = system_prompt {
            let blocks = super::anthropic::build_system_blocks(system, enable_cache);
            request["system"] =
                serde_json::to_value(&blocks).unwrap_or_else(|_| serde_json::json!(system));
        }
        if let Some(tools) = &anthropic_tools {
            request["tools"] = serde_json::to_value(tools)?;
        }
        if let Some(budget) = self.thinking_budget_tokens() {
            request["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }

        let url = self.message_url().to_string();
        let mut req = self
            .http_client()
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");

        req = self.apply_auth_headers(req);

        let response = req
            .json(&request)
            .send()
            .await
            .map_err(|e| eyre::eyre!("Streaming request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let error = response.text().await.unwrap_or_default();
            let mut err_msg = format!("Anthropic API error ({}): {}", status, error);
            if let Some(ref ra) = retry_after {
                err_msg.push_str(&format!(" [retry-after: {}s]", ra));
            }
            return Err(eyre::eyre!("{}", err_msg));
        }

        // Parse SSE stream
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut got_message_done = false;

        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE events
            while let Some(pos) = buffer.find("\n\n") {
                let event_text = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                if let Some(stream_event) = parse_sse_event(&event_text) {
                    // SSE error events must go through the error path (function
                    // return) not the data path (channel), so the failover chain
                    // can classify the error and retry/failover to the next model.
                    if let StreamEvent::Error { ref message } = stream_event {
                        return Err(eyre::eyre!("Anthropic SSE error: {}", message));
                    }
                    let is_done = matches!(stream_event, StreamEvent::MessageDone { .. });
                    if is_done {
                        got_message_done = true;
                    }
                    if tx.send(stream_event).await.is_err() {
                        return Ok(()); // Receiver dropped
                    }
                    if is_done {
                        return Ok(());
                    }
                }
            }
        }

        // Stream ended without a MessageDone event — this indicates an
        // incomplete response (empty body, truncated stream, or API error).
        // Return an error so the pool/failover chain retries with another provider.
        if got_message_done {
            Ok(())
        } else {
            Err(eyre::eyre!(
                "Anthropic API error: stream ended without message_delta (incomplete response)"
            ))
        }
    }
}

fn parse_sse_event(event_text: &str) -> Option<StreamEvent> {
    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_text.lines() {
        if let Some(val) = line.strip_prefix("event: ") {
            event_type = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("data: ") {
            data = val.trim().to_string();
        }
    }

    if data.is_empty() || data == "[DONE]" {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    parse_event_json(&event_type, &json)
}

/// Parse an Anthropic streaming event (already split into event type + JSON body)
/// into a [`StreamEvent`]. Shared between the direct-Anthropic SSE transport and
/// Bedrock's binary event-stream transport, whose payloads carry identical JSON.
pub fn parse_event_json(event_type: &str, json: &serde_json::Value) -> Option<StreamEvent> {
    match event_type {
        "content_block_start" => {
            let index = json["index"].as_u64()? as usize;
            let block = &json["content_block"];
            match block["type"].as_str()? {
                "text" => Some(StreamEvent::ContentStart { index }),
                "tool_use" => Some(StreamEvent::ToolUseStart {
                    index,
                    id: block["id"].as_str()?.to_string(),
                    name: block["name"].as_str()?.to_string(),
                    signature: None,
                }),
                "thinking" => Some(StreamEvent::ContentStart { index }),
                _ => None,
            }
        }
        "content_block_delta" => {
            let index = json["index"].as_u64()? as usize;
            let delta = &json["delta"];
            match delta["type"].as_str()? {
                "text_delta" => Some(StreamEvent::TextDelta {
                    index,
                    text: delta["text"].as_str()?.to_string(),
                }),
                "input_json_delta" => Some(StreamEvent::InputJsonDelta {
                    index,
                    partial: delta["partial_json"].as_str()?.to_string(),
                }),
                "thinking_delta" => Some(StreamEvent::ThinkingDelta {
                    index,
                    thinking: delta["thinking"].as_str().unwrap_or("").to_string(),
                }),
                "signature_delta" => Some(StreamEvent::SignatureDelta {
                    index,
                    signature: delta["signature"].as_str().unwrap_or("").to_string(),
                }),
                _ => None,
            }
        }
        "content_block_stop" => {
            let index = json["index"].as_u64()? as usize;
            Some(StreamEvent::ContentStop { index })
        }
        "message_delta" => {
            let stop_reason = json["delta"]["stop_reason"]
                .as_str()
                .map(|s| match s {
                    "end_turn" => StopReason::EndTurn,
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                })
                .unwrap_or(StopReason::EndTurn);

            let usage = TokenUsage {
                input_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
                output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
                cache_creation_input_tokens: json["usage"]["cache_creation_input_tokens"]
                    .as_u64()
                    .unwrap_or(0) as u32,
                cache_read_input_tokens: json["usage"]["cache_read_input_tokens"]
                    .as_u64()
                    .unwrap_or(0) as u32,
            };

            Some(StreamEvent::MessageDone { stop_reason, usage })
        }
        "message_stop" | "ping" => None,
        "error" => {
            let msg = json["error"]["message"]
                .as_str()
                .or_else(|| json["message"].as_str())
                .unwrap_or("Unknown streaming error");
            Some(StreamEvent::Error {
                message: msg.to_string(),
            })
        }
        _ => None,
    }
}

/// Accumulator that turns a sequence of `StreamEvent`s into content blocks.
///
/// Owns the per-block buffers so any consumer can drive event consumption
/// without re-implementing the same dance — see `collect_stream` (pure)
/// and `runner::process_stream_events` (which layers progress emission
/// on top).
pub struct StreamCollector {
    blocks: Vec<ContentBlock>,
    current_text: String,
    current_thinking: String,
    current_signature: Option<String>,
    current_tool_id: String,
    current_tool_name: String,
    current_tool_input: String,
    current_tool_signature: Option<String>,
    stop_reason: StopReason,
    usage: TokenUsage,
}

impl Default for StreamCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamCollector {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            current_text: String::new(),
            current_thinking: String::new(),
            current_signature: None,
            current_tool_id: String::new(),
            current_tool_name: String::new(),
            current_tool_input: String::new(),
            current_tool_signature: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    /// Apply one event. Returns `Err` if the event is `StreamEvent::Error`.
    pub fn process(&mut self, event: StreamEvent) -> Result<()> {
        match event {
            StreamEvent::ContentStart { .. } => {
                self.current_text.clear();
                self.current_thinking.clear();
                self.current_signature = None;
                self.current_tool_input.clear();
                self.current_tool_signature = None;
            }
            StreamEvent::TextDelta { text, .. } => self.current_text.push_str(&text),
            StreamEvent::ThinkingDelta { thinking, .. } => {
                self.current_thinking.push_str(&thinking)
            }
            StreamEvent::SignatureDelta { signature, .. } => match &mut self.current_signature {
                Some(existing) => existing.push_str(&signature),
                None => self.current_signature = Some(signature),
            },
            StreamEvent::ToolUseStart {
                id,
                name,
                signature,
                ..
            } => {
                self.current_tool_id = id;
                self.current_tool_name = name;
                self.current_tool_input.clear();
                self.current_tool_signature = signature;
            }
            StreamEvent::ToolSignatureUpdate { signature, .. } => {
                self.current_tool_signature = Some(signature);
            }
            StreamEvent::InputJsonDelta { partial, .. } => {
                self.current_tool_input.push_str(&partial)
            }
            StreamEvent::ContentStop { .. } => self.flush_open_block(),
            StreamEvent::MessageDone { stop_reason, usage } => {
                self.stop_reason = stop_reason;
                self.usage = usage;
            }
            StreamEvent::RateLimitWait { .. }
            | StreamEvent::SlowModelFailover { .. }
            | StreamEvent::KeepAlive { .. } => {}
            StreamEvent::Error { message } => {
                return Err(eyre::eyre!("Stream error: {}", message));
            }
        }
        Ok(())
    }

    fn flush_open_block(&mut self) {
        // A signature alone is a block too: encrypted reasoning with no
        // summary must still be echoed back.
        if !self.current_thinking.is_empty() || self.current_signature.is_some() {
            self.blocks.push(ContentBlock::Thinking {
                thinking: std::mem::take(&mut self.current_thinking),
                signature: self.current_signature.take(),
            });
        }
        if !self.current_text.is_empty() {
            let text = super::gemini::strip_thought_tags(&std::mem::take(&mut self.current_text));
            if !text.is_empty() {
                self.blocks.push(ContentBlock::Text { text });
            }
        }
        if !self.current_tool_name.is_empty() {
            let input: serde_json::Value = serde_json::from_str(&self.current_tool_input)
                .unwrap_or_else(|error| {
                    if !self.current_tool_input.is_empty() {
                        tracing::warn!(
                            tool = %self.current_tool_name,
                            bytes = self.current_tool_input.len(),
                            %error,
                            "Streamed tool input did not parse; the call carries no input"
                        );
                    }
                    serde_json::Value::Object(serde_json::Map::new())
                });
            self.blocks.push(ContentBlock::ToolUse {
                id: std::mem::take(&mut self.current_tool_id),
                name: std::mem::take(&mut self.current_tool_name),
                input: super::types::sanitize_tool_input(input),
                signature: self.current_tool_signature.take(),
            });
            self.current_tool_input.clear();
        }
    }

    /// Consume the collector and return the accumulated state.
    pub fn finish(self) -> (Vec<ContentBlock>, StopReason, TokenUsage) {
        (self.blocks, self.stop_reason, self.usage)
    }
}

/// Drive a `StreamCollector` to completion from an event channel.
pub async fn collect_stream(
    mut rx: mpsc::Receiver<StreamEvent>,
) -> Result<(Vec<ContentBlock>, StopReason, TokenUsage)> {
    let mut collector = StreamCollector::new();
    while let Some(event) = rx.recv().await {
        collector.process(event)?;
    }
    Ok(collector.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watchdog_limits(first_token: u64, stall: u64) -> StreamTimeouts {
        StreamTimeouts {
            first_token: Duration::from_secs(first_token),
            stall: Duration::from_secs(stall),
        }
    }

    async fn sleep_secs(secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_first_token_timeout_forwards_nothing() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        let call = async move {
            sleep_secs(600).await;
            let _ = inner_tx.send(StreamEvent::ContentStart { index: 0 }).await;
            Ok(())
        };

        let err = stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap_err();

        assert!(err.to_string().contains(FIRST_TOKEN_TIMEOUT_MARKER));
        assert!(
            rx.try_recv().is_err(),
            "nothing may reach the consumer on a first-token timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_slow_but_alive_stream_passes_untouched() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        let call = async move {
            sleep_secs(30).await; // within first_token
            inner_tx
                .send(StreamEvent::ContentStart { index: 0 })
                .await?;
            sleep_secs(100).await; // within stall, beyond first_token
            inner_tx
                .send(StreamEvent::TextDelta {
                    index: 0,
                    text: "hi".into(),
                })
                .await?;
            sleep_secs(100).await;
            inner_tx.send(StreamEvent::ContentStop { index: 0 }).await?;
            inner_tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
                .await?;
            Ok(())
        };

        stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap();

        let mut forwarded = 0;
        while rx.try_recv().is_ok() {
            forwarded += 1;
        }
        assert_eq!(forwarded, 4, "every event must be forwarded in order");
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_stall_after_first_event_errors_with_partials_forwarded() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        let call = async move {
            inner_tx
                .send(StreamEvent::ContentStart { index: 0 })
                .await?;
            inner_tx
                .send(StreamEvent::TextDelta {
                    index: 0,
                    text: "partial".into(),
                })
                .await?;
            sleep_secs(10_000).await;
            Ok(())
        };

        let err = stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap_err();

        assert!(err.to_string().contains(STREAM_STALL_MARKER));
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamEvent::ContentStart { .. })
        ));
        assert!(
            matches!(rx.try_recv(), Ok(StreamEvent::TextDelta { text, .. }) if text == "partial")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_rate_limit_wait_is_not_a_first_token() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        let call = async move {
            inner_tx
                .send(StreamEvent::RateLimitWait {
                    wait_secs: 20,
                    attempt: 1,
                })
                .await?;
            sleep_secs(10_000).await;
            Ok(())
        };

        let started = tokio::time::Instant::now();
        let err = stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains(FIRST_TOKEN_TIMEOUT_MARKER),
            "{err}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(65));
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamEvent::RateLimitWait { wait_secs: 20, .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_keep_alive_rearms_without_counting_as_first_token() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        let call = async move {
            for _ in 0..3 {
                inner_tx
                    .send(StreamEvent::KeepAlive { within_secs: 30 })
                    .await?;
                sleep_secs(30).await;
            }
            sleep_secs(10_000).await;
            Ok(())
        };

        let started = tokio::time::Instant::now();
        let err = stream_with_watchdog(call, inner_rx, tx, watchdog_limits(20, 5), "m")
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains(FIRST_TOKEN_TIMEOUT_MARKER),
            "still waiting for its first token: {err}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(60 + 30 + 20));
        let mut kept = 0;
        while let Ok(event) = rx.try_recv() {
            assert!(!event.carries_content(), "{event:?}");
            kept += 1;
        }
        assert_eq!(kept, 3, "forwarded for any watchdog further out");
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_drains_buffered_events_after_call_completes() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        // Completes instantly with events still buffered in the channel.
        let call = async move {
            inner_tx
                .send(StreamEvent::ContentStart { index: 0 })
                .await?;
            inner_tx.send(StreamEvent::ContentStop { index: 0 }).await?;
            inner_tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
                .await?;
            Ok(())
        };

        stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap();

        let mut forwarded = 0;
        while rx.try_recv().is_ok() {
            forwarded += 1;
        }
        assert_eq!(forwarded, 3, "buffered events must drain to the consumer");
    }

    /// A provider that drops its sender but keeps running (e.g. cloned the
    /// tx into a task that died) must still trip the first-token timer —
    /// the deadline stays armed while the call is pending.
    #[tokio::test(start_paused = true)]
    async fn watchdog_sender_dropped_early_still_times_out() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, _rx) = mpsc::channel(8);
        let call = async move {
            drop(inner_tx);
            sleep_secs(10_000).await;
            Ok(())
        };

        let err = stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap_err();

        assert!(err.to_string().contains(FIRST_TOKEN_TIMEOUT_MARKER));
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_zero_disables_timers() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, _rx) = mpsc::channel(8);
        let call = async move {
            sleep_secs(10_000).await;
            inner_tx
                .send(StreamEvent::MessageDone {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
                .await?;
            Ok(())
        };

        stream_with_watchdog(call, inner_rx, tx, watchdog_limits(0, 0), "m")
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_provider_error_propagates_after_partial_forward() {
        let (inner_tx, inner_rx) = mpsc::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        let call = async move {
            inner_tx
                .send(StreamEvent::ContentStart { index: 0 })
                .await?;
            Err(eyre::eyre!("upstream exploded"))
        };

        let err = stream_with_watchdog(call, inner_rx, tx, watchdog_limits(45, 180), "m")
            .await
            .unwrap_err();

        assert!(err.to_string().contains("upstream exploded"));
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamEvent::ContentStart { .. })
        ));
    }

    /// Pure UI signal: the accumulator must pass it through without
    /// touching collected content or erroring.
    #[test]
    fn collector_ignores_slow_model_failover() {
        let mut collector = StreamCollector::new();
        collector
            .process(StreamEvent::TextDelta {
                index: 0,
                text: "hello".to_string(),
            })
            .unwrap();
        collector
            .process(StreamEvent::SlowModelFailover {
                model: "gemini:slow".to_string(),
                elapsed_secs: 45,
            })
            .unwrap();
        collector
            .process(StreamEvent::ContentStop { index: 0 })
            .unwrap();
        let (blocks, _, _) = collector.finish();
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], ContentBlock::Text { text } if text == "hello"),
            "collected text must be unaffected by SlowModelFailover"
        );
    }

    /// Pure UI signal: the accumulator must pass it through without
    /// touching collected content or erroring.
    #[test]
    fn collector_ignores_rate_limit_wait() {
        let mut collector = StreamCollector::new();
        collector
            .process(StreamEvent::TextDelta {
                index: 0,
                text: "hello".to_string(),
            })
            .unwrap();
        collector
            .process(StreamEvent::RateLimitWait {
                wait_secs: 30,
                attempt: 1,
            })
            .unwrap();
        collector
            .process(StreamEvent::ContentStop { index: 0 })
            .unwrap();
        let (blocks, _, _) = collector.finish();
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], ContentBlock::Text { text } if text == "hello"),
            "collected text must be unaffected by RateLimitWait"
        );
    }

    #[test]
    fn parse_sse_text_delta() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::TextDelta { index: 0, ref text }) if text == "Hello"
        ));
    }

    #[test]
    fn parse_sse_content_start_text() {
        let event = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::ContentStart { index: 0 })
        ));
    }

    #[test]
    fn parse_sse_content_start_tool_use() {
        let event = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read_file\"}}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::ToolUseStart {
                index: 1,
                ref id,
                ref name,
                ..
            })
            if id == "toolu_01" && name == "read_file"
        ));
    }

    #[test]
    fn parse_sse_input_json_delta() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\"\"}}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::InputJsonDelta { index: 1, ref partial }) if partial == "{\"path\""
        ));
    }

    #[test]
    fn parse_sse_content_stop() {
        let event =
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::ContentStop { index: 0 })
        ));
    }

    #[test]
    fn parse_sse_message_delta() {
        let event = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":100,\"output_tokens\":50}}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::MessageDone {
                stop_reason: StopReason::EndTurn,
                ref usage
            }) if usage.input_tokens == 100 && usage.output_tokens == 50
        ));
    }

    #[test]
    fn parse_sse_message_delta_tool_use() {
        let event = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}";
        let result = parse_sse_event(event);
        assert!(matches!(
            result,
            Some(StreamEvent::MessageDone {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn parse_sse_ping_returns_none() {
        let event = "event: ping\ndata: {}";
        assert!(parse_sse_event(event).is_none());
    }

    #[test]
    fn parse_sse_message_stop_returns_none() {
        let event = "event: message_stop\ndata: {\"type\":\"message_stop\"}";
        assert!(parse_sse_event(event).is_none());
    }

    #[test]
    fn parse_sse_done_marker_returns_none() {
        let event = "event: done\ndata: [DONE]";
        assert!(parse_sse_event(event).is_none());
    }

    #[test]
    fn parse_sse_empty_data_returns_none() {
        let event = "event: content_block_delta\ndata: ";
        assert!(parse_sse_event(event).is_none());
    }

    #[tokio::test]
    async fn collect_stream_text_only() {
        let (tx, rx) = mpsc::channel(32);

        tx.send(StreamEvent::ContentStart { index: 0 }).await.ok();
        tx.send(StreamEvent::TextDelta {
            index: 0,
            text: "Hello".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::TextDelta {
            index: 0,
            text: " world".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::ContentStop { index: 0 }).await.ok();
        tx.send(StreamEvent::MessageDone {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        })
        .await
        .ok();
        drop(tx);

        let (blocks, stop_reason, usage) = collect_stream(rx).await.unwrap();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Hello world"));
        assert_eq!(stop_reason, StopReason::EndTurn);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn collect_stream_text_and_tool_use() {
        let (tx, rx) = mpsc::channel(32);

        // Text block
        tx.send(StreamEvent::ContentStart { index: 0 }).await.ok();
        tx.send(StreamEvent::TextDelta {
            index: 0,
            text: "Let me read that.".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::ContentStop { index: 0 }).await.ok();

        // Tool use block
        tx.send(StreamEvent::ToolUseStart {
            index: 1,
            id: "tool_abc".to_string(),
            name: "filesystem_read".to_string(),
            signature: None,
        })
        .await
        .ok();
        tx.send(StreamEvent::InputJsonDelta {
            index: 1,
            partial: "{\"path\":".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::InputJsonDelta {
            index: 1,
            partial: "\"/tmp/test.txt\"}".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::ContentStop { index: 1 }).await.ok();

        tx.send(StreamEvent::MessageDone {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 30,
                ..Default::default()
            },
        })
        .await
        .ok();
        drop(tx);

        let (blocks, stop_reason, _usage) = collect_stream(rx).await.unwrap();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Let me read that."));
        assert!(matches!(
            &blocks[1],
            ContentBlock::ToolUse { id, name, input, .. }
            if id == "tool_abc" && name == "filesystem_read"
            && input["path"].as_str() == Some("/tmp/test.txt")
        ));
        assert_eq!(stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn unparsable_streamed_tool_input_becomes_an_empty_object() {
        let mut collector = StreamCollector::new();
        for event in [
            StreamEvent::ToolUseStart {
                index: 0,
                id: "tool_cut".to_string(),
                name: "catalogue_remove_entry".to_string(),
                signature: None,
            },
            StreamEvent::InputJsonDelta {
                index: 0,
                partial: "{\"name\": \"fig".to_string(),
            },
            StreamEvent::ContentStop { index: 0 },
        ] {
            collector.process(event).unwrap();
        }
        let (blocks, _, _) = collector.finish();
        assert!(matches!(
            &blocks[..],
            [ContentBlock::ToolUse { input, .. }] if *input == serde_json::json!({})
        ));
    }

    #[tokio::test]
    async fn collect_stream_propagates_tool_use_signature() {
        let (tx, rx) = mpsc::channel(32);

        tx.send(StreamEvent::ContentStart { index: 0 }).await.ok();
        tx.send(StreamEvent::ToolUseStart {
            index: 0,
            id: "tool_xyz".to_string(),
            name: "get_time".to_string(),
            signature: Some("ErYFCrMF...".to_string()),
        })
        .await
        .ok();
        tx.send(StreamEvent::InputJsonDelta {
            index: 0,
            partial: "{}".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::ContentStop { index: 0 }).await.ok();
        tx.send(StreamEvent::MessageDone {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        })
        .await
        .ok();
        drop(tx);

        let (blocks, _, _) = collect_stream(rx).await.unwrap();
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolUse { signature: Some(s), .. } if s == "ErYFCrMF..."
        ));
    }

    /// Gemini OAI-compat streaming sometimes delivers `thought_signature` on a
    /// chunk *after* the one that carried `function.name`. ToolUseStart fires
    /// with `signature: None`, then a later `ToolSignatureUpdate` must attach
    /// the signature to the same tool slot before flush.
    #[tokio::test]
    async fn collect_stream_late_tool_signature_update() {
        let (tx, rx) = mpsc::channel(32);

        tx.send(StreamEvent::ContentStart { index: 0 }).await.ok();
        tx.send(StreamEvent::ToolUseStart {
            index: 0,
            id: "tool_late".to_string(),
            name: "knowledge_index".to_string(),
            signature: None,
        })
        .await
        .ok();
        tx.send(StreamEvent::InputJsonDelta {
            index: 0,
            partial: "{}".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::ToolSignatureUpdate {
            index: 0,
            signature: "LateSig123...".to_string(),
        })
        .await
        .ok();
        tx.send(StreamEvent::ContentStop { index: 0 }).await.ok();
        tx.send(StreamEvent::MessageDone {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        })
        .await
        .ok();
        drop(tx);

        let (blocks, _, _) = collect_stream(rx).await.unwrap();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolUse { signature: Some(s), .. } if s == "LateSig123..."
        ));
    }

    #[tokio::test]
    async fn collect_stream_error_propagates() {
        let (tx, rx) = mpsc::channel(32);

        tx.send(StreamEvent::Error {
            message: "Rate limit".to_string(),
        })
        .await
        .ok();
        drop(tx);

        let result = collect_stream(rx).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Rate limit"));
    }

    fn collect(events: impl IntoIterator<Item = StreamEvent>) -> Vec<ContentBlock> {
        let mut collector = StreamCollector::new();
        events
            .into_iter()
            .try_for_each(|e| collector.process(e))
            .expect("no error events");
        collector.finish().0
    }

    #[test]
    fn reasoning_only_item_without_summary_survives_collector() {
        let blocks = collect([
            StreamEvent::ContentStart { index: 0 },
            StreamEvent::SignatureDelta {
                index: 0,
                signature: "oai-reasoning:v1:{}".to_string(),
            },
            StreamEvent::ContentStop { index: 0 },
        ]);
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Thinking { thinking, signature: Some(s) }]
                if thinking.is_empty() && s == "oai-reasoning:v1:{}"
        ));
    }

    #[test]
    fn existing_anthropic_and_bedrock_stream_fixtures_unchanged_by_collector_change() {
        let sse = [
            r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":12}}}"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Weather first."}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCkYIAxgCIkD"}}"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Checking."}}"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":1}"#,
            r#"event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"weather_get","input":{}}}"#,
            r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"Porto\"}"}}"#,
            r#"event: content_block_stop
data: {"type":"content_block_stop","index":2}"#,
            r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
        ];
        let expected = serde_json::json!([
            {"type": "thinking", "thinking": "Weather first.", "signature": "EqQBCkYIAxgCIkD"},
            {"type": "text", "text": "Checking."},
            {"type": "tool_use", "id": "toolu_1", "name": "weather_get", "input": {"city": "Porto"}},
        ]);
        let blocks = collect(sse.iter().filter_map(|e| parse_sse_event(e)));
        assert_eq!(serde_json::to_value(&blocks).unwrap(), expected);

        let converse = [
            (
                "contentBlockStart",
                serde_json::json!({"contentBlockIndex": 0, "start": {}}),
            ),
            (
                "contentBlockDelta",
                serde_json::json!({"contentBlockIndex": 0,
                "delta": {"reasoningContent": {"text": "Weather first."}}}),
            ),
            (
                "contentBlockDelta",
                serde_json::json!({"contentBlockIndex": 0,
                "delta": {"reasoningContent": {"signature": "EqQBCkYIAxgCIkD"}}}),
            ),
            (
                "contentBlockStop",
                serde_json::json!({"contentBlockIndex": 0}),
            ),
            (
                "contentBlockStart",
                serde_json::json!({"contentBlockIndex": 1, "start": {}}),
            ),
            (
                "contentBlockDelta",
                serde_json::json!({"contentBlockIndex": 1,
                "delta": {"text": "Checking."}}),
            ),
            (
                "contentBlockStop",
                serde_json::json!({"contentBlockIndex": 1}),
            ),
            (
                "contentBlockStart",
                serde_json::json!({"contentBlockIndex": 2,
                "start": {"toolUse": {"toolUseId": "toolu_1", "name": "weather_get"}}}),
            ),
            (
                "contentBlockDelta",
                serde_json::json!({"contentBlockIndex": 2,
                "delta": {"toolUse": {"input": "{\"city\":\"Porto\"}"}}}),
            ),
            (
                "contentBlockStop",
                serde_json::json!({"contentBlockIndex": 2}),
            ),
            ("messageStop", serde_json::json!({"stopReason": "tool_use"})),
        ];
        let mut state = super::super::bedrock_converse::ConverseStreamState::new();
        let blocks = collect(converse.iter().filter_map(|(kind, payload)| {
            super::super::bedrock_converse::map_event(kind, payload, &mut state)
                .expect("no service exception")
        }));
        assert_eq!(serde_json::to_value(&blocks).unwrap(), expected);
    }

    #[test]
    fn stream_event_is_clone_and_debug() {
        let events = vec![
            StreamEvent::ContentStart { index: 0 },
            StreamEvent::TextDelta {
                index: 0,
                text: "hi".to_string(),
            },
            StreamEvent::ToolUseStart {
                index: 1,
                id: "t1".to_string(),
                name: "tool".to_string(),
                signature: None,
            },
            StreamEvent::InputJsonDelta {
                index: 1,
                partial: "{}".to_string(),
            },
            StreamEvent::SignatureDelta {
                index: 0,
                signature: "sig".to_string(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::MessageDone {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            },
            StreamEvent::RateLimitWait {
                wait_secs: 30,
                attempt: 1,
            },
            StreamEvent::Error {
                message: "err".to_string(),
            },
        ];
        for event in &events {
            let cloned = event.clone();
            let _ = format!("{:?}", cloned);
        }
    }
}
