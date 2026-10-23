//! Video composition + single-clip editing on top of `ffmpeg`.
//!
//! [`VideoComposeTool`] (`compose_video`) stitches an ordered list of staged
//! clips/stills into one presentation MP4 — normalizing every segment to a
//! common resolution/fps/format, optionally trimming and fading each one,
//! joining them by a hard cut or a chained transition, and ducking in a music
//! bed. [`TrimVideoTool`] (`trim_video`) edits a single clip into a new one
//! (trim window + fades), preserving its native size; splitting at N seconds
//! is two `trim_video` calls.
//!
//! Both reuse the same probe/normalize/run/stage helpers, return the shared
//! `generated_images` marker the rest of the media path uses (so existing
//! delivery carries the result with no new sink code), and never let
//! model-authored strings reach ffmpeg — the filter graph is built from probed
//! facts. Registration is gated on `ffmpeg`/`ffprobe` being present.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::image_gen::GENERATED_IMAGES_KEY;
use super::registry::ToolRegistry;
use super::{Tool, ToolResult};
use crate::artifacts::resolve::{resolve_bytes_input, BytesRequest};
use crate::artifacts::store::{global_store, ArtifactStore};
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::ffmpeg::{ffmpeg_available, probe_media, run_ffmpeg};

pub const COMPOSE_TOOL_NAME: &str = "compose_video";
pub const TRIM_TOOL_NAME: &str = "trim_video";
pub const GIF_TOOL_NAME: &str = "video_to_gif";

const MIN_SEGMENTS: usize = 1;
const MAX_SEGMENTS: usize = 20;
const MIN_STILL_SECS: f64 = 0.5;
const MAX_STILL_SECS: f64 = 30.0;
const MIN_CLIP_SECS: f64 = 0.05;
const MAX_TOTAL_SECS: f64 = 600.0;
const MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const RENDER_TIMEOUT: Duration = Duration::from_secs(300);
/// Level of a `mix`-mode background track against the segments' own audio.
const BG_MIX_VOLUME: &str = "0.35";

const GIF_DEFAULT_FPS: u32 = 12;
const GIF_MIN_FPS: u32 = 5;
const GIF_MAX_FPS: u32 = 30;
const GIF_DEFAULT_WIDTH: u32 = 480;
const GIF_MIN_WIDTH: u32 = 120;
const GIF_MAX_WIDTH: u32 = 1080;
const GIF_MAX_CAPTION_CHARS: usize = 200;

// Caption auto-fit knobs. The renderer word-wraps each caption to the frame and
// shrinks the font until the widest line fits the usable width and the line
// block fits its vertical budget — so meme text never bleeds off the edges.
const CAPTION_WIDTH_FRAC: f32 = 0.90; // usable width = 90% of frame (5% pad/side)
const CAPTION_LINE_HEIGHT: f32 = 1.18; // line advance as a multiple of font size
const CAPTION_MAX_LINES: usize = 5;
const CAPTION_MIN_FONT: u32 = 12;
const CAPTION_MAX_FONT: u32 = 120;
const CAPTION_FONT_STEP: u32 = 2;

fn delivery_note(noun: &str) -> String {
    format!(
        "The system is delivering the {noun} to this conversation automatically — it happens \
         outside your context, so you will NOT see the attachment. This is expected, NOT a \
         failure: do not re-upload, attach, verify, or claim it failed. Just briefly tell the \
         user what you made."
    )
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

macro_rules! bail_tool {
    ($start:expr, $($arg:tt)*) => {
        return Ok(ToolResult::failure(format!($($arg)*), elapsed_ms($start)))
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
}

/// A resolved, probed segment ready to feed the filter graph. `duration` is
/// the effective (post-trim) length; `trim` holds the source window when the
/// clip is cut.
#[derive(Debug, Clone)]
struct Segment {
    kind: MediaKind,
    path: PathBuf,
    duration: f64,
    has_audio: bool,
    trim: Option<(f64, f64)>,
    fade_in: f64,
    fade_out: f64,
}

/// Per-clip edit parsed from a segment spec against its probed source length.
#[derive(Debug, Clone, PartialEq)]
struct Edit {
    trim: Option<(f64, f64)>,
    duration: f64,
    fade_in: f64,
    fade_out: f64,
}

/// Transition between adjacent segments, with a validated positive overlap.
#[derive(Debug, Clone, PartialEq)]
enum Transition {
    Cut,
    Xfade { name: &'static str, secs: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    w: u32,
    h: u32,
    fps: u32,
}

/// Register the ffmpeg-backed video tools for `agent_id` when the binaries are
/// present. Returns whether they were registered; needs no config or
/// hot-reload (the only dependency is a binary).
pub fn install_video_tools(tools: &ToolRegistry, agent_id: &str) -> bool {
    if !ffmpeg_available() {
        tracing::info!(
            agent = %agent_id,
            "ffmpeg/ffprobe not found on PATH; compose_video, trim_video and video_to_gif disabled"
        );
        return false;
    }
    tools.register(Arc::new(VideoComposeTool {
        agent_id: agent_id.to_string(),
    }));
    tools.register(Arc::new(TrimVideoTool {
        agent_id: agent_id.to_string(),
    }));
    tools.register(Arc::new(VideoToGifTool {
        agent_id: agent_id.to_string(),
    }));
    true
}

pub struct VideoComposeTool {
    agent_id: String,
}

#[async_trait]
impl Tool for VideoComposeTool {
    fn name(&self) -> &str {
        COMPOSE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Stitch one or more existing clips and/or still images into ONE presentation video. Pass \
         `segments` as an ordered list of artifact ids (from generate_video, generate_image, or \
         view_attachment). Per segment you may trim a video with `start_secs`/`end_secs`, hold an \
         image as a slide with `duration_secs`, and fade with `fade_in_secs`/`fade_out_secs`. \
         Optionally add a `transition` between segments and a `background_audio` track. Generated \
         clips carry their OWN soundtrack: to make the user's track the ONLY audio (\"put my song \
         on it\", \"replace the audio\") set `background_audio.mode` to `replace`; the default \
         `mix` plays it quietly UNDER the clips' own sound. This \
         re-encodes and BLOCKS until done (seconds to a minute or two), then the system delivers \
         the finished video to the user automatically — outside your context, so you will NOT see \
         the attachment. That is expected, NOT a failure: do not re-upload, attach, verify, or \
         claim it failed. Just briefly tell the user what you assembled."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "segments": {
                    "type": "array",
                    "minItems": MIN_SEGMENTS,
                    "maxItems": MAX_SEGMENTS,
                    "description": "Ordered clips/stills to join. Each item references a staged artifact.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "artifact_id": {
                                "type": "string",
                                "description": "Artifact id of a video or image (art_…)."
                            },
                            "duration_secs": {
                                "type": "number",
                                "description": "Seconds to hold an IMAGE segment (0.5–30). Ignored for video."
                            },
                            "start_secs": {
                                "type": "number",
                                "description": "Trim a VIDEO segment to start here (seconds). Default 0."
                            },
                            "end_secs": {
                                "type": "number",
                                "description": "Trim a VIDEO segment to end here (seconds). Default: end of clip."
                            },
                            "fade_in_secs": {
                                "type": "number",
                                "description": "Fade this segment in from black over N seconds."
                            },
                            "fade_out_secs": {
                                "type": "number",
                                "description": "Fade this segment out to black over N seconds."
                            }
                        },
                        "required": ["artifact_id"]
                    }
                },
                "transition": {
                    "type": "string",
                    "enum": ["none", "crossfade", "fade", "wipe", "slide", "dissolve", "circle"],
                    "description": "Transition between every adjacent pair. Default 'none' (hard cut)."
                },
                "transition_secs": {
                    "type": "number",
                    "description": "Transition length in seconds (default 0.5; clamped below half the shortest segment)."
                },
                "background_audio": {
                    "type": "object",
                    "description": "Optional audio track for the whole timeline (an audio file, or a video whose audio is used).",
                    "properties": {
                        "artifact_id": { "type": "string" },
                        "mode": {
                            "type": "string",
                            "enum": ["mix", "replace"],
                            "description": "'mix' (default) plays it quietly under the segments' own audio. 'replace' drops the segments' own audio so this track is the only sound."
                        },
                        "start_secs": {
                            "type": "number",
                            "description": "Start this many seconds into the track (e.g. to land on the solo). Default 0."
                        },
                        "loop_video": {
                            "type": "boolean",
                            "description": "Repeat the stitched video until the track ends, so the output is as long as the audio. Default false: the output is as long as the video and the track is cut to fit."
                        }
                    },
                    "required": ["artifact_id"]
                },
                "aspect_ratio": {
                    "type": "string",
                    "enum": ["16:9", "9:16", "1:1"],
                    "description": "Output shape; segments are scaled to fit and letterboxed. Default '16:9'."
                }
            },
            "required": ["segments"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, eyre::Error> {
        let started = Instant::now();

        let Some(store) = global_store() else {
            bail_tool!(
                started,
                "artifact store is not configured; cannot compose video"
            );
        };
        let Some(specs) = input["segments"].as_array() else {
            bail_tool!(started, "compose_video requires a 'segments' array");
        };
        if specs.len() < MIN_SEGMENTS {
            bail_tool!(
                started,
                "compose_video needs at least {MIN_SEGMENTS} segment"
            );
        }
        if specs.len() > MAX_SEGMENTS {
            bail_tool!(
                started,
                "compose_video accepts at most {MAX_SEGMENTS} segments"
            );
        }

        let Ok(dir) = tempfile::tempdir() else {
            bail_tool!(
                started,
                "could not create a working directory for video composition"
            );
        };

        let mut segments: Vec<Segment> = Vec::with_capacity(specs.len());
        for (i, spec) in specs.iter().enumerate() {
            let Some(aid) = spec["artifact_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
            else {
                bail_tool!(started, "segment {i} is missing 'artifact_id'");
            };
            let (path, is_img) = match fetch_to_temp(
                store.as_ref(),
                &self.agent_id,
                dir.path(),
                aid,
                &format!("seg{i}"),
            ) {
                Ok(v) => v,
                Err(e) => bail_tool!(started, "segment {i} ({aid}): {e}"),
            };
            let segment = if is_img {
                let Some(secs) = spec["duration_secs"].as_f64() else {
                    bail_tool!(
                        started,
                        "image segment {i} ({aid}) needs a 'duration_secs' to hold it as a slide"
                    );
                };
                let duration = secs.clamp(MIN_STILL_SECS, MAX_STILL_SECS);
                let (fade_in, fade_out) = parse_fades(spec, duration);
                Segment {
                    kind: MediaKind::Image,
                    path,
                    duration,
                    has_audio: false,
                    trim: None,
                    fade_in,
                    fade_out,
                }
            } else {
                let probe = match probe_media(&path).await {
                    Ok(p) if p.duration > 0.0 && p.has_video => p,
                    Ok(p) if p.duration > 0.0 => bail_tool!(
                        started,
                        "segment {i} ({aid}) has no video stream; segments must be video clips or images"
                    ),
                    Ok(_) => bail_tool!(
                        started,
                        "segment {i} ({aid}) has no readable duration; is it a video?"
                    ),
                    Err(e) => bail_tool!(started, "segment {i} ({aid}): {e}"),
                };
                let edit = match parse_video_edit(spec, probe.duration) {
                    Ok(e) => e,
                    Err(e) => bail_tool!(started, "segment {i} ({aid}): {e}"),
                };
                Segment {
                    kind: MediaKind::Video,
                    path,
                    duration: edit.duration,
                    has_audio: probe.has_audio,
                    trim: edit.trim,
                    fade_in: edit.fade_in,
                    fade_out: edit.fade_out,
                }
            };
            segments.push(segment);
        }

        let total: f64 = segments.iter().map(|s| s.duration).sum();
        if total > MAX_TOTAL_SECS {
            bail_tool!(
                started,
                "composed video would be ~{total:.0}s, over the {MAX_TOTAL_SECS:.0}s limit; use fewer or shorter segments"
            );
        }

        let min_dur = segments
            .iter()
            .map(|s| s.duration)
            .fold(f64::INFINITY, f64::min);
        let transition = match resolve_transition(
            input["transition"].as_str().unwrap_or("none"),
            input["transition_secs"].as_f64().unwrap_or(0.5),
            min_dur,
        ) {
            Ok(t) => t,
            Err(e) => bail_tool!(started, "{e}"),
        };

        let bg = match input.get("background_audio") {
            Some(Value::Null) | None => None,
            Some(bg) => {
                let Some(aid) = bg["artifact_id"].as_str().filter(|s| !s.trim().is_empty()) else {
                    bail_tool!(started, "background_audio needs an 'artifact_id'");
                };
                let path =
                    match fetch_to_temp(store.as_ref(), &self.agent_id, dir.path(), aid, "bg") {
                        Ok((path, _)) => path,
                        Err(e) => bail_tool!(started, "background_audio ({aid}): {e}"),
                    };
                let probe = match probe_media(&path).await {
                    Ok(p) if p.has_audio => p,
                    Ok(_) => bail_tool!(started, "background_audio ({aid}) has no audio track"),
                    Err(e) => bail_tool!(started, "background_audio ({aid}): {e}"),
                };
                match parse_background_audio(bg, path, probe.duration) {
                    Ok(b) => Some(b),
                    Err(e) => bail_tool!(started, "background_audio ({aid}): {e}"),
                }
            }
        };

        let target = target_for(input["aspect_ratio"].as_str().unwrap_or("16:9"));
        let out_path = dir.path().join("presentation.mp4");
        // Looping the picture to the track's length is a second pass over the
        // rendered timeline; otherwise the track is fitted in the one render.
        let video_secs = timeline_secs(&segments, &transition);
        let looped_secs = bg
            .as_ref()
            .filter(|b| b.loop_video)
            .map(|b| (b.duration - b.start).min(MAX_TOTAL_SECS))
            .filter(|audio_secs| *audio_secs > video_secs);
        let timeline_path = dir.path().join("timeline.mp4");
        let (first_out, first_bg) = match looped_secs {
            Some(_) => (&timeline_path, None),
            None => (&out_path, bg.as_ref()),
        };
        let args = build_ffmpeg_args(&segments, &transition, Some(&target), first_bg, first_out);
        if let Err(e) = run_ffmpeg(&args, RENDER_TIMEOUT).await {
            bail_tool!(started, "video composition failed: {e}");
        }
        if let (Some(secs), Some(bg)) = (looped_secs, bg.as_ref()) {
            let args = build_loop_args(&timeline_path, bg, secs, &out_path);
            if let Err(e) = run_ffmpeg(&args, RENDER_TIMEOUT).await {
                bail_tool!(started, "fitting the video to the audio failed: {e}");
            }
        }

        let (artifact_id, bytes_len) = match stage_output(
            store.as_ref(),
            &self.agent_id,
            &out_path,
            COMPOSE_TOOL_NAME,
            "presentation.mp4",
            "video/mp4",
        ) {
            Ok(v) => v,
            Err(e) => bail_tool!(started, "{e}"),
        };

        let output = json!({
            GENERATED_IMAGES_KEY: [{
                "artifact_id": artifact_id,
                "media_type": "video/mp4",
                "filename": "presentation.mp4",
                "bytes": bytes_len,
            }],
            "segments": segments.len(),
            "duration_secs": looped_secs.unwrap_or(video_secs),
            "audio": audio_summary(bg.as_ref()),
            "note": delivery_note("video"),
        });
        Ok(ToolResult::success(output, elapsed_ms(started)))
    }
}

pub struct TrimVideoTool {
    agent_id: String,
}

#[async_trait]
impl Tool for TrimVideoTool {
    fn name(&self) -> &str {
        TRIM_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Trim and/or fade a single existing video clip into a NEW clip. Use `start_secs`/`end_secs` \
         to keep only that window (e.g. drop the first 3s, or cut the end), and \
         `fade_in_secs`/`fade_out_secs` to fade from/to black. To SPLIT a clip at N seconds, call \
         twice: once with end_secs=N, once with start_secs=N. Preserves the source resolution. \
         This re-encodes and BLOCKS until done, then the system delivers the new clip \
         automatically — outside your context, so you will NOT see it. That is expected, NOT a \
         failure: do not re-upload, attach, verify, or claim it failed. Briefly tell the user what \
         you trimmed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "Artifact id of the source video clip (art_…)."
                },
                "start_secs": {
                    "type": "number",
                    "description": "Keep from this point (seconds). Default 0."
                },
                "end_secs": {
                    "type": "number",
                    "description": "Keep until this point (seconds). Default: end of clip."
                },
                "fade_in_secs": {
                    "type": "number",
                    "description": "Fade in from black over N seconds."
                },
                "fade_out_secs": {
                    "type": "number",
                    "description": "Fade out to black over N seconds."
                }
            },
            "required": ["artifact_id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, eyre::Error> {
        let started = Instant::now();

        let Some(store) = global_store() else {
            bail_tool!(
                started,
                "artifact store is not configured; cannot trim video"
            );
        };
        let Some(aid) = input["artifact_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        else {
            bail_tool!(started, "trim_video requires an 'artifact_id'");
        };

        let Ok(dir) = tempfile::tempdir() else {
            bail_tool!(started, "could not create a working directory for trimming");
        };

        let (path, is_img) =
            match fetch_to_temp(store.as_ref(), &self.agent_id, dir.path(), aid, "clip") {
                Ok(v) => v,
                Err(e) => bail_tool!(started, "{e}"),
            };
        if is_img {
            bail_tool!(
                started,
                "trim_video is for video clips; an image has no timeline to trim"
            );
        }
        let probe = match probe_media(&path).await {
            Ok(p) if p.duration > 0.0 && p.has_video => p,
            Ok(p) if p.duration > 0.0 => {
                bail_tool!(started, "this artifact has no video stream to trim")
            }
            Ok(_) => bail_tool!(
                started,
                "the input has no readable duration; is it a video?"
            ),
            Err(e) => bail_tool!(started, "{e}"),
        };
        let edit = match parse_video_edit(&input, probe.duration) {
            Ok(e) => e,
            Err(e) => bail_tool!(started, "{e}"),
        };
        let segment = Segment {
            kind: MediaKind::Video,
            path,
            duration: edit.duration,
            has_audio: probe.has_audio,
            trim: edit.trim,
            fade_in: edit.fade_in,
            fade_out: edit.fade_out,
        };

        let out_path = dir.path().join("clip.mp4");
        let args = build_ffmpeg_args(
            std::slice::from_ref(&segment),
            &Transition::Cut,
            None,
            None,
            &out_path,
        );
        if let Err(e) = run_ffmpeg(&args, RENDER_TIMEOUT).await {
            bail_tool!(started, "trim failed: {e}");
        }

        let (artifact_id, bytes_len) = match stage_output(
            store.as_ref(),
            &self.agent_id,
            &out_path,
            TRIM_TOOL_NAME,
            "clip.mp4",
            "video/mp4",
        ) {
            Ok(v) => v,
            Err(e) => bail_tool!(started, "{e}"),
        };

        let output = json!({
            GENERATED_IMAGES_KEY: [{
                "artifact_id": artifact_id,
                "media_type": "video/mp4",
                "filename": "clip.mp4",
                "bytes": bytes_len,
            }],
            "duration_secs": round_ms(edit.duration),
            "note": delivery_note("video"),
        });
        Ok(ToolResult::success(output, elapsed_ms(started)))
    }
}

pub struct VideoToGifTool {
    agent_id: String,
}

#[async_trait]
impl Tool for VideoToGifTool {
    fn name(&self) -> &str {
        GIF_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Turn an existing video clip (or animated GIF) into an optimized, looping animated GIF — \
         great for memes and Slack reactions. Pass `artifact_id` of a staged video (from \
         generate_video, view_attachment, etc.) or GIF. Optionally trim with `start_secs`/`end_secs`, \
         set `fps` (default 12) and output `width` (default 480px), and burn meme captions with \
         `caption_top` and/or `caption_bottom`. Caption text is automatically word-wrapped and \
         sized to fit the frame — just pass the words, do NOT add your own line breaks. Optionally \
         set `font_size` (px, to override the auto-fit size) and `font_color` (a colour name like \
         `yellow` or a `#RRGGBB` hex; default white). Emoji and non-Latin characters are dropped. \
         This re-encodes and BLOCKS \
         until done (usually a few seconds), then the system delivers the finished GIF to the user \
         automatically — outside your context, so you will NOT see the attachment. That is expected, \
         NOT a failure: do not re-upload, attach, verify, or claim it failed. Just briefly tell the \
         user what you made."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "description": "Artifact id of a video or animated GIF (art_…)."
                },
                "start_secs": {
                    "type": "number",
                    "description": "Trim to start here (seconds). Default 0."
                },
                "end_secs": {
                    "type": "number",
                    "description": "Trim to end here (seconds). Default: end of clip."
                },
                "fps": {
                    "type": "number",
                    "description": "Frames per second (5–30, default 12). Lower = smaller file."
                },
                "width": {
                    "type": "number",
                    "description": "Output width in pixels (120–1080, default 480); height scales to keep aspect."
                },
                "caption_top": {
                    "type": "string",
                    "description": "Optional meme caption across the top. Auto-wrapped and auto-sized to fit; do not add line breaks yourself."
                },
                "caption_bottom": {
                    "type": "string",
                    "description": "Optional meme caption across the bottom. Auto-wrapped and auto-sized to fit; do not add line breaks yourself."
                },
                "font_size": {
                    "type": "number",
                    "description": "Optional caption font size in pixels (12–120). Overrides the automatic fit-to-frame sizing; text still wraps to the width."
                },
                "font_color": {
                    "type": "string",
                    "description": "Optional caption colour: a name (white, black, yellow, red, orange, green, blue, cyan, magenta, pink, purple, gray, gold, lime) or a #RRGGBB hex. Default white. The outline stays black for readability."
                }
            },
            "required": ["artifact_id"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, eyre::Error> {
        let started = Instant::now();

        let Some(store) = global_store() else {
            bail_tool!(
                started,
                "artifact store is not configured; cannot make a GIF"
            );
        };
        let Some(aid) = input["artifact_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        else {
            bail_tool!(started, "video_to_gif requires an 'artifact_id'");
        };
        let spec = match parse_gif_spec(&input) {
            Ok(s) => s,
            Err(e) => bail_tool!(started, "{e}"),
        };
        if spec.has_captions() && !drawtext_available() {
            bail_tool!(
                started,
                "this server's ffmpeg has no drawtext filter, so captions are unavailable; omit caption_top/caption_bottom to make a plain GIF"
            );
        }

        let Ok(dir) = tempfile::tempdir() else {
            bail_tool!(started, "could not create a working directory for the GIF");
        };
        let (path, _is_img) =
            match fetch_to_temp(store.as_ref(), &self.agent_id, dir.path(), aid, "clip") {
                Ok(v) => v,
                Err(e) => bail_tool!(started, "{e}"),
            };
        let probe = match probe_media(&path).await {
            Ok(p) if p.has_video && p.duration > 0.0 => p,
            Ok(p) if p.has_video => bail_tool!(
                started,
                "this looks like a still image; a GIF needs a video or animation with motion"
            ),
            Ok(_) => bail_tool!(
                started,
                "this artifact has no video stream to turn into a GIF"
            ),
            Err(e) => bail_tool!(started, "{e}"),
        };
        let edit = match parse_video_edit(&input, probe.duration) {
            Ok(e) => e,
            Err(e) => bail_tool!(started, "{e}"),
        };

        let font = if spec.has_captions() {
            match crate::tools::report::fonts::font_path() {
                Ok(p) => p.join("Lato-Bold.ttf"),
                Err(e) => bail_tool!(started, "could not load caption font: {e}"),
            }
        } else {
            PathBuf::new()
        };

        // Word-wrap and auto-size each caption so it fits the output frame. The
        // vertical budget splits the height between the two captions when both
        // are present, leaving the middle clear.
        let (src_w, src_h) = probe.display_size();
        let out_h = predict_output_height(spec.width, src_w, src_h);
        let both = spec.top.is_some() && spec.bottom.is_some();
        let height_budget = out_h * if both { 0.40 } else { 0.50 };
        let metrics = if spec.has_captions() {
            match caption_metrics() {
                Some(m) => Some(m),
                None => bail_tool!(started, "could not load caption font metrics"),
            }
        } else {
            None
        };

        let plan = |raw: Option<&str>, name: &str| -> Result<Option<(PathBuf, u32)>, String> {
            let Some(raw) = raw else { return Ok(None) };
            let m = metrics.expect("metrics present when a caption is");
            let (wrapped, fontsize) =
                plan_caption(raw, spec.width, height_budget, m, spec.font_size);
            let path = write_caption(dir.path(), name, Some(&wrapped))?
                .expect("write_caption returns Some for Some text");
            Ok(Some((path, fontsize)))
        };
        let top = match plan(spec.top.as_deref(), "top.txt") {
            Ok(v) => v,
            Err(e) => bail_tool!(started, "{e}"),
        };
        let bottom = match plan(spec.bottom.as_deref(), "bottom.txt") {
            Ok(v) => v,
            Err(e) => bail_tool!(started, "{e}"),
        };
        let top_cap = top.as_ref().map(|(p, fs)| DrawCaption {
            textfile: p.as_path(),
            fontsize: *fs,
            color: spec.color.as_str(),
        });
        let bottom_cap = bottom.as_ref().map(|(p, fs)| DrawCaption {
            textfile: p.as_path(),
            fontsize: *fs,
            color: spec.color.as_str(),
        });

        let out_path = dir.path().join("out.gif");
        let graph =
            build_gif_filtergraph(edit.trim, spec.fps, spec.width, top_cap, bottom_cap, &font);
        let args = build_gif_args(&path, &graph, &out_path);
        if let Err(e) = run_ffmpeg(&args, RENDER_TIMEOUT).await {
            bail_tool!(started, "GIF render failed: {e}");
        }

        let (artifact_id, bytes_len) = match stage_output(
            store.as_ref(),
            &self.agent_id,
            &out_path,
            GIF_TOOL_NAME,
            "meme.gif",
            "image/gif",
        ) {
            Ok(v) => v,
            Err(e) => bail_tool!(started, "{e}"),
        };

        let output = json!({
            GENERATED_IMAGES_KEY: [{
                "artifact_id": artifact_id,
                "media_type": "image/gif",
                "filename": "meme.gif",
                "bytes": bytes_len,
            }],
            "duration_secs": round_ms(edit.duration),
            "note": delivery_note("GIF"),
        });
        Ok(ToolResult::success(output, elapsed_ms(started)))
    }
}

/// Parsed `video_to_gif` knobs: clamped fps/width, sanitized captions, and the
/// optional caption font-size override and validated font colour.
#[derive(Debug, Clone, PartialEq)]
struct GifSpec {
    fps: u32,
    width: u32,
    top: Option<String>,
    bottom: Option<String>,
    /// Explicit caption size in px; `None` ⇒ auto-fit to the frame.
    font_size: Option<u32>,
    /// drawtext-safe colour token (a name from the allowlist or `0xRRGGBB`).
    color: String,
}

impl GifSpec {
    fn has_captions(&self) -> bool {
        self.top.is_some() || self.bottom.is_some()
    }
}

fn parse_gif_spec(input: &Value) -> Result<GifSpec, String> {
    let clamp = |v: f64, lo: u32, hi: u32| (v.round() as i64).clamp(lo as i64, hi as i64) as u32;
    let fps = input["fps"]
        .as_f64()
        .map_or(GIF_DEFAULT_FPS, |v| clamp(v, GIF_MIN_FPS, GIF_MAX_FPS));
    let width = input["width"].as_f64().map_or(GIF_DEFAULT_WIDTH, |v| {
        clamp(v, GIF_MIN_WIDTH, GIF_MAX_WIDTH)
    });
    let top = sanitize_caption(input["caption_top"].as_str())?;
    let bottom = sanitize_caption(input["caption_bottom"].as_str())?;
    let font_size = input["font_size"]
        .as_f64()
        .map(|v| clamp(v, CAPTION_MIN_FONT, CAPTION_MAX_FONT));
    let color = sanitize_color(input["font_color"].as_str())?;
    Ok(GifSpec {
        fps,
        width,
        top,
        bottom,
        font_size,
        color,
    })
}

/// Validate a caption colour into a token safe to splice into the drawtext
/// filtergraph (unlike caption text, the colour DOES enter the graph string).
/// Accepts a small allowlist of names or `#RRGGBB`/`0xRRGGBB` hex; everything
/// else is rejected with guidance. `None`/empty ⇒ the default white.
fn sanitize_color(raw: Option<&str>) -> Result<String, String> {
    const NAMED: &[&str] = &[
        "white", "black", "yellow", "red", "orange", "green", "blue", "cyan", "magenta", "pink",
        "purple", "gray", "grey", "gold", "lime", "navy", "teal", "maroon", "olive", "silver",
        "brown",
    ];
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok("white".to_string());
    };
    let lower = raw.to_ascii_lowercase();
    if NAMED.contains(&lower.as_str()) {
        return Ok(lower);
    }
    let hex = lower
        .strip_prefix('#')
        .or_else(|| lower.strip_prefix("0x"))
        .unwrap_or(&lower);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(format!("0x{hex}"));
    }
    Err(format!(
        "unsupported font_color '{raw}'; use a colour name (e.g. white, yellow, red) or a #RRGGBB hex"
    ))
}

/// Drop characters the bundled Lato font cannot render (emoji, non-Latin
/// scripts) so captions never render as missing-glyph boxes; keep newlines as
/// explicit line breaks. `None` when no caption was asked for; an `Err` when
/// the request was non-empty but nothing renderable survived or it is too long.
fn sanitize_caption(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let cleaned: String = raw.chars().filter(|&c| is_renderable(c)).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err(
            "caption has no renderable text; emoji and non-Latin scripts are not supported"
                .to_string(),
        );
    }
    let count = trimmed.chars().count();
    if count > GIF_MAX_CAPTION_CHARS {
        return Err(format!(
            "caption is {count} characters; keep each caption under {GIF_MAX_CAPTION_CHARS}"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Whether `c` is in the Latin subset the bundled Lato font renders (plus
/// newline): Basic Latin, Latin-1/Extended-A-B, dashes, quotes and ellipsis.
fn is_renderable(c: char) -> bool {
    if c == '\n' {
        return true;
    }
    if c.is_control() {
        return false;
    }
    matches!(
        c as u32,
        0x20..=0x7E | 0xA0..=0x24F | 0x2013..=0x2014 | 0x2018..=0x201F | 0x2026
    )
}

fn write_caption(dir: &Path, name: &str, text: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(text) = text else {
        return Ok(None);
    };
    let path = dir.join(name);
    std::fs::write(&path, text).map_err(|e| format!("could not stage caption text: {e}"))?;
    Ok(Some(path))
}

/// Horizontal glyph advances for the bundled Lato-Bold face, in font units,
/// plus its em size — enough to measure a caption line's pixel width at any font
/// size without a full text-shaping engine. Built once from the in-binary font.
struct FontMetrics {
    units_per_em: f32,
    advances: HashMap<char, u16>,
    fallback: u16,
}

impl FontMetrics {
    fn load() -> Option<FontMetrics> {
        let face =
            ttf_parser::Face::parse(crate::tools::report::fonts::lato_bold_bytes(), 0).ok()?;
        let upem = face.units_per_em();
        let mut advances = HashMap::new();
        // Mirror `is_renderable`'s Latin coverage so every char a caption can
        // contain has a measured advance.
        let ranges: [std::ops::RangeInclusive<u32>; 5] = [
            0x20..=0x7E,
            0xA0..=0x24F,
            0x2013..=0x2014,
            0x2018..=0x201F,
            0x2026..=0x2026,
        ];
        for range in ranges {
            for cp in range {
                if let Some(c) = char::from_u32(cp) {
                    if let Some(adv) = face.glyph_index(c).and_then(|g| face.glyph_hor_advance(g)) {
                        advances.insert(c, adv);
                    }
                }
            }
        }
        let fallback = advances.get(&'x').copied().unwrap_or(upem / 2);
        Some(FontMetrics {
            units_per_em: upem as f32,
            advances,
            fallback,
        })
    }

    /// Pixel width of `line` rendered at `fontsize` (advance widths only — no
    /// kerning; close enough for fit decisions, and it slightly over-estimates).
    fn line_width_px(&self, line: &str, fontsize: f32) -> f32 {
        let units: u32 = line
            .chars()
            .map(|c| self.advances.get(&c).copied().unwrap_or(self.fallback) as u32)
            .sum();
        units as f32 / self.units_per_em * fontsize
    }
}

/// The Lato-Bold metrics, parsed once. `None` only if the bundled font fails to
/// parse (it never has), in which case the caller falls back gracefully.
fn caption_metrics() -> Option<&'static FontMetrics> {
    static METRICS: OnceLock<Option<FontMetrics>> = OnceLock::new();
    METRICS.get_or_init(FontMetrics::load).as_ref()
}

/// The largest auto-fit font size to attempt for a caption at output width `w`:
/// generous so short captions render big and punchy, then shrunk to fit.
fn caption_max_font(w: u32) -> u32 {
    (w / 7).clamp(22, 96)
}

/// Predicted output height: the source aspect applied to the chosen width (the
/// graph scales `width:-1`). Falls back to 16:9 when source dims are unknown, so
/// caption fitting still has a sane vertical budget.
fn predict_output_height(out_w: u32, src_w: u32, src_h: u32) -> f32 {
    if src_w > 0 && src_h > 0 {
        out_w as f32 * src_h as f32 / src_w as f32
    } else {
        out_w as f32 * 9.0 / 16.0
    }
}

/// Collapse every line break and whitespace run — including the literal two-char
/// `\n`/`\t`/`\r` sequences a model sometimes emits instead of a real newline —
/// into single spaces, so the wrapper owns every line decision and a fumbled
/// manual break can never reach drawtext as visible text.
fn collapse_whitespace(text: &str) -> String {
    let unescaped = text
        .replace("\\n", " ")
        .replace("\\r", " ")
        .replace("\\t", " ");
    unescaped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Greedily word-wrap `words` so each line's pixel width stays within `usable`
/// at `fontsize`. A single word wider than `usable` becomes its own (overflowing)
/// line — the caller shrinks the font until even that fits.
fn wrap_words(words: &[&str], usable: f32, metrics: &FontMetrics, fontsize: f32) -> Vec<String> {
    let space = metrics.line_width_px(" ", fontsize);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0.0f32;
    for &word in words {
        let word_w = metrics.line_width_px(word, fontsize);
        if line.is_empty() {
            line.push_str(word);
            line_w = word_w;
        } else if line_w + space + word_w <= usable {
            line.push(' ');
            line.push_str(word);
            line_w += space + word_w;
        } else {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
            line_w = word_w;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Plan one caption: collapse the text to words, then pick the largest font size
/// whose wrapped lines all fit the frame width and whose block fits `max_height`
/// pixels. Returns the wrapped text (real newlines) and the chosen size. An
/// explicit `size_override` is honoured as-is (text still wraps to the width).
fn plan_caption(
    text: &str,
    out_w: u32,
    max_height: f32,
    metrics: &FontMetrics,
    size_override: Option<u32>,
) -> (String, u32) {
    let flat = collapse_whitespace(text);
    let words: Vec<&str> = flat.split(' ').filter(|w| !w.is_empty()).collect();
    let usable = out_w as f32 * CAPTION_WIDTH_FRAC;
    let max_fs = size_override
        .unwrap_or_else(|| caption_max_font(out_w))
        .clamp(CAPTION_MIN_FONT, CAPTION_MAX_FONT);
    if words.is_empty() {
        return (String::new(), max_fs);
    }
    // With an explicit size we don't shrink; otherwise scan down to the minimum.
    let min_fs = size_override.map_or(CAPTION_MIN_FONT, |_| max_fs);
    let mut fontsize = max_fs;
    loop {
        let lines = wrap_words(&words, usable, metrics, fontsize as f32);
        let widest = lines
            .iter()
            .map(|l| metrics.line_width_px(l, fontsize as f32))
            .fold(0.0_f32, f32::max);
        let block_h = lines.len() as f32 * fontsize as f32 * CAPTION_LINE_HEIGHT;
        let fits = widest <= usable && block_h <= max_height && lines.len() <= CAPTION_MAX_LINES;
        if fits || fontsize <= min_fs {
            return (lines.join("\n"), fontsize);
        }
        fontsize = fontsize.saturating_sub(CAPTION_FONT_STEP).max(min_fs);
    }
}

/// A laid-out caption ready for the filtergraph: the staged text file, its
/// chosen font size, and the validated colour token.
struct DrawCaption<'a> {
    textfile: &'a Path,
    fontsize: u32,
    color: &'a str,
}

/// Pure: the `-vf` value for a high-quality GIF — optional trim, fps/scale,
/// optional meme captions, then the canonical `palettegen`/`paletteuse` pair.
/// Caption text never enters this string: it lives in a textfile referenced by
/// its (quoted) path, with `expansion=none` so drawtext renders it verbatim.
fn build_gif_filtergraph(
    trim: Option<(f64, f64)>,
    fps: u32,
    width: u32,
    top: Option<DrawCaption>,
    bottom: Option<DrawCaption>,
    font: &Path,
) -> String {
    let mut chain: Vec<String> = Vec::new();
    if let Some((start, end)) = trim {
        chain.push(format!("trim=start={}:end={}", fsecs(start), fsecs(end)));
        chain.push("setpts=PTS-STARTPTS".into());
    }
    chain.push(format!("fps={fps}"));
    chain.push(format!("scale={width}:-1:flags=lanczos"));
    if let Some(cap) = top {
        let margin = (cap.fontsize / 2).max(8);
        chain.push(drawtext_filter(&cap, font, &margin.to_string()));
    }
    if let Some(cap) = bottom {
        let margin = (cap.fontsize / 2).max(8);
        chain.push(drawtext_filter(&cap, font, &format!("h-text_h-{margin}")));
    }
    format!(
        "{},split[s0][s1];[s0]palettegen=stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
        chain.join(",")
    )
}

/// One `drawtext` clause. `text_align=center` centres each wrapped line within
/// the block; `x=(w-text_w)/2` centres the block in the frame. The colour is a
/// validated token; the text lives in a quoted file with `expansion=none`, so
/// no model-authored string ever reaches the filtergraph.
fn drawtext_filter(cap: &DrawCaption, font: &Path, y: &str) -> String {
    let borderw = (cap.fontsize / 12).max(2);
    format!(
        "drawtext=fontfile={font}:textfile={text}:expansion=none:text_align=center:fontcolor={color}:bordercolor=black:borderw={borderw}:fontsize={fontsize}:x=(w-text_w)/2:y={y}",
        color = cap.color,
        fontsize = cap.fontsize,
        font = ffmpeg_quote(&font.display().to_string()),
        text = ffmpeg_quote(&cap.textfile.display().to_string()),
    )
}

/// Single-quote a tempdir-controlled path for a filtergraph value so spaces and
/// option-delimiter characters (`:`, `,`) stay literal. Paths here never contain
/// a single quote (fixed filenames under `tempfile::tempdir()`); the `'\''` swap
/// is belt-and-braces, not a general-purpose escaper for model-supplied strings.
fn ffmpeg_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Pure: full `ffmpeg` argv (after the binary) for the GIF export. `-loop 0`
/// makes it loop forever; `-an` drops audio (GIF carries none).
fn build_gif_args(input: &Path, graph: &str, out: &Path) -> Vec<String> {
    [
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-i",
        &input.display().to_string(),
        "-vf",
        graph,
        "-loop",
        "0",
        "-an",
        &out.display().to_string(),
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Whether this `ffmpeg` was built with the `drawtext` filter (libfreetype).
/// Probed once and cached — the build doesn't change at runtime.
fn drawtext_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("drawtext"))
            .unwrap_or(false)
    })
}

/// Resolve an artifact to a temp file, naming it by sniffed extension and
/// reporting whether the bytes are a still image (the still-vs-clip split).
fn fetch_to_temp(
    store: &dyn ArtifactStore,
    agent_id: &str,
    dir: &Path,
    artifact_id: &str,
    stem: &str,
) -> Result<(PathBuf, bool), String> {
    let resolved = resolve_bytes_input(
        store,
        agent_id,
        BytesRequest {
            artifact_id: Some(artifact_id),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let path = dir.join(format!("{stem}.{}", sniff_ext(&resolved.bytes)));
    std::fs::write(&path, &resolved.bytes).map_err(|e| format!("could not stage bytes: {e}"))?;
    Ok((path, is_image(&resolved.bytes)))
}

/// Read, size-check, and stage the rendered output; returns its id and size.
fn stage_output(
    store: &dyn ArtifactStore,
    agent_id: &str,
    out: &Path,
    producer: &str,
    filename: &str,
    media_type: &str,
) -> Result<(String, usize), String> {
    let rendered =
        std::fs::read(out).map_err(|e| format!("could not read rendered output: {e}"))?;
    if rendered.is_empty() {
        return Err("ffmpeg produced an empty file".to_string());
    }
    if rendered.len() > MAX_OUTPUT_BYTES {
        return Err(format!(
            "rendered output is {}MiB, over the {}MiB limit; use a shorter clip or lower resolution",
            rendered.len() / (1024 * 1024),
            MAX_OUTPUT_BYTES / (1024 * 1024)
        ));
    }
    let len = rendered.len();
    let descriptor =
        ArtifactDescriptor::from_context(producer, ContentType::OctetStream, agent_id.to_string())
            .with_file_meta(filename, media_type);
    let handle = store
        .write(agent_id, descriptor, ArtifactBody::Bytes(rendered))
        .map_err(|e| format!("failed to stage rendered output: {e}"))?;
    Ok((handle.id.as_str().to_string(), len))
}

fn target_for(aspect: &str) -> Target {
    match aspect {
        "9:16" => Target {
            w: 720,
            h: 1280,
            fps: 30,
        },
        "1:1" => Target {
            w: 720,
            h: 720,
            fps: 30,
        },
        _ => Target {
            w: 1280,
            h: 720,
            fps: 30,
        },
    }
}

/// Parse a clip's trim window + fades against its probed source length. The
/// trim is dropped when it spans the whole clip so the graph stays minimal.
fn parse_video_edit(spec: &Value, source: f64) -> Result<Edit, String> {
    let start = spec["start_secs"].as_f64().unwrap_or(0.0).max(0.0);
    let end = spec["end_secs"].as_f64().unwrap_or(source).min(source);
    if end - start < MIN_CLIP_SECS {
        return Err(format!(
            "start_secs ({start:.2}) must be at least {MIN_CLIP_SECS}s before end_secs ({end:.2}); the clip is {source:.2}s"
        ));
    }
    let duration = end - start;
    let trim = (start > 0.001 || end < source - 0.001).then_some((start, end));
    let (fade_in, fade_out) = parse_fades(spec, duration);
    Ok(Edit {
        trim,
        duration,
        fade_in,
        fade_out,
    })
}

fn parse_fades(spec: &Value, duration: f64) -> (f64, f64) {
    let fade_in = spec["fade_in_secs"]
        .as_f64()
        .unwrap_or(0.0)
        .clamp(0.0, duration);
    let fade_out = spec["fade_out_secs"]
        .as_f64()
        .unwrap_or(0.0)
        .clamp(0.0, (duration - fade_in).max(0.0));
    (fade_in, fade_out)
}

/// Map a user transition name to its ffmpeg `xfade` kind and clamp the overlap
/// to half the shortest segment, which keeps every `xfade` offset positive.
fn resolve_transition(name: &str, secs: f64, min_dur: f64) -> Result<Transition, String> {
    let xfade = match name {
        "none" => return Ok(Transition::Cut),
        "crossfade" => "fade",
        "fade" => "fadeblack",
        "wipe" => "wipeleft",
        "slide" => "slideleft",
        "dissolve" => "dissolve",
        "circle" => "circleopen",
        other => {
            return Err(format!(
                "unknown transition '{other}'; use none, crossfade, fade, wipe, slide, dissolve, or circle"
            ))
        }
    };
    if secs <= 0.0 {
        return Err(format!("transition_secs must be positive (got {secs})"));
    }
    let overlap = secs.min(min_dur * 0.5);
    if overlap < 0.05 {
        return Err(format!(
            "segments are too short ({min_dur:.2}s) for a transition; use clips of ~1s+ or transition 'none'"
        ));
    }
    Ok(Transition::Xfade {
        name: xfade,
        secs: round_ms(overlap),
    })
}

fn round_ms(secs: f64) -> f64 {
    (secs * 1000.0).round() / 1000.0
}

fn fsecs(secs: f64) -> String {
    format!("{secs:.3}")
}

/// PNG / JPEG / GIF / WebP / BMP magic ⇒ still image.
fn is_image(bytes: &[u8]) -> bool {
    matches!(sniff_ext(bytes), "png" | "jpg" | "gif" | "webp" | "bmp")
}

/// Best-effort container extension from magic bytes. The extension is only a
/// demux hint (ffmpeg probes content); the image cases double as the
/// still-vs-clip classifier. Unknown ⇒ "mp4".
fn sniff_ext(b: &[u8]) -> &'static str {
    let has = |sig: &[u8]| b.len() >= sig.len() && &b[..sig.len()] == sig;
    let riff_tag = |tag: &[u8]| b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == tag;
    if has(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if has(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if has(b"GIF8") {
        "gif"
    } else if riff_tag(b"WEBP") {
        "webp"
    } else if has(b"BM") {
        "bmp"
    } else if riff_tag(b"AVI ") {
        "avi"
    } else if riff_tag(b"WAVE") {
        "wav"
    } else if has(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "webm"
    } else if has(b"OggS") {
        "ogg"
    } else if has(b"fLaC") {
        "flac"
    } else if has(b"ID3") || has(&[0xFF, 0xFB]) {
        "mp3"
    } else {
        "mp4"
    }
}

/// Pure: full `ffmpeg` argv (after the binary). Free of I/O so the filter graph
/// and offsets can be unit-tested. `target` is `Some` to normalize every
/// segment onto one canvas (compose), `None` to preserve native size (trim).
fn build_ffmpeg_args(
    segments: &[Segment],
    transition: &Transition,
    target: Option<&Target>,
    bg: Option<&BackgroundAudio>,
    out: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = ["-y", "-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for seg in segments {
        if seg.kind == MediaKind::Image {
            args.extend(["-loop".into(), "1".into(), "-t".into(), fsecs(seg.duration)]);
        }
        args.extend(["-i".into(), seg.path.display().to_string()]);
    }
    if let Some(bg) = bg {
        args.extend([
            "-stream_loop".into(),
            "-1".into(),
            "-i".into(),
            bg.path.display().to_string(),
        ]);
    }
    args.extend([
        "-filter_complex".into(),
        build_filtergraph(segments, transition, target, bg),
        "-map".into(),
        "[vout]".into(),
        "-map".into(),
        "[aout]".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.display().to_string(),
    ]);
    args
}

/// Pure: the `-filter_complex` value. Normalizes/edits each segment (and
/// synthesizes silence for any audio-less input — `concat`/`acrossfade` demand
/// an audio stream on every input), joins them by a hard cut or chained
/// `xfade`/`acrossfade`, then ducks in the optional music bed — or, in
/// `Replace` mode, discards the program audio and plays the track alone.
/// Program audio lands on `[aout]` (or `[aprog]` when a track follows); video
/// on `[vout]`.
fn build_filtergraph(
    segments: &[Segment],
    transition: &Transition,
    target: Option<&Target>,
    bg: Option<&BackgroundAudio>,
) -> String {
    let n = segments.len();
    let prog_audio = if bg.is_some() { "aprog" } else { "aout" };

    let mut chains: Vec<String> = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        chains.push(video_chain(i, seg, target));
        chains.push(audio_chain(i, seg));
    }

    match transition {
        Transition::Cut if n == 1 => {
            // `concat=n=1` is rejected by ffmpeg; pass the lone segment through.
            chains.push("[v0]null[vout]".to_string());
            chains.push(format!("[a0]anull[{prog_audio}]"));
        }
        Transition::Cut => {
            let inputs: String = (0..n).map(|i| format!("[v{i}][a{i}]")).collect();
            chains.push(format!("{inputs}concat=n={n}:v=1:a=1[vout][{prog_audio}]"));
        }
        Transition::Xfade { name, secs } => {
            let d = *secs;
            let mut acc = segments[0].duration;
            let mut prev_v = "v0".to_string();
            let mut prev_a = "a0".to_string();
            for (j, seg) in segments.iter().enumerate().skip(1) {
                let offset = acc - d;
                acc += seg.duration - d;
                let last = j == n - 1;
                let out_v = if last {
                    "vout".to_string()
                } else {
                    format!("vx{j}")
                };
                let out_a = if last {
                    prog_audio.to_string()
                } else {
                    format!("ax{j}")
                };
                chains.push(format!(
                    "[{prev_v}][v{j}]xfade=transition={name}:duration={d}:offset={o}[{out_v}]",
                    d = fsecs(d),
                    o = fsecs(offset),
                ));
                chains.push(format!(
                    "[{prev_a}][a{j}]acrossfade=d={d}[{out_a}]",
                    d = fsecs(d),
                ));
                prev_v = out_v;
                prev_a = out_a;
            }
        }
    }

    if let Some(bg) = bg {
        let bg_idx = n;
        let total = timeline_secs(segments, transition);
        match bg.mode {
            AudioMode::Mix => {
                chains.push(format!(
                    "[{bg_idx}:a]{}volume={BG_MIX_VOLUME}[bgaud]",
                    track_chain(bg.start, None)
                ));
                chains.push(
                    "[aprog][bgaud]amix=inputs=2:duration=first:normalize=0[aout]".to_string(),
                );
            }
            AudioMode::Replace => {
                // The segments' own audio is still built (concat/acrossfade
                // need it) but goes nowhere; the track is cut to the picture.
                chains.push("[aprog]anullsink".to_string());
                chains.push(format!(
                    "[{bg_idx}:a]{}anull[aout]",
                    track_chain(bg.start, Some(total))
                ));
            }
        }
    }

    chains.join(";")
}

/// Pure: the leading filters every background track gets — a common sample
/// format, then an optional window starting `start` seconds in and lasting
/// `len`. Ends with a `,` so the caller appends the final filter.
fn track_chain(start: f64, len: Option<f64>) -> String {
    let mut chain =
        String::from("aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=stereo,");
    let window = match (start > 0.0, len) {
        (_, Some(len)) => Some(format!(
            "atrim=start={}:end={}",
            fsecs(start),
            fsecs(start + len)
        )),
        (true, None) => Some(format!("atrim=start={}", fsecs(start))),
        (false, None) => None,
    };
    if let Some(window) = window {
        chain.push_str(&window);
        chain.push_str(",asetpts=PTS-STARTPTS,");
    }
    chain
}

/// Pure: length of the stitched picture — each transition overlaps its two
/// neighbours, so it shortens the sum of the segments.
fn timeline_secs(segments: &[Segment], transition: &Transition) -> f64 {
    let sum: f64 = segments.iter().map(|s| s.duration).sum();
    match transition {
        Transition::Cut => sum,
        Transition::Xfade { secs, .. } => sum - secs * segments.len().saturating_sub(1) as f64,
    }
}

/// Pure: argv for the second pass of `loop_video` — repeat the rendered
/// timeline (stream-copied, no re-encode) under the track for `secs`.
fn build_loop_args(timeline: &Path, bg: &BackgroundAudio, secs: f64, out: &Path) -> Vec<String> {
    let graph = match bg.mode {
        AudioMode::Mix => format!(
            "[1:a]{}volume={BG_MIX_VOLUME}[bgaud];\
             [0:a][bgaud]amix=inputs=2:duration=first:normalize=0[aout]",
            track_chain(bg.start, None)
        ),
        AudioMode::Replace => format!("[1:a]{}anull[aout]", track_chain(bg.start, Some(secs))),
    };
    let mut args: Vec<String> = ["-y", "-hide_banner", "-loglevel", "error", "-nostdin"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend([
        "-stream_loop".into(),
        "-1".into(),
        "-i".into(),
        timeline.display().to_string(),
        "-i".into(),
        bg.path.display().to_string(),
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "0:v".into(),
        "-map".into(),
        "[aout]".into(),
        "-c:v".into(),
        "copy".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-t".into(),
        fsecs(secs),
        "-movflags".into(),
        "+faststart".into(),
        out.display().to_string(),
    ]);
    args
}

/// How a `background_audio` track sits against the segments' own audio.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AudioMode {
    /// Ducked under the segments' own audio.
    Mix,
    /// The only sound; the segments' own audio is dropped.
    Replace,
}

#[derive(Debug, Clone, PartialEq)]
struct BackgroundAudio {
    path: PathBuf,
    mode: AudioMode,
    /// Seconds into the track where playback starts.
    start: f64,
    /// Probed length of the whole track.
    duration: f64,
    loop_video: bool,
}

/// Pure: read the `background_audio` options for a track of `duration` seconds.
fn parse_background_audio(
    spec: &Value,
    path: PathBuf,
    duration: f64,
) -> Result<BackgroundAudio, String> {
    let mode = match spec["mode"].as_str().unwrap_or("mix") {
        "mix" => AudioMode::Mix,
        "replace" => AudioMode::Replace,
        other => return Err(format!("unknown mode '{other}'; use 'mix' or 'replace'")),
    };
    let start = spec["start_secs"].as_f64().unwrap_or(0.0).max(0.0);
    if start > 0.0 && start >= duration {
        return Err(format!(
            "start_secs {start:.1} is past the end of the {duration:.1}s track"
        ));
    }
    Ok(BackgroundAudio {
        path,
        mode,
        start,
        duration,
        loop_video: spec["loop_video"].as_bool().unwrap_or(false),
    })
}

/// What the model is told about the soundtrack of the result, so it reports
/// the mix it actually made rather than the one it intended.
fn audio_summary(bg: Option<&BackgroundAudio>) -> &'static str {
    match bg.map(|b| b.mode) {
        None => "the segments' own audio",
        Some(AudioMode::Mix) => {
            "background_audio mixed quietly UNDER the segments' own audio (both are audible); \
             use mode 'replace' for the track alone"
        }
        Some(AudioMode::Replace) => "background_audio only; the segments' own audio was dropped",
    }
}

/// Pure: the `[i:v]…[v{i}]` chain. `target` scales+pads onto the canvas and
/// pins fps; `None` keeps the source geometry (trim). Optional trim and
/// from/to-black fades wrap the body.
fn video_chain(i: usize, seg: &Segment, target: Option<&Target>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some((start, end)) = seg.trim {
        parts.push(format!("trim=start={}:end={}", fsecs(start), fsecs(end)));
        parts.push("setpts=PTS-STARTPTS".into());
    }
    if let Some(t) = target {
        parts.push(format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease",
            w = t.w,
            h = t.h
        ));
        parts.push(format!(
            "pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
            w = t.w,
            h = t.h
        ));
        parts.push("setsar=1".into());
        parts.push(format!("fps={}", t.fps));
    } else {
        parts.push("scale=trunc(iw/2)*2:trunc(ih/2)*2".into());
    }
    parts.push("format=yuv420p".into());
    if seg.fade_in > 0.0 {
        parts.push(format!("fade=t=in:st=0:d={}", fsecs(seg.fade_in)));
    }
    if seg.fade_out > 0.0 {
        parts.push(format!(
            "fade=t=out:st={}:d={}",
            fsecs((seg.duration - seg.fade_out).max(0.0)),
            fsecs(seg.fade_out)
        ));
    }
    format!("[{i}:v]{}[v{i}]", parts.join(","))
}

/// Pure: the `[i:a]…[a{i}]` chain. Real audio is trimmed/faded to match the
/// video; audio-less inputs get duration-matched silence so the join filters
/// always see an audio stream.
fn audio_chain(i: usize, seg: &Segment) -> String {
    if !seg.has_audio {
        return format!(
            "anullsrc=channel_layout=stereo:sample_rate=44100,atrim=duration={d},\
             asetpts=PTS-STARTPTS,aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=stereo[a{i}]",
            d = fsecs(seg.duration)
        );
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some((start, end)) = seg.trim {
        parts.push(format!("atrim=start={}:end={}", fsecs(start), fsecs(end)));
    }
    parts.push("aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=stereo".into());
    parts.push("asetpts=PTS-STARTPTS".into());
    if seg.fade_in > 0.0 {
        parts.push(format!("afade=t=in:st=0:d={}", fsecs(seg.fade_in)));
    }
    if seg.fade_out > 0.0 {
        parts.push(format!(
            "afade=t=out:st={}:d={}",
            fsecs((seg.duration - seg.fade_out).max(0.0)),
            fsecs(seg.fade_out)
        ));
    }
    format!("[{i}:a]{}[a{i}]", parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(duration: f64, has_audio: bool) -> Segment {
        Segment {
            kind: MediaKind::Video,
            path: PathBuf::from(format!("v{duration}.mp4")),
            duration,
            has_audio,
            trim: None,
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }

    fn image(duration: f64) -> Segment {
        Segment {
            kind: MediaKind::Image,
            path: PathBuf::from("slide.png"),
            duration,
            has_audio: false,
            trim: None,
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }

    fn track(mode: AudioMode, start: f64) -> BackgroundAudio {
        BackgroundAudio {
            path: PathBuf::from("/tmp/bg.mp3"),
            mode,
            start,
            duration: 60.0,
            loop_video: false,
        }
    }

    fn target() -> Target {
        Target {
            w: 1280,
            h: 720,
            fps: 30,
        }
    }

    #[test]
    fn sniff_classifies_common_magics() {
        assert_eq!(sniff_ext(&[0x89, b'P', b'N', b'G', 0x0D]), "png");
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(sniff_ext(b"GIF89a"), "gif");
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        webp.extend_from_slice(b"VP8 ");
        assert_eq!(sniff_ext(&webp), "webp");
        let mp4 = [
            0u8, 0, 0, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
        ];
        assert_eq!(sniff_ext(&mp4), "mp4");
        assert_eq!(sniff_ext(b"unknown bytes"), "mp4");
    }

    #[test]
    fn is_image_only_for_image_magics() {
        assert!(is_image(&[0x89, b'P', b'N', b'G']));
        assert!(is_image(&[0xFF, 0xD8, 0xFF]));
        assert!(!is_image(&[0u8, 0, 0, 0x18, b'f', b't', b'y', b'p']));
    }

    #[test]
    fn resolve_transition_maps_and_clamps() {
        assert_eq!(
            resolve_transition("none", 1.0, 5.0).unwrap(),
            Transition::Cut
        );
        assert_eq!(
            resolve_transition("crossfade", 0.5, 8.0).unwrap(),
            Transition::Xfade {
                name: "fade",
                secs: 0.5
            }
        );
        assert_eq!(
            resolve_transition("dissolve", 5.0, 2.0).unwrap(),
            Transition::Xfade {
                name: "dissolve",
                secs: 1.0
            }
        );
        assert!(resolve_transition("warp", 0.5, 8.0).is_err());
        assert!(resolve_transition("fade", 0.5, 0.05).is_err());
    }

    #[test]
    fn parse_video_edit_trims_clamps_and_drops_full_window() {
        let full = parse_video_edit(&json!({}), 10.0).unwrap();
        assert_eq!(full.trim, None);
        assert_eq!(full.duration, 10.0);

        let windowed = parse_video_edit(&json!({ "start_secs": 3, "end_secs": 8 }), 10.0).unwrap();
        assert_eq!(windowed.trim, Some((3.0, 8.0)));
        assert_eq!(windowed.duration, 5.0);

        // end past source clamps to source.
        let clamped = parse_video_edit(&json!({ "start_secs": 2, "end_secs": 99 }), 10.0).unwrap();
        assert_eq!(clamped.trim, Some((2.0, 10.0)));

        assert!(parse_video_edit(&json!({ "start_secs": 8, "end_secs": 4 }), 10.0).is_err());
    }

    #[test]
    fn parse_fades_clamps_to_window() {
        assert_eq!(
            parse_fades(&json!({ "fade_in_secs": 1, "fade_out_secs": 2 }), 6.0),
            (1.0, 2.0)
        );
        // fade_out can't exceed what fade_in leaves.
        assert_eq!(
            parse_fades(&json!({ "fade_in_secs": 5, "fade_out_secs": 5 }), 6.0),
            (5.0, 1.0)
        );
        assert_eq!(parse_fades(&json!({}), 6.0), (0.0, 0.0));
    }

    #[test]
    fn cut_uses_concat_over_normalized_pairs() {
        let segs = [video(3.0, true), image(2.0)];
        let graph = build_filtergraph(&segs, &Transition::Cut, Some(&target()), None);
        assert!(graph.contains("[0:v]scale=1280:720:force_original_aspect_ratio=decrease"));
        assert!(
            graph.contains("anullsrc=channel_layout=stereo:sample_rate=44100,atrim=duration=2.000")
        );
        assert!(graph.contains("[0:a]aformat=sample_fmts=fltp"));
        assert!(graph.contains("[v0][a0][v1][a1]concat=n=2:v=1:a=1[vout][aout]"));
    }

    #[test]
    fn xfade_offsets_accumulate_from_durations() {
        let segs = [video(4.0, true), video(6.0, true), video(5.0, true)];
        let transition = Transition::Xfade {
            name: "fade",
            secs: 1.0,
        };
        let graph = build_filtergraph(&segs, &transition, Some(&target()), None);
        assert!(graph.contains("[v0][v1]xfade=transition=fade:duration=1.000:offset=3.000[vx1]"));
        assert!(graph.contains("[vx1][v2]xfade=transition=fade:duration=1.000:offset=8.000[vout]"));
        assert!(graph.contains("[a0][a1]acrossfade=d=1.000[ax1]"));
        assert!(graph.contains("[ax1][a2]acrossfade=d=1.000[aout]"));
    }

    #[test]
    fn background_audio_mixes_into_program_audio() {
        let segs = [video(3.0, true), video(3.0, true)];
        let bg = track(AudioMode::Mix, 0.0);
        let graph = build_filtergraph(&segs, &Transition::Cut, Some(&target()), Some(&bg));
        assert!(graph.contains("concat=n=2:v=1:a=1[vout][aprog]"));
        assert!(graph.contains("[2:a]aformat=sample_fmts=fltp"));
        assert!(graph.contains("channel_layouts=stereo,volume=0.35[bgaud]"));
        assert!(graph.contains("[aprog][bgaud]amix=inputs=2:duration=first:normalize=0[aout]"));
    }

    #[test]
    fn replace_mode_drops_program_audio_and_cuts_track_to_the_picture() {
        let segs = [video(3.0, true), video(5.0, true)];
        let transition = Transition::Xfade {
            name: "fade",
            secs: 1.0,
        };
        let bg = track(AudioMode::Replace, 12.0);
        let graph = build_filtergraph(&segs, &transition, Some(&target()), Some(&bg));
        assert!(graph.contains("[aprog]anullsink"));
        assert!(!graph.contains("amix"));
        // picture = 3 + 5 − 1 overlap = 7s, taken from 12s into the track.
        assert!(graph.contains("atrim=start=12.000:end=19.000,asetpts=PTS-STARTPTS,anull[aout]"));
    }

    #[test]
    fn single_segment_with_replaced_audio_builds_a_connected_graph() {
        let segs = [video(8.0, true)];
        let bg = track(AudioMode::Replace, 0.0);
        let graph = build_filtergraph(&segs, &Transition::Cut, Some(&target()), Some(&bg));
        assert!(graph.contains("[v0]null[vout]"));
        assert!(graph.contains("[a0]anull[aprog];[aprog]anullsink"));
        assert!(graph.contains("[1:a]aformat="));
        assert!(graph.contains("atrim=start=0.000:end=8.000"));
    }

    #[test]
    fn background_audio_options_parse_and_reject_bad_input() {
        let parse = |spec: Value| parse_background_audio(&spec, PathBuf::from("/t/a.mp3"), 60.0);
        let d = parse(json!({ "artifact_id": "art_1" })).unwrap();
        assert_eq!(
            (d.mode, d.start, d.loop_video),
            (AudioMode::Mix, 0.0, false)
        );
        let r =
            parse(json!({ "mode": "replace", "start_secs": 12.5, "loop_video": true })).unwrap();
        assert_eq!(
            (r.mode, r.start, r.loop_video),
            (AudioMode::Replace, 12.5, true)
        );
        assert!(parse(json!({ "mode": "mute" })).is_err());
        assert!(parse(json!({ "start_secs": 60.0 })).is_err());
    }

    #[test]
    fn loop_args_copy_the_looped_timeline_under_the_track() {
        let bg = track(AudioMode::Replace, 2.0);
        let args = build_loop_args(
            Path::new("/t/timeline.mp4"),
            &bg,
            30.0,
            Path::new("/t/o.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-stream_loop -1 -i /t/timeline.mp4 -i /tmp/bg.mp3"));
        assert!(joined.contains("atrim=start=2.000:end=32.000"));
        assert!(joined.contains("-map 0:v -map [aout] -c:v copy"));
        assert!(joined.contains("-t 30.000"));
    }

    #[test]
    fn video_chain_applies_trim_and_fade() {
        let seg = Segment {
            trim: Some((3.0, 8.0)),
            duration: 5.0,
            fade_in: 0.5,
            fade_out: 1.0,
            ..video(5.0, true)
        };
        let chain = video_chain(0, &seg, Some(&target()));
        assert!(chain.starts_with("[0:v]trim=start=3.000:end=8.000,setpts=PTS-STARTPTS,scale="));
        assert!(chain.contains("fade=t=in:st=0:d=0.500"));
        // fade-out starts at duration - fade_out = 4.0.
        assert!(chain.contains("fade=t=out:st=4.000:d=1.000"));
    }

    #[test]
    fn audio_chain_applies_trim_and_fade() {
        let seg = Segment {
            trim: Some((3.0, 8.0)),
            duration: 5.0,
            fade_in: 0.5,
            fade_out: 1.0,
            ..video(5.0, true)
        };
        let chain = audio_chain(0, &seg);
        assert!(chain.starts_with("[0:a]atrim=start=3.000:end=8.000,aformat="));
        assert!(chain.contains("afade=t=in:st=0:d=0.500"));
        assert!(chain.contains("afade=t=out:st=4.000:d=1.000"));
    }

    #[test]
    fn trim_graph_preserves_source_geometry() {
        let seg = Segment {
            trim: Some((1.0, 4.0)),
            duration: 3.0,
            ..video(3.0, true)
        };
        let out = PathBuf::from("/tmp/clip.mp4");
        let args = build_ffmpeg_args(
            std::slice::from_ref(&seg),
            &Transition::Cut,
            None,
            None,
            &out,
        );
        let joined = args.join(" ");
        assert!(joined.contains("trim=start=1.000:end=4.000"));
        assert!(joined.contains("format=yuv420p"));
        // Source geometry is preserved (only an even-dimension guard, no canvas).
        assert!(joined.contains("scale=trunc(iw/2)*2:trunc(ih/2)*2"));
        assert!(!joined.contains("force_original_aspect_ratio"));
        assert!(!joined.contains("pad="));
        assert!(!joined.contains("fps="));
        // A single segment passes through (concat=n=1 is rejected by ffmpeg).
        assert!(joined.contains("[v0]null[vout]"));
        assert!(joined.contains("[a0]anull[aout]"));
        assert!(!joined.contains("concat="));
    }

    #[test]
    fn args_loop_images_and_pass_through_videos() {
        let segs = [video(3.0, true), image(2.0)];
        let out = PathBuf::from("/tmp/out.mp4");
        let args = build_ffmpeg_args(&segs, &Transition::Cut, Some(&target()), None, &out);
        let joined = args.join(" ");
        assert!(joined.contains("-i v3.mp4"));
        assert!(joined.contains("-loop 1 -t 2.000 -i slide.png"));
        assert!(joined.contains("-map [vout] -map [aout]"));
        assert!(joined.contains("-c:v libx264"));
        assert!(joined.ends_with("/tmp/out.mp4"));
        assert!(!joined.contains("-stream_loop"));
    }

    #[test]
    fn args_add_stream_looped_background_audio() {
        let segs = [video(3.0, true), video(3.0, true)];
        let out = PathBuf::from("/tmp/out.mp4");
        let bg = track(AudioMode::Mix, 0.0);
        let args = build_ffmpeg_args(&segs, &Transition::Cut, Some(&target()), Some(&bg), &out);
        assert!(args.join(" ").contains("-stream_loop -1 -i /tmp/bg.mp3"));
    }

    #[test]
    fn target_for_maps_known_aspects() {
        assert_eq!(
            target_for("9:16"),
            Target {
                w: 720,
                h: 1280,
                fps: 30
            }
        );
        assert_eq!(
            target_for("1:1"),
            Target {
                w: 720,
                h: 720,
                fps: 30
            }
        );
        assert_eq!(
            target_for("16:9"),
            Target {
                w: 1280,
                h: 720,
                fps: 30
            }
        );
        assert_eq!(
            target_for("weird"),
            Target {
                w: 1280,
                h: 720,
                fps: 30
            }
        );
    }

    fn generate_input(args: &[&str]) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(args)
            .status()
            .expect("spawn ffmpeg for fixture");
        assert!(status.success(), "fixture generation failed: {args:?}");
    }

    fn small_target() -> Target {
        Target {
            w: 320,
            h: 240,
            fps: 30,
        }
    }

    /// End-to-end against real ffmpeg, driving the actual `build_ffmpeg_args` →
    /// `run_ffmpeg` (not a hand-written graph). Covers an image still flowing
    /// THROUGH an `xfade` transition. Self-skips where ffmpeg is absent.
    #[tokio::test]
    async fn e2e_image_through_crossfade_renders() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let still = dir.path().join("still.png");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-shortest",
            "-pix_fmt",
            "yuv420p",
            clip.to_str().unwrap(),
        ]);
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:size=320x240:duration=1",
            "-frames:v",
            "1",
            still.to_str().unwrap(),
        ]);
        let segments = vec![
            Segment {
                path: clip,
                ..video(2.0, true)
            },
            Segment {
                path: still,
                ..image(1.5)
            },
        ];
        let transition = Transition::Xfade {
            name: "fade",
            secs: 0.5,
        };
        let out = dir.path().join("out.mp4");
        let args = build_ffmpeg_args(&segments, &transition, Some(&small_target()), None, &out);
        run_ffmpeg(&args, RENDER_TIMEOUT)
            .await
            .expect("compose render");
        let probe = probe_media(&out).await.expect("probe output");
        // total = 2.0 + 1.5 − 0.5 overlap = 3.0
        assert!(
            (probe.duration - 3.0).abs() < 0.3,
            "duration {}",
            probe.duration
        );
        assert!(probe.has_video && probe.has_audio);
    }

    /// End-to-end: an `xfade` transition WITH a background-audio bed — the
    /// `[aprog] → amix → [aout]` path no other test exercises.
    #[tokio::test]
    async fn e2e_crossfade_with_background_audio_renders() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let music = dir.path().join("music.mp3");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-shortest",
            "-pix_fmt",
            "yuv420p",
            clip.to_str().unwrap(),
        ]);
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=220:duration=3",
            music.to_str().unwrap(),
        ]);
        let segments = vec![
            Segment {
                path: clip.clone(),
                ..video(2.0, true)
            },
            Segment {
                path: clip,
                ..video(2.0, true)
            },
        ];
        let transition = Transition::Xfade {
            name: "fade",
            secs: 0.5,
        };
        let out = dir.path().join("out.mp4");
        let bg = BackgroundAudio {
            path: music,
            ..track(AudioMode::Mix, 0.0)
        };
        let args = build_ffmpeg_args(
            &segments,
            &transition,
            Some(&small_target()),
            Some(&bg),
            &out,
        );
        run_ffmpeg(&args, RENDER_TIMEOUT)
            .await
            .expect("compose render with bg");
        let probe = probe_media(&out).await.expect("probe output");
        // total = 2.0 + 2.0 − 0.5 overlap = 3.5; bg trimmed to program length.
        assert!(
            (probe.duration - 3.5).abs() < 0.3,
            "duration {}",
            probe.duration
        );
        assert!(probe.has_audio);
    }

    /// Loudest sample of a file's audio in dBFS, via ffmpeg's `volumedetect`.
    fn max_volume_db(path: &Path) -> f64 {
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-nostdin", "-i"])
            .arg(path)
            .args(["-vn", "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .expect("run volumedetect");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .find(|l| l.contains("max_volume:"))
            .unwrap_or_else(|| panic!("no max_volume in: {stderr}"));
        let value = line.split("max_volume:").nth(1).unwrap().trim();
        value.trim_end_matches(" dB").parse().expect("dB value")
    }

    /// A loud clip and a SILENT track, so what is audible in the result says
    /// which of the two survived.
    fn loud_clip_and_silent_track(dir: &Path) -> (PathBuf, PathBuf) {
        let clip = dir.join("clip.mp4");
        let track = dir.join("track.wav");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-shortest",
            "-pix_fmt",
            "yuv420p",
            clip.to_str().unwrap(),
        ]);
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=stereo:sample_rate=44100",
            "-t",
            "5",
            track.to_str().unwrap(),
        ]);
        (clip, track)
    }

    /// End-to-end: ONE clip whose own audio is loud, with a silent track in
    /// `replace` mode, must come out silent — the clip's audio is gone. The
    /// same inputs in `mix` mode stay loud, so the silence is the mode's doing.
    #[tokio::test]
    async fn e2e_replace_mode_silences_the_clips_own_audio() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (clip, music) = loud_clip_and_silent_track(dir.path());
        let segments = vec![Segment {
            path: clip,
            ..video(2.0, true)
        }];
        let render = |mode: AudioMode, name: &str| {
            let bg = BackgroundAudio {
                path: music.clone(),
                duration: 5.0,
                ..track(mode, 0.0)
            };
            let out = dir.path().join(name);
            let args = build_ffmpeg_args(
                &segments,
                &Transition::Cut,
                Some(&small_target()),
                Some(&bg),
                &out,
            );
            (args, out)
        };

        let (args, replaced) = render(AudioMode::Replace, "replaced.mp4");
        run_ffmpeg(&args, RENDER_TIMEOUT)
            .await
            .expect("replace render");
        let probe = probe_media(&replaced).await.expect("probe output");
        assert!(probe.has_video && probe.has_audio);
        assert!(
            (probe.duration - 2.0).abs() < 0.3,
            "duration {}",
            probe.duration
        );
        let db = max_volume_db(&replaced);
        assert!(db < -60.0, "clip audio survived replace: {db} dB");

        let (args, mixed) = render(AudioMode::Mix, "mixed.mp4");
        run_ffmpeg(&args, RENDER_TIMEOUT).await.expect("mix render");
        let db = max_volume_db(&mixed);
        assert!(db > -30.0, "mix lost the clip audio: {db} dB");
    }

    /// End-to-end, the shape of the request that prompted `replace`: several
    /// loud clips crossfaded, with an MP4 (picture + silent audio) as the track,
    /// started part-way in. Only the track's audio may reach the output.
    #[tokio::test]
    async fn e2e_crossfaded_clips_take_their_audio_from_an_mp4_track() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (clip, _) = loud_clip_and_silent_track(dir.path());
        let music = dir.path().join("track.mp4");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=6",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=channel_layout=stereo:sample_rate=44100",
            "-shortest",
            "-pix_fmt",
            "yuv420p",
            music.to_str().unwrap(),
        ]);
        let segments: Vec<Segment> = (0..3)
            .map(|_| Segment {
                path: clip.clone(),
                ..video(2.0, true)
            })
            .collect();
        let transition = Transition::Xfade {
            name: "fade",
            secs: 0.5,
        };
        let bg = BackgroundAudio {
            path: music,
            duration: 6.0,
            ..track(AudioMode::Replace, 1.0)
        };
        let out = dir.path().join("out.mp4");
        let args = build_ffmpeg_args(
            &segments,
            &transition,
            Some(&small_target()),
            Some(&bg),
            &out,
        );
        run_ffmpeg(&args, RENDER_TIMEOUT)
            .await
            .expect("replace render");
        let probe = probe_media(&out).await.expect("probe output");
        // total = 3 × 2.0 − 2 × 0.5 overlap = 5.0
        assert!(
            (probe.duration - 5.0).abs() < 0.3,
            "duration {}",
            probe.duration
        );
        assert!(probe.has_video && probe.has_audio);
        let db = max_volume_db(&out);
        assert!(db < -60.0, "clip audio survived replace: {db} dB");
    }

    /// End-to-end: `loop_video` repeats a 2s timeline under a 5s track — the
    /// output runs the length of the audio and carries only the track.
    #[tokio::test]
    async fn e2e_loop_video_runs_the_length_of_the_track() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (clip, music) = loud_clip_and_silent_track(dir.path());
        let segments = vec![Segment {
            path: clip,
            ..video(2.0, true)
        }];
        let timeline = dir.path().join("timeline.mp4");
        let args = build_ffmpeg_args(
            &segments,
            &Transition::Cut,
            Some(&small_target()),
            None,
            &timeline,
        );
        run_ffmpeg(&args, RENDER_TIMEOUT)
            .await
            .expect("timeline render");

        let bg = BackgroundAudio {
            path: music,
            duration: 5.0,
            loop_video: true,
            ..track(AudioMode::Replace, 0.0)
        };
        let out = dir.path().join("out.mp4");
        let args = build_loop_args(&timeline, &bg, 5.0, &out);
        run_ffmpeg(&args, RENDER_TIMEOUT)
            .await
            .expect("loop render");
        let probe = probe_media(&out).await.expect("probe output");
        assert!(probe.has_video && probe.has_audio);
        assert!(
            (probe.duration - 5.0).abs() < 0.3,
            "duration {}",
            probe.duration
        );
        let db = max_volume_db(&out);
        assert!(db < -60.0, "timeline audio survived the loop: {db} dB");
    }

    #[test]
    fn gif_spec_defaults_and_clamps() {
        let d = parse_gif_spec(&json!({})).unwrap();
        assert_eq!((d.fps, d.width), (GIF_DEFAULT_FPS, GIF_DEFAULT_WIDTH));
        assert!(!d.has_captions());
        assert_eq!((d.font_size, d.color.as_str()), (None, "white"));

        // Explicit font knobs: size is clamped, colour validated.
        let f = parse_gif_spec(&json!({ "font_size": 9999, "font_color": "gold" })).unwrap();
        assert_eq!(
            (f.font_size, f.color.as_str()),
            (Some(CAPTION_MAX_FONT), "gold")
        );
        assert!(parse_gif_spec(&json!({ "font_color": "not-a-colour" })).is_err());

        assert_eq!(
            parse_gif_spec(&json!({ "fps": 1 })).unwrap().fps,
            GIF_MIN_FPS
        );
        assert_eq!(
            parse_gif_spec(&json!({ "fps": 999 })).unwrap().fps,
            GIF_MAX_FPS
        );
        assert_eq!(
            parse_gif_spec(&json!({ "width": 10 })).unwrap().width,
            GIF_MIN_WIDTH
        );
        assert_eq!(
            parse_gif_spec(&json!({ "width": 9999 })).unwrap().width,
            GIF_MAX_WIDTH
        );
    }

    #[test]
    fn gif_spec_sanitizes_captions() {
        let spec =
            parse_gif_spec(&json!({ "caption_top": "  Hello  ", "caption_bottom": "" })).unwrap();
        assert_eq!(spec.top.as_deref(), Some("Hello"));
        assert_eq!(spec.bottom, None);
        assert!(parse_gif_spec(&json!({ "caption_top": "🔥🔥" })).is_err());
    }

    #[test]
    fn sanitize_caption_rules() {
        assert_eq!(sanitize_caption(None).unwrap(), None);
        assert_eq!(sanitize_caption(Some("")).unwrap(), None);
        assert_eq!(sanitize_caption(Some("   ")).unwrap(), None);
        assert_eq!(
            sanitize_caption(Some("Yo 🔥")).unwrap().as_deref(),
            Some("Yo")
        );
        assert_eq!(
            sanitize_caption(Some("Café")).unwrap().as_deref(),
            Some("Café")
        );
        assert_eq!(
            sanitize_caption(Some("top\nline")).unwrap().as_deref(),
            Some("top\nline")
        );
        assert!(sanitize_caption(Some("🔥")).is_err());
        assert!(sanitize_caption(Some(&"a".repeat(GIF_MAX_CAPTION_CHARS + 1))).is_err());
    }

    #[test]
    fn gif_filtergraph_plain() {
        let g = build_gif_filtergraph(None, 12, 480, None, None, Path::new("/f.ttf"));
        assert!(g.contains("fps=12"));
        assert!(g.contains("scale=480:-1:flags=lanczos"));
        assert!(g.contains("split[s0][s1];[s0]palettegen=stats_mode=diff[p]"));
        assert!(g.contains("paletteuse=dither=bayer"));
        assert!(!g.contains("drawtext"));
        assert!(!g.contains("trim="));
    }

    #[test]
    fn gif_filtergraph_trim_and_captions() {
        let (top, bottom) = (PathBuf::from("/t/top.txt"), PathBuf::from("/t/bottom.txt"));
        let g = build_gif_filtergraph(
            Some((0.5, 1.5)),
            12,
            480,
            Some(DrawCaption {
                textfile: &top,
                fontsize: 40,
                color: "white",
            }),
            Some(DrawCaption {
                textfile: &bottom,
                fontsize: 30,
                color: "yellow",
            }),
            Path::new("/fonts/Lato-Bold.ttf"),
        );
        assert!(g.contains("trim=start=0.500:end=1.500"));
        assert!(g.contains("setpts=PTS-STARTPTS"));
        assert_eq!(g.matches("drawtext").count(), 2);
        assert!(g.contains("fontfile='/fonts/Lato-Bold.ttf'"));
        assert!(g.contains("textfile='/t/top.txt'"));
        assert!(g.contains("textfile='/t/bottom.txt'"));
        assert!(g.contains("expansion=none"));
        assert!(g.contains("text_align=center"));
        // Each caption keeps its own size and colour.
        assert!(g.contains("fontcolor=white:bordercolor=black:borderw=3:fontsize=40"));
        assert!(g.contains("fontcolor=yellow:bordercolor=black:borderw=2:fontsize=30"));
        // Top margin = fontsize/2 = 20; bottom anchored above its own margin (15).
        assert!(g.contains("x=(w-text_w)/2:y=20"));
        assert!(g.contains("y=h-text_h-15"));
    }

    #[test]
    fn ffmpeg_quote_escapes() {
        assert_eq!(ffmpeg_quote("/tmp/a.ttf"), "'/tmp/a.ttf'");
        assert_eq!(ffmpeg_quote("with space.txt"), "'with space.txt'");
        assert_eq!(ffmpeg_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn gif_args_shape() {
        let args = build_gif_args(Path::new("/in.mp4"), "GRAPH", Path::new("/out.gif"));
        assert_eq!(args.first().map(String::as_str), Some("-y"));
        assert_eq!(args.last().map(String::as_str), Some("/out.gif"));
        assert!(args.windows(2).any(|w| w == ["-vf", "GRAPH"]));
        assert!(args.windows(2).any(|w| w == ["-loop", "0"]));
        assert!(args.iter().any(|a| a == "-an"));
        assert!(args.iter().any(|a| a == "/in.mp4"));
    }

    #[tokio::test]
    async fn e2e_video_to_gif_renders() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=2",
            "-pix_fmt",
            "yuv420p",
            clip.to_str().unwrap(),
        ]);
        let out = dir.path().join("out.gif");
        let graph = build_gif_filtergraph(None, 12, 160, None, None, Path::new("unused"));
        run_ffmpeg(&build_gif_args(&clip, &graph, &out), RENDER_TIMEOUT)
            .await
            .expect("gif render");
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"GIF8"), "output is a GIF");
        assert!(probe_media(&out).await.expect("probe gif").has_video);
    }

    #[tokio::test]
    async fn e2e_gif_trim_shortens() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=3",
            "-pix_fmt",
            "yuv420p",
            clip.to_str().unwrap(),
        ]);
        let out = dir.path().join("out.gif");
        let graph =
            build_gif_filtergraph(Some((0.5, 1.5)), 10, 160, None, None, Path::new("unused"));
        run_ffmpeg(&build_gif_args(&clip, &graph, &out), RENDER_TIMEOUT)
            .await
            .expect("trimmed gif render");
        let probe = probe_media(&out).await.expect("probe gif");
        assert!(
            probe.duration < 2.0,
            "trim applied (got {}s)",
            probe.duration
        );
    }

    #[tokio::test]
    async fn e2e_gif_with_captions_renders() {
        if !ffmpeg_available() || !drawtext_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        generate_input(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30:duration=1",
            "-pix_fmt",
            "yuv420p",
            clip.to_str().unwrap(),
        ]);
        let font = crate::tools::report::fonts::font_path()
            .unwrap()
            .join("Lato-Bold.ttf");
        let top = write_caption(dir.path(), "top.txt", Some("WHEN IT")).unwrap();
        let bottom = write_caption(dir.path(), "bottom.txt", Some("COMPILES")).unwrap();
        let out = dir.path().join("out.gif");
        let graph = build_gif_filtergraph(
            None,
            10,
            200,
            top.as_deref().map(|p| DrawCaption {
                textfile: p,
                fontsize: 24,
                color: "white",
            }),
            bottom.as_deref().map(|p| DrawCaption {
                textfile: p,
                fontsize: 24,
                color: "#ff0066",
            }),
            &font,
        );
        run_ffmpeg(&build_gif_args(&clip, &graph, &out), RENDER_TIMEOUT)
            .await
            .expect("captioned gif render");
        assert!(std::fs::read(&out).unwrap().starts_with(b"GIF8"));
    }

    #[test]
    fn color_validation() {
        assert_eq!(sanitize_color(None).unwrap(), "white");
        assert_eq!(sanitize_color(Some("  ")).unwrap(), "white");
        assert_eq!(sanitize_color(Some("Yellow")).unwrap(), "yellow");
        assert_eq!(sanitize_color(Some("#FF0066")).unwrap(), "0xff0066");
        assert_eq!(sanitize_color(Some("0xabcdef")).unwrap(), "0xabcdef");
        assert!(sanitize_color(Some("chartreuse")).is_err());
        assert!(sanitize_color(Some("#12")).is_err());
        // No filtergraph metacharacters can survive validation.
        assert!(sanitize_color(Some("white:x=0")).is_err());
    }

    #[test]
    fn collapse_whitespace_kills_manual_breaks() {
        // The exact failure from the screenshot: a literal two-char backslash-n.
        assert_eq!(
            collapse_whitespace("1:\\nI WILL BUILD\\nAN EMPIRE"),
            "1: I WILL BUILD AN EMPIRE"
        );
        // Real newlines and tabs collapse too.
        assert_eq!(collapse_whitespace("a\n\nb\tc  d"), "a b c d");
    }

    #[test]
    fn plan_caption_wraps_and_shrinks() {
        let m = caption_metrics().expect("bundled font parses");
        // A long single-line caption must wrap to several lines, none wider than
        // the usable frame, and never render the words on one overflowing line.
        let long = "DAD, WHERE ARE MY SOCKS RIGHT NOW PLEASE";
        let (wrapped, fs) = plan_caption(long, 480, 480.0 * 0.5, m, None);
        let usable = 480.0 * CAPTION_WIDTH_FRAC;
        assert!(wrapped.contains('\n'), "long caption wraps: {wrapped:?}");
        for line in wrapped.split('\n') {
            assert!(
                m.line_width_px(line, fs as f32) <= usable + 1.0,
                "line {line:?} fits at fs={fs}"
            );
        }
        // A short caption keeps a big, punchy size on one line.
        let (short, short_fs) = plan_caption("OK", 480, 480.0 * 0.5, m, None);
        assert!(!short.contains('\n'));
        assert!(short_fs >= 40, "short caption stays large (got {short_fs})");
        // An explicit size is honoured even if it has to wrap.
        let (_, fixed) = plan_caption(long, 480, 480.0 * 0.5, m, Some(50));
        assert_eq!(fixed, 50);
    }
}
