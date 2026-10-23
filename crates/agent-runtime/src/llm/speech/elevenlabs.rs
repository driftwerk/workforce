//! ElevenLabs speech synthesis — `POST {base}/v1/text-to-speech/{voice_id}`.
//!
//! Auth is the `xi-api-key` header. Output is pinned to
//! `mp3_44100_128` regardless of `[llm.speech].format`: ElevenLabs' non-MP3
//! output formats are raw PCM/µ-law frames without a container header, which
//! Slack won't play.

use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::json;
use workforce_common::secret::{scrub, KeySlot};
use workforce_common::settings::LlmSpeechConfig;

use super::{SpeechOutput, SpeechProvider, SpeechRequest};

const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";
const DEFAULT_MODEL: &str = "eleven_multilingual_v2";
const OUTPUT_FORMAT: &str = "mp3_44100_128";

pub struct ElevenLabsSpeech {
    client: Client,
    api_key: KeySlot,
    base_url: String,
    model_id: String,
    voice_id: String,
    speed: Option<f32>,
}

impl ElevenLabsSpeech {
    /// Errors when `voice` is unset: ElevenLabs has no universal default
    /// voice id, so a missing voice is a config mistake to surface at build
    /// time, not a request-time 404.
    pub fn from_config(cfg: &LlmSpeechConfig, api_key: KeySlot) -> Result<Self> {
        let voice_id = cfg
            .voice
            .clone()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                eyre!(
                    "[llm.speech] provider 'elevenlabs' requires `voice` (an ElevenLabs voice id)"
                )
            })?;
        Ok(Self {
            client: crate::llm::image::http_client(120),
            api_key,
            base_url: cfg
                .url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            model_id: cfg
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            voice_id,
            speed: cfg.speed,
        })
    }
}

#[async_trait::async_trait]
impl SpeechProvider for ElevenLabsSpeech {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
        let api_key = self.api_key.get()?;
        let voice_id = req.voice.as_deref().unwrap_or(&self.voice_id);
        // The voice id is interpolated into the request URL path and can come
        // from a per-USER vault entry — restrict to the identifier charset of
        // real ElevenLabs voice ids so a hostile value can't steer the POST to
        // a different endpoint or inject query params under the shared key.
        if voice_id.is_empty()
            || voice_id.len() > 64
            || !voice_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(eyre!(
                "invalid ElevenLabs voice id '{voice_id}' — expected only letters, digits, \
                 '-' or '_' (check the voice saved in Settings → Integrations)"
            ));
        }
        let mut body = json!({
            "text": req.text,
            "model_id": self.model_id,
        });
        // ElevenLabs accepts 0.7–1.2 for voice_settings.speed.
        if let Some(speed) = req.speed.or(self.speed).filter(|s| *s > 0.0) {
            body["voice_settings"] = json!({ "speed": speed.clamp(0.7, 1.2) });
        }

        let url = format!(
            "{}/v1/text-to-speech/{voice_id}?output_format={OUTPUT_FORMAT}",
            self.base_url
        );
        let response = self
            .client
            .post(&url)
            .header("xi-api-key", &api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("elevenlabs speech request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre!(
                "elevenlabs speech returned {status}: {}",
                scrub(&body, Some(&api_key))
            ));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| eyre!("elevenlabs speech body read failed: {e}"))?
            .to_vec();
        Ok(SpeechOutput {
            bytes,
            media_type: "audio/mpeg",
        })
    }

    fn voice_label(&self) -> String {
        format!("{}/{}", self.model_id, self.voice_id)
    }

    fn provider_id(&self) -> &'static str {
        "elevenlabs"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_is_required() {
        let cfg = LlmSpeechConfig {
            provider: "elevenlabs".into(),
            ..Default::default()
        };
        assert!(ElevenLabsSpeech::from_config(&cfg, KeySlot::fixed("k")).is_err());
    }

    #[tokio::test]
    async fn hostile_voice_override_is_rejected_before_any_request() {
        let cfg = LlmSpeechConfig {
            provider: "elevenlabs".into(),
            voice: Some("voice123".into()),
            ..Default::default()
        };
        let p = ElevenLabsSpeech::from_config(&cfg, KeySlot::fixed("k")).unwrap();
        for hostile in [
            "../../v1/history?x=",
            "x?output_format=pcm_44100",
            "a/b",
            "a#b",
            "voice id with spaces",
            "",
        ] {
            let err = p
                .synthesize(&SpeechRequest {
                    text: "hi",
                    voice: Some(hostile.into()),
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid ElevenLabs voice id"),
                "{hostile:?} → {err}"
            );
        }
    }

    #[test]
    fn defaults_applied() {
        let cfg = LlmSpeechConfig {
            provider: "elevenlabs".into(),
            voice: Some("voice123".into()),
            ..Default::default()
        };
        let p = ElevenLabsSpeech::from_config(&cfg, KeySlot::fixed("k")).unwrap();
        assert_eq!(p.base_url, DEFAULT_BASE_URL);
        assert_eq!(p.model_id, DEFAULT_MODEL);
        assert_eq!(p.voice_label(), "eleven_multilingual_v2/voice123");
    }
}
