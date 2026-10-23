//! Image generation against a self-hosted, OpenAI-Images-compatible endpoint.
//!
//! Targets the `POST {base}/v1/images/generations` contract (response
//! `{ "data": [{ "b64_json": ... }] }`) that FLUX/SD serving shims and
//! OpenAI-compat bridges expose. Point `[llm.image].url` at the deployment;
//! `FLUX.1-dev` runs comfortably on an A100. Auth is an optional bearer token.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::{json, Value};
use workforce_common::secret::KeySlot;

use super::{decode_data_url, GeneratedImage, ImageProvider, ImageRequest};

pub struct VllmImageProvider {
    client: Client,
    base_url: String,
    api_key: KeySlot,
    model: String,
}

impl VllmImageProvider {
    pub fn new(base_url: impl Into<String>, api_key: KeySlot, model: impl Into<String>) -> Self {
        Self {
            client: super::http_client(180),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/images/generations", self.base_url)
    }

    fn maybe_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.api_key.peek().filter(|key| key != "none") {
            Some(key) => rb.bearer_auth(key),
            None => rb,
        }
    }
}

#[async_trait]
impl ImageProvider for VllmImageProvider {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>> {
        if !req.reference_images.is_empty() {
            return Err(eyre!(
                "self-hosted (vllm) image generation does not support reference-image editing; \
                 use a gemini or openrouter:google model for edits"
            ));
        }
        let mut body = json!({
            "model": self.model,
            "prompt": req.prompt,
            "n": req.n.max(1),
            "size": format!("{}x{}", req.size.width, req.size.height),
            "response_format": "b64_json",
        });
        if let Some(neg) = req
            .negative_prompt
            .as_ref()
            .filter(|s| !s.trim().is_empty())
        {
            body["negative_prompt"] = json!(neg);
        }
        if let Some(seed) = req.seed {
            body["seed"] = json!(seed);
        }

        let resp = self
            .maybe_auth(self.client.post(self.endpoint()))
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("self-hosted image HTTP request failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(eyre!(
                "self-hosted image generation failed ({status}): {text}"
            ));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| eyre!("self-hosted image response parse failed: {e}"))?;

        let data = value["data"]
            .as_array()
            .ok_or_else(|| eyre!("self-hosted image response missing 'data' array: {value}"))?;
        let out = data
            .iter()
            .map(|entry| {
                let (data, media_type) = if let Some(b64) = entry["b64_json"].as_str() {
                    let bytes = BASE64
                        .decode(b64)
                        .map_err(|e| eyre!("self-hosted image base64 decode failed: {e}"))?;
                    (bytes, "image/png".to_string())
                } else if let Some(url) = entry["url"].as_str() {
                    decode_data_url(url)?
                } else {
                    return Err(eyre!("self-hosted image entry missing b64_json/url"));
                };
                Ok(GeneratedImage {
                    data,
                    media_type,
                    revised_prompt: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if out.is_empty() {
            return Err(eyre!("self-hosted endpoint returned no images"));
        }
        Ok(out)
    }

    async fn verify_credentials(&self) -> Result<()> {
        // Probe the models listing — auth-only, no generation cost.
        let url = format!("{}/v1/models", self.base_url);
        let resp = self
            .maybe_auth(self.client.get(url))
            .send()
            .await
            .map_err(|e| eyre!("self-hosted image endpoint unreachable: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(eyre!(
                "self-hosted image endpoint check failed ({})",
                resp.status()
            ))
        }
    }
}
