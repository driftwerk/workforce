//! Knowledge tools — 7 tools that give agents deep codebase understanding.
//!
//! - `knowledge.ask` - Natural language Q&A about the codebase
//! - `knowledge.symbol` - Exact symbol-name lookup (where is X defined/used)
//! - `knowledge.explain` - Detailed explanation of a specific entity
//! - `knowledge.impact` - Change impact analysis
//! - `knowledge.overview` - Architecture overview
//! - `knowledge.graph` - Direct graph queries (callers, callees, etc.)
//! - `knowledge.index` - Trigger and monitor indexing
//!
//! The core `KnowledgeService` lives in `workforce-knowledge-service` so it
//! can be shared with the local MCP server. This module re-exports it and
//! supplies a hub-side [`service::HubSummariserBuilder`] for the LLM-backed
//! summariser.

pub mod access_guard;
pub mod ask;
pub mod delete;
pub mod explain;
pub mod global_index;
pub mod graph;
pub mod impact;
pub mod index;
pub mod index_layout;
pub mod lineage_backfill;
pub mod overview;
pub mod pass_state;
pub mod refresh_tool;
pub mod repo_search;
pub mod ring_buffer;
pub mod service;
pub mod status;
pub mod symbol;
pub mod vector_health;
mod wire;

pub use access_guard::{maybe_guard, GuardOutcome, KnowledgeAccessGuard};
pub use ask::KnowledgeAskTool;
pub use delete::{drop_repo_everywhere, KnowledgeDeleteTool};
pub use explain::KnowledgeExplainTool;
pub use global_index::{
    resolve_query_repo, IndexMode, IndexTarget, MaintenanceRunner, PassReport, RecentFailure,
    RepoCatalog, RepoResolver,
};
pub use graph::KnowledgeGraphTool;
pub use impact::KnowledgeImpactTool;
pub use index::KnowledgeIndexTool;
pub use lineage_backfill::backfill_lineage;
pub use overview::KnowledgeOverviewTool;
pub use pass_state::{
    LeaseSnapshot, PassRunOutcome, PassScope, PassStateSnapshot, TargetQueueEntry, TargetState,
};
pub use refresh_tool::KnowledgeRefreshIndexTool;
pub use repo_search::{RepoGlobTool, RepoGrepTool, RepoListTool, RepoReadTool};
pub use ring_buffer::RingBuffer;
pub use service::HubSummariserBuilder;
pub use status::KnowledgeStatusTool;
pub use symbol::KnowledgeSymbolTool;
pub use vector_health::{
    log_state_transition as log_vector_health_transition, BackendHealthProbe, UnhealthyReason,
    VectorBackendHealth,
};
pub(crate) use wire::query_tools;
#[cfg(test)]
pub(crate) use wire::services;
pub use wire::{
    bind_evaluator, code_vector_breaker, code_vector_status, create_service, maintenance_runner,
    rebind_tools, register_tools, set_maintenance_runner, start_background, VectorUpkeep,
};
pub use workforce_knowledge_service::{
    FillPace, FillRun, KnowledgeService, RepoVectors, SummariserBuilder, VectorBackend,
};
