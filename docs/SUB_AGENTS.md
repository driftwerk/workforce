# Sub-Agents

Sub-agents let a running agent spawn background tasks that execute in isolated sessions without blocking the main conversation. Each sub-agent gets its own session, can use a different model, has a configurable timeout, and announces its result back to the parent Slack thread when complete.

## Overview

```
┌──────────────┐    spawn     ┌──────────────────┐
│  Main Agent  │──────────────│  SubAgentManager  │
│  (Slack msg) │              │  (shared state)   │
└──────┬───────┘              └────────┬──────────┘
       │                               │
       │ tool call:                    │ tracks:
       │ subagent { action: "spawn" }  │ - task_id → SubAgentTask
       │                               │ - status, result, cancel
       │                               │
       │         tokio::spawn          │
       │    ┌──────────────────────┐   │
       └───►│  Background Task     │◄──┘
            │  - isolated session  │
            │  - own model config  │
            │  - timeout watchdog  │
            └──────────┬───────────┘
                       │ on complete:
                       │ 1. store result in manager
                       │ 2. announce to Slack
                       ▼
```

The main agent continues its conversation immediately after spawning. It can check on progress, retrieve results, or cancel tasks at any time using the same `subagent` tool.

## The `subagent` Tool

The tool exposes five actions through a single interface:

| Action | Description | Required Params |
|--------|-------------|-----------------|
| `spawn` | Start a background task | `task` |
| `status` | Check a task's progress | `task_id` |
| `list` | Show all sub-agent tasks | — |
| `cancel` | Stop a running task | `task_id` |
| `result` | Get full result of a completed task | `task_id` |

### Parameters

| Parameter | Type | Used By | Description |
|-----------|------|---------|-------------|
| `action` | string (required) | all | One of: `spawn`, `status`, `list`, `cancel`, `result` |
| `task` | string | spawn | What the sub-agent should work on |
| `model` | string | spawn | Model override, e.g. `claude-sonnet-4-5-20250929`. Uses parent's model if omitted |
| `system_prompt` | string | spawn | Custom system prompt. Uses the default agent prompt if omitted |
| `timeout_secs` | integer | spawn | Timeout in seconds. Default: 600 (10 minutes) |
| `task_id` | string | status, cancel, result | The ID returned by `spawn` |
| `channel_id` | string | spawn | Slack channel for result announcement. Auto-captured from messaging context if omitted |
| `thread_id` | string | spawn | Slack thread for result announcement. Auto-captured if omitted |

### Spawn

Starts a background sub-agent. The tool returns immediately with a `task_id`.

```json
{
  "action": "spawn",
  "task": "Analyse the last 7 days of error logs and summarise the top 5 recurring issues",
  "model": "claude-sonnet-4-5-20250929",
  "timeout_secs": 300
}
```

Response:

```json
{
  "task_id": "a1b2c3d4",
  "status": "running",
  "timeout_secs": 300,
  "message": "Sub-agent spawned. Check status with action='status', task_id='a1b2c3d4'"
}
```

### Status

Check progress. Returns a truncated result preview (500 chars) if the task is complete.

```json
{
  "action": "status",
  "task_id": "a1b2c3d4"
}
```

Response:

```json
{
  "task_id": "a1b2c3d4",
  "task": "Analyse the last 7 days of error logs...",
  "status": "completed",
  "started_at": "2025-01-15T10:00:00Z",
  "finished_at": "2025-01-15T10:02:30Z",
  "duration_secs": 150,
  "result_preview": "## Top 5 Recurring Issues\n\n1. Connection timeout to database..."
}
```

### List

Show all tracked sub-agent tasks (running, completed, failed, etc.).

```json
{
  "action": "list"
}
```

Response:

```json
{
  "tasks": [
    {
      "task_id": "a1b2c3d4",
      "task": "Analyse the last 7 days of error logs and summarise the top 5 recurring issues",
      "status": "completed",
      "started_at": "2025-01-15T10:00:00Z",
      "finished_at": "2025-01-15T10:02:30Z"
    },
    {
      "task_id": "e5f6g7h8",
      "task": "Review PR #42 for security issues",
      "status": "running",
      "started_at": "2025-01-15T10:05:00Z",
      "finished_at": null
    }
  ],
  "count": 2
}
```

### Result

Get the full result text of a completed task (up to 4000 chars). Use this after `status` shows the task is done.

```json
{
  "action": "result",
  "task_id": "a1b2c3d4"
}
```

Response:

```json
{
  "task_id": "a1b2c3d4",
  "status": "completed",
  "result": "## Top 5 Recurring Issues\n\n1. Connection timeout to database (47 occurrences)...",
  "error": null
}
```

### Cancel

Stop a running sub-agent. Uses a `CancellationToken` for graceful shutdown.

```json
{
  "action": "cancel",
  "task_id": "e5f6g7h8"
}
```

Response:

```json
{
  "task_id": "e5f6g7h8",
  "status": "cancelling"
}
```

## How It Works

### Isolation

Each sub-agent runs in a completely isolated session:

- **Own session**: Channel `subagent:{task_id}`, kind `SessionKind::SubAgent`
- **Own conversation**: Starts with empty history — no context leakage from the parent
- **Same tools**: Inherits the parent agent's full tool registry (filesystem, exec, memory, scheduler, etc.)
- **Same policy**: Subject to the same policy engine rules as the parent
- **Own model** (optional): Can use a different model than the parent (e.g. use Sonnet for simple tasks to save cost)

### System Prompt

By default, sub-agents use the parent agent's system prompt (built from SOUL.md, AGENTS.md, TOOLS.md, etc. via `build_enhanced_prompt()`). You can override this with a custom `system_prompt` parameter for specialised tasks.

### Timeout and Cancellation

Sub-agents are protected by two mechanisms:

1. **Timeout**: Wraps the agent execution in `tokio::time::timeout`. Default 600 seconds (10 minutes). When exceeded, the task status becomes `timed_out`.
2. **Cancellation token**: The `cancel` action triggers a `CancellationToken` that gracefully stops the background task. Status becomes `cancelled`.

Both are enforced via `tokio::select!` — whichever fires first wins.

### Concurrency Limit

A maximum of **5 sub-agents** can run concurrently per manager instance (defined by `MAX_CONCURRENT`). Attempting to spawn beyond this limit returns an error:

```
Max concurrent sub-agents reached (5)
```

### Result Storage

Results are stored in memory (not persisted to disk) and capped at **4000 characters** (`MAX_RESULT_LEN`). The full session with tool invocations is saved via the session store, so the sub-agent's work history is preserved even if the result is truncated.

### Slack Announcement

When a sub-agent completes, it announces the result in the parent Slack thread:

**Success:**
```
:arrows_counterclockwise: *Sub-agent complete* — _Analyse the last 7 days of error logs..._
> ## Top 5 Recurring Issues
> 1. Connection timeout to database (47 occurrences)...
```

**Failure:**
```
:x: *Sub-agent failed* — _Analyse the last 7 days of error logs..._
> Agent error: LLM provider returned 429 Too Many Requests
```

Announcements are truncated to 2000 characters. The messaging context (channel ID and thread ID) is auto-captured from the parent conversation, so agents don't need to manually pass these values.

## Task Lifecycle

```
spawn()
   │
   ▼
Running ──────────────┐
   │                  │
   ├─ success ──► Completed ──► result available
   │                  │
   ├─ error ────► Failed ──────► error available
   │                  │
   ├─ timeout ──► TimedOut ────► error available
   │                  │
   └─ cancel ───► Cancelled ───► error available
```

All terminal states (`Completed`, `Failed`, `TimedOut`, `Cancelled`) set `finished_at` and clean up the cancellation token.

## Policy Configuration

The `subagent` tool is controlled by the policy engine like any other tool. Use `tool: "subagent"` in your policy rules.

**Allow sub-agents for a specific agent:**

```yaml
rules:
  - name: "allow-subagents"
    scope:
      agent: "primary"
    tool: "subagent"
    action: allow
```

**Allow sub-agents for all agents:**

```yaml
rules:
  - name: "allow-subagents-global"
    scope: global
    tool: "subagent"
    action: allow
```

**Deny sub-agents globally, allow for specific team:**

```yaml
rules:
  - name: "deny-subagents"
    scope: global
    tool: "subagent"
    action: deny

  - name: "allow-subagents-for-devops"
    scope:
      team: "devops"
    tool: "subagent"
    action: allow
```

See [POLICY_ENGINE.md](POLICY_ENGINE.md) for full policy syntax.

## Use Cases

### Parallel Research

Agent needs to investigate multiple topics simultaneously:

```
User: "Compare Redis, Memcached, and DragonflyDB for our caching layer"

Agent spawns 3 sub-agents:
  - Sub-agent 1: "Research Redis: features, performance benchmarks, Rust client ecosystem"
  - Sub-agent 2: "Research Memcached: features, performance benchmarks, Rust client ecosystem"
  - Sub-agent 3: "Research DragonflyDB: features, performance benchmarks, Rust client ecosystem"

Agent continues conversation: "I've kicked off three parallel research tasks..."

[Sub-agents complete and announce results in thread]

Agent collects results and synthesises a comparison.
```

### Background Code Review

```
User: "Review PRs #41, #42, and #43"

Agent spawns 3 sub-agents, each reviewing one PR.
Agent can respond to other messages while reviews run.
Results appear in the thread as each review completes.
```

### Cost-Optimised Delegation

Use a cheaper model for simple sub-tasks:

```json
{
  "action": "spawn",
  "task": "Format this JSON and fix the indentation: { ... }",
  "model": "claude-haiku-4-5-20251001",
  "timeout_secs": 60
}
```

The parent agent uses Opus for complex reasoning while delegating mechanical tasks to Haiku.

### Long-Running Analysis

```json
{
  "action": "spawn",
  "task": "Run the full test suite, analyse failures, and create a report",
  "timeout_secs": 900
}
```

The 15-minute timeout accommodates slow test suites without blocking the main conversation.

## Source Files

| File | Purpose |
|------|---------|
| `crates/hub/src/subagent.rs` | `SubAgentManager`, `SubAgentTask`, `SubAgentStatus` — core lifecycle management |
| `crates/hub/src/subagent_tool.rs` | `SubAgentTool` — the `Tool` trait implementation with all five actions |
| `crates/hub/src/serve.rs` | `AgentService::run_isolated()` — creates and runs isolated sub-agent sessions |
| `crates/cli/src/main.rs` | Registration — creates the manager and registers the tool at startup |

## Limitations

- **No persistence**: Sub-agent task state lives in memory. Tasks are lost on hub restart. (The sub-agent's session and tool invocations are persisted via the session store.)
- **No inter-agent communication**: Sub-agents cannot communicate with each other or the parent during execution. They run independently and deliver results on completion.
- **Result size cap**: Results are truncated to 4000 characters in the manager. Use the session store to access full transcripts.
- **Concurrency limit**: Maximum 5 concurrent sub-agents. This is a safety limit to prevent runaway spawning.
- **Same tools**: Sub-agents inherit the parent's tool registry. There is no per-sub-agent tool filtering (use policy rules for this).
