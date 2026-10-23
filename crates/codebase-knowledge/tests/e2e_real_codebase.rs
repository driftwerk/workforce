//! End-to-end test that indexes the REAL driftwerk/workforce codebase.
//!
//! This validates the full CKE pipeline against actual production code:
//! crawl → parse → extract entities → build relationships → query.
//!
//! Run with: cargo test -p workforce-codebase-knowledge --test e2e_real_codebase

use std::sync::Arc;

use workforce_codebase_knowledge::graph::entities::{EntityKind, Language};
use workforce_codebase_knowledge::graph::CodeGraph;
use workforce_codebase_knowledge::indexer::crawler::{CrawlerConfig, FileCrawler};
use workforce_codebase_knowledge::indexer::{IndexResult, IndexingConfig, IndexingPipeline};
use workforce_codebase_knowledge::query::QueryEngine;

/// Resolve the workforce root directory (parent of crates/).
fn workforce_root() -> std::path::PathBuf {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/codebase-knowledge -> crates -> workforce
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn make_config(root: &std::path::Path, repo_id: &str) -> IndexingConfig {
    IndexingConfig {
        root: root.to_path_buf(),
        repo_id: repo_id.to_string(),
        repo_name: "workforce".to_string(),
        crawler: CrawlerConfig {
            root: root.to_path_buf(),
            ..CrawlerConfig::default()
        },
        parse_concurrency: 8,
        summary_concurrency: 16,
        generate_summaries: false,
    }
}

// ── Crawler against real codebase ─────────────────────────────────

#[tokio::test]
async fn crawl_real_codebase_discovers_files() {
    let root = workforce_root();
    let crawler = FileCrawler::new(CrawlerConfig {
        root: root.clone(),
        ..CrawlerConfig::default()
    });
    let files = crawler.crawl().await.unwrap();

    // The workforce codebase has hundreds of Rust files
    let rust_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == Language::Rust)
        .collect();
    assert!(
        rust_files.len() >= 50,
        "Expected at least 50 Rust files, found {}",
        rust_files.len()
    );

    // Should also find TOML files (Cargo.toml, workforce.toml)
    let toml_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == Language::Toml)
        .collect();
    assert!(
        !toml_files.is_empty(),
        "Expected at least 1 TOML file, found 0"
    );

    // Every file should have a content hash
    for f in &files {
        assert!(
            !f.content_hash.is_empty(),
            "File {} has empty hash",
            f.path.display()
        );
    }

    println!(
        "Crawled {} files ({} Rust, {} TOML)",
        files.len(),
        rust_files.len(),
        toml_files.len()
    );
}

// ── One index of the real codebase, checked many ways ─────────────

const REPO: &str = "workforce";

/// The real codebase, indexed once: a full index takes minutes, so every
/// check below reads this one rather than building its own.
struct Indexed {
    graph: Arc<CodeGraph>,
    pipeline: IndexingPipeline,
    result: IndexResult,
}

#[tokio::test]
async fn real_codebase() {
    let root = workforce_root();
    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    graph
        .register_repo(REPO, root.to_str().unwrap(), "Rust")
        .await
        .unwrap();
    let pipeline = IndexingPipeline::new(make_config(&root, REPO), graph.clone());
    let result = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();
    let indexed = Indexed {
        graph,
        pipeline,
        result,
    };

    index_real_codebase(&indexed).await;
    query_real_codebase_ask(&indexed).await;
    query_real_codebase_graph(&indexed).await;
    query_real_codebase_impact(&indexed).await;
    query_real_codebase_overview(&indexed).await;
    relationship_breakdown(&indexed).await;
    incremental_index_no_changes(&indexed).await;
}

async fn index_real_codebase(indexed: &Indexed) {
    let Indexed { graph, result, .. } = indexed;

    println!(
        "Indexed: {} files, {} entities, {} relationships in {:.1}s",
        result.files_indexed,
        result.entities_created,
        result.relationships_created,
        result.duration_ms as f64 / 1000.0
    );

    // Sanity checks on scale
    assert!(
        result.files_indexed >= 50,
        "Expected >= 50 files indexed, got {}",
        result.files_indexed
    );
    assert!(
        result.entities_created >= 200,
        "Expected >= 200 entities, got {}",
        result.entities_created
    );
    assert!(
        result.relationships_created >= 50,
        "Expected >= 50 relationships, got {}",
        result.relationships_created
    );

    let stats = graph.get_stats(REPO).await.unwrap();
    println!(
        "Graph stats: {} entities, {} relationships, {} files",
        stats.entity_count, stats.relationship_count, stats.file_count
    );

    // ── Verify well-known entities exist ──

    // AgentService — the main hub service
    let agent_svc = graph
        .find_entities_by_name(REPO, "AgentService", 5)
        .await
        .unwrap();
    assert!(
        !agent_svc.is_empty(),
        "Should find AgentService in the graph"
    );
    let svc = &agent_svc[0];
    assert_eq!(svc.kind, EntityKind::Struct);
    assert!(
        svc.file_path.contains("serve"),
        "AgentService should be in serve.rs, found in {}",
        svc.file_path
    );

    // WorkforceConfig — the config struct
    let wf_config = graph
        .find_entities_by_name(REPO, "WorkforceConfig", 5)
        .await
        .unwrap();
    assert!(
        !wf_config.is_empty(),
        "Should find WorkforceConfig in the graph"
    );

    // CodeGraph — our own graph struct
    let code_graph = graph
        .find_entities_by_name(REPO, "CodeGraph", 5)
        .await
        .unwrap();
    assert!(!code_graph.is_empty(), "Should find CodeGraph in the graph");

    // IndexingPipeline
    let pipeline_entity = graph
        .find_entities_by_name(REPO, "IndexingPipeline", 5)
        .await
        .unwrap();
    assert!(
        !pipeline_entity.is_empty(),
        "Should find IndexingPipeline in the graph"
    );

    // QueryEngine
    let query_engine = graph
        .find_entities_by_name(REPO, "QueryEngine", 5)
        .await
        .unwrap();
    assert!(
        !query_engine.is_empty(),
        "Should find QueryEngine in the graph"
    );

    // EntityKind enum
    let entity_kind = graph
        .find_entities_by_name(REPO, "EntityKind", 5)
        .await
        .unwrap();
    assert!(
        !entity_kind.is_empty(),
        "Should find EntityKind in the graph"
    );

    // RelationshipKind enum
    let rel_kind = graph
        .find_entities_by_name(REPO, "RelationshipKind", 5)
        .await
        .unwrap();
    assert!(
        !rel_kind.is_empty(),
        "Should find RelationshipKind in the graph"
    );

    // KnowledgeConfig — our new config struct
    let knowledge_config = graph
        .find_entities_by_name(REPO, "KnowledgeConfig", 5)
        .await
        .unwrap();
    assert!(
        !knowledge_config.is_empty(),
        "Should find KnowledgeConfig in the graph"
    );

    // ── Verify entity kinds are diverse ──

    let all_entities = graph
        .find_entities_by_name(REPO, "", 10000)
        .await
        .unwrap_or_default();

    let struct_count = all_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Struct)
        .count();
    let fn_count = all_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Function || e.kind == EntityKind::Method)
        .count();
    let enum_count = all_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Enum)
        .count();
    let trait_count = all_entities
        .iter()
        .filter(|e| e.kind == EntityKind::Trait)
        .count();

    println!(
        "Entity breakdown: {} structs, {} functions/methods, {} enums, {} traits",
        struct_count, fn_count, enum_count, trait_count
    );

    assert!(
        struct_count >= 20,
        "Expected >= 20 structs, got {}",
        struct_count
    );
    assert!(
        fn_count >= 50,
        "Expected >= 50 functions/methods, got {}",
        fn_count
    );
    assert!(enum_count >= 5, "Expected >= 5 enums, got {}", enum_count);
}

// ── Query engine against real codebase ────────────────────────────

async fn query_real_codebase_ask(indexed: &Indexed) {
    let Indexed { graph, .. } = indexed;

    let engine = QueryEngine::new(graph.clone());

    // Ask about a specific struct
    let result = engine
        .ask(REPO, "What is `CodeGraph`?", None, false)
        .await
        .unwrap();
    assert!(!result.answer.is_empty(), "Answer should not be empty");
    assert!(
        result.confidence > 0.0,
        "Confidence should be positive for a known entity"
    );
    println!(
        "Q: What is CodeGraph?\nA: {}\nConfidence: {:.2}",
        result.answer, result.confidence
    );

    // Ask about the query engine
    let result = engine
        .ask(REPO, "What is `QueryEngine`?", None, false)
        .await
        .unwrap();
    assert!(!result.answer.is_empty());
    println!(
        "Q: What is QueryEngine?\nA: {}\nConfidence: {:.2}",
        result.answer, result.confidence
    );

    // Ask about a concept that spans multiple entities
    let result = engine
        .ask(REPO, "What is `IndexingPipeline`?", None, false)
        .await
        .unwrap();
    assert!(!result.answer.is_empty());
    println!(
        "Q: What is IndexingPipeline?\nA: {}\nConfidence: {:.2}",
        result.answer, result.confidence
    );
}

async fn query_real_codebase_graph(indexed: &Indexed) {
    let Indexed { graph, .. } = indexed;

    let engine = QueryEngine::new(graph.clone());

    // Find CodeGraph and query its callees
    let entities = graph
        .find_entities_by_name(REPO, "CodeGraph", 5)
        .await
        .unwrap();
    assert!(!entities.is_empty(), "Should find CodeGraph");

    let code_graph_entity = &entities[0];
    println!(
        "CodeGraph: {} ({}:{})",
        code_graph_entity.qualified_name, code_graph_entity.file_path, code_graph_entity.start_line
    );

    // Query callees
    let callees = engine
        .graph_query("callees_of", &code_graph_entity.id, 1)
        .await
        .unwrap();
    println!("CodeGraph callees: {} found", callees.len());

    // Query callers
    let callers = engine
        .graph_query("callers_of", &code_graph_entity.id, 1)
        .await
        .unwrap();
    println!("CodeGraph callers: {} found", callers.len());

    // Find RelationshipKind and check implementations
    let rel_kinds = graph
        .find_entities_by_name(REPO, "RelationshipKind", 5)
        .await
        .unwrap();
    if !rel_kinds.is_empty() {
        let impls = engine
            .graph_query("implementations_of", &rel_kinds[0].id, 1)
            .await
            .unwrap();
        println!("RelationshipKind implementations: {} found", impls.len());
    }
}

async fn query_real_codebase_impact(indexed: &Indexed) {
    let Indexed { graph, .. } = indexed;

    let engine = QueryEngine::new(graph.clone());

    // Impact analysis: what if we change EntityKind?
    let entities = graph
        .find_entities_by_name(REPO, "EntityKind", 5)
        .await
        .unwrap();
    assert!(!entities.is_empty());

    let report = engine
        .impact(&entities[0].id, "Add a new variant to EntityKind", 3)
        .await
        .unwrap();

    println!("Impact report for EntityKind change:");
    println!("  Summary: {}", report.summary);
    println!("  Affected files: {}", report.affected_files.len());
    println!("  Test impact: {}", report.test_impact.len());
    println!("  Complexity: {}", report.estimated_complexity);
    println!("  Confidence: {:.2}", report.confidence);

    assert!(
        report.confidence > 0.0,
        "Impact confidence should be positive"
    );

    // Impact analysis: what if we change CodeGraph?
    let cg_entities = graph
        .find_entities_by_name(REPO, "CodeGraph", 5)
        .await
        .unwrap();
    if !cg_entities.is_empty() {
        let report = engine
            .impact(&cg_entities[0].id, "Change the signature of open()", 3)
            .await
            .unwrap();
        println!("\nImpact report for CodeGraph.open() signature change:");
        println!("  Summary: {}", report.summary);
        println!("  Affected files: {}", report.affected_files.len());
    }
}

async fn query_real_codebase_overview(indexed: &Indexed) {
    let Indexed { graph, .. } = indexed;

    let engine = QueryEngine::new(graph.clone());

    // Full overview
    let result = engine.overview(REPO, None, "summary").await.unwrap();
    assert!(!result.answer.is_empty(), "Overview should not be empty");
    println!("Full overview:\n{}", result.answer);

    // Scoped overview (codebase-knowledge crate)
    let result = engine
        .overview(REPO, Some("crates/codebase-knowledge"), "full")
        .await
        .unwrap();
    assert!(
        !result.answer.is_empty(),
        "Scoped overview should not be empty"
    );
    println!("\nCodebase-knowledge overview:\n{}", result.answer);
}

// ── Relationship diagnostics ──────────────────────────────────────

async fn relationship_breakdown(indexed: &Indexed) {
    let Indexed { graph, .. } = indexed;

    let stats = graph.get_stats(REPO).await.unwrap();
    println!(
        "Total: {} entities, {} relationships",
        stats.entity_count, stats.relationship_count
    );

    // Count relationships by kind
    let rel_kinds = [
        "defined_in",
        "contains",
        "type_of",
        "implements",
        "imports",
        "tests_for",
        "calls",
        "called_by",
        "depends_on",
    ];
    for kind in &rel_kinds {
        let count = graph
            .count_relationships_by_kind(REPO, kind)
            .await
            .unwrap_or(0);
        println!("  {}: {}", kind, count);
    }

    // DefinedIn and Contains should be the bulk (every entity has a DefinedIn to its file)
    let defined_in = graph
        .count_relationships_by_kind(REPO, "defined_in")
        .await
        .unwrap_or(0);
    assert!(
        defined_in > 100,
        "Expected many DefinedIn relationships, got {}",
        defined_in
    );

    let contains = graph
        .count_relationships_by_kind(REPO, "contains")
        .await
        .unwrap_or(0);
    assert!(
        contains > 50,
        "Expected many Contains relationships, got {}",
        contains
    );

    // TypeOf: functions referencing types from other files
    let type_of = graph
        .count_relationships_by_kind(REPO, "type_of")
        .await
        .unwrap_or(0);
    println!("TypeOf relationships (cross-file type refs): {}", type_of);
    assert!(
        type_of > 10,
        "Expected TypeOf relationships from cross-file type references, got {}",
        type_of
    );

    // Implements: impl Trait for Struct patterns
    let implements = graph
        .count_relationships_by_kind(REPO, "implements")
        .await
        .unwrap_or(0);
    assert!(
        implements > 10,
        "Expected Implements relationships, got {}",
        implements
    );

    // Calls: function call expressions resolved to known entities
    let calls = graph
        .count_relationships_by_kind(REPO, "calls")
        .await
        .unwrap_or(0);
    assert!(
        calls > 100,
        "Expected many Calls relationships, got {}",
        calls
    );

    // CalledBy: materialised inverse of Calls
    let called_by = graph
        .count_relationships_by_kind(REPO, "called_by")
        .await
        .unwrap_or(0);
    assert_eq!(calls, called_by, "CalledBy count should equal Calls count");
}

// ── Incremental indexing ─────────────────────────────────────────

async fn incremental_index_no_changes(indexed: &Indexed) {
    let Indexed {
        graph,
        pipeline,
        result: full_result,
    } = indexed;

    let stats_after_full = graph.get_stats(REPO).await.unwrap();
    println!(
        "Full index: {} files, {} entities, {} rels in {:.1}s",
        full_result.files_indexed,
        full_result.entities_created,
        full_result.relationships_created,
        full_result.duration_ms as f64 / 1000.0
    );

    // Verify index state was recorded with a commit
    let state = graph.get_index_state(REPO).await.unwrap();
    assert!(state.is_some(), "Index state should exist after full index");
    let state = state.unwrap();
    assert!(
        state.last_commit.is_some(),
        "Last commit should be recorded"
    );
    assert!(
        state.last_full_index_at.is_some(),
        "last_full_index_at should be set after full index"
    );

    // 2. Run incremental index — nothing has changed since HEAD
    let start = std::time::Instant::now();
    let incr_result = pipeline
        .run_incremental_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();
    let incr_ms = start.elapsed().as_millis();

    println!(
        "Incremental (no changes): {} files, {} entities, {} rels in {}ms",
        incr_result.files_indexed,
        incr_result.entities_created,
        incr_result.relationships_created,
        incr_ms
    );

    // Should detect 0 changes and return immediately
    assert_eq!(
        incr_result.files_indexed, 0,
        "No files should be re-indexed when nothing changed"
    );

    // Stats should be unchanged
    let stats_after_incr = graph.get_stats(REPO).await.unwrap();
    assert_eq!(
        stats_after_full.entity_count, stats_after_incr.entity_count,
        "Entity count should not change after no-op incremental"
    );
    assert_eq!(
        stats_after_full.relationship_count, stats_after_incr.relationship_count,
        "Relationship count should not change after no-op incremental"
    );

    // Incremental should be significantly faster than full index
    assert!(
        incr_ms < 2000,
        "No-op incremental should complete in < 2s, took {}ms",
        incr_ms
    );
    println!(
        "Speedup: {:.0}x faster ({:.1}s vs {:.3}s)",
        full_result.duration_ms as f64 / incr_ms.max(1) as f64,
        full_result.duration_ms as f64 / 1000.0,
        incr_ms as f64 / 1000.0
    );
}

// ── Performance sanity check ──────────────────────────────────────

#[tokio::test]
// Hardware/load-sensitive benchmark: asserts absolute wall-clock thresholds
// (index < 60s, ask < 500ms, graph < 100ms) that are flaky on a shared CI
// runner under concurrent compile load. Run manually with `--ignored`.
#[ignore = "perf benchmark — wall-clock thresholds, run with --ignored"]
async fn index_performance_reasonable() {
    let root = workforce_root();
    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "workforce-perf";

    graph
        .register_repo(repo_id, root.to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(&root, repo_id), graph.clone());
    let result = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    // Indexing the workforce crate should complete in under 60 seconds
    assert!(
        result.duration_ms < 60_000,
        "Indexing took too long: {:.1}s (expected < 60s)",
        result.duration_ms as f64 / 1000.0
    );

    let engine = QueryEngine::new(graph.clone());

    // Query latency: knowledge.ask should respond in < 500ms
    let start = std::time::Instant::now();
    let _ = engine
        .ask(repo_id, "What is `CodeGraph`?", None, false)
        .await
        .unwrap();
    let ask_ms = start.elapsed().as_millis();
    assert!(
        ask_ms < 500,
        "knowledge.ask took {}ms (expected < 500ms)",
        ask_ms
    );
    println!("knowledge.ask latency: {}ms", ask_ms);

    // Query latency: knowledge.graph should respond in < 100ms
    let entities = graph
        .find_entities_by_name(repo_id, "CodeGraph", 5)
        .await
        .unwrap();
    if !entities.is_empty() {
        let start = std::time::Instant::now();
        let _ = engine
            .graph_query("callers_of", &entities[0].id, 1)
            .await
            .unwrap();
        let graph_ms = start.elapsed().as_millis();
        assert!(
            graph_ms < 100,
            "knowledge.graph took {}ms (expected < 100ms)",
            graph_ms
        );
        println!("knowledge.graph latency: {}ms", graph_ms);
    }
}
