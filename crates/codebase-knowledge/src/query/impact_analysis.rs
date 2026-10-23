use eyre::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::graph::entities::{CodeEntity, EntityId, EntityKind};
use crate::graph::queries::GraphQueryResult;
use crate::graph::relationships::RelationshipKind;
use crate::graph::CodeGraph;

/// Severity of impact on an affected entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactSeverity {
    /// Will break at compile time (e.g., changing a trait's method signature).
    Breaking,
    /// Needs manual update but won't break compilation.
    RequiresUpdate,
    /// Should be reviewed but may not need changes.
    Informational,
}

impl ImpactSeverity {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Breaking => "breaking",
            Self::RequiresUpdate => "requires_update",
            Self::Informational => "informational",
        }
    }

    pub fn priority(&self) -> u8 {
        match self {
            Self::Breaking => 0,
            Self::RequiresUpdate => 1,
            Self::Informational => 2,
        }
    }
}

/// An affected entity with its severity classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedEntity {
    pub entity_name: String,
    pub entity_kind: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub severity: ImpactSeverity,
    pub reason: String,
    pub distance: usize,
    pub relationship: String,
}

/// Full impact analysis result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactReport {
    pub target_entity: String,
    pub change_description: String,
    pub summary: String,
    pub affected_entities: Vec<AffectedEntity>,
    pub affected_files: Vec<AffectedFileReport>,
    pub affected_tests: Vec<String>,
    pub estimated_complexity: String,
    pub confidence: f32,
}

/// Per-file aggregation of impact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffectedFileReport {
    pub path: String,
    pub severity: ImpactSeverity,
    pub entities: Vec<String>,
    pub reason: String,
}

/// Performs detailed impact analysis using the code graph.
pub struct ImpactAnalyser {
    graph: Arc<CodeGraph>,
}

impl ImpactAnalyser {
    pub fn new(graph: Arc<CodeGraph>) -> Self {
        Self { graph }
    }

    /// Perform full impact analysis for a proposed change.
    pub async fn analyse(
        &self,
        entity_id: &EntityId,
        change_description: &str,
        max_depth: usize,
    ) -> Result<ImpactReport> {
        let target = self.graph.get_entity(entity_id).await?;
        let target_name = target
            .as_ref()
            .map(|e| e.qualified_name.clone())
            .unwrap_or_else(|| entity_id.as_str().to_string());

        // BFS through the graph to find all affected entities
        let affected = self
            .graph
            .find_affected_entities(entity_id, max_depth)
            .await?;

        // Classify severity for each affected entity
        let classified: Vec<AffectedEntity> = affected
            .iter()
            .map(|result| self.classify_impact(result, target.as_ref(), change_description))
            .collect();

        // Find affected tests specifically
        let test_results = self.graph.tests_for(entity_id).await.unwrap_or_default();
        let affected_tests: Vec<String> = test_results
            .iter()
            .map(|r| {
                format!(
                    "{} ({}:{})",
                    r.entity.name, r.entity.file_path, r.entity.start_line
                )
            })
            .collect();

        // Aggregate by file
        let affected_files = Self::aggregate_by_file(&classified);

        // Estimate complexity
        let complexity = Self::estimate_complexity(&classified, &affected_files);

        // Calculate confidence based on graph completeness
        let confidence = Self::calculate_confidence(&classified, max_depth);

        // Build summary
        let breaking_count = classified
            .iter()
            .filter(|a| a.severity == ImpactSeverity::Breaking)
            .count();
        let update_count = classified
            .iter()
            .filter(|a| a.severity == ImpactSeverity::RequiresUpdate)
            .count();
        let info_count = classified
            .iter()
            .filter(|a| a.severity == ImpactSeverity::Informational)
            .count();

        let summary = format!(
            "Change to `{}` affects {} entities across {} files. \
             {} breaking, {} requiring updates, {} informational. \
             {} test(s) affected.",
            target_name,
            classified.len(),
            affected_files.len(),
            breaking_count,
            update_count,
            info_count,
            affected_tests.len(),
        );

        Ok(ImpactReport {
            target_entity: target_name,
            change_description: change_description.to_string(),
            summary,
            affected_entities: classified,
            affected_files,
            affected_tests,
            estimated_complexity: complexity,
            confidence,
        })
    }

    /// Classify the severity of impact on an affected entity based on the
    /// relationship type, the target entity's kind, and the change description.
    fn classify_impact(
        &self,
        result: &GraphQueryResult,
        target: Option<&CodeEntity>,
        change_description: &str,
    ) -> AffectedEntity {
        let change_lower = change_description.to_lowercase();
        let target_kind = target.map(|t| &t.kind).unwrap_or(&EntityKind::Function);

        let (severity, reason) = match (&result.relationship, target_kind) {
            // Trait/interface signature changes break all implementors
            (RelationshipKind::Implements, EntityKind::Trait | EntityKind::Interface) => {
                if change_lower.contains("signature")
                    || change_lower.contains("method")
                    || change_lower.contains("add")
                    || change_lower.contains("remove")
                    || change_lower.contains("rename")
                {
                    (
                        ImpactSeverity::Breaking,
                        "Implements the changed trait/interface — must update implementation"
                            .to_string(),
                    )
                } else {
                    (
                        ImpactSeverity::RequiresUpdate,
                        "Implements the changed trait/interface — may need review".to_string(),
                    )
                }
            }

            // Callers are affected when function signatures change
            (RelationshipKind::CalledBy, _) => {
                if change_lower.contains("signature")
                    || change_lower.contains("parameter")
                    || change_lower.contains("return")
                    || change_lower.contains("remove")
                    || change_lower.contains("rename")
                {
                    (
                        ImpactSeverity::Breaking,
                        "Calls the changed entity — call site will break".to_string(),
                    )
                } else if change_lower.contains("behavior")
                    || change_lower.contains("behaviour")
                    || change_lower.contains("logic")
                    || change_lower.contains("fix")
                {
                    (
                        ImpactSeverity::RequiresUpdate,
                        "Calls the changed entity — behaviour may change".to_string(),
                    )
                } else if result.distance == 1 {
                    (
                        ImpactSeverity::RequiresUpdate,
                        "Direct caller — should verify after change".to_string(),
                    )
                } else {
                    (
                        ImpactSeverity::Informational,
                        format!("Indirect caller (distance {})", result.distance),
                    )
                }
            }

            // Importers need updating when the module interface changes
            (RelationshipKind::Imports, _) => {
                if change_lower.contains("remove")
                    || change_lower.contains("rename")
                    || change_lower.contains("move")
                {
                    (
                        ImpactSeverity::Breaking,
                        "Imports the changed module — import will break".to_string(),
                    )
                } else if result.distance == 1 {
                    (
                        ImpactSeverity::RequiresUpdate,
                        "Directly imports the changed entity".to_string(),
                    )
                } else {
                    (
                        ImpactSeverity::Informational,
                        format!("Transitive import (distance {})", result.distance),
                    )
                }
            }

            // Dependencies
            (RelationshipKind::DependsOn, _) => {
                if result.distance == 1 {
                    (
                        ImpactSeverity::RequiresUpdate,
                        "Direct dependency on the changed entity".to_string(),
                    )
                } else {
                    (
                        ImpactSeverity::Informational,
                        format!("Transitive dependency (distance {})", result.distance),
                    )
                }
            }

            // Default: informational for distant or unclassified relationships
            _ => (
                ImpactSeverity::Informational,
                format!(
                    "{} relationship at distance {}",
                    result.relationship.as_str(),
                    result.distance
                ),
            ),
        };

        AffectedEntity {
            entity_name: result.entity.qualified_name.clone(),
            entity_kind: result.entity.kind.as_str().to_string(),
            file_path: result.entity.file_path.clone(),
            start_line: result.entity.start_line,
            end_line: result.entity.end_line,
            severity,
            reason,
            distance: result.distance,
            relationship: result.relationship.as_str().to_string(),
        }
    }

    /// Aggregate affected entities by file, taking the highest severity per file.
    fn aggregate_by_file(entities: &[AffectedEntity]) -> Vec<AffectedFileReport> {
        let mut file_map: HashMap<String, (ImpactSeverity, Vec<String>, Vec<String>)> =
            HashMap::new();

        for entity in entities {
            let entry = file_map
                .entry(entity.file_path.clone())
                .or_insert_with(|| (ImpactSeverity::Informational, Vec::new(), Vec::new()));

            // Upgrade severity if this entity is more severe
            if entity.severity.priority() < entry.0.priority() {
                entry.0 = entity.severity.clone();
            }
            entry.1.push(entity.entity_name.clone());
            entry.2.push(entity.reason.clone());
        }

        let mut reports: Vec<AffectedFileReport> = file_map
            .into_iter()
            .map(|(path, (severity, entities, reasons))| {
                let reason = reasons.first().cloned().unwrap_or_default();
                AffectedFileReport {
                    path,
                    severity,
                    entities,
                    reason,
                }
            })
            .collect();

        // Sort by severity (breaking first)
        reports.sort_by_key(|r| r.severity.priority());
        reports
    }

    /// Estimate the complexity of making the change.
    fn estimate_complexity(entities: &[AffectedEntity], files: &[AffectedFileReport]) -> String {
        let breaking = entities
            .iter()
            .filter(|e| e.severity == ImpactSeverity::Breaking)
            .count();
        let file_count = files.len();

        match (breaking, file_count) {
            (0, 0) => "trivial".to_string(),
            (0, 1..=2) => "low".to_string(),
            (0, _) => "medium".to_string(),
            (1..=3, 1..=5) => "medium".to_string(),
            (1..=3, _) => "high".to_string(),
            _ => "very_high".to_string(),
        }
    }

    /// Calculate confidence based on graph completeness and depth.
    fn calculate_confidence(entities: &[AffectedEntity], max_depth: usize) -> f32 {
        let base = 0.85f32;

        // Reduce confidence for shallow searches (might miss transitive impacts)
        let depth_factor = if max_depth >= 3 {
            1.0
        } else if max_depth == 2 {
            0.9
        } else {
            0.7
        };

        // Reduce confidence when there are many entities at the boundary
        let boundary_entities = entities.iter().filter(|e| e.distance == max_depth).count();
        let boundary_factor = if boundary_entities > 5 {
            0.85
        } else if boundary_entities > 0 {
            0.95
        } else {
            1.0
        };

        (base * depth_factor * boundary_factor).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, Language};
    use crate::graph::relationships::Relationship;

    fn make_entity(id: &str, name: &str, kind: EntityKind) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test".to_string(),
            kind,
            name: name.to_string(),
            qualified_name: name.to_string(),
            file_path: format!("{name}.rs"),
            start_line: 1,
            end_line: 10,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "abc".to_string(),
            language: Language::Rust,
        }
    }

    #[tokio::test]
    async fn impact_analysis_basic() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let trait_a = make_entity("a", "MyTrait", EntityKind::Trait);
        let impl_b = make_entity("b", "MyImpl", EntityKind::Struct);
        let caller_c = make_entity("c", "caller_fn", EntityKind::Function);

        graph
            .upsert_entities(&[trait_a, impl_b, caller_c])
            .await
            .unwrap();

        // b implements a
        let rel1 = Relationship::new(
            EntityId("b".to_string()),
            EntityId("a".to_string()),
            RelationshipKind::Implements,
            1.0,
        );
        // a is called by c
        let rel2 = Relationship::new(
            EntityId("a".to_string()),
            EntityId("c".to_string()),
            RelationshipKind::CalledBy,
            1.0,
        );
        graph.upsert_relationships(&[rel1, rel2]).await.unwrap();

        let analyser = ImpactAnalyser::new(graph);
        let report = analyser
            .analyse(
                &EntityId("a".to_string()),
                "Adding a new method to the trait",
                3,
            )
            .await
            .unwrap();

        assert_eq!(report.affected_entities.len(), 2);
        assert!(!report.affected_files.is_empty());

        // The implementor should be breaking
        let impl_impact = report
            .affected_entities
            .iter()
            .find(|e| e.entity_name == "MyImpl")
            .unwrap();
        assert_eq!(impl_impact.severity, ImpactSeverity::Breaking);
    }

    #[test]
    fn severity_ordering() {
        assert!(ImpactSeverity::Breaking.priority() < ImpactSeverity::RequiresUpdate.priority());
        assert!(
            ImpactSeverity::RequiresUpdate.priority() < ImpactSeverity::Informational.priority()
        );
    }
}
