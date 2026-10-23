//! What a video attachment shows: still frames sampled with `ffmpeg`, rendered
//! the way an image attachment is — native image blocks for a model that sees,
//! one description per frame from the `[llm.vision]` model otherwise — each
//! labelled with its timestamp so the answer can point into the video.
//!
//! Sampling is bounded by `[attachments.video]`: frames spread evenly with a
//! minimum spacing and a maximum count, or, when a cheap keyframe-only scan
//! finds scene changes, spread across the shots so each shot is seen. Every
//! frame is one input-seeked decode (cost independent of video length),
//! downscaled without upscaling and JPEG-encoded straight to memory. Phone
//! rotation needs no handling: `ffmpeg` applies the display matrix before the
//! scale. Every failure becomes a sentence the model can repeat — never an
//! error that leaves it guessing.
//!
//! A [`FrameSelection`] narrows the sample for a closer look — a time range,
//! exact timestamps, a frame count, a larger frame — under the same limits.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;
use workforce_common::secret::NoKey;
use workforce_common::settings::VideoFramesConfig;

use super::VisionStrategy;
use crate::ffmpeg::{probe_media_with, run_ffmpeg_with, Probe};
use crate::llm::image_cap::frame_label;
use crate::llm::types::{ContentBlock, ImageSource};

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Kept clear of either end of the video, where the first frame is often
/// black and a seek near the end can land past the last frame.
const EDGE_MARGIN_SECS: f64 = 0.5;
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const SCENE_SCAN_TIMEOUT: Duration = Duration::from_secs(60);
/// Wall time all frames of one video may take together.
const EXTRACTION_BUDGET: Duration = Duration::from_secs(120);
/// The scan only needs to see that the picture changed, not what it shows.
const SCENE_SCAN_WIDTH: u32 = 160;
/// Past this a time is a mistake, not a place in a video.
const LONGEST_SECS: f64 = 100.0 * 3600.0;
/// Frames closer together than a label tells apart are the same frame.
const LABEL_SECS: f64 = 0.1;

/// The binaries to run; a seam so tests can make one go missing.
struct Tools {
    ffmpeg: &'static str,
    ffprobe: &'static str,
}

const SYSTEM_TOOLS: Tools = Tools {
    ffmpeg: "ffmpeg",
    ffprobe: "ffprobe",
};

/// Which frames of a video to show. The default is the whole video, sampled
/// as an inbound attachment is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameSelection {
    /// Seconds into the video the sampled range starts at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<f64>,
    /// Seconds into the video the sampled range ends at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<f64>,
    /// Exact timestamps in seconds, taken instead of a sample.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub at: Vec<f64>,
    /// Frames to sample, in place of what the minimum spacing allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<usize>,
    /// Frames at `detail_long_edge` rather than `max_long_edge`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detail: bool,
}

impl FrameSelection {
    /// The tool parameters a selection is read from.
    pub const PARAMETERS: &'static str = "`from`, `to`, `at`, `frames` and `detail`";

    /// The selection a tool call's `input` asks for; `None` when it asks for
    /// none. An error is a sentence for the model.
    pub fn from_tool_input(input: &Value) -> Result<Option<Self>, String> {
        let given = |key: &str| {
            input
                .get(key)
                .filter(|v| !v.is_null() && v.as_str().is_none_or(|s| !s.trim().is_empty()))
        };
        let time = |key: &str| given(key).map(|v| seconds(key, v)).transpose();
        let selection = Self {
            from: time("from")?,
            to: time("to")?,
            at: match given("at") {
                Some(Value::Array(times)) => times
                    .iter()
                    .map(|v| seconds("at", v))
                    .collect::<Result<_, _>>()?,
                Some(one) => vec![seconds("at", one)?],
                None => Vec::new(),
            },
            frames: given("frames")
                .map(|v| {
                    v.as_u64()
                        .or_else(|| v.as_f64().filter(|n| n.fract() == 0.0).map(|n| n as u64))
                        .or_else(|| v.as_str()?.trim().parse().ok())
                        .filter(|n| *n > 0)
                        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                        .ok_or_else(|| format!("`frames` must be a whole number above 0, not {v}."))
                })
                .transpose()?,
            detail: given("detail")
                .map(|v| {
                    v.as_bool()
                        .or_else(|| v.as_str()?.trim().parse().ok())
                        .ok_or_else(|| format!("`detail` must be true or false, not {v}."))
                })
                .transpose()?
                .unwrap_or(false),
        };
        match (selection.from, selection.to) {
            (Some(from), Some(to)) if from >= to => {
                return Err(format!(
                    "`from` ({}) must be before `to` ({}); for a single moment, pass it as `at`.",
                    format_timestamp(from),
                    format_timestamp(to)
                ))
            }
            (None, Some(to)) if to <= 0.0 => {
                return Err(
                    "`to` must be after the start of the video; for its opening frame, pass \
                     `at` 0."
                        .to_string(),
                )
            }
            _ => {}
        }
        if !selection.at.is_empty()
            && (selection.from.is_some() || selection.to.is_some() || selection.frames.is_some())
        {
            return Err(
                "Pass either `at` for exact timestamps, or `from`, `to` and `frames` for a \
                 sampled range, not both."
                    .to_string(),
            );
        }
        Ok((selection != Self::default()).then_some(selection))
    }

    /// Whether this names a place in the video, which only a known length
    /// can locate.
    fn is_positioned(&self) -> bool {
        self.from.is_some() || self.to.is_some() || !self.at.is_empty()
    }
}

/// A tool parameter as seconds into a video.
fn seconds(key: &str, value: &Value) -> Result<f64, String> {
    let secs = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_timestamp(s),
        _ => None,
    }
    .filter(|secs| secs.is_finite())
    .ok_or_else(|| {
        format!("`{key}` must be a time in seconds (`13.5`) or `m:ss` (`0:13.5`), not {value}.")
    })?;
    match secs {
        secs if secs < 0.0 => Err(format!("`{key}` cannot be negative ({value}).")),
        secs if secs > LONGEST_SECS => Err(format!("`{key}` ({value}) is longer than any video.")),
        secs => Ok(secs),
    }
}

/// Frames from the video at `path` that `selection` picks, rendered for
/// `vision`. Always non-empty: what the model cannot be shown, it is told
/// about.
pub async fn render_frames(
    filename: &str,
    path: &Path,
    cfg: &VideoFramesConfig,
    selection: &FrameSelection,
    vision: &VisionStrategy<'_>,
) -> Vec<ContentBlock> {
    render_with(&SYSTEM_TOOLS, filename, path, cfg, selection, vision).await
}

async fn render_with(
    tools: &Tools,
    filename: &str,
    path: &Path,
    cfg: &VideoFramesConfig,
    selection: &FrameSelection,
    vision: &VisionStrategy<'_>,
) -> Vec<ContentBlock> {
    if let VisionStrategy::StripWithPlaceholder(why) = vision {
        return vec![text(format!(
            "Video `{filename}`: its frames cannot be shown — the active model cannot process \
             images and {why}."
        ))];
    }
    match sample(tools, path, cfg, selection).await {
        Ok(Sampled::Frames(sampling)) => {
            tracing::info!(
                file = %filename,
                frames = sampling.frames.len(),
                duration_secs = sampling.duration,
                "sampled video frames"
            );
            present(filename, sampling, vision).await
        }
        Ok(Sampled::Nothing(why)) => vec![text(format!("Video `{filename}`: {why}"))],
        Ok(Sampled::NoVideo) => vec![text(format!(
            "Video `{filename}` has no video stream (audio only), so there are no frames to show."
        ))],
        Ok(Sampled::TooLong(secs)) => vec![text(format!(
            "Video `{filename}` ({}) is longer than the {}-minute limit for frame extraction, so \
             no frames were taken. Raise [attachments.video].max_minutes to see longer videos.",
            format_timestamp(secs),
            cfg.max_minutes,
        ))],
        Err(e) => {
            tracing::warn!(file = %filename, error = %e, "video frame extraction failed");
            vec![text(format!(
                "Video `{filename}`: visual frame extraction failed, so you cannot see what it \
                 shows: {e}"
            ))]
        }
    }
}

enum Sampled {
    Frames(Sampling),
    /// The selection picks nothing from this video, and why.
    Nothing(String),
    NoVideo,
    TooLong(f64),
}

struct Sampling {
    probe: Probe,
    duration: Option<f64>,
    shots: usize,
    coverage: Coverage,
    notes: Vec<String>,
    frames: Vec<Frame>,
    planned: usize,
    budget_exhausted: bool,
}

struct Frame {
    at: f64,
    jpeg: Vec<u8>,
}

async fn sample(
    tools: &Tools,
    path: &Path,
    cfg: &VideoFramesConfig,
    selection: &FrameSelection,
) -> Result<Sampled, String> {
    let probe = probe_media_with(tools.ffprobe, path).await?;
    if !probe.has_video {
        return Ok(Sampled::NoVideo);
    }
    let duration = probe
        .video_duration
        .filter(|d| *d > 0.0)
        .or((probe.duration > 0.0).then_some(probe.duration));
    if let Some(secs) = duration.filter(|d| *d > cfg.max_minutes as f64 * 60.0) {
        return Ok(Sampled::TooLong(secs));
    }
    let cuts = match duration {
        Some(_) if cfg.scene_threshold > 0.0 && selection.at.is_empty() => {
            scene_changes(tools, path, cfg.scene_threshold)
                .await
                .unwrap_or_else(|e| {
                    tracing::debug!(error = %e, "scene scan failed; sampling evenly");
                    Vec::new()
                })
        }
        _ => Vec::new(),
    };
    let plan = match plan_timestamps(duration, &cuts, cfg, selection) {
        Ok(plan) => plan,
        Err(why) => return Ok(Sampled::Nothing(why)),
    };

    let deadline = Instant::now() + EXTRACTION_BUDGET;
    let mut frames = Vec::with_capacity(plan.timestamps.len());
    let mut last_error = None;
    let mut budget_exhausted = false;
    for &at in &plan.timestamps {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            budget_exhausted = true;
            break;
        }
        let args = frame_args(path, at, plan.long_edge, cfg);
        match run_ffmpeg_with(tools.ffmpeg, &args, left.min(FRAME_TIMEOUT)).await {
            Ok(jpeg) if !jpeg.is_empty() => frames.push(Frame { at, jpeg }),
            Ok(_) => last_error = Some(format!("no frame decoded at {}", format_timestamp(at))),
            Err(e) => last_error = Some(e),
        }
    }
    if frames.is_empty() {
        return Err(last_error.unwrap_or_else(|| "no frame could be decoded".to_string()));
    }
    Ok(Sampled::Frames(Sampling {
        probe,
        duration,
        shots: plan.shots,
        coverage: plan.coverage,
        planned: plan.timestamps.len(),
        notes: plan.notes,
        frames,
        budget_exhausted,
    }))
}

/// Where to take frames, across how many detected shots, how large, and
/// what of the selection had to give.
#[derive(Debug, PartialEq)]
struct Plan {
    timestamps: Vec<f64>,
    shots: usize,
    coverage: Coverage,
    long_edge: u32,
    notes: Vec<String>,
}

/// What of the video a plan's frames stand for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Coverage {
    Whole,
    Range(f64, f64),
    Exact,
}

/// Timestamps to take from a video `duration` seconds long, given the scene
/// changes found in it and the `selection` asked for: strictly increasing,
/// within the video, at most `max_frames`. A sample covers the selected range
/// with as many frames as `min_interval_secs` spacing allows, or as many as
/// the selection asks for; exact timestamps are taken as given, each moved
/// inside the video. `detail` enlarges the frames while they cost no more
/// than a full sample. An unknown duration yields the first frame. An error
/// is why the selection picks nothing.
fn plan_timestamps(
    duration: Option<f64>,
    cuts: &[f64],
    cfg: &VideoFramesConfig,
    selection: &FrameSelection,
) -> Result<Plan, String> {
    let ceiling = cfg.max_frames.max(1);
    let (timestamps, shots, coverage, mut notes) =
        match duration.filter(|d| d.is_finite() && *d > 0.0) {
            None if selection.is_positioned() => {
                return Err(
                    "its length is unreadable, so a time range or timestamp cannot be found in \
                     it; view it without `from`, `to` and `at` to see its opening frame."
                        .to_string(),
                )
            }
            None => (vec![0.0], 1, Coverage::Whole, Vec::new()),
            Some(duration) if selection.at.is_empty() => {
                sampled(duration, cuts, cfg, selection, ceiling)?
            }
            Some(duration) => {
                let (timestamps, notes) = exact(duration, &selection.at, ceiling);
                (timestamps, 1, Coverage::Exact, notes)
            }
        };
    let detail_edge = cfg.detail_long_edge.max(cfg.max_long_edge);
    let enlargeable = detailed_frames(ceiling, cfg.max_long_edge, detail_edge);
    let detailed = selection.detail && timestamps.len() <= enlargeable;
    if selection.detail && !detailed {
        notes.push(format!(
            "`detail` enlarges at most {enlargeable} frame(s) a view, so these are the usual size."
        ));
    }
    Ok(Plan {
        timestamps,
        shots,
        coverage,
        long_edge: match detailed {
            true => detail_edge,
            false => cfg.max_long_edge,
        },
        notes,
    })
}

/// How many frames `detail_edge` wide cost no more pixels than `ceiling`
/// frames `edge` wide, so an enlarged view is no heavier than a full sample.
fn detailed_frames(ceiling: usize, edge: u32, detail_edge: u32) -> usize {
    let ratio = f64::from(edge) / f64::from(detail_edge.max(1));
    ((ceiling as f64 * ratio * ratio) as usize).clamp(1, ceiling)
}

/// Kept clear of either end of a video `duration` seconds long.
fn edge_margin(duration: f64) -> f64 {
    EDGE_MARGIN_SECS.min(duration * 0.1)
}

/// A sample of the selected range: spread across its shots when each can
/// have a frame, evenly otherwise.
fn sampled(
    duration: f64,
    cuts: &[f64],
    cfg: &VideoFramesConfig,
    selection: &FrameSelection,
    ceiling: usize,
) -> Result<(Vec<f64>, usize, Coverage, Vec<String>), String> {
    let from = selection.from.unwrap_or(0.0).max(0.0);
    let to = selection.to.unwrap_or(duration);
    if from >= duration {
        return Err(format!(
            "`from` ({}) is past its end ({} long), so there are no frames to show.",
            format_timestamp(from),
            format_timestamp(duration)
        ));
    }
    if from >= to {
        return Err(format!(
            "the range from {} to {} is empty, so there are no frames to show.",
            format_timestamp(from),
            format_timestamp(to)
        ));
    }
    let end = to.min(duration);
    let mut notes = Vec::new();
    if to > duration {
        notes.push(format!(
            "`to` ({}) is past the end of the video, so the range stops there.",
            format_timestamp(to)
        ));
    }
    let coverage = match selection.from.or(selection.to) {
        Some(_) => Coverage::Range(from, end),
        None => Coverage::Whole,
    };

    // The opening frame is avoided unless asked for; the closing one always.
    let margin = edge_margin(duration);
    let first = selection.from.map_or(margin, |_| from);
    let last = duration - margin;
    let span = match end < duration {
        true => end.min(last) - first,
        false => duration - (first + margin),
    };
    let count = match selection.frames {
        _ if span <= 0.0 => 1,
        Some(asked) => {
            let distinct = (span / LABEL_SECS) as usize + 1;
            if asked > ceiling {
                notes.push(format!(
                    "{asked} frames were asked for; {ceiling} is the most one view shows."
                ));
            } else if asked > distinct {
                notes.push(format!(
                    "{asked} frames were asked for; a range this short holds {distinct}."
                ));
            }
            asked.clamp(1, ceiling.min(distinct))
        }
        None => match cfg.min_interval_secs {
            gap if gap > 0.0 => ((span / gap).floor() as usize + 1).min(ceiling),
            _ => ceiling,
        },
    };
    let bounds = shot_bounds(from, end, cuts, cfg.min_interval_secs.max(EDGE_MARGIN_SECS));
    let shots = bounds.len() - 1;
    if shots > 1 && shots <= count {
        return Ok((across_shots(&bounds, count), shots, coverage, notes));
    }
    let timestamps = match count {
        1 => {
            let at = ((from + end) / 2.0).clamp(first.min(last), last);
            if !(from..=end).contains(&at) {
                notes.push(format!(
                    "The nearest frame that can be shown is at {}.",
                    format_timestamp(at)
                ));
            }
            vec![at]
        }
        n => {
            let step = span / (n - 1) as f64;
            (0..n).map(|i| first + step * i as f64).collect()
        }
    };
    Ok((timestamps, 1, coverage, notes))
}

/// The timestamps asked for, in order and once each, none past the last
/// frame a seek can land on, and at most `ceiling` of them.
fn exact(duration: f64, at: &[f64], ceiling: usize) -> (Vec<f64>, Vec<String>) {
    let tenths = |t: &mut f64| (*t / LABEL_SECS).round() as i64;
    let last = duration - edge_margin(duration);
    let mut asked: Vec<f64> = at.iter().copied().filter(|t| t.is_finite()).collect();
    asked.sort_by(f64::total_cmp);
    let given = asked.len();
    asked.dedup_by_key(tenths);
    let mut notes = Vec::new();
    if asked.len() < given {
        notes.push("A timestamp given more than once is shown once.".to_string());
    }
    if asked.iter().any(|t| *t > last) {
        notes.push(format!(
            "Timestamps past {}, the last frame that can be shown, are shown as that frame.",
            format_timestamp(last)
        ));
    }
    let mut timestamps: Vec<f64> = asked.iter().map(|t| t.clamp(0.0, last)).collect();
    timestamps.dedup_by_key(tenths);
    if timestamps.len() > ceiling {
        notes.push(format!(
            "Only the first {ceiling} of {} timestamps were taken; {ceiling} is the most one \
             view shows.",
            timestamps.len()
        ));
        timestamps.truncate(ceiling);
    }
    (timestamps, notes)
}

/// Shot boundaries `[from, cut…, to]`, ignoring cuts that would make a shot
/// shorter than `min_shot` — flashes and jittery detections.
fn shot_bounds(from: f64, to: f64, cuts: &[f64], min_shot: f64) -> Vec<f64> {
    let mut sorted: Vec<f64> = cuts.iter().copied().filter(|c| c.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    let mut bounds = vec![from];
    for cut in sorted {
        let last = *bounds.last().expect("starts with `from`");
        if cut - last >= min_shot && to - cut >= min_shot {
            bounds.push(cut);
        }
    }
    bounds.push(to);
    bounds
}

/// `count` timestamps spread over the shots between `bounds`: one per shot,
/// the rest handed out one at a time to whichever shot each frame currently
/// covers the most of, each shot's frames centred in equal slices of it.
fn across_shots(bounds: &[f64], count: usize) -> Vec<f64> {
    let lengths: Vec<f64> = bounds.windows(2).map(|w| w[1] - w[0]).collect();
    let mut per_shot = vec![1usize; lengths.len()];
    for _ in lengths.len()..count {
        let widest = (0..lengths.len())
            .max_by(|&a, &b| {
                (lengths[a] / (per_shot[a] + 1) as f64)
                    .total_cmp(&(lengths[b] / (per_shot[b] + 1) as f64))
            })
            .expect("at least one shot");
        per_shot[widest] += 1;
    }
    bounds
        .windows(2)
        .zip(per_shot)
        .flat_map(|(w, n)| {
            let slice = (w[1] - w[0]) / n as f64;
            (0..n).map(move |j| w[0] + slice * (j as f64 + 0.5))
        })
        .collect()
}

/// Scene changes at least `threshold` strong, from keyframes only — a
/// fraction of a full decode, and where encoders put a cut anyway.
async fn scene_changes(tools: &Tools, path: &Path, threshold: f64) -> Result<Vec<f64>, String> {
    let args = [
        "-hide_banner".to_string(),
        "-nostdin".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-skip_frame".to_string(),
        "nokey".to_string(),
        "-i".to_string(),
        path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0:V:0".to_string(),
        "-vf".to_string(),
        format!(
            "scale={SCENE_SCAN_WIDTH}:-2,select='gt(scene,{threshold})',metadata=mode=print:file=-"
        ),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let out = run_ffmpeg_with(tools.ffmpeg, &args, SCENE_SCAN_TIMEOUT).await?;
    Ok(parse_scene_changes(&String::from_utf8_lossy(&out)))
}

/// The `pts_time` of every frame the `metadata=print` filter reported.
fn parse_scene_changes(output: &str) -> Vec<f64> {
    output
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("pts_time:")?.parse().ok())
        .collect()
}

/// One frame at `at` seconds: seek the input (fast, then exact), take the
/// first real video stream (`V` skips cover art), cap the long edge at
/// `edge`, JPEG to stdout.
fn frame_args(path: &Path, at: f64, edge: u32, cfg: &VideoFramesConfig) -> Vec<String> {
    [
        "-hide_banner".to_string(),
        "-nostdin".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-ss".to_string(),
        format!("{at:.3}"),
        "-i".to_string(),
        path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0:V:0".to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-vf".to_string(),
        format!(
            "scale='min(iw,{edge})':'min(ih,{edge})':force_original_aspect_ratio=decrease:force_divisible_by=2"
        ),
        "-q:v".to_string(),
        jpeg_qscale(cfg.jpeg_quality).to_string(),
        "-f".to_string(),
        "image2pipe".to_string(),
        "-c:v".to_string(),
        "mjpeg".to_string(),
        "-".to_string(),
    ]
    .into()
}

/// ffmpeg's MJPEG scale runs 2 (best) to 31 (smallest); quality runs 1 to 100.
fn jpeg_qscale(quality: u8) -> u8 {
    let quality = f64::from(quality.clamp(1, 100));
    31 - ((quality - 1.0) * 29.0 / 99.0).round() as u8
}

async fn present(
    filename: &str,
    sampling: Sampling,
    vision: &VisionStrategy<'_>,
) -> Vec<ContentBlock> {
    let total = sampling.frames.len();
    let mut blocks = vec![text(preface(filename, &sampling))];
    for (i, frame) in sampling.frames.into_iter().enumerate() {
        let label = frame_label(i + 1, total, filename, &format_timestamp(frame.at));
        let data = BASE64.encode(&frame.jpeg);
        match vision {
            VisionStrategy::DescribeWith(llm, _) => {
                let described = llm.describe_image("image/jpeg", &data, Some(&label)).await;
                blocks.push(text(match described {
                    Ok(desc) => format!("{label} (described by {}):\n{desc}", llm.model_name()),
                    Err(e) if e.is::<NoKey>() => {
                        blocks.push(text(format!("(The frames are not described: {e})")));
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(file = %filename, frame = i + 1, error = %e, "vision describe failed for video frame");
                        format!("{label}: (description unavailable: {e})")
                    }
                }));
            }
            VisionStrategy::StripWithPlaceholder(_) => blocks.push(text(format!(
                "{label}: (not shown — the active model cannot process images)"
            ))),
            VisionStrategy::Native | VisionStrategy::NativeImages => {
                blocks.push(text(format!("{label}:")));
                blocks.push(ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: "image/jpeg".to_string(),
                        data,
                    },
                });
            }
        }
    }
    blocks
}

fn preface(filename: &str, s: &Sampling) -> String {
    let (w, h) = s.probe.display_size();
    let shape = match (w, h) {
        (0, _) | (_, 0) => String::new(),
        (w, h) if h > w => format!(", {w}×{h} portrait"),
        (w, h) => format!(", {w}×{h}"),
    };
    let length = s
        .duration
        .map(|d| format!("{} long", format_timestamp(d)))
        .unwrap_or_else(|| "length unreadable, so only its opening frame".to_string());
    let spread = match s.shots {
        1 => String::new(),
        n => format!(" across {n} detected shots"),
    };
    let picked = match s.coverage {
        Coverage::Whole => format!("sampled{spread}"),
        Coverage::Range(from, to) => format!(
            "sampled from {} to {}{spread}",
            format_timestamp(from),
            format_timestamp(to)
        ),
        Coverage::Exact => "at the timestamps asked for".to_string(),
    };
    let notes: String = s.notes.iter().map(|note| format!(" {note}")).collect();
    let shortfall = match (s.frames.len() < s.planned, s.budget_exhausted) {
        (true, true) => format!(
            " Only {} of {} planned frames were taken before the time limit.",
            s.frames.len(),
            s.planned
        ),
        (true, false) => format!(
            " {} of {} planned frames could not be decoded.",
            s.planned - s.frames.len(),
            s.planned
        ),
        _ => String::new(),
    };
    format!(
        "Video `{filename}` ({length}{shape}): {n} still frame(s) {picked}, in order, each \
         labelled with its timestamp — refer to them by timestamp when describing what the video \
         shows.{notes}{shortfall}",
        n = s.frames.len(),
    )
}

/// `m:ss.s` under an hour, `h:mm:ss.s` above — seekable in any player.
fn format_timestamp(secs: f64) -> String {
    let tenths = (secs.max(0.0) * 10.0).round() as u64;
    let (whole, tenth) = (tenths / 10, tenths % 10);
    let (h, m, s) = (whole / 3600, (whole % 3600) / 60, whole % 60);
    match h {
        0 => format!("{m}:{s:02}.{tenth}"),
        h => format!("{h}:{m:02}:{s:02}.{tenth}"),
    }
}

/// What [`format_timestamp`] writes, read back: seconds, `m:ss` or
/// `h:mm:ss`, the seconds with or without a fraction.
fn parse_timestamp(text: &str) -> Option<f64> {
    let text = text.trim();
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = text.rsplit(':');
    let secs = parts.next()?;
    let (whole, fraction) = secs.split_once('.').unwrap_or((secs, "0"));
    let secs: f64 = (digits(whole) && digits(fraction))
        .then(|| secs.parse().ok())
        .flatten()?;
    let larger = parts
        .map(|part| digits(part).then(|| part.parse::<f64>().ok()).flatten())
        .collect::<Option<Vec<_>>>()?;
    let total = match larger[..] {
        [] => secs,
        [m] if secs < 60.0 => m * 60.0 + secs,
        [m, h] if secs < 60.0 && m < 60.0 => h * 3600.0 + m * 60.0 + secs,
        _ => return None,
    };
    Some(sign * total)
}

fn text(text: String) -> ContentBlock {
    ContentBlock::Text { text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::VisionLlm;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn cfg() -> VideoFramesConfig {
        VideoFramesConfig::default()
    }

    fn whole(duration: Option<f64>, cuts: &[f64], cfg: &VideoFramesConfig) -> Plan {
        plan_timestamps(duration, cuts, cfg, &FrameSelection::default()).expect("a plan")
    }

    async fn show(
        filename: &str,
        path: &Path,
        cfg: &VideoFramesConfig,
        vision: &VisionStrategy<'_>,
    ) -> Vec<ContentBlock> {
        render_frames(filename, path, cfg, &FrameSelection::default(), vision).await
    }

    fn assert_well_formed(plan: &[f64], duration: f64) {
        assert!(plan.windows(2).all(|w| w[0] < w[1]), "ordered: {plan:?}");
        assert!(
            plan.iter().all(|t| *t >= 0.0 && *t < duration),
            "inside: {plan:?}"
        );
    }

    #[test]
    fn plan_spreads_a_minute_evenly_from_near_start_to_near_end() {
        let plan = whole(Some(47.0), &[], &cfg());
        assert_eq!(plan.shots, 1);
        assert_eq!(plan.timestamps.len(), 8, "max_frames binds");
        assert_eq!(plan.timestamps[0], 0.5);
        assert!((plan.timestamps[7] - 46.5).abs() < 1e-9);
        assert_well_formed(&plan.timestamps, 47.0);
    }

    #[test]
    fn plan_keeps_short_clips_to_distinct_frames() {
        let three = whole(Some(3.0), &[], &cfg()).timestamps;
        assert_eq!(three.len(), 2, "2s spacing leaves room for two");
        assert!((three[0] - 0.3).abs() < 1e-9 && (three[1] - 2.7).abs() < 1e-9);
        assert_eq!(whole(Some(1.0), &[], &cfg()).timestamps, vec![0.5]);
        assert_eq!(whole(Some(0.3), &[], &cfg()).timestamps, vec![0.15]);
    }

    #[test]
    fn plan_caps_long_videos_and_honours_a_zero_spacing() {
        let two_hours = whole(Some(7200.0), &[], &cfg()).timestamps;
        assert_eq!(two_hours.len(), 8);
        assert_well_formed(&two_hours, 7200.0);

        let dense = VideoFramesConfig {
            min_interval_secs: 0.0,
            max_frames: 5,
            ..cfg()
        };
        assert_eq!(whole(Some(2.0), &[], &dense).timestamps.len(), 5);
    }

    #[test]
    fn plan_without_a_duration_takes_the_opening_frame() {
        for unknown in [None, Some(0.0), Some(f64::NAN), Some(-4.0)] {
            assert_eq!(
                whole(unknown, &[], &cfg()).timestamps,
                vec![0.0],
                "{unknown:?}"
            );
        }
    }

    #[test]
    fn plan_gives_every_shot_a_frame_and_spare_frames_to_the_longest() {
        let plan = whole(Some(40.0), &[10.0, 12.5], &cfg());
        assert_eq!(plan.shots, 3);
        assert_eq!(plan.timestamps.len(), 8);
        assert_well_formed(&plan.timestamps, 40.0);
        let in_shot = |lo: f64, hi: f64| {
            plan.timestamps
                .iter()
                .filter(|t| **t >= lo && **t < hi)
                .count()
        };
        assert_eq!(in_shot(10.0, 12.5), 1, "the short shot still gets one");
        assert_eq!(in_shot(12.5, 40.0), 5, "the long shot gets most");
        assert_eq!(in_shot(0.0, 10.0), 2);
    }

    #[test]
    fn plan_ignores_flash_cuts_and_falls_back_when_shots_outnumber_frames() {
        let flashes = whole(Some(30.0), &[0.2, 15.0, 15.4, 29.9], &cfg());
        assert_eq!(flashes.shots, 2, "only the cut at 15s makes a real shot");

        let montage: Vec<f64> = (1..20).map(|i| i as f64 * 3.0).collect();
        let plan = whole(Some(60.0), &montage, &cfg());
        assert_eq!(plan.shots, 1, "19 cuts for 8 frames: even spread instead");
        assert_eq!(plan.timestamps.len(), 8);
    }

    fn select(input: serde_json::Value) -> FrameSelection {
        FrameSelection::from_tool_input(&input)
            .expect("valid")
            .expect("a selection")
    }

    fn rejected(input: serde_json::Value) -> String {
        FrameSelection::from_tool_input(&input).expect_err("rejected")
    }

    fn close(actual: &[f64], expected: &[f64]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?}");
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(a, e)| (a - e).abs() < 1e-9),
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn timestamps_parse_as_seconds_or_clock_time() {
        for (text, secs) in [
            ("13", 13.0),
            (" 13.5 ", 13.5),
            ("0:13", 13.0),
            ("1:02.5", 62.5),
            ("90:00", 5400.0),
            ("1:02:05.0", 3725.0),
            ("-0:05", -5.0),
        ] {
            assert_eq!(parse_timestamp(text), Some(secs), "{text}");
        }
        for junk in [
            "", ":", "1:75", "1:60:00", "1:2:3:4", "0:1e1", "nan", "inf", "13.", ".5", "+5",
            "a:10", "0:-5", "ten",
        ] {
            assert_eq!(parse_timestamp(junk), None, "{junk}");
        }
    }

    #[test]
    fn a_formatted_timestamp_parses_back() {
        for secs in [0.0, 0.5, 14.5, 59.9, 60.0, 613.2, 3725.0, 36_000.4] {
            let parsed = parse_timestamp(&format_timestamp(secs)).expect("parses");
            assert!((parsed - secs).abs() < 1e-9, "{secs} came back as {parsed}");
        }
    }

    #[test]
    fn a_selection_reads_numbers_and_clock_times_from_tool_input() {
        assert_eq!(
            select(serde_json::json!({ "from": "0:10", "to": 16, "frames": 6 })),
            FrameSelection {
                from: Some(10.0),
                to: Some(16.0),
                frames: Some(6),
                ..Default::default()
            }
        );
        assert_eq!(
            select(serde_json::json!({ "at": ["0:13", 2.5], "detail": true })),
            FrameSelection {
                at: vec![13.0, 2.5],
                detail: true,
                ..Default::default()
            }
        );
        assert_eq!(select(serde_json::json!({ "at": "1:02" })).at, vec![62.0]);
        assert_eq!(select(serde_json::json!({ "frames": "4" })).frames, Some(4));
        assert_eq!(select(serde_json::json!({ "frames": 4.0 })).frames, Some(4));
        assert!(select(serde_json::json!({ "detail": "true" })).detail);
        assert_eq!(
            select(serde_json::json!({ "from": " 1:30 ", "to": "0:01:30.5" })),
            FrameSelection {
                from: Some(90.0),
                to: Some(90.5),
                ..Default::default()
            }
        );
    }

    #[test]
    fn input_without_a_selection_is_none() {
        for input in [
            serde_json::json!({}),
            serde_json::json!({ "reference": "room.mp4" }),
            serde_json::json!({ "from": null, "to": "", "at": [], "detail": false }),
            serde_json::json!({ "from": "  ", "frames": " ", "detail": "false" }),
        ] {
            assert_eq!(FrameSelection::from_tool_input(&input), Ok(None), "{input}");
        }
    }

    #[test]
    fn a_selection_that_cannot_be_read_is_refused_in_one_sentence() {
        let negative = rejected(serde_json::json!({ "from": -3 }));
        assert_eq!(negative, "`from` cannot be negative (-3).");
        assert!(rejected(serde_json::json!({ "at": ["0:05", "-0:02"] })).contains("negative"));

        let junk = rejected(serde_json::json!({ "to": "soon" }));
        assert!(junk.starts_with("`to` must be a time in seconds"), "{junk}");
        assert!(rejected(serde_json::json!({ "at": [true] })).starts_with("`at` must be a time"));

        assert!(rejected(serde_json::json!({ "from": "1:75" })).starts_with("`from` must be a"));
        for huge in [serde_json::json!(1e300), serde_json::json!("999999999999")] {
            let why = rejected(serde_json::json!({ "at": [huge] }));
            assert!(why.ends_with("is longer than any video."), "{why}");
        }

        assert_eq!(
            rejected(serde_json::json!({ "from": "0:16", "to": "0:10" })),
            "`from` (0:16.0) must be before `to` (0:10.0); for a single moment, pass it as `at`."
        );
        assert!(rejected(serde_json::json!({ "from": 5, "to": 5 })).contains("must be before"));
        assert_eq!(
            rejected(serde_json::json!({ "to": 0 })),
            "`to` must be after the start of the video; for its opening frame, pass `at` 0."
        );
        assert_eq!(
            rejected(serde_json::json!({ "detail": "large" })),
            "`detail` must be true or false, not \"large\"."
        );

        for mixed in [
            serde_json::json!({ "at": [3], "from": 1 }),
            serde_json::json!({ "at": [3], "to": 9 }),
            serde_json::json!({ "at": [3], "frames": 2 }),
        ] {
            assert!(rejected(mixed).starts_with("Pass either `at`"));
        }
        for frames in [
            serde_json::json!(0),
            serde_json::json!(-2),
            serde_json::json!(1.5),
        ] {
            let why = rejected(serde_json::json!({ "frames": frames }));
            assert!(why.starts_with("`frames` must be a whole number"), "{why}");
        }
    }

    #[test]
    fn a_selection_survives_the_marker_round_trip() {
        let selection = select(serde_json::json!({ "at": [13, 2.5], "detail": true }));
        let json = serde_json::to_value(&selection).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "at": [13.0, 2.5], "detail": true })
        );
        assert_eq!(
            serde_json::from_value::<FrameSelection>(json).unwrap(),
            selection
        );
    }

    #[test]
    fn plan_samples_a_range_from_end_to_end() {
        let range = select(serde_json::json!({ "from": "0:10", "to": "0:16" }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &range).unwrap();
        close(&plan.timestamps, &[10.0, 12.0, 14.0, 16.0]);
        assert_eq!(plan.coverage, Coverage::Range(10.0, 16.0));
        assert_eq!(plan.long_edge, cfg().max_long_edge);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let six = FrameSelection {
            frames: Some(6),
            ..range
        };
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &six).unwrap();
        close(&plan.timestamps, &[10.0, 11.2, 12.4, 13.6, 14.8, 16.0]);
    }

    #[test]
    fn plan_keeps_an_open_ended_range_clear_of_the_video_ends() {
        let tail = select(serde_json::json!({ "from": 40 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &tail).unwrap();
        close(
            &plan.timestamps,
            &[40.0, 40.0 + 6.5 / 3.0, 46.5 - 6.5 / 3.0, 46.5],
        );
        assert_eq!(plan.coverage, Coverage::Range(40.0, 47.0));

        let head = select(serde_json::json!({ "to": 4 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &head).unwrap();
        close(&plan.timestamps, &[0.5, 4.0]);
        assert_eq!(plan.coverage, Coverage::Range(0.0, 4.0));
    }

    #[test]
    fn plan_cuts_a_range_short_at_the_end_of_the_video_and_says_so() {
        let over = select(serde_json::json!({ "from": 44, "to": 90 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &over).unwrap();
        close(&plan.timestamps, &[44.0, 46.5]);
        assert_eq!(plan.coverage, Coverage::Range(44.0, 47.0));
        assert_eq!(
            plan.notes,
            ["`to` (1:30.0) is past the end of the video, so the range stops there."]
        );
        assert_well_formed(&plan.timestamps, 47.0);
    }

    #[test]
    fn plan_refuses_a_range_with_nothing_in_it() {
        let past = select(serde_json::json!({ "from": "1:00" }));
        assert_eq!(
            plan_timestamps(Some(47.0), &[], &cfg(), &past).unwrap_err(),
            "`from` (1:00.0) is past its end (0:47.0 long), so there are no frames to show."
        );
        let at_end = select(serde_json::json!({ "from": 47 }));
        assert!(plan_timestamps(Some(47.0), &[], &cfg(), &at_end).is_err());

        let backwards = FrameSelection {
            from: Some(16.0),
            to: Some(10.0),
            ..Default::default()
        };
        assert_eq!(
            plan_timestamps(Some(47.0), &[], &cfg(), &backwards).unwrap_err(),
            "the range from 0:16.0 to 0:10.0 is empty, so there are no frames to show."
        );
    }

    #[test]
    fn plan_takes_a_range_at_the_start_from_its_first_frame() {
        let opening = select(serde_json::json!({ "from": 0, "to": 0.3 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &opening).unwrap();
        close(&plan.timestamps, &[0.15]);
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let spread = select(serde_json::json!({ "from": 0, "to": 4 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &spread).unwrap();
        close(&plan.timestamps, &[0.0, 2.0, 4.0]);
    }

    #[test]
    fn plan_takes_no_more_frames_than_a_short_range_holds() {
        let crowded = select(serde_json::json!({ "from": 13, "to": 13.25, "frames": 8 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &crowded).unwrap();
        close(&plan.timestamps, &[13.0, 13.125, 13.25]);
        assert_eq!(
            plan.notes,
            ["8 frames were asked for; a range this short holds 3."]
        );
    }

    #[test]
    fn plan_is_total_over_selections_no_tool_call_can_make() {
        let none = FrameSelection {
            frames: Some(0),
            ..Default::default()
        };
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &none).unwrap();
        close(&plan.timestamps, &[23.5]);

        let unreal = FrameSelection {
            at: vec![f64::NAN, f64::INFINITY, 13.0],
            ..Default::default()
        };
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &unreal).unwrap();
        assert_eq!(plan.timestamps, vec![13.0]);

        let endless = FrameSelection {
            from: Some(-3.0),
            to: Some(f64::MAX),
            ..Default::default()
        };
        let plan = plan_timestamps(Some(4.0), &[], &cfg(), &endless).unwrap();
        close(&plan.timestamps, &[0.0, 3.6]);
        assert_eq!(plan.coverage, Coverage::Range(0.0, 4.0));
    }

    #[test]
    fn plan_places_a_closer_look_in_very_short_and_very_long_videos() {
        let blink = select(serde_json::json!({ "from": 0.1, "to": 0.2, "frames": 3 }));
        let plan = plan_timestamps(Some(0.3), &[], &cfg(), &blink).unwrap();
        close(&plan.timestamps, &[0.1, 0.2]);
        assert_well_formed(&plan.timestamps, 0.3);

        let at_end = select(serde_json::json!({ "at": [0.29, 5] }));
        let plan = plan_timestamps(Some(0.3), &[], &cfg(), &at_end).unwrap();
        close(&plan.timestamps, &[0.27]);

        let long = cfg().max_minutes as f64 * 60.0;
        let closing = select(serde_json::json!({ "from": long - 6.0 }));
        let plan = plan_timestamps(Some(long), &[], &cfg(), &closing).unwrap();
        close(
            &plan.timestamps,
            &[long - 6.0, long - 6.0 + 5.5 / 2.0, long - 0.5],
        );
        let moments = select(serde_json::json!({ "at": ["9:59.4", "9:59.9"] }));
        let plan = plan_timestamps(Some(long), &[], &cfg(), &moments).unwrap();
        close(&plan.timestamps, &[long - 0.6, long - 0.5]);
    }

    #[test]
    fn plan_is_the_same_every_time() {
        let selection = select(serde_json::json!({ "at": [9, "0:03", 9.04, 80, 3] }));
        let plans: Vec<Plan> = (0..3)
            .map(|_| plan_timestamps(Some(47.0), &[30.0, 5.0], &cfg(), &selection).unwrap())
            .collect();
        assert!(plans.windows(2).all(|w| w[0] == w[1]));
    }

    /// The planner as it was before a selection could narrow it.
    fn plan_before_selections(
        duration: Option<f64>,
        cuts: &[f64],
        cfg: &VideoFramesConfig,
    ) -> (Vec<f64>, usize) {
        let Some(duration) = duration.filter(|d| d.is_finite() && *d > 0.0) else {
            return (vec![0.0], 1);
        };
        let margin = EDGE_MARGIN_SECS.min(duration * 0.1);
        let span = duration - 2.0 * margin;
        let by_spacing = match cfg.min_interval_secs {
            gap if gap > 0.0 => (span / gap).floor() as usize + 1,
            _ => usize::MAX,
        };
        let count = by_spacing.clamp(1, cfg.max_frames.max(1));
        let min_shot = cfg.min_interval_secs.max(EDGE_MARGIN_SECS);
        let mut sorted: Vec<f64> = cuts.iter().copied().filter(|c| c.is_finite()).collect();
        sorted.sort_by(f64::total_cmp);
        let mut bounds = vec![0.0];
        for cut in sorted {
            let last = *bounds.last().unwrap();
            if cut - last >= min_shot && duration - cut >= min_shot {
                bounds.push(cut);
            }
        }
        bounds.push(duration);
        let shots = bounds.len() - 1;
        if shots > 1 && shots <= count {
            return (across_shots(&bounds, count), shots);
        }
        let timestamps = match count {
            1 => vec![duration / 2.0],
            n => {
                let step = span / (n - 1) as f64;
                (0..n).map(|i| margin + step * i as f64).collect()
            }
        };
        (timestamps, 1)
    }

    #[test]
    fn a_video_viewed_whole_is_planned_exactly_as_before_selections() {
        let durations = (1..=12_000)
            .map(|i| f64::from(i) * 0.05)
            .chain([0.013, 1.0 / 3.0, 2.5, 4.999, 47.0, 613.2, 7200.0])
            .map(Some)
            .chain([None, Some(0.0), Some(-4.0), Some(f64::NAN)]);
        let cut_sets: [&[f64]; 4] = [
            &[],
            &[10.0, 12.5],
            &[0.2, 15.0, 15.4, 29.9, 300.0],
            &[3.0, 6.0, 9.0, 12.0, 15.0, 18.0, 21.0, 24.0, 27.0, 30.0],
        ];
        for duration in durations {
            for cuts in cut_sets {
                for (min_interval_secs, max_frames) in
                    [(2.0, 8), (0.0, 5), (7.0, 20), (2.0, 1), (0.3, 20)]
                {
                    let config = VideoFramesConfig {
                        min_interval_secs,
                        max_frames,
                        ..cfg()
                    };
                    let plan = whole(duration, cuts, &config);
                    let (timestamps, shots) = plan_before_selections(duration, cuts, &config);
                    assert_eq!(
                        plan.timestamps
                            .iter()
                            .map(|t| t.to_bits())
                            .collect::<Vec<_>>(),
                        timestamps.iter().map(|t| t.to_bits()).collect::<Vec<_>>(),
                        "{duration:?} {cuts:?} {min_interval_secs} {max_frames}"
                    );
                    assert_eq!(
                        (plan.shots, plan.coverage, plan.long_edge, &plan.notes),
                        (shots, Coverage::Whole, config.max_long_edge, &Vec::new()),
                        "{duration:?} {cuts:?} {min_interval_secs} {max_frames}"
                    );
                }
            }
        }
    }

    #[test]
    fn plan_takes_one_frame_from_a_range_too_short_to_spread() {
        let sliver = select(serde_json::json!({ "from": 13, "to": 13.4 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &sliver).unwrap();
        close(&plan.timestamps, &[13.2]);

        let last_moment = select(serde_json::json!({ "from": 46.8, "frames": 5 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &last_moment).unwrap();
        close(&plan.timestamps, &[46.5]);
        assert_eq!(
            plan.notes,
            ["The nearest frame that can be shown is at 0:46.5."]
        );
    }

    #[test]
    fn plan_honours_a_frame_count_within_the_configured_maximum() {
        let fewer = select(serde_json::json!({ "frames": 3 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &fewer).unwrap();
        close(&plan.timestamps, &[0.5, 23.5, 46.5]);
        assert_eq!(plan.coverage, Coverage::Whole);
        assert!(plan.notes.is_empty());

        let dense = select(serde_json::json!({ "frames": 5 }));
        let plan = plan_timestamps(Some(3.0), &[], &cfg(), &dense).unwrap();
        assert_eq!(plan.timestamps.len(), 5, "a count overrides the spacing");
        assert_well_formed(&plan.timestamps, 3.0);

        let greedy = select(serde_json::json!({ "frames": 50 }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &greedy).unwrap();
        assert_eq!(plan.timestamps.len(), 8);
        assert_eq!(
            plan.notes,
            ["50 frames were asked for; 8 is the most one view shows."]
        );
    }

    #[test]
    fn plan_spreads_a_range_across_the_shots_inside_it() {
        let range = select(serde_json::json!({ "from": 8, "to": 20, "frames": 4 }));
        let plan = plan_timestamps(Some(40.0), &[4.0, 12.0, 30.0], &cfg(), &range).unwrap();
        assert_eq!(plan.shots, 2, "only the cut at 12s is inside the range");
        close(
            &plan.timestamps,
            &[10.0, 12.0 + 8.0 / 6.0, 16.0, 20.0 - 8.0 / 6.0],
        );
    }

    #[test]
    fn plan_takes_exact_timestamps_in_order_once_each() {
        let exact = select(serde_json::json!({ "at": ["0:13", 2, 13.04, "0:02"] }));
        let plan = plan_timestamps(Some(47.0), &[5.0, 20.0], &cfg(), &exact).unwrap();
        assert_eq!(plan.timestamps, vec![2.0, 13.0]);
        assert_eq!(plan.coverage, Coverage::Exact);
        assert_eq!(plan.shots, 1);
        assert_eq!(
            plan.notes,
            ["A timestamp given more than once is shown once."]
        );

        let opening = select(serde_json::json!({ "at": [0] }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &opening).unwrap();
        assert_eq!(plan.timestamps, vec![0.0], "the first frame when asked for");
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn plan_moves_timestamps_past_the_end_to_the_last_frame() {
        let late = select(serde_json::json!({ "at": [90, 13, "0:46.9"] }));
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &late).unwrap();
        assert_eq!(plan.timestamps, vec![13.0, 46.5]);
        assert_eq!(
            plan.notes,
            ["Timestamps past 0:46.5, the last frame that can be shown, are shown as that frame."]
        );
        assert_well_formed(&plan.timestamps, 47.0);

        let negative = FrameSelection {
            at: vec![-4.0],
            ..Default::default()
        };
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &negative).unwrap();
        assert_eq!(plan.timestamps, vec![0.0]);
    }

    #[test]
    fn plan_caps_exact_timestamps_at_the_configured_maximum() {
        let many = FrameSelection {
            at: (1..=12).map(f64::from).collect(),
            ..Default::default()
        };
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &many).unwrap();
        assert_eq!(plan.timestamps, (1..=8).map(f64::from).collect::<Vec<_>>());
        assert_eq!(
            plan.notes,
            ["Only the first 8 of 12 timestamps were taken; 8 is the most one view shows."]
        );

        let late = FrameSelection {
            at: (0..500).map(|i| 100.0 + f64::from(i)).collect(),
            ..Default::default()
        };
        let plan = plan_timestamps(Some(47.0), &[], &cfg(), &late).unwrap();
        assert_eq!(plan.timestamps, vec![46.5]);
        assert_eq!(
            plan.notes,
            ["Timestamps past 0:46.5, the last frame that can be shown, are shown as that frame."],
            "however many are late, the note is one sentence"
        );
    }

    #[test]
    fn plan_enlarges_only_as_many_frames_as_cost_no_more_than_a_full_sample() {
        assert_eq!(detailed_frames(8, 1024, 1568), 3);
        assert_eq!(detailed_frames(20, 1024, 1568), 8);
        assert_eq!(detailed_frames(8, 1024, 1024), 8);
        assert_eq!(detailed_frames(1, 128, 1568), 1);

        let detail = |input: serde_json::Value| {
            plan_timestamps(Some(47.0), &[], &cfg(), &select(input)).unwrap()
        };
        let exact = detail(serde_json::json!({ "at": [3, 9, 20], "detail": true }));
        assert_eq!(exact.long_edge, cfg().detail_long_edge);
        assert!(exact.notes.is_empty());
        let single = detail(serde_json::json!({ "frames": 1, "detail": true }));
        assert_eq!((single.timestamps.len(), single.long_edge), (1, 1568));
        let few = detail(serde_json::json!({ "from": 10, "to": 14, "detail": true }));
        assert_eq!((few.timestamps.len(), few.long_edge), (3, 1568));

        for many in [
            serde_json::json!({ "at": [3, 9, 20, 30], "detail": true }),
            serde_json::json!({ "from": 10, "to": 16, "detail": true }),
            serde_json::json!({ "detail": true }),
        ] {
            let plan = detail(many);
            assert_eq!(plan.long_edge, 1024, "{:?}", plan.timestamps);
            assert_eq!(
                plan.notes,
                ["`detail` enlarges at most 3 frame(s) a view, so these are the usual size."]
            );
        }

        let smaller = VideoFramesConfig {
            detail_long_edge: 200,
            ..cfg()
        };
        let exact = select(serde_json::json!({ "at": [3], "detail": true }));
        assert_eq!(
            plan_timestamps(Some(47.0), &[], &smaller, &exact)
                .unwrap()
                .long_edge,
            1024,
            "never smaller than a sampled frame"
        );
    }

    #[test]
    fn plan_cannot_place_a_selection_in_a_video_of_unknown_length() {
        for placed in [
            serde_json::json!({ "from": 3 }),
            serde_json::json!({ "to": 3 }),
            serde_json::json!({ "at": [3] }),
        ] {
            let why = plan_timestamps(None, &[], &cfg(), &select(placed)).unwrap_err();
            assert!(why.starts_with("its length is unreadable"), "{why}");
        }
        let counted = select(serde_json::json!({ "frames": 4, "detail": true }));
        let plan = plan_timestamps(None, &[], &cfg(), &counted).unwrap();
        assert_eq!(plan.timestamps, vec![0.0]);
        assert_eq!(plan.long_edge, cfg().detail_long_edge);
    }

    fn sampling(coverage: Coverage, frames: usize, notes: &[&str]) -> Sampling {
        Sampling {
            probe: crate::ffmpeg::parse_probe(&serde_json::json!({})),
            duration: Some(47.0),
            shots: 1,
            coverage,
            notes: notes.iter().map(|n| n.to_string()).collect(),
            frames: (0..frames)
                .map(|i| Frame {
                    at: i as f64,
                    jpeg: Vec::new(),
                })
                .collect(),
            planned: frames,
            budget_exhausted: false,
        }
    }

    #[test]
    fn the_intro_says_what_was_sampled() {
        assert_eq!(
            preface("room.mp4", &sampling(Coverage::Whole, 8, &[])),
            "Video `room.mp4` (0:47.0 long): 8 still frame(s) sampled, in order, each labelled \
             with its timestamp — refer to them by timestamp when describing what the video shows."
        );
        let range = preface("room.mp4", &sampling(Coverage::Range(10.0, 16.0), 6, &[]));
        assert!(
            range.contains("): 6 still frame(s) sampled from 0:10.0 to 0:16.0, in order, each"),
            "{range}"
        );
        let shots = Sampling {
            shots: 2,
            ..sampling(Coverage::Range(10.0, 16.0), 6, &[])
        };
        assert!(preface("room.mp4", &shots)
            .contains("sampled from 0:10.0 to 0:16.0 across 2 detected shots, in order"));
        let exact = preface(
            "room.mp4",
            &sampling(
                Coverage::Exact,
                1,
                &["A timestamp given more than once is shown once."],
            ),
        );
        assert!(
            exact.contains("): 1 still frame(s) at the timestamps asked for, in order, each"),
            "{exact}"
        );
        assert!(
            exact.ends_with("the video shows. A timestamp given more than once is shown once."),
            "{exact}"
        );
    }

    /// The images left in each message, by message.
    fn images_per_message(messages: &[crate::llm::types::LlmMessage]) -> Vec<usize> {
        messages.iter().map(|m| images(&m.content).len()).collect()
    }

    #[tokio::test]
    async fn repeated_closer_looks_stay_within_the_image_caps_newest_first() {
        use crate::llm::image_cap::{anthropic_shaped, cap_images, RESPONSES_MAX_IMAGES};
        use crate::llm::types::{LlmMessage, Role};
        use std::borrow::Cow;

        let mut messages = Vec::new();
        for coverage in std::iter::once(Coverage::Whole)
            .chain(std::iter::repeat_n(Coverage::Range(10.0, 16.0), 8))
        {
            messages.push(LlmMessage {
                role: Role::User,
                content: present(
                    "room.mp4",
                    sampling(coverage, 8, &[]),
                    &VisionStrategy::Native,
                )
                .await,
            });
        }
        assert_eq!(images_per_message(&messages), [8; 9]);

        assert_eq!(
            images_per_message(&anthropic_shaped(&messages)),
            [0, 0, 0, 0, 0, 0, 4, 8, 8],
            "the newest looks survive whole"
        );
        assert_eq!(
            images_per_message(&cap_images(Cow::Borrowed(&messages), RESPONSES_MAX_IMAGES)),
            [0, 0, 2, 8, 8, 8, 8, 8, 8]
        );
    }

    #[test]
    fn scene_changes_parse_from_metadata_output() {
        let out = "frame:0    pts:61440   pts_time:4\nlavfi.scene_score=0.400000\n\
                   frame:1    pts:122880  pts_time:8.25\nlavfi.scene_score=0.907910\n";
        assert_eq!(parse_scene_changes(out), vec![4.0, 8.25]);
        assert!(parse_scene_changes("").is_empty());
    }

    #[test]
    fn jpeg_quality_maps_onto_the_mjpeg_scale() {
        assert_eq!(jpeg_qscale(100), 2);
        assert_eq!(jpeg_qscale(1), 31);
        assert_eq!(jpeg_qscale(0), 31);
        assert_eq!(jpeg_qscale(80), 8);
    }

    #[test]
    fn timestamps_format_like_a_player() {
        assert_eq!(format_timestamp(0.0), "0:00.0");
        assert_eq!(format_timestamp(14.46), "0:14.5");
        assert_eq!(format_timestamp(59.96), "1:00.0");
        assert_eq!(format_timestamp(3725.0), "1:02:05.0");
    }

    fn ffmpeg_available() -> bool {
        crate::ffmpeg::ffmpeg_available()
    }

    fn encode(dir: &Path, name: &str, args: &[&str]) -> std::path::PathBuf {
        let out = dir.join(name);
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .arg(&out)
            .stderr(Stdio::piped())
            .output()
            .expect("spawn ffmpeg");
        assert!(
            status.status.success(),
            "fixture encode failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        out
    }

    /// Colour bars of `secs` seconds at `size`, with no audio track.
    fn clip(dir: &Path, secs: u32, size: &str) -> std::path::PathBuf {
        encode(
            dir,
            "clip.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={secs}:size={size}:rate=10"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
        )
    }

    fn images(blocks: &[ContentBlock]) -> Vec<Vec<u8>> {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { source } => {
                    assert_eq!(source.media_type, "image/jpeg");
                    Some(BASE64.decode(&source.data).unwrap())
                }
                _ => None,
            })
            .collect()
    }

    fn texts(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn jpeg_size(dir: &Path, jpeg: &[u8]) -> (u32, u32) {
        let path = dir.join("frame.jpg");
        std::fs::write(&path, jpeg).unwrap();
        let probe = crate::ffmpeg::probe_media(&path).await.expect("probe jpeg");
        (probe.width, probe.height)
    }

    #[tokio::test]
    async fn a_silent_clip_renders_labelled_frames_within_the_size_cap() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = clip(dir.path(), 12, "320x240");
        let config = VideoFramesConfig {
            max_long_edge: 128,
            ..cfg()
        };
        let blocks = show("room.mp4", &video, &config, &VisionStrategy::Native).await;
        let frames = images(&blocks);
        assert_eq!(frames.len(), 6, "12s at 2s spacing: six frames");
        assert_eq!(jpeg_size(dir.path(), &frames[0]).await, (128, 96));
        let text = texts(&blocks);
        assert!(
            text.contains("Video `room.mp4` (0:12.0 long, 320×240)"),
            "{text}"
        );
        assert!(text.contains("Frame 1/6 of `room.mp4` at 0:00.5"), "{text}");
        assert!(text.contains("Frame 6/6 of `room.mp4` at 0:11.5"), "{text}");
    }

    #[tokio::test]
    async fn a_range_shows_labelled_frames_from_inside_it() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = clip(dir.path(), 12, "320x240");
        let range = select(serde_json::json!({ "from": "0:04", "to": "0:07", "frames": 4 }));
        let blocks =
            render_frames("room.mp4", &video, &cfg(), &range, &VisionStrategy::Native).await;
        assert_eq!(images(&blocks).len(), 4);
        let text = texts(&blocks);
        assert!(
            text.contains("4 still frame(s) sampled from 0:04.0 to 0:07.0, in order"),
            "{text}"
        );
        for label in ["1/4 of `room.mp4` at 0:04.0", "4/4 of `room.mp4` at 0:07.0"] {
            assert!(text.contains(label), "{label}: {text}");
        }
    }

    #[tokio::test]
    async fn exact_timestamps_show_those_frames_enlarged_on_request() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = clip(dir.path(), 12, "320x240");
        let config = VideoFramesConfig {
            max_long_edge: 128,
            detail_long_edge: 192,
            ..cfg()
        };
        let exact = select(serde_json::json!({ "at": ["0:09", "0:03", 30], "detail": true }));
        let blocks =
            render_frames("room.mp4", &video, &config, &exact, &VisionStrategy::Native).await;
        let frames = images(&blocks);
        assert_eq!(frames.len(), 3);
        assert_eq!(jpeg_size(dir.path(), &frames[0]).await, (192, 144));
        let text = texts(&blocks);
        assert!(
            text.contains("3 still frame(s) at the timestamps asked for"),
            "{text}"
        );
        assert!(
            text.contains(
                "Timestamps past 0:11.5, the last frame that can be shown, are shown as that \
                 frame."
            ),
            "{text}"
        );
        for label in [
            "1/3 of `room.mp4` at 0:03.0",
            "2/3 of `room.mp4` at 0:09.0",
            "3/3 of `room.mp4` at 0:11.5",
        ] {
            assert!(text.contains(label), "{label}: {text}");
        }

        let plain = select(serde_json::json!({ "at": [3] }));
        let blocks =
            render_frames("room.mp4", &video, &config, &plain, &VisionStrategy::Native).await;
        assert_eq!(jpeg_size(dir.path(), &images(&blocks)[0]).await, (128, 96));
    }

    #[tokio::test]
    async fn a_range_past_the_end_says_so_instead_of_showing_frames() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = clip(dir.path(), 3, "64x64");
        let past = select(serde_json::json!({ "from": "0:10", "to": "0:16" }));
        let blocks =
            render_frames("room.mp4", &video, &cfg(), &past, &VisionStrategy::Native).await;
        assert_eq!(
            texts(&blocks),
            "Video `room.mp4`: `from` (0:10.0) is past its end (0:03.0 long), so there are no \
             frames to show."
        );
    }

    #[tokio::test]
    async fn a_selection_changes_nothing_for_audio_only_and_corrupt_files() {
        let dir = TempDir::new().unwrap();
        let exact = select(serde_json::json!({ "at": [1] }));
        let bogus = dir.path().join("broken.mp4");
        std::fs::write(&bogus, b"not a real video file").unwrap();
        let blocks = render_frames(
            "broken.mp4",
            &bogus,
            &cfg(),
            &exact,
            &VisionStrategy::Native,
        )
        .await;
        assert_eq!(blocks.len(), 1);
        assert!(texts(&blocks).contains("visual frame extraction failed"));

        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let audio = encode(
            dir.path(),
            "voice.mp4",
            &["-f", "lavfi", "-i", "sine=duration=2", "-c:a", "aac"],
        );
        let blocks =
            render_frames("voice.mp4", &audio, &cfg(), &exact, &VisionStrategy::Native).await;
        assert_eq!(blocks.len(), 1);
        assert!(texts(&blocks).contains("has no video stream"));
    }

    #[tokio::test]
    async fn a_rotated_phone_clip_comes_out_portrait() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let landscape = clip(dir.path(), 3, "320x240");
        let rotated = encode(
            dir.path(),
            "phone.mov",
            &[
                "-display_rotation",
                "90",
                "-i",
                landscape.to_str().unwrap(),
                "-c",
                "copy",
            ],
        );
        let blocks = show("IMG_0001.MOV", &rotated, &cfg(), &VisionStrategy::Native).await;
        let frames = images(&blocks);
        assert!(!frames.is_empty(), "{}", texts(&blocks));
        assert_eq!(jpeg_size(dir.path(), &frames[0]).await, (240, 320));
        assert!(texts(&blocks).contains("240×320 portrait"));

        let config = VideoFramesConfig {
            max_long_edge: 128,
            detail_long_edge: 160,
            ..cfg()
        };
        let closer = select(serde_json::json!({ "at": [1], "detail": true }));
        let blocks = render_frames(
            "IMG_0001.MOV",
            &rotated,
            &config,
            &closer,
            &VisionStrategy::Native,
        )
        .await;
        let frames = images(&blocks);
        assert_eq!(frames.len(), 1, "{}", texts(&blocks));
        assert_eq!(jpeg_size(dir.path(), &frames[0]).await, (120, 160));
    }

    #[tokio::test]
    async fn scene_changes_put_a_frame_in_every_shot() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = encode(
            dir.path(),
            "cuts.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=160x120:d=4:r=10",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=160x120:d=4:r=10",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=160x120:d=4:r=10",
                "-filter_complex",
                "[0][1][2]concat=n=3:v=1[v]",
                "-map",
                "[v]",
                "-c:v",
                "libx264",
                "-g",
                "10",
                "-pix_fmt",
                "yuv420p",
            ],
        );
        let cuts = scene_changes(&SYSTEM_TOOLS, &video, cfg().scene_threshold)
            .await
            .expect("scan");
        assert_eq!(cuts, vec![4.0, 8.0]);
        let blocks = show("cuts.mp4", &video, &cfg(), &VisionStrategy::Native).await;
        assert_eq!(images(&blocks).len(), 6);
        assert!(texts(&blocks).contains("across 3 detected shots"));
    }

    #[tokio::test]
    async fn an_audio_only_file_says_it_has_no_frames() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let audio = encode(
            dir.path(),
            "voice.mp4",
            &["-f", "lavfi", "-i", "sine=duration=2", "-c:a", "aac"],
        );
        let blocks = show("voice.mp4", &audio, &cfg(), &VisionStrategy::Native).await;
        assert_eq!(blocks.len(), 1);
        assert!(texts(&blocks).contains("has no video stream"));
    }

    #[tokio::test]
    async fn a_video_over_the_length_cap_gets_a_note_naming_the_knob() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = clip(dir.path(), 3, "64x64");
        let config = VideoFramesConfig {
            max_minutes: 0,
            ..cfg()
        };
        let blocks = show("long.mp4", &video, &config, &VisionStrategy::Native).await;
        assert!(images(&blocks).is_empty());
        let text = texts(&blocks);
        assert!(text.contains("[attachments.video].max_minutes"), "{text}");
    }

    #[tokio::test]
    async fn a_corrupt_file_says_extraction_failed() {
        let dir = TempDir::new().unwrap();
        let bogus = dir.path().join("broken.mp4");
        std::fs::write(&bogus, b"not a real video file").unwrap();
        let blocks = show("broken.mp4", &bogus, &cfg(), &VisionStrategy::Native).await;
        assert_eq!(blocks.len(), 1);
        let text = texts(&blocks);
        assert!(
            text.contains("Video `broken.mp4`: visual frame extraction failed"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_missing_ffprobe_says_why_frames_are_missing() {
        let dir = TempDir::new().unwrap();
        let video = dir.path().join("v.mp4");
        std::fs::write(&video, b"x").unwrap();
        let tools = Tools {
            ffmpeg: "workforce-no-such-ffmpeg",
            ffprobe: "workforce-no-such-ffprobe",
        };
        let blocks = render_with(
            &tools,
            "v.mp4",
            &video,
            &cfg(),
            &FrameSelection::default(),
            &VisionStrategy::Native,
        )
        .await;
        let text = texts(&blocks);
        assert!(text.contains("visual frame extraction failed"), "{text}");
        assert!(
            text.contains("workforce-no-such-ffprobe could not start"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_model_that_cannot_see_is_told_without_extracting() {
        let blocks = show(
            "v.mp4",
            Path::new("/nonexistent/v.mp4"),
            &cfg(),
            &VisionStrategy::StripWithPlaceholder(crate::attachments::NoDescriber::Unconfigured),
        )
        .await;
        assert_eq!(blocks.len(), 1);
        assert!(texts(&blocks).contains("cannot process images"));
    }

    struct CountingVision(AtomicUsize);

    #[async_trait::async_trait]
    impl VisionLlm for CountingVision {
        async fn describe_image(
            &self,
            media_type: &str,
            _data: &str,
            hint: Option<&str>,
        ) -> eyre::Result<String> {
            assert_eq!(media_type, "image/jpeg");
            assert!(hint.is_some_and(|h| h.contains(" at ")), "{hint:?}");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("a sofa under a window".to_string())
        }
        fn model_name(&self) -> &str {
            "fake-vision"
        }
    }

    #[tokio::test]
    async fn a_text_only_model_gets_one_description_per_frame() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let dir = TempDir::new().unwrap();
        let video = clip(dir.path(), 5, "64x64");
        let vision = CountingVision(AtomicUsize::new(0));
        let vision_cfg = workforce_common::settings::LlmVisionConfig::default();
        let strategy = VisionStrategy::DescribeWith(&vision, &vision_cfg);
        let blocks = show("v.mp4", &video, &cfg(), &strategy).await;
        assert!(
            images(&blocks).is_empty(),
            "no pixels for a text-only model"
        );
        assert_eq!(vision.0.load(Ordering::SeqCst), 3);
        let text = texts(&blocks);
        assert!(
            text.contains("Frame 2/3 of `v.mp4` at 0:02.5 (described by fake-vision):\na sofa"),
            "{text}"
        );

        let closer = select(serde_json::json!({ "at": [1, 3], "detail": true }));
        let blocks = render_frames("v.mp4", &video, &cfg(), &closer, &strategy).await;
        assert!(images(&blocks).is_empty());
        assert_eq!(vision.0.load(Ordering::SeqCst), 5, "two more frames");
        assert!(
            texts(&blocks).contains("Frame 2/2 of `v.mp4` at 0:03.0 (described by fake-vision)"),
            "{}",
            texts(&blocks)
        );
    }
}
