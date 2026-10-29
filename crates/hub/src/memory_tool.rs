use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{
    Memory, MemoryFilter, MemoryId, MemoryLayer, MemorySource, Visibility,
};

use crate::lessons::{
    APPLIED_TAG_PREFIX, CONFIDENCE_TAG_PREFIX, DATE_TAG_PREFIX, SOURCE_TAG, TRIGGER_TAG_PREFIX,
};
use crate::memory::MemoryManager;

fn default_max_results() -> usize {
    10
}

// ─── Memory Tool (remember / recall / forget / search) ──────────────────────

/// Full memory management tool — lets agents store, recall, and forget memories
/// in the vector store (Weaviate or in-memory).
pub struct MemoryTool {
    store: Arc<dyn MemoryStore>,
    agent_id: AgentId,
    workspace: PathBuf,
    /// Session store for `fetch_source` transcript reads. Injected by
    /// production callers (via [`Self::with_sessions`]) so the configured
    /// backend is honoured; `None` (test/local `new`) falls back to a JSONL
    /// store on the workspace.
    sessions: Option<Arc<dyn workforce_common::session::SessionStore>>,
    decay_rate: f64,
    team_id: Option<String>,
    min_certainty: Option<f32>,
}

impl MemoryTool {
    pub fn new(store: Arc<dyn MemoryStore>, agent_id: AgentId, workspace: PathBuf) -> Self {
        Self {
            store,
            agent_id,
            workspace,
            sessions: None,
            decay_rate: 0.005,
            team_id: None,
            min_certainty: None,
        }
    }

    /// Inject the agent's session store so `fetch_source` honours the
    /// configured (JSONL or Postgres) backend instead of constructing its own.
    pub fn with_sessions(
        mut self,
        sessions: Arc<dyn workforce_common::session::SessionStore>,
    ) -> Self {
        self.sessions = Some(sessions);
        self
    }

    pub fn with_decay_rate(mut self, decay_rate: f64) -> Self {
        self.decay_rate = decay_rate;
        self
    }

    pub fn with_team_id(mut self, team_id: impl Into<String>) -> Self {
        self.team_id = Some(team_id.into());
        self
    }

    /// Set the minimum certainty (cosine similarity) for `memory recall`
    /// results. `None` returns the top-N regardless of relevance.
    pub fn with_min_certainty(mut self, min_certainty: Option<f32>) -> Self {
        self.min_certainty = min_certainty;
        self
    }
}

#[derive(Deserialize)]
struct MemoryInput {
    action: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    memory_id: Option<String>,
    #[serde(default)]
    layer: Option<String>,
    #[serde(default)]
    importance: Option<f32>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default)]
    source: Option<String>,
    /// Required when `source = "lesson"`: 1-5 short kebab-case trigger
    /// handles. Ignored for any other source.
    #[serde(default)]
    trigger_tags: Option<Vec<String>>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    /// Opt in to including memories that the audit pass tagged as polluted
    /// (recap artefacts, consolidation self-reports). Off by default.
    #[serde(default)]
    include_polluted: bool,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Store, recall, search, and manage agent memories"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["remember", "recall", "forget", "count", "overview", "timeline", "pin", "unpin", "fetch_source"],
                    "description": "Action to perform. `fetch_source` returns the transcript passage that produced a recalled memory, if it has a source link."
                },
                "content": {
                    "type": "string",
                    "description": "Memory content to store (for remember)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query for semantic recall (for recall)"
                },
                "memory_id": {
                    "type": "string",
                    "description": "Memory ID to forget (for forget)"
                },
                "layer": {
                    "type": "string",
                    "enum": ["working", "episodic", "semantic", "team", "organisation"],
                    "description": "Memory layer (default: episodic)"
                },
                "importance": {
                    "type": "number",
                    "description": "Importance score 0.0-1.0 (default: 0.5)"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Metadata tags for the memory (for remember"
                },
                "source": {
                    "type": "string",
                    "enum": ["human", "technical", "curated", "compaction", "agent", "passive", "lesson", "unknown"],
                    "description": "Memory source. For `remember`, choose `lesson` to store a behaviour-shaping note (requires `trigger_tags` and content starting with When/If/Whenever). For `recall`, filters by source."
                },
                "trigger_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Required when `source=lesson` on `remember`. 1-5 short kebab-case handles describing when this lesson applies (e.g. `bedrock-provider-switch`)."
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "global"],
                    "description": "Who can see this memory (default: private)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results for recall (default: 10)"
                },
                "from": {
                    "type": "string",
                    "description": "Start date for timeline (YYYY-MM-DD, default: 7 days ago)"
                },
                "to": {
                    "type": "string",
                    "description": "End date for timeline (YYYY-MM-DD, default: today)"
                },
                "scope": {
                    "type": "string",
                    "enum": ["me", "all"],
                    "description": "Pin scope"
                },
                "include_polluted": {
                    "type": "boolean",
                    "description": "Include memories the audit pass tagged as recap artefacts or consolidation self-reports. Default false; turn on only for forensic queries."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: MemoryInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                let hint = if msg.contains("expected a sequence") {
                    "tags must be a JSON array like [\"tag1\", \"tag2\"], not a string"
                } else if msg.contains("expected f") || msg.contains("expected a float") {
                    "importance must be a number like 0.8, not a string like \"0.8\""
                } else {
                    &msg
                };
                return Ok(ToolResult::failure(
                    format!("Invalid input: {}", hint),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match input.action.as_str() {
            "remember" => self.handle_remember(input, start).await,
            "recall" => self.handle_recall(input, start).await,
            "forget" => self.handle_forget(input, start).await,
            "count" => self.handle_count(input, start).await,
            "overview" => self.handle_overview(start).await,
            "timeline" => self.handle_timeline(input, start).await,
            "pin" => self.handle_pin(input, start).await,
            "unpin" => self.handle_unpin(input, start).await,
            "fetch_source" => self.handle_fetch_source(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{other}'. Use: remember, recall, forget, count, overview, timeline, pin, unpin, fetch_source"
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Failure result for the `MemoryTool` handlers when no `CURRENT_USER_ID`
/// is in scope. Centralises the message and timing so every handler
/// rejects the same way (DW-518 / DW-520). `action` is the public tool
/// action (`recall`, `pin`, …) for diagnostics.
fn unscoped_identity_failure(action: &str, start: Instant) -> ToolResult {
    ToolResult::failure(
        format!("memory.{action} requires a user identity in scope; refusing to run unscoped"),
        start.elapsed().as_millis() as u64,
    )
}

impl MemoryTool {
    async fn handle_remember(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let content = match input.content {
            Some(c) if !c.is_empty() => c,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'content' — what should be remembered?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("remember", start));
        };

        let layer = input
            .layer
            .as_deref()
            .and_then(parse_layer)
            .unwrap_or(MemoryLayer::Episodic);

        let mut visibility = input
            .visibility
            .as_deref()
            .map(parse_visibility)
            .unwrap_or(Visibility::Private);

        if let Visibility::Team(ref mut tid) = visibility {
            if tid == "default" {
                if let Some(ref real_tid) = self.team_id {
                    *tid = real_tid.clone();
                }
            }
        }

        let source = input
            .source
            .as_deref()
            .map(MemorySource::from_str_lossy)
            .unwrap_or(MemorySource::Agent);

        let (layer, tags) = if matches!(source, MemorySource::Lesson) {
            if !is_trigger_prefixed(&content) {
                return Ok(ToolResult::failure(
                    "Lesson content must start with 'When ', 'If ', or 'Whenever '",
                    start.elapsed().as_millis() as u64,
                ));
            }
            let triggers = input
                .trigger_tags
                .as_ref()
                .filter(|t| !t.is_empty())
                .cloned();
            let Some(triggers) = triggers else {
                return Ok(ToolResult::failure(
                    "Lesson requires non-empty 'trigger_tags' array",
                    start.elapsed().as_millis() as u64,
                ));
            };
            let importance = input.importance.unwrap_or(0.6).clamp(0.0, 1.0);
            let now = chrono::Utc::now();
            let date = now.format("%Y-%m-%d").to_string();
            let mut tags = input.tags.unwrap_or_default();
            tags.push(SOURCE_TAG.to_string());
            for t in triggers.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                tags.push(format!("{TRIGGER_TAG_PREFIX}{t}"));
            }
            tags.push(format!("{CONFIDENCE_TAG_PREFIX}{importance:.2}"));
            tags.push(format!("{APPLIED_TAG_PREFIX}0"));
            tags.push(format!("{DATE_TAG_PREFIX}{date}"));
            (MemoryLayer::Semantic, Some(tags))
        } else {
            (layer, input.tags)
        };

        let mut memory = Memory::new(content.clone(), layer.clone(), self.agent_id.clone())
            .with_visibility(visibility)
            .with_identity_id(identity_id)
            .with_source(source);

        if let Some(importance) = input.importance {
            memory = memory.with_importance(importance);
        }

        if let Some(tags) = tags {
            memory = memory.with_tags(tags);
        }

        let id = self.store.remember(memory).await?;

        Ok(ToolResult::success(
            serde_json::json!({
                "memory_id": id.0,
                "content": content,
                "layer": format!("{:?}", layer).to_lowercase(),
                "source": source.as_str(),
                "status": "stored"
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_recall(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let query = match input.query {
            Some(q) if !q.is_empty() => q,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'query' — what are you looking for?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("recall", start));
        };

        let mut all_results: Vec<serde_json::Value> = Vec::new();

        // File-based search (daily notes, identity files)
        // Only include when no layer filter is specified, since file
        // results carry no layer metadata and would pollute filtered queries.
        if input.layer.is_none() {
            let mgr = MemoryManager::new(&self.workspace);
            if let Ok(file_results) = mgr.search(&query) {
                for r in file_results {
                    all_results.push(serde_json::json!({
                        "result_source": "file",
                        "file": r.file,
                        "snippet": r.snippet,
                        "score": r.score,
                    }));
                }
            }
        }

        let layer = input.layer.as_deref().and_then(parse_layer);
        let source = input
            .source
            .as_deref()
            .map(workforce_memory::types::MemorySource::from_str_lossy);

        let filter = MemoryFilter {
            owner: Some(self.agent_id.clone()),
            layer,
            source,
            tags: input.tags,
            include_polluted: input.include_polluted,
            identity_id: Some(identity_id),
            team_id: self.team_id.clone(),
            limit: Some(input.max_results),
            min_certainty: self.min_certainty,
            ..Default::default()
        };

        match self.store.recall(&query, filter).await {
            Ok(results) => {
                for r in results {
                    all_results.push(serde_json::json!({
                        "result_source": "vector",
                        "memory_id": r.memory.id.0,
                        "content": r.memory.content,
                        "layer": format!("{:?}", r.memory.layer).to_lowercase(),
                        "source": r.memory.source.as_str(),
                        "importance": r.memory.importance,
                        "tags": r.memory.tags,
                        "score": r.score,
                        "created_at": r.memory.created_at.to_rfc3339(),
                        "age": r.memory.age_label(),
                        "freshness": r.memory.recency_score(self.decay_rate),
                    }));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Vector recall failed, using file results only");
                all_results.push(serde_json::json!({
                    "result_source": "system",
                    "warning": "Vector memory store temporarily unavailable — results may be incomplete",
                }));
            }
        }

        all_results.sort_by(|a, b| {
            let sa = a["score"].as_f64().unwrap_or(0.0);
            let sb = b["score"].as_f64().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        all_results.truncate(input.max_results);

        let count = all_results.len();

        Ok(ToolResult::success(
            serde_json::json!({
                "results": all_results,
                "count": count,
                "query": query,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_forget(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let memory_id = match input.memory_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'memory_id' — which memory to forget?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Ownership check: only allow deleting your own or unscoped memories
        if let Some(ref current_uid) = crate::user_config::current_identity_id() {
            if let Ok(Some(mem)) = self.store.get(&MemoryId(memory_id.clone())).await {
                if let Some(ref mem_uid) = mem.identity_id {
                    if mem_uid != current_uid {
                        return Ok(ToolResult::failure(
                            "Cannot forget a memory owned by another user",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
            }
        }

        self.store.forget(&MemoryId(memory_id.clone())).await?;

        Ok(ToolResult::success(
            serde_json::json!({
                "memory_id": memory_id,
                "status": "forgotten"
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_count(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let layer = input.layer.as_deref().and_then(parse_layer);
        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("count", start));
        };

        let filter = MemoryFilter {
            owner: Some(self.agent_id.clone()),
            layer,
            identity_id: Some(identity_id),
            team_id: self.team_id.clone(),
            ..Default::default()
        };

        let count = self.store.count(filter).await?;

        Ok(ToolResult::success(
            serde_json::json!({
                "count": count,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_overview(&self, start: Instant) -> Result<ToolResult> {
        let Some(uid) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("overview", start));
        };
        let identity_id = Some(uid);
        let team_id = self.team_id.clone();

        let semantic_filter = MemoryFilter {
            owner: Some(self.agent_id.clone()),
            layer: Some(MemoryLayer::Semantic),
            min_importance: Some(0.3),
            identity_id: identity_id.clone(),
            team_id: team_id.clone(),
            limit: Some(10),
            ..Default::default()
        };
        let semantic = self
            .store
            .recall("knowledge facts", semantic_filter)
            .await
            .unwrap_or_default();

        let episodic_filter = MemoryFilter {
            owner: Some(self.agent_id.clone()),
            layer: Some(MemoryLayer::Episodic),
            identity_id,
            team_id,
            limit: Some(10),
            ..Default::default()
        };
        let episodic = self
            .store
            .recall("recent activity", episodic_filter)
            .await
            .unwrap_or_default();

        let long_term: Vec<serde_json::Value> = semantic
            .iter()
            .map(|r| {
                serde_json::json!({
                    "content": r.memory.content,
                    "importance": r.memory.importance,
                    "age": r.memory.age_label(),
                })
            })
            .collect();

        let recent: Vec<serde_json::Value> = episodic
            .iter()
            .map(|r| {
                serde_json::json!({
                    "content": r.memory.content,
                    "date": r.memory.created_at.format("%Y-%m-%d").to_string(),
                    "age": r.memory.age_label(),
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "long_term": long_term,
                "long_term_count": long_term.len(),
                "recent": recent,
                "recent_count": recent.len(),
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_timeline(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();

        let from = input.from.unwrap_or(week_ago);
        let to = input.to.unwrap_or(today);

        // Filter on `event_time` (when the depicted event happened) rather
        // than `created_at` (when the memory was written). For live turns the
        // two are equal, but a session-scanner backfill can write today a
        // memory whose event_time is last week — using created_at there
        // would lie about when the event happened.
        let event_after = chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc));
        let event_before = chrono::NaiveDate::parse_from_str(&to, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(23, 59, 59))
            .map(|dt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc));

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("timeline", start));
        };
        let filter = MemoryFilter {
            owner: Some(self.agent_id.clone()),
            layer: Some(MemoryLayer::Episodic),
            identity_id: Some(identity_id),
            team_id: self.team_id.clone(),
            event_after,
            event_before,
            include_polluted: input.include_polluted,
            limit: Some(50),
            ..Default::default()
        };

        let query = format!("activity between {} and {}", from, to);
        let results = self.store.recall(&query, filter).await.unwrap_or_default();

        let entries: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "date": r.memory.event_time.format("%Y-%m-%d %H:%M").to_string(),
                    "content": r.memory.content,
                    "source": r.memory.source.as_str(),
                    "recurrence_count": r.memory.recurrence_count,
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "from": from,
                "to": to,
                "entries": entries,
                "count": entries.len(),
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_pin(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let duration = || start.elapsed().as_millis() as u64;

        if input.scope.as_deref() == Some("all") {
            return Ok(ToolResult::failure(
                "Global (scope=\"all\") pins are not available from agent tools — \
                 use the admin pin API. This limit prevents prompt-injected payloads \
                 from becoming persistent across all users (DW-520).",
                duration(),
            ));
        }

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("pin", start));
        };

        if let Some(ref id) = input.memory_id {
            let mem = self.store.get(&MemoryId(id.clone())).await?;
            let Some(mem) = mem else {
                return Ok(ToolResult::failure(
                    format!("Memory '{id}' not found"),
                    duration(),
                ));
            };
            if mem.identity_id.as_deref() != Some(identity_id.as_str()) {
                return Ok(ToolResult::failure(
                    "Cannot pin a memory owned by another user",
                    duration(),
                ));
            }
            let mut tags = mem.tags.clone();
            if !tags.contains(&"pinned".to_string()) {
                tags.push("pinned".to_string());
            }
            self.store.update_tags(&MemoryId(id.clone()), tags).await?;
            return Ok(ToolResult::success(
                serde_json::json!({ "memory_id": id, "status": "pinned", "scope": "you only" }),
                duration(),
            ));
        }

        let content = match input.content {
            Some(ref c) if !c.is_empty() => c.clone(),
            _ => {
                return Ok(ToolResult::failure(
                    "Provide 'content' to pin as a new memory, or 'memory_id' to pin an existing one",
                    duration(),
                ));
            }
        };

        let mut tags = input.tags.unwrap_or_default();
        if !tags.contains(&"pinned".to_string()) {
            tags.push("pinned".to_string());
        }

        let memory = Memory {
            id: MemoryId::new(),
            content,
            layer: MemoryLayer::Semantic,
            owner: self.agent_id.clone(),
            visibility: Visibility::Private,
            importance: input.importance.unwrap_or(0.9),
            tags,
            source: workforce_memory::types::MemorySource::Human,
            identity_id: Some(identity_id),
            source_session_id: None,
            source_message_ids: Vec::new(),
            created_at: chrono::Utc::now(),
            event_time: chrono::Utc::now(),
            last_accessed: chrono::Utc::now(),
            access_count: 0,
            recurrence_count: 0,
            last_recurrence_at: None,
            deleted: false,
        };

        let id = self.store.remember(memory).await?;

        Ok(ToolResult::success(
            serde_json::json!({
                "memory_id": id.0,
                "status": "pinned",
                "scope": "you only",
                "message": "Pinned for you — appears in your prompts only",
            }),
            duration(),
        ))
    }

    async fn handle_unpin(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        let duration = || start.elapsed().as_millis() as u64;

        let id = match input.memory_id {
            Some(ref id) if !id.is_empty() => id.clone(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'memory_id' — which memory to unpin?",
                    duration(),
                ));
            }
        };

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("unpin", start));
        };

        let mem = self.store.get(&MemoryId(id.clone())).await?;
        let Some(mem) = mem else {
            return Ok(ToolResult::failure(
                format!("Memory '{id}' not found"),
                duration(),
            ));
        };
        if mem.identity_id.as_deref() != Some(identity_id.as_str()) {
            return Ok(ToolResult::failure(
                "Cannot unpin a memory owned by another user",
                duration(),
            ));
        }

        let tags: Vec<String> = mem.tags.into_iter().filter(|t| t != "pinned").collect();
        self.store.update_tags(&MemoryId(id.clone()), tags).await?;

        Ok(ToolResult::success(
            serde_json::json!({ "memory_id": id, "status": "unpinned" }),
            duration(),
        ))
    }

    /// Return the transcript passage that produced a recalled memory.
    ///
    /// Uses `Memory.source_session_id` + `source_message_ids` populated by
    /// passive extraction. Pulls the JSONL transcript from the agent's
    /// workspace and returns the matching turn with one message of context
    /// on each side for coherence.
    async fn handle_fetch_source(&self, input: MemoryInput, start: Instant) -> Result<ToolResult> {
        use workforce_common::session::{JsonlSessionStore, SessionStore};

        let duration = || start.elapsed().as_millis() as u64;

        let id = match input.memory_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'memory_id' — which memory to fetch the source for?",
                    duration(),
                ));
            }
        };

        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(unscoped_identity_failure("fetch_source", start));
        };

        let Some(memory) = self.store.get(&MemoryId(id.clone())).await? else {
            return Ok(ToolResult::failure(
                format!("Memory '{id}' not found"),
                duration(),
            ));
        };

        if memory.identity_id.as_deref() != Some(identity_id.as_str()) {
            return Ok(ToolResult::failure(
                format!("Memory '{id}' not found"),
                duration(),
            ));
        }

        let Some(session_id) = memory.source_session_id.clone() else {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "memory_id": id,
                    "has_source": false,
                    "message": "This memory has no source transcript link."
                }),
                duration(),
            ));
        };

        // Honour the injected (configured-backend) store; fall back to a JSONL
        // store on the workspace for the storeless `new` (local/test) path.
        let sessions: Arc<dyn SessionStore> = match self.sessions.as_ref() {
            Some(s) => Arc::clone(s),
            None => Arc::new(JsonlSessionStore::new(&self.workspace)?),
        };
        let session_owned = matches!(
            sessions.get(&session_id),
            Ok(Some(ref s)) if s.owner == identity_id,
        );
        if !session_owned {
            return Ok(ToolResult::failure(
                format!("Memory '{id}' not found"),
                duration(),
            ));
        }
        let transcript = match sessions.read_transcript(&session_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to load source session '{session_id}': {e}"),
                    duration(),
                ));
            }
        };

        let excerpt = extract_excerpt(&transcript, &memory.source_message_ids);
        if excerpt.is_empty() {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "memory_id": id,
                    "session_id": session_id,
                    "has_source": false,
                    "message": "Source session found but the referenced messages are no longer in the transcript."
                }),
                duration(),
            ));
        }

        Ok(ToolResult::success(
            serde_json::json!({
                "memory_id": id,
                "session_id": session_id,
                "has_source": true,
                "passage": excerpt,
            }),
            duration(),
        ))
    }
}

/// Pull the transcript entries matching `message_ids` plus one entry of
/// context on each side, rendered as `role: content` lines.
fn extract_excerpt(
    transcript: &[workforce_common::session::TranscriptEntry],
    message_ids: &[String],
) -> String {
    if message_ids.is_empty() {
        return String::new();
    }
    let wanted: std::collections::HashSet<&str> = message_ids.iter().map(String::as_str).collect();

    let matched_positions: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter(|(_, e)| wanted.contains(e.id.as_str()))
        .map(|(i, _)| i)
        .collect();

    if matched_positions.is_empty() {
        return String::new();
    }

    let start = matched_positions
        .first()
        .copied()
        .unwrap_or(0)
        .saturating_sub(1);
    let end = matched_positions
        .last()
        .copied()
        .map(|i| (i + 2).min(transcript.len()))
        .unwrap_or(transcript.len());

    transcript[start..end]
        .iter()
        .map(|e| format!("{:?}: {}", e.role, e.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn parse_layer(s: &str) -> Option<MemoryLayer> {
    match s.to_lowercase().as_str() {
        "working" => Some(MemoryLayer::Working),
        "episodic" => Some(MemoryLayer::Episodic),
        "semantic" => Some(MemoryLayer::Semantic),
        "team" => Some(MemoryLayer::Team),
        "organisation" | "organization" => Some(MemoryLayer::Organisation),
        _ => None,
    }
}

fn parse_visibility(s: &str) -> Visibility {
    match s.to_lowercase().as_str() {
        "global" => Visibility::Global,
        "private" => Visibility::Private,
        s if s.starts_with("team:") => Visibility::Team(s[5..].to_string()),
        "team" => Visibility::Team("default".to_string()),
        _ => Visibility::Private,
    }
}

/// Lessons must read as trigger-conditioned advice — content starts with
/// "When ", "If ", or "Whenever " (case-insensitive). Both the in-process
/// extractor (`lessons::extract_and_store`) and the explicit `memory.remember`
/// tool path enforce this; keeping the check in one place would mean exposing
/// `is_valid_lesson` publicly, which carries the rest of the parse shape with
/// it — this small duplication is worth the looser coupling.
fn is_trigger_prefixed(content: &str) -> bool {
    let lower = content.trim_start().to_lowercase();
    lower.starts_with("when ") || lower.starts_with("if ") || lower.starts_with("whenever ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::store::MemoryStore;

    /// All `MemoryTool` handlers refuse to run unscoped (DW-518). Tests
    /// that exercise recall/count/overview/timeline must wrap themselves
    /// in this helper so an identity is in task-local scope.
    async fn with_test_user<F, Fut, T>(body: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        CURRENT_USER_ID
            .scope("test-user".to_string(), async move { body().await })
            .await
    }

    // ─── MemorySearchTool tests ──────────────────────────────────────────

    // ─── MemoryTool tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn remember_and_recall() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, agent_id, dir.path().to_path_buf());

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "remember",
                    "content": "Always use eyre for error handling in Rust",
                    "layer": "semantic",
                    "importance": 0.9,
                    "tags": ["rust", "best-practices"]
                }))
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(result.output["status"], "stored");
            assert_eq!(result.output["layer"], "semantic");

            let result = tool
                .execute(serde_json::json!({
                    "action": "recall",
                    "query": "error handling rust"
                }))
                .await
                .unwrap();

            assert!(result.success);
            assert!(result.output["count"].as_u64().unwrap() > 0);
            let first = &result.output["results"][0];
            assert!(first["content"].as_str().unwrap().contains("eyre"));
            assert_eq!(first["layer"], "semantic");
        })
        .await;
    }

    #[tokio::test]
    async fn remember_lesson_validates_trigger_prefix_and_tags() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let tool = MemoryTool::new(
            store.clone(),
            AgentId::new("a"),
            tempfile::tempdir().unwrap().path().to_path_buf(),
        );

        with_test_user(|| async {
            let missing_prefix = tool
                .execute(serde_json::json!({
                    "action": "remember",
                    "content": "Use jq for JSON",
                    "source": "lesson",
                    "trigger_tags": ["jq"],
                }))
                .await
                .unwrap();
            assert!(!missing_prefix.success);

            let missing_triggers = tool
                .execute(serde_json::json!({
                    "action": "remember",
                    "content": "When parsing JSON, prefer jq.",
                    "source": "lesson",
                }))
                .await
                .unwrap();
            assert!(!missing_triggers.success);

            let ok = tool
                .execute(serde_json::json!({
                    "action": "remember",
                    "content": "When parsing JSON in shell, prefer jq over sed.",
                    "source": "lesson",
                    "trigger_tags": ["jq", "shell-json"],
                }))
                .await
                .unwrap();
            assert!(ok.success);
            assert_eq!(ok.output["source"], "lesson");
            assert_eq!(ok.output["layer"], "semantic");
        })
        .await;
    }

    #[tokio::test]
    async fn recall_excludes_polluted_kinds_by_default() {
        use workforce_memory::audit::{CONSOLIDATION_META_KIND, RECAP_ARTIFACT_KIND};
        use workforce_memory::types::{Memory, MemoryLayer};

        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");

        let clean = Memory::new(
            "the user shipped a feature",
            MemoryLayer::Episodic,
            agent_id.clone(),
        )
        .with_identity_id("test-user");
        let recap = Memory::new(
            "the user shipped a feature too",
            MemoryLayer::Episodic,
            agent_id.clone(),
        )
        .with_tags(vec![RECAP_ARTIFACT_KIND.to_string()])
        .with_identity_id("test-user");
        let consolidation = Memory::new(
            "the user shipped a feature again",
            MemoryLayer::Episodic,
            agent_id.clone(),
        )
        .with_tags(vec![CONSOLIDATION_META_KIND.to_string()])
        .with_identity_id("test-user");

        store.remember(clean).await.unwrap();
        store.remember(recap).await.unwrap();
        store.remember(consolidation).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(Arc::clone(&store), agent_id, dir.path().to_path_buf());

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "recall",
                    "query": "user shipped feature"
                }))
                .await
                .unwrap();
            assert!(result.success);
            let returned: Vec<&serde_json::Value> = result.output["results"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["result_source"] == "vector")
                .collect();
            assert_eq!(returned.len(), 1, "polluted memories must be excluded");
            let tags = returned[0]["tags"].as_array().unwrap();
            assert!(tags.iter().all(|t| t.as_str() != Some(RECAP_ARTIFACT_KIND)
                && t.as_str() != Some(CONSOLIDATION_META_KIND)));

            let result = tool
                .execute(serde_json::json!({
                    "action": "recall",
                    "query": "user shipped feature",
                    "include_polluted": true
                }))
                .await
                .unwrap();
            let returned: Vec<&serde_json::Value> = result.output["results"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["result_source"] == "vector")
                .collect();
            assert_eq!(
                returned.len(),
                3,
                "include_polluted must surface the audit-tagged memories"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn remember_requires_content() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, AgentId::new("test"), dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "remember"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("content"));
    }

    #[tokio::test]
    async fn forget_memory() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(
            Arc::clone(&store),
            agent_id.clone(),
            dir.path().to_path_buf(),
        );

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "remember",
                    "content": "temporary info"
                }))
                .await
                .unwrap();
            let memory_id = result.output["memory_id"].as_str().unwrap().to_string();

            let result = tool
                .execute(serde_json::json!({
                    "action": "recall",
                    "query": "temporary"
                }))
                .await
                .unwrap();
            assert_eq!(result.output["count"], 1);

            let result = tool
                .execute(serde_json::json!({
                    "action": "forget",
                    "memory_id": memory_id
                }))
                .await
                .unwrap();
            assert!(result.success);
            assert_eq!(result.output["status"], "forgotten");

            let result = tool
                .execute(serde_json::json!({
                    "action": "recall",
                    "query": "temporary"
                }))
                .await
                .unwrap();
            assert_eq!(result.output["count"], 0);
        })
        .await;
    }

    #[tokio::test]
    async fn count_memories() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(Arc::clone(&store), agent_id, dir.path().to_path_buf());

        with_test_user(|| async {
            for i in 0..3 {
                tool.execute(serde_json::json!({
                    "action": "remember",
                    "content": format!("Memory number {}", i),
                    "layer": "episodic"
                }))
                .await
                .unwrap();
            }

            let result = tool
                .execute(serde_json::json!({
                    "action": "count",
                    "layer": "episodic"
                }))
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(result.output["count"], 3);
        })
        .await;
    }

    #[tokio::test]
    async fn memory_with_visibility() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, AgentId::new("test"), dir.path().to_path_buf());

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "remember",
                    "content": "Team knowledge about deployment",
                    "layer": "team",
                    "visibility": "global",
                    "tags": ["deployment", "ops"]
                }))
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(result.output["layer"], "team");
        })
        .await;
    }

    #[tokio::test]
    async fn pin_rejects_global_scope_from_agent_tool() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, AgentId::new("test"), dir.path().to_path_buf());

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "pin",
                    "content": "agent-wide pin attempt",
                    "scope": "all"
                }))
                .await
                .unwrap();

            assert!(!result.success);
            assert!(result
                .error
                .unwrap()
                .to_lowercase()
                .contains("not available"));
        })
        .await;
    }

    #[tokio::test]
    async fn pin_rejects_when_no_identity_in_scope() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, AgentId::new("test"), dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "pin",
                "content": "no identity"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap()
            .to_lowercase()
            .contains("requires a user identity"));
    }

    #[tokio::test]
    async fn pin_rejects_other_users_memory() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let other_id = store
            .remember(
                Memory::new(
                    "other user's note",
                    MemoryLayer::Episodic,
                    AgentId::new("test"),
                )
                .with_identity_id("other-user"),
            )
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(
            Arc::clone(&store),
            AgentId::new("test"),
            dir.path().to_path_buf(),
        );

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "pin",
                    "memory_id": other_id.0
                }))
                .await
                .unwrap();
            assert!(!result.success);
            assert!(result
                .error
                .unwrap()
                .to_lowercase()
                .contains("owned by another user"));
        })
        .await;
    }

    #[tokio::test]
    async fn unknown_action() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, AgentId::new("test"), dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "destroy"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[test]
    fn parse_layer_variants() {
        assert_eq!(parse_layer("working"), Some(MemoryLayer::Working));
        assert_eq!(parse_layer("episodic"), Some(MemoryLayer::Episodic));
        assert_eq!(parse_layer("semantic"), Some(MemoryLayer::Semantic));
        assert_eq!(parse_layer("team"), Some(MemoryLayer::Team));
        assert_eq!(parse_layer("organisation"), Some(MemoryLayer::Organisation));
        assert_eq!(parse_layer("organization"), Some(MemoryLayer::Organisation));
        assert_eq!(parse_layer("invalid"), None);
    }

    #[test]
    fn parse_visibility_variants() {
        assert_eq!(parse_visibility("private"), Visibility::Private);
        assert_eq!(parse_visibility("global"), Visibility::Global);
        assert_eq!(
            parse_visibility("team:backend"),
            Visibility::Team("backend".to_string())
        );
        assert_eq!(
            parse_visibility("team"),
            Visibility::Team("default".to_string())
        );
        assert_eq!(parse_visibility("unknown"), Visibility::Private);
    }

    // ─── fetch_source ────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_source_errors_on_missing_memory_id() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(store, AgentId::new("a"), dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({ "action": "fetch_source" }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("memory_id"));
    }

    #[tokio::test]
    async fn fetch_source_reports_no_source_link_when_memory_lacks_one() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(
            Arc::clone(&store),
            AgentId::new("a"),
            dir.path().to_path_buf(),
        );

        let mem = workforce_memory::types::Memory::new(
            "no-source memory",
            MemoryLayer::Episodic,
            AgentId::new("a"),
        )
        .with_identity_id("test-user");
        let id = store.remember(mem).await.unwrap();

        with_test_user(|| async {
            let result = tool
                .execute(serde_json::json!({
                    "action": "fetch_source",
                    "memory_id": id.0
                }))
                .await
                .unwrap();

            assert!(result.success);
            assert_eq!(result.output["has_source"], false);
        })
        .await;
    }

    #[test]
    fn extract_excerpt_returns_messages_plus_one_turn_of_context() {
        use workforce_common::session::{MessageRole, TranscriptEntry};

        let entries: Vec<TranscriptEntry> = ["user-0", "asst-0", "user-1", "asst-1", "user-2"]
            .iter()
            .enumerate()
            .map(|(i, id)| TranscriptEntry {
                id: (*id).to_string(),
                session_id: "s".into(),
                timestamp: chrono::Utc::now(),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: format!("msg {i}"),
                tool_metadata: None,
                token_usage: None,
                desk: None,
            })
            .collect();

        let excerpt = extract_excerpt(&entries, &["user-1".to_string(), "asst-1".to_string()]);
        // Should contain the requested pair (msg 2, msg 3) plus one entry of
        // context on each side (msg 1 before, msg 4 after).
        assert!(!excerpt.contains("msg 0"));
        assert!(excerpt.contains("msg 1"));
        assert!(excerpt.contains("msg 2"));
        assert!(excerpt.contains("msg 3"));
        assert!(excerpt.contains("msg 4"));
    }

    #[test]
    fn extract_excerpt_is_empty_when_message_ids_dont_match() {
        use workforce_common::session::{MessageRole, TranscriptEntry};

        let entries = vec![TranscriptEntry {
            id: "other".into(),
            session_id: "s".into(),
            timestamp: chrono::Utc::now(),
            role: MessageRole::User,
            content: "hi".into(),
            tool_metadata: None,
            token_usage: None,
            desk: None,
        }];

        assert_eq!(
            extract_excerpt(&entries, &["missing".to_string()]),
            String::new()
        );
        assert_eq!(extract_excerpt(&entries, &[]), String::new());
    }
}
