//! Image generation via OpenRouter's image-capable chat models.
//!
//! OpenRouter exposes conversational image generation (e.g. Google's
//! `gemini-2.5-flash-image-preview`) through the standard chat-completions
//! surface: set `modalities: ["image", "text"]` and the assistant message
//! comes back with an `images` array of `data:` URLs. This reuses the same
//! OAI-wire plumbing the rest of the stack already speaks.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::{json, Value};
use workforce_common::secret::{scrub, KeySlot};

use super::{decode_data_url, GeneratedImage, ImageProvider, ImageRequest};

const CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const KEY_PROBE_URL: &str = "https://openrouter.ai/api/v1/key";

pub struct OpenRouterImageProvider {
    client: Client,
    api_key: KeySlot,
    model: String,
    /// Model used when a request carries reference images (the powerful image
    /// tier). Mirrors the direct-Gemini face-edit routing: the cheap/standard
    /// flash-image models refuse face composition, the pro tier performs it.
    edit_model: Option<String>,
}

impl OpenRouterImageProvider {
    pub fn new(api_key: KeySlot, model: impl Into<String>, edit_model: Option<&str>) -> Self {
        Self {
            client: super::http_client(120),
            api_key,
            model: model.into(),
            edit_model: edit_model.map(str::to_string),
        }
    }
}

#[async_trait]
impl ImageProvider for OpenRouterImageProvider {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>> {
        // Negative prompts have no first-class field on the chat surface; fold
        // them into the instruction so the model still benefits.
        let prompt = match &req.negative_prompt {
            Some(neg) if !neg.trim().is_empty() => {
                format!("{}\n\nAvoid: {}", req.prompt, neg)
            }
            _ => req.prompt.clone(),
        };
        // With reference images, the user message becomes a multimodal content
        // array (image parts + text), routed to the powerful edit model. Without
        // them, a plain string content is the text→image path. OpenRouter's
        // google image models accept image input via the standard `image_url`
        // data-URL shape (verified: blend/edit + image_config.strength).
        let (model, content) = if req.reference_images.is_empty() {
            (self.model.as_str(), Value::String(prompt))
        } else {
            let mut parts: Vec<Value> = req
                .reference_images
                .iter()
                .map(|img| {
                    json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.media_type, BASE64.encode(&img.data))
                        }
                    })
                })
                .collect();
            parts.push(json!({ "type": "text", "text": prompt }));
            (
                self.edit_model.as_deref().unwrap_or(&self.model),
                Value::Array(parts),
            )
        };
        let body = json!({
            "model": model,
            "messages": [{ "role": "user", "content": content }],
            "modalities": ["image", "text"],
        });

        let key = self.api_key.get()?;
        let resp = self
            .client
            .post(CHAT_URL)
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("OpenRouter image HTTP request failed: {e}"))?;
        let status = resp.status();
        let text = scrub(&resp.text().await.unwrap_or_default(), Some(&key));
        if !status.is_success() {
            return Err(eyre!(
                "OpenRouter image generation failed ({status}): {text}"
            ));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| eyre!("OpenRouter image response parse failed: {e}"))?;

        let message = &value["choices"][0]["message"];
        let images = message["images"].as_array().ok_or_else(|| {
            eyre!(
                "OpenRouter response carried no images — model '{}' may not support image output",
                self.model
            )
        })?;

        let revised = message["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);

        // Each entry: { "type": "image_url", "image_url": { "url": "data:..." } }
        let out = images
            .iter()
            .map(|img| {
                let url = img["image_url"]["url"]
                    .as_str()
                    .or_else(|| img["url"].as_str())
                    .ok_or_else(|| eyre!("OpenRouter image entry missing url"))?;
                let (data, media_type) = decode_data_url(url)?;
                Ok(GeneratedImage {
                    data,
                    media_type,
                    revised_prompt: revised.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if out.is_empty() {
            return Err(eyre!("OpenRouter returned an empty images array"));
        }
        Ok(out)
    }

    async fn verify_credentials(&self) -> Result<()> {
        // Auth-only probe: the key-info endpoint validates the key without
        // routing to any upstream model.
        let resp = self
            .client
            .get(KEY_PROBE_URL)
            .bearer_auth(self.api_key.get()?)
            .send()
            .await
            .map_err(|e| eyre!("OpenRouter credential probe failed: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(eyre!(
                "OpenRouter credential check failed ({})",
                resp.status()
            ))
        }
    }
}
