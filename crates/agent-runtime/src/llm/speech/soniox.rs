//! Soniox speech synthesis — `POST {base}/tts`.
//!
//! Auth is a Bearer token. Voices are multilingual (any voice speaks all
//! supported languages), so the required `language` ISO code — not the voice —
//! selects pronunciation; it comes from `[llm.speech].language`.

use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::json;
use workforce_common::secret::{scrub, KeySlot};
use workforce_common::settings::LlmSpeechConfig;

use super::{SpeechOutput, SpeechProvider, SpeechRequest};

const DEFAULT_BASE_URL: &str = "https://tts-rt.soniox.com";
const DEFAULT_MODEL: &str = "tts-rt-v1";
const DEFAULT_VOICE: &str = "Maya";
const DEFAULT_LANGUAGE: &str = "en";

pub struct SonioxSpeech {
    client: Client,
    api_key: KeySlot,
    base_url: String,
    model: String,
    voice: String,
    language: String,
    /// `"mp3"` or `"wav"`; anything else falls back to mp3.
    format: String,
    speed: Option<f32>,
}

impl SonioxSpeech {
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
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_VOICE.to_string()),
            language: cfg
                .language
                .clone()
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string()),
            format: if cfg.format == "wav" { "wav" } else { "mp3" }.to_string(),
            speed: cfg.speed,
        }
    }
}

#[async_trait::async_trait]
impl SpeechProvider for SonioxSpeech {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
        let api_key = self.api_key.get()?;
        let voice = req.voice.as_deref().unwrap_or(&self.voice);
        let mut body = json!({
            "model": self.model,
            "text": req.text,
            "voice": voice,
            "language": self.language,
            "audio_format": self.format,
        });
        // Soniox accepts 0.7–1.3 for speed.
        if let Some(speed) = req.speed.or(self.speed).filter(|s| *s > 0.0) {
            body["speed"] = json!(speed.clamp(0.7, 1.3));
        }

        let url = format!("{}/tts", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("soniox speech request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre!(
                "soniox speech returned {status}: {}",
                scrub(&body, Some(&api_key))
            ));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| eyre!("soniox speech body read failed: {e}"))?
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
        format!("{}/{}/{}", self.model, self.voice, self.language)
    }

    fn provider_id(&self) -> &'static str {
        "soniox"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_applied_and_url_trimmed() {
        let cfg = LlmSpeechConfig {
            provider: "soniox".into(),
            url: Some("https://gw.example.com/".into()),
            ..Default::default()
        };
        let p = SonioxSpeech::from_config(&cfg, KeySlot::fixed("k"));
        assert_eq!(p.base_url, "https://gw.example.com");
        assert_eq!(p.model, DEFAULT_MODEL);
        assert_eq!(p.voice, DEFAULT_VOICE);
        assert_eq!(p.language, DEFAULT_LANGUAGE);
        assert_eq!(p.format, "mp3");
        assert_eq!(p.voice_label(), "tts-rt-v1/Maya/en");
    }

    #[test]
    fn configured_voice_and_language_reach_the_label() {
        let cfg = LlmSpeechConfig {
            provider: "soniox".into(),
            voice: Some("Adrian".into()),
            language: Some("el".into()),
            ..Default::default()
        };
        let p = SonioxSpeech::from_config(&cfg, KeySlot::fixed("k"));
        assert_eq!(p.voice_label(), "tts-rt-v1/Adrian/el");
    }

    #[test]
    fn blank_voice_and_language_fall_back_to_defaults() {
        let cfg = LlmSpeechConfig {
            provider: "soniox".into(),
            voice: Some("  ".into()),
            language: Some("".into()),
            ..Default::default()
        };
        let p = SonioxSpeech::from_config(&cfg, KeySlot::fixed("k"));
        assert_eq!(p.voice, DEFAULT_VOICE);
        assert_eq!(p.language, DEFAULT_LANGUAGE);
    }

    #[test]
    fn wav_format_respected_unknown_falls_back_to_mp3() {
        let mut cfg = LlmSpeechConfig {
            provider: "soniox".into(),
            ..Default::default()
        };
        cfg.format = "wav".into();
        assert_eq!(
            SonioxSpeech::from_config(&cfg, KeySlot::fixed("k")).format,
            "wav"
        );
        cfg.format = "flac".into();
        assert_eq!(
            SonioxSpeech::from_config(&cfg, KeySlot::fixed("k")).format,
            "mp3"
        );
    }

    #[tokio::test]
    async fn empty_key_fails_fast_with_remedy_not_a_network_call() {
        let cfg = LlmSpeechConfig {
            provider: "soniox".into(),
            ..Default::default()
        };
        let p = SonioxSpeech::from_config(&cfg, KeySlot::fixed(""));
        let err = p
            .synthesize(&SpeechRequest {
                text: "γεια σου",
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(err.is::<workforce_common::secret::NoKey>(), "{err}");
    }

    #[test]
    fn request_speed_overrides_config_speed() {
        let cfg = LlmSpeechConfig {
            provider: "soniox".into(),
            speed: Some(1.0),
            ..Default::default()
        };
        let p = SonioxSpeech::from_config(&cfg, KeySlot::fixed("k"));
        let req = SpeechRequest {
            text: "γεια σου",
            speed: Some(0.8),
            ..Default::default()
        };
        assert_eq!(req.speed.or(p.speed), Some(0.8));
        let req = SpeechRequest {
            text: "γεια σου",
            ..Default::default()
        };
        assert_eq!(req.speed.or(p.speed), Some(1.0));
    }
}
