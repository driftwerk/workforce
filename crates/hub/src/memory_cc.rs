//! Claude Code memory-consolidation engine (`[memory] consolidation_source =
//! "claude_code"`).
//!
//! Consolidation is a batch transform, so the subprocess needs NO tools at
//! all: the hub prefetches the identity's
//! episodic memories, embeds them in the prompt, and the CLI answers with one
//! JSON array of operations (`remember` / `forget` / `write_profile`) which
//! the hub validates and applies in-process through the same store APIs the
//! `memory` tool uses. A `forget` may only target a prefetched id, so the
//! model can never delete a memory it was not shown. Under a subscription the
//! CLI run is flat-rate — this engine exists to take the recurring
//! consolidation spend off metered API billing.
//!
//! Per Anthropic TOS a Max OAuth token may only flow through the CLI, never
//! the Messages API — this path stays on the CLI.

use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};
use workforce_agent::llm::provider::ProviderKind;
use workforce_agent::llm::types::LlmMessage;
use workforce_agent::runner::with_served_log;
use workforce_common::cost::{CostEntry, TokenCounts};
use workforce_common::subscription::Origin;

use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryId, MemoryLayer, MemorySource};

use crate::claude_code;
use crate::memory_consolidation::MIN_MEMORIES;
use crate::serve::AgentService;

/// Most episodic memories embedded into one consolidation prompt.
const MAX_PREFETCH: usize = 200;
/// Most operations accepted in one returned plan. Larger plans are rejected
/// whole — position-wise truncation could apply a `forget` whose covering
/// `remember` was dropped.
const MAX_OPS: usize = 100;
/// Cap on a `write_profile` op's content, matching the prompt's instruction.
const PROFILE_MAX_CHARS: usize = 1500;

pub(crate) enum CcOutcome {
    /// The plan was applied; carries a short run summary.
    Applied(String),
    /// CC could not run (no login, CLI missing, spawn failure, a failed
    /// round). The caller decides whether to fall back to the model engine.
    Unavailable(String),
    /// Shutdown mid-run — do not apply and do not fall over.
    Cancelled,
}

/// Run one consolidation pass on the Claude Code engine over `identity`'s
/// memories, rewriting their profile too when `include_profile` (a
/// person's): one contained, tool-less completion on the Claude login the
/// payer in scope resolves, its cost recorded against the plan that served
/// it. `cancel` is the scheduler's shutdown token — a run interrupted by it
/// reports [`CcOutcome::Cancelled`] so the caller never fails over to a fresh
/// Model turn mid-shutdown.
pub(crate) async fn run(
    service: &Arc<AgentService>,
    identity: &str,
    include_profile: bool,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> CcOutcome {
    let Some(store) = service.memory_store.clone() else {
        return CcOutcome::Unavailable("memory store unavailable".into());
    };

    let memories = match prefetch_episodic(store.as_ref(), &service.agent_id, identity).await {
        Ok(m) => m,
        Err(e) => return CcOutcome::Unavailable(format!("memory prefetch failed: {e}")),
    };
    if memories.len() < MIN_MEMORIES {
        return CcOutcome::Applied(format!(
            "skipped: only {} episodic memories (need {MIN_MEMORIES})",
            memories.len()
        ));
    }

    let llm = match service
        .providers
        .provider(ProviderKind::ClaudeCode, None, None, false, None)
        .await
    {
        Ok(llm) => llm,
        Err(e) => return CcOutcome::Unavailable(e.to_string()),
    };
    let system = cc_consolidation_system_prompt(include_profile);
    let task = [LlmMessage::user(consolidation_task(
        &memories,
        include_profile,
    ))];
    let call = with_served_log(llm.create_message(Some(&system), &task, &[], None, None, false));
    let (response, served) = match cancel {
        Some(cancel) => tokio::select! {
            _ = cancel.cancelled() => return CcOutcome::Cancelled,
            answered = call => answered,
        },
        None => call.await,
    };
    let response = match response {
        Ok(response) => response,
        Err(e) => return CcOutcome::Unavailable(format!("{e:#}")),
    };
    service.cost_tracker.record(CostEntry {
        task_type: Some("memory_consolidation"),
        identity_id: Some(identity),
        description: Some("memory:consolidation"),
        served,
        ..CostEntry::new(
            service.agent_id.as_str(),
            &response.model,
            TokenCounts::from(&response.usage),
            Origin::Consolidation,
        )
    });
    let text = response.all_text().unwrap_or_default();

    let ops = claude_code::extract_json_array(&text);
    if ops.is_empty() && !text.trim().is_empty() {
        return CcOutcome::Unavailable(
            "Claude Code consolidation returned no parseable JSON array".to_string(),
        );
    }
    if ops.len() > MAX_OPS {
        return CcOutcome::Unavailable(format!(
            "Claude Code consolidation plan had {} ops (max {MAX_OPS})",
            ops.len()
        ));
    }
    let known_ids: std::collections::HashSet<String> =
        memories.iter().map(|m| m.id.0.clone()).collect();
    let applied = apply_plan(
        store.as_ref(),
        &service.agent_id,
        &service.workspace,
        ops,
        &known_ids,
        identity,
        include_profile,
    )
    .await;

    info!(
        identity = %identity,
        remembered = applied.remembered,
        forgotten = applied.forgotten,
        profile_written = applied.profile_written,
        rejected = applied.rejected,
        "Claude Code consolidation applied",
    );
    CcOutcome::Applied(format!(
        "consolidated via Claude Code: {} remembered, {} forgotten, {} rejected{}",
        applied.remembered,
        applied.forgotten,
        applied.rejected,
        if applied.profile_written {
            ", profile updated"
        } else {
            ""
        }
    ))
}

/// The identity's episodic memories, newest first, capped at [`MAX_PREFETCH`].
async fn prefetch_episodic(
    store: &dyn MemoryStore,
    agent_id: &AgentId,
    identity: &str,
) -> eyre::Result<Vec<Memory>> {
    let mut memories = store
        .list(MemoryFilter {
            layer: Some(MemoryLayer::Episodic),
            owner: Some(agent_id.clone()),
            identity_id: Some(identity.to_string()),
            ..MemoryFilter::default()
        })
        .await?;
    memories.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    memories.truncate(MAX_PREFETCH);
    Ok(memories)
}

#[derive(Default)]
struct Applied {
    remembered: u32,
    forgotten: u32,
    profile_written: bool,
    rejected: u32,
}

/// Validate and apply a returned op plan. Invalid ops are counted and skipped
/// — one malformed element never drops the rest.
#[allow(clippy::too_many_arguments)]
async fn apply_plan(
    store: &dyn MemoryStore,
    agent_id: &AgentId,
    workspace: &std::path::Path,
    ops: Vec<Value>,
    known_ids: &std::collections::HashSet<String>,
    identity: &str,
    allow_profile: bool,
) -> Applied {
    let mut applied = Applied::default();
    for op in ops {
        match op.get("op").and_then(Value::as_str) {
            Some("remember") => match remember_from(&op, agent_id, identity) {
                Some(memory) => match store.remember(memory).await {
                    Ok(_) => applied.remembered += 1,
                    Err(e) => {
                        warn!(error = %e, "Consolidation remember failed");
                        applied.rejected += 1;
                    }
                },
                None => applied.rejected += 1,
            },
            Some("forget") => {
                let id = op.get("memory_id").and_then(Value::as_str).unwrap_or("");
                if known_ids.contains(id) {
                    match store.forget(&MemoryId(id.to_string())).await {
                        Ok(()) => applied.forgotten += 1,
                        Err(e) => {
                            warn!(error = %e, memory_id = id, "Consolidation forget failed");
                            applied.rejected += 1;
                        }
                    }
                } else {
                    warn!(
                        memory_id = id,
                        "Consolidation forget rejected: not a prefetched id"
                    );
                    applied.rejected += 1;
                }
            }
            Some("write_profile") => {
                let content = op.get("content").and_then(Value::as_str).unwrap_or("");
                if allow_profile
                    && !applied.profile_written
                    && !content.trim().is_empty()
                    && content.chars().count() <= PROFILE_MAX_CHARS
                {
                    match crate::profile::ProfileManager::new(workspace)
                        .save_profile(identity, content)
                    {
                        Ok(()) => applied.profile_written = true,
                        Err(e) => {
                            warn!(error = %e, "Consolidation profile write failed");
                            applied.rejected += 1;
                        }
                    }
                } else {
                    applied.rejected += 1;
                }
            }
            _ => applied.rejected += 1,
        }
    }
    applied
}

/// Build a `Memory` from a `remember` op, mirroring the `memory` tool's
/// semantics (agent-owned, private, identity-scoped, `source:consolidation`
/// tagged). `None` when the op is malformed.
fn remember_from(op: &Value, agent_id: &AgentId, identity: &str) -> Option<Memory> {
    let content = op.get("content").and_then(Value::as_str)?.trim();
    if content.is_empty() {
        return None;
    }
    let layer = op
        .get("layer")
        .and_then(Value::as_str)
        .and_then(crate::memory_tool::parse_layer)
        .unwrap_or(MemoryLayer::Semantic);
    let importance = op
        .get("importance")
        .and_then(Value::as_f64)
        .unwrap_or(0.6)
        .clamp(0.0, 1.0) as f32;
    let mut tags: Vec<String> = op
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !tags.iter().any(|t| t == "source:consolidation") {
        tags.push("source:consolidation".to_string());
    }
    Some(
        Memory::new(content.to_string(), layer, agent_id.clone())
            .with_identity_id(identity.to_string())
            .with_source(MemorySource::Agent)
            .with_importance(importance)
            .with_tags(tags),
    )
}

fn cc_consolidation_system_prompt(include_profile: bool) -> String {
    let profile_op = if include_profile {
        "\n- {\"op\": \"write_profile\", \"content\": string ≤1500 chars} — at most ONE: a \
         rolling 'about this user' narrative (stable preferences, recurring patterns, habits) \
         written from the agent's perspective. Omit when nothing meaningful changed."
    } else {
        ""
    };
    format!(
        "You are running a background memory-consolidation pass. The user message lists the \
         current episodic memories as JSON. Merge clusters of related memories into consolidated \
         semantic memories and drop redundant fragments.\n\n\
         Output a SINGLE JSON array and NOTHING ELSE — no prose, no markdown fences. Allowed \
         elements:\n\
         - {{\"op\": \"remember\", \"content\": string, \"layer\": \"semantic\", \"importance\": \
         number 0.6-0.9, \"tags\": [string]}} — one consolidated memory replacing a cluster.\n\
         - {{\"op\": \"forget\", \"memory_id\": string}} — an episodic fragment merged into a \
         consolidated memory, or a near-duplicate. Only ids from the list are valid.{profile_op}\n\n\
         Never forget a memory whose content is not covered by a consolidated memory you emit in \
         the same plan. If nothing is worth consolidating, output []."
    )
}

/// The user message: per-layer counts context plus the episodic memories as
/// a JSON array of `{id, content, created_at, importance, tags}`.
fn consolidation_task(memories: &[Memory], include_profile: bool) -> String {
    let listing: Vec<Value> = memories
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id.0,
                "content": m.content,
                "created_at": m.created_at.to_rfc3339(),
                "importance": m.importance,
                "tags": m.tags,
            })
        })
        .collect();
    format!(
        "Consolidate these {} episodic memories.{}\n\n{}",
        memories.len(),
        if include_profile {
            " Also refresh the user profile if the picture of the user has changed."
        } else {
            ""
        },
        serde_json::to_string_pretty(&Value::Array(listing)).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_consolidation::CONSOLIDATION_SYSTEM_IDENTITY;

    fn memory(id: &str, content: &str) -> Memory {
        let mut m = Memory::new(
            content.to_string(),
            MemoryLayer::Episodic,
            AgentId::new("test-agent"),
        );
        m.id = MemoryId(id.to_string());
        m
    }

    #[test]
    fn task_lists_ids_and_contents() {
        let task = consolidation_task(
            &[memory("m1", "likes tea"), memory("m2", "hates tea")],
            true,
        );
        assert!(task.contains("m1"));
        assert!(task.contains("likes tea"));
        assert!(task.contains("2 episodic memories"));
        assert!(task.contains("user profile"));
    }

    #[test]
    fn global_pass_omits_profile_instruction() {
        let task = consolidation_task(&[memory("m1", "x")], false);
        assert!(!task.contains("user profile"));
        assert!(!cc_consolidation_system_prompt(false).contains("write_profile"));
        assert!(cc_consolidation_system_prompt(true).contains("write_profile"));
    }

    #[test]
    fn system_prompt_is_array_only() {
        let p = cc_consolidation_system_prompt(true);
        assert!(p.contains("SINGLE JSON array"));
        assert!(p.contains("Only ids from the list are valid"));
    }

    #[tokio::test]
    async fn apply_plan_enforces_the_forget_whitelist() {
        let store = workforce_memory::in_memory::InMemoryStore::new();
        let agent = AgentId::new("test-agent");
        let dir = tempfile::tempdir().unwrap();
        let kept = store
            .remember(Memory::new(
                "keep me".to_string(),
                MemoryLayer::Episodic,
                agent.clone(),
            ))
            .await
            .unwrap();
        let seen = store
            .remember(Memory::new(
                "merge me".to_string(),
                MemoryLayer::Episodic,
                agent.clone(),
            ))
            .await
            .unwrap();

        let known: std::collections::HashSet<String> = [seen.0.clone()].into();
        let ops = vec![
            serde_json::json!({"op": "forget", "memory_id": seen.0}),
            serde_json::json!({"op": "forget", "memory_id": kept.0}),
            serde_json::json!({"op": "forget"}),
        ];
        let applied = apply_plan(&store, &agent, dir.path(), ops, &known, "user-1", true).await;
        assert_eq!(applied.forgotten, 1);
        assert_eq!(applied.rejected, 2);
        assert!(store.get(&kept).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn apply_plan_remembers_with_consolidation_tag_and_identity() {
        let store = workforce_memory::in_memory::InMemoryStore::new();
        let agent = AgentId::new("test-agent");
        let dir = tempfile::tempdir().unwrap();
        let ops = vec![
            serde_json::json!({"op": "remember", "content": "user prefers tea", "importance": 0.8}),
            serde_json::json!({"op": "remember", "content": "   "}),
            serde_json::json!({"op": "explode"}),
        ];
        let known = std::collections::HashSet::new();
        let applied = apply_plan(&store, &agent, dir.path(), ops, &known, "user-1", true).await;
        assert_eq!(applied.remembered, 1);
        assert_eq!(applied.rejected, 2);

        let stored = store
            .list(MemoryFilter {
                layer: Some(MemoryLayer::Semantic),
                identity_id: Some("user-1".to_string()),
                ..MemoryFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].identity_id.as_deref(), Some("user-1"));
        assert!(stored[0].tags.iter().any(|t| t == "source:consolidation"));
    }

    #[tokio::test]
    async fn apply_plan_skips_profile_for_global_pass() {
        let store = workforce_memory::in_memory::InMemoryStore::new();
        let agent = AgentId::new("test-agent");
        let dir = tempfile::tempdir().unwrap();
        let ops = vec![serde_json::json!({"op": "write_profile", "content": "narrative"})];
        let known = std::collections::HashSet::new();
        let applied = apply_plan(
            &store,
            &agent,
            dir.path(),
            ops.clone(),
            &known,
            CONSOLIDATION_SYSTEM_IDENTITY,
            false,
        )
        .await;
        assert!(!applied.profile_written);
        assert_eq!(applied.rejected, 1);

        let applied = apply_plan(&store, &agent, dir.path(), ops, &known, "user-1", true).await;
        assert!(applied.profile_written);
    }
}
