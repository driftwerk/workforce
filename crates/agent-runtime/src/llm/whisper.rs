//! HTTP client for OpenAI-compatible Whisper transcription endpoints.
//!
//! Targets `/v1/audio/transcriptions` with `multipart/form-data`. Used for
//! both OpenAI Whisper (default base URL) and Groq's Whisper hosting (with
//! `https://api.groq.com/openai/v1` base URL). Self-hosted vLLM Whisper
//! deployments using the same OpenAI-compatible schema work too via a
//! configurable base URL.

use eyre::{eyre, Result};
use reqwest::{multipart, Client};
use serde::Deserialize;
use workforce_common::secret::{scrub, KeySlot};

/// One transcription request.
pub struct WhisperRequest<'a> {
    pub model: &'a str,
    pub audio: Vec<u8>,
    pub filename: &'a str,
    pub mime_type: &'a str,
    /// Optional ISO-639-1 language hint; omit for auto-detect.
    pub language: Option<&'a str>,
    /// Free-form context to bias the transcription.
    pub prompt: Option<&'a str>,
}

/// Subset of the Whisper response we care about. The `verbose_json` format
/// also returns `language` and `duration`; we request it so we can surface
/// duration for cost attribution.
#[derive(Debug, Deserialize)]
pub struct WhisperResponse {
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
}

/// Client for Whisper-style transcription endpoints. The key is read from
/// its slot on every request, for whoever the request is for.
#[derive(Clone)]
pub struct WhisperClient {
    client: Client,
    api_key: KeySlot,
    /// A hosted endpoint takes no request without a key; a self-hosted one
    /// may be open.
    requires_key: bool,
    base_url: String,
}

impl WhisperClient {
    pub fn openai(api_key: KeySlot) -> Self {
        Self::hosted(api_key, "https://api.openai.com/v1")
    }

    pub fn groq(api_key: KeySlot) -> Self {
        Self::hosted(api_key, "https://api.groq.com/openai/v1")
    }

    fn hosted(api_key: KeySlot, base_url: &str) -> Self {
        Self {
            requires_key: true,
            ..Self::new(api_key, base_url)
        }
    }

    /// A self-hosted endpoint, which is sent the key when there is one.
    pub fn new(api_key: KeySlot, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            requires_key: false,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn transcribe(&self, req: WhisperRequest<'_>) -> Result<WhisperResponse> {
        let api_key = match self.requires_key {
            true => Some(self.api_key.get()?),
            false => self.api_key.peek(),
        };
        let mut form = multipart::Form::new()
            .text("model", req.model.to_string())
            .text("response_format", "verbose_json")
            .part(
                "file",
                multipart::Part::bytes(req.audio)
                    .file_name(req.filename.to_string())
                    .mime_str(req.mime_type)
                    .map_err(|e| eyre!("invalid mime '{}': {e}", req.mime_type))?,
            );
        if let Some(lang) = req.language {
            form = form.text("language", lang.to_string());
        }
        if let Some(prompt) = req.prompt {
            form = form.text("prompt", prompt.to_string());
        }

        let url = format!("{}/audio/transcriptions", self.base_url);
        let request = self.client.post(&url).multipart(form);
        let response = api_key
            .iter()
            .fold(request, |request, key| request.bearer_auth(key))
            .send()
            .await
            .map_err(|e| eyre!("whisper request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre!(
                "whisper returned {status}: {}",
                scrub(&body, api_key.as_deref())
            ));
        }

        response
            .json::<WhisperResponse>()
            .await
            .map_err(|e| eyre!("whisper response decode failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::secret::NoKey;

    use crate::runner::CURRENT_USER_ID;

    fn request() -> WhisperRequest<'static> {
        WhisperRequest {
            model: "whisper-1",
            audio: b"audio".to_vec(),
            filename: "voice.mp3",
            mime_type: "audio/mpeg",
            language: None,
            prompt: None,
        }
    }

    /// Alice's key for Alice, nobody's for anyone else.
    fn alices() -> KeySlot {
        KeySlot::new(
            std::sync::Arc::new(|| {
                CURRENT_USER_ID
                    .try_with(Clone::clone)
                    .ok()
                    .filter(|person| person == "alice")
                    .map(|_| "alice-key".to_string())
            }),
            "no transcription key for you",
        )
    }

    #[tokio::test]
    async fn a_hosted_endpoint_is_sent_the_callers_key_and_nothing_without_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .and(header("authorization", "Bearer alice-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "hi"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = WhisperClient::hosted(alices(), &server.uri());

        let heard = CURRENT_USER_ID
            .scope("alice".into(), client.transcribe(request()))
            .await
            .unwrap();
        assert_eq!(heard.text, "hi");

        let refused = CURRENT_USER_ID
            .scope("bob".into(), client.transcribe(request()))
            .await
            .unwrap_err();
        assert_eq!(
            refused.downcast_ref::<NoKey>(),
            Some(&NoKey("no transcription key for you".into()))
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "nothing is sent for someone with no key"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_never_echoes_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key sk-secret-1234"))
            .mount(&server)
            .await;
        let client = WhisperClient::hosted(KeySlot::fixed("sk-secret-1234"), &server.uri());
        let error = client.transcribe(request()).await.unwrap_err().to_string();
        assert!(
            error.contains("401") && error.contains("<redacted>"),
            "{error}"
        );
        assert!(!error.contains("sk-secret"), "{error}");
    }

    #[tokio::test]
    async fn a_self_hosted_endpoint_takes_a_request_with_no_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "hi"})),
            )
            .mount(&server)
            .await;
        let client = WhisperClient::new(KeySlot::fixed(""), server.uri());
        assert_eq!(client.transcribe(request()).await.unwrap().text, "hi");
        let sent = &server.received_requests().await.unwrap()[0];
        assert!(!sent.headers.contains_key("authorization"));
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let c = WhisperClient::new(KeySlot::fixed("k"), "https://example.com/v1/");
        assert_eq!(c.base_url, "https://example.com/v1");
    }

    #[test]
    fn openai_constructor_uses_canonical_base_url() {
        let c = WhisperClient::openai(KeySlot::fixed("k"));
        assert_eq!(c.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn groq_constructor_uses_canonical_base_url() {
        let c = WhisperClient::groq(KeySlot::fixed("k"));
        assert_eq!(c.base_url, "https://api.groq.com/openai/v1");
    }

    #[test]
    fn whisper_response_decodes_minimal_payload() {
        let raw = r#"{"text":"hello world"}"#;
        let r: WhisperResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.text, "hello world");
        assert!(r.language.is_none());
        assert!(r.duration.is_none());
    }

    #[test]
    fn whisper_response_decodes_verbose_payload() {
        let raw = r#"{"text":"hi","language":"en","duration":1.5}"#;
        let r: WhisperResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.text, "hi");
        assert_eq!(r.language.as_deref(), Some("en"));
        assert_eq!(r.duration, Some(1.5));
    }
}
