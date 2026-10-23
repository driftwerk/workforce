use crate::graph::entities::{CodeEntity, CodeVisibility, EntityId, EntityKind, Language};
use crate::indexer::crawler::CrawledFile;
use crate::indexer::parser::ParsedTree;
use std::collections::HashSet;
use std::sync::Mutex;

/// Stack size for the dedicated AST-extraction thread. The recursive walkers
/// descend one frame per AST level; 64 MB comfortably covers the deepest
/// real-world expression nesting (the workforce repo's deepest file needs
/// well over the default ~2 MB). See `extract_from_tree`.
const EXTRACTION_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Complete result of extracting symbols from a parsed file.
pub struct ExtractionResult {
    pub entities: Vec<CodeEntity>,
    /// Trait implementations: (trait_name, type_name) pairs from `impl Trait for Type` blocks.
    pub impl_traits: Vec<ImplTraitInfo>,
    /// Function/method call references found in function bodies.
    pub calls: Vec<CallReference>,
}

/// A trait implementation detected during symbol extraction.
pub struct ImplTraitInfo {
    pub trait_name: String,
    pub type_name: String,
}

/// A call reference from one entity to another (by name, not yet resolved).
pub struct CallReference {
    pub caller_id: EntityId,
    pub callee_name: String,
}

/// Extracts code entities from parsed syntax trees.
pub struct SymbolExtractor {
    repo_id: String,
    /// The ids more than one entity of the file being read would have,
    /// were each left as its language's walker makes it, from its kind
    /// and short name alone: found by reading the file once, and empty
    /// while that is done.
    shared: HashSet<EntityId>,
    /// The id each entity of the file being read was found with, as its
    /// walker made it, in the order they were found.
    found: Mutex<Vec<EntityId>>,
}

impl SymbolExtractor {
    pub fn new(repo_id: &str) -> Self {
        Self {
            repo_id: repo_id.to_string(),
            shared: HashSet::new(),
            found: Mutex::new(Vec::new()),
        }
    }

    /// Create a File-level entity for any file.
    pub fn create_file_entity(&self, file: &CrawledFile, _content: &str) -> CodeEntity {
        let name = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "file", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::File,
            name: name.clone(),
            qualified_name: file.relative_path.clone(),
            file_path: file.relative_path.clone(),
            start_line: 1,
            end_line: 0, // Will be set later
            signature: None,
            visibility: CodeVisibility::Public,
            parent_id: None,
            doc_comment: None,
            content_hash: file.content_hash.clone(),
            language: file.language.clone(),
        }
    }

    /// Extract entities from a parsed tree-sitter AST.
    ///
    /// The per-language walkers (`walk_*_node` / `extract_*_calls`) are
    /// recursive descents whose depth tracks AST nesting. Deeply nested
    /// expressions in real code — long method/builder chains, nested
    /// closures, big nested literals — produce ASTs deep enough to overflow
    /// the default ~2 MB thread stack (the CI `e2e_real_codebase` job aborts
    /// with SIGABRT on the real workforce repo). Run the walk on a dedicated
    /// large-stack thread so extraction depth is bounded by the AST, not by
    /// whichever runtime thread happens to call us. Scoped so the borrows of
    /// `self`/`parsed`/`source`/`file` need no `'static`.
    pub fn extract_from_tree(
        &self,
        parsed: &ParsedTree,
        source: &str,
        file: &CrawledFile,
    ) -> ExtractionResult {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("cke-extract".to_string())
                .stack_size(EXTRACTION_STACK_SIZE)
                .spawn_scoped(scope, || self.extract_from_tree_inner(parsed, source, file))
                .expect("failed to spawn extraction thread")
                .join()
                .expect("extraction thread panicked")
        })
    }

    /// The file read twice where two of its entities share a kind and a
    /// name: once to learn which do, and again knowing it, so that each of
    /// them has, from the moment it is found, an id that does not depend
    /// on which of them came first. See [`Self::told_apart`].
    fn extract_from_tree_inner(
        &self,
        parsed: &ParsedTree,
        source: &str,
        file: &CrawledFile,
    ) -> ExtractionResult {
        // Each reading by an extractor of its own: this one is shared by
        // the files being read at once.
        let reading = Self::new(&self.repo_id);
        let read = reading.read_tree(parsed, source, file);
        let found = reading
            .found
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        let mut seen = HashSet::new();
        let shared: HashSet<EntityId> = found
            .into_iter()
            .filter(|plain| !seen.insert(plain.clone()))
            .collect();
        if shared.is_empty() {
            return read;
        }
        let knowing = Self {
            shared,
            ..Self::new(&self.repo_id)
        };
        knowing.read_tree(parsed, source, file)
    }

    fn read_tree(&self, parsed: &ParsedTree, source: &str, file: &CrawledFile) -> ExtractionResult {
        let mut entities = Vec::new();
        let mut impl_traits = Vec::new();
        let mut calls = Vec::new();

        // Always create a File entity
        let line_count = source.lines().count();
        let mut file_entity = self.create_file_entity(file, source);
        file_entity.end_line = line_count;
        entities.push(file_entity);

        // Walk the AST and extract language-specific entities
        let root = parsed.tree.root_node();
        match parsed.language {
            Language::Rust => {
                self.extract_rust(
                    &root,
                    source,
                    file,
                    &mut entities,
                    &mut impl_traits,
                    &mut calls,
                );
            }
            Language::TypeScript | Language::JavaScript => {
                self.extract_typescript(&root, source, file, &mut entities, &mut calls);
            }
            Language::Python => {
                self.extract_python(&root, source, file, &mut entities, &mut calls);
            }
            Language::Go => self.extract_go(&root, source, file, &mut entities, &mut calls),
            Language::Ruby => {
                self.extract_ruby(&root, source, file, &mut entities, &mut calls);
            }
            Language::Kotlin => {
                self.extract_kotlin(&root, source, file, &mut entities, &mut calls);
            }
            _ => {}
        }

        // NOTE: only the Rust (`#[test]`) and Python (`def test_*`) walkers set
        // `EntityKind::Test`, so on a Kotlin, Ruby, TS or Go repo nothing is a
        // test as far as the GRAPH is concerned — no `TestsFor` edges, and every
        // consumer asking "is this a test?" answers no. Stamping it here from
        // `is_test_path` is the durable fix, but `EntityId::from_components`
        // hashes the kind, so it has to be done where the id is built (per
        // language) rather than by mutating afterwards, and it needs a full
        // re-index to take effect. Consumers that must not wait for that use the
        // path predicate directly (see `hub::code_context::is_test_entity`).
        ExtractionResult {
            entities,
            impl_traits,
            calls,
        }
    }

    /// `entity`, found at `node`, with an id no other entity of its file
    /// has.
    ///
    /// An id is made from the file, the kind and the short name, which two
    /// methods of one name in two impl blocks share (`impl A { fn new }`
    /// and `impl B { fn new }`), as do two functions of one name behind
    /// different `cfg`s. The graph keeps one row an id, so the second
    /// replaced the first and was the only one known.
    ///
    /// An entity whose kind and name no other of the file shares keeps
    /// that id. Each of those that share one ([`Self::shared`]) is given
    /// an id made from where it stands as well ([`Self::scope_of`]: the
    /// impl blocks, modules, classes and functions around it, by name), so
    /// that the id of one does not depend on the others: they can be
    /// reordered, and others of the name added or taken away while two
    /// remain, without any id changing. An id changes only when a name
    /// becomes shared, or stops being: once, for the entities of that name.
    ///
    /// Those that share even where they stand (two `fn f` behind different
    /// `cfg`s, two `impl A` blocks each with a `fn new`) are numbered in
    /// the order they are written. Nothing in their names tells them
    /// apart, so the order is all there is: reordering those swaps their
    /// ids, and removing the first moves the rest up.
    ///
    /// Done as each entity is found, before its id is used for its
    /// children and its calls, so that they are its own.
    fn told_apart(
        &self,
        entities: &[CodeEntity],
        mut entity: CodeEntity,
        node: &tree_sitter::Node,
        source: &str,
    ) -> CodeEntity {
        let taken = |id: &EntityId| entities.iter().any(|known| known.id == *id);
        self.found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entity.id.clone());
        // On the first reading nothing is known to be shared, and a later
        // entity of a name is told apart all the same: every id is its
        // own on either reading, so a parent is never mistaken.
        if !self.shared.contains(&entity.id) && !taken(&entity.id) {
            return entity;
        }
        let scope = self.scope_of(node, source);
        for nth in 1.. {
            // No name holds a control character, so none of these is the
            // short name of another entity.
            let apart = match nth {
                1 => format!("{}\u{1}{scope}", entity.name),
                nth => format!("{}\u{1}{scope}\u{1}{nth}", entity.name),
            };
            let id = EntityId::from_components(
                &self.repo_id,
                &entity.file_path,
                entity.kind.as_str(),
                &apart,
            );
            if !taken(&id) {
                entity.id = id;
                break;
            }
        }
        entity
    }

    /// Where `node` stands in its file: the names of what encloses it,
    /// outermost first. An impl block is named as it is written (`A`,
    /// `From<u8> for A`), whether or not its type is defined in the file;
    /// anything else by its name, if it has one.
    fn scope_of(&self, node: &tree_sitter::Node, source: &str) -> String {
        let mut around = Vec::new();
        let mut above = node.parent();
        while let Some(outer) = above {
            let named = match outer.kind() {
                "impl_item" => self.get_rust_impl_name(&outer, source),
                _ => outer
                    .child_by_field_name("name")
                    .map(|name| self.node_text(&name, source)),
            };
            around.extend(named);
            above = outer.parent();
        }
        around.reverse();
        around.join("::")
    }

    // ========================================================================
    // Rust extraction
    // ========================================================================

    fn extract_rust(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        impl_traits: &mut Vec<ImplTraitInfo>,
        calls: &mut Vec<CallReference>,
    ) {
        self.walk_rust_node(node, source, file, entities, None, impl_traits, calls);
    }

    fn walk_rust_node(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        parent_id: Option<&EntityId>,
        impl_traits: &mut Vec<ImplTraitInfo>,
        calls: &mut Vec<CallReference>,
    ) {
        let kind = node.kind();

        match kind {
            "function_item" => {
                if let Some(mut entity) = self.extract_rust_function(node, source, file, parent_id)
                {
                    // Fix qualified name: use parent entity's name instead of hash ID
                    if let Some(pid) = parent_id {
                        if let Some(parent) = entities.iter().find(|e| e.id == *pid) {
                            entity.qualified_name = format!("{}::{}", parent.name, entity.name);
                        }
                    }
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    // Extract calls from function body
                    if let Some(body) = node.child_by_field_name("body") {
                        self.extract_rust_calls(&body, source, &entity_id, calls);
                    }
                    // Walk children for nested items
                    self.walk_rust_children(
                        node,
                        source,
                        file,
                        entities,
                        Some(&entity_id),
                        impl_traits,
                        calls,
                    );
                }
            }
            "struct_item" => {
                if let Some(entity) = self.extract_rust_struct(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            "enum_item" => {
                if let Some(entity) = self.extract_rust_enum(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            "trait_item" => {
                if let Some(entity) = self.extract_rust_trait(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    self.walk_rust_children(
                        node,
                        source,
                        file,
                        entities,
                        Some(&entity_id),
                        impl_traits,
                        calls,
                    );
                }
            }
            "impl_item" => {
                // Extract methods from impl blocks.
                // Find the matching struct/trait/enum entity already extracted in this file
                // so that methods get a valid parent_id (avoiding FK constraint violations).
                let impl_name = self.get_rust_impl_name(node, source);
                let parent_entity_id = impl_name.as_ref().and_then(|name| {
                    // For trait impls like "Default for AppConfig", extract the type name
                    let type_name = name.split(" for ").last().unwrap_or(name).trim();
                    entities
                        .iter()
                        .find(|e| {
                            e.name == type_name
                                && matches!(
                                    e.kind,
                                    EntityKind::Struct
                                        | EntityKind::Trait
                                        | EntityKind::Enum
                                        | EntityKind::Class
                                )
                        })
                        .map(|e| e.id.clone())
                });

                // Record trait implementations for Implements relationship detection
                if let Some(ref name) = impl_name {
                    if let Some(for_idx) = name.find(" for ") {
                        let trait_name = name[..for_idx].trim().to_string();
                        let type_name = name[for_idx + 5..].trim().to_string();
                        impl_traits.push(ImplTraitInfo {
                            trait_name,
                            type_name,
                        });
                    }
                }

                self.walk_rust_children(
                    node,
                    source,
                    file,
                    entities,
                    parent_entity_id.as_ref(),
                    impl_traits,
                    calls,
                );
            }
            "mod_item" => {
                if let Some(entity) = self.extract_rust_module(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    self.walk_rust_children(
                        node,
                        source,
                        file,
                        entities,
                        Some(&entity_id),
                        impl_traits,
                        calls,
                    );
                }
            }
            "type_item" => {
                if let Some(entity) = self.extract_rust_type_alias(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            "const_item" | "static_item" => {
                if let Some(entity) = self.extract_rust_constant(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            "macro_definition" => {
                if let Some(entity) = self.extract_rust_macro(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            _ => {
                self.walk_rust_children(
                    node,
                    source,
                    file,
                    entities,
                    parent_id,
                    impl_traits,
                    calls,
                );
            }
        }
    }

    fn walk_rust_children(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        parent_id: Option<&EntityId>,
        impl_traits: &mut Vec<ImplTraitInfo>,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_rust_node(
                &child,
                source,
                file,
                entities,
                parent_id,
                impl_traits,
                calls,
            );
        }
    }

    /// Walk a Rust function body and extract call references.
    fn extract_rust_calls(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        caller_id: &EntityId,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "call_expression" {
                if let Some(func) = child.child_by_field_name("function") {
                    let callee = match func.kind() {
                        "identifier" => Some(self.node_text(&func, source)),
                        "scoped_identifier" => {
                            // e.g., MyStruct::new() → "MyStruct::new"
                            Some(self.node_text(&func, source))
                        }
                        "field_expression" => {
                            // e.g., x.method() → extract method name
                            func.child_by_field_name("field")
                                .map(|f| self.node_text(&f, source))
                        }
                        _ => None,
                    };
                    if let Some(name) = callee {
                        calls.push(CallReference {
                            caller_id: caller_id.clone(),
                            callee_name: name,
                        });
                    }
                }
            }
            // Also detect macro invocations (e.g., vec![], println![], info![])
            if child.kind() == "macro_invocation" {
                if let Some(macro_node) = child.child_by_field_name("macro") {
                    let name = self.node_text(&macro_node, source);
                    calls.push(CallReference {
                        caller_id: caller_id.clone(),
                        callee_name: name,
                    });
                }
            }
            // Recurse into children (but not into nested function_items or closures)
            if child.kind() != "function_item" && child.kind() != "closure_expression" {
                self.extract_rust_calls(&child, source, caller_id, calls);
            }
        }
    }

    fn extract_rust_function(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);
        let signature = self.extract_signature(node, source);
        let doc = self.extract_preceding_doc_comment(node, source);

        // Detect test functions
        let is_test = self.has_rust_attribute(node, source, "test")
            || self.has_rust_attribute(node, source, "tokio::test");

        let kind = if is_test {
            EntityKind::Test
        } else if parent_id.is_some() {
            EntityKind::Method
        } else {
            EntityKind::Function
        };

        let qualified = if let Some(pid) = parent_id {
            format!("{}::{}", pid.as_str(), name)
        } else {
            name.clone()
        };

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, kind.as_str(), &name),
            repo_id: self.repo_id.clone(),
            kind,
            name,
            qualified_name: qualified,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(signature),
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: doc,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_struct(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);
        let doc = self.extract_preceding_doc_comment(node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "struct", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Struct,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: doc,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_enum(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);
        let doc = self.extract_preceding_doc_comment(node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "enum", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Enum,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: doc,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_trait(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);
        let doc = self.extract_preceding_doc_comment(node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "trait", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Trait,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: doc,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_module(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "module", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Module,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: None,
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: None,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_type_alias(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "type_alias", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::TypeAlias,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: None,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_constant(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let visibility = self.rust_visibility(node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "constant", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Constant,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility,
            parent_id: parent_id.cloned(),
            doc_comment: None,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    fn extract_rust_macro(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "macro", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Macro,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: None,
            visibility: CodeVisibility::Public,
            parent_id: parent_id.cloned(),
            doc_comment: None,
            content_hash: self.hash_node(node, source),
            language: Language::Rust,
        })
    }

    // ========================================================================
    // TypeScript/JavaScript extraction
    // ========================================================================

    fn extract_typescript(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        calls: &mut Vec<CallReference>,
    ) {
        self.walk_ts_node(node, source, file, entities, None, calls);
    }

    fn walk_ts_node(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        parent_id: Option<&EntityId>,
        calls: &mut Vec<CallReference>,
    ) {
        let kind = node.kind();

        match kind {
            "function_declaration" | "method_definition" | "arrow_function" => {
                if let Some(entity) = self.extract_ts_function(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    // Extract calls from function body
                    if let Some(body) = node.child_by_field_name("body") {
                        self.extract_ts_calls(&body, source, &entity_id, calls);
                    }
                }
            }
            "class_declaration" => {
                if let Some(entity) = self.extract_ts_class(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    let eid = entity.id.clone();
                    entities.push(entity);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk_ts_node(&child, source, file, entities, Some(&eid), calls);
                    }
                    return;
                }
            }
            "interface_declaration" => {
                if let Some(entity) = self.extract_ts_interface(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            "type_alias_declaration" => {
                if let Some(entity) = self.extract_ts_type_alias(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            "enum_declaration" => {
                if let Some(entity) = self.extract_ts_enum(node, source, file, parent_id) {
                    let entity = self.told_apart(entities, entity, node, source);
                    entities.push(entity);
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_ts_node(&child, source, file, entities, parent_id, calls);
        }
    }

    /// Walk a TS/JS function body and extract call references.
    fn extract_ts_calls(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        caller_id: &EntityId,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "call_expression" {
                if let Some(func) = child.child_by_field_name("function") {
                    let callee = match func.kind() {
                        "identifier" => Some(self.node_text(&func, source)),
                        "member_expression" => func
                            .child_by_field_name("property")
                            .map(|p| self.node_text(&p, source)),
                        _ => None,
                    };
                    if let Some(name) = callee {
                        calls.push(CallReference {
                            caller_id: caller_id.clone(),
                            callee_name: name,
                        });
                    }
                }
            }
            // Recurse but not into nested function declarations
            if child.kind() != "function_declaration"
                && child.kind() != "arrow_function"
                && child.kind() != "method_definition"
            {
                self.extract_ts_calls(&child, source, caller_id, calls);
            }
        }
    }

    fn extract_ts_function(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);
        let kind = if parent_id.is_some() {
            EntityKind::Method
        } else {
            EntityKind::Function
        };

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, kind.as_str(), &name),
            repo_id: self.repo_id.clone(),
            kind,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility: CodeVisibility::Public,
            parent_id: parent_id.cloned(),
            doc_comment: self.extract_preceding_doc_comment(node, source),
            content_hash: self.hash_node(node, source),
            language: file.language.clone(),
        })
    }

    fn extract_ts_class(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "class", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Class,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility: CodeVisibility::Public,
            parent_id: parent_id.cloned(),
            doc_comment: self.extract_preceding_doc_comment(node, source),
            content_hash: self.hash_node(node, source),
            language: file.language.clone(),
        })
    }

    fn extract_ts_interface(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "interface", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Interface,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility: CodeVisibility::Public,
            parent_id: parent_id.cloned(),
            doc_comment: self.extract_preceding_doc_comment(node, source),
            content_hash: self.hash_node(node, source),
            language: file.language.clone(),
        })
    }

    fn extract_ts_type_alias(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "type_alias", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::TypeAlias,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility: CodeVisibility::Public,
            parent_id: parent_id.cloned(),
            doc_comment: None,
            content_hash: self.hash_node(node, source),
            language: file.language.clone(),
        })
    }

    fn extract_ts_enum(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        parent_id: Option<&EntityId>,
    ) -> Option<CodeEntity> {
        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node, source);

        Some(CodeEntity {
            id: EntityId::from_components(&self.repo_id, &file.relative_path, "enum", &name),
            repo_id: self.repo_id.clone(),
            kind: EntityKind::Enum,
            name: name.clone(),
            qualified_name: name,
            file_path: file.relative_path.clone(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            signature: Some(self.extract_signature(node, source)),
            visibility: CodeVisibility::Public,
            parent_id: parent_id.cloned(),
            doc_comment: None,
            content_hash: self.hash_node(node, source),
            language: file.language.clone(),
        })
    }

    // ========================================================================
    // Python extraction
    // ========================================================================

    fn extract_python(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        calls: &mut Vec<CallReference>,
    ) {
        self.walk_python_node(node, source, file, entities, None, calls);
    }

    fn walk_python_node(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        parent_id: Option<&EntityId>,
        calls: &mut Vec<CallReference>,
    ) {
        let kind = node.kind();

        match kind {
            "function_definition" => {
                let name_node = node.child_by_field_name("name");
                if let Some(name_node) = name_node {
                    let name = self.node_text(&name_node, source);
                    let entity_kind = if parent_id.is_some() {
                        EntityKind::Method
                    } else if name.starts_with("test_") || name.starts_with("test") {
                        EntityKind::Test
                    } else {
                        EntityKind::Function
                    };

                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            entity_kind.as_str(),
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: entity_kind,
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: Some(self.extract_signature(node, source)),
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: self.extract_python_docstring(node, source),
                        content_hash: self.hash_node(node, source),
                        language: Language::Python,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    // Extract calls from function body
                    if let Some(body) = node.child_by_field_name("body") {
                        self.extract_python_calls(&body, source, &entity_id, calls);
                    }
                }
            }
            "class_definition" => {
                let name_node = node.child_by_field_name("name");
                if let Some(name_node) = name_node {
                    let name = self.node_text(&name_node, source);
                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            "class",
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: EntityKind::Class,
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: Some(self.extract_signature(node, source)),
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: self.extract_python_docstring(node, source),
                        content_hash: self.hash_node(node, source),
                        language: Language::Python,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let eid = entity.id.clone();
                    entities.push(entity);
                    // Walk children with this as parent
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk_python_node(&child, source, file, entities, Some(&eid), calls);
                    }
                    return;
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_python_node(&child, source, file, entities, parent_id, calls);
        }
    }

    /// Walk a Python function body and extract call references.
    fn extract_python_calls(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        caller_id: &EntityId,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "call" {
                if let Some(func) = child.child_by_field_name("function") {
                    let callee = match func.kind() {
                        "identifier" => Some(self.node_text(&func, source)),
                        "attribute" => func
                            .child_by_field_name("attribute")
                            .map(|a| self.node_text(&a, source)),
                        _ => None,
                    };
                    if let Some(name) = callee {
                        calls.push(CallReference {
                            caller_id: caller_id.clone(),
                            callee_name: name,
                        });
                    }
                }
            }
            // Recurse but not into nested function definitions
            if child.kind() != "function_definition" {
                self.extract_python_calls(&child, source, caller_id, calls);
            }
        }
    }

    // ========================================================================
    // Go extraction
    // ========================================================================

    fn extract_go(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "function_declaration" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = self.node_text(&name_node, source);
                        let visibility = if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                            CodeVisibility::Public
                        } else {
                            CodeVisibility::Private
                        };
                        let entity = CodeEntity {
                            id: EntityId::from_components(
                                &self.repo_id,
                                &file.relative_path,
                                "function",
                                &name,
                            ),
                            repo_id: self.repo_id.clone(),
                            kind: EntityKind::Function,
                            name: name.clone(),
                            qualified_name: name,
                            file_path: file.relative_path.clone(),
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: Some(self.extract_signature(&child, source)),
                            visibility,
                            parent_id: None,
                            doc_comment: self.extract_preceding_doc_comment(&child, source),
                            content_hash: self.hash_node(&child, source),
                            language: Language::Go,
                        };
                        let entity = self.told_apart(entities, entity, &child, source);
                        let entity_id = entity.id.clone();
                        entities.push(entity);
                        // Extract calls from function body
                        if let Some(body) = child.child_by_field_name("body") {
                            self.extract_go_calls(&body, source, &entity_id, calls);
                        }
                    }
                }
                "type_declaration" => {
                    // Handle struct, interface type declarations
                    let mut inner_cursor = child.walk();
                    for spec in child.children(&mut inner_cursor) {
                        if spec.kind() == "type_spec" {
                            if let Some(name_node) = spec.child_by_field_name("name") {
                                let name = self.node_text(&name_node, source);
                                let entity = CodeEntity {
                                    id: EntityId::from_components(
                                        &self.repo_id,
                                        &file.relative_path,
                                        "struct",
                                        &name,
                                    ),
                                    repo_id: self.repo_id.clone(),
                                    kind: EntityKind::Struct,
                                    name: name.clone(),
                                    qualified_name: name,
                                    file_path: file.relative_path.clone(),
                                    start_line: spec.start_position().row + 1,
                                    end_line: spec.end_position().row + 1,
                                    signature: Some(self.extract_signature(&spec, source)),
                                    visibility: CodeVisibility::Public,
                                    parent_id: None,
                                    doc_comment: None,
                                    content_hash: self.hash_node(&spec, source),
                                    language: Language::Go,
                                };
                                let entity = self.told_apart(entities, entity, &spec, source);
                                entities.push(entity);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Walk a Go function body and extract call references.
    fn extract_go_calls(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        caller_id: &EntityId,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "call_expression" {
                if let Some(func) = child.child_by_field_name("function") {
                    let callee = match func.kind() {
                        "identifier" => Some(self.node_text(&func, source)),
                        "selector_expression" => func
                            .child_by_field_name("field")
                            .map(|f| self.node_text(&f, source)),
                        _ => None,
                    };
                    if let Some(name) = callee {
                        calls.push(CallReference {
                            caller_id: caller_id.clone(),
                            callee_name: name,
                        });
                    }
                }
            }
            // Recurse but not into nested function literals
            if child.kind() != "func_literal" {
                self.extract_go_calls(&child, source, caller_id, calls);
            }
        }
    }

    // ========================================================================
    // Ruby extraction
    // ========================================================================

    fn extract_ruby(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        calls: &mut Vec<CallReference>,
    ) {
        self.walk_ruby_node(node, source, file, entities, None, calls);
    }

    fn walk_ruby_node(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        parent_id: Option<&EntityId>,
        calls: &mut Vec<CallReference>,
    ) {
        match node.kind() {
            "method" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node, source);
                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            "method",
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: if parent_id.is_some() {
                            EntityKind::Method
                        } else {
                            EntityKind::Function
                        },
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: Some(self.extract_signature(node, source)),
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: None,
                        content_hash: self.hash_node(node, source),
                        language: Language::Ruby,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    // Extract calls from method body
                    self.extract_ruby_calls(node, source, &entity_id, calls);
                }
            }
            "class" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node, source);
                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            "class",
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: EntityKind::Class,
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: Some(self.extract_signature(node, source)),
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: None,
                        content_hash: self.hash_node(node, source),
                        language: Language::Ruby,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let eid = entity.id.clone();
                    entities.push(entity);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk_ruby_node(&child, source, file, entities, Some(&eid), calls);
                    }
                    return;
                }
            }
            "module" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node, source);
                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            "module",
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: EntityKind::Module,
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: None,
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: None,
                        content_hash: self.hash_node(node, source),
                        language: Language::Ruby,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let eid = entity.id.clone();
                    entities.push(entity);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk_ruby_node(&child, source, file, entities, Some(&eid), calls);
                    }
                    return;
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_ruby_node(&child, source, file, entities, parent_id, calls);
        }
    }

    /// Walk a Ruby method body and extract call references.
    fn extract_ruby_calls(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        caller_id: &EntityId,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "call" | "method_call" => {
                    // Try to get method name from the node
                    if let Some(method_node) = child.child_by_field_name("method") {
                        let name = self.node_text(&method_node, source);
                        calls.push(CallReference {
                            caller_id: caller_id.clone(),
                            callee_name: name,
                        });
                    }
                }
                _ => {}
            }
            // Recurse but not into nested method definitions
            if child.kind() != "method" {
                self.extract_ruby_calls(&child, source, caller_id, calls);
            }
        }
    }

    // ========================================================================
    // Kotlin extraction
    // ========================================================================

    fn extract_kotlin(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        calls: &mut Vec<CallReference>,
    ) {
        self.walk_kotlin_node(node, source, file, entities, None, calls);
    }

    fn walk_kotlin_node(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        file: &CrawledFile,
        entities: &mut Vec<CodeEntity>,
        parent_id: Option<&EntityId>,
        calls: &mut Vec<CallReference>,
    ) {
        match node.kind() {
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node, source);
                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            "function",
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: if parent_id.is_some() {
                            EntityKind::Method
                        } else {
                            EntityKind::Function
                        },
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: Some(self.extract_signature(node, source)),
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: None,
                        content_hash: self.hash_node(node, source),
                        language: Language::Kotlin,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let entity_id = entity.id.clone();
                    entities.push(entity);
                    self.extract_kotlin_calls(node, source, &entity_id, calls);
                }
            }
            "class_declaration" | "object_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node, source);
                    let kind_tag = if node.kind() == "class_declaration" {
                        "class"
                    } else {
                        "object"
                    };
                    let entity = CodeEntity {
                        id: EntityId::from_components(
                            &self.repo_id,
                            &file.relative_path,
                            kind_tag,
                            &name,
                        ),
                        repo_id: self.repo_id.clone(),
                        kind: EntityKind::Class,
                        name: name.clone(),
                        qualified_name: name,
                        file_path: file.relative_path.clone(),
                        start_line: node.start_position().row + 1,
                        end_line: node.end_position().row + 1,
                        signature: Some(self.extract_signature(node, source)),
                        visibility: CodeVisibility::Public,
                        parent_id: parent_id.cloned(),
                        doc_comment: None,
                        content_hash: self.hash_node(node, source),
                        language: Language::Kotlin,
                    };
                    let entity = self.told_apart(entities, entity, node, source);
                    let eid = entity.id.clone();
                    entities.push(entity);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        self.walk_kotlin_node(&child, source, file, entities, Some(&eid), calls);
                    }
                    return;
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_kotlin_node(&child, source, file, entities, parent_id, calls);
        }
    }

    fn extract_kotlin_calls(
        &self,
        node: &tree_sitter::Node,
        source: &str,
        caller_id: &EntityId,
        calls: &mut Vec<CallReference>,
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "call_expression" {
                if let Some(callee_name) = self.kotlin_callee_name(&child, source) {
                    calls.push(CallReference {
                        caller_id: caller_id.clone(),
                        callee_name,
                    });
                }
            }
            if child.kind() != "function_declaration" {
                self.extract_kotlin_calls(&child, source, caller_id, calls);
            }
        }
    }

    // call_expression has no `name` field; the callee is the first child that
    // is not an argument list.
    fn kotlin_callee_name(&self, call_node: &tree_sitter::Node, source: &str) -> Option<String> {
        let mut cursor = call_node.walk();
        let callee = call_node.children(&mut cursor).find(|child| {
            !matches!(
                child.kind(),
                "value_arguments" | "type_arguments" | "annotated_lambda"
            )
        })?;
        Some(self.node_text(&callee, source))
    }

    // ========================================================================
    // Utility methods
    // ========================================================================

    fn node_text(&self, node: &tree_sitter::Node, source: &str) -> String {
        let start = node.start_byte();
        let end = node.end_byte();
        source[start..end].to_string()
    }

    fn hash_node(&self, node: &tree_sitter::Node, source: &str) -> String {
        let text = self.node_text(node, source);
        blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
    }

    /// Extract the first line of a node as its signature (up to opening brace).
    fn extract_signature(&self, node: &tree_sitter::Node, source: &str) -> String {
        let text = self.node_text(node, source);
        // Take up to the first { or the first line, whichever is shorter
        let sig = text
            .split('{')
            .next()
            .unwrap_or(&text)
            .lines()
            .next()
            .unwrap_or(&text)
            .trim();

        // Limit signature length
        if sig.len() > 200 {
            format!("{}...", &sig[..sig.floor_char_boundary(200)])
        } else {
            sig.to_string()
        }
    }

    /// Check if a Rust node has a specific attribute (e.g., #[test]).
    fn has_rust_attribute(&self, node: &tree_sitter::Node, source: &str, attr_name: &str) -> bool {
        // Look at preceding siblings for attribute_item nodes
        let mut sibling = node.prev_sibling();
        while let Some(sib) = sibling {
            if sib.kind() == "attribute_item" {
                let text = self.node_text(&sib, source);
                if text.contains(attr_name) {
                    return true;
                }
            } else if sib.kind() != "line_comment" && sib.kind() != "block_comment" {
                break;
            }
            sibling = sib.prev_sibling();
        }
        false
    }

    /// Determine visibility from Rust visibility modifier.
    fn rust_visibility(&self, node: &tree_sitter::Node, source: &str) -> CodeVisibility {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "visibility_modifier" {
                let text = self.node_text(&child, source);
                if text.contains("pub(crate)") {
                    return CodeVisibility::Internal;
                } else if text.contains("pub") {
                    return CodeVisibility::Public;
                }
            }
        }
        CodeVisibility::Private
    }

    /// Extract the name of a Rust impl block (e.g., "Type" or "Trait for Type").
    fn get_rust_impl_name(&self, node: &tree_sitter::Node, source: &str) -> Option<String> {
        let type_node = node.child_by_field_name("type")?;
        let type_name = self.node_text(&type_node, source);

        // Check if there's a trait
        if let Some(trait_node) = node.child_by_field_name("trait") {
            let trait_name = self.node_text(&trait_node, source);
            Some(format!("{} for {}", trait_name, type_name))
        } else {
            Some(type_name)
        }
    }

    /// Extract doc comments preceding a node (/// or /** */).
    fn extract_preceding_doc_comment(
        &self,
        node: &tree_sitter::Node,
        source: &str,
    ) -> Option<String> {
        let mut comments = Vec::new();
        let mut sibling = node.prev_sibling();
        while let Some(sib) = sibling {
            match sib.kind() {
                "line_comment" => {
                    let text = self.node_text(&sib, source);
                    if text.starts_with("///") || text.starts_with("//!") {
                        comments.push(
                            text.trim_start_matches("///")
                                .trim_start_matches("//!")
                                .trim()
                                .to_string(),
                        );
                    } else {
                        break;
                    }
                }
                "block_comment" => {
                    let text = self.node_text(&sib, source);
                    if text.starts_with("/**") {
                        comments.push(
                            text.trim_start_matches("/**")
                                .trim_end_matches("*/")
                                .trim()
                                .to_string(),
                        );
                    }
                    break;
                }
                "attribute_item" => {
                    // Skip attributes, continue looking for doc comments
                }
                "comment" => {
                    // Generic comment node (used in some grammars)
                    let text = self.node_text(&sib, source);
                    if text.starts_with("///") || text.starts_with("/**") || text.starts_with("#") {
                        comments.push(text.trim().to_string());
                    } else {
                        break;
                    }
                }
                _ => break,
            }
            sibling = sib.prev_sibling();
        }

        if comments.is_empty() {
            None
        } else {
            comments.reverse();
            Some(comments.join("\n"))
        }
    }

    /// Extract Python docstring from a function/class body.
    fn extract_python_docstring(&self, node: &tree_sitter::Node, source: &str) -> Option<String> {
        // Look for an expression_statement containing a string as the first child of the body
        let body = node.child_by_field_name("body")?;
        let mut cursor = body.walk();
        // Only check the first statement
        if let Some(child) = body.children(&mut cursor).next() {
            if child.kind() == "expression_statement" {
                let mut inner_cursor = child.walk();
                for inner in child.children(&mut inner_cursor) {
                    if inner.kind() == "string" || inner.kind() == "concatenated_string" {
                        let text = self.node_text(&inner, source);
                        let cleaned = text
                            .trim_start_matches("\"\"\"")
                            .trim_start_matches("'''")
                            .trim_end_matches("\"\"\"")
                            .trim_end_matches("'''")
                            .trim();
                        return Some(cleaned.to_string());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::relationships::RelationshipKind;
    use crate::indexer::parser::AstParser;
    use crate::indexer::relationship_builder::RelationshipBuilder;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    const REPO: &str = "repo";
    const FILE: &str = "src/lib.rs";

    fn read(source: &str) -> ExtractionResult {
        read_as(source, FILE, Language::Rust)
    }

    fn read_as(source: &str, path: &str, language: Language) -> ExtractionResult {
        let parsed = AstParser::new().parse(source, &language).unwrap();
        let file = CrawledFile {
            path: PathBuf::from(path),
            relative_path: path.to_string(),
            language,
            size_bytes: source.len() as u64,
            content_hash: String::new(),
        };
        SymbolExtractor::new(REPO).extract_from_tree(&parsed, source, &file)
    }

    /// The id of each entity of a file by its doc comment, which says in
    /// these sources which is which whatever else of it changes.
    fn ids(source: &str) -> BTreeMap<String, EntityId> {
        let read = read(source);
        let all: HashSet<&EntityId> = read.entities.iter().map(|e| &e.id).collect();
        assert_eq!(all.len(), read.entities.len(), "an id each: {source}");
        read.entities
            .into_iter()
            .filter_map(|e| Some((e.doc_comment?.trim().to_string(), e.id)))
            .collect()
    }

    fn plain(kind: &str, name: &str) -> EntityId {
        EntityId::from_components(REPO, FILE, kind, name)
    }

    fn an_impl(of: &str) -> String {
        format!("impl {of} {{\n    /// {of}'s\n    pub fn new() -> Self {{ {of} }}\n}}\n")
    }

    const ALONE: &str = "/// alone\npub fn alone() {}\n";

    /// The id of an entity whose kind and name others of its file share
    /// does not depend on them: they are reordered, and more are added and
    /// taken away, and no id changes while two of the name remain. One
    /// whose name becomes shared, or stops being, changes its id once.
    #[test]
    fn those_of_one_name_keep_their_ids_whatever_becomes_of_the_others() {
        let file = |impls: &[&str]| {
            let types: String = ["A", "B", "C"]
                .map(|t| format!("pub struct {t};\n"))
                .concat();
            let impls: String = impls.iter().map(|of| an_impl(of)).collect();
            format!("{types}{ALONE}{impls}")
        };
        let two = ids(&file(&["A", "B"]));
        let (a, b) = (two["A's"].clone(), two["B's"].clone());
        assert_ne!(a, b);
        assert_ne!(
            a,
            plain("method", "new"),
            "neither is the one the name alone means"
        );
        assert_ne!(b, plain("method", "new"));
        assert_eq!(two["alone"], plain("function", "alone"));

        let turned = ids(&file(&["B", "A"]));
        assert_eq!((&turned["A's"], &turned["B's"]), (&a, &b), "reordered");

        let three = ids(&file(&["C", "A", "B"]));
        assert_eq!((&three["A's"], &three["B's"]), (&a, &b), "one added before");
        let c = three["C's"].clone();
        assert!(c != a && c != b);
        let three = ids(&file(&["A", "B", "C"]));
        assert_eq!((&three["A's"], &three["B's"], &three["C's"]), (&a, &b, &c));

        let first_gone = ids(&file(&["B", "C"]));
        assert_eq!(
            (&first_gone["B's"], &first_gone["C's"]),
            (&b, &c),
            "the first removed"
        );

        // The last of a name is known by the name alone again: one change,
        // and one back should another of the name return.
        let last = ids(&file(&["B"]));
        assert_eq!(last["B's"], plain("method", "new"));
        assert_eq!(ids(&file(&["B", "A"]))["B's"], b);
        for each in [&turned, &three, &first_gone, &last] {
            assert_eq!(
                each["alone"],
                plain("function", "alone"),
                "a name of its own"
            );
        }
    }

    /// Impl blocks tell their methods apart as they are written, whether
    /// or not the type is defined in the file, and so do modules.
    #[test]
    fn where_one_stands_tells_it_apart_without_its_type_in_the_file() {
        let from = |of: &str| {
            format!("impl From<{of}> for X {{\n    /// from {of}\n    fn from(_: {of}) -> Self {{ X }}\n}}\n")
        };
        let inside = |module: &str| {
            format!("mod {module} {{\n    /// in {module}\n    pub fn make() {{}}\n}}\n")
        };
        let one = ids(&format!(
            "{}{}{}{}",
            from("u8"),
            from("u16"),
            inside("m"),
            inside("n")
        ));
        let other = ids(&format!(
            "{}{}{}{}",
            inside("n"),
            from("u16"),
            inside("m"),
            from("u8")
        ));
        assert_eq!(one, other, "in any order");
        assert_eq!(one.len(), 4);
        let apart: HashSet<&EntityId> = one.values().collect();
        assert_eq!(apart.len(), 4);
    }

    /// Those that nothing but their order tells apart have an id each all
    /// the same.
    #[test]
    fn those_that_stand_in_one_place_under_one_name_are_numbered() {
        let both = ids(
            "/// on unix\n#[cfg(unix)]\npub fn home() {}\n/// elsewhere\n#[cfg(not(unix))]\npub fn home() {}\n",
        );
        assert_eq!(both.len(), 2);
        assert_ne!(both["on unix"], both["elsewhere"]);
    }

    /// What a later one of a name holds, and what it calls and is called
    /// by, is recorded under its own id and not under the one the name
    /// alone would give it.
    #[test]
    fn what_a_later_one_of_a_name_holds_and_calls_is_its_own() {
        let source = "pub struct A;\npub struct B;\n\
            impl A {\n    /// A's\n    pub fn run() {\n        /// in A's\n        fn inner_a() {}\n        first();\n    }\n}\n\
            impl B {\n    /// B's\n    pub fn run() {\n        /// in B's\n        fn inner_b() {}\n        second();\n    }\n}\n\
            /// first\npub fn first() {}\n/// second\npub fn second() {}\n\
            /// asks\npub fn asks() {\n    A::run();\n    B::run();\n}\n";
        let ids = ids(source);
        let read = read(source);
        let (a, b) = (&ids["A's"], &ids["B's"]);
        assert_ne!(a, b);
        let parent_of = |doc: &str| {
            let entity = read.entities.iter().find(|e| e.id == ids[doc]).unwrap();
            entity.parent_id.clone().unwrap()
        };
        assert_eq!(
            (parent_of("in A's"), parent_of("in B's")),
            (a.clone(), b.clone())
        );
        let caller_of = |callee: &str| -> Vec<&EntityId> {
            let calling = read.calls.iter().filter(|call| call.callee_name == callee);
            calling.map(|call| &call.caller_id).collect()
        };
        assert_eq!(
            (caller_of("first"), caller_of("second")),
            (vec![a], vec![b])
        );

        let builder = RelationshipBuilder::new(REPO);
        let related =
            builder.build_cross_file_relationships(&read.entities, &read.impl_traits, &read.calls);
        let calls = |from: &EntityId, to: &EntityId| {
            related
                .iter()
                .any(|r| r.kind == RelationshipKind::Calls && r.source == *from && r.target == *to)
        };
        assert!(
            calls(a, &ids["first"]) && calls(b, &ids["second"]),
            "what each calls"
        );
        assert!(!calls(a, &ids["second"]) && !calls(b, &ids["first"]));
        assert!(
            calls(&ids["asks"], a) && calls(&ids["asks"], b),
            "what calls each"
        );
    }

    /// So in the other languages: a class tells its methods apart, in
    /// whichever order the classes are written.
    #[test]
    fn methods_of_one_name_in_two_classes_are_told_apart_in_any_order() {
        type Class = fn(&str) -> String;
        let languages: [(&str, Language, Class); 4] = [
            ("lib.py", Language::Python, |name| {
                format!("class {name}:\n    def run(self):\n        pass\n\n")
            }),
            ("lib.ts", Language::TypeScript, |name| {
                format!("class {name} {{\n  run(): void {{}}\n}}\n\n")
            }),
            ("lib.rb", Language::Ruby, |name| {
                format!("class {name}\n  def run\n  end\nend\n\n")
            }),
            ("Lib.kt", Language::Kotlin, |name| {
                format!("class {name} {{\n    fun run() {{}}\n}}\n\n")
            }),
        ];
        for (path, language, class) in languages {
            let runs = |source: String| {
                let read = read_as(&source, path, language.clone());
                let all: HashSet<&EntityId> = read.entities.iter().map(|e| &e.id).collect();
                assert_eq!(all.len(), read.entities.len(), "an id each: {path}");
                let runs = read.entities.into_iter().filter(|e| e.name == "run");
                let mut ids: Vec<(usize, EntityId)> = runs.map(|e| (e.start_line, e.id)).collect();
                ids.sort_by_key(|(line, _)| *line);
                ids.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
            };
            let one = runs(format!("{}{}", class("A"), class("B")));
            let other = runs(format!("{}{}", class("B"), class("A")));
            assert_eq!(one.len(), 2, "{path}");
            assert_ne!(one[0], one[1], "{path}");
            assert_eq!((&one[0], &one[1]), (&other[1], &other[0]), "{path}");
            // And with a third before them.
            let three = runs(format!("{}{}{}", class("C"), class("A"), class("B")));
            assert_eq!((&three[1], &three[2]), (&one[0], &one[1]), "{path}");
        }
    }
}
