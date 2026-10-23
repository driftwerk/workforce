use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use workforce_agent::tools::{Tool, ToolResult};

use super::access_guard::{maybe_guard, KnowledgeAccessGuard};
use super::{repo_search, resolve_query_repo, KnowledgeService, RepoResolver};

#[derive(Deserialize)]
struct Input {
    question: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default = "default_depth")]
    depth: String,
    #[serde(default, alias = "path")]
    repo: Option<String>,
}

fn default_depth() -> String {
    "quick".to_string()
}

pub struct KnowledgeAskTool {
    service: Arc<KnowledgeService>,
    agent_workspace: PathBuf,
    /// Phase-9 cascade gate. `None` when the operator hasn't enabled
    /// the permission cascade (`[permissions] enabled = false`); the
    /// tool falls through to PolicyEngine's gate just like every other
    /// tool. When wired, every invocation goes through the cascade
    /// against the active `CURRENT_USER_ID` and a `Deny` short-circuits
    /// the call.
    access_guard: Option<KnowledgeAccessGuard>,
    /// Canonical name → `repo_id` resolver. `None` in single-agent setups,
    /// which query the service's own root.
    resolver: Option<Arc<RepoResolver>>,
}

impl KnowledgeAskTool {
    pub fn new(service: Arc<KnowledgeService>, agent_workspace: PathBuf) -> Self {
        Self {
            service,
            agent_workspace,
            access_guard: None,
            resolver: None,
        }
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }
}

#[async_trait]
impl Tool for KnowledgeAskTool {
    fn name(&self) -> &str {
        "knowledge.ask"
    }

    fn description(&self) -> &str {
        "Ask natural language questions about the codebase"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "Natural language question about the codebase"
                },
                "scope": {
                    "type": "string",
                    "description": "Optional directory path to limit the search scope"
                },
                "depth": {
                    "type": "string",
                    "enum": ["quick", "thorough"],
                    "description": "Search depth"
                },
                "repo": {
                    "type": "string",
                    "description": "Repo or project to query, by name (e.g. \"workforce\"). Omit when only one repo is indexed."
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: Input =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }

        let thorough = input.depth == "thorough";
        let repo_id = match resolve_query_repo(
            &self.service,
            &self.agent_workspace,
            self.resolver.as_deref(),
            &input.repo,
        ) {
            Ok(id) => id,
            Err(msg) => return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64)),
        };

        match self
            .service
            .query_engine()
            .ask(&repo_id, &input.question, input.scope.as_deref(), thorough)
            .await
        {
            Ok(mut result) => {
                // The answer is often in file CONTENT the symbol graph can't
                // see (prose, naming conventions, docs/specs, config, comments).
                // Point the agent at live grep — `repo.grep` reaches the indexed
                // repos, unlike the old "use code.search" dead-end.
                //
                // Two shapes, both real failures: (a) the graph matched nothing
                // (`sources` empty) → full redirect; (b) it matched weak/
                // irrelevant symbols with low confidence (the original ESR case:
                // it confidently returned "grade positions, GraphQL types" — non-
                // empty but wrong) → APPEND a conditional pointer without
                // claiming nothing matched. `< 0.6` is forgiving on purpose: the
                // phrasing ("if these don't answer it") makes a stray fire
                // harmless, and missing the weak-hit case would skip the very
                // shape that motivated this.
                let repo_hint = input.repo.as_deref().unwrap_or("<repo>");
                if result.sources.is_empty() {
                    result.suggestion = Some(format!(
                        "No code symbols matched. The answer may live in file content the symbol \
                         index can't see (prose, naming conventions, docs/specs, config, \
                         comments). Search it directly: `repo.grep repo=\"{repo_hint}\" \
                         pattern=\"...\"`, then `repo.read` the file the hit is in. Use \
                         `repo.list` to see indexed repos."
                    ));
                } else if result.confidence < 0.6 {
                    let pointer = format!(
                        "If these symbols don't actually answer it, the detail may be in file \
                         content the symbol index can't see (prose, conventions, docs/specs, \
                         config) — search it with `repo.grep repo=\"{repo_hint}\" pattern=\"...\"`, \
                         then `repo.read` the hit."
                    );
                    result.suggestion = Some(match result.suggestion.take() {
                        Some(existing) => format!("{existing} {pointer}"),
                        None => pointer,
                    });
                }
                // Always surface the repo's authoritative docs (specs/READMEs,
                // PDFs included). The symbol graph can't see prose, so a
                // confident-but-shallow code answer is exactly when nothing else
                // points the agent at the doc that actually states the intent.
                // Whole-repo scope here (the query isn't tied to a subtree); the
                // grep path scopes to the hits it found.
                if let Some(manifest) = repo_search::nearby_docs_manifest(
                    self.resolver.as_ref(),
                    &self.agent_workspace,
                    &input.repo,
                    None,
                )
                .await
                {
                    result.suggestion = Some(match result.suggestion.take() {
                        Some(existing) => format!("{existing}\n\n{manifest}"),
                        None => manifest,
                    });
                }
                Ok(ToolResult::success(
                    serde_json::to_value(result)?,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Knowledge query failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}
