//! How a hub wires its knowledge service: construction, the tools on it,
//! and the background work it runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};
use workforce_common::settings::WorkforceConfig;

use super::MaintenanceRunner;
use crate::serve::AgentService;

/// Whether a process keeps its code vectors in line with the code graph in
/// the background: fills them after an index run, puts right files noted
/// as pending, compares them with the graph. The hub does. A process that
/// serves one session beside it (`workforce mcp-serve`) leaves them to it:
/// several keepers would each embed the same repo on the one node, and
/// the hub's is the one whose state is reported and whose pace is set.
/// Such a process still notes what its own index runs leave behind, in
/// the graph, where the hub finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorUpkeep {
    Kept,
    Left,
}

/// The knowledge services this process has made and still has.
#[cfg(test)]
pub(crate) fn services() -> Vec<Arc<super::KnowledgeService>> {
    let made = KNOWLEDGE_SERVICES.lock().unwrap_or_else(|e| e.into_inner());
    made.iter().filter_map(std::sync::Weak::upgrade).collect()
}

/// Every knowledge service this process made, for [`code_vector_status`].
static KNOWLEDGE_SERVICES: std::sync::Mutex<Vec<std::sync::Weak<super::KnowledgeService>>> =
    std::sync::Mutex::new(Vec::new());

/// How the code vectors of every indexed repo stood when each of this
/// process's knowledge services last looked, each repo once: for health and
/// status reports. It asks nothing of the vector store or the graph, so a
/// liveness probe may call it whatever either is doing. Empty where no
/// store keeps track of them, and before the first look.
pub fn code_vector_status() -> Vec<super::RepoVectors> {
    let services: Vec<_> = {
        let made = KNOWLEDGE_SERVICES.lock().unwrap_or_else(|e| e.into_inner());
        made.iter().filter_map(std::sync::Weak::upgrade).collect()
    };
    let mut repos: Vec<super::RepoVectors> = Vec::new();
    for service in services {
        for repo in service.vector_status_seen() {
            if !repos.iter().any(|known| known.repo_id == repo.repo_id) {
                repos.push(repo);
            }
        }
    }
    repos
}

/// Whether code questions are asking the vector store (`"closed"`), leaving
/// it alone after it kept not answering in time (`"open"`), or about to
/// try it again (`"half_open"`): the worst of this process's knowledge
/// services, which share the store. `None` where questions have no
/// deadline.
pub fn code_vector_breaker() -> Option<&'static str> {
    let services: Vec<_> = {
        let made = KNOWLEDGE_SERVICES.lock().unwrap_or_else(|e| e.into_inner());
        made.iter().filter_map(std::sync::Weak::upgrade).collect()
    };
    let told = services
        .iter()
        .filter_map(|service| service.vector_breaker());
    told.max_by_key(|told| match *told {
        "open" => 2,
        "half_open" => 1,
        _ => 0,
    })
}

/// Register the knowledge tools from a shared `KnowledgeService`.
pub fn register_tools(
    tools: &workforce_agent::tools::registry::ToolRegistry,
    ks: &Arc<super::KnowledgeService>,
    agent_workspace: &Path,
) {
    for tool in query_tools(ks, agent_workspace, None, None, "default", None) {
        tools.register(tool);
    }
    tools.register(Arc::new(super::KnowledgeIndexTool::new(
        Arc::clone(ks),
        agent_workspace.to_path_buf(),
    )));
    // `knowledge.delete` registered without a project store here.
    // Callers with a store (the CLI projects-tools block) overwrite
    // this entry with `KnowledgeDeleteTool::with_store(...)` so
    // their registration carries DW-508's multi-project usage gate;
    // callers without a store (mcp_serving) get the unguarded tool
    // — same surface they had pre-DW-508.
    tools.register(Arc::new(super::KnowledgeDeleteTool::new(Arc::clone(ks))));
}

/// Build the read-only knowledge query tools, bound to `resolver`
/// when the hub supplies one (canonical name → repo routing) and
/// resolver-less otherwise (single-agent: the service's own root).
/// When `evaluator` is supplied each tool also carries a
/// [`KnowledgeAccessGuard`] keyed on its tool name + the agent id,
/// so a User-scope `Deny` from the cascade short-circuits the call.
pub(crate) fn query_tools(
    ks: &Arc<super::KnowledgeService>,
    agent_workspace: &Path,
    resolver: Option<&Arc<super::RepoResolver>>,
    evaluator: Option<&Arc<workforce_permissions::cache::PermissionEvaluator>>,
    agent_id: &str,
    runner: Option<&Arc<super::MaintenanceRunner>>,
) -> Vec<Arc<dyn workforce_agent::tools::Tool>> {
    macro_rules! query_tool {
        ($tool:expr, $name:literal) => {{
            let tool = $tool;
            let tool = match resolver {
                Some(resolver) => tool.with_resolver(Arc::clone(resolver)),
                None => tool,
            };
            let tool = match evaluator {
                Some(evaluator) => tool.with_access_guard(super::KnowledgeAccessGuard::new(
                    Arc::clone(evaluator),
                    agent_id,
                    $name,
                )),
                None => tool,
            };
            Arc::new(tool) as Arc<dyn workforce_agent::tools::Tool>
        }};
    }
    // `code.context` resolves seeds across the agent's indexed repos itself
    // (from the graph's `repositories` rows), so it needs no `RepoResolver` —
    // only the access guard, plus the maintenance runner to SCOPE the shared
    // codegraph store to THIS agent's repos. Built outside `query_tool!`
    // (which threads a resolver every tool must accept).
    let code_context = {
        let tool = crate::code_context::CodeContextTool::new(
            Arc::clone(ks),
            agent_workspace.to_path_buf(),
        );
        let tool = match runner {
            Some(runner) => tool.with_agent_scope(Arc::clone(runner), agent_id.to_string()),
            None => tool,
        };
        let tool = match evaluator {
            Some(evaluator) => tool.with_access_guard(super::KnowledgeAccessGuard::new(
                Arc::clone(evaluator),
                agent_id,
                "code.context",
            )),
            None => tool,
        };
        Arc::new(tool) as Arc<dyn workforce_agent::tools::Tool>
    };
    let ws = agent_workspace.to_path_buf();
    vec![
        query_tool!(
            super::KnowledgeAskTool::new(Arc::clone(ks), ws.clone()),
            "knowledge.ask"
        ),
        query_tool!(
            super::KnowledgeSymbolTool::new(Arc::clone(ks), ws.clone()),
            "knowledge.symbol"
        ),
        query_tool!(
            super::KnowledgeExplainTool::new(Arc::clone(ks), ws.clone()),
            "knowledge.explain"
        ),
        query_tool!(
            super::KnowledgeImpactTool::new(Arc::clone(ks), ws.clone()),
            "knowledge.impact"
        ),
        query_tool!(
            super::KnowledgeOverviewTool::new(Arc::clone(ks), ws.clone()),
            "knowledge.overview"
        ),
        query_tool!(
            super::KnowledgeGraphTool::new(Arc::clone(ks), ws.clone()),
            "knowledge.graph"
        ),
        // Live, read-only text search over the indexed repo checkouts.
        // The symbol graph above can't see prose/specs/config/comments;
        // these let the agent grep + read the actual current files.
        query_tool!(super::RepoGrepTool::new(ws.clone()), "repo.grep"),
        query_tool!(super::RepoReadTool::new(ws.clone()), "repo.read"),
        query_tool!(super::RepoGlobTool::new(ws.clone()), "repo.glob"),
        query_tool!(super::RepoListTool::new(ws.clone()), "repo.list"),
        // Answer-ready code: assemble a symbol's full body + its call-graph
        // closure + tests as ready-to-read source, instead of pointers the
        // agent must chain follow-up reads on. Resolves across all indexed
        // repos itself (built above, resolver-free).
        code_context,
    ]
}

/// Re-register the name-routed knowledge tools (the six query tools plus
/// `knowledge.index`) bound to `resolver`, overwriting the resolver-less
/// variants registered at build time. Called once the hub's `RepoResolver`
/// exists (the canonical-indexing path); a no-op when the agent has no
/// knowledge service. Carries the cascade guard onto each query tool
/// when the permission evaluator is wired, and the maintenance `runner` so
/// `code.context` can scope the shared codegraph store to this agent's repos.
pub fn rebind_tools(
    service: &AgentService,
    resolver: &Arc<super::RepoResolver>,
    runner: &Arc<super::MaintenanceRunner>,
) {
    let Some(ks) = service.knowledge_service().cloned() else {
        return;
    };
    let evaluator = service.permission_evaluator.read().clone();
    for tool in query_tools(
        &ks,
        &service.workspace,
        Some(resolver),
        evaluator.as_ref(),
        service.agent_id.as_str(),
        Some(runner),
    ) {
        service.register_tool(tool);
    }
    service.register_tool(Arc::new(
        super::KnowledgeIndexTool::new(Arc::clone(&ks), service.workspace.clone())
            .with_resolver(Arc::clone(resolver)),
    ));
    // `context.fuse` names its repo as the query tools do, so it resolves
    // through the same catalog; its code half is `knowledge.ask`'s query
    // and answers to that tool's cascade rule.
    if let Some(memory) = service.memory_store.as_ref() {
        let fuse = crate::context_fuse_tool::ContextFuseTool::new(
            Arc::clone(&ks),
            Arc::clone(memory),
            service.workspace.clone(),
            service.agent_id.clone(),
            service.resolved.clone(),
        )
        .with_resolver(Arc::clone(resolver));
        service.register_tool(Arc::new(match evaluator.as_ref() {
            Some(evaluator) => fuse.with_code_guard(super::KnowledgeAccessGuard::new(
                Arc::clone(evaluator),
                service.agent_id.as_str(),
                "knowledge.ask",
            )),
            None => fuse,
        }));
    }
}

/// Create a `KnowledgeService` from an effective `KnowledgeConfig`.
///
/// Used by `AgentRegistry` to create one instance per unique knowledge root.
/// Accepts a resolved `KnowledgeConfig` (which may be per-agent or global).
/// `fallback_data_dir` is used when neither `knowledge.root` nor `knowledge.data_dir` is set.
/// `summariser` builds the summariser when summaries are on.
///
/// Takes the full `WorkforceConfig` so it can pick up
/// `[codegraph] backend` and `[postgres]` for the optional
/// Postgres code graph backend (Phase 3).
pub fn create_service(
    knowledge_config: &workforce_common::settings::KnowledgeConfig,
    vectors: super::VectorBackend<'_>,
    fallback_data_dir: &str,
    codegraph_config: &workforce_common::settings::CodegraphConfig,
    postgres_config: Option<&workforce_postgres::PostgresConfig>,
    summariser: super::HubSummariserBuilder,
    upkeep: VectorUpkeep,
) -> Option<Arc<super::KnowledgeService>> {
    if !knowledge_config.enabled {
        info!("Knowledge engine disabled by config");
        return None;
    }
    let knowledge_root = knowledge_config
        .root
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(fallback_data_dir));
    let knowledge_data_dir = knowledge_config
        .data_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| knowledge_root.clone());

    let summariser = knowledge_config
        .generate_summaries
        .then(|| summariser.into_arc());

    match super::KnowledgeService::new_with_backend(
        knowledge_root,
        knowledge_data_dir,
        knowledge_config,
        vectors,
        codegraph_config,
        postgres_config,
        summariser,
    ) {
        Ok(ks) => {
            let ks = Arc::new(ks);
            if upkeep == VectorUpkeep::Kept {
                ks.spawn_vector_backfill();
            }
            KNOWLEDGE_SERVICES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&ks));
            Some(ks)
        }
        Err(e) => {
            warn!(
                "Knowledge service unavailable ({}), knowledge tools not registered",
                e
            );
            None
        }
    }
}

/// Start startup indexing for a knowledge service, when configured.
///
/// Called once — either by `build_inner` (single-agent) or by
/// `AgentRegistry` (multi-agent, after all agents are built).
pub fn start_background(ks: &Arc<super::KnowledgeService>, config: &WorkforceConfig) {
    // Startup indexing with retry — during rolling updates the old container
    // may still hold the SQLite lock, so we retry with backoff.
    if config.knowledge.index_on_startup {
        let ks_index = Arc::clone(ks);
        tokio::spawn(async move {
            let max_retries = 3u32;
            for attempt in 0..=max_retries {
                if attempt > 0 {
                    let delay = std::time::Duration::from_secs(10 * attempt as u64);
                    info!(
                        "Retrying startup indexing (attempt {}/{}) after {:?}",
                        attempt + 1,
                        max_retries + 1,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                } else {
                    info!("Startup indexing triggered");
                }
                match ks_index.trigger_incremental_index().await {
                    Ok(r) => {
                        info!(
                            "Startup indexing complete: {} files, {} entities, {} relationships in {:.1}s",
                            r.files_indexed,
                            r.entities_created,
                            r.relationships_created,
                            r.duration_ms as f64 / 1000.0
                        );
                        return;
                    }
                    Err(e) if attempt < max_retries => {
                        warn!("Startup indexing attempt {} failed: {}", attempt + 1, e);
                    }
                    Err(e) => {
                        warn!(
                            "Startup indexing failed after {} attempts: {}",
                            max_retries + 1,
                            e
                        );
                    }
                }
            }
        });
    }
}

/// Late-bind the hub knowledge maintenance runner (see the field). Called
/// from `cli::main` once the runner exists so the push webhook can drive a
/// canonical re-index.
pub fn set_maintenance_runner(service: &AgentService, runner: Arc<MaintenanceRunner>) {
    service.maintenance_runner.set(runner);
}

/// The hub knowledge maintenance runner, once wired.
pub fn maintenance_runner(service: &AgentService) -> Option<Arc<MaintenanceRunner>> {
    service.maintenance_runner.get::<MaintenanceRunner>()
}

/// Re-register every read-side knowledge query tool on `service` with the
/// cascade guard of `evaluator` wired, so later invocations gate per user.
/// Called once the service holds the evaluator.
pub fn bind_evaluator(
    service: &AgentService,
    evaluator: &Arc<workforce_permissions::cache::PermissionEvaluator>,
) {
    let evaluator = Arc::clone(evaluator);
    // Re-register the query tools with the guard wired. Resolver is
    // None here: `rebind_knowledge_tools` runs later (once the
    // canonical resolver exists) and re-applies both resolver and
    // guard via the same factory.
    if let Some(ks) = service.knowledge_service.as_ref() {
        // Scope `code.context` to this agent if the runner is already wired
        // (it is re-applied by `rebind_knowledge_tools` once the canonical
        // resolver exists, which is the steady state).
        let runner = maintenance_runner(service);
        for tool in crate::knowledge::query_tools(
            ks,
            &service.workspace,
            None,
            Some(&evaluator),
            service.agent_id.as_str(),
            runner.as_ref(),
        ) {
            service.tools.register(tool);
        }
    }
}
