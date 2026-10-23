//! Transcription LLM abstraction — turns audio bytes into a textual transcript.
//!
//! Construction lives in `crates/hub/src/transcription/` (config resolution +
//! provider instantiation); this module owns the trait and the cached adapter
//! the hub returns. Mirrors the vision split between
//! `crates/agent-runtime/src/vision/` and `crates/hub/src/vision/`.
//!
//! Two backend shapes are supported:
//!
//!   - **Dedicated Whisper API** (OpenAI, Groq, self-hosted vLLM). Uses
//!     [`crate::llm::whisper::WhisperClient`] under the hood.
//!   - **Multimodal chat API** (Gemini, OpenRouter+Google). Uses an existing
//!     chat client; audio is sent as an `input_audio` content part.
//!
//! Both implement [`TranscriptionProvider`] so the adapter doesn't care which
//! one it's wrapping.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use lru::LruCache;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use workforce_common::secret::KeySlot;

use crate::llm::chat_transcription::{ChatTranscriptionClient, ChatTranscriptionRequest};
use crate::llm::whisper::{WhisperClient, WhisperRequest};

/// Callback invoked after every successful uncached transcription. Used by
/// the hub to attribute the call's cost to the active agent's session ledger.
///
/// Receives `(model_name, duration_seconds)`. Cache hits do not fire this.
pub type TranscriptionUsageObserver = Arc<dyn Fn(&str, f64) + Send + Sync>;

/// One transcription request — raw audio plus optional hints.
pub struct TranscriptionInput<'a> {
    /// IANA mime type, e.g. `"audio/m4a"`. Determines the upload mime when
    /// hitting Whisper-style endpoints; backends that don't care (chat
    /// multimodal) ignore this.
    pub media_type: &'a str,
    /// Decoded (non-base64) audio bytes.
    pub bytes: &'a [u8],
    /// Caller-supplied filename — used for the upload form-data field and
    /// surfaced in the substituted text block.
    pub filename: &'a str,
    /// Optional ISO-639-1 language hint. Omit for auto-detect.
    pub language: Option<&'a str>,
}

/// Result of a successful transcription.
#[derive(Debug, Clone)]
pub struct TranscriptionOutput {
    pub text: String,
    /// Audio duration in seconds. Used for cost attribution. `None` when the
    /// backend doesn't return it.
    pub duration_seconds: Option<f64>,
    /// Detected language, when the backend returns it.
    pub language: Option<String>,
}

/// A backend that can transcribe audio to text. Implemented by both the
/// Whisper-style adapter and the chat-multimodal adapter so the cache layer
/// is shared.
#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    async fn transcribe(&self, input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput>;

    /// Concrete model name (e.g. `"whisper-1"`); used in the substituted
    /// text-block byline.
    fn model_name(&self) -> &str;
}

/// Wraps a [`TranscriptionProvider`] with a bounded LRU cache keyed by
/// SHA-256 of the audio bytes so re-uploaded clips don't re-bill the
/// provider, plus an optional usage observer for cost attribution.
pub struct CachingTranscriber {
    inner: Box<dyn TranscriptionProvider>,
    cache: Option<Mutex<LruCache<[u8; 32], TranscriptionOutput>>>,
    usage_observer: Option<TranscriptionUsageObserver>,
    payer: Option<KeySlot>,
}

impl CachingTranscriber {
    /// `cache_size = 0` disables caching.
    pub fn new(inner: Box<dyn TranscriptionProvider>, cache_size: usize) -> Self {
        let cache = NonZeroUsize::new(cache_size).map(|n| Mutex::new(LruCache::new(n)));
        Self {
            inner,
            cache,
            usage_observer: None,
            payer: None,
        }
    }

    pub fn with_usage_observer(mut self, observer: TranscriptionUsageObserver) -> Self {
        self.usage_observer = Some(observer);
        self
    }

    /// The key that pays for a transcription. Transcripts are kept apart by
    /// payer, and someone with no key is refused before the cache is read,
    /// so nobody is served what another person's key paid for.
    pub fn with_payer(mut self, payer: KeySlot) -> Self {
        self.payer = Some(payer);
        self
    }

    fn cache_key(&self, audio: &[u8]) -> Result<[u8; 32]> {
        let payer = self.payer.as_ref().map(KeySlot::payer).transpose()?;
        Ok(payer
            .iter()
            .fold(Sha256::new(), |hasher, payer| hasher.chain_update(payer))
            .chain_update(audio)
            .finalize()
            .into())
    }

    fn cached(&self, key: &[u8; 32]) -> Option<TranscriptionOutput> {
        self.cache.as_ref()?.lock().get(key).cloned()
    }

    fn store(&self, key: [u8; 32], output: &TranscriptionOutput) {
        if let Some(cache) = self.cache.as_ref() {
            cache.lock().put(key, output.clone());
        }
    }
}

#[async_trait]
impl TranscriptionProvider for CachingTranscriber {
    async fn transcribe(&self, input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
        let key = self.cache_key(input.bytes)?;
        if let Some(cached) = self.cached(&key) {
            tracing::debug!(model = %self.inner.model_name(), "Transcription cache hit");
            return Ok(cached);
        }

        let output = self.inner.transcribe(input).await?;

        if let (Some(observer), Some(secs)) =
            (self.usage_observer.as_ref(), output.duration_seconds)
        {
            observer(self.inner.model_name(), secs);
        }
        if self.cache_key(input.bytes).is_ok_and(|after| after == key) {
            self.store(key, &output);
        }

        Ok(output)
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
}

/// Adapts [`WhisperClient`] (OpenAI / Groq / vLLM Whisper) into the trait.
pub struct WhisperTranscriptionProvider {
    client: WhisperClient,
    model: String,
}

impl WhisperTranscriptionProvider {
    pub fn new(client: WhisperClient, model: String) -> Self {
        Self { client, model }
    }
}

#[async_trait]
impl TranscriptionProvider for WhisperTranscriptionProvider {
    async fn transcribe(&self, input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
        let (filename, media_type) = whisper_upload_identity(input.filename, input.media_type);
        let response = self
            .client
            .transcribe(WhisperRequest {
                model: &self.model,
                audio: input.bytes.to_vec(),
                filename: &filename,
                mime_type: media_type,
                language: input.language,
                prompt: None,
            })
            .await?;
        Ok(TranscriptionOutput {
            text: response.text,
            duration_seconds: response.duration,
            language: response.language,
        })
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

/// Normalise the multipart upload identity for Whisper-style endpoints.
///
/// OpenAI's `/audio/transcriptions` decides the container format from the
/// upload *filename* extension and rejects `.opus` ("Invalid file format")
/// even though WhatsApp voice notes named `.opus` are ordinary Ogg streams —
/// so upload them as `.ogg`, mime to match. Groq and vLLM accept either
/// spelling, which makes the rename safe across hosts. Byte content is
/// untouched.
fn whisper_upload_identity<'a>(
    filename: &'a str,
    media_type: &'a str,
) -> (std::borrow::Cow<'a, str>, &'a str) {
    let media_type = if media_type.eq_ignore_ascii_case("audio/opus") {
        "audio/ogg"
    } else {
        media_type
    };
    let is_opus_name = std::path::Path::new(filename)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("opus"));
    if is_opus_name {
        let stem = &filename[..filename.len() - "opus".len()];
        (std::borrow::Cow::Owned(format!("{stem}ogg")), media_type)
    } else {
        (std::borrow::Cow::Borrowed(filename), media_type)
    }
}

/// Adapts [`ChatTranscriptionClient`] (Gemini direct / OpenRouter+Google /
/// vLLM-multimodal) into the trait. The chat path doesn't return audio
/// duration; the caller is expected to probe duration externally
/// (`ffprobe`) when cost attribution matters.
pub struct ChatTranscriptionProvider {
    client: ChatTranscriptionClient,
    model: String,
}

impl ChatTranscriptionProvider {
    pub fn new(client: ChatTranscriptionClient, model: String) -> Self {
        Self { client, model }
    }
}

#[async_trait]
impl TranscriptionProvider for ChatTranscriptionProvider {
    async fn transcribe(&self, input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
        // Gemini's OpenAI-compatible endpoint accepts only `wav`/`mp3`
        // `input_audio` parts ("Invalid audio format \"m4a\" ... Valid
        // formats are: [wav, mp3]", seen live) — and Slack voice notes are
        // audio/mp4. Transcode everything else to 16 kHz mono WAV first. If
        // ffmpeg is unavailable, fall back to sending the original bytes:
        // lenient upstreams (OpenRouter) accepted them before this transcode
        // existed, so failing hard here would regress those installs.
        let (audio, mime_type): (std::borrow::Cow<'_, [u8]>, &str) =
            if chat_transcription_needs_wav(input.media_type) {
                match transcode_to_wav_16k_mono(input.bytes).await {
                    Ok(wav) => (std::borrow::Cow::Owned(wav), "audio/wav"),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            mime = input.media_type,
                            "audio transcode failed; sending original bytes"
                        );
                        (std::borrow::Cow::Borrowed(input.bytes), input.media_type)
                    }
                }
            } else {
                (std::borrow::Cow::Borrowed(input.bytes), input.media_type)
            };

        let response = self
            .client
            .transcribe(ChatTranscriptionRequest {
                model: &self.model,
                audio: &audio,
                mime_type,
                language: input.language,
            })
            .await?;
        Ok(TranscriptionOutput {
            text: response.text,
            duration_seconds: None,
            language: input.language.map(str::to_string),
        })
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

/// True when `mime` is not already a chat-safe `wav`/`mp3` payload and should
/// be transcoded before hitting a chat-multimodal endpoint.
fn chat_transcription_needs_wav(mime: &str) -> bool {
    !matches!(
        mime.to_ascii_lowercase().as_str(),
        "audio/wav" | "audio/x-wav" | "audio/mp3" | "audio/mpeg" | "audio/mpga"
    )
}

/// Transcode arbitrary audio bytes to 16 kHz mono WAV via `ffmpeg` — ample
/// for speech transcription, and it keeps the base64 payload small.
async fn transcode_to_wav_16k_mono(bytes: &[u8]) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let in_path = dir.path().join("in.audio");
    let out_path = dir.path().join("out.wav");
    tokio::fs::write(&in_path, bytes).await?;

    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(&in_path)
        .args(["-ar", "16000", "-ac", "1", "-f", "wav"])
        .arg(&out_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| eyre::eyre!("ffmpeg transcode timed out"))?
        .map_err(|e| eyre::eyre!("ffmpeg not available: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        eyre::bail!("ffmpeg transcode failed ({}): {tail}", output.status);
    }

    let wav = tokio::fs::read(&out_path).await?;
    if wav.is_empty() {
        eyre::bail!("ffmpeg produced an empty wav");
    }
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn whisper_upload_renames_opus_to_ogg() {
        // OpenAI rejects `.opus` filenames; the bytes are Ogg, so rename.
        let (name, mime) = whisper_upload_identity("PTT-20260722-WA0001.opus", "audio/ogg");
        assert_eq!(name, "PTT-20260722-WA0001.ogg");
        assert_eq!(mime, "audio/ogg");
    }

    #[test]
    fn whisper_upload_remaps_opus_mime() {
        let (name, mime) = whisper_upload_identity("note.OPUS", "audio/opus");
        assert_eq!(name, "note.ogg");
        assert_eq!(mime, "audio/ogg");
    }

    #[test]
    fn whisper_upload_leaves_other_files_alone() {
        let (name, mime) = whisper_upload_identity("voice.m4a", "audio/mp4");
        assert_eq!(name, "voice.m4a");
        assert_eq!(mime, "audio/mp4");
        // No extension → nothing to rename.
        let (name, _) = whisper_upload_identity("opus", "audio/ogg");
        assert_eq!(name, "opus");
    }

    #[test]
    fn chat_path_transcodes_everything_but_wav_and_mp3() {
        // Slack voice notes (iOS m4a, Chrome webm) must be transcoded for
        // Gemini's OpenAI-compat endpoint, which accepts only wav|mp3.
        assert!(chat_transcription_needs_wav("audio/mp4"));
        assert!(chat_transcription_needs_wav("audio/x-m4a"));
        assert!(chat_transcription_needs_wav("audio/webm"));
        assert!(chat_transcription_needs_wav("audio/ogg"));
        assert!(chat_transcription_needs_wav("audio/aiff"));
        assert!(!chat_transcription_needs_wav("audio/wav"));
        assert!(!chat_transcription_needs_wav("audio/x-wav"));
        assert!(!chat_transcription_needs_wav("audio/mpeg"));
        assert!(!chat_transcription_needs_wav("Audio/MP3"));
    }

    struct FakeProvider {
        calls: Arc<AtomicUsize>,
        text: String,
        duration: Option<f64>,
    }

    #[async_trait]
    impl TranscriptionProvider for FakeProvider {
        async fn transcribe(&self, _input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TranscriptionOutput {
                text: self.text.clone(),
                duration_seconds: self.duration,
                language: None,
            })
        }
        fn model_name(&self) -> &str {
            "fake"
        }
    }

    fn fake(text: &str, duration: Option<f64>) -> (CachingTranscriber, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Box::new(FakeProvider {
            calls: Arc::clone(&calls),
            text: text.to_string(),
            duration,
        });
        (CachingTranscriber::new(provider, 8), calls)
    }

    fn input<'a>(bytes: &'a [u8]) -> TranscriptionInput<'a> {
        TranscriptionInput {
            media_type: "audio/m4a",
            bytes,
            filename: "voice.m4a",
            language: None,
        }
    }

    #[tokio::test]
    async fn cache_hit_skips_provider_call() {
        let (t, calls) = fake("hello", Some(1.0));
        let _ = t.transcribe(&input(b"audio")).await.unwrap();
        let _ = t.transcribe(&input(b"audio")).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_transcript_is_served_only_to_the_payer_whose_key_bought_it() {
        use crate::runner::CURRENT_USER_ID;
        use workforce_common::secret::NoKey;

        let (t, calls) = fake("hello", Some(1.0));
        let t = t.with_payer(KeySlot::new(
            Arc::new(|| {
                let person = CURRENT_USER_ID.try_with(Clone::clone).ok()?;
                match person.as_str() {
                    "alice" => Some("alice-key".to_string()),
                    "bob" | "carol" => Some("shared-key".to_string()),
                    _ => None,
                }
            }),
            "no key for you",
        ));
        let clip = input(b"clip");
        let hear = |person: &str| CURRENT_USER_ID.scope(person.to_string(), t.transcribe(&clip));

        hear("alice").await.unwrap();
        hear("alice").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "alice pays once");

        let refused = hear("dave").await.unwrap_err();
        assert_eq!(
            refused.downcast_ref::<NoKey>(),
            Some(&NoKey("no key for you".into())),
            "someone with no key is refused before the cache is read"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        hear("bob").await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "another key pays for its own transcript"
        );
        hear("carol").await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "people on one shared key share what it paid for"
        );
    }

    #[tokio::test]
    async fn a_transcript_is_not_kept_when_the_payer_changed_during_the_call() {
        let reads = Arc::new(AtomicUsize::new(0));
        let (t, calls) = fake("hello", Some(1.0));
        let t = t.with_payer(KeySlot::new(
            {
                let reads = Arc::clone(&reads);
                Arc::new(move || match reads.fetch_add(1, Ordering::SeqCst) {
                    0 => Some("shared-key".to_string()),
                    _ => Some("alices-new-key".to_string()),
                })
            },
            "none",
        ));
        let clip = input(b"clip");
        t.transcribe(&clip).await.unwrap();
        t.transcribe(&clip).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "nothing was filed under the key that did not pay"
        );
        t.transcribe(&clip).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cache_miss_for_different_payload() {
        let (t, calls) = fake("hello", Some(1.0));
        let _ = t.transcribe(&input(b"first")).await.unwrap();
        let _ = t.transcribe(&input(b"second")).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn observer_fires_only_when_duration_available() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Box::new(FakeProvider {
            calls: Arc::clone(&calls),
            text: "x".to_string(),
            duration: Some(2.5),
        });
        let observed = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
        let observed_clone = Arc::clone(&observed);
        let t = CachingTranscriber::new(provider, 8).with_usage_observer(Arc::new(
            move |_model, secs| observed_clone.lock().push(secs),
        ));
        let _ = t.transcribe(&input(b"a")).await.unwrap();
        let _ = t.transcribe(&input(b"a")).await.unwrap(); // cache hit
        let _ = t.transcribe(&input(b"b")).await.unwrap();
        assert_eq!(*observed.lock(), vec![2.5, 2.5]); // fired on uncached calls only
    }

    #[tokio::test]
    async fn observer_skipped_when_duration_missing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Box::new(FakeProvider {
            calls: Arc::clone(&calls),
            text: "x".to_string(),
            duration: None,
        });
        let observed = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
        let observed_clone = Arc::clone(&observed);
        let t = CachingTranscriber::new(provider, 8)
            .with_usage_observer(Arc::new(move |_m, s| observed_clone.lock().push(s)));
        let _ = t.transcribe(&input(b"x")).await.unwrap();
        assert!(observed.lock().is_empty());
    }
}
