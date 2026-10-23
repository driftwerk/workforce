use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use tracing::info;
use workforce_agent::tools::config_guard;
use workforce_agent::tools::paths::CanonicalRoots;

/// Default stale timeout for active tasks (in hours).
/// Tasks owned by a session older than this are treated as unclaimed.
const DEFAULT_STALE_TIMEOUT_HOURS: i64 = 4;

/// Metadata about active task ownership.
///
/// Stored as a JSON sidecar file alongside `active-task.md` to track which
/// session owns the task, preventing concurrent sessions from both auto-resuming
/// the same work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTaskMeta {
    /// Session ID that owns this task.
    pub session_id: Option<String>,
    /// When the task was last set or updated.
    pub updated_at: DateTime<Utc>,
}

impl ActiveTaskMeta {
    /// Create metadata with session ownership.
    pub fn with_session(session_id: &str) -> Self {
        Self {
            session_id: Some(session_id.to_string()),
            updated_at: Utc::now(),
        }
    }

    /// Create metadata without session ownership (legacy / unclaimed).
    pub fn unclaimed() -> Self {
        Self {
            session_id: None,
            updated_at: Utc::now(),
        }
    }

    /// Check if this task is stale (older than the given timeout).
    pub fn is_stale(&self, timeout_hours: i64) -> bool {
        let age = Utc::now() - self.updated_at;
        age.num_hours() >= timeout_hours
    }

    /// Check if this task is owned by the given session.
    pub fn is_owned_by(&self, session_id: &str) -> bool {
        self.session_id.as_deref() == Some(session_id)
    }
}

/// The result of reading an active task, including its ownership status.
#[derive(Debug, Clone)]
pub struct ActiveTaskInfo {
    /// The task content (from active-task.md).
    pub content: String,
    /// Ownership metadata (from active-task.meta.json), if available.
    pub meta: Option<ActiveTaskMeta>,
}

impl ActiveTaskInfo {
    /// Determine the ownership status of this task relative to a session.
    pub fn ownership_status(&self, current_session_id: Option<&str>) -> ActiveTaskOwnership {
        let Some(ref meta) = self.meta else {
            // No metadata file — legacy task, treat as unclaimed
            return ActiveTaskOwnership::Unclaimed;
        };

        let Some(ref owner_session) = meta.session_id else {
            return ActiveTaskOwnership::Unclaimed;
        };

        // Check if stale first
        if meta.is_stale(DEFAULT_STALE_TIMEOUT_HOURS) {
            return ActiveTaskOwnership::Stale {
                session_id: owner_session.clone(),
            };
        }

        // Check if owned by current session
        if let Some(current) = current_session_id {
            if owner_session == current {
                return ActiveTaskOwnership::OwnedBySelf;
            }
        }

        ActiveTaskOwnership::OwnedByOther {
            session_id: owner_session.clone(),
        }
    }
}

/// Ownership status of an active task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveTaskOwnership {
    /// No ownership metadata — legacy or explicitly unclaimed.
    Unclaimed,
    /// Owned by the current session.
    OwnedBySelf,
    /// Owned by a different active session.
    OwnedByOther { session_id: String },
    /// Owned by a session, but the claim is stale (session probably ended).
    Stale { session_id: String },
}

/// Persistent memory manager — handles daily notes and active tasks.
pub struct MemoryManager {
    workspace: PathBuf,
    /// Optional user scope — when set, memory is isolated under `memory/users/{user_id}/`.
    user_scope: Option<String>,
}

impl MemoryManager {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            user_scope: None,
        }
    }

    /// Create a user-scoped memory manager.
    ///
    /// Writes daily notes and active tasks under `memory/users/{user_id}/`
    /// instead of the shared `memory/` directory.
    pub fn for_user(workspace: &Path, user_id: &str) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            user_scope: Some(user_id.to_string()),
        }
    }

    /// Base directory for memory files. User-scoped when `user_scope` is set.
    pub fn memory_base(&self) -> PathBuf {
        match &self.user_scope {
            Some(uid) => self.workspace.join("memory").join("users").join(uid),
            None => self.workspace.join("memory"),
        }
    }

    /// Path to the user's learned profile file.
    pub fn profile_path(&self) -> PathBuf {
        self.memory_base().join("profile.md")
    }

    /// Write `content` to `path` in the workspace. The agent uid writes the
    /// memory tree too and could leave a link in it, so the write goes
    /// through [`config_guard`], which never lands on the agent's
    /// configuration. A person's notes are the hub's own state, which the
    /// agent's tools are kept out of and this is not.
    fn write(&self, path: &Path, content: &str) -> Result<()> {
        let roots = CanonicalRoots::for_hub_state(std::slice::from_ref(&self.workspace));
        let real = roots
            .resolve(path)
            .ok_or_else(|| eyre::eyre!("{} leads outside the workspace", path.display()))?;
        if let Some(dir) = real.parent() {
            config_guard::create_dirs(&roots, dir)?;
        }
        Ok(config_guard::write(&roots, &real, content.as_bytes())?)
    }

    /// Ensure the memory directory structure exists.
    pub fn init(&self) -> Result<()> {
        let memory_dir = self.memory_base();
        std::fs::create_dir_all(&memory_dir)?;
        Ok(())
    }

    /// Get today's daily note path.
    pub fn daily_note_path(&self) -> PathBuf {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        self.memory_base().join(format!("{}.md", today))
    }

    /// Append to today's daily note.
    pub fn append_daily(&self, content: &str) -> Result<()> {
        self.init()?;
        let path = self.daily_note_path();

        if !path.exists() {
            let today = Utc::now().format("%Y-%m-%d").to_string();
            self.write(&path, &format!("# {}\n\n", today))?;
        }

        let mut existing = std::fs::read_to_string(&path)?;
        existing.push_str(content);
        existing.push('\n');
        self.write(&path, &existing)?;

        Ok(())
    }

    /// Directory for compaction flush output (separate from daily notes).
    pub fn compaction_dir(&self) -> PathBuf {
        self.memory_base().join("compaction")
    }

    /// Append to today's compaction log (separate from daily notes).
    ///
    /// Compaction flushes contain technical metadata (tool execution times,
    /// index stats, etc.) that would drown out real conversation content
    /// in daily notes. This writes to `memory/compaction/YYYY-MM-DD.md`.
    pub fn append_compaction(&self, content: &str) -> Result<()> {
        let dir = self.compaction_dir();
        std::fs::create_dir_all(&dir)?;

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let path = dir.join(format!("{}.md", today));

        if !path.exists() {
            self.write(&path, &format!("# Compaction Log — {}\n\n", today))?;
        }

        let mut existing = std::fs::read_to_string(&path)?;
        existing.push_str(content);
        existing.push('\n');
        self.write(&path, &existing)?;

        Ok(())
    }

    /// A day's note in this manager's own scope. Never the shared directory
    /// for a user-scoped manager: files there predate per-user scoping and
    /// hold whoever was talking that day (the prompt path reads strictly too).
    pub fn daily_note_path_for_date(&self, date: &str) -> PathBuf {
        self.memory_base().join(format!("{date}.md"))
    }

    /// Write to the active task file (backward compatible — no session ownership).
    pub fn set_active_task(&self, content: &str) -> Result<()> {
        self.set_active_task_with_session(content, None)
    }

    /// Write to the active task file with explicit session ownership.
    pub fn set_active_task_with_session(
        &self,
        content: &str,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.init()?;
        let task_path = self.memory_base().join("active-task.md");
        let meta_path = self.memory_base().join("active-task.meta.json");

        self.write(&task_path, content)?;

        // Write ownership metadata sidecar
        let meta = match session_id {
            Some(sid) => ActiveTaskMeta::with_session(sid),
            None => ActiveTaskMeta::unclaimed(),
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| eyre::eyre!("Failed to serialize active task metadata: {e}"))?;
        self.write(&meta_path, &meta_json)?;

        Ok(())
    }

    /// Read the active task file (content only, for backward compatibility).
    pub fn get_active_task(&self) -> Option<String> {
        let path = self.memory_base().join("active-task.md");
        let content = std::fs::read_to_string(path).ok()?;
        if content.trim().is_empty() {
            None
        } else {
            Some(content)
        }
    }

    /// Read the active task with full ownership info.
    pub fn get_active_task_info(&self) -> Option<ActiveTaskInfo> {
        let task_path = self.memory_base().join("active-task.md");
        let meta_path = self.memory_base().join("active-task.meta.json");

        let content = std::fs::read_to_string(task_path).ok()?;
        if content.trim().is_empty() {
            return None;
        }

        let meta = std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<ActiveTaskMeta>(&s).ok());

        Some(ActiveTaskInfo { content, meta })
    }

    /// Clear the active task (both content and metadata).
    pub fn clear_active_task(&self) -> Result<()> {
        let task_path = self.memory_base().join("active-task.md");
        let meta_path = self.memory_base().join("active-task.meta.json");

        if task_path.exists() {
            std::fs::remove_file(&task_path)?;
        }
        if meta_path.exists() {
            std::fs::remove_file(&meta_path)?;
        }
        Ok(())
    }

    /// Search memory files for a query (token-based keyword search).
    ///
    /// Splits the query into individual tokens and matches lines containing
    /// those tokens. Lines matching the full query as an exact substring score
    /// 1.0; otherwise the score is the fraction of query tokens found in the
    /// line. At least half the tokens must match to be included (reduces noise).
    pub fn search(&self, query: &str) -> Result<Vec<MemorySearchResult>> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();
        let query_terms: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|t| t.len() >= 2) // skip single-char noise ("a", "I", etc.)
            .collect();

        if query_terms.is_empty() {
            return Ok(results);
        }

        // Search daily notes
        let memory_dir = self.memory_base();
        if memory_dir.exists() {
            let mut entries: Vec<_> = std::fs::read_dir(&memory_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                .collect();
            entries.sort_by_key(|e| std::cmp::Reverse(e.path()));

            for entry in entries.iter().take(30) {
                self.search_file(&entry.path(), &query_lower, &query_terms, &mut results)?;
            }
        }

        // Search workspace-level md files
        for name in ["SOUL.md", "AGENTS.md", "TOOLS.md", "IDENTITY.md"] {
            self.search_file(
                &self.workspace.join(name),
                &query_lower,
                &query_terms,
                &mut results,
            )?;
        }

        Ok(results)
    }

    fn search_file(
        &self,
        path: &Path,
        query: &str,
        query_terms: &[&str],
        results: &mut Vec<MemorySearchResult>,
    ) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(path)?;
        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        let all_lines: Vec<&str> = content.lines().collect();

        let num_terms = query_terms.len();

        for (line_num, line) in all_lines.iter().enumerate() {
            let line_lower = line.to_lowercase();

            // Exact substring match → score 1.0
            if line_lower.contains(query) {
                let start = line_num.saturating_sub(2);
                let end = (line_num + 3).min(all_lines.len());
                let snippet = all_lines[start..end].join("\n");

                results.push(MemorySearchResult {
                    file: filename.clone(),
                    line: line_num + 1,
                    snippet,
                    score: 1.0,
                    matched_terms: num_terms,
                    total_terms: num_terms,
                });
                continue;
            }

            // Token-based matching → score = matched_count / total_count
            let matched = query_terms
                .iter()
                .filter(|term| line_lower.contains(**term))
                .count();

            if matched > 0 {
                // Require at least half the terms to match (reduces noise).
                // div_ceil ensures single-term queries need 1 match, 3-term need 2, etc.
                if matched >= num_terms.div_ceil(2) {
                    let score = matched as f64 / num_terms as f64;
                    let start = line_num.saturating_sub(2);
                    let end = (line_num + 3).min(all_lines.len());
                    let snippet = all_lines[start..end].join("\n");

                    results.push(MemorySearchResult {
                        file: filename.clone(),
                        line: line_num + 1,
                        snippet,
                        score,
                        matched_terms: matched,
                        total_terms: num_terms,
                    });
                }
            }
        }

        Ok(())
    }

    /// List recent daily notes.
    pub fn recent_daily_notes(&self, limit: usize) -> Result<Vec<PathBuf>> {
        let memory_dir = self.memory_base();
        if !memory_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries: Vec<PathBuf> = std::fs::read_dir(&memory_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|ext| ext == "md")
                    && p.file_stem()
                        .is_some_and(|s| s.to_string_lossy().starts_with("20"))
            })
            .collect();

        entries.sort_by(|a, b| b.cmp(a));
        entries.truncate(limit);
        Ok(entries)
    }

    /// Auto-archive old daily notes (compress into monthly summaries).
    pub fn archive_old_notes(&self, keep_days: u32) -> Result<usize> {
        let cutoff = Utc::now() - chrono::Duration::days(keep_days as i64);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        let mut archived = 0;

        let memory_dir = self.memory_base();
        let archive_dir = memory_dir.join("archive");

        if !memory_dir.exists() {
            return Ok(0);
        }

        for entry in std::fs::read_dir(&memory_dir)?.filter_map(|e| e.ok()) {
            let path = entry.path();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            // Only archive date-named files older than cutoff
            if stem.starts_with("20") && stem < cutoff_str {
                std::fs::create_dir_all(&archive_dir)?;
                let dest = archive_dir.join(entry.file_name());
                std::fs::rename(&path, &dest)?;
                archived += 1;
            }
        }

        if archived > 0 {
            info!(count = archived, "Archived old daily notes");
        }

        Ok(archived)
    }
}

// ── Per-Session Task List ──────────────────────────────────────────────────

/// Status of a per-session task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionTaskStatus {
    Active,
    Completed,
    Cancelled,
}

impl std::fmt::Display for SessionTaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Completed => write!(f, "completed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Deserialize a value that may be a string or integer into a String.
/// Handles JSON like `"id": 1` or `"id": "1"`.
fn deserialize_string_or_int<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrInt;

    impl<'de> de::Visitor<'de> for StringOrInt {
        type Value = String;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or integer")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
    }

    deserializer.deserialize_any(StringOrInt)
}

/// A lightweight task tracked per session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTask {
    #[serde(deserialize_with = "deserialize_string_or_int")]
    pub id: String,
    #[serde(alias = "content")]
    pub description: String,
    pub status: SessionTaskStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

impl MemoryManager {
    /// Path to a session's task list file. The id comes from the model, so
    /// one that is not a single name is refused rather than joined.
    pub fn session_tasks_path(&self, session_id: &str) -> Result<PathBuf> {
        if !workforce_common::config_files::is_plain_name(session_id) {
            eyre::bail!("'{session_id}' is not a session id");
        }
        Ok(self
            .memory_base()
            .join("sessions")
            .join(session_id)
            .join("tasks.json"))
    }

    /// Load tasks for a specific session.
    pub fn load_session_tasks(&self, session_id: &str) -> Result<Vec<SessionTask>> {
        let path = self.session_tasks_path(session_id)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let tasks: Vec<SessionTask> = serde_json::from_str(&content)
            .map_err(|e| eyre::eyre!("Failed to parse session tasks: {e}"))?;
        Ok(tasks)
    }

    /// Save tasks for a specific session.
    pub fn save_session_tasks(&self, session_id: &str, tasks: &[SessionTask]) -> Result<()> {
        let path = self.session_tasks_path(session_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(tasks)
            .map_err(|e| eyre::eyre!("Failed to serialize session tasks: {e}"))?;
        self.write(&path, &json)?;
        Ok(())
    }

    /// Add a new task to a session's task list. Returns the created task.
    pub fn add_session_task(&self, session_id: &str, description: &str) -> Result<SessionTask> {
        let mut tasks = self.load_session_tasks(session_id)?;

        // Auto-increment ID based on existing tasks
        let next_id = tasks
            .iter()
            .filter_map(|t| t.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;

        let task = SessionTask {
            id: next_id.to_string(),
            description: description.to_string(),
            status: SessionTaskStatus::Active,
            created_at: Utc::now(),
            completed_at: None,
        };
        tasks.push(task.clone());
        self.save_session_tasks(session_id, &tasks)?;
        Ok(task)
    }

    /// Mark a task as completed in a session.
    pub fn complete_session_task(&self, session_id: &str, task_id: &str) -> Result<()> {
        let mut tasks = self.load_session_tasks(session_id)?;
        let task = tasks.iter_mut().find(|t| t.id == task_id).ok_or_else(|| {
            eyre::eyre!("Task '{}' not found in session '{}'", task_id, session_id)
        })?;
        task.status = SessionTaskStatus::Completed;
        task.completed_at = Some(Utc::now());
        self.save_session_tasks(session_id, &tasks)?;
        Ok(())
    }

    /// Remove a task from a session.
    pub fn remove_session_task(&self, session_id: &str, task_id: &str) -> Result<()> {
        let mut tasks = self.load_session_tasks(session_id)?;
        let len_before = tasks.len();
        tasks.retain(|t| t.id != task_id);
        if tasks.len() == len_before {
            return Err(eyre::eyre!(
                "Task '{}' not found in session '{}'",
                task_id,
                session_id
            ));
        }
        self.save_session_tasks(session_id, &tasks)?;
        Ok(())
    }

    /// Move a task from one session to another. Returns the moved task.
    pub fn move_session_task(
        &self,
        source_session_id: &str,
        target_session_id: &str,
        task_id: &str,
    ) -> Result<SessionTask> {
        // Load source tasks and find the one to move
        let mut source_tasks = self.load_session_tasks(source_session_id)?;
        let idx = source_tasks
            .iter()
            .position(|t| t.id == task_id)
            .ok_or_else(|| {
                eyre::eyre!(
                    "Task '{}' not found in session '{}'",
                    task_id,
                    source_session_id
                )
            })?;
        let mut task = source_tasks.remove(idx);

        // Load target tasks, assign new ID, and add
        let mut target_tasks = self.load_session_tasks(target_session_id)?;
        let next_id = target_tasks
            .iter()
            .filter_map(|t| t.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        task.id = next_id.to_string();

        target_tasks.push(task.clone());

        // Save target first so on partial failure the task is duplicated
        // (recoverable) rather than lost.
        self.save_session_tasks(target_session_id, &target_tasks)?;
        self.save_session_tasks(source_session_id, &source_tasks)?;

        Ok(task)
    }

    /// List all sessions that have task files, with their tasks.
    /// Used for cross-session summaries and global task overview.
    pub fn list_all_session_tasks(&self) -> Result<Vec<(String, Vec<SessionTask>)>> {
        let sessions_dir = self.memory_base().join("sessions");
        let mut result = Vec::new();

        if !sessions_dir.exists() {
            return Ok(result);
        }

        for entry in std::fs::read_dir(&sessions_dir)?.filter_map(|e| e.ok()) {
            if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                continue;
            }
            let session_id = entry.file_name().to_string_lossy().to_string();
            let tasks_path = entry.path().join("tasks.json");
            if tasks_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&tasks_path) {
                    if let Ok(tasks) = serde_json::from_str::<Vec<SessionTask>>(&content) {
                        if !tasks.is_empty() {
                            result.push((session_id, tasks));
                        }
                    }
                }
            }
        }

        Ok(result)
    }
}

/// A search result from memory.
#[derive(Debug, Clone)]
pub struct MemorySearchResult {
    pub file: String,
    pub line: usize,
    pub snippet: String,
    pub score: f64,
    /// Number of query terms that matched this line.
    pub matched_terms: usize,
    /// Total number of query terms in the search.
    pub total_terms: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        mgr.append_daily("## Meeting Notes\nDiscussed X and Y.")
            .unwrap();

        let path = mgr.daily_note_path();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Meeting Notes"));
    }

    #[test]
    fn active_task_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        assert!(mgr.get_active_task().is_none());
        assert!(mgr.get_active_task_info().is_none());

        mgr.set_active_task("Working on OPS-150").unwrap();
        assert_eq!(mgr.get_active_task().unwrap(), "Working on OPS-150");

        // Verify metadata sidecar was created (unclaimed since no session_id)
        let info = mgr.get_active_task_info().unwrap();
        assert_eq!(info.content, "Working on OPS-150");
        assert!(info.meta.is_some());
        assert!(info.meta.as_ref().unwrap().session_id.is_none());

        mgr.clear_active_task().unwrap();
        assert!(mgr.get_active_task().is_none());
        assert!(mgr.get_active_task_info().is_none());

        // Verify metadata sidecar was also removed
        let meta_path = dir.path().join("memory").join("active-task.meta.json");
        assert!(!meta_path.exists());
    }

    #[test]
    fn active_task_with_session_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        mgr.set_active_task_with_session("Building feature X", Some("session-abc-123"))
            .unwrap();

        let info = mgr.get_active_task_info().unwrap();
        assert_eq!(info.content, "Building feature X");

        let meta = info.meta.as_ref().unwrap();
        assert_eq!(meta.session_id.as_deref(), Some("session-abc-123"));
        assert!(!meta.is_stale(DEFAULT_STALE_TIMEOUT_HOURS));
        assert!(meta.is_owned_by("session-abc-123"));
        assert!(!meta.is_owned_by("session-other"));
    }

    #[test]
    fn active_task_ownership_status() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        // Unclaimed task
        mgr.set_active_task("Task 1").unwrap();
        let info = mgr.get_active_task_info().unwrap();
        assert_eq!(
            info.ownership_status(Some("my-session")),
            ActiveTaskOwnership::Unclaimed
        );

        // Task owned by current session
        mgr.set_active_task_with_session("Task 2", Some("my-session"))
            .unwrap();
        let info = mgr.get_active_task_info().unwrap();
        assert_eq!(
            info.ownership_status(Some("my-session")),
            ActiveTaskOwnership::OwnedBySelf
        );

        // Task owned by another session
        assert!(matches!(
            info.ownership_status(Some("other-session")),
            ActiveTaskOwnership::OwnedByOther { .. }
        ));

        // Task with no current session provided
        assert!(matches!(
            info.ownership_status(None),
            ActiveTaskOwnership::OwnedByOther { .. }
        ));
    }

    #[test]
    fn active_task_stale_detection() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        mgr.set_active_task_with_session("Old task", Some("old-session"))
            .unwrap();

        // Manually write stale metadata
        let meta = ActiveTaskMeta {
            session_id: Some("old-session".to_string()),
            updated_at: Utc::now() - chrono::Duration::hours(5),
        };
        let meta_path = dir.path().join("memory").join("active-task.meta.json");
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

        let info = mgr.get_active_task_info().unwrap();
        assert!(matches!(
            info.ownership_status(Some("new-session")),
            ActiveTaskOwnership::Stale { .. }
        ));
    }

    #[test]
    fn active_task_no_meta_file_is_unclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        // Write task file without metadata (legacy scenario)
        std::fs::write(memory_dir.join("active-task.md"), "Legacy task").unwrap();

        let mgr = MemoryManager::new(dir.path());
        let info = mgr.get_active_task_info().unwrap();
        assert_eq!(info.content, "Legacy task");
        assert!(info.meta.is_none());
        assert_eq!(
            info.ownership_status(Some("any-session")),
            ActiveTaskOwnership::Unclaimed
        );
    }

    #[test]
    fn memory_search() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        // Create some memory files
        std::fs::write(
            dir.path().join("SOUL.md"),
            "# Memory\n\n## Kings prefers Rust\nHe also likes TypeScript.",
        )
        .unwrap();

        mgr.append_daily("Worked on workforce framework today.")
            .unwrap();

        let results = mgr.search("rust").unwrap();
        assert!(!results.is_empty());
        assert!(results[0].snippet.contains("Rust"));

        let results = mgr.search("workforce").unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn memory_search_multi_word_query() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        std::fs::write(
            dir.path().join("SOUL.md"),
            "# Memory\n\nDW-230: Session isolation bug in Slack threads\n\
             Prompt caching added to system, need tools to observe\n\
             Unrelated line about nothing relevant\n",
        )
        .unwrap();

        // Multi-word query should match via token-based search
        let results = mgr.search("session isolation bug").unwrap();
        assert!(!results.is_empty(), "multi-word query should find results");
        assert!(results[0].snippet.contains("Session isolation"));

        // Query with separator differences (DW-230 session isolation)
        let results = mgr.search("DW-230 session isolation").unwrap();
        assert!(
            !results.is_empty(),
            "should match even with different separators"
        );

        // Partial overlap query
        let results = mgr.search("prompt caching observability tools").unwrap();
        assert!(
            !results.is_empty(),
            "should match when at least half the terms are present"
        );
        // "prompt" and "caching" match (2/4 = half), "observability" and "tools" appear on the line too
        assert!(results[0].snippet.contains("Prompt caching"));
    }

    #[test]
    fn memory_search_token_scoring() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        std::fs::write(
            dir.path().join("SOUL.md"),
            "# Memory\n\n\
             Line about session and isolation and bug and thread keywords\n\
             Line about session and isolation only here\n",
        )
        .unwrap();

        // 4-term query: threshold = (4+1)/2 = 2
        let results = mgr.search("session isolation bug thread").unwrap();
        assert!(results.len() >= 2, "should have at least 2 results");

        // Line 3 (1-indexed) matches 4/4 terms → score 1.0
        let full_match = results
            .iter()
            .find(|r| r.line == 3)
            .expect("should find line 3");
        assert!((full_match.score - 1.0).abs() < f64::EPSILON);

        // Line 4 matches 2/4 terms → score 0.5, passes threshold (2 >= 2)
        let partial_match = results
            .iter()
            .find(|r| r.line == 4)
            .expect("should find line 4");
        assert!((partial_match.score - 0.5).abs() < f64::EPSILON);

        assert!(full_match.score > partial_match.score);
    }

    #[test]
    fn memory_search_no_false_positives() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        std::fs::write(
            dir.path().join("SOUL.md"),
            "# Memory\n\nThe weather is nice today\nI like to eat pizza\n",
        )
        .unwrap();

        // A 4-term query where only 1 term matches should not produce results
        // (threshold = (4+1)/2 = 2, only 1 match < 2)
        let results = mgr.search("session isolation debugging tools").unwrap();
        assert!(
            results.is_empty(),
            "unrelated content should not match: {:?}",
            results
        );
    }

    #[test]
    fn memory_search_single_char_terms_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        // Query with only single-char words should return nothing
        let results = mgr.search("a I").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn recent_daily_notes_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        std::fs::write(memory_dir.join("2026-02-08.md"), "day 1").unwrap();
        std::fs::write(memory_dir.join("2026-02-09.md"), "day 2").unwrap();
        std::fs::write(memory_dir.join("2026-02-10.md"), "day 3").unwrap();

        let mgr = MemoryManager::new(dir.path());
        let notes = mgr.recent_daily_notes(2).unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].to_string_lossy().contains("2026-02-10"));
    }

    // ── Session Task Tests ─────────────────────────────────────────────

    #[test]
    fn session_task_crud() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());
        let sid = "session-abc";

        // Empty initially
        let tasks = mgr.load_session_tasks(sid).unwrap();
        assert!(tasks.is_empty());

        // Add tasks
        let t1 = mgr
            .add_session_task(sid, "Implement vault feature")
            .unwrap();
        assert_eq!(t1.id, "1");
        assert_eq!(t1.status, SessionTaskStatus::Active);

        let t2 = mgr.add_session_task(sid, "Fix login bug").unwrap();
        assert_eq!(t2.id, "2");

        // List
        let tasks = mgr.load_session_tasks(sid).unwrap();
        assert_eq!(tasks.len(), 2);

        // Complete
        mgr.complete_session_task(sid, "1").unwrap();
        let tasks = mgr.load_session_tasks(sid).unwrap();
        assert_eq!(tasks[0].status, SessionTaskStatus::Completed);
        assert!(tasks[0].completed_at.is_some());

        // Remove
        mgr.remove_session_task(sid, "2").unwrap();
        let tasks = mgr.load_session_tasks(sid).unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn session_task_move() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        // Add task to session A
        mgr.add_session_task("session-a", "Deploy staging").unwrap();
        mgr.add_session_task("session-a", "Write tests").unwrap();

        // Add task to session B
        mgr.add_session_task("session-b", "Review PR").unwrap();

        // Move task 1 from A to B
        let moved = mgr
            .move_session_task("session-a", "session-b", "1")
            .unwrap();
        assert_eq!(moved.description, "Deploy staging");
        assert_eq!(moved.id, "2"); // Re-numbered in target

        // Verify source lost the task
        let a_tasks = mgr.load_session_tasks("session-a").unwrap();
        assert_eq!(a_tasks.len(), 1);
        assert_eq!(a_tasks[0].description, "Write tests");

        // Verify target has both
        let b_tasks = mgr.load_session_tasks("session-b").unwrap();
        assert_eq!(b_tasks.len(), 2);
        assert_eq!(b_tasks[1].description, "Deploy staging");
    }

    #[test]
    fn session_task_list_all() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        mgr.add_session_task("session-a", "Task A1").unwrap();
        mgr.add_session_task("session-b", "Task B1").unwrap();
        mgr.add_session_task("session-b", "Task B2").unwrap();

        let all = mgr.list_all_session_tasks().unwrap();
        assert_eq!(all.len(), 2);

        let total_tasks: usize = all.iter().map(|(_, tasks)| tasks.len()).sum();
        assert_eq!(total_tasks, 3);
    }

    #[test]
    fn session_task_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        assert!(mgr.complete_session_task("session-x", "99").is_err());
        assert!(mgr.remove_session_task("session-x", "99").is_err());
        assert!(mgr
            .move_session_task("session-x", "session-y", "99")
            .is_err());
    }

    /// The agent uid can leave a dangling link where the hub is about to
    /// write a note; following it would create the agent's configuration, or
    /// a learning plan that names who it runs as.
    #[cfg(unix)]
    #[test]
    fn a_planted_link_carries_no_note_into_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let mgr = MemoryManager::new(&workspace);
        mgr.init().unwrap();
        std::fs::create_dir(workspace.join("learning")).unwrap();
        std::os::unix::fs::symlink("../config.toml", mgr.daily_note_path()).unwrap();
        std::os::unix::fs::symlink(
            "../learning/plan.md",
            mgr.memory_base().join("active-task.md"),
        )
        .unwrap();

        assert!(mgr.append_daily("[access]\nmode = \"open\"").is_err());
        assert!(mgr.set_active_task("run_as_identity: admin").is_err());
        assert!(!workspace.join("config.toml").exists());
        assert!(!workspace.join("learning/plan.md").exists());
    }

    #[test]
    fn a_session_id_that_leaves_the_task_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());
        for sid in ["../../learning", "..", "a/b"] {
            assert!(mgr.add_session_task(sid, "plant").is_err(), "{sid}");
            assert!(mgr.load_session_tasks(sid).is_err(), "{sid}");
        }
        assert!(!dir.path().join("learning").exists());
    }

    /// Regression test for DW-233: tasks.json written by agent via exec tool
    /// uses "content" instead of "description" and integer id instead of string.
    #[test]
    fn session_task_parses_alternate_schema() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());
        let sid = "session-alt-schema";

        // Write the exact JSON format from the bug report
        let path = mgr.session_tasks_path(sid).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"[{"id":1,"content":"Test session isolation fixes","status":"active","created_at":"2026-02-15T04:26:58.015730591Z"}]"#,
        )
        .unwrap();

        // Should parse successfully despite schema differences
        let tasks = mgr.load_session_tasks(sid).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "1"); // integer 1 → String "1"
        assert_eq!(tasks[0].description, "Test session isolation fixes"); // "content" → description
        assert_eq!(tasks[0].status, SessionTaskStatus::Active);
    }

    /// Verify our canonical format still round-trips correctly.
    #[test]
    fn session_task_canonical_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());
        let sid = "session-roundtrip";

        let task = mgr.add_session_task(sid, "Test round-trip").unwrap();
        assert_eq!(task.id, "1");
        assert_eq!(task.description, "Test round-trip");

        let tasks = mgr.load_session_tasks(sid).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "1");
        assert_eq!(tasks[0].description, "Test round-trip");

        // Verify the JSON on disk uses canonical field names
        let content = std::fs::read_to_string(mgr.session_tasks_path(sid).unwrap()).unwrap();
        assert!(content.contains("\"description\""));
        assert!(!content.contains("\"content\""));
    }

    // ── Per-User Memory Isolation Tests (DW-147) ─────────────────────

    #[test]
    fn user_scoped_daily_path() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::for_user(dir.path(), "U12345");
        let path = mgr.daily_note_path();
        assert!(path.to_string_lossy().contains("memory/users/U12345"));
    }

    #[test]
    fn default_memory_base() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());
        let path = mgr.daily_note_path();
        assert!(!path.to_string_lossy().contains("users"));
        assert!(path.to_string_lossy().contains("memory"));
    }

    #[test]
    fn user_scoped_write_daily() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::for_user(dir.path(), "U12345");
        mgr.append_daily("Test note").unwrap();
        let content = std::fs::read_to_string(mgr.daily_note_path()).unwrap();
        assert!(content.contains("Test note"));
    }

    // ── Compaction Separation Tests (DW-437, DW-443) ─────────────────

    #[test]
    fn append_compaction_writes_to_separate_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        mgr.append_compaction("## Memory Flush\n\nTechnical details here.")
            .unwrap();

        // Should be in memory/compaction/, NOT memory/
        let compaction_dir = dir.path().join("memory").join("compaction");
        assert!(compaction_dir.exists());

        let entries: Vec<_> = std::fs::read_dir(&compaction_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);

        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(content.contains("Technical details here"));
    }

    #[test]
    fn append_compaction_does_not_pollute_daily_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MemoryManager::new(dir.path());

        mgr.append_daily("Real conversation content").unwrap();
        mgr.append_compaction("Technical flush output").unwrap();

        let daily_content = std::fs::read_to_string(mgr.daily_note_path()).unwrap();
        assert!(daily_content.contains("Real conversation content"));
        assert!(
            !daily_content.contains("Technical flush output"),
            "Compaction output should NOT appear in daily notes"
        );
    }

    // ── Daily Note Path Fallback Tests (DW-438, DW-442) ──────────────

    #[test]
    fn a_user_scoped_manager_never_reads_the_shared_daily_note() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("2026-03-14.md"), "Another user's note").unwrap();

        let mgr = MemoryManager::for_user(dir.path(), "U12345");
        let path = mgr.daily_note_path_for_date("2026-03-14");
        assert!(!path.exists());
        assert!(path.starts_with(dir.path().join("memory").join("users").join("U12345")));
    }

    #[test]
    fn daily_note_fallback_prefers_scoped_file() {
        let dir = tempfile::tempdir().unwrap();

        // Write to both scoped and unscoped
        let unscoped = dir.path().join("memory");
        std::fs::create_dir_all(&unscoped).unwrap();
        std::fs::write(unscoped.join("2026-03-14.md"), "Unscoped note").unwrap();

        let scoped = dir.path().join("memory").join("users").join("U12345");
        std::fs::create_dir_all(&scoped).unwrap();
        std::fs::write(scoped.join("2026-03-14.md"), "Scoped note").unwrap();

        // Should prefer the scoped version
        let mgr = MemoryManager::for_user(dir.path(), "U12345");
        let path = mgr.daily_note_path_for_date("2026-03-14");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "Scoped note");
    }

    #[test]
    fn daily_note_fallback_unscoped_manager() {
        let dir = tempfile::tempdir().unwrap();

        // Unscoped manager should just use the normal path
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("2026-03-14.md"), "Normal note").unwrap();

        let mgr = MemoryManager::new(dir.path());
        let path = mgr.daily_note_path_for_date("2026-03-14");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "Normal note");
    }
}
