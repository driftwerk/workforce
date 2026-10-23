//! Shared serde mirrors for types flowing back out of graph tools. Keeping
//! them here means `codegraph.*` and `impact.*` emit identical shapes for
//! entities and graph hits.

use serde::Serialize;
use workforce_codebase_knowledge::graph::entities::CodeEntity;
use workforce_codebase_knowledge::graph::queries::GraphQueryResult;
use workforce_memory::types::MemoryLayer;

/// Canonical snake_case string for a [`MemoryLayer`]. Matches the serde
/// rename on the enum so tool output stays consistent with the stored form.
pub fn memory_layer_str(layer: &MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::Working => "working",
        MemoryLayer::Episodic => "episodic",
        MemoryLayer::Semantic => "semantic",
        MemoryLayer::Team => "team",
        MemoryLayer::Organisation => "organisation",
    }
}

#[derive(Debug, Serialize)]
pub struct EntitySummary {
    pub entity_id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub language: String,
    pub signature: Option<String>,
    pub repo_id: String,
}

impl From<&CodeEntity> for EntitySummary {
    fn from(e: &CodeEntity) -> Self {
        Self {
            entity_id: e.id.to_string(),
            name: e.name.clone(),
            qualified_name: e.qualified_name.clone(),
            kind: e.kind.as_str().to_string(),
            file: e.file_path.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
            language: e.language.as_str().to_string(),
            signature: e.signature.clone(),
            repo_id: e.repo_id.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GraphHit {
    #[serde(flatten)]
    pub entity: EntitySummary,
    pub distance: usize,
    pub relationship: String,
}

impl From<&GraphQueryResult> for GraphHit {
    fn from(r: &GraphQueryResult) -> Self {
        Self {
            entity: EntitySummary::from(&r.entity),
            distance: r.distance,
            relationship: r.relationship.as_str().to_string(),
        }
    }
}
