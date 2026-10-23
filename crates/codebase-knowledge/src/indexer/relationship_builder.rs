use std::collections::{HashMap, HashSet};

use crate::graph::entities::{CodeEntity, EntityId, EntityKind, Language};
use crate::graph::relationships::{Relationship, RelationshipKind};
use crate::indexer::symbol_extractor::{CallReference, ImplTraitInfo};

/// Builds relationships between code entities using AST-derived information
/// and cross-file name resolution.
pub struct RelationshipBuilder {
    #[allow(dead_code)]
    repo_id: String,
}

impl RelationshipBuilder {
    pub fn new(repo_id: &str) -> Self {
        Self {
            repo_id: repo_id.to_string(),
        }
    }

    /// Build intra-file relationships (Contains, DefinedIn) from parent_id fields.
    pub fn build_intra_file_relationships(&self, entities: &[CodeEntity]) -> Vec<Relationship> {
        let mut relationships = Vec::new();

        // Find the file entity
        let file_entity = entities.iter().find(|e| e.kind == EntityKind::File);

        for entity in entities {
            if entity.kind == EntityKind::File {
                continue;
            }

            // DefinedIn: entity → file
            if let Some(file_ent) = file_entity {
                relationships.push(Relationship::new(
                    entity.id.clone(),
                    file_ent.id.clone(),
                    RelationshipKind::DefinedIn,
                    1.0,
                ));
            }

            // Contains: parent → entity (if parent_id is set)
            if let Some(ref parent_id) = entity.parent_id {
                if entities.iter().any(|e| &e.id == parent_id) {
                    relationships.push(Relationship::new(
                        parent_id.clone(),
                        entity.id.clone(),
                        RelationshipKind::Contains,
                        1.0,
                    ));
                }
            }

            // TestsFor: if this is a test entity, try to find what it tests
            if entity.kind == EntityKind::Test {
                if let Some(tested) = self.infer_tested_entity(entity, entities) {
                    relationships.push(Relationship::new(
                        entity.id.clone(),
                        tested,
                        RelationshipKind::TestsFor,
                        0.7,
                    ));
                }
            }
        }

        relationships
    }

    /// Build cross-file relationships by analysing the full set of entities.
    ///
    /// Detects:
    /// - **TypeOf**: Type references in function signatures
    /// - **Imports**: Rust `use` paths, TS/JS `import` paths, Python `from`/`import`
    /// - **Implements**: From `ImplTraitInfo` data collected during symbol extraction
    /// - **Calls**: From `CallReference` data collected during symbol extraction
    /// - **CalledBy**: Materialised inverse of Calls for fast reverse lookups
    pub fn build_cross_file_relationships(
        &self,
        entities: &[CodeEntity],
        impl_traits: &[ImplTraitInfo],
        calls: &[CallReference],
    ) -> Vec<Relationship> {
        let mut relationships = Vec::new();

        // Build lookup indices for name resolution
        let by_name = self.build_name_index(entities);
        let by_qualified = self.build_qualified_name_index(entities);
        let file_entities: HashMap<&str, &CodeEntity> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::File)
            .map(|e| (e.file_path.as_str(), e))
            .collect();

        // Build a map of trait names to their entity IDs for Implements detection
        let traits_by_name: HashMap<&str, &CodeEntity> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Trait || e.kind == EntityKind::Interface)
            .map(|e| (e.name.as_str(), e))
            .collect();

        // Build Implements relationships from impl_traits data
        let mut seen_implements: HashSet<(String, String)> = HashSet::new();
        for info in impl_traits {
            let trait_name = info
                .trait_name
                .split('<')
                .next()
                .unwrap_or(&info.trait_name)
                .trim();
            let type_name = info
                .type_name
                .split('<')
                .next()
                .unwrap_or(&info.type_name)
                .trim();

            if let Some(trait_entity) = traits_by_name.get(trait_name) {
                if let Some(type_entity) = entities.iter().find(|e| {
                    matches!(
                        e.kind,
                        EntityKind::Struct | EntityKind::Class | EntityKind::Enum
                    ) && e.name == type_name
                }) {
                    let key = (
                        type_entity.id.as_str().to_string(),
                        trait_entity.id.as_str().to_string(),
                    );
                    if seen_implements.insert(key) {
                        relationships.push(Relationship::new(
                            type_entity.id.clone(),
                            trait_entity.id.clone(),
                            RelationshipKind::Implements,
                            1.0,
                        ));
                    }
                }
            }
        }

        for entity in entities {
            if entity.kind == EntityKind::File {
                continue;
            }

            // Detect type references from signatures
            if let Some(sig) = &entity.signature {
                let type_refs = self.extract_type_references(sig, &entity.language);
                for type_name in &type_refs {
                    if let Some(target) = by_name.get(type_name.as_str()) {
                        if target.id != entity.id && target.file_path != entity.file_path {
                            relationships.push(Relationship::new(
                                entity.id.clone(),
                                target.id.clone(),
                                RelationshipKind::TypeOf,
                                0.8,
                            ));
                        }
                    }
                }
            }

            // Detect file-to-file import relationships
            if entity.kind == EntityKind::Import || entity.kind == EntityKind::Module {
                if let Some(import_target) =
                    self.resolve_import(entity, &file_entities, &by_qualified)
                {
                    // Get the file entity for this entity's file
                    if let Some(source_file) = file_entities.get(entity.file_path.as_str()) {
                        relationships.push(Relationship::new(
                            source_file.id.clone(),
                            import_target,
                            RelationshipKind::Imports,
                            0.9,
                        ));
                    }
                }
            }
        }

        // Build Calls relationships from call references
        let mut seen_calls: HashSet<(String, String)> = HashSet::new();
        for call in calls {
            // Try to resolve callee_name to an entity
            let target = by_qualified
                .get(call.callee_name.as_str())
                .or_else(|| by_name.get(call.callee_name.as_str()));

            if let Some(target_entity) = target {
                if target_entity.id != call.caller_id {
                    let key = (
                        call.caller_id.as_str().to_string(),
                        target_entity.id.as_str().to_string(),
                    );
                    if seen_calls.insert(key) {
                        relationships.push(Relationship::new(
                            call.caller_id.clone(),
                            target_entity.id.clone(),
                            RelationshipKind::Calls,
                            0.8,
                        ));
                    }
                }
            }
        }

        // Materialise CalledBy as the inverse of every Calls relationship
        let call_inverses: Vec<Relationship> = relationships
            .iter()
            .filter(|r| r.kind == RelationshipKind::Calls)
            .map(|r| {
                Relationship::new(
                    r.target.clone(),
                    r.source.clone(),
                    RelationshipKind::CalledBy,
                    r.confidence,
                )
            })
            .collect();
        relationships.extend(call_inverses);

        relationships
    }

    /// Extract type references from a signature string.
    fn extract_type_references(&self, signature: &str, language: &Language) -> Vec<String> {
        let mut refs = Vec::new();

        match language {
            Language::Rust => {
                // Extract identifiers that look like type names (PascalCase)
                let word_re = regex::Regex::new(r"\b([A-Z][a-zA-Z0-9]+)\b").unwrap();
                for cap in word_re.captures_iter(signature) {
                    let name = cap[1].to_string();
                    // Filter out common Rust keywords and primitives
                    if !matches!(
                        name.as_str(),
                        "Self"
                            | "String"
                            | "Vec"
                            | "Option"
                            | "Result"
                            | "Box"
                            | "Arc"
                            | "Mutex"
                            | "HashMap"
                            | "HashSet"
                            | "Ok"
                            | "Err"
                            | "Some"
                            | "None"
                            | "Send"
                            | "Sync"
                    ) {
                        refs.push(name);
                    }
                }
            }
            Language::TypeScript | Language::JavaScript => {
                let word_re = regex::Regex::new(r"\b([A-Z][a-zA-Z0-9]+)\b").unwrap();
                for cap in word_re.captures_iter(signature) {
                    let name = cap[1].to_string();
                    if !matches!(
                        name.as_str(),
                        "Promise"
                            | "Array"
                            | "Object"
                            | "Map"
                            | "Set"
                            | "Record"
                            | "Partial"
                            | "Required"
                            | "Readonly"
                    ) {
                        refs.push(name);
                    }
                }
            }
            Language::Python => {
                // Python type hints: name: Type, -> Type
                let word_re = regex::Regex::new(r":\s*([A-Z][a-zA-Z0-9]+)").unwrap();
                for cap in word_re.captures_iter(signature) {
                    refs.push(cap[1].to_string());
                }
            }
            _ => {}
        }

        refs
    }

    /// Resolve an import entity to the target file entity.
    fn resolve_import(
        &self,
        entity: &CodeEntity,
        _file_entities: &HashMap<&str, &CodeEntity>,
        by_qualified: &HashMap<&str, &CodeEntity>,
    ) -> Option<EntityId> {
        // Try to resolve by qualified name match
        let name = &entity.qualified_name;
        if let Some(target) = by_qualified.get(name.as_str()) {
            if target.id != entity.id {
                return Some(target.id.clone());
            }
        }

        // Try path-based resolution for module names
        // e.g., "crate::hub::serve" → look for file "hub/serve.rs" or "hub/serve/mod.rs"
        let segments: Vec<&str> = name.split("::").collect();
        if segments.len() >= 2 {
            let path_candidate = segments[1..].join("/") + ".rs";
            if let Some(target) = by_qualified.get(path_candidate.as_str()) {
                return Some(target.id.clone());
            }
        }

        None
    }

    /// Build a name → entity index (first match wins for ambiguous names).
    fn build_name_index<'a>(&self, entities: &'a [CodeEntity]) -> HashMap<&'a str, &'a CodeEntity> {
        let mut index = HashMap::new();
        for entity in entities {
            if entity.kind != EntityKind::File && entity.kind != EntityKind::Import {
                index.entry(entity.name.as_str()).or_insert(entity);
            }
        }
        index
    }

    /// Build a qualified_name → entity index.
    fn build_qualified_name_index<'a>(
        &self,
        entities: &'a [CodeEntity],
    ) -> HashMap<&'a str, &'a CodeEntity> {
        let mut index = HashMap::new();
        for entity in entities {
            index
                .entry(entity.qualified_name.as_str())
                .or_insert(entity);
        }
        index
    }

    /// Infer what entity a test is testing, based on naming conventions.
    fn infer_tested_entity(&self, test: &CodeEntity, entities: &[CodeEntity]) -> Option<EntityId> {
        let test_name = &test.name;

        // Strip common test prefixes
        let target_name = test_name
            .strip_prefix("test_")
            .or_else(|| test_name.strip_prefix("it_"))
            .or_else(|| test_name.strip_prefix("should_"))
            .unwrap_or(test_name);

        // Look for a non-test entity with a matching name
        entities
            .iter()
            .find(|e| {
                e.kind != EntityKind::Test
                    && e.kind != EntityKind::File
                    && (e.name == target_name
                        || e.name.to_lowercase() == target_name.to_lowercase())
            })
            .map(|e| e.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{CodeVisibility, Language};

    fn make_entity(id: &str, name: &str, kind: EntityKind, parent: Option<&str>) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test".to_string(),
            kind,
            name: name.to_string(),
            qualified_name: name.to_string(),
            file_path: "test.rs".to_string(),
            start_line: 1,
            end_line: 10,
            signature: None,
            visibility: CodeVisibility::Public,
            parent_id: parent.map(|p| EntityId(p.to_string())),
            doc_comment: None,
            content_hash: "abc".to_string(),
            language: Language::Rust,
        }
    }

    fn make_entity_in_file(
        id: &str,
        name: &str,
        kind: EntityKind,
        file: &str,
        sig: Option<&str>,
    ) -> CodeEntity {
        CodeEntity {
            id: EntityId(id.to_string()),
            repo_id: "test".to_string(),
            kind,
            name: name.to_string(),
            qualified_name: name.to_string(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 10,
            signature: sig.map(|s| s.to_string()),
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: "abc".to_string(),
            language: Language::Rust,
        }
    }

    #[test]
    fn intra_file_relationships() {
        let file = make_entity("file1", "test.rs", EntityKind::File, None);
        let func = make_entity("fn1", "my_function", EntityKind::Function, None);
        let method = make_entity("m1", "my_method", EntityKind::Method, Some("struct1"));
        let struct1 = make_entity("struct1", "MyStruct", EntityKind::Struct, None);

        let builder = RelationshipBuilder::new("test");
        let rels = builder.build_intra_file_relationships(&[file, func, struct1, method]);

        let defined_in = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::DefinedIn)
            .count();
        assert_eq!(defined_in, 3);

        let contains = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::Contains)
            .count();
        assert_eq!(contains, 1);
    }

    #[test]
    fn test_inference() {
        let func = make_entity("fn1", "parse_input", EntityKind::Function, None);
        let test = make_entity("t1", "test_parse_input", EntityKind::Test, None);
        let file = make_entity("file1", "lib.rs", EntityKind::File, None);

        let builder = RelationshipBuilder::new("test");
        let rels = builder.build_intra_file_relationships(&[file, func, test]);

        let tests_for = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::TestsFor)
            .count();
        assert_eq!(tests_for, 1);
    }

    #[test]
    fn cross_file_type_references() {
        let file_a = make_entity_in_file("fa", "a.rs", EntityKind::File, "a.rs", None);
        let file_b = make_entity_in_file("fb", "b.rs", EntityKind::File, "b.rs", None);
        let struct_a = make_entity_in_file(
            "sa",
            "MyConfig",
            EntityKind::Struct,
            "a.rs",
            Some("pub struct MyConfig"),
        );
        let func_b = make_entity_in_file(
            "fnb",
            "load_config",
            EntityKind::Function,
            "b.rs",
            Some("pub fn load_config() -> MyConfig"),
        );

        let builder = RelationshipBuilder::new("test");
        let rels =
            builder.build_cross_file_relationships(&[file_a, file_b, struct_a, func_b], &[], &[]);

        let type_ofs = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::TypeOf)
            .count();
        assert!(
            type_ofs >= 1,
            "Should detect TypeOf relationship from func_b to struct_a"
        );
    }

    #[test]
    fn implements_detection_from_impl_traits() {
        let trait_e = make_entity_in_file(
            "tr",
            "MemoryStore",
            EntityKind::Trait,
            "store.rs",
            Some("pub trait MemoryStore"),
        );
        let struct_e = make_entity_in_file(
            "st",
            "SqliteStore",
            EntityKind::Struct,
            "sqlite.rs",
            Some("pub struct SqliteStore"),
        );
        let file_a = make_entity_in_file("fa", "store.rs", EntityKind::File, "store.rs", None);
        let file_b = make_entity_in_file("fb", "sqlite.rs", EntityKind::File, "sqlite.rs", None);

        let impl_traits = vec![ImplTraitInfo {
            trait_name: "MemoryStore".to_string(),
            type_name: "SqliteStore".to_string(),
        }];

        let builder = RelationshipBuilder::new("test");
        let rels = builder.build_cross_file_relationships(
            &[file_a, file_b, trait_e, struct_e],
            &impl_traits,
            &[],
        );

        let implements = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::Implements)
            .count();
        assert!(implements >= 1, "Should detect Implements relationship");
    }

    #[test]
    fn calls_detection_from_call_references() {
        let file_a = make_entity_in_file("fa", "a.rs", EntityKind::File, "a.rs", None);
        let file_b = make_entity_in_file("fb", "b.rs", EntityKind::File, "b.rs", None);
        let caller = make_entity_in_file(
            "fn1",
            "process_data",
            EntityKind::Function,
            "a.rs",
            Some("pub fn process_data()"),
        );
        let callee = make_entity_in_file(
            "fn2",
            "validate",
            EntityKind::Function,
            "b.rs",
            Some("pub fn validate()"),
        );

        let calls = vec![CallReference {
            caller_id: EntityId("fn1".to_string()),
            callee_name: "validate".to_string(),
        }];

        let builder = RelationshipBuilder::new("test");
        let rels =
            builder.build_cross_file_relationships(&[file_a, file_b, caller, callee], &[], &calls);

        let calls_count = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::Calls)
            .count();
        assert_eq!(calls_count, 1, "Should detect Calls relationship");

        let called_by_count = rels
            .iter()
            .filter(|r| r.kind == RelationshipKind::CalledBy)
            .count();
        assert_eq!(
            called_by_count, 1,
            "Should materialise CalledBy relationship"
        );
    }
}
