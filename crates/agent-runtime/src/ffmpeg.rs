//! `ffmpeg`/`ffprobe` plumbing shared by the video tools
//! ([`crate::tools::video_compose`]) and video attachment frames
//! ([`crate::attachments::frames`]): one probe, one bounded runner, one way of
//! reporting what went wrong.

use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

const STDERR_TAIL_LINES: usize = 20;

/// Reading a container's headers takes milliseconds; a probe still running
/// after this is stuck on a hostile or truncated file.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether both `ffmpeg` and `ffprobe` answer `-version`. Probed once and
/// cached — a binary doesn't appear at runtime, so re-checking per agent is
/// wasted work.
pub fn ffmpeg_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| binary_responds("ffmpeg") && binary_responds("ffprobe"))
}

fn binary_responds(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What `ffprobe` says about a media file.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// Container duration, falling back to the longest stream (0 when unknown).
    pub duration: f64,
    /// A real video stream: cover art embedded in an audio file does not count.
    pub has_video: bool,
    pub has_audio: bool,
    /// Stored pixel dimensions of the first video stream (0 when unknown).
    pub width: u32,
    pub height: u32,
    /// Display rotation in degrees, as phones record it instead of rotating
    /// the pixels. `ffmpeg` applies it when decoding.
    pub rotation: i32,
    /// The video stream's own duration, when it states one — often shorter
    /// than the container's when the audio runs on.
    pub video_duration: Option<f64>,
}

impl Probe {
    /// Width and height as the video is shown, rotation applied.
    pub fn display_size(&self) -> (u32, u32) {
        match self.rotation.rem_euclid(180) {
            90 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }
}

/// Probe `path` with `bin` (normally `ffprobe`).
pub async fn probe_media_with(bin: &str, path: &Path) -> Result<Probe, String> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(path);
    let stdout = run(cmd, PROBE_TIMEOUT)
        .await
        .map_err(|e| format!("{bin} could not read the file: {e}"))?;
    let value: Value = serde_json::from_slice(&stdout)
        .map_err(|e| format!("could not parse {bin} output: {e}"))?;
    Ok(parse_probe(&value))
}

/// [`probe_media_with`] the system `ffprobe`.
pub async fn probe_media(path: &Path) -> Result<Probe, String> {
    probe_media_with("ffprobe", path).await
}

/// Pure projection of ffprobe JSON: any audio stream ⇒ `has_audio`; duration
/// from the container, falling back to the longest stream duration.
pub fn parse_probe(value: &Value) -> Probe {
    let streams = value["streams"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let video_stream = streams
        .iter()
        .find(|st| st["codec_type"] == "video" && st["disposition"]["attached_pic"] != 1);
    let has_audio = streams.iter().any(|st| st["codec_type"] == "audio");
    let dim = |key: &str| {
        video_stream
            .and_then(|s| s[key].as_u64())
            .map(|v| v as u32)
            .unwrap_or(0)
    };
    let parse = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let duration = parse(&value["format"]["duration"])
        .or_else(|| {
            streams
                .iter()
                .filter_map(|st| parse(&st["duration"]))
                .reduce(f64::max)
        })
        .unwrap_or(0.0);
    Probe {
        duration,
        has_video: video_stream.is_some(),
        has_audio,
        width: dim("width"),
        height: dim("height"),
        rotation: video_stream.map(rotation_of).unwrap_or(0),
        video_duration: video_stream.and_then(|s| parse(&s["duration"])),
    }
}

/// A stream's display rotation: the display-matrix side data current ffmpeg
/// reports, else the `rotate` tag older muxers wrote.
fn rotation_of(stream: &Value) -> i32 {
    stream["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|sd| sd["rotation"].as_f64())
        .or_else(|| {
            stream["tags"]["rotate"]
                .as_str()
                .and_then(|r| r.parse().ok())
        })
        .map(|r| r.round() as i32)
        .unwrap_or(0)
}

/// Run `ffmpeg` with `args`, bounded by `timeout`, returning its stdout.
pub async fn run_ffmpeg(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
    run_ffmpeg_with("ffmpeg", args, timeout).await
}

/// [`run_ffmpeg`] with another binary in place of `ffmpeg`.
pub async fn run_ffmpeg_with(
    bin: &str,
    args: &[String],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    run(cmd, timeout).await
}

/// Errors are the stderr tail, or name the binary when it never ran.
async fn run(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    let bin = cmd.as_std().get_program().to_string_lossy().into_owned();
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
        Ok(Ok(out)) => Err(stderr_tail(&out.stderr)),
        Ok(Err(e)) => Err(format!("{bin} could not start: {e}")),
        Err(_) => Err(format!("{bin} timed out after {}s", timeout.as_secs())),
    }
}

/// The last lines of a process's stderr, for an error message.
pub fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "no error output".to_string();
    }
    let mut lines: Vec<&str> = trimmed.lines().rev().take(STDERR_TAIL_LINES).collect();
    lines.reverse();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_probe_reads_streams_and_duration() {
        let with_audio = json!({
            "streams": [
                { "codec_type": "video", "duration": "4.0", "width": 1920, "height": 1080 },
                { "codec_type": "audio" }
            ],
            "format": { "duration": "4.20" }
        });
        assert_eq!(
            parse_probe(&with_audio),
            Probe {
                duration: 4.2,
                has_video: true,
                has_audio: true,
                width: 1920,
                height: 1080,
                rotation: 0,
                video_duration: Some(4.0),
            }
        );
        let no_format = json!({
            "streams": [{ "codec_type": "video", "duration": "3.5" }],
            "format": {}
        });
        assert_eq!(
            parse_probe(&no_format),
            Probe {
                duration: 3.5,
                has_video: true,
                has_audio: false,
                width: 0,
                height: 0,
                rotation: 0,
                video_duration: Some(3.5),
            }
        );
        let audio_only = json!({
            "streams": [{ "codec_type": "audio", "duration": "5.0" }],
            "format": { "duration": "5.0" }
        });
        assert_eq!(
            parse_probe(&audio_only),
            Probe {
                duration: 5.0,
                has_video: false,
                has_audio: true,
                width: 0,
                height: 0,
                rotation: 0,
                video_duration: None,
            }
        );
    }

    #[test]
    fn parse_probe_ignores_cover_art() {
        let song = json!({
            "streams": [
                { "codec_type": "audio", "duration": "180.0" },
                { "codec_type": "video", "width": 600, "height": 600,
                  "disposition": { "attached_pic": 1 } }
            ],
            "format": { "duration": "180.0" }
        });
        let probe = parse_probe(&song);
        assert!(!probe.has_video, "cover art is not a video stream");
        assert_eq!((probe.width, probe.height), (0, 0));
    }

    #[test]
    fn parse_probe_reads_rotation_from_side_data_or_tag() {
        let matrix = json!({
            "streams": [{ "codec_type": "video", "width": 1920, "height": 1080,
                          "side_data_list": [{ "side_data_type": "Display Matrix", "rotation": -90 }] }],
            "format": { "duration": "47.0" }
        });
        let probe = parse_probe(&matrix);
        assert_eq!(probe.rotation, -90);
        assert_eq!(probe.display_size(), (1080, 1920));

        let tagged = json!({
            "streams": [{ "codec_type": "video", "width": 1920, "height": 1080,
                          "tags": { "rotate": "180" } }],
            "format": {}
        });
        let probe = parse_probe(&tagged);
        assert_eq!(probe.rotation, 180);
        assert_eq!(probe.display_size(), (1920, 1080), "180° keeps orientation");
    }

    #[test]
    fn stderr_tail_keeps_last_lines() {
        let many = (1..=40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = stderr_tail(many.as_bytes());
        assert!(tail.starts_with("line 21"));
        assert!(tail.ends_with("line 40"));
        assert_eq!(stderr_tail(b"   "), "no error output");
    }

    #[tokio::test]
    async fn missing_binary_is_reported_not_panicked() {
        let err = probe_media_with("workforce-no-such-ffprobe", Path::new("x.mp4"))
            .await
            .unwrap_err();
        assert!(err.contains("could not start"), "{err}");
    }
}
