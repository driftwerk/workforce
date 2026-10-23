//! Google image generation via the native Generative Language REST API.
//!
//! Two model families share one provider:
//!   - `imagen-*`  → the `:predict` endpoint (dedicated high-res photoreal).
//!   - everything else (`gemini-2.5-flash-image`, "Nano Banana") →
//!     `:generateContent` with an image response modality.
//!
//! API-key auth via the `?key=` query parameter, matching the rest of the
//! Gemini integration.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::{json, Value};
use workforce_common::secret::{scrub, KeySlot};

use super::{GeneratedImage, ImageProvider, ImageRequest};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiImageProvider {
    client: Client,
    base_url: String,
    api_key: KeySlot,
    model: String,
    /// Model used when a request carries reference images (the powerful image
    /// tier). The cheap/standard `gemini-*-flash-image` models refuse to
    /// composite a recognizable face (finishReason=IMAGE_OTHER) while the pro
    /// tier performs it, so edits route here. `None` ⇒ fall back to `model`.
    edit_model: Option<String>,
}

impl GeminiImageProvider {
    pub fn new(api_key: KeySlot, model: impl Into<String>, edit_model: Option<&str>) -> Self {
        Self {
            client: super::http_client(120),
            base_url: API_BASE.to_string(),
            api_key,
            model: model.into(),
            edit_model: edit_model.map(str::to_string),
        }
    }

    /// The concrete model for this request: the edit model when reference
    /// images are present (and one is configured), else the base model.
    fn model_for<'a>(&'a self, req: &ImageRequest) -> &'a str {
        if req.reference_images.is_empty() {
            &self.model
        } else {
            self.edit_model.as_deref().unwrap_or(&self.model)
        }
    }

    async fn post(&self, model: &str, action: &str, body: Value) -> Result<Value> {
        let key = self.api_key.get()?;
        let resp = self
            .client
            .post(format!(
                "{}/models/{model}:{action}?key={key}",
                self.base_url
            ))
            .json(&body)
            .send()
            .await
            .map_err(|e| eyre!("Gemini image HTTP request failed: {}", e.without_url()))?;
        let status = resp.status();
        let text = scrub(&resp.text().await.unwrap_or_default(), Some(&key));
        if !status.is_success() {
            return Err(eyre!("Gemini image generation failed ({status}): {text}"));
        }
        serde_json::from_str(&text).map_err(|e| eyre!("Gemini image response parse failed: {e}"))
    }
}

#[async_trait]
impl ImageProvider for GeminiImageProvider {
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>> {
        let model = self.model_for(req);
        if model.starts_with("imagen") {
            // Imagen's edit is mask-inpaint via :predict, not multi-image
            // composition — reject reference-image edits with an actionable hint
            // rather than silently dropping the references.
            if !req.reference_images.is_empty() {
                return Err(eyre!(
                    "Imagen models ('{model}') don't support reference-image editing/composition; \
                     configure a gemini-*-image model (e.g. gemini:powerful) for edits"
                ));
            }
            self.generate_imagen(model, req).await
        } else {
            self.generate_flash_image(model, req).await
        }
    }

    async fn verify_credentials(&self) -> Result<()> {
        // Cheapest auth-only probe: GET the model metadata. A bad key returns
        // 400/403; a missing model returns 404 with a clear message.
        let key = self.api_key.get()?;
        let url = format!("{}/models/{}?key={key}", self.base_url, self.model);
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| eyre!("Gemini image credential probe failed: {}", e.without_url()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let text = scrub(&resp.text().await.unwrap_or_default(), Some(&key));
            Err(eyre!(
                "Gemini image credentials/model check failed ({status}): {text}"
            ))
        }
    }
}

impl GeminiImageProvider {
    async fn generate_imagen(
        &self,
        model: &str,
        req: &ImageRequest,
    ) -> Result<Vec<GeneratedImage>> {
        let body = json!({
            "instances": [{ "prompt": req.prompt }],
            "parameters": {
                "sampleCount": req.n.max(1),
                "aspectRatio": req.size.nearest_aspect_ratio(),
            }
        });
        let value = self.post(model, "predict", body).await?;
        let predictions = value["predictions"]
            .as_array()
            .ok_or_else(|| eyre!("Imagen response missing 'predictions': {value}"))?;
        let out = predictions
            .iter()
            .map(|p| {
                let b64 = p["bytesBase64Encoded"]
                    .as_str()
                    .ok_or_else(|| eyre!("Imagen prediction missing image bytes"))?;
                Ok(GeneratedImage {
                    data: BASE64
                        .decode(b64)
                        .map_err(|e| eyre!("Imagen base64 decode failed: {e}"))?,
                    media_type: p["mimeType"].as_str().unwrap_or("image/png").to_string(),
                    revised_prompt: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if out.is_empty() {
            return Err(eyre!("Imagen returned no images (prompt may be blocked)"));
        }
        Ok(out)
    }

    async fn generate_flash_image(
        &self,
        model: &str,
        req: &ImageRequest,
    ) -> Result<Vec<GeneratedImage>> {
        // Reference images go in first as inline_data parts, then the text
        // prompt (which refers to them positionally) — this is the multi-image
        // composition path ("place the person from image 2 into image 1").
        let mut parts: Vec<Value> = req
            .reference_images
            .iter()
            .map(|img| {
                json!({
                    "inlineData": {
                        "mimeType": img.media_type,
                        "data": BASE64.encode(&img.data),
                    }
                })
            })
            .collect();
        parts.push(json!({ "text": req.prompt }));
        let body = json!({
            "contents": [{ "parts": parts }],
            "generationConfig": { "responseModalities": ["IMAGE"] }
        });
        let value = self.post(model, "generateContent", body).await?;
        let parts = value["candidates"][0]["content"]["parts"]
            .as_array()
            .ok_or_else(|| eyre!("Gemini image response missing candidate parts: {value}"))?;
        let out = parts
            .iter()
            .filter_map(|part| {
                let inline = &part["inlineData"];
                inline["data"].as_str().map(|b64| {
                    Ok(GeneratedImage {
                        data: BASE64
                            .decode(b64)
                            .map_err(|e| eyre!("Gemini image base64 decode failed: {e}"))?,
                        media_type: inline["mimeType"]
                            .as_str()
                            .unwrap_or("image/png")
                            .to_string(),
                        revised_prompt: None,
                    })
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if out.is_empty() {
            return Err(eyre!(
                "Gemini returned no image parts (prompt may be blocked by safety filters)"
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::secret::NoKey;

    use crate::runner::CURRENT_USER_ID;

    const KEY: &str = "AIza-alices-secret-key";

    /// Alice's key for Alice, nobody's for anyone else.
    fn provider(base_url: String) -> GeminiImageProvider {
        let alices = KeySlot::new(
            std::sync::Arc::new(|| {
                CURRENT_USER_ID
                    .try_with(Clone::clone)
                    .ok()
                    .filter(|person| person == "alice")
                    .map(|_| KEY.to_string())
            }),
            "no image key for you",
        );
        GeminiImageProvider {
            base_url,
            ..GeminiImageProvider::new(alices, "imagen-4", None)
        }
    }

    #[tokio::test]
    async fn the_callers_key_rides_the_url_and_an_upstream_error_never_echoes_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/imagen-4:predict"))
            .and(query_param("key", KEY))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(format!("API key not valid: {KEY}. Pass a valid key.")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider(server.uri());

        let refused = CURRENT_USER_ID
            .scope(
                "alice".into(),
                provider.post("imagen-4", "predict", json!({})),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("400") && refused.contains("<redacted>"),
            "{refused}"
        );
        assert!(!refused.contains("AIza"), "{refused}");

        let keyless = CURRENT_USER_ID
            .scope(
                "bob".into(),
                provider.post("imagen-4", "predict", json!({})),
            )
            .await
            .unwrap_err();
        assert_eq!(
            keyless.downcast_ref::<NoKey>(),
            Some(&NoKey("no image key for you".into()))
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "nothing is sent for someone with no key"
        );
    }

    #[tokio::test]
    async fn a_transport_error_is_reported_without_the_url_that_carries_the_key() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let provider = provider(closed);
        for failed in [
            CURRENT_USER_ID
                .scope(
                    "alice".into(),
                    provider.post("imagen-4", "predict", json!({})),
                )
                .await
                .map(drop),
            CURRENT_USER_ID
                .scope("alice".into(), provider.verify_credentials())
                .await,
        ] {
            let failed = format!("{:?}", failed.unwrap_err());
            assert!(failed.contains("Gemini image"), "{failed}");
            assert!(
                !failed.contains("AIza")
                    && !failed.contains("key=")
                    && !failed.contains("127.0.0.1"),
                "{failed}"
            );
        }
    }
}
