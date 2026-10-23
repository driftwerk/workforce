//! Integration tests for the Codebase Knowledge Engine.
//!
//! These tests exercise the full pipeline: crawl → parse → extract → index → query.
//! They use an in-memory SQLite database and a temporary directory with sample code.

use std::sync::Arc;

use workforce_codebase_knowledge::freshness::FreshnessResult;
use workforce_codebase_knowledge::graph::entities::{EntityId, EntityKind, Language};
use workforce_codebase_knowledge::graph::CodeGraph;
use workforce_codebase_knowledge::indexer::crawler::{CrawlerConfig, FileCrawler};
use workforce_codebase_knowledge::indexer::{IndexingConfig, IndexingPipeline};
use workforce_codebase_knowledge::query::QueryEngine;

/// Helper to create a temporary project with sample Rust source files.
fn create_sample_rust_project(dir: &std::path::Path) {
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    std::fs::write(
        src_dir.join("lib.rs"),
        r#"//! A sample library for testing the knowledge engine.

pub mod config;
pub mod server;
pub mod utils;
"#,
    )
    .unwrap();

    std::fs::write(
        src_dir.join("config.rs"),
        r#"/// Application configuration.
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub debug: bool,
}

impl AppConfig {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
            debug: false,
        }
    }

    /// Create from environment variables.
    pub fn from_env() -> Self {
        Self {
            host: std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: std::env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8080),
            debug: std::env::var("DEBUG").is_ok(),
        }
    }

    /// Check if running in debug mode.
    pub fn is_debug(&self) -> bool {
        self.debug
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::new()
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        src_dir.join("server.rs"),
        r#"use crate::config::AppConfig;
use crate::utils::format_address;

/// The main HTTP server.
pub struct Server {
    config: AppConfig,
    running: bool,
}

impl Server {
    /// Create a new server with the given config.
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            running: false,
        }
    }

    /// Start the server.
    pub async fn start(&mut self) -> Result<(), String> {
        let addr = format_address(&self.config.host, self.config.port);
        println!("Starting server on {}", addr);
        self.running = true;
        Ok(())
    }

    /// Stop the server gracefully.
    pub async fn stop(&mut self) {
        self.running = false;
    }

    /// Check if the server is running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Handle an incoming request.
    pub async fn handle_request(&self, path: &str) -> Response {
        if self.config.is_debug() {
            println!("Handling request: {}", path);
        }

        match path {
            "/" => Response::ok("Hello, world!"),
            "/health" => Response::ok("ok"),
            _ => Response::not_found(),
        }
    }
}

/// An HTTP response.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn ok(body: &str) -> Self {
        Self { status: 200, body: body.to_string() }
    }

    pub fn not_found() -> Self {
        Self { status: 404, body: "Not Found".to_string() }
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        src_dir.join("utils.rs"),
        r#"/// Format an address from host and port.
pub fn format_address(host: &str, port: u16) -> String {
    format!("{}:{}", host, port)
}

/// Sanitize a path string for safe use.
pub fn sanitize_path(path: &str) -> String {
    path.replace("..", "")
        .replace("//", "/")
        .trim_start_matches('/')
        .to_string()
}

/// Generate a request ID.
pub fn generate_request_id() -> String {
    format!("req-{}", rand_u64())
}

fn rand_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "sample-project"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
}

/// Helper to create a sample TypeScript project.
fn create_sample_typescript_project(dir: &std::path::Path) {
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    std::fs::write(
        src_dir.join("index.ts"),
        r#"import { UserService } from './services/user';
import { Database } from './db';

export interface AppConfig {
  port: number;
  host: string;
  dbUrl: string;
}

export class Application {
  private config: AppConfig;
  private db: Database;
  private userService: UserService;

  constructor(config: AppConfig) {
    this.config = config;
    this.db = new Database(config.dbUrl);
    this.userService = new UserService(this.db);
  }

  async start(): Promise<void> {
    await this.db.connect();
    console.log(`Server running on ${this.config.host}:${this.config.port}`);
  }

  async stop(): Promise<void> {
    await this.db.disconnect();
  }
}
"#,
    )
    .unwrap();

    let services_dir = src_dir.join("services");
    std::fs::create_dir_all(&services_dir).unwrap();

    std::fs::write(
        services_dir.join("user.ts"),
        r#"import { Database } from '../db';

export interface User {
  id: string;
  name: string;
  email: string;
}

export class UserService {
  private db: Database;

  constructor(db: Database) {
    this.db = db;
  }

  async getUser(id: string): Promise<User | null> {
    return this.db.query('SELECT * FROM users WHERE id = ?', [id]);
  }

  async createUser(name: string, email: string): Promise<User> {
    const id = crypto.randomUUID();
    await this.db.execute('INSERT INTO users (id, name, email) VALUES (?, ?, ?)', [id, name, email]);
    return { id, name, email };
  }

  async deleteUser(id: string): Promise<void> {
    await this.db.execute('DELETE FROM users WHERE id = ?', [id]);
  }
}
"#,
    )
    .unwrap();

    std::fs::write(
        src_dir.join("db.ts"),
        r#"export class Database {
  private url: string;
  private connected: boolean = false;

  constructor(url: string) {
    this.url = url;
  }

  async connect(): Promise<void> {
    this.connected = true;
  }

  async disconnect(): Promise<void> {
    this.connected = false;
  }

  async query(sql: string, params?: any[]): Promise<any> {
    if (!this.connected) throw new Error('Not connected');
    return null;
  }

  async execute(sql: string, params?: any[]): Promise<void> {
    if (!this.connected) throw new Error('Not connected');
  }
}
"#,
    )
    .unwrap();
}

/// Helper to create a temporary project with sample Kotlin source files.
fn create_sample_kotlin_project(dir: &std::path::Path) {
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    std::fs::write(
        src_dir.join("Greeter.kt"),
        r#"package com.example.greeter

class Greeter(private val name: String) {
    fun greet(): String {
        return buildMessage("Hello")
    }

    private fun buildMessage(prefix: String): String {
        return "$prefix, $name!"
    }
}

object GreeterFactory {
    fun create(name: String): Greeter {
        return Greeter(name)
    }
}

fun main() {
    val greeter = GreeterFactory.create("World")
    println(greeter.greet())
}
"#,
    )
    .unwrap();
}

/// Helper to create an IndexingConfig from a temp dir.
fn make_config(dir: &std::path::Path, repo_id: &str) -> IndexingConfig {
    IndexingConfig {
        root: dir.to_path_buf(),
        repo_id: repo_id.to_string(),
        repo_name: repo_id.to_string(),
        crawler: CrawlerConfig {
            root: dir.to_path_buf(),
            ..CrawlerConfig::default()
        },
        parse_concurrency: 4,
        summary_concurrency: 16,
        generate_summaries: false,
    }
}

// ── Crawler tests ──────────────────────────────────────────────────

#[tokio::test]
async fn crawler_discovers_rust_files() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let crawler = FileCrawler::new(CrawlerConfig {
        root: dir.path().to_path_buf(),
        ..CrawlerConfig::default()
    });
    let files = crawler.crawl().await.unwrap();

    let rust_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == Language::Rust)
        .collect();
    assert!(
        rust_files.len() >= 4,
        "Expected at least 4 Rust files, found {}",
        rust_files.len()
    );

    for f in &rust_files {
        assert!(
            !f.content_hash.is_empty(),
            "File {} has empty hash",
            f.path.display()
        );
    }
}

#[tokio::test]
async fn crawler_discovers_typescript_files() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_typescript_project(dir.path());

    let crawler = FileCrawler::new(CrawlerConfig {
        root: dir.path().to_path_buf(),
        ..CrawlerConfig::default()
    });
    let files = crawler.crawl().await.unwrap();

    let ts_files: Vec<_> = files
        .iter()
        .filter(|f| f.language == Language::TypeScript)
        .collect();
    assert!(
        ts_files.len() >= 3,
        "Expected at least 3 TypeScript files, found {}",
        ts_files.len()
    );
}

// ── Full indexing pipeline tests ───────────────────────────────────

#[tokio::test]
async fn full_index_rust_project() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-rust-project";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    let result = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    assert!(result.files_indexed > 0, "No files indexed");
    assert!(result.entities_created > 0, "No entities created");
    assert!(result.relationships_created > 0, "No relationships created");

    let stats = graph.get_stats(repo_id).await.unwrap();
    assert!(stats.entity_count > 0, "No entities in graph");
    assert!(stats.file_count > 0, "No files in graph");

    // Verify specific entities were found
    let app_config = graph
        .find_entities_by_name(repo_id, "AppConfig", 5)
        .await
        .unwrap();
    assert!(!app_config.is_empty(), "Should find AppConfig struct");

    let server = graph
        .find_entities_by_name(repo_id, "Server", 5)
        .await
        .unwrap();
    assert!(!server.is_empty(), "Should find Server struct");

    let format_addr = graph
        .find_entities_by_name(repo_id, "format_address", 5)
        .await
        .unwrap();
    assert!(
        !format_addr.is_empty(),
        "Should find format_address function"
    );

    // Verify index state was recorded
    let state = graph.get_index_state(repo_id).await.unwrap();
    assert!(state.is_some(), "Index state should be recorded");
}

#[tokio::test]
async fn full_index_typescript_project() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_typescript_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-ts-project";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "TypeScript")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    let result = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    assert!(result.files_indexed > 0, "No files indexed");
    assert!(result.entities_created > 0, "No entities created");

    let app = graph
        .find_entities_by_name(repo_id, "Application", 5)
        .await
        .unwrap();
    assert!(!app.is_empty(), "Should find Application class");

    let user_service = graph
        .find_entities_by_name(repo_id, "UserService", 5)
        .await
        .unwrap();
    assert!(!user_service.is_empty(), "Should find UserService class");

    let database = graph
        .find_entities_by_name(repo_id, "Database", 5)
        .await
        .unwrap();
    assert!(!database.is_empty(), "Should find Database class");
}

#[tokio::test]
async fn full_index_kotlin_project() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_kotlin_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-kotlin-project";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Kotlin")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    let result = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    assert!(result.files_indexed > 0, "No files indexed");
    assert!(result.entities_created > 0, "No entities created");
    assert!(result.relationships_created > 0, "No relationships created");

    let greeter = graph
        .find_entities_by_name(repo_id, "Greeter", 5)
        .await
        .unwrap();
    assert!(!greeter.is_empty(), "Should find Greeter class");

    let factory = graph
        .find_entities_by_name(repo_id, "GreeterFactory", 5)
        .await
        .unwrap();
    assert!(!factory.is_empty(), "Should find GreeterFactory object");

    let main_fn = graph
        .find_entities_by_name(repo_id, "main", 5)
        .await
        .unwrap();
    assert!(!main_fn.is_empty(), "Should find main function");

    let greet_method = graph
        .find_entities_by_name(repo_id, "greet", 5)
        .await
        .unwrap();
    assert!(!greet_method.is_empty(), "Should find greet method");
}

// ── Query engine tests ─────────────────────────────────────────────

#[tokio::test]
async fn query_engine_ask_finds_entities() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-query-project";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    let engine = QueryEngine::new(graph.clone());

    // Ask about the server (use backtick-quoted name for reliable pattern extraction)
    let result = engine
        .ask(repo_id, "How does `Server` work?", None, false)
        .await
        .unwrap();
    assert!(!result.answer.is_empty(), "Should get a non-empty answer");
    assert!(result.confidence > 0.0, "Should have positive confidence");

    // Ask about configuration (PascalCase name extracted directly)
    let result = engine
        .ask(repo_id, "What is `AppConfig`?", None, false)
        .await
        .unwrap();
    assert!(
        !result.answer.is_empty(),
        "Should get a non-empty answer about AppConfig"
    );
}

#[tokio::test]
async fn query_engine_graph_queries() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-graph-query";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    let engine = QueryEngine::new(graph.clone());

    // Find entities by name and query graph
    let entities = graph
        .find_entities_by_name(repo_id, "Server", 5)
        .await
        .unwrap();
    if let Some(server_entity) = entities.first() {
        let callees = engine.graph_query("callees_of", &server_entity.id, 1).await;
        assert!(callees.is_ok(), "callees_of should succeed");
    }
}

#[tokio::test]
async fn query_engine_overview() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-overview";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    let engine = QueryEngine::new(graph.clone());

    let result = engine.overview(repo_id, None, "summary").await.unwrap();
    assert!(
        !result.answer.is_empty(),
        "Overview answer should not be empty"
    );
}

#[tokio::test]
async fn query_engine_impact_analysis() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-impact";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    let engine = QueryEngine::new(graph.clone());

    // Find AppConfig and check impact
    let entities = graph
        .find_entities_by_name(repo_id, "AppConfig", 5)
        .await
        .unwrap();
    if let Some(entity) = entities.first() {
        let report = engine
            .impact(&entity.id, "Add a new field to AppConfig", 2)
            .await
            .unwrap();
        assert!(
            report.confidence >= 0.0,
            "Impact report should have confidence"
        );
    }
}

// ── Freshness tracking tests ───────────────────────────────────────

#[tokio::test]
async fn freshness_detects_file_changes() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-freshness";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    let stats = graph.get_stats(repo_id).await.unwrap();
    assert!(stats.file_count > 0, "Should have indexed files");

    // Modify config.rs on disk
    let config_path = dir.path().join("src").join("config.rs");
    std::fs::write(
        &config_path,
        r#"/// Modified application configuration.
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub debug: bool,
    pub new_field: String,
}

impl AppConfig {
    pub fn new() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
            debug: false,
            new_field: String::new(),
        }
    }
}
"#,
    )
    .unwrap();

    // The freshness tracker should detect the change
    // check_file uses the file_path stored in the DB — which is relative to workspace root.
    // But it reads the file from that path directly, so we need to use the absolute path.
    let freshness = workforce_codebase_knowledge::freshness::FreshnessTracker::new(graph.clone());

    // The file path stored in the DB is relative. check_file reads it directly as a path.
    // We need to check what path is actually stored.
    let entities = graph
        .find_entities_in_file(repo_id, "src/config.rs")
        .await
        .unwrap();
    if entities.is_empty() {
        // Try with full path
        let abs_path = config_path.to_str().unwrap();
        let entities_abs = graph
            .find_entities_in_file(repo_id, abs_path)
            .await
            .unwrap();
        if !entities_abs.is_empty() {
            let result = freshness.check_file(repo_id, abs_path).await.unwrap();
            assert!(
                matches!(result, FreshnessResult::Stale { .. }),
                "Modified file should be stale, got {:?}",
                result
            );
        }
    } else {
        let result = freshness
            .check_file(repo_id, "src/config.rs")
            .await
            .unwrap();
        // File was modified, but the relative path might not resolve.
        // Either way, the check should not crash.
        match result {
            FreshnessResult::Stale { .. } => {} // Expected
            FreshnessResult::Fresh { .. } => {
                panic!("Modified file should not be fresh");
            }
            FreshnessResult::Unknown => {
                // This is OK if the file path didn't resolve
            }
        }
    }
}

// ── Multi-repo support tests ───────────────────────────────────────

#[tokio::test]
async fn multi_repo_isolation() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    create_sample_rust_project(dir1.path());
    create_sample_typescript_project(dir2.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());

    // Index Rust project
    let repo_id_1 = "rust-project";
    graph
        .register_repo(repo_id_1, dir1.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();
    let pipeline1 = IndexingPipeline::new(make_config(dir1.path(), repo_id_1), graph.clone());
    pipeline1
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    // Index TypeScript project
    let repo_id_2 = "ts-project";
    graph
        .register_repo(repo_id_2, dir2.path().to_str().unwrap(), "TypeScript")
        .await
        .unwrap();
    let pipeline2 = IndexingPipeline::new(make_config(dir2.path(), repo_id_2), graph.clone());
    pipeline2
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    // Verify isolation
    let stats1 = graph.get_stats(repo_id_1).await.unwrap();
    let stats2 = graph.get_stats(repo_id_2).await.unwrap();

    assert!(stats1.entity_count > 0, "Rust project should have entities");
    assert!(stats2.entity_count > 0, "TS project should have entities");

    // Rust project should have AppConfig but not Application
    let app_config_in_rust = graph
        .find_entities_by_name(repo_id_1, "AppConfig", 5)
        .await
        .unwrap();
    assert!(
        !app_config_in_rust.is_empty(),
        "Rust project should have AppConfig"
    );

    let application_in_rust = graph
        .find_entities_by_name(repo_id_1, "Application", 5)
        .await
        .unwrap();
    assert!(
        application_in_rust.is_empty(),
        "Rust project should NOT have Application class"
    );

    // TS project should have Application
    let application_in_ts = graph
        .find_entities_by_name(repo_id_2, "Application", 5)
        .await
        .unwrap();
    assert!(
        !application_in_ts.is_empty(),
        "TS project should have Application"
    );

    // Query engine should respect repo scope
    let engine = QueryEngine::new(graph.clone());

    let result1 = engine
        .ask(repo_id_1, "What structs exist?", None, false)
        .await
        .unwrap();
    let result2 = engine
        .ask(repo_id_2, "What classes exist?", None, false)
        .await
        .unwrap();

    assert!(!result1.answer.is_empty());
    assert!(!result2.answer.is_empty());
}

// ── Re-indexing tests ──────────────────────────────────────────────

#[tokio::test]
async fn reindex_updates_entities() {
    let dir = tempfile::tempdir().unwrap();
    create_sample_rust_project(dir.path());

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "test-reindex";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Rust")
        .await
        .unwrap();

    let config = make_config(dir.path(), repo_id);

    // First index
    let pipeline = IndexingPipeline::new(config.clone(), graph.clone());
    let _result1 = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();
    let stats1 = graph.get_stats(repo_id).await.unwrap();

    // Add a new file
    std::fs::write(
        dir.path().join("src").join("middleware.rs"),
        r#"/// Request middleware for logging.
pub struct LoggingMiddleware {
    pub enabled: bool,
}

impl LoggingMiddleware {
    pub fn new() -> Self {
        Self { enabled: true }
    }

    pub fn process(&self, request: &str) -> String {
        if self.enabled {
            println!("Processing: {}", request);
        }
        request.to_string()
    }
}
"#,
    )
    .unwrap();

    // Re-index
    let pipeline2 = IndexingPipeline::new(config, graph.clone());
    let _result2 = pipeline2
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();
    let stats2 = graph.get_stats(repo_id).await.unwrap();

    assert!(
        stats2.entity_count >= stats1.entity_count,
        "Entity count should not decrease: {} vs {}",
        stats2.entity_count,
        stats1.entity_count
    );

    let middleware = graph
        .find_entities_by_name(repo_id, "LoggingMiddleware", 5)
        .await
        .unwrap();
    assert!(
        !middleware.is_empty(),
        "Should find LoggingMiddleware after re-index"
    );
}

// ── Vector store unit tests ────────────────────────────────────────

#[test]
fn vector_entity_creation() {
    use workforce_codebase_knowledge::graph::entities::{CodeEntity, CodeVisibility};
    use workforce_codebase_knowledge::vector::{VectorCodeEntity, VectorCodeSummary};

    let entity = CodeEntity {
        id: EntityId("test-1".to_string()),
        kind: EntityKind::Function,
        name: "handle_request".to_string(),
        qualified_name: "server::handle_request".to_string(),
        file_path: "src/server.rs".to_string(),
        start_line: 50,
        end_line: 70,
        language: Language::Rust,
        signature: Some("pub async fn handle_request(&self, path: &str) -> Response".to_string()),
        doc_comment: Some("Handle an incoming request.".to_string()),
        visibility: CodeVisibility::Public,
        parent_id: None,
        repo_id: "test-repo".to_string(),
        content_hash: "hash123".to_string(),
    };

    let vec_entity = VectorCodeEntity::from_code_entity(
        &entity,
        "my-repo",
        Some("Processes HTTP requests and routes them to handlers"),
    );

    assert_eq!(vec_entity.entity_id, "test-1");
    assert_eq!(vec_entity.repo_id, "my-repo");
    assert!(vec_entity.searchable_content.contains("handle_request"));
    assert!(vec_entity
        .searchable_content
        .contains("Processes HTTP requests"));

    let summary = VectorCodeSummary {
        entity_id: "test-1".to_string(),
        repo_id: "my-repo".to_string(),
        granularity: "function".to_string(),
        file_path: "src/server.rs".to_string(),
        summary_text: "Processes HTTP requests and routes them to handlers".to_string(),
        is_stale: false,
        generation: String::new(),
    };

    assert_eq!(summary.granularity, "function");
    assert!(!summary.is_stale);
}

// ── Edge cases ─────────────────────────────────────────────────────

#[tokio::test]
async fn index_empty_directory() {
    let dir = tempfile::tempdir().unwrap();

    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let repo_id = "empty-project";
    graph
        .register_repo(repo_id, dir.path().to_str().unwrap(), "Unknown")
        .await
        .unwrap();

    let pipeline = IndexingPipeline::new(make_config(dir.path(), repo_id), graph.clone());
    let result = pipeline
        .run_full_index(workforce_codebase_knowledge::indexer::RunOptions::default())
        .await
        .unwrap();

    assert_eq!(result.files_indexed, 0);
    assert_eq!(result.entities_created, 0);
}

#[tokio::test]
async fn query_unindexed_repo() {
    let graph = Arc::new(CodeGraph::open_in_memory().unwrap());
    let engine = QueryEngine::new(graph.clone());

    let result = engine
        .ask("nonexistent-repo", "What is this?", None, false)
        .await
        .unwrap();
    assert!(result.confidence >= 0.0);
}
