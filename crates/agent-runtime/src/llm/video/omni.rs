//! Gemini Omni video generation via the Gemini API's Interactions endpoint.
//!
//! Background create → poll the stored interaction → read the clip, which
//! arrives inline as base64 (or as a file URI). The key travels in the
//! `x-goog-api-key` header, never the URL, and only to the API's own origin:
//! the keyed client follows no redirect off it (reqwest strips only its
//! standard credential headers on a cross-host redirect, not this one), and a
//! clip hosted elsewhere is fetched without the key.
//!
//! Verified live (2026-10-27): a 10s 360p text-to-video completed in ~21s, a
//! 4s 360p image-to-video in ~14s, both H.264 + AAC MP4 returned inline. The
//! poll loop tolerates transient 5xx/429 — a live Veo run hit a 503 mid-poll.
//!
//! A refusal has not been observed, so all three plausible shapes end in an
//! error the agent can relay: a 4xx on create, a `failed` interaction, and a
//! `completed` one whose output is text instead of a video.

use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::{eyre, Result};
use reqwest::{redirect, Client, RequestBuilder, Response, StatusCode, Url};
use serde_json::{json, Value};
use workforce_common::secret::{scrub, KeySlot};

use super::{GeneratedVideo, Resolution, VideoProvider, VideoRequest};

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Pins the Interactions response schema this client parses.
const API_REVISION: &str = "2026-05-20";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 4k is upscaled server-side, far slower than the ~20s measured at 360p.
/// `generate_video` declares no tool deadline, so this is the only limit.
const DEADLINE: Duration = Duration::from_secs(600);
/// Each request is quick, but a finished interaction carries the whole clip
/// inline and a 4k clip runs to tens of MB.
const REQUEST_TIMEOUT_SECS: u64 = 180;
const DEFAULT_MEDIA_TYPE: &str = "video/mp4";

pub struct OmniVideoProvider {
    /// Carries the key; follows redirects only within the API's origin.
    keyed: Client,
    /// Fetches clips hosted off the API, never with the key.
    plain: Client,
    base_url: String,
    poll_interval: Duration,
    api_key: KeySlot,
    model: String,
    resolution: Option<Resolution>,
}

impl OmniVideoProvider {
    pub fn new(api_key: KeySlot, model: String, resolution: Option<Resolution>) -> Self {
        Self::at(API_BASE, POLL_INTERVAL, api_key, model, resolution)
    }

    fn at(
        base_url: &str,
        poll_interval: Duration,
        api_key: KeySlot,
        model: String,
        resolution: Option<Resolution>,
    ) -> Self {
        Self {
            keyed: keyed_client(base_url),
            plain: crate::llm::image::http_client(REQUEST_TIMEOUT_SECS),
            base_url: base_url.to_string(),
            poll_interval,
            api_key,
            model,
            resolution,
        }
    }

    async fn create(&self, req: &VideoRequest, key: &str) -> Result<String> {
        let body = interaction_body(&self.model, self.resolution, req);
        let response = authed(
            self.keyed
                .post(format!("{}/interactions", self.base_url))
                .json(&body),
            key,
        )
        .send()
        .await
        .map_err(|e| eyre!("Omni request failed: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(eyre!(
                "Omni refused the request ({status}): {}",
                refusal(response, key).await
            ));
        }
        let created: Value = response
            .json()
            .await
            .map_err(|e| eyre!("Omni create response read failed: {}", e.without_url()))?;
        created["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| eyre!("Omni create response carried no interaction id"))
    }

    /// Poll the stored interaction until it completes, retrying only what may
    /// pass: a request that never got an answer, a 5xx, a 429.
    async fn completion(&self, stored: &mut Stored, key: &str) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        let mut last_hiccup: Option<String> = None;
        loop {
            if tokio::time::Instant::now() >= deadline {
                let cause = last_hiccup.map(|e| format!(" (last poll: {e})"));
                return Err(eyre!(
                    "Omni generation timed out after {}s{}",
                    DEADLINE.as_secs(),
                    cause.unwrap_or_default()
                ));
            }
            tokio::time::sleep(self.poll_interval).await;
            let response = match authed(self.keyed.get(&stored.url), key).send().await {
                Ok(response) => response,
                Err(e) => {
                    last_hiccup = Some(e.without_url().to_string());
                    continue;
                }
            };
            let status = response.status();
            if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                last_hiccup = Some(status.to_string());
                continue;
            }
            if !status.is_success() {
                return Err(eyre!(
                    "Omni poll failed ({status}): {}",
                    refusal(response, key).await
                ));
            }
            let interaction: Value = response
                .json()
                .await
                .map_err(|e| eyre!("Omni poll response read failed: {}", e.without_url()))?;
            match progress(interaction) {
                Progress::Running => {}
                Progress::Completed(interaction) => {
                    stored.running = false;
                    return Ok(interaction);
                }
                Progress::Ended(error) => {
                    stored.running = false;
                    return Err(error);
                }
                Progress::Stuck(error) => return Err(error),
            }
        }
    }

    async fn download(&self, uri: &str, key: &str) -> Result<Vec<u8>> {
        let failed = |e: reqwest::Error| eyre!("Omni video download failed: {}", e.without_url());
        let response = if same_origin(uri, &self.base_url) {
            let response = authed(self.keyed.get(uri), key)
                .send()
                .await
                .map_err(failed)?;
            match redirected_off(&response) {
                Some(elsewhere) => self.plain.get(elsewhere).send().await.map_err(failed)?,
                None => response,
            }
        } else {
            self.plain.get(uri).send().await.map_err(failed)?
        };
        if !response.status().is_success() {
            return Err(eyre!("Omni video download failed ({})", response.status()));
        }
        let bytes = response.bytes().await.map_err(failed)?;
        Ok(bytes.to_vec())
    }
}

#[async_trait]
impl VideoProvider for OmniVideoProvider {
    async fn generate(&self, req: &VideoRequest) -> Result<GeneratedVideo> {
        // The key is read once, so whoever asked pays for the create, the
        // polls and the download alike.
        let key = self.api_key.get()?;
        let id = self.create(req, &key).await?;
        let mut stored = Stored {
            client: self.keyed.clone(),
            url: format!("{}/interactions/{id}", self.base_url),
            key: key.clone(),
            running: true,
        };
        let interaction = self.completion(&mut stored, &key).await?;
        match clip(&interaction)? {
            Clip::Inline { data, media_type } => Ok(GeneratedVideo { data, media_type }),
            Clip::Uri { uri, media_type } => Ok(GeneratedVideo {
                data: self.download(&uri, &key).await?,
                media_type,
            }),
        }
    }
}

/// The client that carries the key: it follows a redirect only within
/// `base_url`'s origin and hands any other back as the response.
fn keyed_client(base_url: &str) -> Client {
    let base = base_url.to_string();
    Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .referer(false)
        .redirect(redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() < 10 && same_origin(attempt.url().as_str(), &base) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .unwrap_or_default()
}

fn authed(request: RequestBuilder, key: &str) -> RequestBuilder {
    request
        .header("x-goog-api-key", key)
        .header("Api-Revision", API_REVISION)
}

/// Google's message from an error response, the key scrubbed out.
async fn refusal(response: Response, key: &str) -> String {
    let body = scrub(&response.text().await.unwrap_or_default(), Some(key));
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or(body)
}

/// Where a redirect the keyed client declined to follow points.
fn redirected_off(response: &Response) -> Option<Url> {
    if !response.status().is_redirection() {
        return None;
    }
    let location = response.headers().get(reqwest::header::LOCATION)?;
    response.url().join(location.to_str().ok()?).ok()
}

/// Whether `uri` shares `base`'s scheme, host and port — the only origin the
/// key is sent to.
fn same_origin(uri: &str, base: &str) -> bool {
    match (Url::parse(uri), Url::parse(base)) {
        (Ok(uri), Ok(base)) => {
            uri.scheme() == base.scheme()
                && uri.host_str() == base.host_str()
                && uri.port_or_known_default() == base.port_or_known_default()
        }
        _ => false,
    }
}

/// The interaction behind one generation, stored at Google until dropped.
/// Dropped while it runs — the deadline, a stopped turn dropping the tool
/// call — it is cancelled, so Google stops generating (and billing) a clip
/// nobody will collect. Either way it is then deleted: nothing reads it
/// again, and it holds the request's input images. Best effort, off the
/// caller's path.
struct Stored {
    client: Client,
    url: String,
    key: String,
    running: bool,
}

impl Drop for Stored {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (client, url, key, running) = (
            self.client.clone(),
            std::mem::take(&mut self.url),
            std::mem::take(&mut self.key),
            self.running,
        );
        runtime.spawn(async move {
            let calls = running
                .then(|| client.post(format!("{url}/cancel")))
                .into_iter()
                .chain([client.delete(&url)]);
            for call in calls {
                if let Err(e) = authed(call, &key)
                    .send()
                    .await
                    .and_then(Response::error_for_status)
                {
                    tracing::debug!(error = %e.without_url(), "Omni interaction cleanup failed");
                }
            }
        });
    }
}

/// The create body: background, the prompt alone or the images in order
/// followed by the prompt, and the clip's shape.
fn interaction_body(model: &str, resolution: Option<Resolution>, req: &VideoRequest) -> Value {
    let input = if req.images.is_empty() {
        json!(req.prompt)
    } else {
        let images = req.images.iter().map(|image| {
            json!({
                "type": "image",
                "data": BASE64.encode(&image.data),
                "mime_type": image.media_type,
            })
        });
        let prompt = json!({ "type": "text", "text": req.prompt });
        Value::Array(images.chain([prompt]).collect())
    };
    let mut format = json!({ "type": "video", "aspect_ratio": req.aspect_ratio.as_str() });
    if let Some(resolution) = resolution {
        format["resolution"] = json!(resolution.as_str());
    }
    if let Some(secs) = req.duration_secs {
        format["duration"] = json!(format!("{secs}s"));
    }
    json!({
        "model": model,
        "background": true,
        "input": input,
        "response_format": format,
    })
}

/// Where a polled interaction stands.
#[derive(Debug)]
enum Progress {
    /// `in_progress`, or a status this client doesn't know: keep polling.
    Running,
    Completed(Value),
    /// `failed` or `cancelled`: over, with nothing to cancel.
    Ended(eyre::Report),
    /// `requires_action`: paused for an input this client never gives.
    Stuck(eyre::Report),
}

fn progress(interaction: Value) -> Progress {
    let failure = |status: &str| match interaction.get("error").filter(|e| !e.is_null()) {
        Some(error) => eyre!("Omni generation {status}: {error}"),
        None => eyre!("Omni generation {status}"),
    };
    match interaction["status"].as_str() {
        Some("completed") => Progress::Completed(interaction),
        Some(status @ ("failed" | "cancelled")) => Progress::Ended(failure(status)),
        Some("requires_action") => Progress::Stuck(eyre!(
            "Omni generation paused for an action it can't be given"
        )),
        _ => Progress::Running,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Clip {
    Inline { data: Vec<u8>, media_type: String },
    Uri { uri: String, media_type: String },
}

/// The video in a completed interaction's model output; a completed
/// interaction without one is a refusal, explained by its text.
fn clip(interaction: &Value) -> Result<Clip> {
    let output: Vec<&Value> = interaction["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|step| step["type"] == "model_output")
        .flat_map(|step| step["content"].as_array().into_iter().flatten())
        .collect();
    let Some(video) = output.iter().find(|part| part["type"] == "video") else {
        let said: Vec<&str> = output
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect();
        return Err(if said.is_empty() {
            eyre!("Omni returned no video")
        } else {
            eyre!("Omni returned no video: {}", said.join(" "))
        });
    };
    let media_type = video["mime_type"]
        .as_str()
        .unwrap_or(DEFAULT_MEDIA_TYPE)
        .to_string();
    if let Some(b64) = video["data"].as_str() {
        let data = BASE64
            .decode(b64.trim())
            .map_err(|e| eyre!("Omni video base64 decode failed: {e}"))?;
        return Ok(Clip::Inline { data, media_type });
    }
    video["uri"]
        .as_str()
        .map(|uri| Clip::Uri {
            uri: uri.to_string(),
            media_type,
        })
        .ok_or_else(|| eyre!("Omni video output carried neither data nor a uri"))
}

#[cfg(test)]
mod tests {
    use super::super::AspectRatio;
    use super::*;
    use crate::llm::image::InputImage;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const KEY: &str = "AIzaSyTESTKEY000000000000000000000000";

    fn request(images: Vec<InputImage>, duration_secs: Option<u32>) -> VideoRequest {
        VideoRequest {
            prompt: "a paper boat".into(),
            images,
            aspect_ratio: AspectRatio::Portrait,
            duration_secs,
        }
    }

    fn image(byte: u8, media_type: &str) -> InputImage {
        InputImage {
            data: vec![byte],
            media_type: media_type.into(),
        }
    }

    /// A completed interaction as Omni returned it on 2026-10-27, its
    /// signature and base64 trimmed.
    fn completed(output: Value) -> Value {
        json!({
            "id": "v1_abc",
            "status": "completed",
            "steps": [
                {"type": "user_input", "content": [{"type": "text", "text": "a paper boat"}]},
                {"type": "thought", "signature": "Eo9O"},
                {"type": "model_output", "content": [output]},
            ],
        })
    }

    fn provider(server: &MockServer) -> OmniVideoProvider {
        OmniVideoProvider::at(
            &server.uri(),
            Duration::from_millis(10),
            KeySlot::fixed(KEY),
            "omni".into(),
            None,
        )
    }

    async fn mount(server: &MockServer, verb: &str, at: &str, reply: ResponseTemplate) {
        Mock::given(method(verb))
            .and(path(at))
            .and(header("x-goog-api-key", KEY))
            .respond_with(reply)
            .mount(server)
            .await;
    }

    async fn created(server: &MockServer) {
        let reply = ResponseTemplate::new(200)
            .set_body_json(json!({"id": "v1_abc", "status": "in_progress"}));
        mount(server, "POST", "/interactions", reply).await;
        mount(
            server,
            "DELETE",
            "/interactions/v1_abc",
            ResponseTemplate::new(200),
        )
        .await;
        mount(
            server,
            "POST",
            "/interactions/v1_abc/cancel",
            ResponseTemplate::new(200),
        )
        .await;
    }

    /// The paths `server` was called on, once the spawned cleanup has had its
    /// chance to land.
    async fn calls(server: &MockServer, settled_after: &str) -> Vec<String> {
        for _ in 0..100 {
            let received = server.received_requests().await.unwrap();
            let seen: Vec<String> = received
                .iter()
                .map(|r: &Request| format!("{} {}", r.method, r.url.path()))
                .collect();
            if seen.iter().any(|call| call == settled_after) {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{settled_after} never arrived");
    }

    #[test]
    fn a_prompt_alone_is_sent_as_the_input_string() {
        let body = interaction_body("omni", None, &request(Vec::new(), None));
        assert_eq!(
            body,
            json!({
                "model": "omni",
                "background": true,
                "input": "a paper boat",
                "response_format": {"type": "video", "aspect_ratio": "9:16"},
            })
        );
    }

    #[test]
    fn images_precede_the_prompt_in_order_with_the_clips_shape() {
        let body = interaction_body(
            "omni",
            Some(Resolution::P1080),
            &request(vec![image(1, "image/png"), image(2, "image/jpeg")], Some(6)),
        );
        assert_eq!(
            body["input"],
            json!([
                {"type": "image", "data": BASE64.encode([1]), "mime_type": "image/png"},
                {"type": "image", "data": BASE64.encode([2]), "mime_type": "image/jpeg"},
                {"type": "text", "text": "a paper boat"},
            ])
        );
        assert_eq!(
            body["response_format"],
            json!({"type": "video", "aspect_ratio": "9:16", "resolution": "1080p", "duration": "6s"})
        );
    }

    #[test]
    fn a_completed_interaction_without_a_video_is_a_refusal_in_its_own_words() {
        let error = clip(&completed(
            json!({"type": "text", "text": "I can't animate that."}),
        ))
        .unwrap_err()
        .to_string();
        assert_eq!(error, "Omni returned no video: I can't animate that.");
        let silent = completed(json!({"type": "text"}));
        assert_eq!(
            clip(&silent).unwrap_err().to_string(),
            "Omni returned no video"
        );
    }

    #[test]
    fn only_a_documented_end_stops_the_polling() {
        assert!(matches!(
            progress(json!({"status": "in_progress"})),
            Progress::Running
        ));
        assert!(matches!(
            progress(json!({"status": "queued_somewhere_new"})),
            Progress::Running
        ));
        assert!(matches!(
            progress(json!({"status": "completed"})),
            Progress::Completed(_)
        ));
        let Progress::Ended(failed) =
            progress(json!({"status": "failed", "error": {"message": "rate limit"}}))
        else {
            panic!("failed is an end");
        };
        assert!(failed.to_string().contains("rate limit"), "{failed}");
        assert!(matches!(
            progress(json!({"status": "requires_action"})),
            Progress::Stuck(_)
        ));
    }

    #[test]
    fn the_key_goes_only_to_the_apis_own_origin() {
        let api = "https://generativelanguage.googleapis.com/v1beta";
        assert!(same_origin(
            "https://generativelanguage.googleapis.com/v1beta/files/abc:download?alt=media",
            api
        ));
        for elsewhere in [
            "https://storage.example.com/clip.mp4?sig=1",
            "http://generativelanguage.googleapis.com/v1beta/files/abc",
            "https://generativelanguage.googleapis.com:8443/v1beta/files/abc",
            "not a url",
        ] {
            assert!(!same_origin(elsewhere, api), "{elsewhere}");
        }
    }

    #[tokio::test]
    async fn a_clip_rides_out_a_busy_poll_and_its_interaction_is_deleted() {
        let server = MockServer::start().await;
        created(&server).await;
        Mock::given(method("GET"))
            .and(path("/interactions/v1_abc"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        let done = completed(json!({
            "type": "video", "mime_type": "video/mp4", "data": BASE64.encode(b"mp4"),
        }));
        mount(
            &server,
            "GET",
            "/interactions/v1_abc",
            ResponseTemplate::new(200).set_body_json(done),
        )
        .await;

        let video = provider(&server)
            .generate(&request(Vec::new(), None))
            .await
            .unwrap();
        assert_eq!(
            (video.data.as_slice(), video.media_type.as_str()),
            (&b"mp4"[..], "video/mp4")
        );

        let seen = calls(&server, "DELETE /interactions/v1_abc").await;
        assert!(
            !seen.iter().any(|call| call.ends_with("/cancel")),
            "{seen:?}"
        );
        let received = server.received_requests().await.unwrap();
        assert!(received
            .iter()
            .all(|r| !r.url.as_str().contains(KEY) && r.headers.get("x-goog-api-key").is_some()));
    }

    #[tokio::test]
    async fn a_refusal_names_googles_reason_without_the_key() {
        let server = MockServer::start().await;
        let echo = json!({"error": {"code": 400, "message": format!("API key {KEY} rejected")}});
        mount(
            &server,
            "POST",
            "/interactions",
            ResponseTemplate::new(400).set_body_json(echo),
        )
        .await;
        let refused = provider(&server)
            .generate(&request(Vec::new(), None))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("400") && refused.contains("rejected"),
            "{refused}"
        );
        assert!(!refused.contains(KEY), "{refused}");
    }

    #[tokio::test]
    async fn a_failed_generation_is_deleted_not_cancelled() {
        let server = MockServer::start().await;
        created(&server).await;
        let failed = json!({"id": "v1_abc", "status": "failed", "error": {"message": "unsafe"}});
        mount(
            &server,
            "GET",
            "/interactions/v1_abc",
            ResponseTemplate::new(200).set_body_json(failed),
        )
        .await;
        let error = provider(&server)
            .generate(&request(Vec::new(), None))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsafe"), "{error}");
        let seen = calls(&server, "DELETE /interactions/v1_abc").await;
        assert!(
            !seen.iter().any(|call| call.ends_with("/cancel")),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn an_abandoned_generation_is_cancelled_then_deleted() {
        let server = MockServer::start().await;
        created(&server).await;
        let running = json!({"id": "v1_abc", "status": "in_progress"});
        mount(
            &server,
            "GET",
            "/interactions/v1_abc",
            ResponseTemplate::new(200).set_body_json(running),
        )
        .await;
        let provider = provider(&server);
        let stopped = tokio::time::timeout(
            Duration::from_millis(100),
            provider.generate(&request(Vec::new(), None)),
        )
        .await;
        assert!(stopped.is_err(), "the turn stopped it mid-generation");
        let seen = calls(&server, "DELETE /interactions/v1_abc").await;
        assert!(
            seen.contains(&"POST /interactions/v1_abc/cancel".to_string()),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn the_key_never_follows_a_clip_off_the_api() {
        let api = MockServer::start().await;
        let storage = MockServer::start().await;
        created(&api).await;
        let at_api = format!("{}/files/abc:download", api.uri());
        mount(
            &api,
            "GET",
            "/interactions/v1_abc",
            ResponseTemplate::new(200).set_body_json(completed(json!({
                "type": "video", "mime_type": "video/mp4", "uri": at_api,
            }))),
        )
        .await;
        mount(
            &api,
            "GET",
            "/files/abc:download",
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/clip.mp4", storage.uri())),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/clip.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"mp4".to_vec()))
            .mount(&storage)
            .await;

        let video = provider(&api)
            .generate(&request(Vec::new(), None))
            .await
            .unwrap();
        assert_eq!(video.data, b"mp4");
        let fetched = storage.received_requests().await.unwrap();
        assert_eq!(fetched.len(), 1);
        assert!(fetched[0].headers.get("x-goog-api-key").is_none());
    }
}
