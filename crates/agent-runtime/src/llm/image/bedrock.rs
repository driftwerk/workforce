//! Image generation via AWS Bedrock `InvokeModel`.
//!
//! Reuses the chat path's SigV4 signer, region resolver, and credential chain
//! ([`crate::llm::bedrock`]) but speaks the `InvokeModel` request/response
//! shapes — image generation does **not** go through the Converse API.
//!
//! Two model families, detected by id prefix:
//!   - `stability.*`  → Stable Image (Ultra/Core, SD3.5) request/response.
//!   - `amazon.*`     → Amazon Nova Canvas / Titan Image `TEXT_IMAGE` shape.
//!
//! Operational note (matches our documented Bedrock gotchas): image models
//! require explicit per-model access opt-in in the Bedrock console and are not
//! offered in every region — eu-west-2 in particular. Those failures surface
//! from [`generate`] with an actionable hint rather than a bare 4xx.

use std::time::SystemTime;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::{json, Value};

use super::{GeneratedImage, ImageProvider, ImageRequest};
use crate::llm::bedrock::{invoke_url, resolve_region, sign_v4};
use crate::llm::bedrock_capabilities::WireFormat;
use crate::llm::bedrock_credentials::CredentialResolver;

#[derive(Clone, Copy)]
enum Family {
    Stability,
    Nova,
}

pub struct BedrockImageProvider {
    client: Client,
    resolver: CredentialResolver,
    region: String,
    model: String,
}

impl BedrockImageProvider {
    pub fn new(packed_api_key: &str, model: impl Into<String>, region: Option<&str>) -> Self {
        Self {
            client: super::http_client(120),
            resolver: CredentialResolver::from_packed(packed_api_key),
            region: region
                .map(str::to_string)
                .unwrap_or_else(|| resolve_region(None)),
            model: model.into(),
        }
    }

    fn family(&self) -> Family {
        if self.model.starts_with("stability.") {
            Family::Stability
        } else {
            Family::Nova
        }
    }

    /// Sign and POST an `InvokeModel` request, returning the parsed JSON body.
    async fn invoke(&self, body: &Value) -> Result<Value> {
        let url = invoke_url(&self.region, &self.model, WireFormat::Invoke, false);
        let body_bytes = serde_json::to_vec(body)?;
        let credentials = self.resolver.resolve().await?;
        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];
        let signed = sign_v4(
            "POST",
            &url,
            &headers,
            &body_bytes,
            &credentials,
            &self.region,
            SystemTime::now(),
        )?;
        headers.extend(signed);

        let mut req = self.client.post(&url);
        for (name, value) in &headers {
            req = req.header(name.as_str(), value.as_str());
        }
        let resp = req
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| eyre!("Bedrock image HTTP request failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(eyre!(
                "Bedrock InvokeModel failed ({status}) for image model '{model}' in {region}: \
                 {text}\nhint: image models need per-model access enabled in the Bedrock console \
                 and are not offered in every region (Nova Canvas / Stability are absent from \
                 some regions including eu-west-2) — verify access and availability for {region}.",
                model = self.model,
                region = self.region,
            ));
        }
        serde_json::from_str(&text).map_err(|e| eyre!("Bedrock image response parse failed: {e}"))
    }

    fn build_body(&self, req: &ImageRequest) -> Value {
        match self.family() {
            Family::Stability => {
                let mut body = json!({
                    "prompt": req.prompt,
                    "mode": "text-to-image",
                    "aspect_ratio": req.size.nearest_aspect_ratio(),
                    "output_format": "png",
                });
                if let Some(neg) = req
                    .negative_prompt
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                {
                    body["negative_prompt"] = json!(neg);
                }
                if let Some(seed) = req.seed {
                    // Stable Image seeds are u32; clamp to the valid range.
                    body["seed"] = json!((seed % 4_294_967_294) as u32);
                }
                body
            }
            Family::Nova => {
                let mut text_params = json!({ "text": req.prompt });
                // Nova/Titan require negativeText to be at least 3 chars.
                if let Some(neg) = req.negative_prompt.as_ref().filter(|s| s.trim().len() >= 3) {
                    text_params["negativeText"] = json!(neg);
                }
                let mut gen_config = json!({
                    "numberOfImages": req.n.clamp(1, 5),
                    "width": req.size.width,
                    "height": req.size.height,
                    "cfgScale": 8.0,
                });
                if let Some(seed) = req.seed {
                    gen_config["seed"] = json!((seed % 2_147_483_646) as u32);
                }
                json!({
                    "taskType": "TEXT_IMAGE",
                    "textToImageParams": text_params,
                    "imageGenerationConfig": gen_config,
                })
            }
        }
    }
}

#[async_trait]
impl ImageProvider for BedrockImageProvider {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>> {
        if !req.reference_images.is_empty() {
            return Err(eyre!(
                "Bedrock image generation does not support reference-image editing/composition; \
                 use a gemini or openrouter:google model for edits"
            ));
        }
        match self.family() {
            // Nova/Titan batch natively via numberOfImages.
            Family::Nova => {
                let value = self.invoke(&self.build_body(req)).await?;
                if let Some(err) = value["error"].as_str() {
                    if !err.is_empty() {
                        return Err(eyre!("Nova Canvas reported an error: {err}"));
                    }
                }
                decode_image_array(&value, "Nova Canvas")
            }
            // Stable Image returns a single image per call — loop for n>1.
            Family::Stability => {
                let mut out = Vec::new();
                for _ in 0..req.n.max(1) {
                    let value = self.invoke(&self.build_body(req)).await?;
                    // finish_reasons[i] non-null means that image was filtered.
                    if let Some(reasons) = value["finish_reasons"].as_array() {
                        if let Some(reason) = reasons.iter().find_map(|r| r.as_str()) {
                            return Err(eyre!("Stability filtered the request: {reason}"));
                        }
                    }
                    out.extend(decode_image_array(&value, "Stability")?);
                }
                Ok(out)
            }
        }
    }

    async fn verify_credentials(&self) -> Result<()> {
        // Resolve the AWS credential chain — catches missing/invalid creds
        // cheaply. We deliberately do NOT issue an InvokeModel here: every such
        // call generates a billable image. Per-model access and regional
        // availability are validated on first real generation, where the error
        // carries an actionable hint (see `invoke`).
        self.resolver
            .resolve()
            .await
            .map(|_| ())
            .map_err(|e| eyre!("Bedrock image credentials unavailable: {e}"))
    }
}

/// Decode the `images: [base64, ...]` array shared by both Bedrock families.
fn decode_image_array(value: &Value, who: &str) -> Result<Vec<GeneratedImage>> {
    let images = value["images"]
        .as_array()
        .ok_or_else(|| eyre!("{who} response missing 'images' array: {value}"))?;
    let out = images
        .iter()
        .map(|b64| {
            let s = b64
                .as_str()
                .ok_or_else(|| eyre!("{who} image entry was not a string"))?;
            Ok(GeneratedImage {
                data: BASE64
                    .decode(s)
                    .map_err(|e| eyre!("{who} base64 decode failed: {e}"))?,
                media_type: "image/png".to_string(),
                revised_prompt: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if out.is_empty() {
        return Err(eyre!("{who} returned no images"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::image::ImageSize;

    fn provider(model: &str) -> BedrockImageProvider {
        BedrockImageProvider::new("none", model, None)
    }

    fn req() -> ImageRequest {
        ImageRequest {
            prompt: "a butterfly in a forest".to_string(),
            negative_prompt: Some("blurry".to_string()),
            size: ImageSize {
                width: 1024,
                height: 1024,
            },
            n: 1,
            seed: Some(42),
            reference_images: Vec::new(),
        }
    }

    #[test]
    fn stability_body_shape() {
        let body = provider("stability.stable-image-ultra-v1:1").build_body(&req());
        assert_eq!(body["mode"], "text-to-image");
        assert_eq!(body["aspect_ratio"], "1:1");
        assert_eq!(body["negative_prompt"], "blurry");
        assert_eq!(body["seed"], 42);
        assert!(body.get("taskType").is_none());
    }

    #[test]
    fn nova_body_shape() {
        let body = provider("amazon.nova-canvas-v1:0").build_body(&req());
        assert_eq!(body["taskType"], "TEXT_IMAGE");
        assert_eq!(body["textToImageParams"]["text"], "a butterfly in a forest");
        assert_eq!(body["textToImageParams"]["negativeText"], "blurry");
        assert_eq!(body["imageGenerationConfig"]["width"], 1024);
        assert_eq!(body["imageGenerationConfig"]["numberOfImages"], 1);
    }

    #[test]
    fn nova_drops_short_negative_text() {
        let mut r = req();
        r.negative_prompt = Some("no".to_string()); // < 3 chars
        let body = provider("amazon.nova-canvas-v1:0").build_body(&r);
        assert!(body["textToImageParams"].get("negativeText").is_none());
    }

    #[test]
    fn decode_image_array_reads_base64() {
        let value = json!({ "images": ["aGk="] }); // "hi"
        let imgs = decode_image_array(&value, "test").unwrap();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].data, b"hi");
        assert_eq!(imgs[0].media_type, "image/png");
    }
}
