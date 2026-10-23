use serde::{Deserialize, Serialize};

use super::entities::EntityId;

/// A relationship between two code entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: String,
    pub source: EntityId,
    pub target: EntityId,
    pub kind: RelationshipKind,
    /// Confidence score: 1.0 for syntactic (AST-derived), 0.6-0.8 for heuristic.
    pub confidence: f32,
    pub metadata: Option<serde_json::Value>,
}

impl Relationship {
    /// Create a new relationship with a deterministic ID.
    pub fn new(
        source: EntityId,
        target: EntityId,
        kind: RelationshipKind,
        confidence: f32,
    ) -> Self {
        let id = format!("{}:{}:{}", source.as_str(), kind.as_str(), target.as_str());
        let hash = blake3::hash(id.as_bytes());
        Self {
            id: hash.to_hex()[..16].to_string(),
            source,
            target,
            kind,
            confidence,
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// The kind of relationship between code entities.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    /// Module/file contains entity (parent-child in AST).
    Contains,
    /// Entity is defined in file.
    DefinedIn,
    /// Struct implements Trait (Rust) / Class implements Interface (TS).
    Implements,
    /// Class extends another class (TS/Python/Ruby).
    Extends,
    /// Entity has type X.
    TypeOf,
    /// File/module imports from another file/module.
    Imports,
    /// Package depends on another package (Cargo.toml, package.json).
    DependsOn,
    /// Function calls another function.
    Calls,
    /// Inverse of Calls — materialised for fast lookups.
    CalledBy,
    /// Test function tests a specific entity.
    TestsFor,
    /// Entity is documented by a doc comment.
    DocumentedBy,
}

impl RelationshipKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Contains => "contains",
            Self::DefinedIn => "defined_in",
            Self::Implements => "implements",
            Self::Extends => "extends",
            Self::TypeOf => "type_of",
            Self::Imports => "imports",
            Self::DependsOn => "depends_on",
            Self::Calls => "calls",
            Self::CalledBy => "called_by",
            Self::TestsFor => "tests_for",
            Self::DocumentedBy => "documented_by",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "contains" => Self::Contains,
            "defined_in" => Self::DefinedIn,
            "implements" => Self::Implements,
            "extends" => Self::Extends,
            "type_of" => Self::TypeOf,
            "imports" => Self::Imports,
            "depends_on" => Self::DependsOn,
            "calls" => Self::Calls,
            "called_by" => Self::CalledBy,
            "tests_for" => Self::TestsFor,
            "documented_by" => Self::DocumentedBy,
            _ => Self::Calls, // fallback
        }
    }

    /// Get the inverse relationship kind (if applicable).
    pub fn inverse(&self) -> Option<Self> {
        match self {
            Self::Contains => Some(Self::DefinedIn),
            Self::DefinedIn => Some(Self::Contains),
            Self::Calls => Some(Self::CalledBy),
            Self::CalledBy => Some(Self::Calls),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationship_deterministic_id() {
        let source = EntityId("abc123".to_string());
        let target = EntityId("def456".to_string());
        let r1 = Relationship::new(source.clone(), target.clone(), RelationshipKind::Calls, 1.0);
        let r2 = Relationship::new(source, target, RelationshipKind::Calls, 1.0);
        assert_eq!(r1.id, r2.id);
    }

    #[test]
    fn relationship_kind_roundtrip() {
        let kinds = vec![
            RelationshipKind::Contains,
            RelationshipKind::Calls,
            RelationshipKind::Implements,
            RelationshipKind::Imports,
            RelationshipKind::TestsFor,
        ];
        for kind in kinds {
            assert_eq!(RelationshipKind::from_str(kind.as_str()), kind);
        }
    }

    #[test]
    fn inverse_relationships() {
        assert_eq!(
            RelationshipKind::Calls.inverse(),
            Some(RelationshipKind::CalledBy)
        );
        assert_eq!(
            RelationshipKind::Contains.inverse(),
            Some(RelationshipKind::DefinedIn)
        );
        assert_eq!(RelationshipKind::Imports.inverse(), None);
    }
}
