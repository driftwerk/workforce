//! `get_user_photo` — stage a chat user's profile photo as an artifact.
//!
//! The input counterpart for editing: to "add Tom into this photo" the agent
//! needs Tom's face as image bytes the generator can consume. This tool looks
//! up a user's avatar via the channel surface (`member_info`), downloads the
//! bytes (avatar URLs are public CDN, so a plain GET — no auth), stages them in
//! the artifact store, and returns the same `viewed_images` marker shape as
//! `view_attachment`. The agent then passes the artifact id to
//! `generate_image { images: [...] }`.
//!
//! It takes a platform `user_id` (e.g. a Slack `U…`). When the agent only has a
//! display name, it should resolve the id first with its user-lookup tools
//! (e.g. `slack.list_users` with a name filter) — keeping this tool surface-
//! agnostic (it only needs the `member_info` trait method).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::info;

use workforce_agent::llm::image::sniff_media_type;
use workforce_agent::runner::CURRENT_CHANNEL_ID;
use workforce_agent::tools::image_view::{stage_for_view, VIEWED_IMAGES_KEY};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::channels::SurfaceRegistry;

use crate::adapters::messaging_routing::{channel_hint_from_input, platform_arg};

const GET_USER_PHOTO_TOOL_NAME: &str = "get_user_photo";

/// Read a task-local string, treating empty as absent.
fn task_local(key: &'static tokio::task::LocalKey<String>) -> Option<String> {
    key.try_with(|v| v.clone()).ok().filter(|s| !s.is_empty())
}

/// Fetches a user's profile photo into the agent's view, for use as a reference
/// image in `generate_image` edits.
pub struct GetUserPhotoTool {
    surfaces: Arc<RwLock<SurfaceRegistry>>,
    http: reqwest::Client,
}

impl GetUserPhotoTool {
    pub fn new(surfaces: Arc<RwLock<SurfaceRegistry>>) -> Self {
        Self {
            surfaces,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl Tool for GetUserPhotoTool {
    fn name(&self) -> &str {
        GET_USER_PHOTO_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Fetch a chat user's profile photo and stage it so you can use it as a reference image in \
         `generate_image` (e.g. 'add Tom into this scene' or 'make a video of Tom'). Takes the \
         user's platform id (`user_id`, e.g. a Slack 'U…'); if you only have a name, look up the \
         id first with your user-listing tool. Returns an artifact id you pass to \
         `generate_image { images: [...] }`. The photo appears in your view on your next step."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "user_id": {
                    "type": "string",
                    "description": "Platform user id (e.g. Slack 'U0123ABCD'). Resolve a display name to an id first if needed."
                },
                "name": {
                    "type": "string",
                    "description": "Optional display name, used only to name the staged file."
                }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult, eyre::Error> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let Some(user_id) = input["user_id"].as_str().filter(|s| !s.trim().is_empty()) else {
            return Ok(ToolResult::failure(
                "get_user_photo requires a non-empty 'user_id'",
                elapsed(),
            ));
        };

        let Some(channel) = task_local(&CURRENT_CHANNEL_ID) else {
            return Ok(ToolResult::failure(
                "get_user_photo can only be used inside a conversation",
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

        let member = match surface.member_info(user_id).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e.wrap_err(format!("failed to look up user {user_id}")),
                    elapsed(),
                ))
            }
        };
        let Some(avatar_url) = member.avatar_url.filter(|u| !u.is_empty()) else {
            return Ok(ToolResult::failure(
                format!("user {user_id} has no profile photo set"),
                elapsed(),
            ));
        };
        // Prefer a higher-res variant where the CDN URL exposes the size suffix
        // (Slack: `..._192.png`); harmless string op that no-ops otherwise.
        let url = avatar_url.replace("_192.", "_512.");

        let resp = match self.http.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                return Ok(ToolResult::failure(
                    format!("profile photo download failed ({})", r.status()),
                    elapsed(),
                ))
            }
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("profile photo download failed: {e}"),
                    elapsed(),
                ))
            }
        };
        let header_mt = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|s| s.starts_with("image/"))
            .map(str::to_string);
        let bytes = match resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to read profile photo bytes: {e}"),
                    elapsed(),
                ))
            }
        };
        let media_type = header_mt.unwrap_or_else(|| sniff_media_type(&bytes).to_string());

        let display = input["name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if !member.display_name.is_empty() {
                    member.display_name.clone()
                } else {
                    user_id.to_string()
                }
            });
        let ext = match media_type.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "jpg",
        };
        let filename = format!("{}.{ext}", display.replace(' ', "_"));
        let staged = match stage_for_view(GET_USER_PHOTO_TOOL_NAME, bytes, &media_type, &filename) {
            Ok(s) => s,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };

        info!(user = %user_id, channel = %channel, "Staged user profile photo");
        Ok(ToolResult::success(
            serde_json::json!({
                VIEWED_IMAGES_KEY: staged.marker,
                "note": format!(
                    "Fetched {display}'s profile photo (artifact {}). Pass this artifact_id to \
                     generate_image's `images` to compose them into a scene or animate them. It \
                     also renders into your view on your next step. If it looks like a plain \
                     initials/blank avatar, the user has no real photo — say so rather than using it.",
                    staged.artifact_id
                ),
            }),
            elapsed(),
        ))
    }
}
