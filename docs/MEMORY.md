# Memory Management

Workforce agents have a dual memory system: **filesystem memory** for persistent context that loads automatically into every session, and a **vector store** for semantic search across accumulated knowledge. Session compaction bridges long conversations by flushing important context to daily notes before summarising old messages.

## Architecture Overview

```
                    ┌─────────────────────────────────────────┐
                    │             System Prompt                │
                    │  (stable across turns)                  │
                    ├─────────────────────────────────────────┤
                    │  SOUL.md, IDENTITY.md, USER.md          │
                    │  AGENTS.md (operational guidelines)     │
                    │  TOOLS.md                               │
                    │  MEMORY.md (truncated to 4000 chars)    │
                    ├─────────────────────────────────────────┤
                    │  Per-turn context block                 │
                    │  (leads the task message, every turn)   │
                    ├─────────────────────────────────────────┤
                    │  Yesterday's + today's daily notes      │
                    │  active-task.md                         │
                    │  Date/time, workspace path              │
                    └───────────────┬─────────────────────────┘
                                    │
    ┌──────────────────┬────────────┼────────────┬──────────────────┐
    │                  │            │            │                  │
 Filesystem       Vector Store  Compaction   Passive            Periodic
 Memory           (on-demand)   (auto-flush) Extraction         Consolidation
 (always loaded)                             (post-turn)        (scheduled)
    │                  │            │            │                  │
 memory/*.md      SQLite /      memory_     LLM extracts      Agent reviews
 MEMORY.md        Weaviate      flush() →   facts after       episodic →
                                writer      each turn         semantic
    │                  │
 ┌──┴───┐       ┌──────┴───────┐
 │notes │       │ memory       │
 │ tool │       │ memory_search│
 └──────┘       └──────────────┘
```

**Filesystem memory** is the primary continuity mechanism. Files in the agent's workspace are loaded into the prompt at the start of every message — identity files into the system prompt, daily notes and the active task into the per-turn context block that leads the task message — so the agent always has this context without needing to call any tools.

**Vector store memory** provides semantic search. Agents call the `memory` tool to store and recall facts. Results are returned on demand, not injected automatically.

**Compaction** protects against context loss. When sessions grow large, the compaction engine asks the LLM to identify critical information, then persists it to daily notes via the `MemoryWriter` trait before summarising old messages.

**Passive extraction** (optional) runs a lightweight LLM call after each substantive turn to extract memorable facts without the agent needing to explicitly decide to store them. See [Passive Memory](#passive-memory).

**Periodic consolidation** (optional) merges fragmented episodic memories into consolidated semantic memories on a schedule. See [Passive Memory](#passive-memory).

## Filesystem Memory

### Workspace Layout

Each agent has a workspace directory containing identity files and a `memory/` subdirectory:

```
{workspace}/
├── SOUL.md              # Personality, values, style
├── IDENTITY.md          # Name, avatar, vibe
├── AGENTS.md            # Operational guidelines
├── TOOLS.md             # Tool-specific notes
├── HEARTBEAT.md         # Periodic task config
├── WELCOME.md           # New-session greeting
└── memory/
    ├── 2026-02-12.md    # Today's daily note
    ├── 2026-02-11.md    # Yesterday's daily note
    ├── active-task.md   # Current task bookmark
    └── archive/         # Old notes (auto-archived)
```

### What Gets Injected Into the Prompt

Every time the agent processes a message, the prompt carries these sections in this order. The first six form the stable system prompt; the rest are per-turn context, which the hub sends as the `[SYSTEM CONTEXT …]` block leading the task message so the system prompt stays byte-identical across turns (isolated runs such as workers and the CLI keep them at the end of the system prompt via `AgentIdentity::system_prompt()`):

| Section | Source | Notes |
|---------|--------|-------|
| Your Identity | `SOUL.md` | |
| Who You Are | `IDENTITY.md` | |
| About Your Human | `USER.md` | |
| Operational Guidelines | `AGENTS.md` | Falls back to built-in defaults if missing |
| Tool Notes | `TOOLS.md` | |
| Long-Term Memory | `MEMORY.md` | Truncated to 4000 characters if larger |
| Recent Daily Notes | `memory/{yesterday}.md` + `memory/{today}.md` | Both loaded if they exist |
| Active Task | `memory/active-task.md` | Loaded with "Resume if appropriate" prompt |
| Date & Time | System clock | User timezone if configured |
| Workspace | Workspace path | |

This means the agent always has access to its identity, recent notes, and active task without making any tool calls.

### Default Agent Guidelines

When no custom `AGENTS.md` exists in the workspace, agents get built-in operational guidelines that instruct them to:

1. **Every session:** Check date/time, read identity files, check `active-task.md`, scan today's and yesterday's daily notes
2. **Daily notes:** Write to `memory/YYYY-MM-DD.md` as they work — progress, decisions, blockers
3. **Long-term memory:** Periodically distil important information from daily notes into `MEMORY.md`
4. **Active task:** Write current state to `memory/active-task.md`, update as work progresses, clear when done

### `notes` Tool

The `notes` tool gives agents direct access to filesystem memory. It wraps `MemoryManager` methods.

**Input:**

```json
{
  "action": "write_daily | read_daily | set_task | get_task | clear_task | append_memory",
  "content": "string (required for write_daily, set_task, append_memory)",
  "date": "YYYY-MM-DD (optional, for read_daily — defaults to today)"
}
```

**Actions:**

| Action | Description | Requires |
|--------|-------------|----------|
| `write_daily` | Append to today's daily note (`memory/YYYY-MM-DD.md`) | `content` |
| `read_daily` | Read a daily note (defaults to today) | `date` (optional) |
| `set_task` | Write to `memory/active-task.md` | `content` |
| `get_task` | Read `memory/active-task.md` | — |
| `clear_task` | Remove `memory/active-task.md` | — |
| `append_memory` | Store a curated long-term fact in the vector store | `content` |

**Example — write a daily note:**

```json
{
  "action": "write_daily",
  "content": "## Deployment Discussion\n- Decided on blue-green deploys\n- Cutover window: 2am UTC"
}
```

**Response:**

```json
{ "action": "write_daily", "file": "2026-02-12.md", "status": "written" }
```

## Vector Store Memory

### Memory Structure

Each memory entry has:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | UUID | Auto-generated | Unique identifier |
| `content` | String | — | The memory text |
| `layer` | Enum | — | Classification (see below) |
| `owner` | AgentId | Creator | Who created this memory |
| `visibility` | Enum | `private` | Access control |
| `importance` | Float | `0.5` | Relevance score (0.0–1.0, clamped) |
| `tags` | String[] | `[]` | Metadata tags |
| `access_count` | Integer | `0` | How many times recalled |
| `deleted` | Boolean | `false` | Soft-delete flag |

### Memory Layers

Memories are classified into hierarchical layers:

| Layer | Scope | Lifetime | Use Case |
|-------|-------|----------|----------|
| `working` | Current session | Session | Immediate task context |
| `episodic` | Per-agent | Long-term | Agent experiences, specific events |
| `semantic` | Per-agent | Long-term | Domain knowledge, patterns, facts |
| `team` | Shared team | Long-term | Team conventions, shared decisions |
| `organisation` | Global | Long-term | Company policies, org-wide knowledge |

### Visibility

| Level | Description |
|-------|-------------|
| `private` | Only the owning agent (default) |
| `team:{id}` | All members of a specific team |
| `global` | All agents in the organisation |

### `memory` Tool

Full CRUD operations on the vector store.

**Actions:**

| Action | Description | Required Input |
|--------|-------------|---------------|
| `remember` | Store a new memory | `content` |
| `recall` | Semantic search for memories | `query` |
| `forget` | Soft-delete a memory by ID | `memory_id` |
| `count` | Count memories matching a filter | — |

**Example — store a memory:**

```json
{
  "action": "remember",
  "content": "The deployment uses blue-green strategy with 2am UTC cutover",
  "layer": "semantic",
  "importance": 0.8,
  "tags": ["deployment", "infrastructure"],
  "visibility": "team:backend"
}
```

**Response:**

```json
{
  "memory_id": "a1b2c3d4-...",
  "content": "The deployment uses blue-green strategy...",
  "layer": "semantic",
  "status": "stored"
}
```

**Example — recall memories:**

```json
{
  "action": "recall",
  "query": "deployment strategy",
  "layer": "semantic",
  "max_results": 5
}
```

**Response:**

```json
{
  "memories": [
    {
      "memory_id": "a1b2c3d4-...",
      "content": "The deployment uses blue-green strategy...",
      "layer": "semantic",
      "importance": 0.8,
      "tags": ["deployment", "infrastructure"],
      "score": 0.92,
      "access_count": 3
    }
  ],
  "count": 1,
  "query": "deployment strategy"
}
```

### `memory_search` Tool

Hybrid search across both filesystem memory and the vector store.

**Input:**

```json
{
  "query": "search keywords or natural language",
  "max_results": 10,
  "layer": "episodic"
}
```

Only `query` is required. Results combine:

1. **File-based search** — keyword matching across MEMORY.md, daily notes (up to 30 most recent), SOUL.md, AGENTS.md, USER.md, TOOLS.md, IDENTITY.md
2. **Vector store search** — semantic similarity search (if a store is configured)

Results are merged, sorted by score (descending), and truncated to `max_results`.

### Backends

Three vector store backends are available:

| Backend | Config Value | Description |
|---------|-------------|-------------|
| SQLite | `"sqlite"` (default) | FTS5 keyword search with optional OpenAI embeddings for hybrid search |
| Weaviate | `"weaviate"` | Full vector DB with `text2vec-transformers` for semantic search |
| In-Memory | `"in_memory"` | HashMap-based, keyword scoring only. Lost on restart. For testing/dev. |

When using SQLite without embeddings (`embedding_provider = "none"`), search uses BM25 keyword matching only. Adding an embedding provider enables hybrid scoring (keyword + vector similarity, weighted by `vector_weight`).

## Configuration

### `[memory]` — Vector Store Backend

```toml
[memory]
backend = "sqlite"                               # "sqlite" | "weaviate" | "in_memory"
db_path = "data/memory.db"                       # SQLite database path (sqlite backend only)

# Optional: enable embeddings for semantic search
# embedding_provider = "openai"                  # "openai" or "none" (default)
# embedding_model = "text-embedding-3-small"
# embedding_api_url = "https://api.openai.com/v1/embeddings"
# embedding_api_key = "sk-..."                   # Or set env var (see below)
```

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | `"sqlite"` | Which vector store backend to use |
| `db_path` | `"data/memory.db"` | Path to SQLite database |
| `embedding_provider` | `"none"` | `"openai"` for vector search, `"none"` for keyword-only |
| `embedding_model` | `"text-embedding-3-small"` | Embedding model name |
| `embedding_api_url` | `"https://api.openai.com/v1/embeddings"` | OpenAI-compatible endpoint |
| `embedding_api_key` | — | API key (prefer env var) |

### `[weaviate]` — Weaviate Connection

Only needed when running Weaviate separately (e.g. production with `text2vec-transformers`):

```toml
[weaviate]
enabled = true
url = "http://localhost:8080"
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Whether to use Weaviate as the memory store |
| `url` | `"http://localhost:8080"` | Weaviate HTTP URL |

When `weaviate.enabled = true`, the hub connects to Weaviate and creates a `WorkforceMemory` collection if it doesn't exist. If Weaviate is unreachable, it falls back to in-memory.

### `[session_lifecycle]` — Session Expiry

```toml
[session_lifecycle]
daily_reset_hour = 4             # Reset all sessions at 4am (user timezone)
idle_expiry_minutes = 60         # Expire after 60 minutes of inactivity
archive_expired = true           # Archive to disk before clearing
check_interval_secs = 300        # Check every 5 minutes
```

| Field | Default | Description |
|-------|---------|-------------|
| `daily_reset_hour` | — (disabled) | Hour (0–23) to reset all sessions daily, in user timezone |
| `idle_expiry_minutes` | — (disabled) | Expire idle sessions after N minutes. 0 or absent = disabled |
| `archive_expired` | `true` | Archive expired sessions to disk before clearing |
| `check_interval_secs` | `300` | How often to check for expired sessions (seconds) |

### `[session_policies]` — Per-Type and Per-Channel Overrides

Policies layer on top of `[session_lifecycle]` for fine-grained control. Resolution order:
1. Channel-specific override (highest priority)
2. Context-type default
3. Global `[session_lifecycle]` config

```toml
# DMs get longer idle expiry
[session_policies.types.dm]
idle_expiry_minutes = 120

# Threads expire quickly
[session_policies.types.thread]
daily_reset = false
idle_expiry_minutes = 15

# CLI sessions never expire
[session_policies.channels.cli]
daily_reset = false
idle_expiry_minutes = 0
```

| Field | Type | Description |
|-------|------|-------------|
| `daily_reset` | `Option<bool>` | Override whether daily resets apply. `None` = inherit. |
| `reset_hour` | `Option<u32>` | Override the reset hour (0–23). `None` = inherit. |
| `idle_expiry_minutes` | `Option<u32>` | Override idle expiry. `0` = explicitly disable. `None` = inherit. |

### Environment Variable Overrides

| Variable | Overrides |
|----------|-----------|
| `WORKFORCE_MEMORY_BACKEND` | `memory.backend` |
| `WORKFORCE_MEMORY_DB_PATH` | `memory.db_path` |
| `WORKFORCE_EMBEDDING_API_KEY` | `memory.embedding_api_key` |
| `OPENAI_API_KEY` | `memory.embedding_api_key` (fallback) |
| `WORKFORCE_WEAVIATE_URL` | `weaviate.url` |

## Session Compaction

When sessions grow large, the compaction engine manages the context window to prevent exceeding LLM limits.

### How It Triggers

Compaction checks happen before every LLM call. The trigger condition:

```
estimated_tokens > max_context_tokens × compaction_threshold
```

With defaults: triggers when estimated tokens exceed 160,000 (200,000 × 0.8).

### Compaction Process

Three steps, applied in order:

1. **Prune tool outputs** (lightweight) — Truncate large tool outputs (>2000 chars) in older messages. If this brings tokens below the threshold, stop here.

2. **Memory flush** (if enabled) — Make a separate LLM call asking the model to identify critical information from the messages about to be compacted. The output is persisted to today's daily note under a `## Memory Flush (compaction)` heading via the `MemoryWriter` trait.

3. **Summarise and replace** — Use the LLM to create a condensed summary of old messages. Replace everything except the most recent N messages and system messages with the summary.

### Configuration

Compaction uses hardcoded defaults (not yet configurable via `workforce.toml`):

| Setting | Default | Description |
|---------|---------|-------------|
| `max_context_tokens` | `200,000` | Maximum context window size in tokens |
| `compaction_threshold` | `0.8` | Fraction of max tokens that triggers compaction |
| `keep_recent_messages` | `20` | Number of recent messages to preserve during compaction |
| `enable_memory_flush` | `true` | Whether to flush context to memory before summarising |
| `prune_tool_outputs` | `true` | Whether to truncate large tool outputs as a first step |
| `prune_tool_output_max_chars` | `2,000` | Maximum chars for tool output before truncation |

### Slash Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `/compact` | `/gc` | Trigger manual compaction (memory flush + summarise) |
| `/context` | `/ctx` | Show context window usage (estimated tokens, threshold) |
| `/new [model]` | `/n` | Reset session (optionally switch model) |
| `/reset` | `/r`, `/clear` | Clear session history |
| `/status` | `/s` | Show session info (tokens, messages, model) |

## Passive Memory

By default, agents only store memories when they explicitly call `memory remember` or when compaction flushes context on long sessions. The **passive memory system** adds two automatic layers that capture knowledge without relying on the agent deciding to do it. Both layers default to **off** and must be explicitly enabled.

### Architecture

```
     User ↔ Agent Turn
            │
            ▼
  ┌─────────────────────┐
  │  Layer 1: Post-Turn  │  Fire-and-forget LLM call after each
  │  Extraction          │  substantive turn. Extracts memorable
  │  (immediate)         │  facts → stores as episodic memories
  └──────────┬──────────┘  via MemoryWriter (Weaviate/SQLite/file)
             │
             ▼
  ┌─────────────────────┐
  │  Layer 2: Periodic   │  Two-phase scheduled job:
  │  Consolidation       │  1. Scan recent sessions → extract
  │  (background)        │     facts (reuses Layer 1 extraction)
  └──────────┬──────────┘  2. Agent merges episodic → semantic,
             │                deduplicates, soft-deletes fragments
             ▼
     Sessions → Episodic → Semantic
```

### Layer 1: Post-Turn Extraction

After each user↔agent exchange, a lightweight LLM call runs in the background to extract any facts worth remembering. The extraction only fires when the combined user message + agent response exceeds a configurable token threshold (default 200 tokens), skipping trivial exchanges like greetings.

The extraction prompt focuses on:
- User preferences, decisions, and stated plans
- Project context and business decisions
- Personal details (name, role, team, goals)
- Feedback on agent behaviour
- Technical decisions and architectural choices

The LLM responds with concise bullet points or `NONE` if nothing is worth remembering. Extracted facts are stored as episodic memories via the existing `MemoryWriter` (same path as compaction flush — Weaviate/SQLite with file fallback). Memories are tagged with `source:passive`.

**Model selection:** uses `passive_model` if set, otherwise falls back to `compaction_model`, then the agent's default model. Set to `"cheap"` to use the least expensive available model.

### Layer 2: Periodic Consolidation

Runs every N hours (default 4) in two phases:

**Phase 1 — Session scanning:** A background task scans session transcripts updated since the last scan. For each substantive user↔assistant turn, it runs the same LLM extraction as Layer 1 and stores memorable facts as episodic memories. A cursor file (`memory/consolidation-cursor`) tracks the last scan timestamp so sessions are only processed once. Sessions that receive new messages after scanning get re-scanned on the next run.

**Phase 2 — Memory merging:** A scheduled `AgentTurn` job instructs the agent to review its episodic memories using the `memory` tool. The agent:

1. Counts memories per layer — skips if fewer than 5 episodic memories
2. Recalls recent episodic memories with broad queries
3. Identifies clusters of related memories about the same topic/person/decision
4. Creates consolidated semantic memories (importance 0.6–0.9, tagged `source:consolidation`)
5. Soft-deletes the episodic fragments that were merged
6. Deduplicates near-identical memories

Session scanning ensures that even if post-turn extraction (Layer 1) is disabled, conversations are still captured during consolidation. The lookback window (`consolidation_lookback_secs`, default 12 hours) controls how far back the first scan looks; subsequent scans use the cursor.

### Configuration

Both layers are configured in `[memory]` with per-agent overrides in `[[agents]]` or `{workspace}/config.toml`:

```toml
[memory]
passive = true                      # Enable post-turn extraction (default false)
passive_model = "cheap"             # Model for extraction LLM call
passive_min_turn_tokens = 200       # Skip turns below this threshold
# passive_prompt = "..."            # Custom extraction prompt (optional)
consolidation = true                # Enable periodic consolidation (default false)
consolidation_interval_secs = 14400 # Every 4 hours (default)
consolidation_lookback_secs = 43200 # 12-hour session scan window (default)
```

Per-agent override in `workforce.toml`:

```toml
[[agents]]
id = "atlas"

[agents.memory]
passive = true
passive_min_turn_tokens = 100       # Lower threshold for this agent
consolidation = true
```

Per-agent workspace override in `{workspace}/config.toml`:

```toml
[memory]
passive = true
passive_model = "cheap"
consolidation = true
consolidation_interval_secs = 7200  # Every 2 hours for this agent
```

Config merge follows the standard 3-layer priority: global `[memory]` → per-agent `[[agents]]` → workspace `config.toml`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `passive` | bool | `false` | Enable post-turn memory extraction |
| `passive_model` | Option\<String\> | None | Model for extraction (falls back to compaction_model) |
| `passive_min_turn_tokens` | u32 | `200` | Minimum turn size to trigger extraction |
| `passive_prompt` | Option\<String\> | None | Custom system prompt for extraction |
| `consolidation` | bool | `false` | Enable periodic memory consolidation |
| `consolidation_interval_secs` | u64 | `14400` | Consolidation interval (seconds) |
| `consolidation_lookback_secs` | u64 | `43200` | Session scan lookback window (seconds) |
| `consolidation_model` | Option\<String\> | None | Model for the merge phase (falls back to agent default) |

### How It Fits Together

```
Active Memory (agent-driven):
  memory remember  ──→ Vector store (episodic/semantic)
  notes append_memory ──→ Vector store (semantic) + MEMORY.md

Passive Memory (automatic):
  Post-turn extraction ──→ Vector store (episodic, source:passive)
  Periodic consolidation ──→ episodic → semantic (source:consolidation)

Compaction Memory (session-driven):
  memory_flush() ──→ Vector store (episodic, source:compaction)
```

All three paths write to the same vector store via `MemoryWriter`, so memories from all sources are searchable with `memory recall`.

### Layer Promotion

Memories can be promoted up the hierarchy:

```
Working → Episodic → Semantic → Team → Organisation
```

- **Agent → Team:** `MemoryLayerManager::promote_to_team()` changes visibility to team-wide
- **Team → Org:** `MemoryLayerManager::promote_to_org()` changes visibility to global

Teams can also store **conventions** (reusable patterns/rules) and organisations can store **policies** (enforced rules).

## Identity Files

Each agent workspace is bootstrapped with default identity files on first run. These files are loaded into the system prompt and can be customised.

| File | Purpose | Auto-Created |
|------|---------|-------------|
| `SOUL.md` | Core personality, values, communication style | Yes |
| `IDENTITY.md` | Name, avatar, vibe, emoji | Yes |
| `AGENTS.md` | Operational guidelines (memory management, session startup) | Yes |
| `TOOLS.md` | Tool-specific notes and usage tips | Yes |
| `HEARTBEAT.md` | Periodic background task configuration | Yes |
| `WELCOME.md` | Greeting message for new sessions | Yes |

All files are optional — if missing, the corresponding section is omitted from the system prompt (except `AGENTS.md`, which falls back to built-in defaults).

Identity files are hot-reloaded before every message, so edits take effect immediately without restarting the hub.

## Operational Guide

### Production Setup with Weaviate

For production deployments with full semantic search:

```toml
[weaviate]
enabled = true
url = "http://weaviate:8080"
```

Run Weaviate with the `text2vec-transformers` module for embedding generation. The hub auto-creates the `WorkforceMemory` collection on startup.

If Weaviate is unreachable at startup, the hub logs a warning and falls back to the in-memory store.

**Required Weaviate server config:** set `AUTOSCHEMA_ENABLED=false` on the Weaviate container. The hub's `ensure_collection()` pins property types, tokenisation and `invertedIndexConfig.stopwords` explicitly; auto-schema can race the first write and create a structurally-wrong class (for example `memoryId` inferred as `uuid` instead of `text`, or metadata fields with `tokenization: "word"` instead of `"field"`). Symptoms include `invalid search term, only stopwords provided` errors from identity-filtered recall. Since 2026-Q2 the hub validates the live schema shape against the expected property set at start-up and refuses to use a mismatching class.

### Recovering from a broken Weaviate schema

If the hub start-up log reports `Weaviate schema mismatch` or Weaviate is emitting `only stopwords provided` errors on recall:

- `workforce memory doctor` — read-only check. Compares the live schema against the expected property set and exits non-zero on mismatch, listing every diverging property.
- `workforce memory rebuild-schema --yes` — destructive. Exports every object via REST to a timestamped JSON backup, deletes the `WorkforceMemory` class, calls `ensure_collection()` to recreate it with the pinned shape, then re-ingests the backup. Runs as a dry-run by default; `--yes` is required to delete. Use the `--backup <path>` flag to choose where the backup is written.

Unrelated symptoms that can look similar but aren't a schema issue: `remote client vectorize: fail with status 500: 'int' object has no attribute 'detach'` means the `text2vec-transformers` sidecar is broken — pin a known-good image digest in the deployment or restart the sidecar. The hub can't fix that.

### Enabling Embeddings with SQLite

For semantic search without running Weaviate:

```toml
[memory]
backend = "sqlite"
embedding_provider = "openai"
embedding_model = "text-embedding-3-small"
# Set OPENAI_API_KEY or WORKFORCE_EMBEDDING_API_KEY env var
```

This gives hybrid search (BM25 keyword + vector similarity). Without embeddings, SQLite uses keyword search only.

### Monitoring Memory

- **`/context`** — Check current context window usage and compaction threshold
- **`/status`** — See session info including message count and estimated tokens
- **`memory_search` tool** — Agents can search across all memory files and vector store
- **`notes` tool with `read_daily`** — Read any daily note by date to check what was recorded

### Workspace Paths

Default workspace location: `{data_dir}/agents/{agent_id}/`

On the server (Docker): workspaces are at `/app/data/agents/{agent_id}/` with `/app/data` mounted from the host.

### Backup

To preserve agent memory:

- **Filesystem memory:** Back up the entire workspace directory (identity files + `memory/` subdirectory)
- **SQLite store:** Back up `data/memory.db`
- **Weaviate store:** Use Weaviate's backup API or volume snapshots
- **Sessions:** JSONL session files are stored in the workspace directory

### Session Storage

Sessions are stored as JSONL files in the agent's workspace. Each session is a separate file containing the full message history, compaction events, and metadata. Sessions are keyed by channel + user.
