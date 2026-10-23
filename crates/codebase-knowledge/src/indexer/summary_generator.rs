use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};

use crate::graph::entities::EntityId;

/// LLM backend for the summary generator.
#[async_trait]
pub trait SummaryLlm: Send + Sync {
    async fn generate_text(&self, prompt: &str, max_tokens: usize) -> Result<String>;
    fn model_name(&self) -> &str;
}

/// A generated summary for a code entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSummary {
    pub entity_id: EntityId,
    pub granularity: SummaryGranularity,
    pub text: String,
    pub source_hash: String,
    pub model_used: String,
    pub generated_at: String,
    pub is_stale: bool,
}

/// Granularity levels for summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryGranularity {
    Function,
    File,
    Module,
    Architecture,
}

impl SummaryGranularity {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Function => "function",
            Self::File => "file",
            Self::Module => "module",
            Self::Architecture => "architecture",
        }
    }
}

/// Generates code summaries at various granularities using a pluggable LLM backend.
pub struct SummaryGenerator {
    llm: Box<dyn SummaryLlm>,
    max_tokens: usize,
}

impl SummaryGenerator {
    pub fn new(llm: Box<dyn SummaryLlm>, max_tokens: usize) -> Self {
        Self { llm, max_tokens }
    }

    /// The model name used for summary generation.
    pub fn model_name(&self) -> &str {
        self.llm.model_name()
    }

    /// Generate a summary for a code entity.
    pub async fn generate(
        &self,
        entity_source: &str,
        context: &str,
        granularity: SummaryGranularity,
    ) -> Result<String> {
        let prompt = Self::build_prompt(entity_source, context, &granularity);
        self.llm.generate_text(&prompt, self.max_tokens).await
    }

    fn build_prompt(
        entity_source: &str,
        context: &str,
        granularity: &SummaryGranularity,
    ) -> String {
        match granularity {
            SummaryGranularity::Function => format!(
                "Summarise what this function does in 1-2 sentences. Be precise and technical.\n\n```\n{}\n```",
                entity_source
            ),
            SummaryGranularity::File => format!(
                "Summarise what this file is responsible for in 2-3 sentences. Mention key types, functions, and their purpose.\n\nFile contents (signatures only):\n{}\n\nAdditional context:\n{}",
                entity_source, context
            ),
            SummaryGranularity::Module => format!(
                "Summarise what this module/directory does in 3-5 sentences. Explain its role in the system and key abstractions.\n\nFile summaries:\n{}",
                entity_source
            ),
            SummaryGranularity::Architecture => format!(
                "Explain how this part of the system works, including data flow, key abstractions, and design decisions. Be thorough but concise.\n\nRelevant module summaries:\n{}",
                entity_source
            ),
        }
    }
}
