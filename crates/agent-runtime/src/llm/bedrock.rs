//! AWS Bedrock provider — dispatches to either the Anthropic-Messages Invoke
//! path or the unified Converse path based on the model ID. Both paths share
//! the SigV4 signing helper, the URL builder, the event-stream frame decoder,
//! and the region resolution logic below; credential sourcing lives in
//! [`super::bedrock_credentials`].

use std::sync::Arc;
use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningSettings};
use aws_sigv4::sign::v4;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use futures::StreamExt;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;

use super::bedrock_capabilities::{self, WireFormat};
use super::bedrock_catalog::{self, BedrockCatalogStore, CatalogState};
use super::bedrock_converse::{self, ConverseStreamState};
use super::bedrock_credentials::{BedrockCredentials, CredentialResolver};
use super::bedrock_eventstream::FrameDecoder;
use super::bedrock_invoke;
use super::bedrock_openai;
use super::streaming::{parse_event_json, StreamEvent};
use super::types::{LlmMessage, LlmResponse, LlmToolDef};

const SERVICE: &str = "bedrock";
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_MAX_TOKENS: u32 = 8192;
const DEFAULT_MODEL: &str = "anthropic.claude-sonnet-5-5";

const PATH_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b':')
    .add(b'?')
    .add(b'#')
    .add(b'[')
    .add(b']')
    .add(b'@')
    .add(b'!')
    .add(b'$')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b';')
    .add(b'=')
    .add(b'%');

/// Resolve the AWS region for Bedrock from explicit config or the standard
/// AWS environment variables, falling back to `us-east-1`.
pub fn resolve_region(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("BEDROCK_REGION").ok())
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .unwrap_or_else(|| DEFAULT_REGION.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    No,
    Yes,
}

/// Build the runtime URL for a Bedrock request.
pub fn invoke_url(region: &str, model_id: &str, wire: WireFormat, streaming: bool) -> String {
    let encoded = utf8_percent_encode(model_id, PATH_ENCODE).to_string();
    let suffix = match (wire, streaming) {
        (WireFormat::Invoke, false) | (WireFormat::OpenAi, false) => "invoke",
        (WireFormat::Invoke, true) | (WireFormat::OpenAi, true) => "invoke-with-response-stream",
        (WireFormat::Converse, false) => "converse",
        (WireFormat::Converse, true) => "converse-stream",
    };
    format!("https://bedrock-runtime.{region}.amazonaws.com/model/{encoded}/{suffix}")
}

pub struct BedrockClient {
    client: Client,
    resolver: CredentialResolver,
    region: String,
    default_model: String,
    default_max_tokens: u32,
    thinking_budget_tokens: Option<u32>,
    catalog: Option<Arc<BedrockCatalogStore>>,
}

impl BedrockClient {
    pub fn new(resolver: CredentialResolver, region: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| Client::new()),
            resolver,
            region: region.into(),
            default_model: DEFAULT_MODEL.to_string(),
            default_max_tokens: DEFAULT_MAX_TOKENS,
            thinking_budget_tokens: None,
            catalog: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = max_tokens;
        self
    }

    pub fn with_thinking_budget(mut self, budget_tokens: u32) -> Self {
        self.thinking_budget_tokens = Some(budget_tokens);
        self
    }

    /// The thinking budget for the request now being built: the per-turn
    /// override when one is in scope (see
    /// [`crate::llm::provider::effective_thinking_budget`]), otherwise this
    /// client's configured budget.
    fn effective_thinking_budget(&self) -> Option<u32> {
        super::provider::effective_thinking_budget(self.thinking_budget_tokens)
    }

    pub fn with_catalog(mut self, catalog: Arc<BedrockCatalogStore>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Best-effort catalog rewrite at invocation time. Catches paths that
    /// bypass `SystemSetProviderTool` — workflow steps, session-model
    /// overrides, boot-time provider config — so every AWS call carries the
    /// canonical inference-profile id when one exists. Returns the input
    /// unchanged when no catalog is attached or the catalog has no match.
    async fn canonicalize(&self, model: &str) -> String {
        match &self.catalog {
            Some(catalog) => catalog.resolve(model, &self.region).await,
            None => model.to_string(),
        }
    }

    pub async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let raw_model = strip_bedrock_prefix(model.unwrap_or(&self.default_model));
        let canonical = self.canonicalize(raw_model).await;
        let model = canonical.as_str();
        warn_unresolved_model(raw_model, model, &self.region);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        let wire = bedrock_capabilities::wire_format_for(model);
        tracing::info!(
            provider = "bedrock",
            model_in = %raw_model,
            model_out = %model,
            rewritten = raw_model != model,
            streaming = false,
            wire = ?wire,
            region = %self.region,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            catalog_attached = self.catalog.is_some(),
            "LLM invocation start"
        );
        let body = build_body_for(
            wire,
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            self.effective_thinking_budget(),
        );

        let url = invoke_url(&self.region, model, wire, false);
        let response = self.send_signed(&url, &body, Stream::No, model).await?;
        let response_body = response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| eyre!("Failed to read Bedrock response body: {e}"))?;
        let value: Value = serde_json::from_slice(&response_body)
            .map_err(|e| eyre!("Failed to parse Bedrock response JSON: {e}"))?;

        let (content, stop_reason, usage, resolved_model) = match wire {
            WireFormat::Invoke => bedrock_invoke::parse_response(value, model)?,
            WireFormat::Converse => bedrock_converse::parse_response(value, model)?,
            WireFormat::OpenAi => bedrock_openai::parse_response(value, model)?,
        };

        tracing::info!(
            model = %resolved_model,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read = usage.cache_read_input_tokens,
            cache_write = usage.cache_creation_input_tokens,
            content_blocks = content.len(),
            stop_reason = ?stop_reason,
            "Bedrock response received"
        );

        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
            model: resolved_model,
        })
    }

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
        let raw_model = strip_bedrock_prefix(model.unwrap_or(&self.default_model));
        let canonical = self.canonicalize(raw_model).await;
        let model = canonical.as_str();
        warn_unresolved_model(raw_model, model, &self.region);
        let max_tokens = max_tokens.unwrap_or(self.default_max_tokens);
        let wire = bedrock_capabilities::wire_format_for(model);
        tracing::info!(
            provider = "bedrock",
            model_in = %raw_model,
            model_out = %model,
            rewritten = raw_model != model,
            streaming = true,
            wire = ?wire,
            region = %self.region,
            max_tokens,
            messages = messages.len(),
            tools = tools.len(),
            catalog_attached = self.catalog.is_some(),
            "LLM invocation start"
        );
        let body = build_body_for(
            wire,
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            self.effective_thinking_budget(),
        );

        let url = invoke_url(&self.region, model, wire, true);
        let response = self.send_signed(&url, &body, Stream::Yes, model).await?;
        let mut stream = response.bytes_stream();
        let mut decoder = FrameDecoder::new();
        match wire {
            WireFormat::Invoke => run_invoke_stream(&mut stream, &mut decoder, &tx).await,
            WireFormat::Converse => run_converse_stream(&mut stream, &mut decoder, &tx).await,
            WireFormat::OpenAi => {
                bedrock_openai::run_openai_stream(&mut stream, &mut decoder, &tx).await
            }
        }
    }

    async fn send_signed(
        &self,
        url: &str,
        body: &Value,
        stream: Stream,
        model: &str,
    ) -> Result<reqwest::Response> {
        let body_bytes = serde_json::to_vec(body)?;
        let response = self
            .signed_request_builder(url, &body_bytes, stream)
            .await?
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| eyre!("Bedrock HTTP request failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format_response_error(
                response,
                model,
                &self.region,
                self.catalog.as_deref(),
            )
            .await);
        }
        Ok(response)
    }

    async fn signed_request_builder(
        &self,
        url: &str,
        body: &[u8],
        stream: Stream,
    ) -> Result<reqwest::RequestBuilder> {
        let credentials = self.resolver.resolve().await?;
        let mut headers = build_request_headers(stream);
        let signed = sign_v4(
            "POST",
            url,
            &headers,
            body,
            &credentials,
            &self.region,
            SystemTime::now(),
        )?;
        headers.extend(signed);
        let mut req = self.client.post(url);
        for (name, value) in &headers {
            req = req.header(name.as_str(), value.as_str());
        }
        Ok(req)
    }
}

#[allow(clippy::too_many_arguments)]
fn build_body_for(
    wire: WireFormat,
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    max_tokens: u32,
    model: &str,
    enable_cache: bool,
    thinking_budget: Option<u32>,
) -> Value {
    match wire {
        WireFormat::Invoke => bedrock_invoke::build_body(
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            thinking_budget,
        ),
        WireFormat::Converse => bedrock_converse::build_body(
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            thinking_budget,
        ),
        WireFormat::OpenAi => bedrock_openai::build_body(
            system_prompt,
            messages,
            tools,
            max_tokens,
            model,
            enable_cache,
            thinking_budget,
        ),
    }
}

async fn format_response_error(
    response: reqwest::Response,
    model: &str,
    region: &str,
    catalog: Option<&BedrockCatalogStore>,
) -> eyre::Report {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let amzn_request_id = response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body_text = response.text().await.unwrap_or_default();
    let mut msg = format!("Bedrock API error ({status}): {body_text}");
    if let Some(rid) = amzn_request_id {
        msg.push_str(&format!(" [request-id: {rid}]"));
    }
    if let Some(ra) = retry_after {
        msg.push_str(&format!(" [retry-after: {ra}s]"));
    }
    if status == reqwest::StatusCode::BAD_REQUEST && looks_like_invalid_model_error(&body_text) {
        if let Some(suffix) = catalog_suggestion_suffix(catalog, model, region).await {
            msg.push_str(&suffix);
        }
    }
    eyre!("{}", msg)
}

fn looks_like_invalid_model_error(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("invalid") && lower.contains("model")
}

async fn catalog_suggestion_suffix(
    catalog: Option<&BedrockCatalogStore>,
    model: &str,
    region: &str,
) -> Option<String> {
    let store = catalog?;
    let state = store.get(region).await;
    let catalog = match state {
        CatalogState::Ready(c) => c,
        CatalogState::Unavailable { .. } => return None,
    };
    let suggestions = bedrock_catalog::suggest(&catalog, model, 3);
    (!suggestions.is_empty()).then(|| {
        let listed = suggestions
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(" [available alternatives: {listed}]")
    })
}

fn strip_bedrock_prefix(model: &str) -> &str {
    model.strip_prefix("bedrock:").unwrap_or(model)
}

/// Flag a model that survived canonicalization without looking like a
/// Bedrock id — the invocation is near-certain to be rejected with
/// "provided model identifier is invalid".
fn warn_unresolved_model(raw: &str, canonical: &str, region: &str) {
    if raw == canonical && !super::tiers::is_bedrock_model_id(canonical) {
        tracing::warn!(
            model = %canonical,
            region = %region,
            "Model not canonicalized to a Bedrock id — Bedrock will likely reject it"
        );
    }
}

fn build_request_headers(stream: Stream) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        // The model output payload format. Always JSON — Bedrock wraps each
        // JSON chunk into the eventstream envelope when streaming. Sending
        // "vnd.amazon.eventstream" here is a category error (that's the
        // wire format, not a model output format) and newer Anthropic models
        // (Claude 4.6+) reject it with "Accept Type is invalid or not
        // supported for this model".
        (
            "x-amzn-bedrock-accept".to_string(),
            "application/json".to_string(),
        ),
    ];
    let accept = match stream {
        Stream::Yes => "application/vnd.amazon.eventstream",
        Stream::No => "application/json",
    };
    headers.push(("accept".to_string(), accept.to_string()));
    headers
}

/// Sign an HTTP request with AWS SigV4 and return the headers that must be
/// added to it. Pure with respect to `time` for testability.
pub fn sign_v4(
    method: &str,
    url: &str,
    existing_headers: &[(String, String)],
    body: &[u8],
    creds: &BedrockCredentials,
    region: &str,
    time: SystemTime,
) -> Result<Vec<(String, String)>> {
    let credentials = Credentials::new(
        creds.access_key_id.clone(),
        creds.secret_access_key.clone(),
        creds.session_token.clone(),
        None,
        "workforce-bedrock",
    );
    let identity = credentials.into();

    let settings = SigningSettings::default();
    let params: aws_sigv4::http_request::SigningParams = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SERVICE)
        .time(time)
        .settings(settings)
        .build()
        .map_err(|e| eyre!("SigV4 params build failed: {e}"))?
        .into();

    let header_refs: Vec<(&str, &str)> = existing_headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let signable = SignableRequest::new(
        method,
        url,
        header_refs.into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|e| eyre!("SigV4 SignableRequest failed: {e}"))?;

    let (instructions, _signature) = sign(signable, &params)
        .map_err(|e| eyre!("SigV4 sign failed: {e}"))?
        .into_parts();
    let (signed_headers, _) = instructions.into_parts();
    Ok(signed_headers
        .into_iter()
        .map(|h| (h.name().to_string(), h.value().to_string()))
        .collect())
}

async fn run_invoke_stream<S, E>(
    stream: &mut S,
    decoder: &mut FrameDecoder,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<()>
where
    S: futures::Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| eyre!("Bedrock stream read failed: {e}"))?;
        decoder.feed(&chunk);
        while let Some(frame) = decoder.next_frame()? {
            if frame.message_type == "exception" {
                return Err(eyre!(
                    "Bedrock streaming exception ({}): {}",
                    frame.exception_type,
                    String::from_utf8_lossy(&frame.payload)
                ));
            }
            let inner = decode_chunk_payload(&frame.payload)?;
            let event_type = inner
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if event_type == "error" {
                let message = inner["error"]["message"]
                    .as_str()
                    .or_else(|| inner["message"].as_str())
                    .unwrap_or("Unknown Bedrock streaming error");
                return Err(eyre!("Bedrock streaming error: {message}"));
            }
            if let Some(event) = parse_event_json(&event_type, &inner) {
                let done = matches!(event, StreamEvent::MessageDone { .. });
                if let StreamEvent::Error { message } = &event {
                    return Err(eyre!("Bedrock streaming error: {message}"));
                }
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
                if done {
                    return Ok(());
                }
            }
        }
    }
    Err(eyre!(
        "Bedrock streaming API error: stream ended without message_delta"
    ))
}

async fn run_converse_stream<S, E>(
    stream: &mut S,
    decoder: &mut FrameDecoder,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<()>
where
    S: futures::Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut state = ConverseStreamState::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| eyre!("Bedrock stream read failed: {e}"))?;
        decoder.feed(&chunk);
        while let Some(frame) = decoder.next_frame()? {
            if frame.message_type == "exception" {
                return Err(eyre!(
                    "Bedrock streaming exception ({}): {}",
                    frame.exception_type,
                    String::from_utf8_lossy(&frame.payload)
                ));
            }
            let payload: Value = serde_json::from_slice(&frame.payload)
                .map_err(|e| eyre!("Bedrock Converse frame JSON parse failed: {e}"))?;
            if let Some(event) =
                bedrock_converse::map_event(&frame.event_type, &payload, &mut state)?
            {
                let done = matches!(event, StreamEvent::MessageDone { .. });
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
                if done {
                    return Ok(());
                }
            }
        }
    }
    if let Some(flush) = bedrock_converse::flush_terminal(&mut state) {
        let _ = tx.send(flush).await;
        return Ok(());
    }
    Err(eyre!(
        "Bedrock Converse streaming API error: stream ended without messageStop"
    ))
}

fn decode_chunk_payload(payload: &[u8]) -> Result<Value> {
    let outer: Value = serde_json::from_slice(payload)
        .map_err(|e| eyre!("Bedrock Invoke frame JSON parse failed: {e}"))?;
    if let Some(b64) = outer.get("bytes").and_then(Value::as_str) {
        let decoded = BASE64
            .decode(b64)
            .map_err(|e| eyre!("Bedrock Invoke frame base64 decode failed: {e}"))?;
        serde_json::from_slice(&decoded)
            .map_err(|e| eyre!("Bedrock Invoke decoded payload JSON parse failed: {e}"))
    } else {
        Ok(outer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::bedrock_eventstream::encode_frame;
    use crate::llm::types::StopReason;
    use serde_json::json;

    fn creds() -> BedrockCredentials {
        BedrockCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        }
    }

    fn static_resolver() -> CredentialResolver {
        CredentialResolver::Static(creds())
    }

    #[test]
    fn invoke_url_encodes_colon_in_model_id() {
        let url = invoke_url(
            "us-east-1",
            "meta.llama4-maverick-17b-instruct-v1:0",
            WireFormat::Converse,
            false,
        );
        assert_eq!(
            url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/meta.llama4-maverick-17b-instruct-v1%3A0/converse"
        );
    }

    #[test]
    fn invoke_url_inference_profile_streaming() {
        let url = invoke_url(
            "us-east-1",
            "us.anthropic.claude-opus-4-7",
            WireFormat::Invoke,
            true,
        );
        assert!(url.ends_with("/invoke-with-response-stream"));
        assert!(url.contains("us.anthropic.claude-opus-4-7"));
    }

    #[test]
    fn converse_url_streaming_without_colon() {
        let url = invoke_url("eu-west-1", "zai.glm-5", WireFormat::Converse, true);
        assert!(url.ends_with("/converse-stream"));
        assert!(url.contains("zai.glm-5"));
        assert!(!url.contains("%3A"));
    }

    #[test]
    fn sign_v4_produces_expected_headers() {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let headers = vec![("content-type".to_string(), "application/json".to_string())];
        let signed = sign_v4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            &headers,
            b"{}",
            &creds(),
            "us-east-1",
            time,
        )
        .unwrap();
        let names: std::collections::HashSet<String> =
            signed.iter().map(|(k, _)| k.to_lowercase()).collect();
        assert!(names.contains("authorization"));
        assert!(names.contains("x-amz-date"));
    }

    #[test]
    fn sign_v4_includes_session_token_when_present() {
        let mut c = creds();
        c.session_token = Some("EXAMPLETOKEN".to_string());
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let signed = sign_v4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            &[],
            b"{}",
            &c,
            "us-east-1",
            time,
        )
        .unwrap();
        let names: std::collections::HashSet<String> =
            signed.iter().map(|(k, _)| k.to_lowercase()).collect();
        assert!(names.contains("x-amz-security-token"));
    }

    #[test]
    fn sign_v4_is_deterministic_for_fixed_time() {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let a = sign_v4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            &[],
            b"hello",
            &creds(),
            "us-east-1",
            time,
        )
        .unwrap();
        let b = sign_v4(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            &[],
            b"hello",
            &creds(),
            "us-east-1",
            time,
        )
        .unwrap();
        let extract = |hs: &[(String, String)], n: &str| {
            hs.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(extract(&a, "authorization"), extract(&b, "authorization"));
    }

    #[test]
    fn build_request_headers_streaming_announces_eventstream() {
        let h = build_request_headers(Stream::Yes);
        let find = |name: &str| {
            h.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(find("accept"), "application/vnd.amazon.eventstream");
        assert_eq!(find("x-amzn-bedrock-accept"), "application/json");
        assert_eq!(find("content-type"), "application/json");
    }

    #[test]
    fn build_request_headers_nonstreaming_uses_json_accept() {
        let h = build_request_headers(Stream::No);
        let find = |name: &str| {
            h.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(find("accept"), "application/json");
        assert_eq!(find("x-amzn-bedrock-accept"), "application/json");
        assert_eq!(find("content-type"), "application/json");
    }

    #[test]
    fn decode_chunk_payload_extracts_base64_inner() {
        let inner_json = b"{\"type\":\"message_stop\"}";
        let outer = json!({ "bytes": BASE64.encode(inner_json) });
        let inner = decode_chunk_payload(outer.to_string().as_bytes()).unwrap();
        assert_eq!(inner["type"].as_str(), Some("message_stop"));
    }

    #[test]
    fn decode_chunk_payload_passes_through_plain_json() {
        let payload = br#"{"type":"messageStart"}"#;
        let v = decode_chunk_payload(payload).unwrap();
        assert_eq!(v["type"].as_str(), Some("messageStart"));
    }

    #[test]
    fn resolve_region_explicit_wins() {
        std::env::remove_var("BEDROCK_REGION");
        assert_eq!(resolve_region(Some("ap-northeast-1")), "ap-northeast-1");
    }

    #[tokio::test]
    async fn run_invoke_stream_routes_through_parse_event_json() {
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(32);
        let frames = [
            encode_frame(
                &[(":event-type", "chunk"), (":message-type", "event")],
                &wrap_invoke_event(json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                })),
            ),
            encode_frame(
                &[(":event-type", "chunk"), (":message-type", "event")],
                &wrap_invoke_event(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "hi"},
                })),
            ),
            encode_frame(
                &[(":event-type", "chunk"), (":message-type", "event")],
                &wrap_invoke_event(json!({
                    "type": "content_block_stop",
                    "index": 0,
                })),
            ),
            encode_frame(
                &[(":event-type", "chunk"), (":message-type", "event")],
                &wrap_invoke_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                })),
            ),
        ];
        let combined: Vec<u8> = frames.into_iter().flatten().collect();
        let stream = futures::stream::iter(vec![Ok::<bytes::Bytes, std::io::Error>(
            bytes::Bytes::from(combined),
        )]);
        let mut stream = Box::pin(stream);
        let mut decoder = FrameDecoder::new();
        run_invoke_stream(&mut stream, &mut decoder, &tx)
            .await
            .unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        assert!(matches!(events[0], StreamEvent::ContentStart { index: 0 }));
        assert!(matches!(&events[1], StreamEvent::TextDelta { text, .. } if text == "hi"));
        assert!(matches!(events[2], StreamEvent::ContentStop { index: 0 }));
        assert!(matches!(
            events.last().unwrap(),
            StreamEvent::MessageDone {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    fn wrap_invoke_event(inner: Value) -> Vec<u8> {
        let inner_bytes = serde_json::to_vec(&inner).unwrap();
        let outer = json!({ "bytes": BASE64.encode(&inner_bytes) });
        serde_json::to_vec(&outer).unwrap()
    }

    #[tokio::test]
    async fn run_converse_stream_buffers_message_stop_until_metadata() {
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(32);
        let frames = [
            encode_frame(
                &[(":event-type", "messageStart"), (":message-type", "event")],
                br#"{"role":"assistant"}"#,
            ),
            encode_frame(
                &[
                    (":event-type", "contentBlockDelta"),
                    (":message-type", "event"),
                ],
                br#"{"contentBlockIndex":0,"delta":{"text":"hello"}}"#,
            ),
            encode_frame(
                &[
                    (":event-type", "contentBlockStop"),
                    (":message-type", "event"),
                ],
                br#"{"contentBlockIndex":0}"#,
            ),
            encode_frame(
                &[(":event-type", "messageStop"), (":message-type", "event")],
                br#"{"stopReason":"end_turn"}"#,
            ),
            encode_frame(
                &[(":event-type", "metadata"), (":message-type", "event")],
                br#"{"usage":{"inputTokens":3,"outputTokens":1}}"#,
            ),
        ];
        let combined: Vec<u8> = frames.into_iter().flatten().collect();
        let stream = futures::stream::iter(vec![Ok::<bytes::Bytes, std::io::Error>(
            bytes::Bytes::from(combined),
        )]);
        let mut stream = Box::pin(stream);
        let mut decoder = FrameDecoder::new();
        run_converse_stream(&mut stream, &mut decoder, &tx)
            .await
            .unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        assert!(matches!(&events[0], StreamEvent::TextDelta { text, .. } if text == "hello"));
        assert!(matches!(events[1], StreamEvent::ContentStop { index: 0 }));
        assert!(matches!(
            events.last().unwrap(),
            StreamEvent::MessageDone { stop_reason: StopReason::EndTurn, ref usage }
                if usage.input_tokens == 3 && usage.output_tokens == 1
        ));
    }

    #[tokio::test]
    async fn run_converse_stream_propagates_exception() {
        let (tx, _rx) = mpsc::channel::<StreamEvent>(32);
        let frame = encode_frame(
            &[
                (":event-type", "ThrottlingException"),
                (":message-type", "exception"),
                (":exception-type", "ThrottlingException"),
            ],
            br#"{"message":"too many"}"#,
        );
        let stream = futures::stream::iter(vec![Ok::<bytes::Bytes, std::io::Error>(
            bytes::Bytes::from(frame),
        )]);
        let mut stream = Box::pin(stream);
        let mut decoder = FrameDecoder::new();
        let result = run_converse_stream(&mut stream, &mut decoder, &tx).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ThrottlingException"));
    }

    #[test]
    fn strip_bedrock_prefix_removes_only_known_prefix() {
        assert_eq!(strip_bedrock_prefix("bedrock:zai.glm-5"), "zai.glm-5");
        assert_eq!(
            strip_bedrock_prefix("anthropic.claude-sonnet-4-6"),
            "anthropic.claude-sonnet-4-6"
        );
    }

    #[tokio::test]
    async fn bedrock_client_routes_each_wire_format() {
        let anth = BedrockClient::new(static_resolver(), "us-east-1")
            .with_model("us.anthropic.claude-sonnet-4-6");
        assert_eq!(
            bedrock_capabilities::wire_format_for(strip_bedrock_prefix(anth.default_model())),
            WireFormat::Invoke
        );

        let zai = BedrockClient::new(static_resolver(), "us-east-1").with_model("zai.glm-5");
        assert_eq!(
            bedrock_capabilities::wire_format_for(strip_bedrock_prefix(zai.default_model())),
            WireFormat::OpenAi
        );

        let llama = BedrockClient::new(static_resolver(), "us-east-1")
            .with_model("meta.llama3-70b-instruct-v1:0");
        assert_eq!(
            bedrock_capabilities::wire_format_for(strip_bedrock_prefix(llama.default_model())),
            WireFormat::Converse
        );
    }

    #[test]
    fn invoke_url_openai_routes_to_invoke_endpoint() {
        let url = invoke_url("eu-west-2", "zai.glm-5", WireFormat::OpenAi, false);
        assert!(url.ends_with("/invoke"));
        assert!(url.contains("zai.glm-5"));

        let stream = invoke_url("eu-west-2", "zai.glm-5", WireFormat::OpenAi, true);
        assert!(stream.ends_with("/invoke-with-response-stream"));
    }
}
