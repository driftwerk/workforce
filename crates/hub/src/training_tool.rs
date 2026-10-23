//! `training_add` — invoked by the agent to ingest a file already
//! present in the current Slack thread (or any channel surface) into
//! the agent's training corpus (DW-541 follow-up, "Slack file ingest").
//!
//! Sits in `crates/hub/src/` rather than `crates/skills/` because the
//! load pipeline depends on the hub-side `TrainingStore` + `MemoryStore`
//! handles. The file selection and surface fetch mirror
//! `workforce_skills::adapters::image_view::ViewAttachmentTool` so a
//! reference like "1" or "report.pdf" behaves identically across both
//! tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use tokio::sync::RwLock;
use tracing::info;
use uuid::Uuid;

use workforce_agent::runner::{
    CURRENT_AGENT_ID, CURRENT_CHANNEL_ID, CURRENT_THREAD_ID, CURRENT_USER_ID,
};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::channels::{ChannelFile, ChannelMessage, SurfaceRegistry};
use workforce_common::settings::TrainingConfig;
use workforce_memory::store::MemoryStore;
use workforce_skills::adapters::messaging_routing::{channel_hint_from_input, platform_arg};
use workforce_training::{sanitize_filename, validate_slug, SourceKind, TrainingStore};

use crate::training::{doc_short_id, load_corpus, LoadRequest, VisionContext};

const TOOL_NAME: &str = "training_add";

/// Cap on how many recent messages we scan to find a file. Mirrors the
/// `view_attachment` window — 50 messages is enough to cover the
/// typical thread/channel without scanning unbounded history.
const READ_WINDOW: usize = 50;

pub struct TrainingAddTool {
    training: Arc<dyn TrainingStore>,
    memory: Arc<dyn MemoryStore>,
    surfaces: Arc<RwLock<SurfaceRegistry>>,
    config: TrainingConfig,
    hub_data_dir: PathBuf,
    vision: Option<VisionBundle>,
}

/// Owned form of `VisionContext` so the tool can keep the handles
/// across `execute` calls without juggling lifetimes.
struct VisionBundle {
    llm: Arc<dyn workforce_agent::vision::VisionLlm>,
    config: workforce_common::settings::LlmVisionConfig,
}

impl TrainingAddTool {
    pub fn new(
        training: Arc<dyn TrainingStore>,
        memory: Arc<dyn MemoryStore>,
        surfaces: Arc<RwLock<SurfaceRegistry>>,
        config: TrainingConfig,
        hub_data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            training,
            memory,
            surfaces,
            config,
            hub_data_dir: hub_data_dir.into(),
            vision: None,
        }
    }

    pub fn with_vision(
        mut self,
        llm: Arc<dyn workforce_agent::vision::VisionLlm>,
        config: workforce_common::settings::LlmVisionConfig,
    ) -> Self {
        self.vision = Some(VisionBundle { llm, config });
        self
    }
}

#[async_trait]
impl Tool for TrainingAddTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Ingest a file from the current conversation into your training corpus so future answers \
         can cite it. Use this when the human says \"add this to your training\", \"remember this \
         file\", \"learn from this\", or similar. The file must already be attached in the thread \
         (or channel); reference it by filename or by 1-based recency position (1 = most recent). \
         Always ask the human which corpus name to file it under if they didn't specify one — the \
         corpus groups related material together. Supported formats: JSON, PDF, DOCX, XLSX (and \
         siblings), TXT, Markdown."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "corpus": {
                    "type": "string",
                    "description": "Corpus name (slug, ASCII alphanumeric + `-`, `_`, `.`). \
                                    Reused on subsequent uploads — pick one that groups \
                                    related material together (e.g. `product-catalogue`, `onboarding`)."
                },
                "reference": {
                    "type": "string",
                    "description": "Which file to ingest: a filename, or a 1-based recency index \
                                    (1 = most recent). Omit for the most recent file in the \
                                    thread/channel."
                },
                "platform": {
                    "type": "string",
                    "description": "Platform (slack, discord, etc.). Defaults to slack."
                },
                "workspace_id": {
                    "type": "string",
                    "description": "Workspace identifier (rarely needed — auto-detected from \
                                    the current channel)."
                }
            },
            "required": ["corpus"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(corpus_raw) = input["corpus"].as_str() else {
            return Ok(ToolResult::failure(
                "missing required `corpus` field",
                elapsed(),
            ));
        };
        let corpus_name = corpus_raw.trim();
        if let Err(e) = validate_slug(corpus_name) {
            return Ok(ToolResult::failure(format!("`corpus` {e}"), elapsed()));
        }

        let Some(channel) = task_local(&CURRENT_CHANNEL_ID) else {
            return Ok(ToolResult::failure(
                "training_add can only be used inside a conversation",
                elapsed(),
            ));
        };
        let thread = task_local(&CURRENT_THREAD_ID);
        let Some(agent_id) = task_local(&CURRENT_AGENT_ID) else {
            return Ok(ToolResult::failure(
                "training_add can only be used inside a run (no agent_id in context)",
                elapsed(),
            ));
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

        let messages = match &thread {
            Some(t) => surface.read_thread(&channel, t, READ_WINDOW).await,
            None => surface.read_messages(&channel, READ_WINDOW, None).await,
        };
        let messages = match messages {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to read conversation: {e}"),
                    elapsed(),
                ));
            }
        };

        let files = collect_files(&messages);
        // See the same guard in `adapters::image_view`: an empty conversation is
        // a finding, not a failed action, and must not reach the "claims may be
        // wrong" notice. A non-matching reference stays a failure — retrying with
        // a different one is real recovery; here there is nothing to retry.
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

        let max_bytes = self.config.max_file_size_mb.saturating_mul(1024 * 1024);
        if let Some(size) = file.size.filter(|s| *s as usize > max_bytes) {
            return Ok(ToolResult::failure(
                format!(
                    "file `{}` is {} bytes which exceeds the {} MiB cap",
                    file.name, size, self.config.max_file_size_mb,
                ),
                elapsed(),
            ));
        }

        let bytes = match surface.fetch_file(file).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to download `{}`: {e}", file.name),
                    elapsed(),
                ));
            }
        };
        if bytes.len() > max_bytes {
            return Ok(ToolResult::failure(
                format!(
                    "downloaded `{}` ({} bytes) exceeds the {} MiB cap",
                    file.name,
                    bytes.len(),
                    self.config.max_file_size_mb,
                ),
                elapsed(),
            ));
        }

        let filename = sanitize_filename(&file.name);
        let storage_uri = match stash_original(
            &self.config,
            &self.hub_data_dir,
            &agent_id,
            corpus_name,
            &filename,
            &bytes,
        )
        .await
        {
            Ok(uri) => uri,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to persist original: {e}"),
                    elapsed(),
                ));
            }
        };

        let uploaded_by =
            task_local(&CURRENT_USER_ID).unwrap_or_else(|| "slack-unknown".to_string());
        let source_ref = thread.as_deref();
        let vision = self
            .vision
            .as_ref()
            .filter(|v| v.config.enabled)
            .map(|v| VisionContext {
                llm: v.llm.as_ref(),
                config: &v.config,
            });
        let req = LoadRequest {
            agent_id: &agent_id,
            corpus_name,
            original_filename: &filename,
            source_kind: SourceKind::Slack,
            source_ref,
            uploaded_by: &uploaded_by,
            bytes: &bytes,
            storage_uri: &storage_uri,
            format: None,
            vision,
        };

        let outcome =
            match load_corpus(Arc::clone(&self.training), Arc::clone(&self.memory), req).await {
                Ok(o) => o,
                Err(e) => {
                    return Ok(ToolResult::failure(format!("load failed: {e}"), elapsed()));
                }
            };

        info!(
            agent = %agent_id,
            corpus = %corpus_name,
            file = %filename,
            channel = %channel,
            written = outcome.written,
            unchanged = outcome.unchanged,
            deleted = outcome.deleted,
            "training: ingested via training_add",
        );

        Ok(ToolResult::success(
            serde_json::json!({
                "corpus": corpus_name,
                "filename": filename,
                "document_id": outcome.document_id.map(|u| u.to_string()),
                "document_short": outcome.document_id.map(doc_short_id),
                "written": outcome.written,
                "unchanged": outcome.unchanged,
                "deleted": outcome.deleted,
                "note": format!(
                    "Added `{}` to corpus `{}` ({} new chunks). Future answers will surface \
                     relevant excerpts automatically and cite the source.",
                    filename, corpus_name, outcome.written,
                ),
            }),
            elapsed(),
        ))
    }
}

/// Stash the original file under
/// `<hub_data_dir>/<training.data_dir>/<agent>/<corpus>/<uuid>__<filename>`.
/// Mirrors the HTTP + CLI direct layouts so all three ingest paths
/// share the same on-disk archive.
async fn stash_original(
    cfg: &TrainingConfig,
    hub_data_dir: &Path,
    agent: &str,
    corpus: &str,
    filename: &str,
    bytes: &[u8],
) -> eyre::Result<String> {
    let raw = Path::new(&cfg.data_dir);
    let root = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        hub_data_dir.join(raw)
    };
    let dest_dir = root.join(agent).join(corpus);
    tokio::fs::create_dir_all(&dest_dir).await?;
    let stamped = format!("{}__{}", Uuid::new_v4().simple(), filename);
    let dest = dest_dir.join(stamped);
    tokio::fs::write(&dest, bytes).await?;
    Ok(format!("file://{}", dest.display()))
}

fn task_local(key: &'static tokio::task::LocalKey<String>) -> Option<String> {
    key.try_with(|v| v.clone()).ok().filter(|s| !s.is_empty())
}

struct Candidate {
    ts: f64,
    file: ChannelFile,
}

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

    fn file(name: &str, mime: &str) -> ChannelFile {
        ChannelFile {
            id: name.into(),
            name: name.into(),
            mime_type: mime.into(),
            url: Some(format!("https://files.slack.com/{name}")),
            size: Some(10),
        }
    }

    #[test]
    fn select_defaults_to_most_recent_then_matches_by_name_or_index() {
        let messages = vec![
            msg(
                "100.0",
                vec![
                    file("old.json", "application/json"),
                    file("notes.pdf", "application/pdf"),
                ],
            ),
            msg("300.0", vec![file("new.pdf", "application/pdf")]),
            msg("200.0", vec![file("mid.docx", "application/octet-stream")]),
        ];
        let files = collect_files(&messages);

        assert_eq!(select(&files, None).unwrap().name, "new.pdf");
        assert_eq!(select(&files, Some("1")).unwrap().name, "new.pdf");
        assert_eq!(select(&files, Some("mid.docx")).unwrap().name, "mid.docx");
        assert_eq!(select(&files, Some("OLD")).unwrap().name, "old.json");
        assert!(select(&files, Some("99")).is_err());
        assert!(select(&files, Some("nothing")).is_err());
    }

    #[test]
    fn select_errors_when_no_files() {
        assert!(select(&[], None).is_err());
    }
}
