//! Fish Audio speech synthesis — `POST {base}/v1/tts`.
//!
//! Auth is a Bearer token; the speech model generation (`s1`, `s2-pro`) is
//! selected with a `model` header. The optional `reference_id` picks a voice
//! from the Fish Audio library; omitted, the model's default voice speaks.

use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::json;
use workforce_common::secret::{scrub, KeySlot};
use workforce_common::settings::LlmSpeechConfig;

use super::{SpeechOutput, SpeechProvider, SpeechRequest};

const DEFAULT_BASE_URL: &str = "https://api.fish.audio";
const DEFAULT_MODEL: &str = "s1";

pub struct FishAudioSpeech {
    client: Client,
    api_key: KeySlot,
    base_url: String,
    model: String,
    reference_id: Option<String>,
    /// `"mp3"` or `"wav"`; anything else falls back to mp3.
    format: String,
    speed: Option<f32>,
}

impl FishAudioSpeech {
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
            reference_id: cfg.voice.clone().filter(|v| !v.trim().is_empty()),
            format: if cfg.format == "wav" { "wav" } else { "mp3" }.to_string(),
            speed: cfg.speed,
        }
    }
}

#[async_trait::async_trait]
impl SpeechProvider for FishAudioSpeech {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
        let api_key = self.api_key.get()?;
        let mut body = json!({
            "text": req.text,
            "format": self.format,
            "latency": "normal",
        });
        if self.format == "mp3" {
            body["mp3_bitrate"] = json!(128);
        }
        if let Some(reference_id) = req.voice.as_deref().or(self.reference_id.as_deref()) {
            body["reference_id"] = json!(reference_id);
        }
        // Fish prosody.speed accepts 0.5–2.0.
        if let Some(speed) = req.speed.or(self.speed).filter(|s| *s > 0.0) {
            body["prosody"] = json!({ "speed": speed.clamp(0.5, 2.0) });
        }

        let url = format!("{}/v1/tts", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&api_key)
            .header("model", &self.model)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("fish audio speech request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre!(
                "fish audio speech returned {status}: {}",
                scrub(&body, Some(&api_key))
            ));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| eyre!("fish audio speech body read failed: {e}"))?
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
        match &self.reference_id {
            Some(id) => format!("{}/{}", self.model, id),
            None => format!("{}/default-voice", self.model),
        }
    }

    fn provider_id(&self) -> &'static str {
        "fish_audio"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_applied_and_reference_optional() {
        let cfg = LlmSpeechConfig {
            provider: "fish_audio".into(),
            ..Default::default()
        };
        let p = FishAudioSpeech::from_config(&cfg, KeySlot::fixed("k"));
        assert_eq!(p.base_url, DEFAULT_BASE_URL);
        assert_eq!(p.model, DEFAULT_MODEL);
        assert!(p.reference_id.is_none());
        assert_eq!(p.voice_label(), "s1/default-voice");
    }

    #[test]
    fn blank_voice_treated_as_unset() {
        let cfg = LlmSpeechConfig {
            provider: "fish_audio".into(),
            voice: Some("  ".into()),
            ..Default::default()
        };
        assert!(FishAudioSpeech::from_config(&cfg, KeySlot::fixed("k"))
            .reference_id
            .is_none());
    }

    #[tokio::test]
    async fn empty_key_fails_fast_with_remedy_not_a_network_call() {
        let cfg = LlmSpeechConfig {
            provider: "fish_audio".into(),
            ..Default::default()
        };
        let p = FishAudioSpeech::from_config(&cfg, KeySlot::fixed(""));
        let err = p
            .synthesize(&SpeechRequest {
                text: "olá",
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.is::<workforce_common::secret::NoKey>(), "{err}");
    }

    #[test]
    fn request_speed_overrides_config_speed() {
        let cfg = LlmSpeechConfig {
            provider: "fish_audio".into(),
            speed: Some(1.0),
            ..Default::default()
        };
        let p = FishAudioSpeech::from_config(&cfg, KeySlot::fixed("k"));
        // Same precedence expression the providers use at request time.
        let req = SpeechRequest {
            text: "olá",
            speed: Some(0.8),
            ..Default::default()
        };
        assert_eq!(req.speed.or(p.speed), Some(0.8));
        let req = SpeechRequest {
            text: "olá",
            ..Default::default()
        };
        assert_eq!(req.speed.or(p.speed), Some(1.0));
    }
}
