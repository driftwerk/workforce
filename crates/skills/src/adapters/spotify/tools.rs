//! Agent-callable Spotify playback tools.
//!
//! Every one of these is `allow` rather than `require_approval`, which is a
//! deliberate departure from the convention that mutating tools are gated. The
//! surface these exist for is a speaker in a room with no screen: an approval
//! prompt there is not a safety check, it is a request nobody can see or
//! answer, and "pause the music" would simply never happen. The blast radius is
//! the person's own music, which they can undo by asking again.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use workforce_agent::tools::{Tool, ToolResult};

use super::client::{describe, Kind, SpotifyContext, Target};

pub type Ctx = Arc<SpotifyContext>;

/// What each tool does, in the words the model sees.
struct Spec {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn no_input() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn specs() -> Vec<Spec> {
    vec![
        Spec {
            name: "spotify.play",
            description: "Play music on Spotify. Omit `query` to resume what was paused.\n\n\
                 Choose `kind` by what was actually asked for — this decides whether the music \
                 keeps going or stops after one song:\n\
                 • `artist` — \"play some Michael Jackson\", \"put on Bowie\". Plays that \
                 artist's music continuously, shuffled.\n\
                 • `playlist` — a genre, era, mood or activity: \"eighties hits\", \"something \
                 relaxing\", \"music for working\". Plays continuously, shuffled.\n\
                 • `track` — one named song: \"play Billie Jean\". Plays it once.\n\n\
                 Use `track` only when a specific song was named. Asked for an artist or a \
                 genre, `track` plays one song and then silence, which is not what was meant.\n\n\
                 `device` plays somewhere other than the default speaker — pass what the person \
                 said (\"Ash\", \"my laptop\"). Use spotify.devices to see what is available.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to play. Omit to resume."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["artist", "playlist", "track"],
                        "description": "What the request is for. Defaults to artist when a query is given, because most requests name one."
                    },
                    "device": {
                        "type": "string",
                        "description": "Play here instead of the default speaker. Omit for the default."
                    }
                },
                "additionalProperties": false
            }),
        },
        Spec {
            name: "spotify.pause",
            description: "Pause Spotify playback. Spotify has no separate stop; pausing is what \
                          stopping means.",
            schema: no_input(),
        },
        Spec {
            name: "spotify.skip",
            description: "Skip to the next track, or back to the previous one.",
            schema: json!({
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["next", "previous"],
                        "description": "Defaults to next."
                    }
                },
                "additionalProperties": false
            }),
        },
        Spec {
            name: "spotify.volume",
            description: "Set Spotify's volume, 0 to 100.",
            schema: json!({
                "type": "object",
                "properties": {
                    "percent": { "type": "integer", "minimum": 0, "maximum": 100 }
                },
                "required": ["percent"],
                "additionalProperties": false
            }),
        },
        Spec {
            name: "spotify.now_playing",
            description: "What is playing on Spotify right now, and where.",
            schema: no_input(),
        },
        Spec {
            name: "spotify.devices",
            description: "Every device Spotify can currently play on. Useful when playback fails \
                          because nothing is available.",
            schema: no_input(),
        },
    ]
}

pub fn tool_names() -> Vec<&'static str> {
    specs().into_iter().map(|s| s.name).collect()
}

/// Tool metadata and policies for the skill definition.
pub fn skill_tools_and_policies() -> (
    Vec<crate::definition::SkillTool>,
    Vec<crate::definition::SkillPolicy>,
) {
    use crate::definition::{SkillPolicy, SkillTool};
    specs()
        .into_iter()
        .map(|spec| {
            (
                SkillTool {
                    name: spec.name.to_string(),
                    description: spec.description.to_string(),
                    input_schema: spec.schema,
                },
                SkillPolicy {
                    tool: spec.name.to_string(),
                    // See the module docs: gating these would break the only
                    // surface they exist for.
                    action: "allow".to_string(),
                },
            )
        })
        .unzip()
}

pub fn build_tools(ctx: Ctx) -> Vec<Arc<dyn Tool>> {
    specs()
        .into_iter()
        .map(|spec| {
            Arc::new(SpotifyTool {
                ctx: Arc::clone(&ctx),
                spec,
            }) as Arc<dyn Tool>
        })
        .collect()
}

struct SpotifyTool {
    ctx: Ctx,
    spec: Spec,
}

impl SpotifyTool {
    async fn run(&self, input: Value) -> Result<Value> {
        match self.spec.name {
            "spotify.play" => {
                let query = input
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let requested_device = input
                    .get("device")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|d| !d.is_empty());
                if query.is_empty() {
                    let device = self
                        .ctx
                        .play(Target::Resume, requested_device, false)
                        .await?;
                    return Ok(json!({ "playing": true, "device": device.name }));
                }
                // Artist by default: most requests name one, and guessing
                // `track` there plays a single song and stops.
                let kind = input
                    .get("kind")
                    .and_then(Value::as_str)
                    .and_then(Kind::parse)
                    .unwrap_or(Kind::Artist);
                let (target, described) = self.ctx.find(query, kind).await?;
                let device = self
                    .ctx
                    .play(target, requested_device, kind.shuffles())
                    .await?;
                Ok(json!({
                    "playing": described,
                    "kind": format!("{kind:?}").to_lowercase(),
                    "continuous": kind.shuffles(),
                    "device": device.name,
                }))
            }
            "spotify.pause" => {
                self.ctx.pause().await?;
                Ok(json!({ "paused": true }))
            }
            "spotify.skip" => {
                let forward = input
                    .get("direction")
                    .and_then(Value::as_str)
                    .is_none_or(|d| !d.eq_ignore_ascii_case("previous"));
                self.ctx.skip(forward).await?;
                Ok(json!({ "skipped": if forward { "next" } else { "previous" } }))
            }
            "spotify.volume" => {
                let percent = input
                    .get("percent")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| eyre::eyre!("'percent' is required, from 0 to 100"))?;
                let percent = percent.min(100) as u8;
                self.ctx.set_volume(percent).await?;
                Ok(json!({ "volume_percent": percent }))
            }
            "spotify.now_playing" => match self.ctx.now_playing().await? {
                Some(playing) => Ok(json!({
                    "playing": playing.is_playing,
                    "track": describe(&playing.track, &playing.artist),
                    "device": playing.device,
                })),
                // Not a failure: "nothing is playing" is a correct answer.
                None => Ok(json!({ "playing": false, "track": null })),
            },
            "spotify.devices" => {
                let devices = self.ctx.devices().await?;
                Ok(json!({
                    "preferred": self.ctx.preferred_device(),
                    "devices": devices
                        .iter()
                        .map(|d| json!({
                            "name": d.name,
                            "active": d.is_active,
                            "type": d.kind,
                            "volume_percent": d.volume_percent,
                        }))
                        .collect::<Vec<_>>(),
                }))
            }
            other => Err(eyre::eyre!("unknown Spotify tool: {other}")),
        }
    }
}

#[async_trait]
impl Tool for SpotifyTool {
    fn name(&self) -> &str {
        self.spec.name
    }

    fn description(&self) -> &str {
        self.spec.description
    }

    fn input_schema(&self) -> Value {
        self.spec.schema.clone()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let elapsed = |s: Instant| s.elapsed().as_millis() as u64;
        match self.run(input).await {
            Ok(output) => Ok(ToolResult::success(output, elapsed(started))),
            // Reported rather than raised: the messages name what to do about
            // them, and an agent that can say "Spotify isn't connected" is more
            // use than one that fails the turn.
            Err(e) => Ok(ToolResult::from_report(&e, elapsed(started))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_allowed_rather_than_gated() {
        let (tools, policies) = skill_tools_and_policies();
        assert_eq!(tools.len(), policies.len());
        assert!(
            policies.iter().all(|p| p.action == "allow"),
            "an approval prompt on a screenless speaker cannot be answered"
        );
    }

    #[test]
    fn the_tools_cover_what_someone_would_ask_out_loud() {
        let names = tool_names();
        for expected in [
            "spotify.play",
            "spotify.pause",
            "spotify.skip",
            "spotify.now_playing",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    /// Schemas are what the model plans against, so a required field it cannot
    /// know about would make the tool unusable.
    #[test]
    fn play_and_skip_need_no_arguments() {
        for spec in specs() {
            if matches!(spec.name, "spotify.play" | "spotify.skip") {
                assert!(
                    spec.schema.get("required").is_none(),
                    "{} should work with no arguments",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn volume_requires_a_percentage() {
        let volume = specs()
            .into_iter()
            .find(|s| s.name == "spotify.volume")
            .unwrap();
        assert_eq!(volume.schema["required"], json!(["percent"]));
    }
}
