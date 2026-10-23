//! Chat-multimodal transcription — posts audio as an `input_audio` content
//! part to an OpenAI-compatible chat completions endpoint.
//!
//! Used for backends that support audio in chat content but don't expose a
//! dedicated `/v1/audio/transcriptions` endpoint:
//!
//!   - Gemini direct (`https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`)
//!   - OpenRouter routed to a Google upstream
//!   - Self-hosted vLLM serving an audio-multimodal model
//!
//! The audio is base64-encoded and shipped alongside a single text instruction
//! that asks for verbatim transcription. The chat response text is the
//! transcript.

use base64::Engine;
use eyre::{eyre, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_common::secret::{scrub, KeySlot};

const TRANSCRIBE_INSTRUCTION: &str = "Transcribe the spoken content of this audio verbatim. \
    Return only the transcript text — no commentary, summaries, timestamps, or speaker labels. \
    If the audio is silent or unintelligible, respond with exactly: [no speech detected].";

/// Chat-multimodal transcription client. The key is read from its slot on
/// every request, for whoever the request is for.
#[derive(Clone)]
pub struct ChatTranscriptionClient {
    client: Client,
    api_key: KeySlot,
    base_url: String,
}

/// One request to the chat endpoint.
pub struct ChatTranscriptionRequest<'a> {
    pub model: &'a str,
    pub audio: &'a [u8],
    pub mime_type: &'a str,
    pub language: Option<&'a str>,
}

/// Result of a successful transcription. Duration is `None` because the
/// chat path doesn't return it; callers wanting cost attribution must probe
/// duration separately (e.g. via ffprobe).
pub struct ChatTranscriptionOutput {
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    content: Option<String>,
}

impl ChatTranscriptionClient {
    pub fn new(api_key: KeySlot, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Construct for Gemini's OpenAI-compatible endpoint.
    pub fn gemini(api_key: KeySlot) -> Self {
        Self::new(
            api_key,
            "https://generativelanguage.googleapis.com/v1beta/openai",
        )
    }

    /// Construct for OpenRouter.
    pub fn openrouter(api_key: KeySlot) -> Self {
        Self::new(api_key, "https://openrouter.ai/api/v1")
    }

    pub async fn transcribe(
        &self,
        req: ChatTranscriptionRequest<'_>,
    ) -> Result<ChatTranscriptionOutput> {
        let api_key = self.api_key.get()?;
        let format = audio_format_token(req.mime_type)?;
        let audio_b64 = base64::engine::general_purpose::STANDARD.encode(req.audio);
        let prompt = match req.language {
            Some(lang) if !lang.is_empty() => {
                format!("{TRANSCRIBE_INSTRUCTION} The audio is in language `{lang}`.")
            }
            _ => TRANSCRIBE_INSTRUCTION.to_string(),
        };

        let body = json!({
            "model": req.model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "input_audio", "input_audio": { "data": audio_b64, "format": format } },
                    { "type": "text", "text": prompt },
                ],
            }],
            "temperature": 0.0,
        });

        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("chat transcription request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(eyre!(
                "chat transcription returned {status}: {}",
                scrub(&body, Some(&api_key))
            ));
        }

        let payload: Value = response
            .json()
            .await
            .map_err(|e| eyre!("chat transcription decode failed: {e}"))?;
        let parsed: ChatResponse = serde_json::from_value(payload.clone())
            .map_err(|e| eyre!("chat transcription schema unexpected ({e}): {payload}"))?;
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| eyre!("chat transcription returned no content"))?;

        Ok(ChatTranscriptionOutput {
            text: text.trim().to_string(),
        })
    }
}

/// Map a mime type to the OpenAI `input_audio.format` token.
///
/// The OpenAI chat schema requires one of: `wav`, `mp3`, `flac`, `m4a`, `ogg`,
/// `webm`. Slack delivers `audio/mp4` for iOS voice memos and `audio/webm`
/// for Chrome desktop, so we cover the common Slack shapes.
fn audio_format_token(mime: &str) -> Result<&'static str> {
    Ok(match mime.to_lowercase().as_str() {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mp3" | "audio/mpeg" | "audio/mpga" => "mp3",
        "audio/flac" => "flac",
        "audio/m4a" | "audio/x-m4a" | "audio/mp4" => "m4a",
        "audio/ogg" | "audio/opus" | "audio/vorbis" => "ogg",
        "audio/webm" => "webm",
        _ => {
            return Err(eyre!(
                "unsupported audio mime for chat transcription: {mime}"
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_covers_slack_voice_memo_shapes() {
        assert_eq!(audio_format_token("audio/mp4").unwrap(), "m4a");
        assert_eq!(audio_format_token("audio/x-m4a").unwrap(), "m4a");
        assert_eq!(audio_format_token("audio/webm").unwrap(), "webm");
        assert_eq!(audio_format_token("audio/mpeg").unwrap(), "mp3");
        assert_eq!(audio_format_token("audio/wav").unwrap(), "wav");
        assert_eq!(audio_format_token("audio/ogg").unwrap(), "ogg");
    }

    #[test]
    fn audio_format_is_case_insensitive() {
        assert_eq!(audio_format_token("Audio/Mp4").unwrap(), "m4a");
        assert_eq!(audio_format_token("AUDIO/WEBM").unwrap(), "webm");
    }

    #[test]
    fn audio_format_rejects_unknown_mime() {
        assert!(audio_format_token("video/mp4").is_err());
        assert!(audio_format_token("audio/aiff").is_err());
        assert!(audio_format_token("text/plain").is_err());
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let c = ChatTranscriptionClient::new(KeySlot::fixed("k"), "https://example.com/v1/");
        assert_eq!(c.base_url, "https://example.com/v1");
    }
}
