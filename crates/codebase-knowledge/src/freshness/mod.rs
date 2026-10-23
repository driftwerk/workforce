pub mod git_monitor;
pub mod staleness;

use eyre::Result;
use std::sync::Arc;

use crate::graph::entities::EntityId;
use crate::graph::CodeGraph;

/// Result of a freshness check.
#[derive(Debug, Clone)]
pub enum FreshnessResult {
    /// The entity's knowledge is current.
    Fresh { last_indexed: String },
    /// The entity has changed since last indexing.
    Stale {
        last_indexed: String,
        affected_dependents: usize,
    },
    /// Entity not yet indexed.
    Unknown,
}

/// Tracks the freshness of indexed knowledge by comparing content hashes
/// against the actual files on disk.
pub struct FreshnessTracker {
    graph: Arc<CodeGraph>,
}

impl FreshnessTracker {
    pub fn new(graph: Arc<CodeGraph>) -> Self {
        Self { graph }
    }

    /// Check if the knowledge for a specific entity is current.
    /// Compares the stored content hash against a live BLAKE3 hash of the entity's
    /// source range on disk.
    pub async fn check_entity(&self, entity_id: &EntityId) -> Result<FreshnessResult> {
        let entity = match self.graph.get_entity(entity_id).await? {
            Some(e) => e,
            None => return Ok(FreshnessResult::Unknown),
        };

        // Get the indexed_at timestamp from the DB
        let last_indexed = self
            .graph
            .get_entity_indexed_at(entity_id)
            .await?
            .unwrap_or_default();

        // Read the actual file from disk
        let content = match tokio::fs::read_to_string(&entity.file_path).await {
            Ok(c) => c,
            Err(_) => {
                // File no longer exists — the entity is definitely stale
                let dependents = self.graph.count_dependents(entity_id).await.unwrap_or(0);
                return Ok(FreshnessResult::Stale {
                    last_indexed,
                    affected_dependents: dependents,
                });
            }
        };

        // Extract the entity's source range and hash it
        let lines: Vec<&str> = content.lines().collect();
        let start = entity.start_line.saturating_sub(1);
        let end = entity.end_line.min(lines.len());

        if start >= lines.len() || start >= end {
            // Entity's line range is outside file bounds — likely stale
            let dependents = self.graph.count_dependents(entity_id).await.unwrap_or(0);
            return Ok(FreshnessResult::Stale {
                last_indexed,
                affected_dependents: dependents,
            });
        }

        let source = lines[start..end].join("\n");
        let live_hash = blake3::hash(source.as_bytes()).to_hex()[..16].to_string();

        if live_hash == entity.content_hash {
            Ok(FreshnessResult::Fresh { last_indexed })
        } else {
            let dependents = self.graph.count_dependents(entity_id).await.unwrap_or(0);
            Ok(FreshnessResult::Stale {
                last_indexed,
                affected_dependents: dependents,
            })
        }
    }

    /// Check if a file has changed since last indexing.
    /// Compares the stored full-file content hash against a live BLAKE3 hash of the file on disk.
    pub async fn check_file(&self, repo_id: &str, file_path: &str) -> Result<FreshnessResult> {
        let stored_hash = self.graph.get_file_hash(repo_id, file_path).await?;

        match stored_hash {
            None => Ok(FreshnessResult::Unknown),
            Some(stored) => {
                let last_indexed = self
                    .graph
                    .get_file_indexed_at(repo_id, file_path)
                    .await?
                    .unwrap_or_default();

                let content = match tokio::fs::read(file_path).await {
                    Ok(c) => c,
                    Err(_) => {
                        // File no longer exists on disk — stale
                        return Ok(FreshnessResult::Stale {
                            last_indexed,
                            affected_dependents: 0,
                        });
                    }
                };
                let live_hash = blake3::hash(&content).to_hex().to_string();

                if live_hash == stored {
                    Ok(FreshnessResult::Fresh { last_indexed })
                } else {
                    // Count entities in this file that have dependents
                    let entities = self.graph.find_entities_in_file(repo_id, file_path).await?;
                    let mut total_dependents = 0;
                    for entity in &entities {
                        total_dependents +=
                            self.graph.count_dependents(&entity.id).await.unwrap_or(0);
                    }
                    Ok(FreshnessResult::Stale {
                        last_indexed,
                        affected_dependents: total_dependents,
                    })
                }
            }
        }
    }

    /// Batch-check freshness for all files in a repository.
    /// Returns a list of stale file paths.
    pub async fn find_stale_files(&self, repo_id: &str) -> Result<Vec<String>> {
        let file_paths = self.graph.get_file_paths(repo_id).await?;
        let mut stale = Vec::new();
        for path in &file_paths {
            if let FreshnessResult::Stale { .. } = self.check_file(repo_id, path).await? {
                stale.push(path.clone());
            }
        }
        Ok(stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeEntity, CodeVisibility, EntityKind, Language};
    use tempfile::TempDir;

    #[tokio::test]
    async fn check_entity_fresh() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.rs");
        let source = "fn hello() {\n    println!(\"world\");\n}\n";
        tokio::fs::write(&file_path, source).await.unwrap();

        // Hash lines 1-3 (the function body)
        let lines: Vec<&str> = source.lines().collect();
        let entity_source = lines[0..3].join("\n");
        let hash = blake3::hash(entity_source.as_bytes()).to_hex()[..16].to_string();

        let entity = CodeEntity {
            id: EntityId("test-entity".to_string()),
            repo_id: "test".to_string(),
            kind: EntityKind::Function,
            name: "hello".to_string(),
            qualified_name: "hello".to_string(),
            file_path: file_path.to_string_lossy().to_string(),
            start_line: 1,
            end_line: 3,
            signature: Some("fn hello()".to_string()),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: hash,
            language: Language::Rust,
        };
        graph.upsert_entities(&[entity]).await.unwrap();

        let tracker = FreshnessTracker::new(graph);
        let result = tracker
            .check_entity(&EntityId("test-entity".to_string()))
            .await
            .unwrap();
        assert!(matches!(result, FreshnessResult::Fresh { .. }));
    }

    #[tokio::test]
    async fn check_entity_stale() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.rs");
        tokio::fs::write(&file_path, "fn hello() {\n    println!(\"changed\");\n}\n")
            .await
            .unwrap();

        let entity = CodeEntity {
            id: EntityId("test-entity".to_string()),
            repo_id: "test".to_string(),
            kind: EntityKind::Function,
            name: "hello".to_string(),
            qualified_name: "hello".to_string(),
            file_path: file_path.to_string_lossy().to_string(),
            start_line: 1,
            end_line: 3,
            signature: Some("fn hello()".to_string()),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "stale_hash_value".to_string(),
            language: Language::Rust,
        };
        graph.upsert_entities(&[entity]).await.unwrap();

        let tracker = FreshnessTracker::new(graph);
        let result = tracker
            .check_entity(&EntityId("test-entity".to_string()))
            .await
            .unwrap();
        assert!(matches!(result, FreshnessResult::Stale { .. }));
    }

    #[tokio::test]
    async fn check_entity_unknown() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        let tracker = FreshnessTracker::new(graph);
        let result = tracker
            .check_entity(&EntityId("nonexistent".to_string()))
            .await
            .unwrap();
        assert!(matches!(result, FreshnessResult::Unknown));
    }
}
