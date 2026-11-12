//! Spoken replies for the Desk app: text in, audio out, synthesised by the
//! hub with a key from its vault. The app never holds the key.

use async_trait::async_trait;
use serde::Deserialize;

/// The most characters one request speaks.
pub const MAX_SPEAK_CHARS: usize = 2000;

/// A speech provider the app can ask for: the ones that speak with a key
/// someone brings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SpeechProvider {
    #[serde(rename = "fish_audio")]
    FishAudio,
    #[serde(rename = "elevenlabs")]
    ElevenLabs,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "soniox")]
    Soniox,
}

impl SpeechProvider {
    pub const ALL: [Self; 4] = [
        Self::FishAudio,
        Self::ElevenLabs,
        Self::OpenAi,
        Self::Soniox,
    ];

    /// The hub's id for the provider, as its vault path spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::FishAudio => "fish_audio",
            Self::ElevenLabs => "elevenlabs",
            Self::OpenAi => "openai",
            Self::Soniox => "soniox",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FishAudio => "Fish Audio",
            Self::ElevenLabs => "ElevenLabs",
            Self::OpenAi => "OpenAI",
            Self::Soniox => "Soniox",
        }
    }
}

/// A voice key Desk keeps: a speech provider's, or the Gemini key Gemini
/// Live talks with (the Gemini provider's, when one is connected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKey {
    Speech(SpeechProvider),
    Gemini,
}

impl VoiceKey {
    /// The key named as its vault path spells it (`speech/<name>/api_key`).
    pub fn parse(name: &str) -> Option<Self> {
        SpeechProvider::ALL
            .into_iter()
            .find(|p| p.name() == name)
            .map(Self::Speech)
            .or((name == "gemini").then_some(Self::Gemini))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Speech(p) => p.name(),
            Self::Gemini => "gemini",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Speech(p) => p.label(),
            Self::Gemini => "Gemini",
        }
    }
}

/// What to say, and how. `model` and `voice` fall back to the provider's
/// defaults; `speed` is a rate multiplier each provider clamps to its range.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Speak {
    pub provider: SpeechProvider,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub text: String,
    pub speed: Option<f32>,
}

impl Speak {
    /// The request with blank choices dropped, or why it can't be spoken.
    pub(crate) fn validated(self) -> Result<Self, SpeakError> {
        if self.text.trim().is_empty() {
            return Err(SpeakError::Invalid("there's no text to speak".into()));
        }
        let chars = self.text.chars().count();
        if chars > MAX_SPEAK_CHARS {
            return Err(SpeakError::Invalid(format!(
                "the text is {chars} characters; at most {MAX_SPEAK_CHARS} are spoken at once"
            )));
        }
        let given = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        Ok(Self {
            model: given(self.model),
            voice: given(self.voice),
            ..self
        })
    }
}

/// Synthesised speech.
#[derive(Debug, Clone, PartialEq)]
pub struct Audio {
    pub bytes: Vec<u8>,
    /// The audio's media type, e.g. `audio/mpeg`.
    pub media_type: String,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum SpeakError {
    #[error("{0}")]
    Invalid(String),
    #[error(
        "Desk has no {} key yet: add a {} key under Voices in Desk Settings",
        .0.label(),
        .0.label()
    )]
    NoKey(SpeechProvider),
    #[error("{0}")]
    Upstream(String),
    /// The provider refused a voice key when it was tried: why, in the
    /// person's words, with the fix.
    #[error("{0}")]
    Refused(String),
}

/// Speaks for the app. Implemented by the hub over its speech providers and
/// vault.
#[async_trait]
pub trait Speech: Send + Sync {
    async fn speak(&self, request: &Speak) -> Result<Audio, SpeakError>;

    /// Tries `key` with the smallest call its provider takes, then keeps
    /// it where Desk reads it. A key the provider refuses is not kept, and
    /// [`SpeakError::Refused`] says why in the person's words; neither ever
    /// carries the key.
    async fn save_key(&self, key: VoiceKey, _value: &str) -> Result<(), SpeakError> {
        Err(SpeakError::Invalid(format!(
            "this Desk can't check a {} key yet",
            key.label()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use parking_lot::Mutex;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::api::voice_router;
    use crate::realtime::Realtime;
    use crate::testing::{Bearer, NoKeys};

    /// Speaks every request as its text's bytes, or answers `fails`; keeps
    /// every key but `refused`.
    #[derive(Default)]
    struct Fake {
        heard: Mutex<Vec<Speak>>,
        fails: Option<SpeakError>,
        kept: Mutex<Vec<(VoiceKey, String)>>,
    }

    const REFUSED: &str = "refused-key-0123456789";

    #[async_trait]
    impl Speech for Fake {
        async fn speak(&self, request: &Speak) -> Result<Audio, SpeakError> {
            self.heard.lock().push(request.clone());
            match &self.fails {
                Some(e) => Err(e.clone()),
                None => Ok(Audio {
                    bytes: request.text.as_bytes().to_vec(),
                    media_type: "audio/mpeg".into(),
                }),
            }
        }

        async fn save_key(&self, key: VoiceKey, value: &str) -> Result<(), SpeakError> {
            if value == REFUSED {
                return Err(SpeakError::Refused(format!(
                    "{} didn't accept that key",
                    key.label()
                )));
            }
            self.kept.lock().push((key, value.into()));
            Ok(())
        }
    }

    fn app(speech: Arc<Fake>) -> Router {
        voice_router(
            Arc::new(Bearer),
            Arc::new(Realtime::new(Arc::new(NoKeys))),
            speech,
        )
    }

    async fn speak(app: Router, token: &str, body: Value) -> (StatusCode, String, Vec<u8>) {
        let request = Request::post("/api/desk/voice/speak")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let media_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, media_type, body.to_vec())
    }

    fn error(body: &[u8]) -> String {
        serde_json::from_slice::<Value>(body).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn speaks_the_text_as_the_providers_audio() {
        let fake = Arc::new(Fake::default());
        let (status, media_type, body) = speak(
            app(Arc::clone(&fake)),
            "hub-token",
            json!({
                "provider": "elevenlabs",
                "model": "eleven_v3",
                "voice": "  ",
                "text": "Hello there",
                "speed": 1.25,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(media_type, "audio/mpeg");
        assert_eq!(body, b"Hello there");
        assert_eq!(
            fake.heard.lock().as_slice(),
            [Speak {
                provider: SpeechProvider::ElevenLabs,
                model: Some("eleven_v3".into()),
                voice: None,
                text: "Hello there".into(),
                speed: Some(1.25),
            }]
        );
    }

    #[tokio::test]
    async fn takes_each_provider_by_its_hub_name() {
        for provider in SpeechProvider::ALL {
            let fake = Arc::new(Fake::default());
            let (status, ..) = speak(
                app(Arc::clone(&fake)),
                "hub-token",
                json!({ "provider": provider.name(), "text": "hi" }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{}", provider.name());
            assert_eq!(fake.heard.lock()[0].provider, provider);
        }
    }

    #[tokio::test]
    async fn refuses_what_it_cannot_speak_before_asking_a_provider() {
        let too_long = "a".repeat(MAX_SPEAK_CHARS + 1);
        for body in [
            json!({ "provider": "kokoro", "text": "hi" }),
            json!({ "provider": "builtin", "text": "hi" }),
            json!({ "provider": "openai" }),
            json!({ "provider": "openai", "text": " \n " }),
            json!({ "provider": "openai", "text": too_long }),
        ] {
            let fake = Arc::new(Fake::default());
            let (status, _, response) =
                speak(app(Arc::clone(&fake)), "hub-token", body.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(!error(&response).is_empty());
            assert!(fake.heard.lock().is_empty());
        }
        let fake = Arc::new(Fake::default());
        let (status, ..) = speak(
            app(Arc::clone(&fake)),
            "hub-token",
            json!({ "provider": "openai", "text": "é".repeat(MAX_SPEAK_CHARS) }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the cap counts characters, not bytes"
        );
    }

    async fn save_key(app: Router, token: &str, name: &str, body: Value) -> (StatusCode, Value) {
        let request = Request::post(format!("/api/desk/voice/keys/{name}"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_voice_key_is_kept_once_its_provider_takes_it_and_never_echoed() {
        let fake = Arc::new(Fake::default());
        let (status, body) = save_key(
            app(Arc::clone(&fake)),
            "hub-token",
            "elevenlabs",
            json!({ "key": "  el-right  " }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = save_key(
            app(Arc::clone(&fake)),
            "hub-token",
            "gemini",
            json!({ "key": "gm-right" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            *fake.kept.lock(),
            vec![
                (
                    VoiceKey::Speech(SpeechProvider::ElevenLabs),
                    "el-right".into()
                ),
                (VoiceKey::Gemini, "gm-right".into()),
            ]
        );

        let (status, body) = save_key(
            app(Arc::clone(&fake)),
            "hub-token",
            "openai",
            json!({ "key": REFUSED }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let said = body["error"].as_str().unwrap();
        assert_eq!(said, "OpenAI didn't accept that key");
        assert_eq!(body["code"], crate::providers::KEY_REFUSED);
        assert!(!body.to_string().contains(REFUSED));

        let (status, _) = save_key(
            app(Arc::clone(&fake)),
            "hub-token",
            "openai",
            json!({ "key": " " }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = save_key(
            app(Arc::clone(&fake)),
            "hub-token",
            "kokoro",
            json!({ "key": "k" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = save_key(
            app(Arc::clone(&fake)),
            "wrong",
            "openai",
            json!({ "key": "k" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(fake.kept.lock().len(), 2);
    }

    #[test]
    fn voice_keys_are_named_as_their_vault_paths_spell_them() {
        for provider in SpeechProvider::ALL {
            assert_eq!(
                VoiceKey::parse(provider.name()),
                Some(VoiceKey::Speech(provider))
            );
        }
        assert_eq!(VoiceKey::parse("gemini"), Some(VoiceKey::Gemini));
        assert_eq!(VoiceKey::parse("kokoro"), None);
    }

    #[tokio::test]
    async fn needs_the_hub_token() {
        let fake = Arc::new(Fake::default());
        let (status, ..) = speak(
            app(Arc::clone(&fake)),
            "wrong",
            json!({ "provider": "openai", "text": "hi" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(fake.heard.lock().is_empty());
    }

    #[tokio::test]
    async fn a_missing_key_says_where_to_add_one() {
        let fake = Arc::new(Fake {
            fails: Some(SpeakError::NoKey(SpeechProvider::FishAudio)),
            ..Fake::default()
        });
        let (status, _, body) = speak(
            app(fake),
            "hub-token",
            json!({ "provider": "fish_audio", "text": "hi" }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error(&body),
            "Desk has no Fish Audio key yet: add a Fish Audio key under Voices in Desk Settings"
        );
    }

    #[tokio::test]
    async fn a_provider_failure_is_a_bad_gateway_with_its_message() {
        let fake = Arc::new(Fake {
            fails: Some(SpeakError::Upstream(
                "soniox speech API error 429: slow down".into(),
            )),
            ..Fake::default()
        });
        let (status, _, body) = speak(
            app(fake),
            "hub-token",
            json!({ "provider": "soniox", "text": "hi" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(error(&body), "soniox speech API error 429: slow down");
    }
}
