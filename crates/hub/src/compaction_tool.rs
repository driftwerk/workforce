//! `compaction` tool — lets agents view, configure, and trigger context compaction.
//!
//! Access is controlled by the policy engine via `tool: "compaction"`.

use std::sync::Arc;
use std::time::Instant;

use crate::serve::AgentService;
use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::llm::model_limits;
use workforce_agent::tools::{Tool, ToolResult};

/// Tool that lets agents manage compaction configuration and trigger compaction.
pub struct CompactionTool {
    service: Arc<AgentService>,
}

impl CompactionTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self { service }
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CompactionInput {
    action: String,
    /// Direct session ID (for status/compact — preferred method)
    #[serde(default)]
    session_id: Option<String>,
    /// Session channel key (for status/compact — includes platform prefix like 'slack:')
    #[serde(default)]
    session_channel: Option<String>,
    /// Raw channel ID (for status/compact — fallback if session_channel not provided).
    #[serde(default)]
    channel: Option<String>,
    /// User/owner (for status/compact — to find the session).
    #[serde(default)]
    user: Option<String>,
    /// Where to save config: "global" or "agent" (for save action)
    #[serde(default)]
    scope: Option<String>,
    // Fields for "set" action — individual overrides
    #[serde(default)]
    max_context_tokens: Option<u32>,
    #[serde(default)]
    compaction_threshold: Option<f32>,
    #[serde(default)]
    keep_recent_messages: Option<usize>,
    #[serde(default)]
    enable_memory_flush: Option<bool>,
    #[serde(default)]
    prune_tool_outputs: Option<bool>,
    #[serde(default)]
    prune_tool_output_max_chars: Option<usize>,
    #[serde(default)]
    max_compaction_passes: Option<u32>,
    #[serde(default)]
    min_messages_to_compact: Option<usize>,
    #[serde(default)]
    urgent_compaction_threshold: Option<f32>,
    #[serde(default)]
    compaction_model: Option<String>,
}

#[async_trait]
impl Tool for CompactionTool {
    fn name(&self) -> &str {
        "compaction"
    }

    fn description(&self) -> &str {
        "Manage context window compaction config and status"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get", "set", "reset", "save", "status", "compact"],
                    "description": "Action to perform"
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "agent"],
                    "description": "Where to save"
                },
                "session_id": {
                    "type": "string",
                    "description": "Session ID for direct lookup (preferred for status/compact)"
                },
                "session_channel": {
                    "type": "string",
                    "description": "Session channel key with platform prefix"
                },
                "channel": {
                    "type": "string",
                    "description": "Raw channel ID (fallback if session_channel not available)"
                },
                "user": {
                    "type": "string",
                    "description": "User/owner to find the session (for status/compact)"
                },
                "max_context_tokens": {
                    "type": "integer",
                    "description": "Maximum context window size in tokens (for set)"
                },
                "compaction_threshold": {
                    "type": "number",
                    "description": "Fraction of max tokens at which compaction triggers"
                },
                "keep_recent_messages": {
                    "type": "integer",
                    "description": "Number of recent messages to always keep (for set)"
                },
                "enable_memory_flush": {
                    "type": "boolean",
                    "description": "Whether to flush to memory before compacting (for set)"
                },
                "prune_tool_outputs": {
                    "type": "boolean",
                    "description": "Whether to prune large tool outputs (for set)"
                },
                "prune_tool_output_max_chars": {
                    "type": "integer",
                    "description": "Max chars for tool output before truncation (for set)"
                },
                "max_compaction_passes": {
                    "type": "integer",
                    "description": "Maximum compaction iterations per cycle (for set)"
                },
                "min_messages_to_compact": {
                    "type": "integer",
                    "description": "Minimum messages required before compaction runs (for set)"
                },
                "urgent_compaction_threshold": {
                    "type": "number",
                    "description": "Fraction of max tokens at which compaction MUST block"
                },
                "compaction_model": {
                    "type": "string",
                    "description": "Model for compaction LLM calls (summarisation/memory flush)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: CompactionInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid compaction input: {}", e))?;

        match input.action.as_str() {
            "get" => self.handle_get(start),
            "set" => self.handle_set(input, start),
            "reset" => self.handle_reset(start),
            "save" => self.handle_save(input, start),
            "status" => self.handle_status(input, start).await,
            "compact" => self.handle_compact(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{}'. Use: get, set, reset, save, status, compact",
                    other
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl CompactionTool {
    fn handle_get(&self, start: Instant) -> Result<ToolResult> {
        let current = self.service.current_compaction_config();
        let has_override = self.service.active_compaction.read().is_some();

        Ok(ToolResult::success(
            serde_json::json!({
                "config": {
                    "max_context_tokens": current.max_context_tokens,
                    "compaction_threshold": current.compaction_threshold,
                    "trigger_threshold_tokens": current.trigger_threshold(),
                    "urgent_compaction_threshold": current.urgent_compaction_threshold,
                    "urgent_threshold_tokens": current.urgent_threshold(),
                    "keep_recent_messages": current.keep_recent_messages,
                    "enable_memory_flush": current.enable_memory_flush,
                    "prune_tool_outputs": current.prune_tool_outputs,
                    "prune_tool_output_max_chars": current.prune_tool_output_max_chars,
                    "max_compaction_passes": current.max_compaction_passes,
                    "min_messages_to_compact": current.min_messages_to_compact,
                    "compaction_model": current.compaction_model,
                },
                "has_runtime_override": has_override,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    fn handle_set(&self, input: CompactionInput, start: Instant) -> Result<ToolResult> {
        // Start from current config, apply field-level overrides
        let mut config = self.service.current_compaction_config();

        if let Some(v) = input.max_context_tokens {
            config.max_context_tokens = v;
        }
        if let Some(v) = input.compaction_threshold {
            if !(0.0..=1.0).contains(&v) {
                return Ok(ToolResult::failure(
                    "compaction_threshold must be between 0.0 and 1.0",
                    start.elapsed().as_millis() as u64,
                ));
            }
            config.compaction_threshold = v;
        }
        if let Some(v) = input.keep_recent_messages {
            config.keep_recent_messages = v;
        }
        if let Some(v) = input.enable_memory_flush {
            config.enable_memory_flush = v;
        }
        if let Some(v) = input.prune_tool_outputs {
            config.prune_tool_outputs = v;
        }
        if let Some(v) = input.prune_tool_output_max_chars {
            config.prune_tool_output_max_chars = v;
        }
        if let Some(v) = input.max_compaction_passes {
            config.max_compaction_passes = v;
        }
        if let Some(v) = input.min_messages_to_compact {
            config.min_messages_to_compact = v;
        }
        if let Some(v) = input.urgent_compaction_threshold {
            if !(0.0..=1.0).contains(&v) {
                return Ok(ToolResult::failure(
                    "urgent_compaction_threshold must be between 0.0 and 1.0",
                    start.elapsed().as_millis() as u64,
                ));
            }
            if v <= config.compaction_threshold {
                return Ok(ToolResult::failure(
                    "urgent_compaction_threshold must be greater than compaction_threshold",
                    start.elapsed().as_millis() as u64,
                ));
            }
            config.urgent_compaction_threshold = v;
        }
        if let Some(v) = input.compaction_model {
            config.compaction_model = if v.is_empty() { None } else { Some(v) };
        }

        // Persisted first, and only if the agent would load the result: a
        // compaction model it could not boot with never goes live.
        if let Err(e) = self.service.persist_workspace(|cfg| {
            let comp = cfg.compaction.get_or_insert_with(Default::default);
            comp.max_context_tokens = Some(config.max_context_tokens);
            comp.compaction_threshold = Some(config.compaction_threshold);
            comp.keep_recent_messages = Some(config.keep_recent_messages);
            comp.enable_memory_flush = Some(config.enable_memory_flush);
            comp.prune_tool_outputs = Some(config.prune_tool_outputs);
            comp.prune_tool_output_max_chars = Some(config.prune_tool_output_max_chars);
            comp.max_compaction_passes = Some(config.max_compaction_passes);
            comp.min_messages_to_compact = Some(config.min_messages_to_compact);
            comp.urgent_compaction_threshold = Some(config.urgent_compaction_threshold);
            comp.compaction_model = config.compaction_model.clone();
        }) {
            return Ok(ToolResult::failure(
                format!("Compaction config not changed: {e}. No changes were made."),
                start.elapsed().as_millis() as u64,
            ));
        }
        *self.service.active_compaction.write() = Some(config.clone());

        Ok(ToolResult::success(
            serde_json::json!({
                "message": "Compaction config updated (runtime override active)",
                "config": {
                    "max_context_tokens": config.max_context_tokens,
                    "compaction_threshold": config.compaction_threshold,
                    "trigger_threshold_tokens": config.trigger_threshold(),
                    "urgent_compaction_threshold": config.urgent_compaction_threshold,
                    "urgent_threshold_tokens": config.urgent_threshold(),
                    "keep_recent_messages": config.keep_recent_messages,
                    "enable_memory_flush": config.enable_memory_flush,
                    "prune_tool_outputs": config.prune_tool_outputs,
                    "prune_tool_output_max_chars": config.prune_tool_output_max_chars,
                    "max_compaction_passes": config.max_compaction_passes,
                    "min_messages_to_compact": config.min_messages_to_compact,
                    "compaction_model": config.compaction_model,
                },
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    fn handle_reset(&self, start: Instant) -> Result<ToolResult> {
        *self.service.active_compaction.write() = None;
        // Remove from workspace config.toml
        if let Err(e) = self.service.persist_workspace(|cfg| cfg.compaction = None) {
            tracing::warn!(error = %e, "Failed to update workspace config");
        }
        // Legacy vault cleanup
        if let Some(ref vault) = self.service.vault {
            let _ = vault.delete("compaction/config");
        }
        let config = self.service.effective_compaction();

        Ok(ToolResult::success(
            serde_json::json!({
                "message": "Runtime override cleared, reverted to TOML config",
                "config": {
                    "max_context_tokens": config.max_context_tokens,
                    "compaction_threshold": config.compaction_threshold,
                    "trigger_threshold_tokens": config.trigger_threshold(),
                    "keep_recent_messages": config.keep_recent_messages,
                },
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    fn handle_save(&self, _input: CompactionInput, start: Instant) -> Result<ToolResult> {
        // The 'save' action is now deprecated — compaction config is automatically
        // persisted to {workspace}/config.toml by the 'set' action.
        // Keep the action but inform the user.
        let active_compaction = self.service.active_compaction.read().clone();

        if active_compaction.is_none() {
            return Ok(ToolResult::success(
                serde_json::json!({
                    "message": "No runtime override active. Compaction config is automatically \
                                saved to workspace config.toml when using the 'set' action.",
                }),
                start.elapsed().as_millis() as u64,
            ));
        }

        Ok(ToolResult::success(
            serde_json::json!({
                "message": "Compaction config is already persisted in workspace config.toml. \
                            The 'save' action is no longer needed — 'set' automatically saves.",
                "saved_to": self.service.workspace.join("config.toml").display().to_string(),
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_status(&self, input: CompactionInput, start: Instant) -> Result<ToolResult> {
        let session = if let Some(session_id) = &input.session_id {
            // Direct session lookup by ID
            match self.service.sessions.get(session_id)? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        format!("Session not found: {}", session_id),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        } else {
            // Smart session discovery via channel/user
            let channel = input
                .session_channel
                .as_deref()
                .or(input.channel.as_deref())
                .unwrap_or("unknown");
            let user = input.user.as_deref().unwrap_or("unknown");

            match self.service.sessions.list(Some(channel), 1) {
                Ok(mut sessions) if !sessions.is_empty() => {
                    // Use the most recent session for this channel
                    sessions.remove(0)
                }
                _ => {
                    // No existing session found, create one
                    match self.service.sessions.get_or_create(
                        channel,
                        user,
                        self.service.agent_id.as_str(),
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            return Ok(ToolResult::failure(
                                format!("Session error: {}", e),
                                start.elapsed().as_millis() as u64,
                            ))
                        }
                    }
                }
            }
        };

        let resolved_model = self.service.resolve_model(&session, None, None);
        let config = self.service.compaction_config_for(&resolved_model).await;
        let effective_max = config.max_context_tokens;

        let estimated_tokens = session.estimated_total_tokens();
        let threshold = (effective_max as f32 * config.compaction_threshold) as u32;
        let usage_pct = (estimated_tokens as f64 / effective_max as f64 * 100.0).min(100.0);
        let needs_compaction = estimated_tokens > threshold;

        let history: Vec<serde_json::Value> = session
            .compaction_history
            .iter()
            .rev()
            .take(5)
            .map(|e| {
                serde_json::json!({
                    "timestamp": e.timestamp.to_rfc3339(),
                    "kind": format!("{:?}", e.kind),
                    "messages_compacted": e.messages_compacted,
                    "tokens_before": e.tokens_before,
                    "tokens_after": e.tokens_after,
                    "reduction_pct": if e.tokens_before > 0 {
                        ((e.tokens_before - e.tokens_after) as f64 / e.tokens_before as f64 * 100.0) as u32
                    } else {
                        0
                    },
                })
            })
            .collect();

        let urgent_threshold = (effective_max as f32 * config.urgent_compaction_threshold) as u32;
        let needs_urgent = estimated_tokens > urgent_threshold;
        let zone = if needs_urgent {
            "red (blocking compaction)"
        } else if needs_compaction {
            "yellow (deferred compaction)"
        } else {
            "green (no compaction needed)"
        };

        Ok(ToolResult::success(
            serde_json::json!({
                "session_id": session.id,
                "estimated_tokens": estimated_tokens,
                "max_context_tokens": config.max_context_tokens,
                "model_context_limit": model_limits::model_limits(&resolved_model).map(|l| l.context_tokens),
                "effective_max_context_tokens": effective_max,
                "trigger_threshold": threshold,
                "urgent_threshold": urgent_threshold,
                "usage_pct": format!("{:.1}%", usage_pct),
                "zone": zone,
                "needs_compaction": needs_compaction,
                "needs_urgent_compaction": needs_urgent,
                "compaction_model": config.compaction_model,
                "message_count": session.message_count(),
                "recent_compactions": history,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_compact(&self, input: CompactionInput, start: Instant) -> Result<ToolResult> {
        let mut session = if let Some(session_id) = &input.session_id {
            // Direct session lookup by ID
            match self.service.sessions.get(session_id)? {
                Some(s) => s,
                None => {
                    return Ok(ToolResult::failure(
                        format!("Session not found: {}", session_id),
                        start.elapsed().as_millis() as u64,
                    ))
                }
            }
        } else {
            // Smart session discovery via channel/user
            let channel = input
                .session_channel
                .as_deref()
                .or(input.channel.as_deref())
                .unwrap_or("unknown");
            let user = input.user.as_deref().unwrap_or("unknown");

            match self.service.sessions.list(Some(channel), 1) {
                Ok(mut sessions) if !sessions.is_empty() => {
                    // Use the most recent session for this channel
                    sessions.remove(0)
                }
                _ => {
                    // No existing session found, create one
                    match self.service.sessions.get_or_create(
                        channel,
                        user,
                        self.service.agent_id.as_str(),
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            return Ok(ToolResult::failure(
                                format!("Session error: {}", e),
                                start.elapsed().as_millis() as u64,
                            ))
                        }
                    }
                }
            }
        };

        let compaction_config = self.service.current_compaction_config();
        let memory_writer = crate::notes_tool::memory_writer(&self.service);
        let engine = workforce_agent::compaction::CompactionEngine::with_memory_writer(
            compaction_config,
            Arc::clone(&self.service.llm),
            Some(self.service.effective_llm.default_model.clone()),
            memory_writer,
        );

        match engine.compact_manual(&mut session).await {
            Ok(event) => {
                if event.messages_compacted > 0 {
                    if let Err(e) = self.service.sessions.save(&session) {
                        return Ok(ToolResult::failure(
                            format!("Compaction succeeded but save failed: {}", e),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "messages_compacted": event.messages_compacted,
                        "tokens_before": event.tokens_before,
                        "tokens_after": event.tokens_after,
                        "tokens_saved": event.tokens_before.saturating_sub(event.tokens_after),
                        "kind": format!("{:?}", event.kind),
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Compaction failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use workforce_common::session::CompactionConfig;

    #[test]
    fn config_get_returns_defaults() {
        let config = CompactionConfig::default();
        assert_eq!(config.max_context_tokens, 200_000);
        assert_eq!(config.max_compaction_passes, 3);
        assert_eq!(config.min_messages_to_compact, 5);
        assert!((config.urgent_compaction_threshold - 0.95).abs() < f32::EPSILON);
        assert_eq!(config.urgent_threshold(), 190_000);
        assert_eq!(config.compaction_model, None);
    }
}
