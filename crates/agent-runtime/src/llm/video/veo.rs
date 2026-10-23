//! Google Veo video generation via the Generative Language REST API.
//!
//! Long-running: submit to `:predictLongRunning` → returns an operation name →
//! poll the operation until `done` → the result carries a **file URI** (not
//! inline base64), which we download with the api key appended. API-key auth via
//! `?key=`, matching [`crate::llm::image::gemini`].
//!
//! Verified live (2026-06-08): `veo-3.0-fast` returned an 8s 720p H.264+AAC clip
//! in ~40s; image-to-video of a real person works with
//! `personGeneration: "allow_adult"`. The poll loop **must** tolerate transient
//! 5xx/429 — a live run hit a 503 mid-poll.
//!
//! `personGeneration` is mode-dependent (verified live 2026-06-13): Google
//! accepts `allow_all` for text-to-video and `allow_adult` for image-to-video
//! (and only `allow_adult` in EU/UK/CH/MENA), rejecting the wrong value with a
//! 400 "… is currently not supported". [`person_generation_candidates`] picks
//! the mode-correct value first and falls back to the alternate, so a single
//! `[llm.video] person_generation` setting works across modes and regions.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use reqwest::Client;
use serde_json::{json, Value};
use workforce_common::secret::{scrub, KeySlot};

use super::{GeneratedVideo, VideoProvider, VideoRequest};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Seconds between operation polls.
const POLL_INTERVAL_SECS: u64 = 8;
/// Max polls before giving up (≈ 8 min ceiling; Veo is typically 1–6 min).
const MAX_POLLS: u32 = 60;

pub struct VeoVideoProvider {
    client: Client,
    api_key: KeySlot,
    model: String,
}

impl VeoVideoProvider {
    pub fn new(api_key: KeySlot, model: impl Into<String>) -> Self {
        Self {
            // Generous per-request timeout: submit + each poll are quick, but the
            // final file download can be a few MB.
            client: super::super::image::http_client(180),
            api_key,
            model: model.into(),
        }
    }

    fn build_instance(&self, req: &VideoRequest) -> Value {
        let mut instance = json!({ "prompt": req.prompt });
        if let Some(img) = &req.input_image {
            instance["image"] = json!({
                "bytesBase64Encoded": BASE64.encode(&img.data),
                "mimeType": img.media_type,
            });
        }
        instance
    }

    fn build_parameters(&self, req: &VideoRequest, person_generation: &str) -> Value {
        let mut params = json!({
            "aspectRatio": req.aspect_ratio,
            "personGeneration": person_generation,
        });
        if let Some(d) = req.duration_secs {
            params["durationSeconds"] = json!(d);
        }
        params
    }
}

/// Provider-accepted `personGeneration` values to try, in order, for this
/// request.
///
/// Google accepts a DIFFERENT value per mode — text-to-video wants
/// `allow_all`, image-to-video wants `allow_adult` — and flips both to
/// `allow_adult`-only in EU/UK/CH/MENA regions. Pinning a single value
/// therefore breaks either the other mode or the other region (verified live
/// 2026-06-13: `allow_all` works for text-to-video but is rejected for
/// image-to-video, and vice-versa). So we try the mode-correct value first and
/// fall back to the alternate on a "not supported" rejection: the first
/// attempt is right in the common case (no extra call) and it self-heals
/// everywhere else, including future provider changes. `dont_allow` is honored
/// verbatim — never send a people-allowing value when the operator opted out.
fn person_generation_candidates(req: &VideoRequest) -> Vec<&'static str> {
    if req.person_generation.eq_ignore_ascii_case("dont_allow") {
        return vec!["dont_allow"];
    }
    if req.input_image.is_some() {
        // image-to-video / interpolation / reference images
        vec!["allow_adult", "allow_all"]
    } else {
        // text-to-video
        vec!["allow_all", "allow_adult"]
    }
}

/// Whether a failed submit is a `personGeneration`-value rejection we should
/// retry with the alternate candidate (rather than a real error to surface).
fn is_person_generation_rejection(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("persongeneration") && b.contains("not supported")
}

#[async_trait]
impl VideoProvider for VeoVideoProvider {
    async fn generate(&self, req: &VideoRequest) -> Result<GeneratedVideo> {
        // 1) Submit the long-running operation. Google accepts a different
        // `personGeneration` value per mode/region, so try the mode-correct
        // value first and fall back to the alternate on a "not supported"
        // rejection (see `person_generation_candidates`). The key is read
        // once, so whoever asked pays for the submit, the polls and the
        // download alike.
        let key = self.api_key.get()?;
        let submit_url = format!(
            "{API_BASE}/models/{model}:predictLongRunning?key={key}",
            model = self.model,
        );
        let candidates = person_generation_candidates(req);
        let mut op: Option<String> = None;
        let mut last_err: Option<eyre::Error> = None;
        for (i, pg) in candidates.iter().enumerate() {
            let body = json!({
                "instances": [self.build_instance(req)],
                "parameters": self.build_parameters(req, pg),
            });
            let resp = self
                .client
                .post(&submit_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| eyre!("Veo submit request failed: {}", e.without_url()))?;
            let status = resp.status();
            let text = scrub(&resp.text().await.unwrap_or_default(), Some(&key));
            if status.is_success() {
                let submit: Value = serde_json::from_str(&text)
                    .map_err(|e| eyre!("Veo submit response parse failed: {e}"))?;
                op = Some(
                    submit["name"]
                        .as_str()
                        .ok_or_else(|| {
                            eyre!("Veo submit response missing operation 'name': {submit}")
                        })?
                        .to_string(),
                );
                break;
            }
            // Only fall through to the next candidate when the provider rejected
            // the personGeneration value itself and an alternate remains; any
            // other failure is a real error to surface immediately.
            let has_fallback = i + 1 < candidates.len();
            if status.as_u16() == 400 && has_fallback && is_person_generation_rejection(&text) {
                tracing::info!(
                    rejected = pg,
                    fallback = candidates[i + 1],
                    "Veo rejected personGeneration value; retrying with fallback"
                );
                last_err = Some(eyre!("Veo submit failed ({status}): {text}"));
                continue;
            }
            return Err(eyre!("Veo submit failed ({status}): {text}"));
        }
        let op = op.ok_or_else(|| {
            last_err.unwrap_or_else(|| {
                eyre!("Veo submit failed: no personGeneration candidate accepted")
            })
        })?;

        // 2) Poll the operation until done, tolerating transient 5xx/429.
        let op_url = format!("{API_BASE}/{op}?key={key}");
        let mut done: Option<Value> = None;
        for _ in 0..MAX_POLLS {
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
            let r = match self.client.get(&op_url).send().await {
                Ok(r) => r,
                Err(_) => continue, // transient transport error — retry
            };
            let st = r.status();
            if st.is_server_error() || st.as_u16() == 429 {
                continue; // transient backend hiccup — retry (the 503 the probe hit)
            }
            let body = r
                .text()
                .await
                .map(|body| scrub(&body, Some(&key)))
                .map_err(|e| eyre!("Veo poll read failed: {}", e.without_url()))?;
            if !st.is_success() {
                return Err(eyre!("Veo poll failed ({st}): {body}"));
            }
            let od: Value = serde_json::from_str(&body)
                .map_err(|e| eyre!("Veo poll response parse failed: {e}"))?;
            if od["done"].as_bool().unwrap_or(false) {
                if let Some(err) = od.get("error").filter(|e| !e.is_null()) {
                    return Err(eyre!("Veo generation failed: {err}"));
                }
                done = Some(od);
                break;
            }
        }
        let od = done.ok_or_else(|| {
            eyre!(
                "Veo generation timed out after {}s",
                MAX_POLLS as u64 * POLL_INTERVAL_SECS
            )
        })?;

        // 3) Extract the result: inline base64 if present, else a file URI to
        // download (the verified shape).
        if let Some(b64) = find_first_str(&od, &["bytesBase64Encoded", "videoBytes"]) {
            let data = BASE64
                .decode(b64.trim())
                .map_err(|e| eyre!("Veo video base64 decode failed: {e}"))?;
            return Ok(GeneratedVideo {
                data,
                media_type: "video/mp4".to_string(),
            });
        }
        if let Some(uri) = find_first_str(&od, &["uri"]) {
            let r = self
                .client
                .get(download_url(&uri, &key))
                .send()
                .await
                .map_err(|e| eyre!("Veo video download failed: {}", e.without_url()))?;
            if !r.status().is_success() {
                return Err(eyre!("Veo video download failed ({})", r.status()));
            }
            let bytes = r
                .bytes()
                .await
                .map_err(|e| eyre!("Veo video download read failed: {}", e.without_url()))?;
            return Ok(GeneratedVideo {
                data: bytes.to_vec(),
                media_type: "video/mp4".to_string(),
            });
        }
        Err(eyre!(
            "Veo operation completed but carried no video (uri/bytes): {od}"
        ))
    }

    async fn verify_credentials(&self) -> Result<()> {
        let url = format!(
            "{API_BASE}/models/{model}?key={key}",
            model = self.model,
            key = self.api_key.get()?,
        );
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| eyre!("Veo credential probe failed: {}", e.without_url()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(eyre!(
                "Veo credentials/model check failed ({})",
                resp.status()
            ))
        }
    }
}

/// Where a finished clip is fetched from: `uri` with the key when it is on
/// the Gemini API's own host, and `uri` alone anywhere else, so the key is
/// only ever sent to the provider that issued it.
fn download_url(uri: &str, key: &str) -> String {
    let own = reqwest::Url::parse(uri)
        .ok()
        .zip(reqwest::Url::parse(API_BASE).ok());
    match own {
        Some((uri, api)) if uri.scheme() == "https" && uri.host_str() == api.host_str() => {
            let mut uri = uri;
            uri.query_pairs_mut().append_pair("key", key);
            uri.into()
        }
        _ => uri.to_string(),
    }
}

/// Recursively find the first string value under any key in `keys`.
fn find_first_str(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if keys.contains(&k.as_str()) {
                    if let Some(s) = v.as_str() {
                        return Some(s.to_string());
                    }
                }
                if let Some(found) = find_first_str(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_first_str(v, keys)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_is_fetched_with_the_key_only_from_the_gemini_api_itself() {
        assert_eq!(
            download_url(
                "https://generativelanguage.googleapis.com/v1beta/files/abc:download?alt=media",
                "sk-veo"
            ),
            "https://generativelanguage.googleapis.com/v1beta/files/abc:download?alt=media&key=sk-veo"
        );
        assert_eq!(
            download_url(
                "https://generativelanguage.googleapis.com/v1beta/files/abc",
                "sk-veo"
            ),
            "https://generativelanguage.googleapis.com/v1beta/files/abc?key=sk-veo"
        );
        for elsewhere in [
            "https://storage.example.com/clip.mp4?sig=1",
            "http://generativelanguage.googleapis.com/v1beta/files/abc",
            "https://generativelanguage.googleapis.com.evil.example/files/abc",
            "not a url",
        ] {
            assert_eq!(download_url(elsewhere, "sk-veo"), elsewhere);
        }
    }

    #[test]
    fn build_instance_includes_image_when_present() {
        let p = VeoVideoProvider::new(KeySlot::fixed("k"), "veo-3.0-fast-generate-001");
        let req = VideoRequest {
            prompt: "dance".into(),
            input_image: Some(crate::llm::image::InputImage {
                data: vec![1, 2, 3],
                media_type: "image/png".into(),
            }),
            aspect_ratio: "16:9".into(),
            duration_secs: Some(6),
            person_generation: "allow_adult".into(),
        };
        let inst = p.build_instance(&req);
        assert_eq!(inst["prompt"], "dance");
        assert_eq!(inst["image"]["mimeType"], "image/png");
        assert!(inst["image"]["bytesBase64Encoded"].is_string());
        let params = p.build_parameters(&req, "allow_adult");
        assert_eq!(params["aspectRatio"], "16:9");
        assert_eq!(params["personGeneration"], "allow_adult");
        assert_eq!(params["durationSeconds"], 6);
    }

    fn req_with(input_image: bool, person_generation: &str) -> VideoRequest {
        VideoRequest {
            prompt: "x".into(),
            input_image: input_image.then(|| crate::llm::image::InputImage {
                data: vec![1],
                media_type: "image/png".into(),
            }),
            aspect_ratio: "16:9".into(),
            duration_secs: None,
            person_generation: person_generation.into(),
        }
    }

    #[test]
    fn person_generation_candidates_are_mode_aware() {
        // Text-to-video tries allow_all first (the global-accepted value), then
        // falls back to allow_adult (the EU/UK-accepted value).
        assert_eq!(
            person_generation_candidates(&req_with(false, "allow_adult")),
            vec!["allow_all", "allow_adult"]
        );
        // Image-to-video is the reverse: allow_adult first, allow_all fallback.
        assert_eq!(
            person_generation_candidates(&req_with(true, "allow_adult")),
            vec!["allow_adult", "allow_all"]
        );
        // The configured allow_* value doesn't reorder — both mean "allow
        // people"; only the mode decides the provider-accepted spelling.
        assert_eq!(
            person_generation_candidates(&req_with(false, "allow_all")),
            vec!["allow_all", "allow_adult"]
        );
    }

    #[test]
    fn dont_allow_is_honored_verbatim_no_fallback() {
        assert_eq!(
            person_generation_candidates(&req_with(false, "dont_allow")),
            vec!["dont_allow"]
        );
        assert_eq!(
            person_generation_candidates(&req_with(true, "dont_allow")),
            vec!["dont_allow"]
        );
    }

    #[test]
    fn detects_person_generation_rejection_only() {
        assert!(is_person_generation_rejection(
            "allow_all for personGeneration is currently not supported"
        ));
        assert!(is_person_generation_rejection(
            "allow_adult for personGeneration is currently not supported"
        ));
        // Unrelated 400s must NOT trigger the fallback retry.
        assert!(!is_person_generation_rejection("invalid aspectRatio"));
        assert!(!is_person_generation_rejection(
            "personGeneration accepted but quota exceeded"
        ));
    }

    #[test]
    fn find_first_str_walks_nested() {
        let v = json!({
            "response": { "generateVideoResponse": { "generatedSamples": [
                { "video": { "uri": "https://example/files/abc:download" } }
            ]}}
        });
        assert_eq!(
            find_first_str(&v, &["uri"]).as_deref(),
            Some("https://example/files/abc:download")
        );
        assert_eq!(find_first_str(&v, &["bytesBase64Encoded"]), None);
    }
}
