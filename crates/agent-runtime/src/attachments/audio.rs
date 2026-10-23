//! Audio attachment processing.
//!
//! Decodes the payload (or takes a file already on disk for by-reference
//! attachments), checks duration against `max_audio_minutes`, and hands off
//! to the configured [`TranscriptionProvider`]. Media longer than
//! `chunk_minutes` is transcoded to speech-shaped mono MP3 and split with
//! `ffmpeg -f segment`, each chunk transcribed separately and stitched with
//! time offsets — so one provider request never carries more than a chunk's
//! worth of audio. Duration probe uses `ffprobe` (ships with ffmpeg); when
//! missing we skip the cap and send the whole file — the provider's own
//! limits act as a backstop.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::llm::types::ContentBlock;
use crate::transcription::{TranscriptionInput, TranscriptionProvider};
use base64::Engine;
use eyre::{eyre, Result};
use tempfile::{NamedTempFile, TempDir};
use workforce_common::secret::NoKey;
use workforce_common::settings::LlmTranscriptionConfig;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Process an inline (base64) audio attachment, returning the content blocks
/// to substitute in place of the original (saved-to-workspace) handling.
pub async fn process_audio(
    filename: &str,
    base64_data: &str,
    mime_type: &str,
    provider: &dyn TranscriptionProvider,
    config: &LlmTranscriptionConfig,
) -> Result<Vec<ContentBlock>> {
    let bytes = BASE64
        .decode(base64_data)
        .map_err(|e| eyre!("base64 decode failed: {e}"))?;
    let mut tmp = NamedTempFile::new()?;
    tmp.write_all(&bytes)?;
    process_audio_file(filename, tmp.path(), mime_type, provider, config).await
}

/// Process an audio file already on disk — the shared core behind the inline
/// path above and the by-reference (artifact store) path, which materialises
/// to disk precisely so a large file never has to live in memory whole.
/// Past this size a file is never shipped to a provider in its original
/// container: a short-but-dense format (hi-res WAV/FLAC easily reaches
/// hundreds of MB inside ten minutes) or an unprobeable one is transcoded to
/// speech-shaped MP3 first, so neither hub memory nor the request body ever
/// carries the original bytes whole.
const SINGLE_SHOT_MAX_BYTES: u64 = 24 * 1024 * 1024;

pub async fn process_audio_file(
    filename: &str,
    path: &Path,
    mime_type: &str,
    provider: &dyn TranscriptionProvider,
    config: &LlmTranscriptionConfig,
) -> Result<Vec<ContentBlock>> {
    let duration = probe_duration_seconds(path);
    if let Some(secs) = duration.filter(|s| over_cap(*s, config)) {
        return Ok(vec![duration_cap_block("Audio", filename, secs, config)]);
    }

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let must_chunk = match (duration, chunk_seconds(config)) {
        (Some(total), Some(chunk)) => total > chunk as f64,
        (None, Some(_)) => file_size > SINGLE_SHOT_MAX_BYTES,
        _ => false,
    };
    if must_chunk {
        let chunk = chunk_seconds(config).expect("must_chunk implies chunking enabled");
        let dir = TempDir::new()?;
        let segments = segment_blocking(path, dir.path(), Some(chunk)).await?;
        // Container duration was unreadable, but the transcoded MP3 segments
        // are ours — reconstruct the total so the budget still binds and the
        // stitched offsets stay honest.
        let total = duration.unwrap_or_else(|| estimate_total_secs(&segments, chunk));
        if over_cap(total, config) {
            return Ok(vec![duration_cap_block("Audio", filename, total, config)]);
        }
        return transcribe_segments(filename, &segments, total, chunk, provider, config).await;
    }

    if file_size > SINGLE_SHOT_MAX_BYTES {
        // Short (or unprobeable) but dense — one request, but of the small
        // speech-shaped encode, never the original container.
        let dir = TempDir::new()?;
        let segments = segment_blocking(path, dir.path(), None).await?;
        let first = segments
            .first()
            .ok_or_else(|| eyre!("transcode produced no audio (no audio track?)"))?;
        let bytes = std::fs::read(first)?;
        return transcribe_and_format(filename, &bytes, "audio/mpeg", provider, config).await;
    }

    // Small media: whole file in one request, in its original container —
    // the path every provider already accepts.
    let bytes = std::fs::read(path)?;
    transcribe_and_format(filename, &bytes, mime_type, provider, config).await
}

/// [`segment_to_speech_mp3`] on the blocking pool — transcoding a multi-hour
/// file takes minutes of wall time and must not pin a tokio worker thread.
pub(crate) async fn segment_blocking(
    input: &Path,
    dir: &Path,
    chunk_secs: Option<usize>,
) -> Result<Vec<PathBuf>> {
    let input = input.to_path_buf();
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || segment_to_speech_mp3(&input, &dir, chunk_secs))
        .await
        .map_err(|e| eyre!("transcode task panicked: {e}"))?
}

/// Total duration reconstructed from segment files: full chunks plus a probe
/// of the last (possibly short) one. Falls back to counting the last as a
/// full chunk if even the MP3 won't probe.
fn estimate_total_secs(segments: &[PathBuf], chunk_secs: usize) -> f64 {
    let full = segments.len().saturating_sub(1) as f64 * chunk_secs as f64;
    let last = segments
        .last()
        .and_then(|p| probe_duration_seconds(p))
        .unwrap_or(chunk_secs as f64);
    full + last
}

/// Placeholder for media over the `max_audio_minutes` budget.
pub(crate) fn duration_cap_block(
    kind: &str,
    filename: &str,
    secs: f64,
    config: &LlmTranscriptionConfig,
) -> ContentBlock {
    tracing::warn!(
        file = %filename,
        duration_secs = secs,
        cap_minutes = config.max_audio_minutes,
        "media exceeds max_audio_minutes; emitting placeholder"
    );
    ContentBlock::Text {
        text: format!(
            "{kind} `{filename}` was attached ({secs:.0}s) but exceeds the \
             {cap}-minute transcription cap. Increase \
             [llm.transcription].max_audio_minutes to transcribe longer files.",
            cap = config.max_audio_minutes,
        ),
    }
}

/// Shared by the audio path and the video path (video extracts audio first
/// then calls this). Caller is responsible for the duration cap.
pub(crate) async fn transcribe_and_format(
    filename: &str,
    bytes: &[u8],
    mime_type: &str,
    provider: &dyn TranscriptionProvider,
    config: &LlmTranscriptionConfig,
) -> Result<Vec<ContentBlock>> {
    let input = TranscriptionInput {
        media_type: mime_type,
        bytes,
        filename,
        language: config.language.as_deref(),
    };

    match provider.transcribe(&input).await {
        Ok(output) => {
            let duration_note = output
                .duration_seconds
                .map(|s| format!(", duration {:.0}s", s))
                .unwrap_or_default();
            let language_note = output
                .language
                .as_deref()
                .map(|l| format!(", language {l}"))
                .unwrap_or_default();
            Ok(vec![ContentBlock::Text {
                text: format!(
                    "Audio `{filename}` (transcribed by {model}{duration_note}{language_note}):\n{text}",
                    model = provider.model_name(),
                    text = output.text,
                ),
            }])
        }
        Err(e) if e.is::<NoKey>() => Ok(vec![not_transcribed(filename, &e)]),
        Err(e) => {
            tracing::warn!(file = %filename, error = %e, "transcription failed; emitting placeholder");
            Ok(vec![ContentBlock::Text {
                text: format!("Audio `{filename}` was attached but transcription failed: {e}"),
            }])
        }
    }
}

/// What stands in for audio nobody's key could transcribe.
fn not_transcribed(filename: &str, why: &eyre::Report) -> ContentBlock {
    ContentBlock::Text {
        text: format!("Audio `{filename}` was attached but not transcribed: {why}"),
    }
}

/// Transcribe pre-split MP3 segments in order and stitch the parts into one
/// transcript block with time offsets. A failed part becomes an inline note
/// rather than sinking the whole file — a transcript with one silent window
/// beats no transcript.
pub(crate) async fn transcribe_segments(
    filename: &str,
    segments: &[PathBuf],
    total_secs: f64,
    chunk_secs: usize,
    provider: &dyn TranscriptionProvider,
    config: &LlmTranscriptionConfig,
) -> Result<Vec<ContentBlock>> {
    if segments.is_empty() {
        return Err(eyre!("segmentation produced no output"));
    }
    if segments.len() == 1 {
        let bytes = std::fs::read(&segments[0])?;
        return transcribe_and_format(filename, &bytes, "audio/mpeg", provider, config).await;
    }

    let total = segments.len();
    let mut parts: Vec<String> = Vec::with_capacity(total);
    let mut failed = 0usize;
    for (i, segment) in segments.iter().enumerate() {
        let start = i as f64 * chunk_secs as f64;
        let end = ((i + 1) as f64 * chunk_secs as f64).min(total_secs);
        let header = format!(
            "[part {n}/{total} — {s}–{e}]",
            n = i + 1,
            s = format_offset(start),
            e = format_offset(end),
        );
        let bytes = std::fs::read(segment)?;
        let input = TranscriptionInput {
            media_type: "audio/mpeg",
            bytes: &bytes,
            filename,
            language: config.language.as_deref(),
        };
        match provider.transcribe(&input).await {
            Ok(output) => parts.push(format!("{header}\n{}", output.text)),
            Err(e) if e.is::<NoKey>() => return Ok(vec![not_transcribed(filename, &e)]),
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    file = %filename,
                    part = i + 1,
                    total,
                    error = %e,
                    "segment transcription failed; continuing with remaining parts"
                );
                parts.push(format!(
                    "{header}\n[transcription of this part failed: {e}]"
                ));
            }
        }
    }
    if failed == total {
        return Ok(vec![ContentBlock::Text {
            text: format!(
                "Audio `{filename}` was attached but transcription failed for all \
                 {total} parts. First part: {first}",
                first = parts[0],
            ),
        }]);
    }
    Ok(vec![ContentBlock::Text {
        text: format!(
            "Audio `{filename}` (transcribed by {model} in {total} parts, duration {total_secs:.0}s):\n{body}",
            model = provider.model_name(),
            body = parts.join("\n"),
        ),
    }])
}

/// `m:ss` under an hour, `h:mm:ss` above — the shape people expect from
/// video players, so offsets in the transcript are directly seekable.
fn format_offset(secs: f64) -> String {
    let s = secs.round() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Encode any container ffmpeg reads (audio or video — `-vn` drops video
/// streams) to speech-shaped mono MP3: 22.05 kHz, 32 kbps, ≈0.24 MB/min.
///
/// 22.05 kHz rather than the 16 kHz ASR models actually want: below 32 kHz
/// libmp3lame switches to MPEG-2 Layer III, and while every decoder handles
/// it, the sample rate costs nothing here (bitrate sets the size) and staying
/// on MPEG-1 keeps the payload identical in shape to what providers already
/// accept from the plain-audio path.
///
/// With `chunk_secs` set the encode is split into segments of that length
/// (`-f segment`); `None` produces a single file. Returns the segment paths
/// in playback order.
pub(crate) fn segment_to_speech_mp3(
    input: &Path,
    dir: &Path,
    chunk_secs: Option<usize>,
) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args([
            "-vn", // drop video stream
            "-acodec",
            "libmp3lame",
            "-ac",
            "1", // mono
            "-ar",
            "22050", // highest rate that stays MPEG-1 (see above)
            "-b:a",
            "32k", // ≈0.24 MB/min of speech
        ]);
    match chunk_secs {
        Some(secs) => {
            cmd.args([
                "-f",
                "segment",
                "-segment_time",
                &secs.to_string(),
                "-reset_timestamps",
                "1",
            ])
            .arg(dir.join("chunk%04d.mp3"));
        }
        None => {
            cmd.arg(dir.join("chunk0000.mp3"));
        }
    }
    let output = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| eyre!("failed to spawn ffmpeg: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(eyre!(
            "ffmpeg exited with status {}: {}",
            output.status,
            if detail.is_empty() {
                "no stderr output"
            } else {
                detail
            }
        ));
    }
    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "mp3")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("chunk"))
        })
        .collect();
    segments.sort();
    Ok(segments)
}

/// The configured segment length in seconds, `None` when chunking is
/// disabled (`chunk_minutes = 0`).
pub(crate) fn chunk_seconds(config: &LlmTranscriptionConfig) -> Option<usize> {
    (config.chunk_minutes > 0).then_some(config.chunk_minutes * 60)
}

/// Whether a probed duration exceeds the `max_audio_minutes` budget. Note
/// `max_audio_minutes = 0` means "reject everything", not "uncapped" —
/// pre-existing semantics, deliberately preserved.
pub(crate) fn over_cap(secs: f64, config: &LlmTranscriptionConfig) -> bool {
    secs > (config.max_audio_minutes as f64) * 60.0
}

/// Probe media duration via `ffprobe`. Returns `None` when the tool isn't
/// installed or the input can't be parsed — the caller proceeds without the
/// duration cap rather than blocking.
pub(crate) fn probe_duration_seconds(path: &Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::TranscriptionOutput;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeProvider {
        calls: Arc<AtomicUsize>,
        text: String,
    }

    #[async_trait]
    impl TranscriptionProvider for FakeProvider {
        async fn transcribe(&self, _input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TranscriptionOutput {
                text: self.text.clone(),
                duration_seconds: Some(12.5),
                language: Some("en".to_string()),
            })
        }
        fn model_name(&self) -> &str {
            "fake-whisper"
        }
    }

    fn cfg() -> LlmTranscriptionConfig {
        LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..LlmTranscriptionConfig::default()
        }
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// A sine-tone MP3 of `secs` seconds for chunking tests.
    fn sample_audio(secs: u32) -> NamedTempFile {
        let tmp = tempfile::Builder::new().suffix(".mp3").tempfile().unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={secs}"),
            ])
            .arg(tmp.path())
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success());
        tmp
    }

    #[tokio::test]
    async fn happy_path_emits_transcript_block() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = FakeProvider {
            calls: Arc::clone(&calls),
            text: "hello there".to_string(),
        };
        let blocks = process_audio("memo.m4a", "AAAA", "audio/mp4", &provider, &cfg())
            .await
            .unwrap();
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("memo.m4a"));
        assert!(text.contains("hello there"));
        assert!(text.contains("fake-whisper"));
        assert!(text.contains("duration ") && text.contains("s"));
        assert!(text.contains("language en"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_base64_returns_error() {
        let provider = FakeProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            text: "x".to_string(),
        };
        assert!(
            process_audio("bad.m4a", "not-base64!!!", "audio/mp4", &provider, &cfg())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn provider_error_emits_placeholder_not_failure() {
        struct ErrProvider;
        #[async_trait]
        impl TranscriptionProvider for ErrProvider {
            async fn transcribe(
                &self,
                _input: &TranscriptionInput<'_>,
            ) -> Result<TranscriptionOutput> {
                Err(eyre!("upstream rate-limited"))
            }
            fn model_name(&self) -> &str {
                "fake"
            }
        }
        let blocks = process_audio("m.m4a", "AAAA", "audio/mp4", &ErrProvider, &cfg())
            .await
            .unwrap();
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("transcription failed"));
        assert!(text.contains("rate-limited"));
    }

    #[tokio::test]
    async fn long_audio_is_chunked_and_stitched() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = FakeProvider {
            calls: Arc::clone(&calls),
            text: "chunk transcript".to_string(),
        };
        // 25s of audio segmented at 10s → 3 parts. (chunk_minutes only has
        // minute granularity; the segmenter itself takes seconds.)
        let audio = sample_audio(25);
        let dir = TempDir::new().unwrap();
        let segments = segment_to_speech_mp3(audio.path(), dir.path(), Some(10)).unwrap();
        assert_eq!(segments.len(), 3, "25s at 10s chunks should be 3 segments");
        let blocks = transcribe_segments("long.mp3", &segments, 25.0, 10, &provider, &cfg())
            .await
            .unwrap();
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("in 3 parts"), "got: {text}");
        assert!(text.contains("[part 1/3 — 0:00–0:10]"), "got: {text}");
        assert!(text.contains("[part 3/3 — 0:20–0:25]"), "got: {text}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn failed_segment_becomes_note_not_failure() {
        struct FlakyProvider {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl TranscriptionProvider for FlakyProvider {
            async fn transcribe(&self, _: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    Err(eyre!("mid-file blip"))
                } else {
                    Ok(TranscriptionOutput {
                        text: "ok".into(),
                        duration_seconds: None,
                        language: None,
                    })
                }
            }
            fn model_name(&self) -> &str {
                "flaky"
            }
        }
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let audio = sample_audio(25);
        let dir = TempDir::new().unwrap();
        let segments = segment_to_speech_mp3(audio.path(), dir.path(), Some(10)).unwrap();
        let provider = FlakyProvider {
            calls: AtomicUsize::new(0),
        };
        let blocks = transcribe_segments("f.mp3", &segments, 25.0, 10, &provider, &cfg())
            .await
            .unwrap();
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(
            text.contains("[transcription of this part failed: mid-file blip]"),
            "got: {text}"
        );
        assert!(text.matches("\nok").count() == 2, "got: {text}");
    }

    #[tokio::test]
    async fn a_missing_key_is_said_once_not_for_every_part() {
        struct Keyless(AtomicUsize);
        #[async_trait]
        impl TranscriptionProvider for Keyless {
            async fn transcribe(&self, _: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(NoKey("Transcription is not available to you.".into()).into())
            }
            fn model_name(&self) -> &str {
                "keyless"
            }
        }
        let dir = TempDir::new().unwrap();
        let segments: Vec<PathBuf> = (0..3)
            .map(|i| {
                let path = dir.path().join(format!("{i}.mp3"));
                std::fs::write(&path, b"audio").unwrap();
                path
            })
            .collect();
        let provider = Keyless(AtomicUsize::new(0));
        let blocks = transcribe_segments("f.mp3", &segments, 25.0, 10, &provider, &cfg())
            .await
            .unwrap();
        let [ContentBlock::Text { text }] = blocks.as_slice() else {
            panic!("expected one text block");
        };
        assert_eq!(
            text,
            "Audio `f.mp3` was attached but not transcribed: Transcription is not available to you."
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn format_offset_is_seekable_shape() {
        assert_eq!(format_offset(0.0), "0:00");
        assert_eq!(format_offset(65.0), "1:05");
        assert_eq!(format_offset(3600.0), "1:00:00");
        assert_eq!(format_offset(3725.0), "1:02:05");
    }
}
