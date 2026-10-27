//! Every `[llm.vision]`, `[llm.transcription]`, `[llm.image]` and
//! `[llm.video]` spec the install ships or documents names a model that can
//! be called. No spec falls back to a provider it does not name, so an
//! example that resolves to nothing is a misconfiguration handed to whoever
//! copies it.

use std::collections::BTreeSet;

use workforce_agent::llm::image_tiers::ImageTierRegistry;
use workforce_agent::llm::transcription_tiers::TranscriptionTierRegistry;
use workforce_agent::llm::video_tiers::{VideoTierRegistry, GEMINI};
use workforce_agent::llm::vision_tiers::VisionTierRegistry;

const DOCUMENTS: [&str; 3] = [
    include_str!("../../../end-user-docs/configuration/tiers.mdx"),
    include_str!("../../../end-user-docs/reference/agent-types.mdx"),
    include_str!("../../common/src/settings.rs"),
];

/// Every quoted spec on a `model =` line under a `[…llm.<section>]` header
/// of the documents, whether prose, a doc comment or a test's config.
fn documented(section: &str) -> BTreeSet<String> {
    let header = format!("llm.{section}]");
    DOCUMENTS
        .iter()
        .flat_map(|document| {
            document
                .lines()
                .map(|line| line.trim_start_matches('/').trim())
                .scan(false, |inside, line| {
                    if line.starts_with('[') {
                        *inside = line.ends_with(&header);
                    }
                    Some((*inside, line))
                })
                .filter(|(inside, line)| *inside && line.starts_with("model "))
                .flat_map(|(_, line)| line.split('"').skip(1).step_by(2))
        })
        .map(str::to_string)
        .collect()
}

fn config<T: serde::de::DeserializeOwned>(model: &str) -> T {
    toml::from_str(&format!("model = {model:?}")).unwrap()
}

#[test]
fn the_documents_are_read() {
    for (section, example) in [
        ("vision", "openrouter:anthropic:cheap"),
        ("transcription", "openrouter:google:cheap"),
        ("image", "bedrock:stability:standard"),
        ("video", "gemini:standard"),
    ] {
        let documented = documented(section);
        assert!(documented.contains(example), "{section}: {documented:?}");
    }
}

#[test]
fn every_vision_default_and_documented_example_names_a_describer() {
    let tiers = VisionTierRegistry::from_defaults().unwrap();
    let specs: BTreeSet<String> = tiers
        .tier_specs()
        .chain(documented("vision"))
        .chain(["chatgpt:gpt-6.1-sol".to_string()])
        .collect();
    for spec in specs {
        let (kind, model) =
            crate::vision::route(&config(&spec), &tiers).unwrap_or_else(|e| panic!("{e}"));
        assert!(kind.describes_images(), "{spec}");
        assert!(tiers.is_vision_capable(&model), "{spec} names {model}");
    }
}

#[test]
fn every_transcription_default_and_documented_example_names_a_transcriber() {
    let tiers = TranscriptionTierRegistry::from_defaults().unwrap();
    let specs: BTreeSet<String> = tiers
        .tier_specs()
        .chain(documented("transcription"))
        .collect();
    for spec in specs {
        let (_, _, model) =
            crate::transcription::route(&config(&spec), &tiers).unwrap_or_else(|e| panic!("{e}"));
        assert!(tiers.is_supported(&model), "{spec} names {model}");
    }
}

#[test]
fn every_image_default_and_documented_example_names_a_generator() {
    let tiers = ImageTierRegistry::from_defaults().unwrap();
    let specs: BTreeSet<String> = tiers.tier_specs().chain(documented("image")).collect();
    for spec in specs {
        let (provider, model) = tiers
            .resolve_routed_image_model(&spec, None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            workforce_agent::llm::provider::ProviderKind::from_str_loose(&provider).is_some(),
            "{spec} names {provider}"
        );
        assert!(tiers.lists(&model), "{spec} names {model}");
    }
}

#[test]
fn every_video_default_and_documented_example_names_a_gemini_model() {
    let tiers = VideoTierRegistry::from_defaults().unwrap();
    let specs: BTreeSet<String> = tiers.tier_specs().chain(documented("video")).collect();
    for spec in specs {
        let model = tiers
            .resolve_video_model(&spec, Some(GEMINI))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(tiers.lists(&model), "{spec} names {model}");
    }
}
