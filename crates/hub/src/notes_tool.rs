use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{config_guard, Tool, ToolResult};
use workforce_common::settings::AgentRecallScope;
use workforce_core::types::AgentId;
use workforce_memory::store::{apply_agent_scope, MemoryStore};
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer, MemorySource};

use crate::memory::{MemoryManager, SessionTask, SessionTaskStatus};
use crate::serve::AgentService;

/// Cap on the notes joined into one day's rendered string, preserved from
/// the pre-agent-scoping `limit: 50` query.
const DAILY_NOTE_LIMIT: usize = 50;

/// One day's store-backed daily notes, distinguishing "the store has nothing
/// for this user/date" from "rows exist but none survive the agent scope".
/// Callers with a legacy markdown fallback must only take it on `Absent`:
/// a `Suppressed` day HAS store content — it just isn't this agent's to see —
/// and falling back would reintroduce the unscoped leak the filter closed.
#[derive(Debug, PartialEq, Eq)]
pub enum EpisodicDay {
    /// No store rows for this user/date (or no store/identity/read error).
    Absent,
    /// Rows exist for this user/date but the agent scope filtered them all.
    Suppressed,
    /// The surviving notes, joined chronologically.
    Notes(String),
}

/// Read this user's episodic memories whose depicted **event** falls on `date`
/// (`YYYY-MM-DD`, UTC), joined chronologically into one daily-note string.
/// `Absent` when there is nothing, or when no identity is in scope (we fail
/// closed rather than issue an unscoped query).
///
/// Filtering is by the `date:{date}` tag, not a `created_at` range: the tag is
/// derived from each memory's `event_time` at write time, so it stays correct
/// for memories **backfilled** for an older day. `list()` (not `recall()`)
/// avoids a nearText embedding-sidecar dependency. The final `retain` keeps the
/// scope strictly per-user even if a backend's identity filter admits
/// `identityId IS NULL` agent-wide rows — daily notes never cross users.
///
/// Agent scoping mirrors the vector-recall surfaces: `Isolated` confines the
/// read to `agent`'s own notes via the server-side `owner` filter; `Shared`
/// post-filters with [`apply_agent_scope`] so another agent's Private notes
/// never surface here (only their Team/Global ones can). Shared over-fetches
/// so to-be-discarded foreign rows don't eat the day's row budget. Any note
/// that survives with a different owner is rendered with a
/// `[from agent "…"]` prefix so the reading agent doesn't mistake it for
/// its own.
pub async fn read_episodic_day(
    store: &dyn MemoryStore,
    date: &str,
    identity_id: Option<String>,
    agent: &AgentId,
    scope: AgentRecallScope,
) -> EpisodicDay {
    let Some(identity) = identity_id else {
        return EpisodicDay::Absent;
    };
    let owned_only = scope.owned_only();

    let filter = MemoryFilter {
        layer: Some(MemoryLayer::Episodic),
        tags: Some(vec![format!("date:{date}")]),
        identity_id: Some(identity.clone()),
        owner: owned_only.then(|| agent.clone()),
        // Shared discards foreign Private rows in-process, so over-fetch to
        // keep them from crowding the agent's own notes out of the budget.
        limit: Some(if owned_only {
            DAILY_NOTE_LIMIT
        } else {
            DAILY_NOTE_LIMIT * 3
        }),
        ..Default::default()
    };

    let Ok(mut memories) = store.list(filter).await else {
        return EpisodicDay::Absent;
    };
    memories.retain(|m| m.identity_id.as_deref() == Some(identity.as_str()));
    let had_rows = !memories.is_empty();
    let mut memories = apply_agent_scope(scope, agent, memories);
    memories.truncate(DAILY_NOTE_LIMIT);
    if memories.is_empty() {
        if had_rows {
            return EpisodicDay::Suppressed;
        }
        // Under Isolated the owner filter above hides foreign rows from the
        // backend read itself, so an empty result can't distinguish "day has
        // nothing" from "day has only other agents' notes". Probe without the
        // owner filter (only on this already-empty path) — Suppressed and
        // Absent route callers differently (no-fallback vs legacy fallback).
        if owned_only {
            let probe = MemoryFilter {
                layer: Some(MemoryLayer::Episodic),
                tags: Some(vec![format!("date:{date}")]),
                identity_id: Some(identity.clone()),
                limit: Some(DAILY_NOTE_LIMIT),
                ..Default::default()
            };
            if let Ok(rows) = store.list(probe).await {
                if rows
                    .iter()
                    .any(|m| m.identity_id.as_deref() == Some(identity.as_str()))
                {
                    return EpisodicDay::Suppressed;
                }
            }
        }
        return EpisodicDay::Absent;
    }
    memories.sort_by_key(|m| m.event_time);
    let joined = memories
        .iter()
        .map(|m| match m.origin_label(agent) {
            Some(label) => format!("[{label}]\n{}", m.content),
            None => m.content.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    if joined.trim().is_empty() {
        EpisodicDay::Suppressed
    } else {
        EpisodicDay::Notes(joined)
    }
}

/// MemoryWriter implementation that persists compaction output to Weaviate (or files as fallback).
pub struct DailyNotesMemoryWriter {
    workspace: PathBuf,
    store: Option<Arc<dyn MemoryStore>>,
    agent_id: AgentId,
}

impl DailyNotesMemoryWriter {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            store: None,
            agent_id: AgentId::new("default"),
        }
    }

    pub fn with_store(workspace: PathBuf, store: Arc<dyn MemoryStore>, agent_id: AgentId) -> Self {
        Self {
            workspace,
            store: Some(store),
            agent_id,
        }
    }
}

/// Append an entry to the technical compaction log at memory/compaction/YYYY-MM-DD-compact.md.
fn append_technical_log(workspace: &std::path::Path, content: &str, heading: &str) -> Result<()> {
    use chrono::Local;
    let date = Local::now().format("%Y-%m-%d");
    let roots = workforce_agent::tools::paths::CanonicalRoots::new(&[workspace.to_path_buf()]);
    let compaction_dir = workspace.join("memory").join("compaction");
    let log_path = roots
        .resolve(&compaction_dir.join(format!("{}-compact.md", date)))
        .ok_or_else(|| eyre::eyre!("{} leads outside the workspace", compaction_dir.display()))?;

    let entry = format!(
        "\n## {} ({})\n\n{}\n",
        heading,
        Local::now().format("%H:%M"),
        content
    );

    if let Some(dir) = log_path.parent() {
        config_guard::create_dirs(&roots, dir)?;
    }
    Ok(config_guard::append(&roots, &log_path, entry.as_bytes())?)
}

#[async_trait]
impl workforce_agent::compaction::MemoryWriter for DailyNotesMemoryWriter {
    async fn write_flush(&self, content: &str, label: &str) -> Result<()> {
        if let Some(ref store) = self.store {
            if let Some(uid) = crate::user_config::current_identity_id() {
                let tags = vec![
                    "source:compaction".to_string(),
                    format!("label:{}", label),
                    format!("date:{}", chrono::Utc::now().format("%Y-%m-%d")),
                ];
                let memory = Memory::new(content, MemoryLayer::Episodic, self.agent_id.clone())
                    .with_tags(tags)
                    .with_source(MemorySource::Compaction)
                    .with_identity_id(uid);
                if store.remember(memory).await.is_ok() {
                    return Ok(());
                }
            }
        }

        let mgr = match crate::user_config::current_identity_id() {
            Some(uid) => MemoryManager::for_user(&self.workspace, &uid),
            None => MemoryManager::new(&self.workspace),
        };
        mgr.append_compaction(&format!("\n## Memory Flush ({})\n\n{}\n", label, content))
    }

    async fn write_technical_log(&self, content: &str, label: &str) -> Result<()> {
        if let Some(ref store) = self.store {
            if let Some(uid) = crate::user_config::current_identity_id() {
                let tags = vec![
                    "source:technical".to_string(),
                    format!("label:{}", label),
                    format!("date:{}", chrono::Utc::now().format("%Y-%m-%d")),
                ];
                let memory = Memory::new(content, MemoryLayer::Working, self.agent_id.clone())
                    .with_tags(tags)
                    .with_source(MemorySource::Technical)
                    .with_identity_id(uid);
                if store.remember(memory).await.is_ok() {
                    return Ok(());
                }
            }
        }

        append_technical_log(&self.workspace, content, label)
    }
}

/// Agent-callable tool for filesystem-based memory: daily notes and active task.
pub struct NotesTool {
    workspace: PathBuf,
    store: Option<Arc<dyn MemoryStore>>,
    agent_id: AgentId,
    /// Session store for ownership checks. Injected by production callers so
    /// the configured backend is honoured; `None` (test/local `new`) falls
    /// back to a JSONL store on the workspace.
    sessions: Option<Arc<dyn workforce_common::session::SessionStore>>,
    /// Agent scoping applied to `read_daily`, mirroring the recall surfaces.
    /// Static snapshot for short-lived callers (worker tasks, MCP serving);
    /// the long-lived hub service injects `resolved` instead so hot-reloaded
    /// scope changes reach the tool path without a rebuild.
    recall_scope: AgentRecallScope,
    /// Live resolved config; when present it wins over `recall_scope`.
    resolved: Option<crate::resolved_config::ConfigCell>,
}

impl NotesTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            store: None,
            agent_id: AgentId::new("default"),
            sessions: None,
            recall_scope: AgentRecallScope::default(),
            resolved: None,
        }
    }

    /// Create a NotesTool with a vector store for automatic write_human indexing.
    pub fn with_store(workspace: PathBuf, store: Arc<dyn MemoryStore>, agent_id: AgentId) -> Self {
        Self {
            workspace,
            store: Some(store),
            agent_id,
            sessions: None,
            recall_scope: AgentRecallScope::default(),
            resolved: None,
        }
    }

    /// Apply the agent's effective `agent_recall_scope` so `read_daily`
    /// honours the same Shared/Isolated narrowing as the recall surfaces.
    /// For long-lived services prefer [`Self::with_live_config`], which
    /// tracks hot-reloads.
    pub fn with_recall_scope(mut self, scope: AgentRecallScope) -> Self {
        self.recall_scope = scope;
        self
    }

    /// Track the live resolved config so a hot-reloaded `agent_recall_scope`
    /// takes effect on the next `read_daily` call, matching the per-turn
    /// injection paths.
    pub fn with_live_config(mut self, resolved: crate::resolved_config::ConfigCell) -> Self {
        self.resolved = Some(resolved);
        self
    }

    fn effective_recall_scope(&self) -> AgentRecallScope {
        self.resolved
            .as_ref()
            .map(|cell| cell.snapshot().memory.agent_recall_scope)
            .unwrap_or(self.recall_scope)
    }

    /// Inject the agent's session store so ownership checks honour the
    /// configured (JSONL or Postgres) backend instead of constructing their own.
    pub fn with_sessions(
        mut self,
        sessions: Arc<dyn workforce_common::session::SessionStore>,
    ) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// Resolve the session ID: prefer explicit input, fall back to task-local.
    fn resolve_session_id(&self, explicit: Option<&str>) -> Option<String> {
        if let Some(sid) = explicit {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
        workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
    }

    /// True if the requesting principal is allowed to inspect the named
    /// session via this tool. The current task-local session is always
    /// allowed (the agent runner already validated the principal at dispatch).
    /// For any other session_id the requester must match the on-disk owner.
    /// Sessions that don't exist on disk return `true` because notes-only
    /// sessions live in `MemoryManager`, not `JsonlSessionStore`. Used by
    /// DW-522 to gate `list_tasks` against arbitrary session ids.
    fn caller_owns_session(&self, session_id: &str) -> Result<bool> {
        use workforce_common::session::{JsonlSessionStore, SessionStore};

        let current_session = workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok();
        if current_session.as_deref() == Some(session_id) {
            return Ok(true);
        }

        let Some(uid) = crate::user_config::current_identity_id() else {
            return Ok(false);
        };
        // Honour the injected (configured-backend) store; fall back to a JSONL
        // store on the workspace for the storeless `new` (local/test) path.
        let session = match self.sessions.as_ref() {
            Some(sessions) => sessions.get(session_id)?,
            None => JsonlSessionStore::new(&self.workspace)?.get(session_id)?,
        };
        match session {
            Some(s) => Ok(s.owner == uid),
            None => Ok(true),
        }
    }

    async fn store_memory(&self, content: &str, layer: MemoryLayer, source: MemorySource) -> bool {
        let Some(ref store) = self.store else {
            return false;
        };

        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let tags = vec![
            format!("source:{}", source.as_str()),
            format!("date:{}", date),
        ];

        let mut memory = Memory::new(content, layer, self.agent_id.clone())
            .with_tags(tags)
            .with_source(source);

        if let Ok(uid) = workforce_agent::runner::CURRENT_USER_ID.try_with(|u| u.clone()) {
            if !uid.is_empty() {
                memory = memory.with_identity_id(uid);
            }
        }

        match store.remember(memory).await {
            Ok(id) => {
                tracing::debug!(memory_id = %id, "Memory stored in vector store");
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "Vector store write failed — falling back to file");
                false
            }
        }
    }
}

#[derive(Deserialize)]
struct NotesInput {
    action: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    target_session_id: Option<String>,
    /// Opt-in flag for `list_tasks` / `list_all_tasks`. Without it, completed
    /// and cancelled tasks are filtered out — listings answer "what's still
    /// open?" instead of "what ever existed?".
    #[serde(default)]
    include_completed: Option<bool>,
}

fn task_json(t: &&SessionTask) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "description": t.description,
        "status": t.status.to_string(),
    })
}

fn filter_visible(
    tasks: &[SessionTask],
    include_completed: bool,
) -> impl Iterator<Item = &SessionTask> {
    tasks
        .iter()
        .filter(move |t| include_completed || matches!(t.status, SessionTaskStatus::Active))
}

#[async_trait]
impl Tool for NotesTool {
    fn name(&self) -> &str {
        "notes"
    }

    fn description(&self) -> &str {
        "Manage notes/tasks: write, read, list, delete, search, archive"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "write_human", "write_technical",
                        "write_daily", "read_daily",
                        "set_task", "get_task", "clear_task",
                        "add_task", "complete_task", "list_tasks", "list_all_tasks", "move_task",
                        "append_memory",
                        "write_profile"
                    ],
                    "description": "Action to perform. `write_profile` replaces the current user's 'Learned Preferences' profile with a distilled narrative (called from the per-user consolidation job)."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write (for write_daily"
                },
                "date": {
                    "type": "string",
                    "description": "Date in YYYY-MM-DD format (for read_daily"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID — auto-resolved from context if omitted"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID (for complete_task, move_task)"
                },
                "target_session_id": {
                    "type": "string",
                    "description": "Target session ID (for move_task)"
                },
                "include_completed": {
                    "type": "boolean",
                    "description": "For list_tasks / list_all_tasks: include completed and cancelled tasks. Defaults to false — listings answer 'what's still open?'"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: NotesInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid notes input: {}", e))?;

        let mgr = match workforce_agent::runner::CURRENT_USER_ID.try_with(|u| u.clone()) {
            Ok(uid) if !uid.is_empty() => MemoryManager::for_user(&self.workspace, &uid),
            _ => MemoryManager::new(&self.workspace),
        };

        match input.action.as_str() {
            "write_daily" => {
                let content = match input.content {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — what should be written to today's daily note?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let stored = self
                    .store_memory(&content, MemoryLayer::Episodic, MemorySource::Human)
                    .await;

                if !stored {
                    mgr.append_daily(&content)?;
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "write_daily",
                        "store": if stored { "weaviate" } else { "file" },
                        "status": "written"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "read_daily" => {
                let date = input
                    .date
                    .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());

                // Prefer the episodic store (identity-scoped), falling back to
                // the legacy per-user markdown file. See `read_episodic_day`
                // for why this is both leak-safe and backfill-correct. A
                // `Suppressed` day must NOT fall back: the store has content
                // for this user/date, it just isn't this agent's to see.
                let from_store = if let Some(ref store) = self.store {
                    read_episodic_day(
                        store.as_ref(),
                        &date,
                        crate::user_config::current_identity_id(),
                        &self.agent_id,
                        self.effective_recall_scope(),
                    )
                    .await
                } else {
                    EpisodicDay::Absent
                };

                let (content, source) = match from_store {
                    EpisodicDay::Notes(combined) => (combined, "weaviate"),
                    EpisodicDay::Suppressed => (String::new(), "weaviate"),
                    EpisodicDay::Absent => {
                        let path = mgr.daily_note_path_for_date(&date);
                        let file_content = if path.exists() {
                            std::fs::read_to_string(&path)?
                        } else {
                            String::new()
                        };
                        (file_content, "file")
                    }
                };

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "read_daily",
                        "date": date,
                        "content": content,
                        "source": source,
                        "exists": !content.is_empty()
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "set_task" => {
                let content = match input.content {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — what is the active task?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let resolved_sid = self.resolve_session_id(input.session_id.as_deref());
                mgr.set_active_task_with_session(&content, resolved_sid.as_deref())?;

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "set_task",
                        "status": "set"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "get_task" => {
                let resolved_sid = self.resolve_session_id(input.session_id.as_deref());
                match mgr.get_active_task_info() {
                    Some(info) => {
                        let ownership = info.ownership_status(resolved_sid.as_deref());
                        Ok(ToolResult::success(
                            serde_json::json!({
                                "action": "get_task",
                                "content": info.content,
                                "has_task": true,
                                "ownership": format!("{:?}", ownership),
                            }),
                            start.elapsed().as_millis() as u64,
                        ))
                    }
                    None => {
                        Ok(ToolResult::success(
                            serde_json::json!({
                                "action": "get_task",
                                "content": "",
                                "has_task": false
                            }),
                            start.elapsed().as_millis() as u64,
                        ))
                    }
                }
            }

            "clear_task" => {
                mgr.clear_active_task()?;

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "clear_task",
                        "status": "cleared"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "append_memory" => {
                let content = match input.content {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — what long-term fact should be remembered?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                if !self
                    .store_memory(&content, MemoryLayer::Semantic, MemorySource::Curated)
                    .await
                {
                    return Ok(ToolResult::failure(
                        "Memory store unavailable — the fact was not persisted. Retry once the vector store is reachable.",
                        start.elapsed().as_millis() as u64,
                    ));
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "append_memory",
                        "store": "weaviate",
                        "status": "appended"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            // ── Human/Technical Memory Actions (DW-452) ──────────────────

            "write_human" => {
                let content = match input.content {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — what human conversation should be recorded?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let stored = self
                    .store_memory(&content, MemoryLayer::Episodic, MemorySource::Human)
                    .await;

                if !stored {
                    mgr.append_daily(&content)?;
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "write_human",
                        "store": if stored { "weaviate" } else { "file" },
                        "status": "written"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "write_technical" => {
                let content = match input.content {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — what technical details should be logged?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let stored = self
                    .store_memory(&content, MemoryLayer::Working, MemorySource::Technical)
                    .await;

                if !stored {
                    append_technical_log(&self.workspace, &content, "Technical Note")?;
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "write_technical",
                        "store": if stored { "weaviate" } else { "file" },
                        "status": "written"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            // ── Per-Session Task List Actions ──────────────────────────────

            "add_task" => {
                let content = match input.content {
                    Some(c) if !c.is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — describe the task to add",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let session_id = self.resolve_session_id(input.session_id.as_deref());
                let Some(sid) = session_id else {
                    return Ok(ToolResult::failure(
                        "No session_id available — pass it explicitly or ensure session context is set",
                        start.elapsed().as_millis() as u64,
                    ));
                };

                let task = mgr.add_session_task(&sid, &content)?;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "add_task",
                        "task_id": task.id,
                        "description": task.description,
                        "status": task.status.to_string(),
                        "session_id": sid,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "complete_task" => {
                let task_id = match input.task_id {
                    Some(ref id) if !id.is_empty() => id.clone(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'task_id' — which task to complete?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let session_id = self.resolve_session_id(input.session_id.as_deref());
                let Some(sid) = session_id else {
                    return Ok(ToolResult::failure(
                        "No session_id available — pass it explicitly",
                        start.elapsed().as_millis() as u64,
                    ));
                };

                mgr.complete_session_task(&sid, &task_id)?;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "complete_task",
                        "task_id": task_id,
                        "status": "completed",
                        "session_id": sid,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "list_tasks" => {
                let session_id = self.resolve_session_id(input.session_id.as_deref());
                let Some(sid) = session_id else {
                    return Ok(ToolResult::failure(
                        "No session_id available — pass it explicitly",
                        start.elapsed().as_millis() as u64,
                    ));
                };

                if !self.caller_owns_session(&sid)? {
                    return Ok(ToolResult::failure(
                        format!("Session '{sid}' not found"),
                        start.elapsed().as_millis() as u64,
                    ));
                }

                let include_completed = input.include_completed.unwrap_or(false);
                let all = mgr.load_session_tasks(&sid)?;
                let visible: Vec<&SessionTask> = filter_visible(&all, include_completed).collect();
                let task_list: Vec<serde_json::Value> = visible.iter().map(task_json).collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "list_tasks",
                        "session_id": sid,
                        "tasks": task_list,
                        "count": visible.len(),
                        "total_count": all.len(),
                        "include_completed": include_completed,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "list_all_tasks" => {
                let include_completed = input.include_completed.unwrap_or(false);
                let all = mgr.list_all_session_tasks()?;
                let mut visible_total = 0usize;
                let mut total = 0usize;
                let sessions: Vec<serde_json::Value> = all
                    .iter()
                    .filter_map(|(sid, tasks)| {
                        total += tasks.len();
                        let visible: Vec<&SessionTask> =
                            filter_visible(tasks, include_completed).collect();
                        if visible.is_empty() {
                            return None;
                        }
                        visible_total += visible.len();
                        let task_list: Vec<serde_json::Value> =
                            visible.iter().map(task_json).collect();
                        Some(serde_json::json!({
                            "session_id": sid,
                            "tasks": task_list,
                        }))
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "list_all_tasks",
                        "sessions": sessions,
                        "total_tasks": visible_total,
                        "total_count_including_filtered": total,
                        "include_completed": include_completed,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "move_task" => {
                let task_id = match input.task_id {
                    Some(ref id) if !id.is_empty() => id.clone(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'task_id' — which task to move?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let source_sid = self.resolve_session_id(input.session_id.as_deref());
                let Some(source) = source_sid else {
                    return Ok(ToolResult::failure(
                        "No source session_id — pass 'session_id' for the source session",
                        start.elapsed().as_millis() as u64,
                    ));
                };

                let target = match input.target_session_id {
                    Some(ref id) if !id.is_empty() => id.clone(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'target_session_id' — which session to move the task to?",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };

                let moved = mgr.move_session_task(&source, &target, &task_id)?;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "move_task",
                        "task_id": moved.id,
                        "description": moved.description,
                        "from_session": source,
                        "to_session": target,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            "write_profile" => {
                let Some(user_id) = workforce_agent::runner::CURRENT_USER_ID
                    .try_with(|u| u.clone())
                    .ok()
                    .filter(|u| !u.is_empty())
                else {
                    return Ok(ToolResult::failure(
                        "write_profile requires a user context (CURRENT_USER_ID not in scope)",
                        start.elapsed().as_millis() as u64,
                    ));
                };
                let content = match input.content {
                    Some(c) if !c.trim().is_empty() => c,
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing 'content' — provide the distilled 'about this user' narrative",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                crate::profile::ProfileManager::new(&self.workspace)
                    .save_profile(&user_id, &content)?;
                Ok(ToolResult::success(
                    serde_json::json!({
                        "action": "write_profile",
                        "user_id": user_id,
                        "bytes_written": content.len().min(crate::profile::MAX_PROFILE_SIZE),
                        "status": "written"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }

            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{other}'. Use: write_human, write_technical, write_daily, read_daily, \
                     set_task, get_task, clear_task, add_task, complete_task, list_tasks, \
                     list_all_tasks, move_task, append_memory, write_profile"
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

pub fn memory_writer(service: &AgentService) -> Arc<dyn workforce_agent::compaction::MemoryWriter> {
    match &service.memory_store {
        Some(store) => Arc::new(crate::notes_tool::DailyNotesMemoryWriter::with_store(
            service.workspace.clone(),
            Arc::clone(store),
            service.agent_id.clone(),
        )),
        None => Arc::new(crate::notes_tool::DailyNotesMemoryWriter::new(
            service.workspace.clone(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    /// The agent uid writes memory/ too: a link it leaves where today's
    /// technical log goes carries nothing into the agent's identity.
    #[cfg(unix)]
    #[test]
    fn the_technical_log_follows_no_planted_link_onto_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::write(workspace.join("SOUL.md"), "soul").unwrap();
        std::fs::create_dir_all(workspace.join("memory/compaction")).unwrap();
        let date = chrono::Local::now().format("%Y-%m-%d");
        std::os::unix::fs::symlink(
            "../../SOUL.md",
            workspace.join(format!("memory/compaction/{date}-compact.md")),
        )
        .unwrap();

        assert!(append_technical_log(&workspace, "be someone else", "Note").is_err());
        assert_eq!(
            std::fs::read_to_string(workspace.join("SOUL.md")).unwrap(),
            "soul"
        );
    }

    #[test]
    fn the_technical_log_appends() {
        let dir = tempfile::tempdir().unwrap();
        append_technical_log(dir.path(), "first", "One").unwrap();
        append_technical_log(dir.path(), "second", "Two").unwrap();
        let date = chrono::Local::now().format("%Y-%m-%d");
        let log = std::fs::read_to_string(
            dir.path()
                .join(format!("memory/compaction/{date}-compact.md")),
        )
        .unwrap();
        assert!(log.contains("first") && log.contains("second"), "{log}");
    }

    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_core::types::AgentId;
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::store::MemoryStore;
    use workforce_memory::types::{MemoryFilter, MemoryLayer};

    use super::*;

    /// Unwrap the `Notes` arm or fail the test with context.
    fn notes(day: EpisodicDay, why: &str) -> String {
        match day {
            EpisodicDay::Notes(s) => s,
            other => panic!("{why}: expected Notes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_episodic_day_is_per_user_and_date_scoped() {
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent = AgentId::new("allan");
        let dated = |content: &str, uid: Option<&str>, date: &str| {
            let mut m = Memory::new(content, MemoryLayer::Episodic, agent.clone())
                .with_tags(vec![format!("date:{date}")]);
            if let Some(uid) = uid {
                m = m.with_identity_id(uid);
            }
            m
        };
        store
            .remember(dated("u1 monday", Some("u1"), "2026-06-01"))
            .await
            .unwrap();
        store
            .remember(dated("u1 tuesday", Some("u1"), "2026-06-02"))
            .await
            .unwrap();
        store
            .remember(dated("u2 monday", Some("u2"), "2026-06-01"))
            .await
            .unwrap();
        // Agent-wide (NULL identity) note for the same day — must NOT leak.
        store
            .remember(dated("agent-wide monday", None, "2026-06-01"))
            .await
            .unwrap();

        let scope = AgentRecallScope::Shared;
        let monday = notes(
            read_episodic_day(
                store.as_ref(),
                "2026-06-01",
                Some("u1".into()),
                &agent,
                scope,
            )
            .await,
            "u1 has a monday note",
        );
        assert!(monday.contains("u1 monday"));
        // Strictly per-user: neither another user's nor an agent-wide note.
        assert!(!monday.contains("u2 monday"));
        assert!(!monday.contains("agent-wide"));
        // Date-scoped: a monday read never returns tuesday.
        assert!(!monday.contains("u1 tuesday"));
        // The agent's own notes carry no attribution prefix.
        assert!(!monday.contains("[from agent"));

        let tuesday = notes(
            read_episodic_day(
                store.as_ref(),
                "2026-06-02",
                Some("u1".into()),
                &agent,
                scope,
            )
            .await,
            "u1 has a tuesday note",
        );
        assert!(tuesday.contains("u1 tuesday"));

        // Fail closed with no identity, and Absent when the day has nothing.
        assert_eq!(
            read_episodic_day(store.as_ref(), "2026-06-01", None, &agent, scope).await,
            EpisodicDay::Absent
        );
        assert_eq!(
            read_episodic_day(
                store.as_ref(),
                "2026-06-09",
                Some("u1".into()),
                &agent,
                scope
            )
            .await,
            EpisodicDay::Absent
        );
    }

    /// The Allan↔Sarah leak: another agent's Private daily notes for the same
    /// user must never surface (Shared), Team/Global ones surface WITH an
    /// attribution prefix, and Isolated confines the read to own notes only.
    #[tokio::test]
    async fn read_episodic_day_is_agent_scoped_and_attributed() {
        use workforce_memory::types::Visibility;

        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let allan = AgentId::new("allan");
        let sarah = AgentId::new("sarah");
        let date = "2026-07-12";
        let dated = |content: &str, owner: &AgentId, vis: Visibility| {
            Memory::new(content, MemoryLayer::Episodic, owner.clone())
                .with_tags(vec![format!("date:{date}")])
                .with_identity_id("u1")
                .with_visibility(vis)
        };
        store
            .remember(dated("allan note", &allan, Visibility::Private))
            .await
            .unwrap();
        store
            .remember(dated("sarah private lesson", &sarah, Visibility::Private))
            .await
            .unwrap();
        store
            .remember(dated("sarah global note", &sarah, Visibility::Global))
            .await
            .unwrap();

        // Shared: own + other agents' non-Private, the latter attributed.
        let shared = notes(
            read_episodic_day(
                store.as_ref(),
                date,
                Some("u1".into()),
                &allan,
                AgentRecallScope::Shared,
            )
            .await,
            "allan has notes",
        );
        assert!(shared.contains("allan note"));
        assert!(!shared.contains("sarah private lesson"));
        assert!(shared.contains("[from agent \"sarah\"]\nsarah global note"));

        // Isolated: strictly own notes, nothing from sarah at all.
        let isolated = notes(
            read_episodic_day(
                store.as_ref(),
                date,
                Some("u1".into()),
                &allan,
                AgentRecallScope::Isolated,
            )
            .await,
            "allan has own notes",
        );
        assert!(isolated.contains("allan note"));
        assert!(!isolated.contains("sarah"));

        // Sarah reading her own day sees her notes unattributed, and allan's
        // Private note stays invisible to her too — the narrowing is mutual.
        let sarahs = notes(
            read_episodic_day(
                store.as_ref(),
                date,
                Some("u1".into()),
                &sarah,
                AgentRecallScope::Shared,
            )
            .await,
            "sarah has notes",
        );
        assert!(sarahs.contains("sarah private lesson"));
        assert!(!sarahs.contains("[from agent \"sarah\"]"));
        assert!(!sarahs.contains("allan note"));
    }

    /// A day whose store rows are ALL hidden by the agent scope must read as
    /// `Suppressed`, not `Absent` — `Absent` would send callers to the
    /// unscoped legacy markdown fallback and reintroduce the leak.
    #[tokio::test]
    async fn read_episodic_day_suppressed_days_do_not_read_as_absent() {
        use workforce_memory::types::Visibility;

        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let allan = AgentId::new("allan");
        let sarah = AgentId::new("sarah");
        let date = "2026-07-12";
        store
            .remember(
                Memory::new("sarah private lesson", MemoryLayer::Episodic, sarah.clone())
                    .with_tags(vec![format!("date:{date}")])
                    .with_identity_id("u1")
                    .with_visibility(Visibility::Private),
            )
            .await
            .unwrap();

        // Allan's read of a sarah-only day: Suppressed under both scopes.
        for scope in [AgentRecallScope::Shared, AgentRecallScope::Isolated] {
            assert_eq!(
                read_episodic_day(store.as_ref(), date, Some("u1".into()), &allan, scope).await,
                EpisodicDay::Suppressed,
                "scope {scope:?}"
            );
        }
        // A day with no rows at all stays Absent (fallback allowed).
        assert_eq!(
            read_episodic_day(
                store.as_ref(),
                "2026-07-01",
                Some("u1".into()),
                &allan,
                AgentRecallScope::Shared
            )
            .await,
            EpisodicDay::Absent
        );
    }

    #[tokio::test]
    async fn write_and_read_daily() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        // Write
        let result = tool
            .execute(serde_json::json!({
                "action": "write_daily",
                "content": "## Meeting Notes\nDiscussed deployment strategy."
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["status"], "written");

        // Read today
        let result = tool
            .execute(serde_json::json!({
                "action": "read_daily"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output["exists"].as_bool().unwrap());
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("Meeting Notes"));
    }

    #[tokio::test]
    async fn read_daily_nonexistent_date() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "read_daily",
                "date": "2020-01-01"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(!result.output["exists"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn active_task_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        // No task initially
        let result = tool
            .execute(serde_json::json!({
                "action": "get_task",
                "session_id": "test-session"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(!result.output["has_task"].as_bool().unwrap());

        // Set task with session ownership
        let result = tool
            .execute(serde_json::json!({
                "action": "set_task",
                "content": "Implementing vault feature",
                "session_id": "test-session"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["status"], "set");

        // Get task with same session
        let result = tool
            .execute(serde_json::json!({
                "action": "get_task",
                "session_id": "test-session"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output["has_task"].as_bool().unwrap());
        assert!(result.output["content"].as_str().unwrap().contains("vault"));
        assert_eq!(result.output["ownership"].as_str().unwrap(), "OwnedBySelf");

        // Clear task
        let result = tool
            .execute(serde_json::json!({ "action": "clear_task" }))
            .await
            .unwrap();
        assert!(result.success);

        // Verify cleared
        let result = tool
            .execute(serde_json::json!({ "action": "get_task" }))
            .await
            .unwrap();
        assert!(!result.output["has_task"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn append_memory_hard_errors_without_store() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "append_memory",
                "content": "## User Preferences\nKings prefers Rust over Go."
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(!dir.path().join("MEMORY.md").exists());
    }

    #[tokio::test]
    async fn append_memory_stores_to_vector_store() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");
        let tool = NotesTool::with_store(dir.path().to_path_buf(), Arc::clone(&store), agent_id);

        let result = tool
            .execute(serde_json::json!({
                "action": "append_memory",
                "content": "Kings prefers Rust over Go."
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(!dir.path().join("MEMORY.md").exists());

        let results = store
            .recall("Rust", MemoryFilter::system_unscoped())
            .await
            .unwrap();
        assert!(results.iter().any(|r| r.memory.content.contains("Rust")));
    }

    #[tokio::test]
    async fn write_daily_requires_content() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({ "action": "write_daily" }))
            .await
            .unwrap();

        assert!(!result.success);
    }

    #[tokio::test]
    async fn unknown_action() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({ "action": "destroy" }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    // ── Session Task Tool Tests ─────────────────────────────────────────

    #[tokio::test]
    async fn session_task_add_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());
        let sid = "test-session-1";

        CURRENT_USER_ID
            .scope("test-user".to_string(), async move {
                let result = tool
                    .execute(serde_json::json!({
                        "action": "add_task",
                        "content": "Implement vault feature",
                        "session_id": sid
                    }))
                    .await
                    .unwrap();
                assert!(result.success);
                assert_eq!(result.output["task_id"], "1");

                let result = tool
                    .execute(serde_json::json!({
                        "action": "add_task",
                        "content": "Fix login bug",
                        "session_id": sid
                    }))
                    .await
                    .unwrap();
                assert!(result.success);
                assert_eq!(result.output["task_id"], "2");

                let result = tool
                    .execute(serde_json::json!({
                        "action": "list_tasks",
                        "session_id": sid
                    }))
                    .await
                    .unwrap();
                assert!(result.success);
                assert_eq!(result.output["count"], 2);
            })
            .await;
    }

    #[tokio::test]
    async fn session_task_complete() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());
        let sid = "test-session-2";

        CURRENT_USER_ID
            .scope("test-user".to_string(), async move {
                tool.execute(serde_json::json!({
                    "action": "add_task",
                    "content": "Deploy staging",
                    "session_id": sid
                }))
                .await
                .unwrap();

                let result = tool
                    .execute(serde_json::json!({
                        "action": "complete_task",
                        "task_id": "1",
                        "session_id": sid
                    }))
                    .await
                    .unwrap();
                assert!(result.success);
                assert_eq!(result.output["status"], "completed");

                // Completed tasks are filtered out of the default listing.
                let result = tool
                    .execute(serde_json::json!({
                        "action": "list_tasks",
                        "session_id": sid
                    }))
                    .await
                    .unwrap();
                assert_eq!(result.output["count"], 0);
                assert_eq!(result.output["total_count"], 1);

                // Caller can opt in to see them.
                let result = tool
                    .execute(serde_json::json!({
                        "action": "list_tasks",
                        "session_id": sid,
                        "include_completed": true,
                    }))
                    .await
                    .unwrap();
                let tasks = result.output["tasks"].as_array().unwrap();
                assert_eq!(tasks[0]["status"], "completed");
            })
            .await;
    }

    #[tokio::test]
    async fn list_all_tasks_filters_completed_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        for (sid, content) in [
            ("session-a", "Active A"),
            ("session-b", "To complete"),
            ("session-c", "Active C"),
        ] {
            tool.execute(serde_json::json!({
                "action": "add_task",
                "content": content,
                "session_id": sid,
            }))
            .await
            .unwrap();
        }
        tool.execute(serde_json::json!({
            "action": "complete_task",
            "task_id": "1",
            "session_id": "session-b",
        }))
        .await
        .unwrap();

        // Default: completed sessions drop out entirely.
        let result = tool
            .execute(serde_json::json!({ "action": "list_all_tasks" }))
            .await
            .unwrap();
        assert_eq!(result.output["total_tasks"], 2);
        assert_eq!(result.output["total_count_including_filtered"], 3);
        let sids: Vec<&str> = result.output["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap())
            .collect();
        assert!(!sids.contains(&"session-b"));

        // Opt-in restores the completed-task session.
        let result = tool
            .execute(serde_json::json!({
                "action": "list_all_tasks",
                "include_completed": true,
            }))
            .await
            .unwrap();
        assert_eq!(result.output["total_tasks"], 3);
        assert_eq!(result.output["sessions"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn session_task_list_all() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        tool.execute(serde_json::json!({
            "action": "add_task",
            "content": "Task A",
            "session_id": "session-a"
        }))
        .await
        .unwrap();

        tool.execute(serde_json::json!({
            "action": "add_task",
            "content": "Task B",
            "session_id": "session-b"
        }))
        .await
        .unwrap();

        let result = tool
            .execute(serde_json::json!({ "action": "list_all_tasks" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["total_tasks"], 2);
        assert_eq!(result.output["sessions"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn session_task_move() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        CURRENT_USER_ID
            .scope("test-user".to_string(), async move {
                tool.execute(serde_json::json!({
                    "action": "add_task",
                    "content": "Deploy staging",
                    "session_id": "session-a"
                }))
                .await
                .unwrap();

                let result = tool
                    .execute(serde_json::json!({
                        "action": "move_task",
                        "task_id": "1",
                        "session_id": "session-a",
                        "target_session_id": "session-b"
                    }))
                    .await
                    .unwrap();
                assert!(result.success);
                assert_eq!(result.output["to_session"], "session-b");

                let result = tool
                    .execute(serde_json::json!({
                        "action": "list_tasks",
                        "session_id": "session-a"
                    }))
                    .await
                    .unwrap();
                assert_eq!(result.output["count"], 0);

                let result = tool
                    .execute(serde_json::json!({
                        "action": "list_tasks",
                        "session_id": "session-b"
                    }))
                    .await
                    .unwrap();
                assert_eq!(result.output["count"], 1);
            })
            .await;
    }

    /// Regression test for DW-227 / DW-228: verify that add_task and list_tasks
    /// resolve the session ID from the task-local when no explicit session_id is
    /// passed (the exact scenario that was broken before the fix).
    #[tokio::test]
    async fn session_task_uses_task_local_session_id() {
        use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY};

        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let sid = "task-local-session-abc";

        // Run inside a task-local scope (simulating what the runner does)
        CURRENT_SESSION_KEY
            .scope(
                "slack:ws:C123:thread_ts".to_string(),
                CURRENT_SESSION_ID.scope(sid.to_string(), async {
                    // Add task WITHOUT explicit session_id — should use task-local
                    let result = tool
                        .execute(serde_json::json!({
                            "action": "add_task",
                            "content": "Task from task-local context"
                        }))
                        .await
                        .unwrap();
                    assert!(result.success, "add_task should succeed via task-local");
                    assert_eq!(result.output["session_id"], sid);
                    assert_eq!(result.output["task_id"], "1");

                    // List tasks WITHOUT explicit session_id — should find the task
                    let result = tool
                        .execute(serde_json::json!({
                            "action": "list_tasks"
                        }))
                        .await
                        .unwrap();
                    assert!(result.success, "list_tasks should succeed via task-local");
                    assert_eq!(result.output["count"], 1);
                    assert_eq!(result.output["session_id"], sid);

                    // Complete task WITHOUT explicit session_id
                    let result = tool
                        .execute(serde_json::json!({
                            "action": "complete_task",
                            "task_id": "1"
                        }))
                        .await
                        .unwrap();
                    assert!(
                        result.success,
                        "complete_task should succeed via task-local"
                    );
                }),
            )
            .await;
    }

    /// Verify that without a task-local AND without explicit session_id,
    /// session task actions return a clear error (not silently empty results).
    #[tokio::test]
    async fn session_task_fails_without_session_context() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        // No task-local scope, no explicit session_id
        let result = tool
            .execute(serde_json::json!({
                "action": "add_task",
                "content": "Orphan task"
            }))
            .await
            .unwrap();
        assert!(!result.success, "should fail without session context");

        let result = tool
            .execute(serde_json::json!({
                "action": "list_tasks"
            }))
            .await
            .unwrap();
        assert!(!result.success, "should fail without session context");
    }

    #[tokio::test]
    async fn write_human_appends_to_daily_notes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "write_human",
                "content": "Kings mentioned he was tired today."
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["status"], "written");
        assert_eq!(result.output["store"], "file");
    }

    #[tokio::test]
    async fn write_technical_creates_compaction_log() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "write_technical",
                "content": "Deployed v2.3.1 to staging."
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["status"], "written");
        assert_eq!(result.output["store"], "file");

        let compaction_dir = dir.path().join("memory").join("compaction");
        assert!(compaction_dir.exists());
        let entries: Vec<_> = std::fs::read_dir(&compaction_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);

        let content = std::fs::read_to_string(entries[0].as_ref().unwrap().path()).unwrap();
        assert!(content.contains("Deployed v2.3.1"));
        assert!(content.contains("Technical Note"));
    }

    #[tokio::test]
    async fn write_human_requires_content() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({ "action": "write_human" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn write_technical_requires_content() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({ "action": "write_technical" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    // ── DW-457: Vector Store Integration Tests ──────────────────────────

    #[tokio::test]
    async fn write_human_indexes_to_vector_store() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");
        let tool = NotesTool::with_store(dir.path().to_path_buf(), Arc::clone(&store), agent_id);

        let result = tool
            .execute(serde_json::json!({
                "action": "write_human",
                "content": "Kings mentioned he enjoys Rust programming."
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["status"], "written");
        assert_eq!(result.output["store"], "weaviate");

        let filter = MemoryFilter {
            layer: Some(MemoryLayer::Episodic),
            ..MemoryFilter::system_unscoped()
        };
        let results = store.recall("Rust programming", filter).await.unwrap();
        assert!(
            !results.is_empty(),
            "vector store should contain the indexed memory"
        );
        let memory = &results[0].memory;
        assert!(memory.content.contains("Kings mentioned he enjoys Rust"));
        assert_eq!(memory.layer, MemoryLayer::Episodic);
        assert!(
            memory.tags.contains(&"source:human".to_string()),
            "should have source tag, got: {:?}",
            memory.tags
        );
        assert!(
            memory.tags.iter().any(|t| t.starts_with("date:")),
            "should have date tag"
        );
    }

    #[tokio::test]
    async fn write_human_succeeds_without_store() {
        // Regression: NotesTool::new() (no store) must still work correctly.
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());

        let result = tool
            .execute(serde_json::json!({
                "action": "write_human",
                "content": "Session notes without vector store."
            }))
            .await
            .unwrap();

        assert!(result.success, "write_human should succeed without a store");
        assert_eq!(result.output["status"], "written");
        assert_eq!(result.output["store"], "file");

        // Verify the file was actually written
        let mgr = MemoryManager::new(dir.path());
        let path = mgr.daily_note_path();
        assert!(path.exists(), "daily note file should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Session notes without vector store"));
    }

    #[tokio::test]
    async fn write_human_vector_tags_include_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
        let agent_id = AgentId::new("test-agent");
        let tool = NotesTool::with_store(dir.path().to_path_buf(), Arc::clone(&store), agent_id);

        tool.execute(serde_json::json!({
            "action": "write_human",
            "content": "Reviewed deployment architecture with the team."
        }))
        .await
        .unwrap();

        // Check that tags contain required metadata
        let filter = MemoryFilter {
            layer: Some(MemoryLayer::Episodic),
            ..MemoryFilter::system_unscoped()
        };
        let results = store
            .recall("deployment architecture", filter)
            .await
            .unwrap();
        assert!(!results.is_empty());
        let tags = &results[0].memory.tags;

        assert!(
            tags.contains(&"source:human".to_string()),
            "tags must contain source:human, got: {:?}",
            tags
        );

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        assert!(
            tags.contains(&format!("date:{}", today)),
            "tags must contain date:{}, got: {:?}",
            today,
            tags
        );
    }

    // ─── write_profile ───────────────────────────────────────────────────

    #[tokio::test]
    async fn write_profile_errors_when_no_user_id_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "action": "write_profile",
                "content": "user is X"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("user context"), "got: {err}");
    }

    #[tokio::test]
    async fn write_profile_errors_on_missing_content() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());
        let workspace = dir.path().to_path_buf();
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("uid-a".to_string(), async {
                tool.execute(serde_json::json!({ "action": "write_profile" }))
                    .await
                    .unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("content"));
        assert!(!workspace
            .join("memory")
            .join("users")
            .join("uid-a")
            .join("profile.md")
            .exists());
    }

    #[tokio::test]
    async fn write_profile_saves_to_per_user_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());
        let workspace = dir.path().to_path_buf();
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("uid-a".to_string(), async {
                tool.execute(serde_json::json!({
                    "action": "write_profile",
                    "content": "Kings tends to overeat at dinner."
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(result.success);
        let stored = std::fs::read_to_string(
            workspace
                .join("memory")
                .join("users")
                .join("uid-a")
                .join("profile.md"),
        )
        .unwrap();
        assert!(stored.contains("overeat"));
    }

    #[tokio::test]
    async fn write_profile_truncates_to_cap() {
        let dir = tempfile::tempdir().unwrap();
        let tool = NotesTool::new(dir.path().to_path_buf());
        let workspace = dir.path().to_path_buf();
        let long: String = (0..500)
            .map(|i| format!("line number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = workforce_agent::runner::CURRENT_USER_ID
            .scope("uid-a".to_string(), async {
                tool.execute(serde_json::json!({
                    "action": "write_profile",
                    "content": long.clone()
                }))
                .await
                .unwrap()
            })
            .await;
        assert!(result.success);
        let stored = std::fs::read_to_string(
            workspace
                .join("memory")
                .join("users")
                .join("uid-a")
                .join("profile.md"),
        )
        .unwrap();
        assert!(
            stored.len() <= crate::profile::MAX_PROFILE_SIZE + 1,
            "profile exceeded cap: {} bytes",
            stored.len()
        );
    }
}
