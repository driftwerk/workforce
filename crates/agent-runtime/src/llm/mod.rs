pub mod anthropic;
pub mod bedrock;
pub mod bedrock_capabilities;
pub mod bedrock_catalog;
pub mod bedrock_converse;
pub mod bedrock_credentials;
pub mod bedrock_eventstream;
pub mod bedrock_invoke;
pub mod bedrock_openai;
pub mod cache_utils;
pub mod cerebras;
pub mod chat_transcription;

/// Embedded default pricing TOML for all providers.
pub const DEFAULT_PRICING_TOML: &str = include_str!("default_pricing.toml");

/// Embedded default vision-model pricing TOML.
///
/// Layered on top of [`DEFAULT_PRICING_TOML`] at startup so vision-only models
/// (Qwen-VL, GLM-V) appear in the unified pricing map. Models already priced in
/// `default_pricing.toml` are not duplicated here.
pub const DEFAULT_VISION_PRICING_TOML: &str = include_str!("default_vision_pricing.toml");

/// Embedded default transcription-model pricing TOML.
///
/// Layered on top of [`DEFAULT_PRICING_TOML`] at startup. Whisper-style APIs
/// bill per audio-minute, not per token; the file uses a per-millisecond-as-token
/// convention (1 token = 1 ms of audio). See the file header for the math.
pub const DEFAULT_TRANSCRIPTION_PRICING_TOML: &str =
    include_str!("default_transcription_pricing.toml");

#[cfg(test)]
mod toml_smoke_tests {
    use super::*;

    #[test]
    fn default_vision_pricing_parses_as_toml() {
        let parsed: toml::Table = DEFAULT_VISION_PRICING_TOML
            .parse()
            .expect("default_vision_pricing.toml must be valid TOML");
        assert!(!parsed.is_empty(), "vision pricing table is empty");
    }

    #[test]
    fn default_transcription_pricing_parses_as_toml() {
        let parsed: toml::Table = DEFAULT_TRANSCRIPTION_PRICING_TOML
            .parse()
            .expect("default_transcription_pricing.toml must be valid TOML");
        assert!(!parsed.is_empty(), "transcription pricing table is empty");
    }

    #[test]
    fn default_pricing_files_parse_independently() {
        let _: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let _: toml::Table = DEFAULT_VISION_PRICING_TOML.parse().unwrap();
        let _: toml::Table = DEFAULT_TRANSCRIPTION_PRICING_TOML.parse().unwrap();
    }

    fn collect_bedrock_pricing_keys() -> Vec<String> {
        let table: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let bedrock = match table.get("bedrock") {
            Some(toml::Value::Table(t)) => t,
            _ => return Vec::new(),
        };
        let mut keys = Vec::new();
        for sub in bedrock.values() {
            if let toml::Value::Table(family) = sub {
                for k in family.keys() {
                    keys.push(k.clone());
                }
            }
        }
        keys
    }

    #[test]
    fn every_bedrock_tier_model_id_has_pricing() {
        let pricing_keys: std::collections::HashSet<String> =
            collect_bedrock_pricing_keys().into_iter().collect();
        let tiers: toml::Table = include_str!("default_tiers.toml").parse().unwrap();
        let bedrock_tiers = match tiers.get("bedrock") {
            Some(toml::Value::Table(t)) => t,
            _ => panic!("default_tiers.toml has no [bedrock] section"),
        };
        for (family, val) in bedrock_tiers {
            let toml::Value::Table(tier_map) = val else {
                continue;
            };
            for (tier, model) in tier_map {
                let model_id = model.as_str().unwrap();
                assert!(
                    pricing_keys.contains(model_id),
                    "[bedrock.{family}] tier '{tier}' references model '{model_id}' which has no pricing entry"
                );
            }
        }
    }

    #[test]
    fn bedrock_pricing_has_no_placeholder_or_zero_values() {
        let table: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let toml::Value::Table(bedrock) = table.get("bedrock").unwrap() else {
            panic!("no [bedrock] section");
        };
        for (family, sub) in bedrock {
            let toml::Value::Table(entries) = sub else {
                continue;
            };
            for (model_id, entry) in entries {
                let toml::Value::Table(fields) = entry else {
                    panic!("[bedrock.{family}] '{model_id}' is not a table");
                };
                let input = fields
                    .get("input")
                    .and_then(|v| v.as_float())
                    .unwrap_or(0.0);
                let output = fields
                    .get("output")
                    .and_then(|v| v.as_float())
                    .unwrap_or(0.0);
                assert!(
                    input > 0.0,
                    "[bedrock.{family}] '{model_id}' has non-positive input price"
                );
                assert!(
                    output > 0.0,
                    "[bedrock.{family}] '{model_id}' has non-positive output price"
                );
            }
        }
    }

    #[test]
    fn bedrock_anthropic_pricing_keys_route_to_invoke() {
        use crate::llm::bedrock_capabilities::{wire_format_for, WireFormat};
        let table: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let toml::Value::Table(bedrock) = table.get("bedrock").unwrap() else {
            panic!("no [bedrock] section");
        };
        let toml::Value::Table(anth) = bedrock.get("anthropic").unwrap() else {
            panic!("no [bedrock.anthropic] section");
        };
        for model_id in anth.keys() {
            assert_eq!(
                wire_format_for(model_id),
                WireFormat::Invoke,
                "expected Invoke for {model_id}"
            );
        }
    }

    #[test]
    fn bedrock_non_anthropic_pricing_keys_route_to_non_invoke() {
        use crate::llm::bedrock_capabilities::{wire_format_for, WireFormat};
        let table: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let toml::Value::Table(bedrock) = table.get("bedrock").unwrap() else {
            panic!("no [bedrock] section");
        };
        for (family, sub) in bedrock {
            if family == "anthropic" {
                continue;
            }
            let toml::Value::Table(entries) = sub else {
                continue;
            };
            for model_id in entries.keys() {
                let wire = wire_format_for(model_id);
                assert!(
                    matches!(wire, WireFormat::Converse | WireFormat::OpenAi),
                    "expected Converse or OpenAi for {model_id} in family {family}, got {wire:?}"
                );
            }
        }
    }

    #[test]
    fn every_subscription_tier_model_has_list_pricing() {
        let pricing: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let tiers: toml::Table = include_str!("default_tiers.toml").parse().unwrap();
        for provider in ["chatgpt", "codex", "claude-code"] {
            let Some(toml::Value::Table(tier_map)) = tiers.get(provider) else {
                panic!("default_tiers.toml has no [{provider}] section");
            };
            let Some(toml::Value::Table(rows)) = pricing.get(provider) else {
                panic!("default_pricing.toml has no [\"{provider}\"] section");
            };
            for (tier, model) in tier_map {
                let key = format!("{provider}:{}", model.as_str().unwrap());
                assert!(
                    rows.contains_key(&key),
                    "[{provider}] tier '{tier}' is recorded as '{key}', which has no pricing row"
                );
            }
        }
        let toml::Value::Table(rows) = &pricing["chatgpt"] else {
            panic!("[\"chatgpt\"] is a table");
        };
        let mut supported: Vec<String> = responses::ChatGptModel::ALL
            .iter()
            .map(|model| format!("chatgpt:{}", model.id()))
            .collect();
        supported.sort();
        let mut priced: Vec<&String> = rows.keys().collect();
        priced.sort();
        assert_eq!(
            priced,
            supported.iter().collect::<Vec<_>>(),
            "chatgpt rows price exactly the models the backend serves"
        );
    }

    fn collect_vertex_pricing_keys() -> Vec<String> {
        let table: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let vertex = match table.get("vertex") {
            Some(toml::Value::Table(t)) => t,
            _ => return Vec::new(),
        };
        vertex
            .values()
            .filter_map(|sub| match sub {
                toml::Value::Table(family) => Some(family.keys().cloned()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn every_vertex_tier_model_id_has_pricing() {
        let pricing_keys: std::collections::HashSet<String> =
            collect_vertex_pricing_keys().into_iter().collect();
        let tiers: toml::Table = include_str!("default_tiers.toml").parse().unwrap();
        let vertex_tiers = match tiers.get("vertex") {
            Some(toml::Value::Table(t)) => t,
            _ => panic!("default_tiers.toml has no [vertex] section"),
        };
        for (family, val) in vertex_tiers {
            let toml::Value::Table(tier_map) = val else {
                continue;
            };
            for (tier, model) in tier_map {
                let model_id = model.as_str().unwrap();
                assert!(
                    pricing_keys.contains(model_id),
                    "[vertex.{family}] tier '{tier}' references model '{model_id}' which has no pricing entry"
                );
            }
        }
    }

    #[test]
    fn vertex_pricing_has_no_placeholder_or_zero_values() {
        let table: toml::Table = DEFAULT_PRICING_TOML.parse().unwrap();
        let toml::Value::Table(vertex) = table.get("vertex").unwrap() else {
            panic!("no [vertex] section");
        };
        for (family, sub) in vertex {
            let toml::Value::Table(entries) = sub else {
                continue;
            };
            for (model_id, entry) in entries {
                let toml::Value::Table(fields) = entry else {
                    panic!("[vertex.{family}] '{model_id}' is not a table");
                };
                let input = fields
                    .get("input")
                    .and_then(|v| v.as_float())
                    .unwrap_or(0.0);
                let output = fields
                    .get("output")
                    .and_then(|v| v.as_float())
                    .unwrap_or(0.0);
                assert!(
                    input > 0.0,
                    "[vertex.{family}] '{model_id}' has non-positive input price"
                );
                assert!(
                    output > 0.0,
                    "[vertex.{family}] '{model_id}' has non-positive output price"
                );
            }
        }
    }
}
pub mod environment;
pub mod failover;
pub mod gemini;
pub mod grok;
pub mod image;
pub mod image_cap;
pub mod image_tiers;
pub mod mcp_workspace;
pub mod model_limits;
pub mod oai_common;
pub mod ollama;
pub mod openrouter;
pub mod pool;
pub mod prefix;
pub mod provider;
pub mod responses;
pub mod speech;
pub mod sse;
pub mod streaming;
pub mod tiers;
pub mod tool_schema;
pub mod transcription_tiers;
pub mod types;
pub mod vertex;
pub mod vertex_auth;
pub mod vertex_capabilities;
pub mod vertex_credentials;
pub mod video;
pub mod video_tiers;
pub mod vision_tiers;
pub mod vllm;
pub mod whisper;
pub mod zai;
