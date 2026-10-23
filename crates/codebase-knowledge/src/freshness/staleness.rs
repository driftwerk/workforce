use eyre::Result;
use std::sync::Arc;
use tracing::{debug, info};

use crate::graph::entities::EntityId;
use crate::graph::CodeGraph;

/// Propagates staleness through the code graph when entities change.
/// When an entity is modified, its summaries become stale — and so do the summaries
/// of any entity that depends on it (callers, importers, implementors), up to a
/// configurable depth (default: 2 hops).
pub struct StalenessPropagator {
    graph: Arc<CodeGraph>,
    max_hops: usize,
}

impl StalenessPropagator {
    pub fn new(graph: Arc<CodeGraph>) -> Self {
        Self { graph, max_hops: 2 }
    }

    /// Create a propagator with a custom hop limit.
    pub fn with_max_hops(graph: Arc<CodeGraph>, max_hops: usize) -> Self {
        Self { graph, max_hops }
    }

    /// Mark an entity's summaries as stale and propagate to dependents.
    /// Returns the list of entity IDs that were marked stale (including the changed entity).
    pub async fn propagate(&self, changed_entity: &EntityId) -> Result<Vec<EntityId>> {
        // Find all affected entities (up to max_hops via CalledBy, Imports, DependsOn, Implements)
        let affected = self
            .graph
            .find_affected_entities(changed_entity, self.max_hops)
            .await?;

        // Collect all entity IDs that need staleness marking: the changed entity + all affected
        let mut stale_ids: Vec<EntityId> = vec![changed_entity.clone()];
        stale_ids.extend(affected.iter().map(|r| r.entity.id.clone()));

        // Mark all their summaries as stale in the database
        let marked = self.graph.mark_summaries_stale(&stale_ids).await?;

        info!(
            "Staleness propagated from {}: {} entities affected, {} summaries marked stale",
            changed_entity.as_str(),
            stale_ids.len(),
            marked
        );

        for result in &affected {
            debug!(
                "  {} '{}' (distance={}, via {})",
                result.entity.kind.as_str(),
                result.entity.name,
                result.distance,
                result.relationship.as_str(),
            );
        }

        Ok(stale_ids)
    }

    /// Propagate staleness for all entities in a file.
    /// Call this when a file has been detected as changed.
    pub async fn propagate_file(&self, repo_id: &str, file_path: &str) -> Result<Vec<EntityId>> {
        let entities = self.graph.find_entities_in_file(repo_id, file_path).await?;
        let mut all_stale = Vec::new();

        for entity in &entities {
            let stale = self.propagate(&entity.id).await?;
            for id in stale {
                if !all_stale
                    .iter()
                    .any(|existing: &EntityId| existing.as_str() == id.as_str())
                {
                    all_stale.push(id);
                }
            }
        }

        info!(
            "File {} staleness: {} total entities marked stale",
            file_path,
            all_stale.len()
        );

        Ok(all_stale)
    }

    /// Propagate staleness for multiple changed files at once (e.g., after a git pull).
    pub async fn propagate_files(
        &self,
        repo_id: &str,
        file_paths: &[String],
    ) -> Result<Vec<EntityId>> {
        let mut all_stale = Vec::new();

        for path in file_paths {
            let stale = self.propagate_file(repo_id, path).await?;
            for id in stale {
                if !all_stale
                    .iter()
                    .any(|existing: &EntityId| existing.as_str() == id.as_str())
                {
                    all_stale.push(id);
                }
            }
        }

        info!(
            "Batch staleness propagation for {} files: {} total entities stale",
            file_paths.len(),
            all_stale.len()
        );

        Ok(all_stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeEntity, CodeVisibility, EntityKind, Language};
    use crate::graph::relationships::{Relationship, RelationshipKind};

    fn make_entity(id: &str, name: &str, file: &str) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test".to_string(),
            kind: EntityKind::Function,
            name: name.to_string(),
            qualified_name: name.to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 10,
            signature: Some(format!("fn {name}()")),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "abc123".to_string(),
            language: Language::Rust,
        }
    }

    #[tokio::test]
    async fn propagates_staleness_through_graph() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        // Create entities: a -> b -> c (a is called by b, b is called by c)
        let a = make_entity("a", "fn_a", "a.rs");
        let b = make_entity("b", "fn_b", "b.rs");
        let c = make_entity("c", "fn_c", "c.rs");
        graph.upsert_entities(&[a, b, c]).await.unwrap();

        // b calls a (so a is CalledBy b)
        let rel1 = Relationship::new(
            EntityId("a".to_string()),
            EntityId("b".to_string()),
            RelationshipKind::CalledBy,
            1.0,
        );
        // c calls b (so b is CalledBy c)
        let rel2 = Relationship::new(
            EntityId("b".to_string()),
            EntityId("c".to_string()),
            RelationshipKind::CalledBy,
            1.0,
        );
        graph.upsert_relationships(&[rel1, rel2]).await.unwrap();

        // Store summaries for all three
        graph
            .upsert_summary(
                &EntityId("a".to_string()),
                "function",
                "Summary of a",
                "hash_a",
                "test",
            )
            .await
            .unwrap();
        graph
            .upsert_summary(
                &EntityId("b".to_string()),
                "function",
                "Summary of b",
                "hash_b",
                "test",
            )
            .await
            .unwrap();
        graph
            .upsert_summary(
                &EntityId("c".to_string()),
                "function",
                "Summary of c",
                "hash_c",
                "test",
            )
            .await
            .unwrap();

        // Propagate from a (which was changed)
        let propagator = StalenessPropagator::new(graph.clone());
        let stale = propagator
            .propagate(&EntityId("a".to_string()))
            .await
            .unwrap();

        // a, b, c should all be stale (a changed, b calls a, c calls b)
        assert_eq!(stale.len(), 3);

        // Verify summaries are marked stale in DB
        let (_, is_stale_a) = graph
            .get_summary(&EntityId("a".to_string()), "function")
            .await
            .unwrap()
            .unwrap();
        assert!(is_stale_a);

        let (_, is_stale_b) = graph
            .get_summary(&EntityId("b".to_string()), "function")
            .await
            .unwrap()
            .unwrap();
        assert!(is_stale_b);

        let (_, is_stale_c) = graph
            .get_summary(&EntityId("c".to_string()), "function")
            .await
            .unwrap()
            .unwrap();
        assert!(is_stale_c);
    }

    #[tokio::test]
    async fn propagation_respects_max_hops() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        // Chain: a -> b -> c -> d (3 hops)
        let entities: Vec<CodeEntity> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| make_entity(id, &format!("fn_{id}"), &format!("{id}.rs")))
            .collect();
        graph.upsert_entities(&entities).await.unwrap();

        let rels = vec![
            Relationship::new(
                EntityId("a".to_string()),
                EntityId("b".to_string()),
                RelationshipKind::CalledBy,
                1.0,
            ),
            Relationship::new(
                EntityId("b".to_string()),
                EntityId("c".to_string()),
                RelationshipKind::CalledBy,
                1.0,
            ),
            Relationship::new(
                EntityId("c".to_string()),
                EntityId("d".to_string()),
                RelationshipKind::CalledBy,
                1.0,
            ),
        ];
        graph.upsert_relationships(&rels).await.unwrap();

        // With max_hops=1, only b should be reached
        let propagator = StalenessPropagator::with_max_hops(graph.clone(), 1);
        let stale = propagator
            .propagate(&EntityId("a".to_string()))
            .await
            .unwrap();

        // a (self) + b (1 hop)
        assert_eq!(stale.len(), 2);
        let ids: Vec<&str> = stale.iter().map(|id| id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
    }
}
