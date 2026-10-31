//! Speech synthesis (text-to-speech) — turns text into playable audio bytes.
//!
//! Mirrors [`crate::llm::image`]: a provider trait, per-backend clients, and
//! [`provider_from_config`] which builds one from `[llm.speech]`. The
//! providers mirror the mobile app's TTS options:
//!
//!   - **builtin** — Piper, run locally as a subprocess. The binary and the
//!     voice model file are downloaded on first use; no API key.
//!   - **fish_audio** — Fish Audio HTTP API.
//!   - **elevenlabs** — ElevenLabs HTTP API.
//!   - **openai** — OpenAI speech API (`/v1/audio/speech`).
//!   - **soniox** — Soniox HTTP API (multilingual voices; `language` selects
//!     pronunciation).
//!
//! There is deliberately no tier registry (unlike image/video/transcription):
//! TTS "models" are voices, which don't map onto cheap/standard/powerful
//! tiers, so `[llm.speech].provider` names the backend explicitly.

pub mod elevenlabs;
pub mod fish;
pub mod markup;
pub mod openai;
pub mod piper;
pub mod soniox;

use std::path::Path;

use eyre::{bail, Result};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmSpeechConfig;

pub use markup::{Markup, MarkupStream};

/// One synthesis request. Model/format are provider construction-time choices
/// from config; `speed` is per-clip (the model chooses), and `voice` is the
/// speaking user's own voice from their vault tier. `None` falls back to the
/// configured shared voice. The key is the provider's to read, for whoever
/// the request is for.
#[derive(Default)]
pub struct SpeechRequest<'a> {
    pub text: &'a str,
    /// Speaking-rate multiplier for this clip (1.0 = normal). Providers clamp
    /// to their own supported range.
    pub speed: Option<f32>,
    /// The turn user's own voice id (`users/{uid}/speech/<provider>/voice`).
    pub voice: Option<String>,
}

/// Synthesized audio.
#[derive(Debug)]
pub struct SpeechOutput {
    pub bytes: Vec<u8>,
    /// IANA media type of `bytes`, e.g. `"audio/mpeg"`.
    pub media_type: &'static str,
}

/// A backend that can synthesize speech from text.
#[async_trait::async_trait]
pub trait SpeechProvider: Send + Sync {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput>;
    /// Human-readable `model/voice` label for logs and the tool description.
    fn voice_label(&self) -> String;
    /// Canonical provider id ("builtin", "fish_audio", "elevenlabs", "openai",
    /// "soniox").
    fn provider_id(&self) -> &'static str;
    /// The voice directions it performs.
    fn markup(&self) -> Markup {
        Markup::Plain
    }
}

/// A provider that is only ever handed directions it performs: the rest are
/// removed (or rewritten as it takes them) before it sees the text, so no
/// voice reads `(pensive)` aloud.
struct Marked(Box<dyn SpeechProvider>);

#[async_trait::async_trait]
impl SpeechProvider for Marked {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
        let text = self.0.markup().apply(req.text);
        let text = text.trim();
        if text.is_empty() {
            bail!("nothing is left to speak once the voice directions are removed");
        }
        self.0
            .synthesize(&SpeechRequest {
                text,
                speed: req.speed,
                voice: req.voice.clone(),
            })
            .await
    }

    fn voice_label(&self) -> String {
        self.0.voice_label()
    }

    fn provider_id(&self) -> &'static str {
        self.0.provider_id()
    }

    fn markup(&self) -> Markup {
        self.0.markup()
    }
}

/// Normalize the configured provider id, tolerating the spelling variants the
/// mobile clients already send (`fishaudio` vs `fish_audio`, etc.).
pub fn canonical_speech_provider(raw: &str) -> Option<&'static str> {
    match raw
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
        "builtin" | "piper" | "local" => Some("builtin"),
        "fish_audio" | "fishaudio" | "fish" => Some("fish_audio"),
        "elevenlabs" | "eleven_labs" | "eleven" => Some("elevenlabs"),
        "openai" => Some("openai"),
        "soniox" => Some("soniox"),
        _ => None,
    }
}

/// File extension (no dot) for a [`SpeechOutput::media_type`].
pub fn extension_for_speech(media_type: &str) -> &'static str {
    match media_type {
        "audio/wav" => "wav",
        _ => "mp3",
    }
}

/// Build a [`SpeechProvider`] from `[llm.speech]`. `speech_home` is the
/// default download directory for the builtin (Piper) backend, used when
/// `data_dir` is unset — the hub passes `{workspace}/piper`. `key` is where
/// the provider it names reads its key on every request; a missing one still
/// registers the tool, whose call then fails saying so rather than sending an
/// empty bearer token. `enabled = false` is an error so callers treat it as
/// "no provider", mirroring image. Whatever it builds checks voice
/// directions before it speaks (see [`markup`]).
pub fn provider_from_config(
    cfg: &LlmSpeechConfig,
    speech_home: &Path,
    key: impl Fn(&'static str) -> KeySlot,
) -> Result<Box<dyn SpeechProvider>> {
    Ok(Box::new(Marked(provider(cfg, speech_home, key)?)))
}

fn provider(
    cfg: &LlmSpeechConfig,
    speech_home: &Path,
    key: impl Fn(&'static str) -> KeySlot,
) -> Result<Box<dyn SpeechProvider>> {
    if !cfg.enabled {
        bail!("[llm.speech] is present but disabled (enabled = false)");
    }
    let Some(provider) = canonical_speech_provider(&cfg.provider) else {
        bail!(
            "[llm.speech].provider '{}' is not one of: builtin, fish_audio, elevenlabs, openai, \
             soniox",
            cfg.provider
        );
    };
    match provider {
        "builtin" => Ok(Box::new(piper::PiperSpeech::from_config(cfg, speech_home))),
        "openai" => Ok(Box::new(openai::OpenAiSpeech::from_config(
            cfg,
            key(provider),
        ))),
        "elevenlabs" => Ok(Box::new(elevenlabs::ElevenLabsSpeech::from_config(
            cfg,
            key(provider),
        )?)),
        "fish_audio" => Ok(Box::new(fish::FishAudioSpeech::from_config(
            cfg,
            key(provider),
        ))),
        "soniox" => Ok(Box::new(soniox::SonioxSpeech::from_config(
            cfg,
            key(provider),
        ))),
        other => bail!("unhandled speech provider '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_key(_: &'static str) -> KeySlot {
        KeySlot::fixed("")
    }

    fn cfg(provider: &str) -> LlmSpeechConfig {
        LlmSpeechConfig {
            provider: provider.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn canonical_provider_accepts_mobile_spellings() {
        assert_eq!(canonical_speech_provider("fishaudio"), Some("fish_audio"));
        assert_eq!(canonical_speech_provider("fish_audio"), Some("fish_audio"));
        assert_eq!(canonical_speech_provider("Fish-Audio"), Some("fish_audio"));
        assert_eq!(canonical_speech_provider("elevenlabs"), Some("elevenlabs"));
        assert_eq!(canonical_speech_provider("piper"), Some("builtin"));
        assert_eq!(canonical_speech_provider("builtin"), Some("builtin"));
        assert_eq!(canonical_speech_provider("openai"), Some("openai"));
        assert_eq!(canonical_speech_provider("soniox"), Some("soniox"));
        assert_eq!(canonical_speech_provider("Soniox"), Some("soniox"));
        assert_eq!(canonical_speech_provider("kokoro"), None);
        assert_eq!(canonical_speech_provider("system"), None);
    }

    /// Says back what it was handed, as its bytes.
    struct Echo(Markup);

    #[async_trait::async_trait]
    impl SpeechProvider for Echo {
        async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
            Ok(SpeechOutput {
                bytes: req.text.as_bytes().to_vec(),
                media_type: "audio/mpeg",
            })
        }
        fn voice_label(&self) -> String {
            "echo".into()
        }
        fn provider_id(&self) -> &'static str {
            "echo"
        }
        fn markup(&self) -> Markup {
            self.0
        }
    }

    async fn heard(markup: Markup, text: &str) -> Result<String> {
        let out = Marked(Box::new(Echo(markup)))
            .synthesize(&SpeechRequest {
                text,
                ..Default::default()
            })
            .await?;
        Ok(String::from_utf8(out.bytes).unwrap())
    }

    #[tokio::test]
    async fn a_voice_is_handed_only_the_directions_it_performs() {
        let said = "(excited) Found it. (pensive) Hmm.";
        assert_eq!(
            heard(Markup::FishS1, said).await.unwrap(),
            "(excited) Found it. Hmm."
        );
        assert_eq!(heard(Markup::Plain, said).await.unwrap(), "Found it. Hmm.");
        assert!(heard(Markup::Plain, " (pensive) ").await.is_err());
    }

    #[test]
    fn built_providers_know_their_markup() {
        let mut fish = cfg("fish_audio");
        assert_eq!(
            provider_from_config(&fish, Path::new("/tmp"), no_key)
                .unwrap()
                .markup(),
            Markup::FishS1
        );
        fish.model = Some("s2-pro".into());
        assert_eq!(
            provider_from_config(&fish, Path::new("/tmp"), no_key)
                .unwrap()
                .markup(),
            Markup::FishS2
        );
        assert_eq!(
            provider_from_config(&cfg("openai"), Path::new("/tmp"), no_key)
                .unwrap()
                .markup(),
            Markup::Plain
        );
    }

    #[test]
    fn build_rejects_disabled_and_unknown() {
        let mut c = cfg("openai");
        c.enabled = false;
        assert!(provider_from_config(&c, Path::new("/tmp"), no_key).is_err());
        assert!(provider_from_config(&cfg("kokoro"), Path::new("/tmp"), no_key).is_err());
    }

    #[test]
    fn build_constructs_each_provider() {
        for p in ["builtin", "openai", "fish_audio", "soniox"] {
            let built = provider_from_config(&cfg(p), Path::new("/tmp"), no_key);
            assert!(built.is_ok(), "provider {p} failed to build");
        }
        // ElevenLabs requires a voice id — config error, not credential error.
        assert!(provider_from_config(&cfg("elevenlabs"), Path::new("/tmp"), no_key).is_err());
        let mut el = cfg("elevenlabs");
        el.voice = Some("voice123".to_string());
        assert!(provider_from_config(&el, Path::new("/tmp"), no_key).is_ok());
    }
}
