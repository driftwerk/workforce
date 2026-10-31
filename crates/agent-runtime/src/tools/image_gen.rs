//! `generate_image` — text-to-image generation as an agent tool.
//!
//! The tool calls the configured [`ImageProvider`], offloads the resulting
//! bytes to the artifact store, and returns only a small handle
//! (`artifact_id` + media type). Image bytes never enter the LLM context — a
//! deliberate guard against transcript bloat. A post-turn delivery step
//! (see the agent runner) picks up these handles and routes the bytes to the
//! active channel surface: a native Slack file upload, or a gateway blob
//! attachment for the iOS client.
//!
//! The output shape is the contract the delivery step keys on: a
//! `generated_images` array of `{ artifact_id, media_type }` entries.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmImageGenerationConfig;

use super::registry::ToolRegistry;
use super::{Tool, ToolResult};
use crate::artifacts::store::global_store;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType};
use crate::llm::image::{provider_from_config, ImageProvider, ImageRequest, ImageSize, InputImage};
use crate::llm::provider::ProviderKind;
use crate::runner::SessionContext;

/// Resolve one image-input object into an [`InputImage`]: either
/// `{ "artifact_id": "art_…" }` (a prior generation or a
/// `view_attachment`/`get_user_photo` result, read back from the store) or
/// `{ "data_base64": "…" }` (inline). `media_type` is honoured when given, else
/// sniffed from the bytes. `label` prefixes errors (e.g. `"images[0]"`).
fn resolve_image_input(entry: &Value, agent_id: &str, label: &str) -> Result<InputImage, String> {
    let bytes = if let Some(id_str) = entry.get("artifact_id").and_then(Value::as_str) {
        let id = ArtifactId::parse(id_str)
            .map_err(|e| format!("{label}: invalid artifact_id '{id_str}': {e}"))?;
        let store =
            global_store().ok_or_else(|| format!("{label}: artifact store not configured"))?;
        // Session/user-scoped: a model could otherwise pass another user's
        // `artifact_id` (this is the one model-driven read that bypassed the
        // shared session/user gate). Returns "not found" across owners.
        match crate::artifacts::resolve::read_session_scoped(store.as_ref(), agent_id, &id) {
            Ok((ArtifactBody::Bytes(b), _)) => b,
            Ok((other, _)) => {
                return Err(format!(
                    "{label}: artifact {id_str} is {:?}, not image bytes",
                    other.content_type()
                ))
            }
            Err(e) => return Err(format!("{label}: failed to read artifact {id_str}: {e}")),
        }
    } else if let Some(b64) = entry.get("data_base64").and_then(Value::as_str) {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine;
        BASE64
            .decode(b64.trim())
            .map_err(|e| format!("{label}: data_base64 decode failed: {e}"))?
    } else {
        return Err(format!("{label}: needs an 'artifact_id' or 'data_base64'"));
    };
    let media_type = entry
        .get("media_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::llm::image::sniff_media_type(&bytes).to_string());
    Ok(InputImage {
        data: bytes,
        media_type,
    })
}

/// Resolve the `images` input array into reference images, preserving order.
/// Shared with `generate_video`'s input images.
pub(crate) fn resolve_reference_images(
    images: &Value,
    agent_id: &str,
) -> Result<Vec<InputImage>, String> {
    let Some(arr) = images.as_array() else {
        return Ok(Vec::new());
    };
    arr.iter()
        .enumerate()
        .map(|(i, entry)| resolve_image_input(entry, agent_id, &format!("images[{i}]")))
        .collect()
}

/// Registry name of the image-generation tool. Used for (un)registration on
/// hot-reload so removal never relies on a duplicated string literal.
pub const IMAGE_TOOL_NAME: &str = "generate_image";

/// A callback that re-runs [`reinstall_image_tool`] against the agent's current
/// `[llm.image]` config and vault. Invoked from sites that mutate the vault's
/// `llm/accounts/*` entries (chat-side `system.add_account`, `vault.set` /
/// `vault.delete` on those paths) so the image provider rebuilds with the new
/// credential — without it, the provider keeps the empty/stale `api_key` it
/// captured at the last config change.
pub type ReloadImageTool = Arc<dyn Fn() + Send + Sync>;

/// A no-op [`ReloadImageTool`] for call sites where the image tool isn't wired
/// (tests, single-tenant CLI, agents with no vault). Cheaper than threading
/// `Option<ReloadImageTool>` through every constructor.
pub fn noop_reload_image_tool() -> ReloadImageTool {
    Arc::new(|| {})
}

/// Marker key in a tool result whose presence tells the runner this turn
/// produced images that must be delivered to the channel surface.
pub const GENERATED_IMAGES_KEY: &str = "generated_images";

/// A staged attachment referenced from a tool result: the bytes live in the
/// artifact store, this is the handle the delivery layer resolves. Despite the
/// `Image` name (kept for the original `generate_image` producer), this carries
/// any binary attachment — `browser.screenshot` (PNG) and `browser.pdf` (PDF)
/// reuse the same channel.
#[derive(Debug, Clone)]
pub struct GeneratedImageRef {
    pub artifact_id: ArtifactId,
    pub media_type: String,
    /// Filename to deliver the attachment under (e.g. `report.pdf`). When
    /// `None` the sink derives one from `media_type`. Lets non-image producers
    /// control the uploaded file's name/extension so Slack infers the type.
    pub filename: Option<String>,
    /// One-line text posted with the file (Slack's `initial_comment`, the
    /// gateway message body) — a chart's takeaway, a report's title.
    pub caption: Option<String>,
    /// What the file was drawn from, staged as a file of its own (a chart's
    /// validated spec), for a reader that draws it itself in its place.
    pub spec: Option<Box<GeneratedImageRef>>,
}

/// Marker key naming, on a staged file's marker, the file it was drawn from.
pub const SPEC_KEY: &str = "spec";

fn generated_file(e: &Value) -> Option<GeneratedImageRef> {
    let id = ArtifactId::parse(e.get("artifact_id")?.as_str()?).ok()?;
    let media_type = e
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_string();
    let text = |key: &str| {
        e.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(GeneratedImageRef {
        artifact_id: id,
        media_type,
        filename: text("filename"),
        caption: text("caption"),
        spec: e.get(SPEC_KEY).and_then(generated_file).map(Box::new),
    })
}

/// Extract the staged-attachment handles from a tool result's output. Returns
/// empty for any tool that didn't emit the marker key, so the runner can call
/// this indiscriminately on every successful tool result.
pub fn parse_generated_images(output: &serde_json::Value) -> Vec<GeneratedImageRef> {
    let Some(entries) = output.get(GENERATED_IMAGES_KEY).and_then(Value::as_array) else {
        return Vec::new();
    };
    entries.iter().filter_map(generated_file).collect()
}

/// Delivers a generated image to the surface the current turn originates from.
///
/// The runner owns an optional `Arc<dyn ImageSink>` and calls [`Self::deliver`]
/// inline as soon as a `generate_image` tool result lands — so the user sees
/// the image before the agent's accompanying text. The implementation (in the
/// hub) dispatches on the [`SessionContext`]: a Slack origin becomes a native
/// `files.upload` to the channel/thread; a gateway origin becomes a
/// `ServerMessage` image attachment on the websocket.
#[async_trait]
pub trait ImageSink: Send + Sync {
    /// Resolve `artifact_id` from the artifact store and deliver it to the
    /// surface implied by `ctx`. `filename`, when set, names the delivered
    /// file (the sink derives one from `media_type` otherwise). Errors are
    /// logged by the runner, never fatal to the turn — a failed delivery must
    /// not abort the agent's response.
    async fn deliver(
        &self,
        ctx: &SessionContext,
        artifact_id: &ArtifactId,
        media_type: &str,
        filename: Option<&str>,
        caption: Option<&str>,
    ) -> eyre::Result<()>;

    /// Deliver `file` as its producer described it. A sink that can show a
    /// reader what the file was drawn from (its [`GeneratedImageRef::spec`])
    /// overrides this; every other sink delivers the file itself.
    async fn deliver_file(
        &self,
        ctx: &SessionContext,
        file: &GeneratedImageRef,
    ) -> eyre::Result<()> {
        self.deliver(
            ctx,
            &file.artifact_id,
            &file.media_type,
            file.filename.as_deref(),
            file.caption.as_deref(),
        )
        .await
    }
}

pub struct ImageGenerationTool {
    provider: Arc<dyn ImageProvider>,
    agent_id: String,
    default_size: String,
    default_n: u8,
}

impl ImageGenerationTool {
    pub fn new(
        provider: Arc<dyn ImageProvider>,
        agent_id: impl Into<String>,
        default_size: impl Into<String>,
        default_n: u8,
    ) -> Self {
        Self {
            provider,
            agent_id: agent_id.into(),
            default_size: default_size.into(),
            default_n: default_n.max(1),
        }
    }
}

#[async_trait]
impl Tool for ImageGenerationTool {
    fn name(&self) -> &str {
        IMAGE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Generate or EDIT an image. Without `images`, generates from your text `prompt` (describe \
         subject + style: \"photorealistic\", \"anime\", \"watercolour\", \"3D render\"). With \
         `images` (artifact ids from view_attachment / get_user_photo / a prior generation), it \
         EDITS/COMPOSES over them — e.g. place a person from one image into a scene from another, \
         or add an object; the `prompt` then describes the change and refers to the inputs (\"put \
         the person from the second image into the first, standing and smiling\"). The system \
         delivers the result to the current conversation automatically. IMPORTANT: you will NOT \
         see the output in your own context and the artifact is internal — that is expected, not a \
         failure. Do not upload, attach, re-send, or verify it yourself, and never tell the user \
         it failed to deliver. Just briefly confirm what you created. The ONE exception: if the \
         user asked you to send it somewhere else (DM it to someone, or post it in another \
         channel), call messaging.send_file with the returned artifact_id — and set `deliver` \
         false here if it should go ONLY there, not also to this conversation."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What to depict (generate), or the change to make (edit). When editing, refer to the inputs positionally, e.g. 'place the person from the second image into the first'."
                },
                "images": {
                    "type": "array",
                    "description": "Optional reference images to EDIT/COMPOSE over. Each entry is { \"artifact_id\": \"art_…\", \"media_type\": \"image/png\" } from a view_attachment / get_user_photo / prior generate_image result. Presence switches to edit mode (routed to a face-capable model). Omit for plain text→image.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "artifact_id": { "type": "string", "description": "Artifact id (art_…) of a staged image." },
                            "media_type": { "type": "string", "description": "Optional MIME, e.g. 'image/png'. Sniffed from bytes if omitted." }
                        }
                    }
                },
                "style": {
                    "type": "string",
                    "description": "Optional art style, e.g. 'photorealistic', 'anime', 'oil painting', '3D render'. Folded into the prompt."
                },
                "negative_prompt": {
                    "type": "string",
                    "description": "Optional: things to avoid (honoured by Stability/FLUX; ignored by Gemini)."
                },
                "size": {
                    "type": "string",
                    "description": "Optional WxH, e.g. '1024x1024' or '1280x720'. Defaults to the configured size."
                },
                "n": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 4,
                    "description": "Optional number of images to generate (default 1)."
                },
                "seed": {
                    "type": "integer",
                    "description": "Optional seed for reproducibility, where the provider supports it."
                },
                "deliver": {
                    "type": "boolean",
                    "description": "Default true: post the image to THIS conversation automatically. Set false when you only want to send it elsewhere (e.g. DM it to someone or post it in another channel) — the image is staged and its artifact_id returned, but not posted here; then call messaging.send_file with that artifact_id."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, eyre::Error> {
        let started = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let Some(prompt) = input["prompt"].as_str().filter(|s| !s.trim().is_empty()) else {
            return Ok(ToolResult::failure(
                "generate_image requires a non-empty 'prompt'",
                elapsed(started),
            ));
        };

        let full_prompt = match input["style"].as_str().filter(|s| !s.trim().is_empty()) {
            Some(style) => format!("{prompt}, {style} style"),
            None => prompt.to_string(),
        };

        let size = input["size"]
            .as_str()
            .map(ImageSize::parse)
            .unwrap_or_else(|| ImageSize::parse(&self.default_size));
        let n = input["n"]
            .as_u64()
            .map(|v| v.clamp(1, 4) as u8)
            .unwrap_or(self.default_n);

        let reference_images = match resolve_reference_images(&input["images"], &self.agent_id) {
            Ok(imgs) => imgs,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed(started))),
        };

        let req = ImageRequest {
            prompt: full_prompt,
            negative_prompt: input["negative_prompt"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
            size,
            n,
            seed: input["seed"].as_u64(),
            reference_images,
        };

        let mut images = match self.provider.generate(&req).await {
            Ok(images) => images,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("image generation failed: {e}"),
                    elapsed(started),
                ));
            }
        };
        // Cap to the requested count: some providers (e.g. OpenRouter chat) and
        // some models return more images than asked for, which would otherwise
        // be staged and delivered as duplicates.
        images.truncate(n.max(1) as usize);

        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage generated image",
                elapsed(started),
            ));
        };

        let mut entries = Vec::new();
        for image in images {
            let descriptor = ArtifactDescriptor {
                media_type: Some(image.media_type.clone()),
                ..ArtifactDescriptor::from_context(
                    "generate_image",
                    ContentType::OctetStream,
                    self.agent_id.clone(),
                )
            };
            let bytes_len = image.data.len();
            let handle =
                match store.write(&self.agent_id, descriptor, ArtifactBody::Bytes(image.data)) {
                    Ok(handle) => handle,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("failed to stage generated image: {e}"),
                            elapsed(started),
                        ));
                    }
                };
            entries.push(json!({
                "artifact_id": handle.id.as_str(),
                "media_type": image.media_type,
                "bytes": bytes_len,
                "revised_prompt": image.revised_prompt,
            }));
        }

        // `deliver=false` stages the image without auto-posting it here — the
        // model still gets the artifact_id and can forward it via
        // messaging.send_file. The runner sink only delivers entries under
        // GENERATED_IMAGES_KEY, so non-delivered images go under a different key.
        // Coerce the stringy/int forms some models emit so a `"false"` can't
        // leak the image into the channel the user wanted to exclude.
        let deliver = match &input["deliver"] {
            Value::Bool(b) => *b,
            Value::String(s) => !matches!(s.trim(), "false" | "False" | "0" | "no"),
            Value::Number(n) => n.as_i64() != Some(0),
            _ => true,
        };
        let count = entries.len();
        let mut output = serde_json::Map::new();
        let note = if deliver {
            output.insert(GENERATED_IMAGES_KEY.to_string(), Value::Array(entries));
            "Generated. The system is delivering the image(s) to this conversation automatically — it happens outside your context, so you will NOT see the attachment and the artifact is not visible to you. This is expected, NOT a failure: do not re-upload, attach, verify, or claim it failed. Just briefly tell the user what you made. (To ALSO send it to another channel or DM someone, call messaging.send_file with the artifact_id above.)"
        } else {
            output.insert("staged_images".to_string(), Value::Array(entries));
            "Generated and staged, but NOT posted to this conversation (deliver=false). It is not visible to you. To deliver it, call messaging.send_file with the artifact_id above and a `channel` or `user`."
        };
        output.insert("count".to_string(), json!(count));
        output.insert("prompt".to_string(), json!(prompt));
        output.insert("note".to_string(), json!(note));
        Ok(ToolResult::success(Value::Object(output), elapsed(started)))
    }
}

/// Outcome of applying a (possibly changed) `[llm.image]` config to the live
/// tool registry. Returned so the caller can narrate the swap, mirroring how
/// the LLM provider hot-swap is logged.
pub enum ImageReload {
    Unchanged,
    Installed { model: String },
    Disabled,
    BuildFailed(eyre::Report),
}

impl ImageReload {
    /// The config to record as the one the tool is now built from, given the
    /// `applied` block this outcome came from. `BuildFailed` records `None` so a
    /// later reload re-diffs and retries the build instead of short-circuiting
    /// on an unchanged-but-broken block; every other outcome records `applied`
    /// (which is itself `None` for a removed block). On `Unchanged`, `applied`
    /// already equals the recorded value, so storing it is a no-op.
    pub fn recorded(
        &self,
        applied: Option<LlmImageGenerationConfig>,
    ) -> Option<LlmImageGenerationConfig> {
        match self {
            ImageReload::BuildFailed(_) => None,
            _ => applied,
        }
    }

    pub fn log(&self, agent_id: &str) {
        match self {
            ImageReload::Unchanged => {}
            ImageReload::Installed { model } => {
                tracing::info!(agent = %agent_id, %model, "Image generation tool (re)installed")
            }
            ImageReload::Disabled => {
                tracing::info!(agent = %agent_id, "Image generation disabled; tool removed")
            }
            ImageReload::BuildFailed(e) => {
                tracing::warn!(agent = %agent_id, error = %e, "Image config changed but provider build failed; tool removed")
            }
        }
    }
}

/// Reconcile the registered `generate_image` tool with `next` config, given the
/// `current` config it was last built from. The single path shared by startup
/// (`current = None`) and hot-reload.
///
/// Provider construction does no network I/O — credential/region errors surface
/// on the first real request, so `verify_credentials` is deliberately not called
/// here. A build failure or a disabled/absent block removes the tool, so the
/// registry never advertises a `generate_image` whose provider can't run.
///
/// `key` is where the provider the config routes to reads its key on every
/// request.
pub fn install_image_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    current: Option<&LlmImageGenerationConfig>,
    next: Option<&LlmImageGenerationConfig>,
    key: impl Fn(ProviderKind) -> KeySlot,
) -> ImageReload {
    if current == next {
        return ImageReload::Unchanged;
    }
    reinstall_image_tool(tools, agent_id, next, key)
}

/// Rebuild the `generate_image` tool from `cfg` unconditionally. Used when the
/// config is unchanged but a credential read at build time isn't — a Bedrock
/// account written to `llm/accounts/*` after the provider was built. Returns the same
/// outcome variants as [`install_image_tool`]; `Unchanged` is never produced
/// here because the caller has already decided a rebuild is needed.
pub fn reinstall_image_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    cfg: Option<&LlmImageGenerationConfig>,
    key: impl Fn(ProviderKind) -> KeySlot,
) -> ImageReload {
    match cfg.filter(|c| c.enabled) {
        Some(cfg) => match provider_from_config(cfg, key) {
            Ok(provider) => {
                tools.register(Arc::new(ImageGenerationTool::new(
                    Arc::from(provider),
                    agent_id,
                    cfg.size.clone(),
                    cfg.n,
                )));
                ImageReload::Installed {
                    model: cfg.model.clone(),
                }
            }
            Err(e) => {
                tools.unregister(IMAGE_TOOL_NAME);
                ImageReload::BuildFailed(e)
            }
        },
        None => {
            tools.unregister(IMAGE_TOOL_NAME);
            ImageReload::Disabled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::types::ArtifactId;

    /// Pins the producer↔consumer contract: the JSON the tool emits under
    /// `GENERATED_IMAGES_KEY` must be exactly what `parse_generated_images`
    /// (called by the runner's delivery step) reads back. A rename on either
    /// side breaks delivery silently — this test fails first.
    #[test]
    fn parse_round_trips_tool_output_shape() {
        let id = ArtifactId::fresh();
        let mut output = serde_json::Map::new();
        output.insert(
            GENERATED_IMAGES_KEY.to_string(),
            Value::Array(vec![json!({
                "artifact_id": id.as_str(),
                "media_type": "image/jpeg",
                "bytes": 2048,
                "revised_prompt": null,
            })]),
        );
        output.insert("count".to_string(), json!(1));
        let refs = parse_generated_images(&Value::Object(output));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].artifact_id, id);
        assert_eq!(refs[0].media_type, "image/jpeg");
    }

    #[test]
    fn parse_defaults_media_type_when_missing() {
        let id = ArtifactId::fresh();
        let output = json!({ GENERATED_IMAGES_KEY: [{ "artifact_id": id.as_str() }] });
        let refs = parse_generated_images(&output);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].media_type, "image/png");
    }

    #[test]
    fn parse_reads_filename_and_caption_when_present() {
        let id = ArtifactId::fresh();
        let output = json!({ GENERATED_IMAGES_KEY: [
            { "artifact_id": id.as_str(), "filename": "q3.png", "caption": "Pro leads." },
            { "artifact_id": id.as_str(), "filename": "", "caption": "" },
        ] });
        let refs = parse_generated_images(&output);
        assert_eq!(refs[0].filename.as_deref(), Some("q3.png"));
        assert_eq!(refs[0].caption.as_deref(), Some("Pro leads."));
        assert_eq!(
            (refs[1].filename.as_deref(), refs[1].caption.as_deref()),
            (None, None)
        );
    }

    #[test]
    fn parse_reads_the_spec_a_file_was_drawn_from() {
        let (png, spec) = (ArtifactId::fresh(), ArtifactId::fresh());
        let output = json!({ GENERATED_IMAGES_KEY: [
            { "artifact_id": png.as_str(), "filename": "q3.png", SPEC_KEY: {
                "artifact_id": spec.as_str(), "media_type": "application/vnd.workforce.chart+json",
                "filename": "q3.json"
            } },
            { "artifact_id": png.as_str(), SPEC_KEY: { "artifact_id": "not-an-art-id" } },
        ] });
        let refs = parse_generated_images(&output);
        let drawn = refs[0].spec.as_deref().expect("a spec");
        assert_eq!(drawn.artifact_id, spec);
        assert_eq!(drawn.media_type, "application/vnd.workforce.chart+json");
        assert_eq!(drawn.filename.as_deref(), Some("q3.json"));
        assert!(refs[1].spec.is_none(), "an unreadable spec is no spec");
    }

    #[test]
    fn parse_ignores_non_image_tool_output() {
        // A different tool's result must yield no image refs.
        let output = json!({ "rows": [1, 2, 3], "status": "ok" });
        assert!(parse_generated_images(&output).is_empty());
    }

    #[test]
    fn parse_skips_entries_with_invalid_artifact_id() {
        let output = json!({ GENERATED_IMAGES_KEY: [{ "artifact_id": "not-an-art-id" }] });
        assert!(parse_generated_images(&output).is_empty());
    }

    fn gemini_cfg(model: &str) -> LlmImageGenerationConfig {
        LlmImageGenerationConfig {
            model: model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn install_registers_enabled_provider() {
        let tools = ToolRegistry::new();
        let cfg = gemini_cfg("gemini:standard");
        let outcome =
            install_image_tool(&tools, "agent", None, Some(&cfg), |_| KeySlot::fixed("k"));
        assert!(matches!(outcome, ImageReload::Installed { .. }));
        assert!(tools.has(IMAGE_TOOL_NAME));
    }

    #[test]
    fn install_asks_for_the_key_of_the_provider_the_config_routes_to() {
        let tools = ToolRegistry::new();
        let cfg = gemini_cfg("gemini:standard");
        let outcome = install_image_tool(&tools, "agent", None, Some(&cfg), |kind| {
            assert_eq!(kind, ProviderKind::Gemini);
            KeySlot::fixed("k")
        });
        assert!(matches!(outcome, ImageReload::Installed { .. }));
        assert!(tools.has(IMAGE_TOOL_NAME));
    }

    #[test]
    fn install_removes_tool_when_disabled() {
        let tools = ToolRegistry::new();
        let enabled = gemini_cfg("gemini:standard");
        install_image_tool(&tools, "agent", None, Some(&enabled), |_| {
            KeySlot::fixed("k")
        });

        let disabled = LlmImageGenerationConfig {
            enabled: false,
            ..enabled.clone()
        };
        let outcome = install_image_tool(&tools, "agent", Some(&enabled), Some(&disabled), |_| {
            KeySlot::fixed("k")
        });
        assert!(matches!(outcome, ImageReload::Disabled));
        assert!(!tools.has(IMAGE_TOOL_NAME));
    }

    #[test]
    fn install_removes_tool_when_block_absent() {
        let tools = ToolRegistry::new();
        let enabled = gemini_cfg("gemini:standard");
        install_image_tool(&tools, "agent", None, Some(&enabled), |_| {
            KeySlot::fixed("k")
        });

        let outcome = install_image_tool(&tools, "agent", Some(&enabled), None, |_| {
            KeySlot::fixed("k")
        });
        assert!(matches!(outcome, ImageReload::Disabled));
        assert!(!tools.has(IMAGE_TOOL_NAME));
    }

    #[test]
    fn install_is_noop_when_unchanged() {
        let tools = ToolRegistry::new();
        let cfg = gemini_cfg("gemini:standard");
        install_image_tool(&tools, "agent", None, Some(&cfg), |_| KeySlot::fixed("k"));

        let outcome = install_image_tool(&tools, "agent", Some(&cfg), Some(&cfg), |_| {
            KeySlot::fixed("k")
        });
        assert!(matches!(outcome, ImageReload::Unchanged));
        assert!(tools.has(IMAGE_TOOL_NAME));
    }

    #[test]
    fn install_removes_tool_on_build_failure() {
        let tools = ToolRegistry::new();
        let bad = gemini_cfg("");
        let outcome =
            install_image_tool(&tools, "agent", None, Some(&bad), |_| KeySlot::fixed("k"));
        assert!(matches!(outcome, ImageReload::BuildFailed(_)));
        assert!(!tools.has(IMAGE_TOOL_NAME));
    }

    #[test]
    fn recorded_clears_on_build_failure_to_allow_retry() {
        let cfg = gemini_cfg("gemini:standard");
        let failed = ImageReload::BuildFailed(eyre::eyre!("boom"));
        assert!(failed.recorded(Some(cfg.clone())).is_none());

        assert_eq!(
            ImageReload::Installed {
                model: cfg.model.clone()
            }
            .recorded(Some(cfg.clone())),
            Some(cfg.clone())
        );
        assert_eq!(ImageReload::Disabled.recorded(Some(cfg.clone())), Some(cfg));
    }

    #[test]
    fn install_swaps_provider_in_place() {
        let tools = ToolRegistry::new();
        let a = gemini_cfg("gemini:standard");
        install_image_tool(&tools, "agent", None, Some(&a), |_| KeySlot::fixed("k"));

        let b = gemini_cfg("gemini:powerful");
        let outcome =
            install_image_tool(&tools, "agent", Some(&a), Some(&b), |_| KeySlot::fixed("k"));
        assert!(matches!(outcome, ImageReload::Installed { .. }));
        assert!(tools.has(IMAGE_TOOL_NAME));
    }
}
