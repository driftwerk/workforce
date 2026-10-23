//! Real-repo validation of the answer-ready context retriever.
//!
//! Indexes the REAL local workforce checkout with the same indexing path
//! `knowledge.index` uses (`KnowledgeService` → `IndexingPipeline`), then runs
//! `CodeContextService::assemble_for` for two prose questions and prints a full
//! diagnostic: the seed entities (patterns + ranked entity list), the assembled
//! context (bodies truncated), and the per-component `reached_via` tags.
//!
//! This is a DIAGNOSTIC, not a green-check test — everything is printed before
//! any assert, so a failed assert can't abort the report. The asserts are loose
//! sanity gates ("something assembled"), not completeness checks; completeness
//! is judged from the printed output.
//!
//! Symbol-only path: NO Weaviate vector store (the default `enabled=false`
//! config). This is the worst case for prose questions — seeds come purely from
//! LIKE-substring symbol search over names/signatures/doc-comments.
//!
//! Run with:
//!   cargo test -p workforce-hub --test code_context_real_repo -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::Arc;

use workforce_codebase_knowledge::query::natural_language::QueryPlanner;
use workforce_common::settings::{KnowledgeConfig, WeaviateConfig};
use workforce_hub::code_context::CodeContextService;
use workforce_hub::knowledge::KnowledgeService;

/// The repo root to index: the worktree checkout this test runs in
/// (`CARGO_MANIFEST_DIR` = `<root>/crates/hub`, so two levels up). This is the
/// real workforce tree — `scheduler.rs`/`vault.rs` are unchanged on this branch,
/// so the validation is faithful to production code. (The task named the
/// original `/Users/.../workforce` checkout; the worktree is byte-identical for
/// the files under test.)
fn workforce_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

/// Build a `KnowledgeService` whose default workspace IS the workforce root, so
/// `resolve_repo_id(.., None)` (what `assemble_for` calls) derives the same
/// repo_id the index lives under. Indexes only `crates/hub/src` +
/// `crates/agent-runtime/src` by excluding everything else, keeping the run fast
/// while preserving call-graph closure within those crates. The data dir
/// (SQLite DB) is a throwaway tempdir.
async fn index_subset(tmp: &std::path::Path) -> (Arc<KnowledgeService>, String) {
    let root = workforce_root();
    // Exclude everything except the two target crates' src dirs. The crawler
    // applies these as gitignore-style globs against repo-relative paths.
    // `crates/**` then re-includes via the negation is not supported by the
    // ignore engine here, so instead we exclude the sibling crates and the
    // non-src parts explicitly. Simplest robust approach: exclude all crates
    // except hub + agent-runtime, plus the usual heavy dirs.
    let kcfg = KnowledgeConfig {
        generate_summaries: false,
        parse_concurrency: 8,
        exclude_patterns: exclude_all_but_targets(&root),
        ..KnowledgeConfig::default()
    };

    let wcfg = WeaviateConfig::default(); // enabled = false → symbol-only path
    let service = Arc::new(
        KnowledgeService::new(root.clone(), tmp.to_path_buf(), &kcfg, &wcfg, None)
            .expect("build KnowledgeService"),
    );

    let repo_id = service.repo_id();
    let result = service
        .trigger_full_index()
        .await
        .expect("full index of subset");
    eprintln!(
        "\n=== INDEX: {} files, {} entities, {} relationships in {:.1}s (repo_id={}) ===",
        result.files_indexed,
        result.entities_created,
        result.relationships_created,
        result.duration_ms as f64 / 1000.0,
        repo_id,
    );
    (service, repo_id)
}

/// Exclude every top-level dir and every crate EXCEPT `crates/hub` and
/// `crates/agent-runtime`, so the crawl is bounded to the two target crates.
fn exclude_all_but_targets(root: &std::path::Path) -> Vec<String> {
    let keep = ["hub", "agent-runtime"];
    let mut patterns: Vec<String> = Vec::new();
    // Exclude all non-crates top-level dirs.
    for entry in std::fs::read_dir(root).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "crates" || name.starts_with('.') {
            continue;
        }
        if entry.path().is_dir() {
            patterns.push(format!("{name}/**"));
        }
    }
    // Exclude every crate except the ones we keep.
    if let Ok(crates) = std::fs::read_dir(root.join("crates")) {
        for entry in crates.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() && !keep.contains(&name.as_str()) {
                patterns.push(format!("crates/{name}/**"));
            }
        }
    }
    patterns
}

/// Print the seed diagnostic for a question: extracted patterns + the ranked
/// entity list `resolve_seed_entities` returns, marking which top-5 the
/// assembler actually uses.
async fn print_seeds(service: &KnowledgeService, repo_id: &str, question: &str) {
    let plan = QueryPlanner::plan(question, false);
    eprintln!("\n--- SEEDS for: {question:?}");
    eprintln!("    entity_patterns = {:?}", plan.entity_patterns);
    eprintln!("    preferred_kinds = {:?}", plan.preferred_kinds);
    match service
        .query_engine()
        .resolve_seed_entities(repo_id, question, None, &plan)
        .await
    {
        Ok((entities, best_vec)) => {
            eprintln!(
                "    resolved {} entities (best_vector_score={:.3}); top-5 = the closure seeds:",
                entities.len(),
                best_vec
            );
            for (i, e) in entities.iter().enumerate() {
                let mark = if i < 5 { ">>" } else { "  " };
                eprintln!(
                    "    {mark} [{i}] {:?} {}  @ {}:{}-{}",
                    e.kind.as_str(),
                    e.qualified_name,
                    e.file_path,
                    e.start_line,
                    e.end_line,
                );
            }
        }
        Err(e) => eprintln!("    resolve_seed_entities ERROR: {e}"),
    }
}

/// Run `assemble_for`, print component list + truncated bodies.
async fn run_and_print(service: Arc<KnowledgeService>, question: &str, budget: usize) {
    let svc = CodeContextService::new(Arc::clone(&service), workforce_root());
    eprintln!("\n========================================================");
    eprintln!("QUESTION: {question}");
    eprintln!("========================================================");
    print_seeds(&service, &service.repo_id(), question).await;

    let assembled = svc.assemble_for(question, budget).await;
    match assembled {
        None => {
            eprintln!("\n!!! assemble_for returned None (no seeds / empty closure / no bodies)")
        }
        Some(ctx) => {
            eprintln!(
                "\n--- ASSEMBLED: {} components, repo={} @ {}",
                ctx.components, ctx.repo, ctx.commit
            );
            // Walk the assembled text: each component starts with a `// repo/path:..` line.
            let total_lines = ctx.text.lines().count();
            eprintln!("    total assembled lines = {total_lines}");
            eprintln!("\n--- COMPONENT LIST (locator lines only) ---");
            for line in ctx.text.lines() {
                if line.starts_with("// ")
                    && (line.contains(".rs:")
                        || line.starts_with("// ") && line.contains("component"))
                {
                    eprintln!("{line}");
                }
            }
            eprintln!("\n--- FULL CONTEXT (bodies truncated to first 15 lines each) ---");
            print_truncated(&ctx.text);
        }
    }
}

/// Print the assembled block, truncating each component's body to 15 lines after
/// its locator comment. Component boundaries are the `// <repo>/<path>:..` lines.
fn print_truncated(text: &str) {
    let mut body_lines = 0usize;
    for line in text.lines() {
        let is_locator = line.starts_with("// ") && line.contains(".rs:");
        let is_header = line.starts_with("// ") && line.contains("component") && line.contains('@');
        if is_locator || is_header {
            body_lines = 0;
            eprintln!("{line}");
            continue;
        }
        if line.trim().is_empty() {
            eprintln!();
            continue;
        }
        body_lines += 1;
        if body_lines <= 15 {
            eprintln!("{line}");
        } else if body_lines == 16 {
            eprintln!("        … (body truncated)");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-repo diagnostic — slow, run with --ignored --nocapture"]
async fn assemble_scheduler_and_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, _repo_id) = index_subset(tmp.path()).await;

    // The two required prose questions, plus a symbol-named variant of each to
    // separate "prose can't seed" from "closure incomplete once seeded right".
    let questions = [
        "How does the scheduler work in workforce?",
        "How does the secret vault encrypt secrets?",
        // Symbol-named diagnostic variants:
        "How does the `Scheduler` `tick` loop and `Schedule` enum work?",
        "How does `Vault` `encrypt_blob` use `Aes256Gcm` and `derive_key`?",
    ];

    for q in questions {
        run_and_print(Arc::clone(&service), q, 1500).await;
    }

    // Loose sanity gate: the two prose questions must at least assemble
    // something (non-None) — completeness is judged from the printed output.
    let svc = CodeContextService::new(Arc::clone(&service), workforce_root());
    let a = svc
        .assemble_for("How does the scheduler work in workforce?", 1500)
        .await;
    let b = svc
        .assemble_for("How does the secret vault encrypt secrets?", 1500)
        .await;
    assert!(a.is_some(), "scheduler question assembled nothing");
    assert!(b.is_some(), "vault question assembled nothing");
}

/// ACCEPTANCE (2026-07-31): the production question that exposed the whole
/// failure chain, run against the real repo offline.
///
/// "Where do we enforce the tool allowlist, and is there any path that
/// bypasses it?" — bare English, no symbol names, mechanism vocabulary. It must
/// (1) pass the intent gate, (2) assemble real enforcement code, (3) stay
/// inside the requested line budget under the HONEST accounting (locators,
/// joiners, header and grep block all counted — the old body-only accounting
/// emitted 1,359 lines on a 1,200 budget in production), and (4) produce
/// deterministic citations, because the grounding line under the answer is the
/// reader's confirmation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-repo acceptance — slow, run with --ignored --nocapture"]
async fn acceptance_allowlist_question_injects_within_budget_with_citations() {
    let question =
        "Where do we enforce the tool allowlist, and is there any path that bypasses it?";
    assert!(
        workforce_hub::code_context::question_seeks_code(question),
        "the acceptance question must pass the intent gate"
    );

    let tmp = tempfile::tempdir().unwrap();
    let (service, _repo_id) = index_subset(tmp.path()).await;
    run_and_print(Arc::clone(&service), question, 1200).await;

    let svc = CodeContextService::new(Arc::clone(&service), workforce_root());
    let ctx = svc
        .assemble_for(question, 1200)
        .await
        .expect("the acceptance question must inject");
    let total_lines = ctx.text.lines().count();
    assert!(
        total_lines <= 1200,
        "honest budget: emitted {total_lines} lines on a 1200 budget"
    );
    assert!(
        !ctx.citations.is_empty(),
        "citations are the reader's confirmation — none produced"
    );
    let line = workforce_hub::code_context::grounding_line(&ctx.citations);
    assert!(
        line.contains(".rs:"),
        "grounding line must carry file:line locations: {line}"
    );
    eprintln!("\nGROUNDING LINE:{line}");
}

/// Isolate the CLOSURE half from the SEED half. Seed resolution (above) ranks
/// behaviour-bearing methods out of the top-5, so the closure is never given a
/// good function seed. Here we bypass seed resolution: look up
/// `Scheduler::tick` / `Vault::encrypt_blob` directly by name and feed them as
/// the seed to `full_context_for`, then read the bodies. This answers: when the
/// closure IS seeded on the right function, does it pull the call graph the
/// completeness checklist demands — and what does it still miss?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-repo diagnostic — slow, run with --ignored --nocapture"]
async fn closure_on_ideal_function_seeds() {
    use workforce_codebase_knowledge::graph::entities::EntityId;

    let tmp = tempfile::tempdir().unwrap();
    let (service, repo_id) = index_subset(tmp.path()).await;
    let graph = service.graph();

    // (name to look up, file the canonical definition lives in) — we pick the
    // entity whose file_path matches, to avoid same-name collisions.
    let targets = [
        ("tick", "crates/hub/src/scheduler.rs"),
        ("encrypt_blob", "crates/hub/src/vault.rs"),
    ];

    for (name, want_file) in targets {
        eprintln!("\n========================================================");
        eprintln!("IDEAL-SEED CLOSURE PROBE: {name} ({want_file})");
        eprintln!("========================================================");
        let candidates = graph
            .find_entities_by_name(&repo_id, name, 20)
            .await
            .unwrap();
        let Some(seed) = candidates
            .iter()
            .find(|e| e.file_path == want_file)
            .or_else(|| candidates.first())
        else {
            eprintln!("!!! no entity named {name:?} found");
            continue;
        };
        eprintln!(
            "seed = {} {}  @ {}:{}-{}",
            seed.kind.as_str(),
            seed.qualified_name,
            seed.file_path,
            seed.start_line,
            seed.end_line
        );

        let closure = graph
            .full_context_for(
                &repo_id,
                &[EntityId(seed.id.as_str().to_string())],
                &[name.to_string()],
                1500,
            )
            .await
            .unwrap();
        eprintln!("closure = {} entities (budget 1500):", closure.len());
        let mut total_span = 0usize;
        for ce in &closure {
            let span = ce.entity.end_line.saturating_sub(ce.entity.start_line) + 1;
            total_span += span;
            eprintln!(
                "   [{:<6}] {:<8} {}  @ {}:{}-{}",
                ce.reached_via.as_str(),
                ce.entity.kind.as_str(),
                ce.entity.qualified_name,
                ce.entity.file_path,
                ce.entity.start_line,
                ce.entity.end_line,
            );
        }
        eprintln!("total inclusive span = {total_span} lines");

        // Now run the full assemble path seeded on this ideal entity, so we see
        // the bodies a weak model would actually read.
        let svc = CodeContextService::new(Arc::clone(&service), workforce_root());
        // We can't inject seeds into assemble_for (it resolves its own), so this
        // probe stays at the closure+graph level; the body read is exercised by
        // the prose test above. Report the closure shape — that's the gap.
        let _ = svc;
    }
}
