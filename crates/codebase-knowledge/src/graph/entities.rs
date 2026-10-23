use serde::{Deserialize, Serialize};

/// Unique identifier for a code entity.
/// Deterministic: hash(repo_id + file_path + kind + name).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId(pub String);

impl EntityId {
    /// Create a deterministic entity ID from its components.
    pub fn from_components(repo_id: &str, file_path: &str, kind: &str, name: &str) -> Self {
        let input = format!("{repo_id}:{file_path}:{kind}:{name}");
        let hash = blake3::hash(input.as_bytes());
        Self(hash.to_hex()[..16].to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A code entity extracted from AST parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEntity {
    pub id: EntityId,
    pub repo_id: String,
    pub kind: EntityKind,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
    pub visibility: CodeVisibility,
    pub parent_id: Option<EntityId>,
    pub doc_comment: Option<String>,
    pub content_hash: String,
    pub language: Language,
}

impl CodeEntity {
    /// Construct a CodeEntity from a SQLite row.
    pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> Self {
        Self {
            id: EntityId(row.get::<_, String>(0).unwrap_or_default()),
            repo_id: row.get(1).unwrap_or_default(),
            kind: EntityKind::from_str(&row.get::<_, String>(2).unwrap_or_default()),
            name: row.get(3).unwrap_or_default(),
            qualified_name: row.get(4).unwrap_or_default(),
            file_path: row.get(5).unwrap_or_default(),
            start_line: row.get::<_, i64>(6).unwrap_or(0) as usize,
            end_line: row.get::<_, i64>(7).unwrap_or(0) as usize,
            signature: row.get(8).ok(),
            visibility: CodeVisibility::from_str(&row.get::<_, String>(9).unwrap_or_default()),
            parent_id: row
                .get::<_, Option<String>>(10)
                .unwrap_or(None)
                .map(EntityId),
            doc_comment: row.get(11).ok().flatten(),
            content_hash: row.get(12).unwrap_or_default(),
            language: Language::from_str(&row.get::<_, String>(13).unwrap_or_default()),
        }
    }

    /// Whether this entity should receive an LLM-generated summary: a
    /// parser-language (else a file-entity would summarise from a bare filename
    /// and pollute retrieval), files/modules always, functions/methods unless a
    /// trivial private helper.
    ///
    /// MUST stay in lockstep with [`summarisable_where_sql`] (the SQL the
    /// completeness-gate denominator counts with): if the two diverge,
    /// `summaries_missing()` never reaches zero and the summary phase re-runs
    /// on every trigger forever. The `count_matches_is_summarisable_predicate`
    /// test guards the coupling.
    pub fn is_summarisable(&self) -> bool {
        if !self.language.has_parser() {
            return false;
        }
        match self.kind {
            EntityKind::Function | EntityKind::Method => !self.is_trivial_private_helper(),
            EntityKind::File | EntityKind::Module => true,
            _ => false,
        }
    }

    /// A private, undocumented, very short function/method. The doc check is
    /// exact-empty (not trimmed) so it cannot diverge from the SQL `doc_comment
    /// = ''` in [`summarisable_where_sql`] on whitespace-only values.
    fn is_trivial_private_helper(&self) -> bool {
        self.visibility == CodeVisibility::Private
            && self.doc_comment.as_deref().unwrap_or("").is_empty()
            && self.end_line.saturating_sub(self.start_line) <= TRIVIAL_FN_MAX_SPAN
    }
}

/// Function/method bodies at or below this line span — when *also* private
/// and undocumented — are too trivial to be worth an LLM summary. Must match
/// the `(end_line - start_line) <= N` clause in `count_summarisable_entities`
/// (both sqlite and postgres). Span is `end_line - start_line`, so `2` covers
/// up to a three-physical-line body (e.g. a one-line getter).
pub const TRIVIAL_FN_MAX_SPAN: usize = 2;

/// The languages whose entities are worth summarising — exactly those with a
/// tree-sitter parser (see [`Language::has_parser`]). Single source of truth
/// shared between the Rust predicate and the SQL count via
/// [`summarisable_languages_sql`].
pub const SUMMARISABLE_LANGUAGES: [&str; 7] = [
    "rust",
    "typescript",
    "javascript",
    "python",
    "go",
    "ruby",
    "kotlin",
];

/// Render [`SUMMARISABLE_LANGUAGES`] as a SQL `IN (...)` list literal, e.g.
/// `'rust', 'typescript', ...`. The values are compile-time constants (never
/// user input), so direct interpolation is injection-safe.
pub fn summarisable_languages_sql() -> String {
    SUMMARISABLE_LANGUAGES
        .iter()
        .map(|l| format!("'{l}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The SQL `WHERE`-clause body (without the `repo_id` predicate) selecting
/// exactly the entities [`CodeEntity::is_summarisable`] accepts. Used verbatim
/// by the SQLite and Postgres `count_summarisable_entities` so the
/// completeness-gate denominator can never drift from the Rust predicate. All
/// interpolated values are compile-time constants — injection-safe.
pub fn summarisable_where_sql() -> String {
    format!(
        "language IN ({langs}) \
         AND (kind IN ('file', 'module') \
              OR (kind IN ('function', 'method') \
                  AND NOT (visibility = 'private' \
                           AND (doc_comment IS NULL OR doc_comment = '') \
                           AND (end_line - start_line) <= {span})))",
        langs = summarisable_languages_sql(),
        span = TRIVIAL_FN_MAX_SPAN,
    )
}

/// Test-only builder for a [`CodeEntity`]. Distinct `(file_path, kind, name)`
/// triples yield distinct ids, so several can coexist in one set. Shared by the
/// indexer and graph test modules.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn mk_entity(
    name: &str,
    kind: EntityKind,
    file_path: &str,
    signature: Option<&str>,
    visibility: CodeVisibility,
    doc: Option<&str>,
    language: Language,
    span: usize,
) -> CodeEntity {
    CodeEntity {
        id: EntityId::from_components("r", file_path, kind.as_str(), name),
        repo_id: "r".to_string(),
        kind,
        name: name.to_string(),
        qualified_name: format!("mod::{name}"),
        file_path: file_path.to_string(),
        start_line: 1,
        end_line: 1 + span,
        signature: signature.map(str::to_string),
        visibility,
        parent_id: None,
        doc_comment: doc.map(str::to_string),
        content_hash: "h".to_string(),
        language,
    }
}

/// The kind of code entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    File,
    Module,
    Function,
    Method,
    Struct,
    Class,
    Interface,
    Trait,
    Enum,
    EnumVariant,
    TypeAlias,
    Constant,
    Variable,
    Import,
    Test,
    Macro,
}

impl EntityKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::File => "file",
            Self::Module => "module",
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Enum => "enum",
            Self::EnumVariant => "enum_variant",
            Self::TypeAlias => "type_alias",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Import => "import",
            Self::Test => "test",
            Self::Macro => "macro",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "file" => Self::File,
            "module" => Self::Module,
            "function" => Self::Function,
            "method" => Self::Method,
            "struct" => Self::Struct,
            "class" => Self::Class,
            "interface" => Self::Interface,
            "trait" => Self::Trait,
            "enum" => Self::Enum,
            "enum_variant" => Self::EnumVariant,
            "type_alias" => Self::TypeAlias,
            "constant" => Self::Constant,
            "variable" => Self::Variable,
            "import" => Self::Import,
            "test" => Self::Test,
            "macro" => Self::Macro,
            _ => Self::Function, // fallback
        }
    }
}

/// Whether `path` is a test file, by the layout conventions of the languages we
/// index — the single definition for consumers that must not put test code in
/// front of a model. The extractors do NOT consume it yet: stamping
/// [`EntityKind::Test`] from here is the durable fix but needs a full re-index,
/// so it is deferred (see the note in `indexer::symbol_extractor`).
///
/// Path-shaped on purpose. The per-language attribute checks the extractors do
/// (`#[test]`, `def test_*`) only ever covered Rust and Python, so on a JVM,
/// Ruby, TS or Go repo *nothing* read as a test: core-api's 13,050 indexed entities
/// under `src/test/` carried `method`/`class`/`file` and not one `test`, which is
/// how a `billing-plans` question got a 1196-line dump of
/// `StaleOrderReconciliationServiceTest.kt` (2026-08-06).
///
/// Directory matches are segment-bounded by the surrounding slashes, so
/// `/latest/` and `/protest/` do not match `/test/`.
pub fn is_test_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    let lower = path.to_lowercase();
    // Test ROOTS: `tests/` (Rust, Python, JS), `test/` (JVM `src/test/kotlin`,
    // `src/test/java`), `spec/` (RSpec, Jasmine), `__tests__/` (Jest),
    // `testFixtures/` (the Gradle test-fixtures source set — shared container and
    // stub helpers, test infrastructure rather than production code).
    for dir in ["tests", "test", "spec", "__tests__", "testfixtures"] {
        if lower.starts_with(&format!("{dir}/")) || lower.contains(&format!("/{dir}/")) {
            return true;
        }
    }
    let lower_file = lower.rsplit('/').next().unwrap_or(&lower);
    // Jest/Vitest/Mocha: `foo.test.ts`, `foo.spec.tsx`.
    if lower_file.contains(".test.") || lower_file.contains(".spec.") {
        return true;
    }
    // Go `foo_test.go`; Ruby `foo_spec.rb`; Python `test_foo.py` / `foo_test.py`.
    // The underscore is the word boundary, so `latest.go` cannot match here.
    if lower_file.starts_with("test_")
        || ["_test", "_spec"].iter().any(|s| {
            lower_file
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.ends_with(s))
        })
    {
        return true;
    }
    // JVM/.NET naming (`FooServiceTest.kt`, `FooServiceTests.cs`), matched on the
    // ORIGINAL-CASE stem: the capital is the word boundary, so `latest.ts` does
    // not match while `FooServiceTest.kt` does, and a `TestHarness.kt` helper
    // stays out because the check is a suffix.
    //
    // Deliberately NOT `ends_with("Spec")`. Checked against the live prod index:
    // a bare `*Spec` stem outside a spec/ directory is PRODUCTION code here —
    // `WriteOpenApiSpec.kt` exists under `src/main/kotlin` in ~20 core-api services
    // (40 entities), and excluding it would silently drop production files from
    // explanations, which is this bug inverted. RSpec is already covered by the
    // `spec/` directory clause, which is where `*Spec` genuinely means test.
    let file = path.rsplit('/').next().unwrap_or(&path);
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    stem.ends_with("Test") || stem.ends_with("Tests")
}

/// Visibility of a code entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeVisibility {
    Public,
    #[default]
    Private,
    Protected,
    Internal,
}

impl CodeVisibility {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Protected => "protected",
            Self::Internal => "internal",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "public" => Self::Public,
            "protected" => Self::Protected,
            "internal" => Self::Internal,
            _ => Self::Private,
        }
    }
}

/// Supported programming languages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
    Ruby,
    Kotlin,
    Markdown,
    Toml,
    Yaml,
    Json,
    Unknown(String),
}

impl Language {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Ruby => "ruby",
            Self::Kotlin => "kotlin",
            Self::Markdown => "markdown",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Json => "json",
            Self::Unknown(s) => s.as_str(),
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "rust" => Self::Rust,
            "typescript" => Self::TypeScript,
            "javascript" => Self::JavaScript,
            "python" => Self::Python,
            "go" => Self::Go,
            "ruby" => Self::Ruby,
            "kotlin" => Self::Kotlin,
            "markdown" => Self::Markdown,
            "toml" => Self::Toml,
            "yaml" => Self::Yaml,
            "json" => Self::Json,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Detect language from a file extension.
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "rs" => Self::Rust,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "rb" => Self::Ruby,
            "kt" | "kts" => Self::Kotlin,
            "md" | "mdx" => Self::Markdown,
            "toml" => Self::Toml,
            "yml" | "yaml" => Self::Yaml,
            "json" => Self::Json,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether this language has tree-sitter grammar support.
    pub fn has_parser(&self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::TypeScript
                | Self::JavaScript
                | Self::Python
                | Self::Go
                | Self::Ruby
                | Self::Kotlin
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_deterministic() {
        let id1 = EntityId::from_components("repo1", "src/main.rs", "function", "main");
        let id2 = EntityId::from_components("repo1", "src/main.rs", "function", "main");
        assert_eq!(id1, id2);

        let id3 = EntityId::from_components("repo1", "src/main.rs", "function", "other");
        assert_ne!(id1, id3);
    }

    #[test]
    fn language_from_extension() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("tsx"), Language::TypeScript);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("kt"), Language::Kotlin);
        assert_eq!(Language::from_extension("kts"), Language::Kotlin);
        assert!(Language::from_extension("rs").has_parser());
        assert!(Language::from_extension("kt").has_parser());
        assert!(!Language::from_extension("md").has_parser());
    }

    #[test]
    fn summarisable_languages_match_has_parser() {
        // The const list (used to build the SQL count) must agree with
        // `has_parser` (used by the Rust predicate), or the two diverge and
        // the completeness gate loops.
        for l in SUMMARISABLE_LANGUAGES {
            assert!(
                Language::from_str(l).has_parser(),
                "{l} should have a parser"
            );
        }
        for l in ["markdown", "toml", "yaml", "json", "weird"] {
            assert!(
                !Language::from_str(l).has_parser(),
                "{l} should not have a parser"
            );
        }
    }

    #[test]
    fn summarisable_languages_sql_is_quoted_list() {
        let sql = summarisable_languages_sql();
        assert!(sql.starts_with("'rust'"));
        assert!(sql.contains("'kotlin'"));
        assert_eq!(sql.matches('\'').count(), SUMMARISABLE_LANGUAGES.len() * 2);
    }

    #[test]
    fn is_test_path_covers_every_indexed_language() {
        // The case that shipped the 2026-08-06 regression: JVM `src/test/`.
        for p in [
            "billing-plans/src/test/kotlin/com/harbor/billingplans/service/StaleOrderReconciliationServiceTest.kt",
            "src/test/java/com/example/FooServiceTest.java",
            "app/src/androidTest/java/FooTests.kt", // by the `Tests` stem
            // Gradle test-fixtures source set: shared container/stub helpers.
            "common/src/testFixtures/kotlin/com/harbor/commontest/aws/LocalStackForTests.kt",
            // A test that lives in the main source set anyway (core-api has these).
            "order-routing/src/main/kotlin/com/harbor/orderrouting/scenario/RouteTest.kt",
            "crates/hub/tests/code_context_real_repo.rs", // Rust
            "spec/models/order_spec.rb",                  // RSpec
            "app/models/foo_spec.rb",                     // RSpec, flat
            "src/__tests__/widget.ts",                    // Jest
            "src/widget.test.tsx",                        // Vitest
            "src/widget.spec.js",
            "internal/store/store_test.go",               // Go
            "tests/test_pipeline.py",                     // Python
            "pipeline/test_pipeline.py",
        ] {
            assert!(is_test_path(p), "expected a test path: {p}");
        }
    }

    #[test]
    fn is_test_path_leaves_production_code_alone() {
        for p in [
            "billing-plans/src/main/kotlin/com/harbor/billingplans/http/WebSocketHandler.kt",
            "billing-plans/src/main/kotlin/com/harbor/billingplans/service/StaleOrderReconciliationService.kt",
            "crates/hub/src/code_context.rs",
            "app/models/order.rb",
            // `latest` ends in "test" — the underscore/capital word boundary is
            // what keeps these out, and getting it wrong would silently drop
            // production files from every explanation.
            "src/utils/latest.ts",
            "internal/version/latest.go",
            // A helper named Test* is not itself a test.
            "src/main/kotlin/TestHarness.kt",
            "src/support/TestDataBuilder.java",
            // Not a segment match.
            "src/protest/handler.rs",
            // A bare `*Spec` stem outside a spec/ directory is PRODUCTION code in
            // this domain — verified on the live index, `WriteOpenApiSpec.kt` sits
            // under `src/main/kotlin` in ~20 core-api services. Excluding it would
            // silently drop production files from explanations.
            "billing-plans/src/main/kotlin/com/harbor/billingplans/api/WriteOpenApiSpec.kt",
            "audit-service/src/main/kotlin/com/harbor/auditservice/contract/WriteOpenApiSpec.kt",
        ] {
            assert!(!is_test_path(p), "expected production code: {p}");
        }
    }

    #[test]
    fn entity_kind_roundtrip() {
        let kinds = vec![
            EntityKind::File,
            EntityKind::Function,
            EntityKind::Struct,
            EntityKind::Trait,
            EntityKind::Enum,
            EntityKind::Test,
        ];
        for kind in kinds {
            assert_eq!(EntityKind::from_str(kind.as_str()), kind);
        }
    }
}
