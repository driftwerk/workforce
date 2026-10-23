//! OpenAI speech synthesis — `POST {base}/audio/speech`.
//!
//! Also serves OpenAI-compatible gateways via `[llm.speech].url`.

use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::json;
use workforce_common::secret::{scrub, KeySlot};
use workforce_common::settings::LlmSpeechConfig;

use super::{SpeechOutput, SpeechProvider, SpeechRequest};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini-tts";
const DEFAULT_VOICE: &str = "alloy";

pub struct OpenAiSpeech {
    client: Client,
    api_key: KeySlot,
    base_url: String,
    model: String,
    voice: String,
    /// `"mp3"` or `"wav"` — already validated by config parsing; anything
    /// else falls back to mp3.
    format: String,
    speed: Option<f32>,
}

impl OpenAiSpeech {
    pub fn from_config(cfg: &LlmSpeechConfig, api_key: KeySlot) -> Self {
        Self {
            client: crate::llm::image::http_client(120),
            api_key,
            base_url: cfg
                .url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            model: cfg
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            voice: cfg
                .voice
                .clone()
                .unwrap_or_else(|| DEFAULT_VOICE.to_string()),
            format: if cfg.format == "wav" { "wav" } else { "mp3" }.to_string(),
            speed: cfg.speed,
        }
    }
}

#[async_trait::async_trait]
impl SpeechProvider for OpenAiSpeech {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
        let api_key = self.api_key.get()?;
        let voice = req.voice.as_deref().unwrap_or(&self.voice);
        let mut body = json!({
            "model": self.model,
            "input": req.text,
            "voice": voice,
            "response_format": self.format,
        });
        if let Some(speed) = req.speed.or(self.speed).filter(|s| *s > 0.0) {
            body["speed"] = json!(speed.clamp(0.25, 4.0));
        }

        let url = format!("{}/audio/speech", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("openai speech request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre!(
                "openai speech returned {status}: {}",
                scrub(&body, Some(&api_key))
            ));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| eyre!("openai speech body read failed: {e}"))?
            .to_vec();
        Ok(SpeechOutput {
            bytes,
            media_type: if self.format == "wav" {
                "audio/wav"
            } else {
                "audio/mpeg"
            },
        })
    }

    fn voice_label(&self) -> String {
        format!("{}/{}", self.model, self.voice)
    }

    fn provider_id(&self) -> &'static str {
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::secret::NoKey;

    use crate::runner::CURRENT_USER_ID;

    /// Each person's own key, and nobody's for dave.
    fn speech_at(url: String) -> OpenAiSpeech {
        let cfg = LlmSpeechConfig {
            provider: "openai".into(),
            url: Some(url),
            ..Default::default()
        };
        let own = KeySlot::new(
            std::sync::Arc::new(|| {
                CURRENT_USER_ID
                    .try_with(Clone::clone)
                    .ok()
                    .filter(|person| person != "dave")
                    .map(|person| format!("sk-{person}-secret"))
            }),
            "no speech key for you",
        );
        OpenAiSpeech::from_config(&cfg, own)
    }

    async fn say(speech: &OpenAiSpeech, person: &str) -> Result<SpeechOutput> {
        let request = SpeechRequest {
            text: "hello",
            ..Default::default()
        };
        CURRENT_USER_ID
            .scope(person.to_string(), speech.synthesize(&request))
            .await
    }

    #[tokio::test]
    async fn a_clip_is_paid_for_by_the_speakers_own_key_and_never_anothers() {
        let server = MockServer::start().await;
        for person in ["alice", "bob"] {
            Mock::given(method("POST"))
                .and(path("/audio/speech"))
                .and(header(
                    "authorization",
                    format!("Bearer sk-{person}-secret"),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(person.as_bytes()))
                .expect(1)
                .mount(&server)
                .await;
        }
        let speech = speech_at(server.uri());

        assert_eq!(say(&speech, "alice").await.unwrap().bytes, b"alice");
        assert_eq!(say(&speech, "bob").await.unwrap().bytes, b"bob");
        let refused = say(&speech, "dave").await.err().unwrap();
        assert_eq!(
            refused.downcast_ref::<NoKey>(),
            Some(&NoKey("no speech key for you".into()))
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "nothing is sent for someone with no key"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_never_echoes_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("Incorrect API key provided: sk-alice-secret."),
            )
            .mount(&server)
            .await;
        let refused = say(&speech_at(server.uri()), "alice")
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            refused.contains("401") && refused.contains("<redacted>"),
            "{refused}"
        );
        assert!(!refused.contains("sk-alice"), "{refused}");
    }

    #[test]
    fn defaults_applied_and_url_trimmed() {
        let cfg = LlmSpeechConfig {
            provider: "openai".into(),
            url: Some("https://gw.example.com/v1/".into()),
            ..Default::default()
        };
        let p = OpenAiSpeech::from_config(&cfg, KeySlot::fixed("k"));
        assert_eq!(p.base_url, "https://gw.example.com/v1");
        assert_eq!(p.model, DEFAULT_MODEL);
        assert_eq!(p.voice, DEFAULT_VOICE);
        assert_eq!(p.format, "mp3");
        assert_eq!(p.voice_label(), "gpt-4o-mini-tts/alloy");
    }

    #[test]
    fn wav_format_respected_unknown_falls_back_to_mp3() {
        let mut cfg = LlmSpeechConfig {
            provider: "openai".into(),
            ..Default::default()
        };
        cfg.format = "wav".into();
        assert_eq!(
            OpenAiSpeech::from_config(&cfg, KeySlot::fixed("k")).format,
            "wav"
        );
        cfg.format = "flac".into();
        assert_eq!(
            OpenAiSpeech::from_config(&cfg, KeySlot::fixed("k")).format,
            "mp3"
        );
    }
}
