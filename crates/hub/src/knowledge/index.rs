use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use workforce_agent::tools::{Tool, ToolResult};

use super::{KnowledgeService, RepoResolver};

#[derive(Deserialize)]
struct Input {
    action: String,
    /// Repo or project to act on, by name (e.g. `"workforce"`); omit when only
    /// one repo is indexed. In single-agent setups this is a workspace-relative
    /// path. Accepts the legacy `path` key.
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

pub struct KnowledgeIndexTool {
    service: Arc<KnowledgeService>,
    /// The agent's workspace root, used to resolve a relative `repo` path in
    /// single-agent setups.
    agent_workspace: PathBuf,
    /// Canonical name → checkout resolver. `None` in single-agent setups,
    /// which act on the service's own root.
    resolver: Option<Arc<RepoResolver>>,
}

impl KnowledgeIndexTool {
    pub fn new(service: Arc<KnowledgeService>, agent_workspace: PathBuf) -> Self {
        Self {
            service,
            agent_workspace,
            resolver: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Resolve the directory to act on. With a canonical resolver (hub) `repo`
    /// names the repo and resolves to its canonical checkout; without one
    /// (single-agent) it is a workspace-relative path, defaulting to the
    /// service root.
    fn resolve_root(&self, repo: &Option<String>) -> Result<PathBuf, String> {
        if let Some(resolver) = &self.resolver {
            return resolver.resolve_dest(repo.as_deref());
        }
        match repo {
            Some(rel_path) => {
                let resolved = self.agent_workspace.join(rel_path);
                if !resolved.is_dir() {
                    return Err(format!(
                        "Path '{}' does not exist or is not a directory (resolved to {})",
                        rel_path,
                        resolved.display()
                    ));
                }
                std::fs::canonicalize(&resolved)
                    .map_err(|e| format!("Cannot resolve path '{}': {}", resolved.display(), e))
            }
            None => Ok(self.service.workspace().to_path_buf()),
        }
    }
}

#[async_trait]
impl Tool for KnowledgeIndexTool {
    fn name(&self) -> &str {
        "knowledge.index"
    }

    fn description(&self) -> &str {
        "Trigger or check codebase indexing status"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "trigger", "trigger_full", "cancel"],
                    "description": "'status' = check index state"
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to act on, by name. Omit when only one repo is indexed."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let root = match self.resolve_root(&input.repo) {
            Ok(r) => r,
            Err(msg) => return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64)),
        };

        let repo_id = KnowledgeService::repo_id_for(&root);

        match input.action.as_str() {
            "status" => {
                let head = self.service.get_head_commit_for(&root).ok();
                let completeness = self.service.completeness(&repo_id, head.as_deref()).await?;
                let action =
                    completeness.next_action(workforce_codebase_knowledge::lease::now_ms());
                let lease_json = completeness.lease.as_ref().map(|l| {
                    serde_json::json!({
                        "worker_id": l.worker_id,
                        "phase": l.phase.as_str(),
                        "acquired_at_ms": l.acquired_at_ms,
                        "last_heartbeat_at_ms": l.last_heartbeat_at_ms,
                        "entities_total": l.entities_total,
                        "entities_processed": l.entities_processed,
                        "summaries_total": l.summaries_total,
                        "summaries_processed": l.summaries_processed,
                    })
                });

                let output = serde_json::json!({
                    "repo_id": repo_id,
                    "repo_name": KnowledgeService::repo_name_for(&root),
                    "workspace": root.to_string_lossy(),
                    "indexed": completeness.indexed_commit.is_some(),
                    "is_current": completeness.is_current(),
                    "next_action": action.as_str(),
                    "head_commit": completeness.head_commit,
                    "indexed_commit": completeness.indexed_commit,
                    "last_full_index_at": completeness.last_full_index_at,
                    "last_incremental_at": completeness.last_incremental_at,
                    "total_entities": completeness.entities,
                    "vector_entities": completeness.vectors,
                    "summaries_expected": completeness.summaries_expected,
                    "summaries_present": completeness.summaries_present,
                    "summaries_stale": completeness.summaries_stale,
                    "summaries_missing": completeness.summaries_missing(),
                    "generate_summaries": completeness.generate_summaries,
                    "vector_enabled": self.service.vector_store().is_some(),
                    "lease": lease_json,
                });

                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }

            "trigger" => {
                // The smart dispatcher: consults `next_action()` and
                // either short-circuits (Complete / AlreadyIndexing) or
                // spawns the right pipeline mode in the background.
                let head = self.service.get_head_commit_for(&root).ok();
                let completeness = self.service.completeness(&repo_id, head.as_deref()).await?;
                let action =
                    completeness.next_action(workforce_codebase_knowledge::lease::now_ms());

                use workforce_codebase_knowledge::completeness::NextAction;
                match action {
                    NextAction::Complete => Ok(ToolResult::success(
                        serde_json::json!({
                            "status": "current",
                            "repo_id": repo_id,
                            "indexed_commit": completeness.indexed_commit,
                            "message": "Index is up to date."
                        }),
                        start.elapsed().as_millis() as u64,
                    )),
                    NextAction::AlreadyIndexing(l) => Ok(ToolResult::success(
                        serde_json::json!({
                            "status": "already_indexing",
                            "repo_id": repo_id,
                            "phase": l.phase.as_str(),
                            "worker_id": l.worker_id,
                            "message": "Indexing is already in progress. Use 'status' to check progress."
                        }),
                        start.elapsed().as_millis() as u64,
                    )),
                    NextAction::ResumeSummaries | NextAction::Incremental | NextAction::Full => {
                        let mode = action.as_str();
                        let svc = self.service.clone();
                        let root_clone = root.clone();
                        tokio::spawn(async move {
                            if let Err(e) = svc.trigger_for(&root_clone).await {
                                tracing::warn!(
                                    root = %root_clone.display(),
                                    "Background indexing failed: {}", e
                                );
                            }
                        });
                        Ok(ToolResult::success(
                            serde_json::json!({
                                "status": "started",
                                "mode": mode,
                                "repo_id": repo_id,
                                "workspace": root.to_string_lossy(),
                                "message": format!(
                                    "Indexing started in background ({mode}). Use 'status' to check progress."
                                ),
                            }),
                            start.elapsed().as_millis() as u64,
                        ))
                    }
                }
            }

            "trigger_full" => {
                // Check if already indexing
                if self.service.is_indexing_repo(&repo_id) {
                    return Ok(ToolResult::success(
                        serde_json::json!({
                            "status": "already_indexing",
                            "repo_id": repo_id,
                            "message": "Indexing is already in progress. Use 'status' to check progress."
                        }),
                        start.elapsed().as_millis() as u64,
                    ));
                }

                let svc = self.service.clone();
                let root_clone = root.clone();
                tokio::spawn(async move {
                    if let Err(e) = svc.trigger_full_index_for(&root_clone).await {
                        tracing::warn!(
                            root = %root_clone.display(),
                            "Background full indexing failed: {}", e
                        );
                    }
                });

                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": "started",
                        "mode": "full",
                        "repo_id": repo_id,
                        "workspace": root.to_string_lossy(),
                        "message": "Full re-index started in background. Use 'status' to check progress."
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "cancel" => {
                let was_indexing = self.service.cancel_indexing(&repo_id).await;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "repo_id": repo_id,
                        "was_indexing": was_indexing,
                        "message": if was_indexing {
                            "Indexing lock released. The background task may still run briefly."
                        } else {
                            "No indexing was in progress for this repo."
                        }
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            _ => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{}'. Valid actions: status, trigger, trigger_full, cancel",
                    input.action
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}
