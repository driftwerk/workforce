use eyre::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::graph::entities::{CodeEntity, EntityId};
use crate::graph::CodeGraph;

/// An assembled explanation with full context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Explanation {
    /// The main explanation text.
    pub text: String,
    /// The entity being explained.
    pub entity_name: String,
    pub entity_kind: String,
    pub file_path: String,
    pub lines: String,
    /// Signature of the entity.
    pub signature: Option<String>,
    /// Doc comment if available.
    pub doc_comment: Option<String>,
    /// Dependencies this entity relies on (what it calls/imports).
    pub dependencies: Vec<EntityRef>,
    /// Entities that use this entity (callers/importers).
    pub dependents: Vec<EntityRef>,
    /// Implementations (if trait/interface).
    pub implementations: Vec<EntityRef>,
    /// Related test entities.
    pub tests: Vec<EntityRef>,
    /// Cached summary if available.
    pub summary: Option<SummaryInfo>,
    /// Suggested follow-up queries.
    pub suggestions: Vec<String>,
}

/// A reference to a related entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRef {
    pub name: String,
    pub qualified_name: String,
    pub file: String,
    pub line: usize,
    pub kind: String,
    pub relationship: String,
}

/// Summary information attached to an explanation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryInfo {
    pub text: String,
    pub is_stale: bool,
    pub granularity: String,
}

/// Aspect of an entity to focus the explanation on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplanationAspect {
    /// Full explanation (default).
    Full,
    /// Focus on control flow — what does this call and what calls it.
    Flow,
    /// Focus on dependencies — what does this import/depend on.
    Dependencies,
    /// Focus on usage — who uses this entity.
    Usage,
    /// Focus on testing — what tests cover this.
    Testing,
}

impl ExplanationAspect {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "flow" => Self::Flow,
            "dependencies" | "deps" => Self::Dependencies,
            "usage" => Self::Usage,
            "testing" | "tests" | "test" => Self::Testing,
            _ => Self::Full,
        }
    }
}

/// Builds multi-hop explanations by traversing the code graph and assembling
/// relevant context from entities, relationships, and summaries.
pub struct ExplanationBuilder {
    graph: Arc<CodeGraph>,
}

impl ExplanationBuilder {
    pub fn new(graph: Arc<CodeGraph>) -> Self {
        Self { graph }
    }

    /// Build an explanation for the given entity.
    pub async fn explain(
        &self,
        entity_id: &EntityId,
        aspect: &ExplanationAspect,
    ) -> Result<Explanation> {
        let entity = self
            .graph
            .get_entity(entity_id)
            .await?
            .ok_or_else(|| eyre::eyre!("Entity not found: {}", entity_id.as_str()))?;

        // Gather context based on the requested aspect
        let (dependencies, dependents, implementations, tests) = match aspect {
            ExplanationAspect::Full => {
                let deps = self.gather_callees(entity_id).await?;
                let callers = self.gather_callers(entity_id).await?;
                let impls = self.gather_implementations(entity_id).await?;
                let tests = self.gather_tests(entity_id).await?;
                (deps, callers, impls, tests)
            }
            ExplanationAspect::Flow => {
                let deps = self.gather_callees(entity_id).await?;
                let callers = self.gather_callers(entity_id).await?;
                (deps, callers, vec![], vec![])
            }
            ExplanationAspect::Dependencies => {
                let deps = self.gather_callees(entity_id).await?;
                let imports = self.gather_imports(entity_id).await?;
                let mut all_deps = deps;
                all_deps.extend(imports);
                (all_deps, vec![], vec![], vec![])
            }
            ExplanationAspect::Usage => {
                let callers = self.gather_callers(entity_id).await?;
                let impls = self.gather_implementations(entity_id).await?;
                (vec![], callers, impls, vec![])
            }
            ExplanationAspect::Testing => {
                let tests = self.gather_tests(entity_id).await?;
                (vec![], vec![], vec![], tests)
            }
        };

        // Get cached summary if available
        let summary = self.get_entity_summary(entity_id).await?;

        // Compose the explanation text
        let text = self.compose_text(
            &entity,
            &dependencies,
            &dependents,
            &implementations,
            &tests,
            &summary,
            aspect,
        );

        // Generate suggestions for follow-up
        let suggestions = self.generate_suggestions(
            &entity,
            &dependencies,
            &dependents,
            &implementations,
            &tests,
        );

        Ok(Explanation {
            text,
            entity_name: entity.name.clone(),
            entity_kind: entity.kind.as_str().to_string(),
            file_path: entity.file_path.clone(),
            lines: format!("{}-{}", entity.start_line, entity.end_line),
            signature: entity.signature.clone(),
            doc_comment: entity.doc_comment.clone(),
            dependencies,
            dependents,
            implementations,
            tests,
            summary,
            suggestions,
        })
    }

    /// Gather entities that this entity calls.
    async fn gather_callees(&self, entity_id: &EntityId) -> Result<Vec<EntityRef>> {
        let results = self.graph.callees_of(entity_id, 1).await?;
        Ok(results
            .into_iter()
            .map(|r| EntityRef {
                name: r.entity.name.clone(),
                qualified_name: r.entity.qualified_name.clone(),
                file: r.entity.file_path.clone(),
                line: r.entity.start_line,
                kind: r.entity.kind.as_str().to_string(),
                relationship: "calls".to_string(),
            })
            .collect())
    }

    /// Gather entities that call this entity.
    async fn gather_callers(&self, entity_id: &EntityId) -> Result<Vec<EntityRef>> {
        let results = self.graph.callers_of(entity_id, 1).await?;
        Ok(results
            .into_iter()
            .map(|r| EntityRef {
                name: r.entity.name.clone(),
                qualified_name: r.entity.qualified_name.clone(),
                file: r.entity.file_path.clone(),
                line: r.entity.start_line,
                kind: r.entity.kind.as_str().to_string(),
                relationship: "called_by".to_string(),
            })
            .collect())
    }

    /// Gather entities that import this entity's module.
    async fn gather_imports(&self, entity_id: &EntityId) -> Result<Vec<EntityRef>> {
        let results = self.graph.imported_by(entity_id, 1).await?;
        Ok(results
            .into_iter()
            .map(|r| EntityRef {
                name: r.entity.name.clone(),
                qualified_name: r.entity.qualified_name.clone(),
                file: r.entity.file_path.clone(),
                line: r.entity.start_line,
                kind: r.entity.kind.as_str().to_string(),
                relationship: "imports".to_string(),
            })
            .collect())
    }

    /// Gather entities that implement this trait/interface.
    async fn gather_implementations(&self, entity_id: &EntityId) -> Result<Vec<EntityRef>> {
        let results = self.graph.implementations_of(entity_id).await?;
        Ok(results
            .into_iter()
            .map(|r| EntityRef {
                name: r.entity.name.clone(),
                qualified_name: r.entity.qualified_name.clone(),
                file: r.entity.file_path.clone(),
                line: r.entity.start_line,
                kind: r.entity.kind.as_str().to_string(),
                relationship: "implements".to_string(),
            })
            .collect())
    }

    /// Gather tests for this entity.
    async fn gather_tests(&self, entity_id: &EntityId) -> Result<Vec<EntityRef>> {
        let results = self.graph.tests_for(entity_id).await?;
        Ok(results
            .into_iter()
            .map(|r| EntityRef {
                name: r.entity.name.clone(),
                qualified_name: r.entity.qualified_name.clone(),
                file: r.entity.file_path.clone(),
                line: r.entity.start_line,
                kind: r.entity.kind.as_str().to_string(),
                relationship: "tests_for".to_string(),
            })
            .collect())
    }

    /// Get the cached summary for an entity, if available.
    async fn get_entity_summary(&self, entity_id: &EntityId) -> Result<Option<SummaryInfo>> {
        // Try function-level first, then file-level
        for granularity in &["function", "file", "module"] {
            if let Some((text, is_stale)) = self.graph.get_summary(entity_id, granularity).await? {
                return Ok(Some(SummaryInfo {
                    text,
                    is_stale,
                    granularity: granularity.to_string(),
                }));
            }
        }
        Ok(None)
    }

    /// Compose a human-readable explanation from gathered context.
    fn compose_text(
        &self,
        entity: &CodeEntity,
        dependencies: &[EntityRef],
        dependents: &[EntityRef],
        implementations: &[EntityRef],
        tests: &[EntityRef],
        summary: &Option<SummaryInfo>,
        aspect: &ExplanationAspect,
    ) -> String {
        let mut parts = Vec::new();

        // Header
        parts.push(format!(
            "**{} `{}`** in `{}` (lines {}-{})",
            entity.kind.as_str(),
            entity.qualified_name,
            entity.file_path,
            entity.start_line,
            entity.end_line,
        ));

        // Signature
        if let Some(sig) = &entity.signature {
            parts.push(format!("\nSignature: `{}`", sig));
        }

        // Doc comment
        if let Some(doc) = &entity.doc_comment {
            parts.push(format!("\nDocumentation: {}", doc.trim()));
        }

        // Summary
        if let Some(s) = summary {
            let stale_marker = if s.is_stale { " *(stale)*" } else { "" };
            parts.push(format!("\nSummary{}: {}", stale_marker, s.text));
        }

        // Aspect-specific sections
        match aspect {
            ExplanationAspect::Full | ExplanationAspect::Flow => {
                if !dependencies.is_empty() {
                    parts.push(format!("\n**Calls** ({}):", dependencies.len()));
                    for dep in dependencies.iter().take(10) {
                        parts.push(format!(
                            "  - `{}` ({}) in `{}:{}`",
                            dep.qualified_name, dep.kind, dep.file, dep.line
                        ));
                    }
                    if dependencies.len() > 10 {
                        parts.push(format!("  ... and {} more", dependencies.len() - 10));
                    }
                }

                if !dependents.is_empty() {
                    parts.push(format!("\n**Called by** ({}):", dependents.len()));
                    for dep in dependents.iter().take(10) {
                        parts.push(format!(
                            "  - `{}` ({}) in `{}:{}`",
                            dep.qualified_name, dep.kind, dep.file, dep.line
                        ));
                    }
                    if dependents.len() > 10 {
                        parts.push(format!("  ... and {} more", dependents.len() - 10));
                    }
                }
            }
            ExplanationAspect::Dependencies => {
                if !dependencies.is_empty() {
                    parts.push(format!("\n**Dependencies** ({}):", dependencies.len()));
                    for dep in dependencies.iter().take(15) {
                        parts.push(format!(
                            "  - `{}` ({}) — {} in `{}:{}`",
                            dep.qualified_name, dep.kind, dep.relationship, dep.file, dep.line
                        ));
                    }
                }
            }
            ExplanationAspect::Usage => {
                if !dependents.is_empty() {
                    parts.push(format!("\n**Used by** ({}):", dependents.len()));
                    for dep in dependents.iter().take(15) {
                        parts.push(format!(
                            "  - `{}` ({}) in `{}:{}`",
                            dep.qualified_name, dep.kind, dep.file, dep.line
                        ));
                    }
                }
            }
            ExplanationAspect::Testing => {}
        }

        // Implementations (for traits/interfaces)
        if !implementations.is_empty() {
            parts.push(format!(
                "\n**Implementations** ({}):",
                implementations.len()
            ));
            for imp in implementations.iter().take(10) {
                parts.push(format!(
                    "  - `{}` in `{}:{}`",
                    imp.qualified_name, imp.file, imp.line
                ));
            }
        }

        // Tests
        if !tests.is_empty() {
            parts.push(format!("\n**Tests** ({}):", tests.len()));
            for test in tests.iter().take(10) {
                parts.push(format!(
                    "  - `{}` in `{}:{}`",
                    test.name, test.file, test.line
                ));
            }
        } else if *aspect == ExplanationAspect::Testing || *aspect == ExplanationAspect::Full {
            parts.push("\n**Tests**: No tests found for this entity.".to_string());
        }

        parts.join("\n")
    }

    /// Generate suggestions for follow-up queries based on the explanation.
    fn generate_suggestions(
        &self,
        entity: &CodeEntity,
        dependencies: &[EntityRef],
        dependents: &[EntityRef],
        implementations: &[EntityRef],
        tests: &[EntityRef],
    ) -> Vec<String> {
        let mut suggestions = Vec::new();

        if !dependents.is_empty() && dependents.len() > 3 {
            suggestions.push(format!(
                "Use knowledge.impact to analyse what would break if `{}` changes",
                entity.name
            ));
        }

        if tests.is_empty() {
            suggestions.push(format!(
                "No tests found — consider using code.search to check for test coverage of `{}`",
                entity.name
            ));
        }

        if !dependencies.is_empty() {
            suggestions.push(format!(
                "Use knowledge.graph callers_of to see the full reverse call graph for `{}`",
                entity.name
            ));
        }

        if !implementations.is_empty() {
            suggestions.push(
                "Use knowledge.explain on individual implementations to understand each variant"
                    .to_string(),
            );
        }

        suggestions.push(format!(
            "Use code.read to see the full source of `{}` at `{}:{}-{}`",
            entity.name, entity.file_path, entity.start_line, entity.end_line
        ));

        suggestions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, EntityKind, Language};
    use crate::graph::relationships::{Relationship, RelationshipKind};

    #[tokio::test]
    async fn explain_entity_with_context() {
        let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
        graph.register_repo("test", "Test", "/tmp").await.unwrap();

        let fn_a = CodeEntity {
            id: EntityId("a".to_string()),
            repo_id: "test".to_string(),
            kind: EntityKind::Function,
            name: "process".to_string(),
            qualified_name: "crate::handler::process".to_string(),
            file_path: "src/handler.rs".to_string(),
            start_line: 10,
            end_line: 30,
            signature: Some("pub async fn process(msg: Message) -> Result<()>".to_string()),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: Some("Process an incoming message.".to_string()),
            content_hash: "abc".to_string(),
            language: Language::Rust,
        };
        let fn_b = CodeEntity {
            id: EntityId("b".to_string()),
            repo_id: "test".to_string(),
            kind: EntityKind::Function,
            name: "validate".to_string(),
            qualified_name: "crate::validation::validate".to_string(),
            file_path: "src/validation.rs".to_string(),
            start_line: 5,
            end_line: 20,
            signature: Some("fn validate(msg: &Message) -> bool".to_string()),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "def".to_string(),
            language: Language::Rust,
        };
        let test_fn = CodeEntity {
            id: EntityId("t".to_string()),
            repo_id: "test".to_string(),
            kind: EntityKind::Test,
            name: "test_process".to_string(),
            qualified_name: "crate::tests::test_process".to_string(),
            file_path: "src/handler.rs".to_string(),
            start_line: 50,
            end_line: 70,
            signature: Some("fn test_process()".to_string()),
            visibility: CodeVisibility::Private,
            parent_id: None,
            doc_comment: None,
            content_hash: "ghi".to_string(),
            language: Language::Rust,
        };

        graph.upsert_entities(&[fn_a, fn_b, test_fn]).await.unwrap();

        // process calls validate
        let rel1 = Relationship::new(
            EntityId("a".to_string()),
            EntityId("b".to_string()),
            RelationshipKind::Calls,
            1.0,
        );
        // test_process tests process
        let rel2 = Relationship::new(
            EntityId("t".to_string()),
            EntityId("a".to_string()),
            RelationshipKind::TestsFor,
            1.0,
        );
        graph.upsert_relationships(&[rel1, rel2]).await.unwrap();

        let builder = ExplanationBuilder::new(graph);
        let explanation = builder
            .explain(&EntityId("a".to_string()), &ExplanationAspect::Full)
            .await
            .unwrap();

        assert_eq!(explanation.entity_name, "process");
        assert!(!explanation.dependencies.is_empty());
        assert!(!explanation.tests.is_empty());
        assert!(explanation.text.contains("process"));
        assert!(explanation.text.contains("handler.rs"));
        assert!(!explanation.suggestions.is_empty());
    }

    #[test]
    fn aspect_from_str() {
        assert_eq!(ExplanationAspect::from_str("flow"), ExplanationAspect::Flow);
        assert_eq!(
            ExplanationAspect::from_str("dependencies"),
            ExplanationAspect::Dependencies
        );
        assert_eq!(
            ExplanationAspect::from_str("tests"),
            ExplanationAspect::Testing
        );
        assert_eq!(
            ExplanationAspect::from_str("usage"),
            ExplanationAspect::Usage
        );
        assert_eq!(
            ExplanationAspect::from_str("anything"),
            ExplanationAspect::Full
        );
    }
}
