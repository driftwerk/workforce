use serde::{Deserialize, Serialize};

/// Confidence score for a knowledge query result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceScore {
    pub overall: f32,
    pub factors: ConfidenceFactors,
}

/// Breakdown of confidence factors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceFactors {
    /// Are all source entities current with the files on disk?
    pub source_freshness: f32,
    /// Average vector similarity from Weaviate search.
    pub vector_similarity: f32,
    /// Were all relationships resolved in the graph?
    pub graph_completeness: f32,
    /// Was live code verification performed?
    pub live_verified: bool,
    /// Do multiple independent sources agree?
    pub corroboration: f32,
}

impl ConfidenceScore {
    /// Calculate an overall confidence score from factors.
    pub fn calculate(factors: ConfidenceFactors) -> Self {
        let live_bonus = if factors.live_verified { 0.1 } else { 0.0 };
        let overall = (factors.source_freshness * 0.3
            + factors.vector_similarity * 0.25
            + factors.graph_completeness * 0.2
            + factors.corroboration * 0.15
            + live_bonus)
            .clamp(0.0, 1.0);

        Self { overall, factors }
    }

    /// Whether this result is reliable enough to present without caveats.
    pub fn is_reliable(&self) -> bool {
        self.overall >= 0.7
    }
}
