//! Agent-callable tool for the v2 artifact knowledge base and decisions.
//!
//! Artifacts are stored through an [`ArtifactBackend`] (Postgres
//! `KnowledgeStore` shared across agents, or a per-agent file fallback) with
//! per-user ACL, tags, semantic search, and a `snippet` type. Decisions are
//! unchanged — still an append-only log in the agent's [`SharedWorkspace`].
//!
//! Owner / ACL identity: the caller key is the human `identity_id` from the
//! task-local `CURRENT_USER_ID` when present, else this tool's agent id.
//! `owner_id` is set to that key on create; reads/mutations are gated against
//! it in the backend.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::types::AgentId;
use workforce_orchestrator::artifact_store::{
    Artifact, ArtifactBackend, ArtifactFilter, ArtifactType, Caller, Visibility,
};
use workforce_orchestrator::workspace::{Decision, Dissent, SharedWorkspace};

/// Agent-callable tool for managing artifacts and decisions.
pub struct ArtifactTool {
    backend: Arc<dyn ArtifactBackend>,
    /// Decisions still live in the per-agent file workspace (out of scope
    /// for the DB migration).
    decisions: SharedWorkspace,
    agent_id: AgentId,
}

impl ArtifactTool {
    /// Build the tool over a shared/owned artifact backend. `workspace_dir`
    /// backs the (unchanged) decision log only.
    pub fn new(
        backend: Arc<dyn ArtifactBackend>,
        workspace_dir: PathBuf,
        agent_id: AgentId,
    ) -> Result<Self> {
        let decisions = SharedWorkspace::create(&workspace_dir)?;
        Ok(Self {
            backend,
            decisions,
            agent_id,
        })
    }

    /// Resolve the ACL principal for this call: the human `identity_id` if a
    /// user is in the loop, otherwise this agent's id.
    fn caller(&self) -> Caller {
        let identity = workforce_agent::runner::CURRENT_USER_ID
            .try_with(|id| id.clone())
            .ok()
            .filter(|s| !s.is_empty());
        Caller::user(identity.unwrap_or_else(|| self.agent_id.as_str().to_string()))
    }
}

#[derive(Deserialize)]
struct ArtifactInput {
    action: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    artifact_type: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    query: Option<String>,
    /// Grant read access to this user id (`artifact.share`).
    #[serde(default)]
    share_with: Option<String>,
    // Optional list filters.
    #[serde(default)]
    filter_type: Option<String>,
    #[serde(default)]
    filter_tag: Option<String>,
    // Decision fields.
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    approved_by: Option<String>,
    #[serde(default)]
    dissent_agent: Option<String>,
    #[serde(default)]
    dissent_position: Option<String>,
}

fn artifact_json(a: &Artifact) -> serde_json::Value {
    serde_json::json!({
        "id": a.id,
        "title": a.title,
        "artifact_type": a.artifact_type.to_string(),
        "visibility": a.visibility.to_string(),
        "tags": a.tags,
        "status": a.status.to_string(),
        "version": a.version,
        "owner_id": a.owner_id,
        "created_by": a.created_by,
        "last_modified_by": a.last_modified_by,
    })
}

#[async_trait]
impl Tool for ArtifactTool {
    fn name(&self) -> &str {
        "artifact"
    }

    fn description(&self) -> &str {
        "Manage knowledge-base artifacts: create, read, update, list, search, \
         finalize, share, delete; plus record/list team decisions"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "artifact.create", "artifact.read", "artifact.update",
                        "artifact.list", "artifact.search", "artifact.finalize",
                        "artifact.share", "artifact.delete",
                        "decision.record", "decision.list"
                    ],
                    "description": "Action to perform"
                },
                "id": { "type": "string", "description": "Artifact or decision ID" },
                "title": { "type": "string", "description": "Title for new artifact" },
                "content": { "type": "string", "description": "Markdown content for artifact" },
                "artifact_type": {
                    "type": "string",
                    "enum": ["spec", "ticket", "architecture", "notes", "decision", "snippet"],
                    "description": "Type of artifact (create)"
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "public"],
                    "description": "Visibility scope (create / share). Default private."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Manual tags (merged with auto-tags), lowercased + de-duped"
                },
                "query": { "type": "string", "description": "Search query (artifact.search)" },
                "share_with": {
                    "type": "string",
                    "description": "Grant a specific user id read access (artifact.share)"
                },
                "filter_type": {
                    "type": "string",
                    "description": "Filter list/search by artifact type"
                },
                "filter_tag": {
                    "type": "string",
                    "description": "Filter list/search by tag"
                },
                "summary": { "type": "string", "description": "Decision summary" },
                "rationale": { "type": "string", "description": "Decision rationale" },
                "approved_by": { "type": "string", "description": "Agent ID who approved" },
                "dissent_agent": { "type": "string", "description": "Agent ID of dissenter" },
                "dissent_position": { "type": "string", "description": "Dissenting position text" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let input: ArtifactInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid artifact input: {}", e))?;
        let caller = self.caller();

        macro_rules! require_id {
            ($what:literal) => {
                match input.id.as_deref() {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok(ToolResult::failure(concat!("Missing 'id' — ", $what), ms())),
                }
            };
        }

        match input.action.as_str() {
            "artifact.create" => {
                let id = match input.id.as_deref() {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'id' — provide a unique artifact identifier",
                            ms(),
                        ))
                    }
                };
                let title = input.title.clone().unwrap_or_else(|| id.clone());
                let content = input.content.clone().unwrap_or_default();
                let artifact_type: ArtifactType = input
                    .artifact_type
                    .as_deref()
                    .unwrap_or("notes")
                    .parse()
                    .unwrap();
                let visibility: Visibility = input
                    .visibility
                    .as_deref()
                    .unwrap_or("private")
                    .parse()
                    .unwrap();

                let artifact = Artifact::new(
                    id.clone(),
                    artifact_type,
                    title,
                    content,
                    caller.key.clone(),
                    self.agent_id.as_str(),
                    visibility,
                    input.tags.clone().unwrap_or_default(),
                );

                match self.backend.create(&artifact).await {
                    Ok(()) => Ok(ToolResult::success(
                        serde_json::json!({
                            "action": "artifact.create",
                            "id": id,
                            "visibility": artifact.visibility.to_string(),
                            "status": "created"
                        }),
                        ms(),
                    )),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "artifact.read" => {
                let id = require_id!("which artifact to read?");
                match self.backend.read(id, &caller).await {
                    Ok(Some(a)) => {
                        let mut out = artifact_json(&a);
                        out["action"] = "artifact.read".into();
                        out["content"] = a.content.clone().into();
                        Ok(ToolResult::success(out, ms()))
                    }
                    Ok(None) => Ok(ToolResult::failure(
                        format!("Artifact '{id}' not found"),
                        ms(),
                    )),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "artifact.update" => {
                let id = require_id!("which artifact to update?");
                let content = match input.content.as_deref() {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — what to update the artifact with?",
                            ms(),
                        ))
                    }
                };
                match self
                    .backend
                    .update(id, content, self.agent_id.as_str(), &caller)
                    .await
                {
                    Ok(a) => Ok(ToolResult::success(
                        serde_json::json!({
                            "action": "artifact.update",
                            "id": id,
                            "version": a.version,
                            "status": "updated"
                        }),
                        ms(),
                    )),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "artifact.list" => {
                let filter = ArtifactFilter {
                    artifact_type: input
                        .filter_type
                        .as_deref()
                        .map(|s| s.parse::<ArtifactType>().unwrap()),
                    visibility: None,
                    tag: input.filter_tag.clone(),
                };
                match self.backend.list(&caller, &filter).await {
                    Ok(items) => Ok(ToolResult::success(
                        serde_json::json!({
                            "action": "artifact.list",
                            "count": items.len(),
                            "artifacts": items.iter().map(artifact_json).collect::<Vec<_>>(),
                        }),
                        ms(),
                    )),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "artifact.search" => {
                let query = match input.query.as_deref().or(input.content.as_deref()) {
                    Some(q) if !q.is_empty() => q,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'query' — what to search for?",
                            ms(),
                        ))
                    }
                };
                let filter = ArtifactFilter {
                    artifact_type: input
                        .filter_type
                        .as_deref()
                        .map(|s| s.parse::<ArtifactType>().unwrap()),
                    visibility: None,
                    tag: input.filter_tag.clone(),
                };
                match self.backend.search_told(query, &caller, &filter, 20).await {
                    Ok(searched) => Ok(ToolResult::success(searched_json(&searched), ms())),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "artifact.finalize" => {
                let id = require_id!("which artifact to finalize?");
                match self.backend.finalize(id, &caller).await {
                    Ok(_) => Ok(ToolResult::success(
                        serde_json::json!({
                            "action": "artifact.finalize",
                            "id": id,
                            "status": "finalized"
                        }),
                        ms(),
                    )),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "artifact.share" => {
                let id = require_id!("which artifact to share?");
                // Set visibility if supplied.
                if let Some(vis) = input.visibility.as_deref() {
                    let visibility: Visibility = vis.parse().unwrap();
                    if let Err(e) = self.backend.set_visibility(id, visibility, &caller).await {
                        return Ok(ToolResult::failure(e.to_string(), ms()));
                    }
                }
                // Add a per-user grant if supplied.
                if let Some(user) = input.share_with.as_deref().filter(|s| !s.is_empty()) {
                    if let Err(e) = self.backend.share_with(id, user, &caller).await {
                        return Ok(ToolResult::failure(e.to_string(), ms()));
                    }
                }
                if input.visibility.is_none() && input.share_with.is_none() {
                    return Ok(ToolResult::failure(
                        "Provide 'visibility' and/or 'share_with' to share",
                        ms(),
                    ));
                }
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "artifact.share",
                        "id": id,
                        "status": "shared"
                    }),
                    ms(),
                ))
            }

            "artifact.delete" => {
                let id = require_id!("which artifact to delete?");
                match self.backend.delete(id, &caller).await {
                    Ok(()) => Ok(ToolResult::success(
                        serde_json::json!({
                            "action": "artifact.delete",
                            "id": id,
                            "status": "deleted"
                        }),
                        ms(),
                    )),
                    Err(e) => Ok(ToolResult::failure(e.to_string(), ms())),
                }
            }

            "decision.record" => {
                let id = match input.id.as_deref() {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => uuid::Uuid::new_v4().to_string(),
                };
                let summary = match input.summary.as_deref() {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'summary' — what was decided?",
                            ms(),
                        ))
                    }
                };
                let rationale = input.rationale.clone().unwrap_or_default();
                let mut dissent = Vec::new();
                if let (Some(agent), Some(position)) =
                    (&input.dissent_agent, &input.dissent_position)
                {
                    dissent.push(Dissent {
                        agent_id: AgentId::new(agent),
                        position: position.clone(),
                    });
                }
                let decision = Decision {
                    id: id.clone(),
                    summary,
                    rationale,
                    proposed_by: self.agent_id.clone(),
                    approved_by: input.approved_by.as_deref().map(AgentId::new),
                    dissent,
                    created_at: chrono::Utc::now(),
                };
                self.decisions.record_decision(&decision)?;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "decision.record",
                        "id": id,
                        "status": "recorded"
                    }),
                    ms(),
                ))
            }

            "decision.list" => {
                let decisions = self.decisions.list_decisions()?;
                let items: Vec<serde_json::Value> = decisions
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "summary": d.summary,
                            "proposed_by": d.proposed_by.as_str(),
                            "approved_by": d.approved_by.as_ref().map(|a| a.as_str()),
                            "dissent_count": d.dissent.len(),
                        })
                    })
                    .collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "decision.list",
                        "count": items.len(),
                        "decisions": items
                    }),
                    ms(),
                ))
            }

            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action: '{other}'. Use: artifact.create, artifact.read, \
                     artifact.update, artifact.list, artifact.search, artifact.finalize, \
                     artifact.share, artifact.delete, decision.record, decision.list"
                ),
                ms(),
            )),
        }
    }
}

/// What `artifact.search` answers: the hits, and how it searched. By
/// meaning, ranked by the index, which may not yet hold the artifacts
/// written last (`not_indexed` of them, which it could not have found, and
/// the answer says so); or by keyword, with why the index was not used.
fn searched_json(searched: &workforce_orchestrator::artifact_store::Searched) -> serde_json::Value {
    let mut answer = serde_json::json!({
        "action": "artifact.search",
        "count": searched.hits.len(),
        "searched_by": match searched.by_keyword {
            None => "meaning",
            Some(_) => "keyword",
        },
        "index_skipped": searched.by_keyword.as_ref().map(ToString::to_string),
        "results": searched.hits.iter().map(|h| {
            let mut j = artifact_json(&h.artifact);
            j["score"] = h.score.into();
            j
        }).collect::<Vec<_>>(),
    });
    if searched.by_keyword.is_none() && searched.not_indexed > 0 {
        let n = searched.not_indexed;
        answer["not_indexed"] = n.into();
        answer["note"] = format!(
            "searched by meaning over every artifact but at least {n} not yet indexed as they are stored, which this search could not find; a search by keyword finds them"
        )
        .into();
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_orchestrator::artifact_store::FsArtifactBackend;

    /// A search by the index says how many artifacts it could not have
    /// found, and does not call itself a search over everything; one with
    /// nothing waiting, or by keyword, says nothing of it.
    #[test]
    fn a_search_by_meaning_says_what_it_could_not_find() {
        use workforce_orchestrator::artifact_store::{Searched, Skipped};
        let by_meaning = |not_indexed| Searched {
            hits: Vec::new(),
            by_keyword: None,
            not_indexed,
        };
        let told = searched_json(&by_meaning(3));
        assert_eq!(told["searched_by"], "meaning");
        assert_eq!(told["not_indexed"], 3);
        let note = told["note"].as_str().unwrap();
        assert!(
            note.contains("but at least 3 not yet indexed") && note.contains("could not find"),
            "{note}"
        );
        let told = searched_json(&by_meaning(0));
        assert!(
            told.get("not_indexed").is_none() && told.get("note").is_none(),
            "{told}"
        );
        let by_keyword = Searched {
            hits: Vec::new(),
            by_keyword: Some(Skipped::Behind {
                pending: 30,
                stored: 100,
            }),
            not_indexed: 0,
        };
        let told = searched_json(&by_keyword);
        assert_eq!(told["searched_by"], "keyword");
        assert!(told["index_skipped"]
            .as_str()
            .unwrap()
            .contains("30 of the 100"));
        assert!(told.get("note").is_none());
    }

    fn make_tool() -> (tempfile::TempDir, ArtifactTool) {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = Arc::new(FsArtifactBackend::create_at(tmp.path()).unwrap());
        let tool = ArtifactTool::new(
            backend,
            tmp.path().to_path_buf(),
            AgentId::new("test-agent"),
        )
        .unwrap();
        (tmp, tool)
    }

    #[tokio::test]
    async fn create_read_update_finalize_roundtrip() {
        let (_tmp, tool) = make_tool();

        let r = tool
            .execute(serde_json::json!({
                "action": "artifact.create",
                "id": "spec-001",
                "title": "Test Spec",
                "content": "# Overview",
                "artifact_type": "snippet",
                "visibility": "public",
                "tags": ["Rust", "rust"]
            }))
            .await
            .unwrap();
        assert!(r.success, "create failed: {:?}", r.output);
        assert_eq!(r.output["visibility"], "public");

        let r = tool
            .execute(serde_json::json!({"action": "artifact.read", "id": "spec-001"}))
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.output["artifact_type"], "snippet");
        assert_eq!(r.output["tags"], serde_json::json!(["rust"]));

        let r = tool
            .execute(serde_json::json!({
                "action": "artifact.update", "id": "spec-001", "content": "v2"
            }))
            .await
            .unwrap();
        assert_eq!(r.output["version"], 2);

        let r = tool
            .execute(serde_json::json!({"action": "artifact.finalize", "id": "spec-001"}))
            .await
            .unwrap();
        assert!(r.success);
    }

    #[tokio::test]
    async fn search_and_delete() {
        let (_tmp, tool) = make_tool();
        tool.execute(serde_json::json!({
            "action": "artifact.create", "id": "a1", "content": "about databases and rust"
        }))
        .await
        .unwrap();

        let r = tool
            .execute(serde_json::json!({"action": "artifact.search", "query": "databases"}))
            .await
            .unwrap();
        assert_eq!(r.output["count"], 1);

        let r = tool
            .execute(serde_json::json!({"action": "artifact.delete", "id": "a1"}))
            .await
            .unwrap();
        assert!(r.success);

        let r = tool
            .execute(serde_json::json!({"action": "artifact.read", "id": "a1"}))
            .await
            .unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn decisions_still_work() {
        let (_tmp, tool) = make_tool();
        let r = tool
            .execute(serde_json::json!({
                "action": "decision.record", "id": "d1",
                "summary": "Use Rust", "approved_by": "lead"
            }))
            .await
            .unwrap();
        assert!(r.success);
        let r = tool
            .execute(serde_json::json!({"action": "decision.list"}))
            .await
            .unwrap();
        assert_eq!(r.output["count"], 1);
    }
}
