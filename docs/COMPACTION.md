# Compaction

Compaction manages the context window for long-running agent sessions. As conversations grow — tool calls, large outputs, extended back-and-forth — the session can exceed the LLM's context window. Compaction keeps sessions within limits using a layered strategy: prune, flush, summarise.

## Overview

```
                       Session grows over time
                              │
                              ▼
               ┌──────────────────────────┐
               │  compact_if_needed()     │
               │  called before each LLM  │
               │  message send            │
               └────────────┬─────────────┘
                            │
               tokens > threshold?
                    │              │
                   NO             YES
                    │              │
                 return         ┌──┴──┐
                               │     │
                          Step 1    Step 2
                          Prune     Multi-pass
                          tool      summarisation
                          outputs   loop
                               │
                            tokens
                          still over?
                            │     │
                           NO    YES
                            │     │
                         done   repeat
                                (up to
                                max_passes)
```

## Strategies

Compaction applies three strategies in order of increasing cost:

### 1. Pruning (Lightweight)

Truncate large tool outputs in-place. This is fast (no LLM call) and reduces token usage without losing recent conversation structure.

- Only affects messages older than `keep_recent_messages` from the end
- Truncates `tool_data.output` fields exceeding `prune_tool_output_max_chars`
- Also truncates the tool message `content` if oversized
- Adds a `[output truncated, was N chars]` marker
- Records a `CompactionKind::Pruning` event

### 2. Memory Flush (Optional)

Before summarisation, an LLM call asks the model to identify critical information that should be persisted to memory. The output is written to the agent's daily notes file via the `MemoryWriter` trait.

This prevents important context (active tasks, decisions, user preferences) from being lost when old messages are replaced by a summary.

The flush prompt focuses on:
- Active tasks and their current state
- Important decisions that were made
- File paths, configurations, or technical details
- User preferences or instructions

Controlled by the `enable_memory_flush` config flag (default: true).

### 3. Summarisation (LLM-Based)

Uses the LLM to condense old messages into a single summary message:

1. Selects messages to compact (everything except system messages and the most recent `keep_recent_messages`)
2. Filters out low-value tool messages before sending to the LLM:
   - Messages containing `[truncated` (already pruned, low information)
   - Short messages containing `completed in` or `executed in` (trivial status lines)
3. Formats remaining messages into a readable text block
4. Sends to the LLM with a summarisation prompt
5. Replaces old messages with a single `[Compaction Summary]` system message

The summary message preserves:
- What was discussed and decided
- Tool calls and their results (briefly)
- Current state of tasks in progress
- Important context needed for continuity

## Multi-Pass Compaction

Sessions that grow very large may not reach below the threshold in a single summarisation pass. The engine loops:

```
for pass in 0..max_compaction_passes:
    if compactable messages < min_messages_to_compact: break
    run compact_full()
    if tokens_after >= tokens_before: break   // safety valve
    if under threshold: break                 // done
```

Safety guards:
- **Max passes** (`max_compaction_passes`, default 3): prevents runaway loops
- **No progress**: if a pass doesn't reduce tokens, stop immediately
- **Min messages** (`min_messages_to_compact`, default 5): skip summarisation when too few messages would be replaced (avoids degenerate summaries that are longer than the input)

## Dynamic Summary Token Budget

The summary token budget scales with the input being compacted:

```
target = (compacting_tokens / 5).clamp(1024, 16384)
```

This means summaries are approximately 20% of the input size, with a floor of 1024 tokens and a ceiling of 16384 tokens. This prevents the common failure mode where a fixed 4096-token summary ends up being as large as (or larger than) a small set of compacted messages.

## Configuration

### CompactionConfig

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_context_tokens` | u32 | 1,000,000 | Maximum context window size in tokens. Sized for Claude Opus 4.6. |
| `compaction_threshold` | f32 | 0.8 | Fraction of max tokens at which compaction triggers. 0.8 = compact at 80% capacity. |
| `keep_recent_messages` | usize | 20 | Number of recent messages to always keep during compaction. These are never summarised. |
| `enable_memory_flush` | bool | true | Whether to flush important context to memory files before compacting. |
| `prune_tool_outputs` | bool | true | Whether to prune large tool outputs as a lightweight first step. |
| `prune_tool_output_max_chars` | usize | 2000 | Maximum character length for tool output before truncation. |
| `max_compaction_passes` | u32 | 3 | Maximum summarisation iterations per compaction cycle. Safety valve. |
| `min_messages_to_compact` | usize | 5 | Minimum compactable messages before summarisation runs. Prevents degenerate summaries. |

### Trigger Threshold

The compaction trigger threshold is calculated as:

```
trigger_threshold = max_context_tokens * compaction_threshold
```

With defaults: `1,000,000 * 0.8 = 800,000 tokens`.

Compaction runs when `session.estimated_total_tokens() > trigger_threshold`.

### Token Estimation

Tokens are estimated at approximately 4 characters per token:

```rust
fn estimate_tokens(text: &str) -> u32 {
    (text.len() as u32 / 4).max(1)
}
```

This is a rough heuristic, not a precise tokenizer count. It's intentionally fast since it's called frequently.

## TOML Configuration

### Global Config

```toml
[compaction]
max_context_tokens = 1_000_000
compaction_threshold = 0.8
keep_recent_messages = 20
enable_memory_flush = true
prune_tool_outputs = true
prune_tool_output_max_chars = 2000
max_compaction_passes = 3
min_messages_to_compact = 5
```

### Per-Agent Overrides

Each agent can override any compaction field. Unset fields inherit from the global `[compaction]` section.

```toml
[[agents]]
id = "allan"
workspace = "./data/agents/allan"

# Agent-level overrides (all optional)
compaction.max_context_tokens = 200_000   # smaller model, smaller window
compaction.compaction_threshold = 0.7     # compact earlier
compaction.keep_recent_messages = 10      # keep fewer messages
```

The resolution follows the same pattern as `AgentLlmConfig`:

```
effective_compaction = agent_override.unwrap_or(global_config)
```

### Runtime Override

Agents can also override compaction config at runtime via the `compaction` tool (see below). Runtime overrides take precedence over both global and per-agent TOML config:

```
runtime_override > per_agent_config > global_config
```

Runtime overrides are stored in `AgentService.active_compaction` (an `Arc<RwLock<Option<CompactionConfig>>>`) and are lost on restart.

## Slash Commands

Two slash commands provide user-facing compaction controls:

### `/compact`

Trigger manual compaction on the current session. Runs memory flush + summarisation regardless of threshold.

```
🗜️ Compacted: 450000 → 120000 tokens (330000 saved, 87 messages compacted)
```

### `/context`

Show context window usage with a visual progress bar.

```
📏 Context Window

  [████████████░░░░░░░░] 60.5%
  Tokens: 605000 / 1000000
  Messages: 342
  Compaction threshold: 80%
```

## The `compaction` Tool

The `compaction` tool lets agents view, modify, and trigger compaction programmatically. This is useful for agents that want to self-manage their context window.

### Actions

| Action | Description | Required Params |
|--------|-------------|-----------------|
| `get` | Show current effective config | — |
| `set` | Update config fields at runtime | any config field |
| `reset` | Clear runtime overrides, revert to TOML config | — |
| `status` | Show session token usage vs thresholds | `channel`, `user` |
| `compact` | Trigger manual compaction | `channel`, `user` |

### Get

Returns the current effective config and whether a runtime override is active.

```json
{
  "action": "get"
}
```

Response:

```json
{
  "config": {
    "max_context_tokens": 1000000,
    "compaction_threshold": 0.8,
    "trigger_threshold_tokens": 800000,
    "keep_recent_messages": 20,
    "enable_memory_flush": true,
    "prune_tool_outputs": true,
    "prune_tool_output_max_chars": 2000,
    "max_compaction_passes": 3,
    "min_messages_to_compact": 5
  },
  "has_runtime_override": false
}
```

### Set

Update individual config fields at runtime. Only the fields you include are changed; others keep their current values. The change is stored as a runtime override.

```json
{
  "action": "set",
  "compaction_threshold": 0.6,
  "keep_recent_messages": 30
}
```

Response:

```json
{
  "message": "Compaction config updated (runtime override active)",
  "config": {
    "max_context_tokens": 1000000,
    "compaction_threshold": 0.6,
    "trigger_threshold_tokens": 600000,
    "keep_recent_messages": 30,
    "..."
  }
}
```

Validation: `compaction_threshold` must be between 0.0 and 1.0.

### Reset

Clear runtime overrides and revert to the TOML configuration (global + per-agent).

```json
{
  "action": "reset"
}
```

### Status

Show the current session's token usage relative to compaction thresholds. Includes recent compaction history.

```json
{
  "action": "status",
  "channel": "slack",
  "user": "kings"
}
```

Response:

```json
{
  "session_id": "abc-123",
  "estimated_tokens": 450000,
  "max_context_tokens": 1000000,
  "trigger_threshold": 800000,
  "usage_pct": "45.0%",
  "needs_compaction": false,
  "message_count": 234,
  "recent_compactions": [
    {
      "timestamp": "2026-02-12T14:30:00Z",
      "kind": "Summarisation",
      "messages_compacted": 45,
      "tokens_before": 850000,
      "tokens_after": 320000,
      "reduction_pct": 62
    }
  ]
}
```

### Compact

Trigger manual compaction on the specified session.

```json
{
  "action": "compact",
  "channel": "slack",
  "user": "kings"
}
```

Response:

```json
{
  "messages_compacted": 87,
  "tokens_before": 450000,
  "tokens_after": 120000,
  "tokens_saved": 330000,
  "kind": "Manual"
}
```

## Policy Configuration

The `compaction` tool is controlled by the policy engine like any other tool. Use `tool: "compaction"` in your policy rules.

```yaml
rules:
  - name: "allow-compaction"
    scope:
      agent: "allan"
    tool: "compaction"
    action: allow
```

See [POLICY_ENGINE.md](POLICY_ENGINE.md) for full policy syntax.

## Compaction Events

Every compaction operation records a `CompactionEvent` in the session's `compaction_history`:

```rust
pub struct CompactionEvent {
    pub timestamp: DateTime<Utc>,
    pub messages_compacted: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub kind: CompactionKind,
}
```

Kinds:
- `Summarisation` — automatic LLM-based summarisation
- `Pruning` — lightweight tool output truncation
- `Manual` — triggered by `/compact` command or `compaction` tool

The history is persisted with the session and can be viewed via the `status` action.

## Session Structure After Compaction

After compaction, the session messages look like:

```
[System] Original system prompt (SOUL.md, etc.)
[System] [Compaction Summary — earlier conversation condensed]
         • User asked about deployment pipeline
         • Agent created CI workflow in .github/
         • Tool calls: filesystem.write (3), exec.run (2)
         • Current task: waiting for test results
[User]   Most recent message 1          ┐
[Tool]   Tool result                    │ keep_recent_messages
[Asst]   Response                       │ (preserved)
[User]   Most recent message N          ┘
```

System messages are always preserved. The compaction summary is inserted as a system message between the original system prompt and the kept recent messages.

## Source Files

| File | Purpose |
|------|---------|
| `crates/common/src/session.rs` | `CompactionConfig`, `CompactionEvent`, `CompactionKind`, `Session::compact_with_summary()`, `Session::prune_tool_outputs()`, `Session::needs_compaction()` |
| `crates/agent-runtime/src/compaction.rs` | `CompactionEngine` — orchestrates pruning, memory flush, and multi-pass summarisation |
| `crates/agent-runtime/src/llm/model_limits.rs` | `ModelLimits`, `model_limits()`, `effective_context_limit()`, model-aware compaction threshold functions |
| `crates/common/src/settings.rs` | `AgentCompactionConfig`, `WorkforceConfig.compaction`, `AgentConfig.effective_compaction()` |
| `crates/hub/src/compaction_tool.rs` | `CompactionTool` — the `compaction` tool for agent self-management |
| `crates/hub/src/commands.rs` | `/compact` and `/context` slash commands |
| `crates/hub/src/serve.rs` | `AgentService.effective_compaction`, `AgentService.active_compaction`, `AgentService.current_compaction_config()` |

## Model-Aware Compaction

When a failover chain includes models with different context window sizes (e.g. Claude at 200K, Grok at 131K, Gemini at 1M+), compaction thresholds are dynamically adjusted to match the resolved model's actual context limit.

### How It Works

1. **Model limits registry** (`crates/agent-runtime/src/llm/model_limits.rs`) maps known model identifiers to their actual context window and max output token limits.

2. **Effective context limit** is calculated as `min(model_limit, configured_max)`:
   - If Grok (131K) is the resolved model and config says 200K → effective limit is 131K
   - If Gemini (1M) is the resolved model and config says 200K → effective limit is 200K
   - Unknown models use the configured `max_context_tokens` unchanged

3. **Compaction thresholds** are recalculated using the effective limit:
   - Trigger threshold = `effective_limit × compaction_threshold` (default 0.8)
   - Urgent threshold = `effective_limit × urgent_compaction_threshold` (default 0.95)

4. **Context overflow escalation** in the failover chain: if a model returns a `ContextOverflow` error and the chain contains a subsequent model with a larger context window, the request escalates to that model instead of aborting. If no larger model exists, the error propagates for the runner to handle (emergency compaction).

### Known Model Limits

| Model Pattern | Context Window | Max Output |
|---------------|---------------|------------|
| `claude-opus-4*` | 200,000 | 32,768 |
| `claude-sonnet-4*` | 200,000 | 65,536 |
| `claude-haiku-4*` | 200,000 | 65,536 |
| `grok-4*` | 131,072 | 32,768 |
| `grok-3*` | 131,072 | 32,768 |
| `gemini-2.5-pro*` | 1,000,000 | 65,536 |
| `gemini-2.5-flash*` | 1,000,000 | 65,536 |

### Backward Compatibility

- Single-provider setups are unaffected — unknown models fall back to `max_context_tokens`
- The `compaction status` and `context status` tools now show both the configured max and the effective (model-aware) max
