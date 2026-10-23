//! Video attachment audio: extract the audio track with `ffmpeg`, then route
//! it through the transcription pipeline. Long videos are split into
//! `chunk_minutes` segments during extraction, each transcribed separately
//! and stitched with time offsets (see [`super::audio`]). What the video shows
//! is [`super::frames`].
//!
//! When `ffmpeg` isn't installed we degrade to a placeholder rather than
//! fail — same shape as the PDF rasterisation fallback.

use std::path::Path;

use crate::llm::types::ContentBlock;
use crate::transcription::TranscriptionProvider;
use eyre::Result;
use tempfile::TempDir;
use workforce_common::settings::LlmTranscriptionConfig;

use super::audio::{
    chunk_seconds, duration_cap_block, over_cap, probe_duration_seconds, segment_blocking,
    transcribe_segments,
};

/// Transcribe the audio of a video file on disk. Inline and by-reference
/// attachments both land on disk first: ffmpeg needs a seekable file, and a
/// large upload never transits memory as one buffer.
pub async fn process_video_file(
    filename: &str,
    path: &Path,
    provider: &dyn TranscriptionProvider,
    config: &LlmTranscriptionConfig,
) -> Result<Vec<ContentBlock>> {
    // Cap first: probing the container is cheap, so a five-hour upload never
    // pays for a transcode whose output we'd refuse to send anyway.
    let duration = probe_duration_seconds(path);
    if let Some(secs) = duration.filter(|s| over_cap(*s, config)) {
        return Ok(vec![duration_cap_block("Video", filename, secs, config)]);
    }

    // Segment only when the duration is known to exceed one chunk; an
    // unprobeable container still extracts as a single file, exactly as
    // before chunking existed.
    let chunk = match (duration, chunk_seconds(config)) {
        (Some(total), Some(chunk)) if total > chunk as f64 => Some(chunk),
        _ => None,
    };

    let dir = TempDir::new()?;
    match segment_blocking(path, dir.path(), chunk).await {
        Ok(segments) => {
            let extracted: u64 = segments
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum();
            tracing::info!(
                file = %filename,
                audio_bytes = extracted,
                segments = segments.len(),
                "extracted audio from video"
            );
            transcribe_segments(
                filename,
                &segments,
                duration.unwrap_or_default(),
                chunk.unwrap_or(usize::MAX),
                provider,
                config,
            )
            .await
        }
        Err(e) => {
            tracing::warn!(file = %filename, error = %e, "video audio extraction failed; emitting placeholder");
            Ok(vec![ContentBlock::Text {
                text: format!(
                    "Video `{filename}` was attached. Audio-track extraction failed: {e}. \
                     Install `ffmpeg` on the agent host to enable video transcription, or \
                     extract the audio upstream.",
                ),
            }])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::{TranscriptionInput, TranscriptionOutput};
    use std::io::Write;
    use std::process::{Command, Stdio};
    use tempfile::NamedTempFile;

    async fn process_video(
        filename: &str,
        bytes: &[u8],
        provider: &dyn TranscriptionProvider,
        config: &LlmTranscriptionConfig,
    ) -> Result<Vec<ContentBlock>> {
        let mut tmp = NamedTempFile::new()?;
        tmp.write_all(bytes)?;
        process_video_file(filename, tmp.path(), provider, config).await
    }

    /// The test paths exit before transcription is attempted, so the provider
    /// is unreachable — but the trait still has to be satisfied.
    struct UnreachableProvider;

    #[async_trait::async_trait]
    impl TranscriptionProvider for UnreachableProvider {
        async fn transcribe(&self, _input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
            unreachable!("test exits before transcription")
        }
        fn model_name(&self) -> &str {
            "unreachable"
        }
    }

    struct CountingProvider(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl TranscriptionProvider for CountingProvider {
        async fn transcribe(&self, _input: &TranscriptionInput<'_>) -> Result<TranscriptionOutput> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(TranscriptionOutput {
                text: "segment text".into(),
                duration_seconds: None,
                language: None,
            })
        }
        fn model_name(&self) -> &str {
            "counting"
        }
    }

    fn cfg() -> LlmTranscriptionConfig {
        LlmTranscriptionConfig {
            model: "fake".to_string(),
            ..Default::default()
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

    /// A test clip of `secs` seconds: colour bars plus a sine tone, so both
    /// streams exist and the duration is what we asked for.
    fn sample_video(secs: u32) -> Vec<u8> {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("sample.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={secs}:size=64x64:rate=10"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&out)
            .stderr(Stdio::piped())
            .output()
            .expect("spawn ffmpeg");
        assert!(
            status.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        std::fs::read(&out).unwrap()
    }

    /// Probe a stream property of the extracted audio via `ffprobe`.
    fn probe_stream(audio: &Path, entry: &str) -> String {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                &format!("stream={entry}"),
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(audio)
            .output()
            .expect("spawn ffprobe");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn extracted_audio_is_mono_and_small() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let video = sample_video(2);
        let mut input = NamedTempFile::new().unwrap();
        input.write_all(&video).unwrap();
        let dir = TempDir::new().unwrap();
        let segments =
            crate::attachments::audio::segment_to_speech_mp3(input.path(), dir.path(), None)
                .expect("extraction");
        assert_eq!(segments.len(), 1);
        assert_eq!(probe_stream(&segments[0], "channels"), "1", "must be mono");
        assert_eq!(
            probe_stream(&segments[0], "sample_rate"),
            "22050",
            "below 32 kHz libmp3lame drops to MPEG-2; 22.05 kHz keeps MPEG-1"
        );
        // 2s at 32 kbps ≈ 8 KB. Generous ceiling; the point is that we are
        // nowhere near the ~1 MB/min the old stereo VBR encode produced.
        let len = std::fs::metadata(&segments[0]).unwrap().len();
        assert!(
            len < 40_000,
            "extracted audio unexpectedly large: {len} bytes"
        );
    }

    #[tokio::test]
    async fn video_over_duration_cap_emits_placeholder() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let config = LlmTranscriptionConfig {
            model: "fake".to_string(),
            max_audio_minutes: 1,
            ..Default::default()
        };
        // 65s against a 1-minute cap — a real cap, not the degenerate zero.
        let blocks = process_video("long.mp4", &sample_video(65), &UnreachableProvider, &config)
            .await
            .unwrap();
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("long.mp4"));
        assert!(text.contains("cap"), "got: {text}");
        assert!(text.contains("max_audio_minutes"), "got: {text}");
    }

    #[tokio::test]
    async fn ffmpeg_failure_emits_placeholder_not_failure() {
        // Bytes that aren't a valid video. ffmpeg either fails to parse them
        // or isn't installed — both must produce a placeholder, not Err.
        let blocks = process_video(
            "broken.mp4",
            b"not a real video file",
            &UnreachableProvider,
            &cfg(),
        )
        .await
        .unwrap();
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert!(text.contains("broken.mp4"));
        assert!(text.contains("extraction failed") || text.contains("ffmpeg"));
    }

    #[tokio::test]
    async fn long_video_is_transcribed_in_parts() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        // 65s video with 1-minute chunks → 2 segments, 2 provider calls.
        let config = LlmTranscriptionConfig {
            model: "fake".to_string(),
            chunk_minutes: 1,
            ..Default::default()
        };
        let provider = CountingProvider(std::sync::atomic::AtomicUsize::new(0));
        let blocks = process_video("long.mp4", &sample_video(65), &provider, &config)
            .await
            .unwrap();
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("expected text block");
        };
        assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(text.contains("in 2 parts"), "got: {text}");
        assert!(text.contains("[part 1/2 — 0:00–1:00]"), "got: {text}");
        assert!(text.contains("[part 2/2 — 1:00–1:05]"), "got: {text}");
    }
}
