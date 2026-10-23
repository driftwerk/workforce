//! Context management tools — let agents monitor and manage their context window.
//!
//! Tools:
//! - `context.status` — current context window usage, thresholds, and token trends
//! - `context.suggest` — heuristic-based suggestions for memory searches and context management
//! - `context.drop` — selective eviction of tool outputs from the context window

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::llm::model_limits;
use workforce_agent::tools::{Tool, ToolResult};

use workforce_common::session::{MessageRole, Session, SessionStore};

use crate::memory::MemoryManager;
use crate::serve::AgentService;

/// Resolve the most recently updated session, preferring the given
/// channel and falling back to any. Always returns a fully hydrated
/// session (not a meta-only shell) — `list()` may hand back shells, so
/// we re-fetch via `get()` to trigger lazy hydration before any callers
/// iterate `session.messages`.
fn most_recent_session(
    sessions: &Arc<dyn SessionStore>,
    channel: Option<&str>,
) -> Result<Option<Session>> {
    let id = sessions
        .list(channel, 1)?
        .into_iter()
        .next()
        .or_else(|| sessions.list(None, 1).ok().and_then(|mut v| v.pop()))
        .map(|s| s.id);
    match id {
        Some(id) => sessions.get(&id),
        None => Ok(None),
    }
}

/// How many of the session's newest messages follow its last `memory.recall`
/// call, judged by the tool rows' recorded calls.
fn messages_since_memory_recall(session: &Session) -> usize {
    session
        .messages
        .iter()
        .rev()
        .take_while(|m| {
            !(m.role == MessageRole::Tool
                && m.tool_data
                    .as_ref()
                    .is_some_and(|td| td["name"] == "memory" && td["input"]["action"] == "recall"))
        })
        .count()
}

// ─── context.status ──────────────────────────────────────────────────────────

/// Tool that shows current session context window usage, proximity to
/// compaction thresholds, and recent token trends.
pub struct ContextStatusTool {
    service: Arc<AgentService>,
}

impl ContextStatusTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self { service }
    }
}

#[derive(Deserialize)]
struct ContextStatusInput {
    /// Direct session ID.
    #[serde(default)]
    session_id: Option<String>,
    /// Channel key for session discovery.
    #[serde(default)]
    channel: Option<String>,
}

#[async_trait]
impl Tool for ContextStatusTool {
    fn name(&self) -> &str {
        "context.status"
    }

    fn description(&self) -> &str {
        "Check your current context window usage"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID to check (default: most recent session)"
                },
                "channel": {
                    "type": "string",
                    "description": "Channel key to find session (alternative to session_id)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ContextStatusInput =
            serde_json::from_value(input).unwrap_or(ContextStatusInput {
                session_id: None,
                channel: None,
            });

        let session = if let Some(session_id) = &input.session_id {
            match self.service.sessions.get(session_id)? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        format!("Session not found: {}", session_id),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            let channel = input.channel.as_deref().unwrap_or("unknown");
            match most_recent_session(&self.service.sessions, Some(channel))? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        "No active session found",
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        };

        let resolved_model = self.service.resolve_model(&session, None, None);
        let config = self.service.compaction_config_for(&resolved_model).await;
        let effective_max = config.max_context_tokens;

        let estimated_tokens = session.estimated_total_tokens();
        let threshold = (effective_max as f32 * config.compaction_threshold) as u32;
        let tokens_remaining = effective_max.saturating_sub(estimated_tokens);
        let usage_pct = (estimated_tokens as f64 / effective_max as f64 * 100.0).min(100.0);

        let needs_compaction = estimated_tokens > threshold;
        let urgent_threshold = (effective_max as f32 * config.urgent_compaction_threshold) as u32;
        let needs_urgent = estimated_tokens > urgent_threshold;
        let zone = if needs_urgent {
            "red"
        } else if needs_compaction {
            "yellow"
        } else {
            "green"
        };

        // Count messages since last compaction
        let messages_since_compaction = if let Some(last) = session.compaction_history.last() {
            session
                .messages
                .iter()
                .filter(|m| m.timestamp > last.timestamp)
                .count()
        } else {
            session.messages.len()
        };

        // Recent token trend: estimate tokens per message for the last 5 messages
        let recent_trend: Vec<u32> = session
            .messages
            .iter()
            .rev()
            .take(5)
            .map(|m| m.tokens)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "session_id": session.id,
                "estimated_tokens": estimated_tokens,
                "max_tokens": config.max_context_tokens,
                "model_context_limit": model_limits::model_limits(&resolved_model).map(|l| l.context_tokens),
                "effective_max_tokens": effective_max,
                "tokens_remaining": tokens_remaining,
                "usage_pct": format!("{:.1}%", usage_pct),
                "zone": zone,
                "compaction_threshold": threshold,
                "message_count": session.message_count(),
                "messages_since_last_compaction": messages_since_compaction,
                "recent_token_trend": recent_trend,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── context.suggest ─────────────────────────────────────────────────────────

/// Tool that analyzes the current session and provides proactive suggestions
/// for memory searches, context management, and task tracking.
pub struct ContextSuggestTool {
    service: Arc<AgentService>,
    workspace: PathBuf,
}

impl ContextSuggestTool {
    pub fn new(service: Arc<AgentService>, workspace: PathBuf) -> Self {
        Self { service, workspace }
    }
}

#[derive(Deserialize)]
struct ContextSuggestInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    channel: Option<String>,
}

#[async_trait]
impl Tool for ContextSuggestTool {
    fn name(&self) -> &str {
        "context.suggest"
    }

    fn description(&self) -> &str {
        "Get proactive suggestions for context management"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID to analyze (default: most recent session)"
                },
                "channel": {
                    "type": "string",
                    "description": "Channel key to find session"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ContextSuggestInput =
            serde_json::from_value(input).unwrap_or(ContextSuggestInput {
                session_id: None,
                channel: None,
            });

        let session = if let Some(session_id) = &input.session_id {
            match self.service.sessions.get(session_id)? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        format!("Session not found: {}", session_id),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            let channel = input.channel.as_deref().unwrap_or("unknown");
            match most_recent_session(&self.service.sessions, Some(channel))? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        "No active session found",
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        };

        let mut suggestions: Vec<serde_json::Value> = Vec::new();

        // 1. Context pressure check
        let config = self.service.current_compaction_config();
        let estimated_tokens = session.estimated_total_tokens();
        let usage_pct = estimated_tokens as f64 / config.max_context_tokens as f64 * 100.0;

        if usage_pct > 60.0 {
            suggestions.push(serde_json::json!({
                "type": "context_pressure",
                "reason": format!(
                    "Context at {:.0}% capacity ({}/{}k tokens)",
                    usage_pct,
                    estimated_tokens,
                    config.max_context_tokens / 1000
                ),
                "action": "compaction.compact",
                "priority": if usage_pct > 80.0 { "high" } else { "medium" },
            }));
        }

        // 2. Extract potential search terms from recent messages
        let recent_text: String = session
            .messages
            .iter()
            .rev()
            .take(10)
            .filter(|m| m.role == MessageRole::User || m.role == MessageRole::Assistant)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        // Look for ticket IDs (e.g., DW-123, PROJ-456)
        let ticket_pattern = regex::Regex::new(r"\b[A-Z]{2,6}-\d{1,6}\b")
            .unwrap_or_else(|_| regex::Regex::new(r"NOMATCH").unwrap());
        for cap in ticket_pattern.find_iter(&recent_text) {
            let ticket = cap.as_str();
            suggestions.push(serde_json::json!({
                "type": "memory_search",
                "reason": format!("Recent messages reference '{ticket}'"),
                "action": "memory.recall",
                "params": { "query": ticket },
                "priority": "medium",
            }));
        }

        // 3. Check if memory.recall was used recently
        let messages_since_search = messages_since_memory_recall(&session);

        if messages_since_search > 15 && session.messages.len() > 15 {
            suggestions.push(serde_json::json!({
                "type": "stale_context",
                "reason": format!(
                    "No memory.recall in the last {} messages — context may be stale",
                    messages_since_search
                ),
                "action": "memory.recall",
                "priority": "low",
            }));
        }

        // 4. Active task check
        let mgr = match workforce_agent::runner::CURRENT_USER_ID.try_with(|u| u.clone()) {
            Ok(uid) if !uid.is_empty() => MemoryManager::for_user(&self.workspace, &uid),
            _ => MemoryManager::new(&self.workspace),
        };
        let has_active_task = mgr.get_active_task().is_some();

        if !has_active_task && session.messages.len() > 5 {
            suggestions.push(serde_json::json!({
                "type": "task_tracking",
                "reason": "No active task set — consider tracking current work",
                "action": "notes.set_task",
                "priority": "low",
            }));
        }

        // Deduplicate ticket suggestions (keep first occurrence)
        let mut seen_tickets = std::collections::HashSet::new();
        suggestions.retain(|s| {
            if s["type"] == "memory_search" {
                let query = s["params"]["query"].as_str().unwrap_or("");
                seen_tickets.insert(query.to_string())
            } else {
                true
            }
        });

        // Limit suggestions
        suggestions.truncate(5);

        Ok(ToolResult::success(
            serde_json::json!({
                "suggestions": suggestions,
                "suggestion_count": suggestions.len(),
                "context_health": {
                    "usage_pct": (usage_pct * 10.0).round() / 10.0,
                    "message_count": session.messages.len(),
                    "messages_since_memory_search": messages_since_search,
                    "has_active_task": has_active_task,
                },
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ─── context.drop ────────────────────────────────────────────────────────────

/// Tool that lets agents selectively evict tool outputs from the context window.
/// Supports filtering by tool name, round number, with keep_last protection.
pub struct ContextDropTool {
    service: Arc<AgentService>,
}

impl ContextDropTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self { service }
    }
}

#[derive(Deserialize)]
struct ContextDropInput {
    /// Action: "drop_tool_outputs" or "drop_by_round".
    action: String,
    /// Filter eviction to only these tool names.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Only evict outputs from rounds (message indices) before this number.
    #[serde(default)]
    older_than_round: Option<usize>,
    /// Always preserve the N most recent outputs per tool type (default: 2).
    #[serde(default = "default_keep_last")]
    keep_last: usize,
    /// Skip evicting outputs smaller than this many chars (default: 500).
    #[serde(default = "default_min_chars")]
    min_chars: usize,
    /// Direct session ID lookup.
    #[serde(default)]
    session_id: Option<String>,
    /// Channel key with platform prefix for session discovery.
    #[serde(default)]
    session_channel: Option<String>,
    /// Raw channel ID fallback.
    #[serde(default)]
    channel: Option<String>,
}

fn default_keep_last() -> usize {
    2
}
fn default_min_chars() -> usize {
    500
}

#[async_trait]
impl Tool for ContextDropTool {
    fn name(&self) -> &str {
        "context.drop"
    }

    fn description(&self) -> &str {
        "Selectively evict tool outputs from context window"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform",
                    "enum": ["drop_tool_outputs", "drop_by_round"]
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter eviction to these tool names only"
                },
                "older_than_round": {
                    "type": "integer",
                    "description": "Only evict outputs from rounds (message indices) before"
                },
                "keep_last": {
                    "type": "integer",
                    "description": "Always preserve the N most recent outputs per tool type"
                },
                "min_chars": {
                    "type": "integer",
                    "description": "Skip evicting outputs smaller than this many chars (default"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID for direct lookup (preferred)"
                },
                "session_channel": {
                    "type": "string",
                    "description": "Session channel key with platform prefix"
                },
                "channel": {
                    "type": "string",
                    "description": "Raw channel ID (fallback)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ContextDropInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Session lookup — same pattern as CompactionTool / ContextStatusTool
        let mut session = if let Some(session_id) = &input.session_id {
            match self.service.sessions.get(session_id)? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        format!("Session not found: {}", session_id),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            let channel = input
                .session_channel
                .as_deref()
                .or(input.channel.as_deref())
                .unwrap_or("unknown");
            match most_recent_session(&self.service.sessions, Some(channel))? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        "No active session found",
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        };

        let result = match input.action.as_str() {
            "drop_tool_outputs" => {
                let filter = input.tools.as_deref();
                session.evict_tool_outputs(
                    filter,
                    input.older_than_round,
                    input.keep_last,
                    input.min_chars,
                )
            }
            "drop_by_round" => {
                let before_round = match input.older_than_round {
                    Some(r) => r,
                    None => {
                        return Ok(ToolResult::failure(
                            "drop_by_round requires 'older_than_round' parameter",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                session.evict_by_round(before_round, input.min_chars)
            }
            other => {
                return Ok(ToolResult::failure(
                    format!(
                        "Unknown action '{}'. Supported: drop_tool_outputs, drop_by_round",
                        other
                    ),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Calculate context usage after eviction
        let resolved_model = self.service.resolve_model(&session, None, None);
        let config = self.service.compaction_config_for(&resolved_model).await;
        let effective_max = config.max_context_tokens;
        let estimated_tokens = session.estimated_total_tokens();
        let usage_pct = (estimated_tokens as f64 / effective_max as f64 * 100.0).min(100.0);

        // Persist the modified session
        if result.evicted > 0 {
            self.service.sessions.save(&session)?;
        }

        Ok(ToolResult::success(
            serde_json::json!({
                "evicted": result.evicted,
                "kept": result.kept,
                "tokens_freed_estimate": result.tokens_freed,
                "context_usage_after": format!("{:.1}%", usage_pct),
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::SessionMessage;

    #[test]
    fn only_a_memory_recall_call_counts_as_a_recall() {
        let tool = |name: &str, input: serde_json::Value| {
            SessionMessage::tool(name, &input, &serde_json::json!("ok"), true, 3, None)
        };
        let mut session = Session::new("c", "U1", "agent");
        session.push_message(tool(
            "memory",
            serde_json::json!({ "action": "recall", "query": "x" }),
        ));
        session.add_user_message("did you look?", None);
        session.push_message(tool(
            "memory",
            serde_json::json!({ "action": "remember", "content": "y" }),
        ));
        session.push_message(tool(
            "messaging.send",
            serde_json::json!({ "channel": "D1", "text": "I ran memory recall for you" }),
        ));
        assert_eq!(messages_since_memory_recall(&session), 3);
    }
}
