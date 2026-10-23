# Tools

Tools are the capabilities that agents can use to interact with the world — reading files, running commands, searching memory, setting reminders. Every tool follows the same interface, making it straightforward to add new ones.

## Architecture

```
Agent (LLM)
    │
    ├─ Decides to use a tool
    │  (based on tool definitions sent in the system prompt)
    │
    ▼
Policy Engine
    │
    ├─ Evaluates the request against rules
    │  (see docs/POLICY_ENGINE.md)
    │
    ├─ Allow → execute the tool
    ├─ Deny → return error to agent
    └─ RequireApproval → run only once a human approves
    │
    ▼
Tool Execution
    │
    ├─ Deserialise JSON input
    ├─ Do the work (async)
    └─ Return ToolResult (success/failure + output + timing)
    │
    ▼
Agent receives result, continues reasoning
```

Tools run in parallel when the LLM requests multiple tools in a single round. The runner uses `tokio::JoinSet` to execute them concurrently, which significantly speeds up rounds with several tool calls.

## Built-in Tools

Run `workforce tools` to see all available tools. Here's the current set:

### Filesystem Tools

| Tool | Description |
|---|---|
| `filesystem.read` | Read file contents, with optional `offset` and `limit` for large files |
| `filesystem.write` | Write content to a file, creating parent directories if needed |
| `filesystem.edit` | Surgical find-and-replace editing (old string must match exactly once) |
| `filesystem.list` | List directory contents with file types and sizes |

Filesystem tools accept an `allowed_paths` list at construction. If provided, all operations are sandboxed to those directories. Paths outside the allowed set return an access denied error.

### Execution

| Tool | Description |
|---|---|
| `exec.run` | Execute a shell command via `sh -c`, capturing stdout, stderr, and exit code |

Supports optional `workdir` and `timeout_secs` parameters. Default timeout is 30 seconds. Commands that exceed the timeout are killed and return a timeout error.

### Memory

| Tool | Description |
|---|---|
| `memory_search` | Search across identity files (SOUL.md, AGENTS.md, etc.) and the vector memory store |
| `memory` | Store, recall, and manage memories in Weaviate (requires Weaviate to be running) |

The `memory_search` tool works without Weaviate by searching local identity files. When Weaviate is configured, it also searches the vector store for semantic matches.

The `memory` tool provides full CRUD operations on the vector store with layers (working, episodic, semantic, team, organisation), importance scores, tags, and visibility controls.

### Scheduler

| Tool | Description |
|---|---|
| `scheduler` | Create, list, update, and remove scheduled jobs for reminders and recurring tasks |

Only available in daemon mode (`workforce serve`). Supports one-shot reminders (fire once at a specific time or after a delay) and recurring cron-style jobs.

| Action | What it does |
|---|---|
| `create` | Add a job: `message`, one of `in_time` / `at` / `every_secs` / `cron`, and optionally `name`, `description`, `timezone`, `channel_id`, `thread_id`, `model`, `tools`, `agent_owns_delivery`, `context`, `max_runs`, `expires_in`. |
| `list` | The jobs the caller may see, each with its `context`, `tools` and `run_count`, and `spent_usd` on those the caller may change. |
| `update` | Change a job in place (owner or admin): `name`, schedule, `enabled`, `visibility`, `context`, `tools` (`[]` restores the full toolset). |
| `remove`, `run`, `history` | Delete a job, fire it now, read its recent runs (owner or admin). |
| `set_bill_to` | Admin only: charge a job's spend to another identity. |

`context` is the session a job's runs use. `isolated` (the default) is a fresh session every run. `conversation` pins the job to one session of its own, minted by the scheduler: every run replays all its earlier runs, so each run costs more than the one before, and a recurring job needs the user's confirmation (`confirmed: true`) to be created or switched that way. `channel_id` is delivery only and never selects a session; a caller cannot name a session key.

A job's mode is read off its stored payload: it is pinned exactly when `payload.channel` is set. Pinning stores a key minted for the job (`scheduler:pinned:{job id}:{uuid}`); a job stored by an earlier version may hold a conversation's key (a surface name, a DM id). Either way the run's session is `scheduler:{person}:{key}` for the person the job runs as (the key alone for a person-less system job), so no two people's runs share one. Switching to `isolated` and back mints a new key. A pinned session rolls over once it holds `[compaction] chat_rollover_max_messages` messages, and `[scheduler] pinned_sessions = false` makes an agent's pinned recurring jobs run fresh, noted on each run record.

An edit asks for the user's confirmation, with the same estimate as `create`, when it pins a recurring job or moves a job to a schedule that runs more often and is projected over `[budget] confirm_recurring_daily_usd`. The check lives in `Scheduler::update_job`, so it holds on every surface: the tool's `update` answers `needs_confirmation`, and `PUT /api/schedules/{id}` and the admin `PUT /api/scheduler/{id}` answer `409` with the same estimate until the edit is re-sent with `confirmed: true`. `spent_usd` is listed on the caller's own jobs (every job, for an admin).

Every edit goes through `Scheduler::update_job(job_id, JobEdit, editor)`. One that changes the schedule, session, tools or visibility records the editor in `edited_by` (the caller's identity; the owner token as `rest:owner-token`; the automated source when made inside a heartbeat, monitor, workflow or job turn); a save by the job's creator records the creator, which makes the job theirs again. A rename or pause records nothing. The field is written only when it changes, so a stale write from another replica never drops or forges it.

### Sub-Agents

| Tool | Description |
|---|---|
| `subagent` | Spawn background sub-agents for parallel task execution |

Lets agents spawn isolated background tasks that run without blocking the main conversation. Sub-agents can use a different model, have configurable timeouts, and announce results back to the parent Slack thread when complete. See [SUB_AGENTS.md](SUB_AGENTS.md) for full documentation.

### Compaction

| Tool | Description |
|---|---|
| `compaction` | View, configure, and trigger context window compaction |

Lets agents self-manage their context window. Actions: `get` (show config), `set` (update fields at runtime), `reset` (revert to TOML config), `status` (show token usage vs thresholds), `compact` (trigger manual compaction). See [COMPACTION.md](COMPACTION.md) for full documentation.

### System

| Tool | Description |
|---|---|
| `system.logs` | Read and search hub logs (in-memory ring buffer) |
| `settings` | The agent's effective settings: each value, its source and its status |

Provides agents with access to the hub's runtime logs for self-diagnosis and platform work. Supports `read` (last N lines) and `search` (filter by level, target, keyword, time range) actions.

`settings` answers what is configured and why: `show` (a section, or a summary of all of them) and `explain` (one setting's chain from default through install and agent config to runtime). It reads the values the runtime itself merged, never shows a secret, and shows install-wide tables to admins only.

### Identity lifecycle (admin)

| Action | What it does |
|---|---|
| `holdings` | What one identity holds, category by category, with what a merge and a delete would do. |
| `merge`, `delete` | Merge an identity into another (its usage and cost records go with it), or delete it; dry runs unless `dry_run` is `false`. A real one starts in the background and answers with its operation. |
| `transfer` | Give one job, workflow, playbook, monitor or consolidation template to another identity. |
| `operations` | The lifecycle operations recorded, newest first. |
| `operation` | One operation by `id`: its progress while it runs, then its report. |
| `orphans` | What identities that no longer exist left behind: counts per missing identity and category, ids nobody can prove were identities, names several missing identities shared, and counts of owners that are not people. Reads only. |
| `transfer_orphan` | Hand one missing identity's holdings to a live identity (`to`): jobs, definitions, shared artifacts and skills go as the admin's edit; credentials, devices, roles, access and private artifacts are erased; conversations, memories, notes and documents stay where they are unless `move_personal` is `true`, when they move as in a merge. Dry run unless `dry_run` is `false`. |
| `delete_orphan` | Erase one missing identity's holdings by the delete rules (`recipient`, `choices` as for `delete`), keeping billing and audit records. Dry run unless `dry_run` is `false`. |

`identity.lifecycle` is for an active System Admin only, and every call waits for a person's approval, so no scheduled job or workflow step can run it. The same operations are on the API (`/api/identities/{id}/…` and `/api/identity-lifecycle/orphans[/{id}/transfer|delete]`) and in the admin UI (a user's *Merge & delete* tab, and *Users → Orphaned data*).

## The Tool Trait

Every tool implements the `Tool` trait from `workforce-agent`:

```rust
use async_trait::async_trait;
use eyre::Result;

#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique name using "category.action" convention (e.g. "git.push").
    fn name(&self) -> &str;

    /// Human-readable description — sent to the LLM so it knows what the tool does.
    fn description(&self) -> &str;

    /// JSON Schema describing expected input parameters.
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given input.
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult>;
}
```

The trait requires `Send + Sync` because tools execute concurrently across async tasks. Use `async_trait` for the async `execute` method.

## ToolResult

Every tool execution returns a `ToolResult`:

```rust
pub struct ToolResult {
    /// Whether the tool execution succeeded.
    pub success: bool,
    /// The output (arbitrary JSON — the LLM sees this).
    pub output: serde_json::Value,
    /// Error message if the tool failed.
    pub error: Option<String>,
    /// Execution time in milliseconds.
    pub duration_ms: u64,
}
```

Use the convenience constructors:

```rust
// Success
ToolResult::success(serde_json::json!({"file": "created"}), 42)

// Failure
ToolResult::failure("File not found", 3)
```

**Important:** Return `Ok(ToolResult::failure(...))` for expected errors (file not found, permission denied, invalid input). Reserve `Err(...)` for unexpected internal errors. This way the agent sees a clean error message and can decide what to do, rather than the entire run failing.

## Creating a New Tool

Here's a complete example — a tool that fetches HTTP URLs:

```rust
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};

/// Tool for fetching HTTP URLs.
pub struct HttpFetch {
    client: reqwest::Client,
    timeout_secs: u64,
}

impl HttpFetch {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            timeout_secs,
        }
    }
}

#[derive(Deserialize)]
struct HttpFetchInput {
    url: String,
    #[serde(default = "default_method")]
    method: String,
}

fn default_method() -> String {
    "GET".to_string()
}

#[async_trait]
impl Tool for HttpFetch {
    fn name(&self) -> &str {
        "http.fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from an HTTP URL. Returns the response body, status code, and headers."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "method": {
                    "type": "string",
                    "description": "HTTP method (GET, POST, etc.). Defaults to GET.",
                    "enum": ["GET", "POST", "PUT", "DELETE"]
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: HttpFetchInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let timeout = std::time::Duration::from_secs(self.timeout_secs);

        let request = match input.method.as_str() {
            "POST" => self.client.post(&input.url),
            "PUT" => self.client.put(&input.url),
            "DELETE" => self.client.delete(&input.url),
            _ => self.client.get(&input.url),
        };

        match request.timeout(timeout).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();

                // Truncate very large responses
                let truncated = if body.len() > 50_000 {
                    format!("{}... (truncated, {} bytes total)", &body[..50_000], body.len())
                } else {
                    body
                };

                Ok(ToolResult::success(
                    serde_json::json!({
                        "status": status,
                        "body": truncated,
                        "url": input.url,
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("HTTP request failed: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}
```

## Registering a Tool

Tools are registered in the `ToolRegistry`, a `BTreeMap<String, Arc<dyn Tool>>` keyed by name so definitions are always offered in the same order:

```rust
use std::sync::Arc;
use workforce_agent::tools::registry::ToolRegistry;

let mut registry = ToolRegistry::new();

// Register built-in tools
registry.register(Arc::new(ReadFile::new(vec![workdir.clone()])));
registry.register(Arc::new(WriteFile::new(vec![workdir.clone()])));
registry.register(Arc::new(ExecCommand::default()));

// Register your custom tool
registry.register(Arc::new(HttpFetch::new(30)));
```

The `default_tools()` function in `workforce_agent::runner` creates a registry with all the standard filesystem and exec tools:

```rust
pub fn default_tools(allowed_paths: Vec<PathBuf>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadFile::new(allowed_paths.clone())));
    registry.register(Arc::new(WriteFile::new(allowed_paths.clone())));
    registry.register(Arc::new(EditFile::new(allowed_paths.clone())));
    registry.register(Arc::new(ListDir::new(allowed_paths)));
    registry.register(Arc::new(ExecCommand::default()));
    registry
}
```

To add your tool, either:
1. Add it to `default_tools()` if it should always be available.
2. Register it after calling `default_tools()` for optional tools (like memory or scheduler).

## How Tools Reach the LLM

When the runner calls the LLM, it converts every registered tool's definition into the LLM's tool format:

1. `registry.definitions()` collects all `ToolDefinition` structs.
2. Each definition is mapped to an `LlmToolDef` with `name`, `description`, and `input_schema`.
3. These are sent in the API request so the LLM knows what tools are available.
4. The LLM responds with tool use blocks containing the tool name and JSON input.
5. The runner looks up each tool by name in the registry, checks the policy, and calls `execute()`.
6. Results are fed back to the LLM as tool result messages for the next round.

This loop continues until the LLM responds with text only (no tool calls) or the maximum rounds limit is reached.

## Tool Naming Convention

Tools follow a `category.action` pattern:

- `filesystem.read`, `filesystem.write`, `filesystem.edit`, `filesystem.list`
- `exec.run`
- `git.push`, `git.commit`, `git.branch`
- `email.send`, `email.read`
- `http.fetch`, `http.post`

This convention enables wildcard matching in policy rules. A rule targeting `filesystem.*` matches all filesystem tools. A rule targeting `*` matches everything.

Single-word names (like `scheduler` and `memory_search`) also work but don't support category-level wildcards.

## Progress Events

Tool executions emit progress events that surfaces (CLI, WebSocket gateway) can display:

- **ToolStart** — Emitted when a tool begins executing. Includes the tool name and a human-readable detail string (e.g. the file path or command being run).
- **ToolEnd** — Emitted when a tool finishes. Includes success/failure status and execution time.

The `tool_detail()` function in the runner extracts a summary from each tool's input parameters:

| Tool Pattern | Detail Source |
|---|---|
| `*read*`, `*write*`, `*edit*` | `path` or `file_path` parameter |
| `*list*` | `path` or `directory` parameter |
| `*exec*` | `command` parameter (truncated to 80 chars) |
| `*search*` | `query` parameter |
| `scheduler` | `action` parameter |
| Other | Empty string |

## Testing Tools

Tools are easy to test since they're just async functions:

```rust
#[tokio::test]
async fn my_tool_works() {
    let tool = HttpFetch::new(10);

    let result = tool
        .execute(serde_json::json!({
            "url": "https://httpbin.org/get"
        }))
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(result.output["status"], 200);
}

#[tokio::test]
async fn my_tool_handles_errors() {
    let tool = HttpFetch::new(1);

    let result = tool
        .execute(serde_json::json!({
            "url": "https://nonexistent.invalid"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.is_some());
}
```

Test both happy paths and error cases. Remember that expected errors should return `Ok(ToolResult::failure(...))`, so your tests should assert on `result.success` rather than expecting `Err`.

## Crate Layout

```
crates/agent-runtime/src/tools/
├── mod.rs          # Tool trait, ToolResult, ToolDefinition
├── registry.rs     # ToolRegistry (HashMap-based lookup)
├── filesystem.rs   # ReadFile, WriteFile, EditFile, ListDir
└── exec.rs         # ExecCommand

crates/hub/src/
├── memory_tool.rs      # MemorySearchTool, MemoryTool
├── scheduler_tool.rs   # SchedulerTool
├── compaction_tool.rs  # CompactionTool (get, set, reset, status, compact)
├── subagent.rs         # SubAgentManager (lifecycle, spawn, announce)
├── subagent_tool.rs    # SubAgentTool (spawn, status, list, cancel, result)
├── logs_tool.rs        # SystemLogsTool (read, search)
└── system_tool.rs      # SystemTool
```

Core tool types (`Tool`, `ToolResult`, `ToolDefinition`, `ToolRegistry`) live in `workforce-agent`. Runtime-specific tools that need access to hub services (memory store, scheduler) live in `workforce-hub`.
