//! `view_attachment` — load a file already present in the conversation into the
//! agent's view.
//!
//! The agent produced a file (a generated image, a `browser.pdf`/screenshot), or
//! a human posted one, but reading back the channel history only yields text —
//! the bytes never reach the model. This tool resolves an attachment from the
//! current thread (or channel), downloads it via the channel surface, and stages
//! it in the artifact store. A post-turn render step in the agent runner (see
//! [`workforce_agent::tools::image_view`]) picks up the staged handle and appends
//! it to the conversation as model-visible content, routing it through the same
//! per-type pipeline used for inbound attachments (image → vision, PDF →
//! text/document, audio → transcript, video → timestamped frames + transcript,
//! text/office → extracted text). A video can be re-opened for a closer look:
//! the call's [`FrameSelection`] rides the marker to the render step.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::info;

use workforce_agent::attachments::FrameSelection;
use workforce_agent::runner::{CURRENT_CHANNEL_ID, CURRENT_THREAD_ID};
use workforce_agent::tools::image_view::{stage_for_view, view_pending_note, VIEWED_IMAGES_KEY};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::channels::{ChannelFile, ChannelMessage, SurfaceRegistry};

use crate::adapters::messaging_routing::{channel_hint_from_input, platform_arg};

const VIEW_ATTACHMENT_TOOL_NAME: &str = "view_attachment";

/// Read a task-local string, treating empty as absent.
fn task_local(key: &'static tokio::task::LocalKey<String>) -> Option<String> {
    key.try_with(|v| v.clone()).ok().filter(|s| !s.is_empty())
}

/// A file attachment paired with the timestamp of the message it came from,
/// used to order candidates by recency.
struct Candidate {
    ts: f64,
    file: ChannelFile,
}

/// Collect file attachments across `messages`, newest first.
fn collect_files(messages: &[ChannelMessage]) -> Vec<Candidate> {
    let mut files: Vec<Candidate> = messages
        .iter()
        .flat_map(|m| {
            let ts = m.timestamp.parse::<f64>().unwrap_or(0.0);
            m.files.iter().map(move |f| Candidate {
                ts,
                file: f.clone(),
            })
        })
        .collect();
    files.sort_by(|a, b| b.ts.total_cmp(&a.ts));
    files
}

/// Resolve `reference` against the recency-ordered candidates. `None` selects
/// the most recent; a positive integer selects by 1-based recency index; any
/// other string matches a filename (exact then substring, case-insensitive).
fn select<'a>(files: &'a [Candidate], reference: Option<&str>) -> Result<&'a ChannelFile, String> {
    let names = || {
        files
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}: {}", i + 1, c.file.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(first) = files.first() else {
        return Err("no files found in this conversation".to_string());
    };
    let Some(reference) = reference.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(&first.file);
    };
    if let Ok(n) = reference.parse::<usize>() {
        if n >= 1 {
            return files
                .get(n - 1)
                .map(|c| &c.file)
                .ok_or_else(|| format!("only {} file(s) available — {}", files.len(), names()));
        }
    }
    let lower = reference.to_lowercase();
    files
        .iter()
        .find(|c| c.file.name.eq_ignore_ascii_case(reference))
        .or_else(|| {
            files
                .iter()
                .find(|c| c.file.name.to_lowercase().contains(&lower))
        })
        .map(|c| &c.file)
        .ok_or_else(|| format!("no file matching '{reference}' — available: {}", names()))
}

/// Loads a file from the current conversation into the agent's view.
pub struct ViewAttachmentTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
}

impl ViewAttachmentTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self { surfaces }
    }
}

#[async_trait]
impl Tool for ViewAttachmentTool {
    fn name(&self) -> &str {
        VIEW_ATTACHMENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Look at a file posted in a conversation — an image, PDF, spreadsheet, text/code file, or \
         audio/video. Use this whenever you need a file's contents, because attachments are NOT \
         visible in your text history (this includes files you created, e.g. a generated image or \
         a browser.pdf, and files shared earlier that have since dropped out of your context). It \
         downloads the file with the workspace's own authentication — you do NOT need a personal \
         OAuth token, and Slack files are reachable, not 'behind auth'. Defaults to the most \
         recent file in the current conversation; pass `reference` to pick by filename or 1-based \
         recency index. Pass `channel` to read a file from a DIFFERENT channel that YOU are a \
         member of. It appears in your view on your next step; spreadsheets/CSVs are also \
         re-staged as queryable for data.query, PDFs are read as text, audio transcribed, and \
         videos shown as timestamped still frames (plus the transcript of their audio). Asked \
         about a moment or a detail of a video, open it again for a closer look instead of \
         answering from what you saw before: `from`/`to` sample a time range, `at` takes exact \
         timestamps, `frames` sets how many, `detail` enlarges a few frames so small things are \
         legible. A closer look shows frames only; for what is said, open the video without \
         these."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "reference": {
                    "type": "string",
                    "description": "Which file to view: a filename, or a 1-based recency index \
                                    (1 = most recent). Omit for the most recent file."
                },
                "channel": {
                    "type": "string",
                    "description": "Optional channel id to look in instead of the current \
                                    conversation — use this to fetch a file posted in another \
                                    channel that the requesting user is a member of (their access \
                                    is verified)."
                },
                "thread_id": {
                    "type": "string",
                    "description": "Optional thread timestamp to scope the search to one thread \
                                    within `channel`. Only honoured together with `channel`."
                },
                "from": {
                    "type": ["string", "number"],
                    "description": "Video only: start of the time range to sample, in seconds \
                                    (`13.5`) or `m:ss` (`0:13.5`). Defaults to the start."
                },
                "to": {
                    "type": ["string", "number"],
                    "description": "Video only: end of the time range to sample. Defaults to \
                                    the end."
                },
                "at": {
                    "type": ["array", "string", "number"],
                    "items": { "type": ["string", "number"] },
                    "description": "Video only: exact timestamps to show one frame each of, \
                                    instead of a sampled range."
                },
                "frames": {
                    "type": ["integer", "string"],
                    "description": "Video only: how many frames to sample from the range, up \
                                    to the configured maximum."
                },
                "detail": {
                    "type": ["boolean", "string"],
                    "description": "Video only: larger frames, for a few timestamps or a short \
                                    range."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let selection = match FrameSelection::from_tool_input(&input) {
            Ok(selection) => selection,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };

        // An explicit `channel` reads from another conversation (the bot must
        // be a member); otherwise fall back to the current channel/thread.
        let explicit_channel = input["channel"].as_str().filter(|s| !s.trim().is_empty());
        let Some(channel) = explicit_channel
            .map(str::to_string)
            .or_else(|| task_local(&CURRENT_CHANNEL_ID))
        else {
            return Ok(ToolResult::failure(
                "view_attachment can only be used inside a conversation, or pass an explicit `channel`",
                elapsed(),
            ));
        };
        let thread = match explicit_channel {
            // For a foreign channel, only the caller-supplied thread applies —
            // the current thread belongs to a different conversation.
            Some(_) => input["thread_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            None => task_local(&CURRENT_THREAD_ID),
        };

        let surface = match self
            .surfaces
            .read()
            .await
            .resolve(
                platform_arg(&input),
                &channel,
                channel_hint_from_input(&input),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::failure(format!("{e}"), elapsed())),
        };

        // Cross-channel read guard: a foreign channel may be read only if the
        // requesting USER (not just the bot) is a member of it. No-op for the
        // current conversation.
        if let Err(msg) = crate::adapters::messaging_routing::ensure_channel_read_access(
            surface.as_ref(),
            &channel,
        )
        .await
        {
            return Ok(ToolResult::failure(msg, elapsed()));
        }

        // Deeper lookback than a normal read: a re-read in a long thread may be
        // many messages back from the latest turn.
        const LOOKBACK: usize = 200;
        let messages = match &thread {
            Some(thread) => surface.read_thread(&channel, thread, LOOKBACK).await,
            None => surface.read_messages(&channel, LOOKBACK, None).await,
        };
        let messages = match messages {
            Ok(m) => m,
            Err(e) => {
                let hint = if explicit_channel.is_some() {
                    " (is the bot a member of that channel?)"
                } else {
                    ""
                };
                return Ok(ToolResult::failure(
                    format!("failed to read conversation: {e}{hint}"),
                    elapsed(),
                ));
            }
        };

        let files = collect_files(&messages);
        // Nothing attached at all is a FINDING, not a failed action: the
        // conversation was read successfully and it contains no files. Answering
        // "you didn't attach anything" is then a verified claim, so this must not
        // reach the post-turn "these actions failed, claims may be wrong" notice
        // — that told readers to doubt an answer this very result had confirmed.
        //
        // A reference that matches nothing stays a failure on purpose: there the
        // model asked for a specific file among several, and retrying with a
        // different reference is real recovery. Here there is nothing to retry.
        if files.is_empty() {
            return Ok(ToolResult::empty(
                "no files found in this conversation",
                elapsed(),
            ));
        }
        let file = match select(&files, input["reference"].as_str()) {
            Ok(f) => f,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };

        let bytes = match surface.fetch_file(file).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to download file: {e}"),
                    elapsed(),
                ))
            }
        };

        let staged = match stage_for_view(
            VIEW_ATTACHMENT_TOOL_NAME,
            bytes,
            &file.mime_type,
            &file.name,
        ) {
            Ok(staged) => match &selection {
                Some(selection) => staged.selecting(selection),
                None => staged,
            },
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };

        info!(file = %file.name, channel = %channel, "Staged conversation file for viewing");
        Ok(ToolResult::success(
            serde_json::json!({
                VIEWED_IMAGES_KEY: staged.marker,
                "note": view_pending_note(&file.name),
            }),
            elapsed(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ts: &str, files: Vec<ChannelFile>) -> ChannelMessage {
        ChannelMessage {
            channel: "C1".into(),
            message_id: ts.into(),
            user: "U1".into(),
            user_name: None,
            channel_name: None,
            text: String::new(),
            timestamp: ts.into(),
            thread_id: None,
            files,
            bot_id: None,
        }
    }

    fn image(name: &str) -> ChannelFile {
        ChannelFile {
            id: name.into(),
            name: name.into(),
            mime_type: "image/png".into(),
            url: Some(format!("https://files.slack.com/{name}")),
            size: Some(10),
        }
    }

    fn doc(name: &str) -> ChannelFile {
        ChannelFile {
            mime_type: "application/pdf".into(),
            ..image(name)
        }
    }

    #[test]
    fn collect_files_orders_newest_first_and_includes_non_images() {
        let messages = vec![
            msg("100.0", vec![image("old.png"), doc("notes.pdf")]),
            msg("300.0", vec![image("new.png")]),
            msg("200.0", vec![doc("mid.pdf")]),
        ];
        let files = collect_files(&messages);
        let names: Vec<&str> = files.iter().map(|c| c.file.name.as_str()).collect();
        assert_eq!(names, vec!["new.png", "mid.pdf", "old.png", "notes.pdf"]);
    }

    #[test]
    fn select_defaults_to_most_recent() {
        let messages = vec![
            msg("100.0", vec![image("old.png")]),
            msg("200.0", vec![doc("new.pdf")]),
        ];
        let files = collect_files(&messages);
        assert_eq!(select(&files, None).unwrap().name, "new.pdf");
    }

    #[test]
    fn select_by_recency_index_is_one_based() {
        let messages = vec![
            msg("100.0", vec![image("old.png")]),
            msg("200.0", vec![image("new.png")]),
        ];
        let files = collect_files(&messages);
        assert_eq!(select(&files, Some("1")).unwrap().name, "new.png");
        assert_eq!(select(&files, Some("2")).unwrap().name, "old.png");
        assert!(select(&files, Some("3")).is_err());
    }

    #[test]
    fn select_by_filename_matches_exact_then_substring() {
        let messages = vec![msg("200.0", vec![doc("report-final.pdf")])];
        let files = collect_files(&messages);
        assert_eq!(
            select(&files, Some("REPORT-FINAL.PDF")).unwrap().name,
            "report-final.pdf"
        );
        assert_eq!(
            select(&files, Some("report")).unwrap().name,
            "report-final.pdf"
        );
        assert!(select(&files, Some("missing")).is_err());
    }

    #[test]
    fn a_closer_look_the_tool_reads_also_passes_argument_validation() {
        let schema =
            ViewAttachmentTool::new(Arc::new(RwLock::new(SurfaceRegistry::new()))).input_schema();
        let check = |input: &serde_json::Value| {
            workforce_agent::tools::arguments::check_arguments(&schema, input)
                .expect("the schema compiles")
        };
        for input in [
            serde_json::json!({ "from": 10, "to": "0:16", "frames": 6 }),
            serde_json::json!({ "from": 90.5, "to": "0:01:31" }),
            serde_json::json!({ "at": [13.5, "0:14"], "detail": true }),
            serde_json::json!({ "at": "0:14", "detail": "true" }),
            serde_json::json!({ "at": 14, "frames": null }),
            serde_json::json!({ "frames": "4" }),
            serde_json::json!({ "frames": 4.0 }),
        ] {
            let report = check(&input);
            assert_eq!(report.problems, [], "{input}");
            assert!(report.unknown_keys.is_empty(), "{input}");
            assert!(FrameSelection::from_tool_input(&input).is_ok(), "{input}");
        }
        for input in [
            serde_json::json!({ "from": true }),
            serde_json::json!({ "at": [[13]] }),
            serde_json::json!({ "frames": 1.5 }),
            serde_json::json!({ "detail": 1 }),
        ] {
            assert!(!check(&input).problems.is_empty(), "{input}");
            assert!(FrameSelection::from_tool_input(&input).is_err(), "{input}");
        }
    }

    #[test]
    fn select_errors_when_no_files() {
        assert!(select(&[], None).is_err());
    }
}
