//! `generate_speech` — text-to-speech as an agent tool.
//!
//! Mirrors `generate_image`: the tool calls the configured
//! [`SpeechProvider`], offloads the audio bytes to the artifact store, and
//! returns only a small handle. The result rides the same delivery contract
//! as generated images (`GENERATED_IMAGES_KEY`), so the runner's sink posts
//! the clip to the originating surface — on Slack that renders as a playable
//! audio file in the channel/thread.
//!
//! The spoken text is normalized through a caller-supplied hook (the hub
//! passes its TTS normalizer, which strips markdown and emoji) so the model's
//! phrasing habits don't get read out loud.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmSpeechConfig;

use super::image_gen::GENERATED_IMAGES_KEY;
use super::registry::ToolRegistry;
use super::{Tool, ToolResult};
use crate::artifacts::store::global_store;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::llm::speech::{
    extension_for_speech, provider_from_config, SpeechProvider, SpeechRequest,
};

pub const SPEECH_TOOL_NAME: &str = "generate_speech";

/// Character cap on one clip — bounds provider cost and keeps clips at
/// voice-note length (~3 minutes spoken). OpenAI rejects >4096 anyway.
const MAX_SPEECH_CHARS: usize = 4000;

/// Hook that cleans text before it is spoken (markdown/emoji stripping).
/// Identity when the host doesn't wire one.
pub type SpeechNormalizer = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// A user's own voice, resolved at request time: `(uid, provider_id)` → the
/// voice in their vault tier (`users/{uid}/speech/{provider}/voice`). `None`
/// falls back to the configured shared voice.
pub type SpeechUserVoice = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// The internal identity UUID of the user whose turn this is, when the
/// task-local is populated (it is on normal conversational turns; background
/// jobs have no user and speak in the shared voice).
fn current_turn_user() -> Option<String> {
    crate::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// Provider-specific guidance folded into the tool description, mirroring the
/// gateway voice-mode directive so the model writes text that speaks well. A
/// voice that takes directions gets exactly the ones it performs, from the
/// same table its text is checked against.
fn provider_phrasing_rules(provider: &dyn SpeechProvider) -> String {
    if let Some(guidance) = provider.markup().guidance() {
        return guidance.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    match provider.provider_id() {
        "elevenlabs" => {
            "Write clean, natural sentences — the voice model handles emotion automatically; \
             no special tags."
        }
        "soniox" => {
            "Write clean, natural sentences; no special tags. Numbers, email addresses, and \
             codes are pronounced accurately as written — no need to spell them out."
        }
        "builtin" => {
            "Use plain text with simple sentence structure — this is a lightweight local \
             voice; no tags, and spell out anything that reads ambiguously."
        }
        _ => "Write clean, natural sentences; no special tags.",
    }
    .to_string()
}

pub struct SpeechGenerationTool {
    provider: Arc<dyn SpeechProvider>,
    agent_id: String,
    normalizer: Option<SpeechNormalizer>,
    user_voice: Option<SpeechUserVoice>,
    description: String,
}

impl SpeechGenerationTool {
    pub fn new(
        provider: Arc<dyn SpeechProvider>,
        agent_id: impl Into<String>,
        normalizer: Option<SpeechNormalizer>,
        user_voice: Option<SpeechUserVoice>,
    ) -> Self {
        let description = format!(
            "Convert text to speech and send it as a playable voice clip. On chat surfaces \
             that support audio files (Slack), the system posts the clip to the current \
             conversation automatically (voice: {label}); on the mobile/web app the clip is \
             staged but cannot be rendered yet — if the user is on mobile and asks for audio, \
             say voice clips are only available in Slack for now. Write the `text` exactly as \
             it should be SPOKEN: no markdown, emojis, lists, or code — say things naturally, \
             as in conversation. {rules} IMPORTANT: you will NOT see or hear the output in \
             your own context — that is expected, not a failure. Do not upload, re-send, or \
             verify it; just briefly say what you sent. To send the clip somewhere else \
             instead (a DM, another channel), set `deliver` false and call \
             messaging.send_file with the returned artifact_id. PER-USER voice & billing: \
             each user can register their own provider API key and voice, which \
             automatically override the shared ones for THEIR clips — they add them in \
             Settings → Integrations, or via `cmd: secret mykey <value>` then vault.set \
             path 'speech/{provider_id}/api_key' (and optionally .../voice) with the \
             default per-user scope. If synthesis fails with a missing/invalid key, relay \
             that exact remedy — do NOT delete or modify stored secrets.",
            label = provider.voice_label(),
            rules = provider_phrasing_rules(provider.as_ref()),
            provider_id = provider.provider_id(),
        );
        Self {
            provider,
            agent_id: agent_id.into(),
            normalizer,
            user_voice,
            description,
        }
    }
}

#[async_trait]
impl Tool for SpeechGenerationTool {
    fn name(&self) -> &str {
        SPEECH_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The words to speak, written exactly as they should sound. Plain conversational sentences only — no markdown, emojis, bullet points, or code."
                },
                "filename": {
                    "type": "string",
                    "description": "Optional filename for the delivered clip (extension is corrected to match the audio format). Default 'voice-note'."
                },
                "deliver": {
                    "type": "boolean",
                    "description": "Default true: post the clip to THIS conversation automatically. Set false to only stage it (then forward the artifact_id via messaging.send_file)."
                },
                "speed": {
                    "type": "number",
                    "minimum": 0.5,
                    "maximum": 2.0,
                    "description": "Speaking-rate multiplier for this clip. 1.0 = natural pace. Use 0.7–0.85 to speak slowly and clearly — right for language learners or dictation; use it whenever the user says you're speaking too fast. Omit for the configured default."
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, eyre::Error> {
        let started = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let Some(text) = input["text"].as_str().filter(|s| !s.trim().is_empty()) else {
            return Ok(ToolResult::failure(
                "generate_speech requires a non-empty 'text'",
                elapsed(started),
            ));
        };

        let spoken = match &self.normalizer {
            Some(normalize) => normalize(text),
            None => text.to_string(),
        };
        let spoken = spoken.trim();
        if spoken.is_empty() {
            return Ok(ToolResult::failure(
                "nothing speakable left after stripping formatting — pass plain sentences",
                elapsed(started),
            ));
        }
        if spoken.chars().count() > MAX_SPEECH_CHARS {
            return Ok(ToolResult::failure(
                format!(
                    "text is {} characters; the cap per clip is {MAX_SPEECH_CHARS}. Split it \
                     into shorter clips.",
                    spoken.chars().count()
                ),
                elapsed(started),
            ));
        }

        let speed = input["speed"]
            .as_f64()
            .map(|s| s as f32)
            .filter(|s| *s > 0.0);

        // The turn user's own voice, when they registered one — resolved per
        // message so the right voice speaks on an agent shared by many users.
        let voice = self
            .user_voice
            .as_ref()
            .zip(current_turn_user())
            .and_then(|(resolve, uid)| resolve(&uid, self.provider.provider_id()));

        let output = match self
            .provider
            .synthesize(&SpeechRequest {
                text: spoken,
                speed,
                voice,
            })
            .await
        {
            Ok(output) => output,
            Err(e) => {
                // Error-path visibility is non-negotiable: the failure must
                // land in the hub logs, not only inside the tool result where
                // just the model sees it.
                tracing::warn!(
                    agent = %self.agent_id,
                    provider = self.provider.provider_id(),
                    voice = %self.provider.voice_label(),
                    error = %e,
                    "speech synthesis failed"
                );
                return Ok(ToolResult::failure(
                    format!("speech synthesis failed: {e}"),
                    elapsed(started),
                ));
            }
        };

        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage generated speech",
                elapsed(started),
            ));
        };
        let descriptor = ArtifactDescriptor {
            media_type: Some(output.media_type.to_string()),
            ..ArtifactDescriptor::from_context(
                SPEECH_TOOL_NAME,
                ContentType::OctetStream,
                self.agent_id.clone(),
            )
        };
        let bytes_len = output.bytes.len();
        let handle = match store.write(
            &self.agent_id,
            descriptor,
            ArtifactBody::Bytes(output.bytes),
        ) {
            Ok(handle) => handle,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage generated speech: {e}"),
                    elapsed(started),
                ));
            }
        };

        let ext = extension_for_speech(output.media_type);
        let filename = match input["filename"].as_str().filter(|f| !f.trim().is_empty()) {
            Some(name) => {
                let stem = name.trim().trim_end_matches(&format!(".{ext}"));
                format!("{stem}.{ext}")
            }
            None => format!("voice-note.{ext}"),
        };
        let entry = json!({
            "artifact_id": handle.id.as_str(),
            "media_type": output.media_type,
            "filename": filename,
            "bytes": bytes_len,
        });

        // Same stringy-boolean coercion as generate_image, so a "false" from
        // the model can't leak the clip into the conversation it excluded.
        let deliver = match &input["deliver"] {
            Value::Bool(b) => *b,
            Value::String(s) => !matches!(s.trim(), "false" | "False" | "0" | "no"),
            Value::Number(n) => n.as_i64() != Some(0),
            _ => true,
        };
        let mut out = serde_json::Map::new();
        let note = if deliver {
            out.insert(GENERATED_IMAGES_KEY.to_string(), json!([entry]));
            "Synthesized. On audio-capable surfaces (Slack) the system is posting the voice \
             clip to this conversation automatically — outside your context, so you will NOT \
             see the attachment; do not re-upload or verify it, just briefly tell the user \
             what you sent. If this conversation is on the mobile/web app, audio delivery is \
             not supported yet — tell the user voice clips currently work in Slack only. (To \
             ALSO send it elsewhere, call messaging.send_file with the artifact_id above.)"
        } else {
            out.insert("staged_audio".to_string(), json!([entry]));
            "Synthesized and staged, but NOT posted to this conversation (deliver=false). To \
             deliver it, call messaging.send_file with the artifact_id above and a `channel` \
             or `user`."
        };
        out.insert("voice".to_string(), json!(self.provider.voice_label()));
        out.insert("characters".to_string(), json!(spoken.chars().count()));
        out.insert("note".to_string(), json!(note));
        Ok(ToolResult::success(Value::Object(out), elapsed(started)))
    }
}

/// Outcome of applying a (possibly changed) `[llm.speech]` config to the live
/// tool registry. Mirrors `ImageReload`.
pub enum SpeechReload {
    Unchanged,
    Installed { voice: String },
    Disabled,
    BuildFailed(eyre::Report),
}

impl SpeechReload {
    /// See `ImageReload::recorded`: `BuildFailed` records `None` so the next
    /// reload retries instead of short-circuiting on an unchanged broken block.
    pub fn recorded(&self, applied: Option<LlmSpeechConfig>) -> Option<LlmSpeechConfig> {
        match self {
            SpeechReload::BuildFailed(_) => None,
            _ => applied,
        }
    }

    pub fn log(&self, agent_id: &str) {
        match self {
            SpeechReload::Unchanged => {}
            SpeechReload::Installed { voice } => {
                tracing::info!(agent = %agent_id, %voice, "Speech synthesis tool (re)installed")
            }
            SpeechReload::Disabled => {
                tracing::info!(agent = %agent_id, "Speech synthesis disabled; tool removed")
            }
            SpeechReload::BuildFailed(e) => {
                tracing::warn!(agent = %agent_id, error = %e, "Speech config changed but provider build failed; tool removed")
            }
        }
    }
}

/// Reconcile the registered `generate_speech` tool with `next` config — the
/// single path shared by startup (`current = None`) and hot-reload, mirroring
/// `install_image_tool`. `speech_home` is the default builtin (Piper) data
/// dir; `key` is where the provider the config names reads its key.
pub fn install_speech_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    current: Option<&LlmSpeechConfig>,
    next: Option<&LlmSpeechConfig>,
    speech_home: &std::path::Path,
    key: impl Fn(&'static str) -> KeySlot,
    normalizer: Option<SpeechNormalizer>,
    user_voice: Option<SpeechUserVoice>,
) -> SpeechReload {
    if current == next {
        return SpeechReload::Unchanged;
    }
    reinstall_speech_tool(
        tools,
        agent_id,
        next,
        speech_home,
        key,
        normalizer,
        user_voice,
    )
}

/// Rebuild the `generate_speech` tool from `cfg` unconditionally. Mirrors
/// `reinstall_image_tool`.
pub fn reinstall_speech_tool(
    tools: &ToolRegistry,
    agent_id: &str,
    cfg: Option<&LlmSpeechConfig>,
    speech_home: &std::path::Path,
    key: impl Fn(&'static str) -> KeySlot,
    normalizer: Option<SpeechNormalizer>,
    user_voice: Option<SpeechUserVoice>,
) -> SpeechReload {
    match cfg.filter(|c| c.enabled) {
        Some(cfg) => match provider_from_config(cfg, speech_home, key) {
            Ok(provider) => {
                let provider: Arc<dyn SpeechProvider> = Arc::from(provider);
                let voice = provider.voice_label();
                tools.register(Arc::new(SpeechGenerationTool::new(
                    provider, agent_id, normalizer, user_voice,
                )));
                SpeechReload::Installed { voice }
            }
            Err(e) => {
                tools.unregister(SPEECH_TOOL_NAME);
                SpeechReload::BuildFailed(e)
            }
        },
        None => {
            tools.unregister(SPEECH_TOOL_NAME);
            SpeechReload::Disabled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::speech::SpeechOutput;

    struct FakeSpeech;

    #[async_trait]
    impl SpeechProvider for FakeSpeech {
        async fn synthesize(&self, _req: &SpeechRequest<'_>) -> eyre::Result<SpeechOutput> {
            Ok(SpeechOutput {
                bytes: vec![1, 2, 3],
                media_type: "audio/mpeg",
            })
        }
        fn voice_label(&self) -> String {
            "fake/voice".to_string()
        }
        fn provider_id(&self) -> &'static str {
            "fish_audio"
        }
        fn markup(&self) -> crate::llm::speech::Markup {
            crate::llm::speech::Markup::FishS1
        }
    }

    fn no_key(_: &'static str) -> KeySlot {
        KeySlot::fixed("")
    }

    fn builtin_cfg() -> LlmSpeechConfig {
        LlmSpeechConfig {
            provider: "builtin".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn install_registers_and_disable_removes() {
        let tools = ToolRegistry::new();
        let cfg = builtin_cfg();
        let home = std::path::Path::new("/tmp/piper");
        let outcome =
            install_speech_tool(&tools, "agent", None, Some(&cfg), home, no_key, None, None);
        assert!(matches!(outcome, SpeechReload::Installed { .. }));
        assert!(tools.has(SPEECH_TOOL_NAME));

        // Unchanged config is a no-op.
        let outcome = install_speech_tool(
            &tools,
            "agent",
            Some(&cfg),
            Some(&cfg),
            home,
            no_key,
            None,
            None,
        );
        assert!(matches!(outcome, SpeechReload::Unchanged));

        // Removed block removes the tool.
        let outcome =
            install_speech_tool(&tools, "agent", Some(&cfg), None, home, no_key, None, None);
        assert!(matches!(outcome, SpeechReload::Disabled));
        assert!(!tools.has(SPEECH_TOOL_NAME));
    }

    #[test]
    fn build_failure_records_none_so_reload_retries() {
        let tools = ToolRegistry::new();
        let bad = LlmSpeechConfig {
            provider: "kokoro".to_string(), // client-side only; invalid here
            ..Default::default()
        };
        let home = std::path::Path::new("/tmp/piper");
        let outcome =
            install_speech_tool(&tools, "agent", None, Some(&bad), home, no_key, None, None);
        assert!(matches!(outcome, SpeechReload::BuildFailed(_)));
        assert!(outcome.recorded(Some(bad)).is_none());
        assert!(!tools.has(SPEECH_TOOL_NAME));
    }

    #[test]
    fn description_carries_provider_phrasing_rules() {
        let tool = SpeechGenerationTool::new(Arc::new(FakeSpeech), "agent", None, None);
        let description = tool.description();
        assert!(description.contains("fake/voice"));
        assert!(description.contains("(excited)") && description.contains("(soft tone)"));
        for unsupported in ["thoughtful", "concerned", "amused", "serious"] {
            assert!(!description.contains(unsupported), "offers {unsupported}");
        }
    }

    #[tokio::test]
    async fn execute_rejects_empty_and_oversized_text() {
        let tool = SpeechGenerationTool::new(Arc::new(FakeSpeech), "agent", None, None);
        let r = tool.execute(json!({ "text": "  " })).await.unwrap();
        assert!(!r.success);

        let r = tool
            .execute(json!({ "text": "x".repeat(MAX_SPEECH_CHARS + 1) }))
            .await
            .unwrap();
        assert!(!r.success);
    }

    /// Provider that records the voice each request asked for.
    type CapturedVoices = Arc<std::sync::Mutex<Vec<Option<String>>>>;
    struct CapturingSpeech(CapturedVoices);

    #[async_trait]
    impl SpeechProvider for CapturingSpeech {
        async fn synthesize(&self, req: &SpeechRequest<'_>) -> eyre::Result<SpeechOutput> {
            self.0.lock().unwrap().push(req.voice.clone());
            Ok(SpeechOutput {
                bytes: vec![1],
                media_type: "audio/mpeg",
            })
        }
        fn voice_label(&self) -> String {
            "cap/voice".to_string()
        }
        fn provider_id(&self) -> &'static str {
            "fish_audio"
        }
    }

    #[tokio::test]
    async fn a_users_own_voice_reaches_the_provider_only_on_their_turn() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let voices: SpeechUserVoice = Arc::new(|uid: &str, provider: &str| {
            assert_eq!(provider, "fish_audio");
            (uid == "user-1").then(|| "user1-voice".to_string())
        });
        let tool = SpeechGenerationTool::new(
            Arc::new(CapturingSpeech(Arc::clone(&captured))),
            "agent",
            None,
            Some(voices),
        );

        crate::runner::CURRENT_USER_ID
            .scope("user-1".to_string(), tool.execute(json!({"text": "oi"})))
            .await
            .unwrap();
        crate::runner::CURRENT_USER_ID
            .scope("user-2".to_string(), tool.execute(json!({"text": "oi"})))
            .await
            .unwrap();
        // No user in scope (a background job): the shared voice.
        tool.execute(json!({"text": "oi"})).await.unwrap();

        assert_eq!(
            captured.lock().unwrap().clone(),
            vec![Some("user1-voice".to_string()), None, None]
        );
    }

    #[tokio::test]
    async fn normalizer_applies_before_synthesis() {
        let normalizer: SpeechNormalizer = Arc::new(|_s: &str| "   ".to_string());
        let tool = SpeechGenerationTool::new(Arc::new(FakeSpeech), "agent", Some(normalizer), None);
        // Normalizer strips everything → speakable-empty failure, proving it ran.
        let r = tool.execute(json!({ "text": "**bold**" })).await.unwrap();
        assert!(!r.success);
    }
}
