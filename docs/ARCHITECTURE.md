# Architecture

Workforce is a Rust workspace of 11 crates that work together to orchestrate AI agents. This document describes how the crates fit together, what each one does, and the key design decisions.

## Workspace Layout

```
workforce/
├── Cargo.toml              # Workspace root (11 members)
├── crates/
│   ├── core/               # Foundational types (729 lines)
│   ├── common/             # Shared infrastructure (2723 lines)
│   ├── nats-bus/           # NATS messaging layer (818 lines)
│   ├── policy-engine/      # Policy evaluation (835 lines)
│   ├── agent-runtime/      # LLM + tools + runner (2892 lines)
│   ├── orchestrator/       # Multi-agent coordination (2764 lines)
│   ├── memory/             # Vector memory store (2417 lines)
│   ├── skills/             # Pluggable capabilities (2135 lines)
│   ├── distributed/        # Node registry + leaf nodes (1883 lines)
│   ├── hub/                # Server: API, gateway, Slack, scheduler (3640 lines)
│   └── cli/                # CLI binary (867 lines)
├── deploy/                 # Docker + systemd configs
├── docs/                   # Documentation
└── policies/               # Default policy files (created by `workforce init`)
```

**Total: ~21,700 lines of Rust, 282 tests.**

## Crate Dependency Graph

```
cli ──► hub ──► agent-runtime ──► core
                 │                   ▲
                 ├── orchestrator ───┤
                 ├── memory          │
                 ├── skills          │
                 └── distributed     │
                                     │
         common ─────────────────────┘
         nats-bus ───────────────────┘
         policy-engine ──────────────┘
```

- **core** has zero internal dependencies — only external crates (serde, uuid, chrono, strum)
- **common** depends on core
- **agent-runtime** depends on core and policy-engine
- **hub** depends on everything (it's the integration point)
- **cli** depends on hub, agent-runtime, common, core, and policy-engine

## Crate Details

### `workforce-core` — Foundation Types

The types that everything else builds on:

| Type | Purpose |
|------|---------|
| `AgentId` | Newtype around `String` identifying an agent |
| `TaskId` | Newtype around `Uuid` for task tracking |
| `Message` | Chat message with role, content, timestamp, metadata |
| `MessageRole` | Enum: System, User, Assistant, Tool |
| `Participant` | Agent or human with id, name, role, capabilities |
| `PolicyRequest` / `PolicyResponse` | Input/output for policy evaluation |
| `PolicyDecision` | Enum: Allow, Deny, Escalate, Audit |
| `Task` / `TaskStatus` | Task tracking with status, assignee, priority, deadline |

### `workforce-common` — Shared Infrastructure

Cross-cutting concerns used by multiple crates:

| Module | Purpose |
|--------|---------|
| `config` | Layered config loading (file → env vars) |
| `settings` | `WorkforceConfig` struct — the root configuration type |
| `identity` | `AgentIdentity` — loads SOUL.md, AGENTS.md, USER.md, MEMORY.md etc. from workspace |
| `session` | `SessionStore` trait + `InMemorySessionStore` for conversation tracking |
| `persistence` | SQLite-backed persistence for sessions and state |
| `cost` | Token cost tracking per agent, per team, per day |
| `admin` | Admin dashboard state (teams, agents, tasks, audit) |
| `errors` | Error handling setup (color-eyre) |
| `logging` | Tracing/logging setup (tracing-subscriber with env-filter) |
| `locking` | Distributed locking primitives |
| `tenancy` | Multi-tenant isolation (tenant context, scoping) |

### `workforce-nats` — NATS Messaging

The message bus for multi-agent communication:

| Module | Purpose |
|--------|---------|
| `client` | NATS client wrapper with auto-reconnect |
| `envelope` | Typed message envelopes with headers, routing, correlation IDs |
| `conversation` | Request-reply conversation patterns |
| `subjects` | Subject naming conventions (`workforce.agent.{id}.inbox`, etc.) |
| `streams` | JetStream configuration for persistent messaging |

### `workforce-policy` — Policy Engine

YAML-driven policy evaluation with hierarchical scope composition:

| Module | Purpose |
|--------|---------|
| `engine` | `PolicyEngine` — evaluates requests against rules |
| `rules` | Rule parsing, scope types, condition evaluation, glob matching |

**Rule evaluation order:** Rules are sorted by priority (descending), then by scope specificity (agent > role > team > global). The first matching rule wins.

**Scope hierarchy:**
1. `global` — applies to all agents
2. `team { team: "dev" }` — applies to agents in a specific team
3. `role { role: "lead" }` — applies to agents with a specific role
4. `agent { agent: "architect" }` — applies to a specific agent

**Condition operators:** `equals`, `not_equals`, `contains`, `matches` (regex), `greater_than`, `less_than`.

Example policy:
```yaml
rules:
  - name: "allow-filesystem"
    scope: global
    tool: "filesystem.*"
    action: allow
    priority: 10

  - name: "deny-rm-rf"
    scope: global
    tool: "exec.run"
    action: deny
    conditions:
      - field: command
        operator: contains
        value: "rm -rf"
    priority: 100
```

### `workforce-agent` — Agent Runtime

The core agent loop: LLM ↔ Tools with policy checks:

| Module | Purpose |
|--------|---------|
| `runner` | `AgentRunner` — the main agent execution loop |
| `chat` | `ChatSession` — multi-turn conversation with history |
| `llm/provider` | `LlmProvider` trait + `create_provider_async()` factory |
| `llm/anthropic` | Direct Anthropic API client (Messages API) |
| `llm/openrouter` | OpenRouter API client |
| `llm/streaming` | SSE stream parser for LLM responses |
| `llm/types` | `LlmRequest`, `LlmResponse`, `ToolUse`, `TokenUsage` |
| `tools/mod` | `Tool` trait, `ToolResult`, `ToolDefinition` |
| `tools/registry` | `ToolRegistry` — named tool lookup |
| `tools/filesystem` | File read/write/edit/list tools (sandboxed to allowed paths) |
| `tools/exec` | Shell command execution tool |

**Agent execution loop (`AgentRunner::run`):**
1. Send messages + tool definitions to LLM
2. If LLM returns tool calls → check each against policy engine
3. Execute allowed tools, collect results
4. Send tool results back to LLM
5. Repeat until LLM returns a text response or max rounds reached
6. Return final response + usage stats + tool invocation log

**LLM providers:**
- **`anthropic`** — Direct HTTP to `api.anthropic.com/v1/messages`. Pay-per-token.
- **`openrouter`** — HTTP to `openrouter.ai/api/v1/chat/completions`. Multi-model routing.
- **`claude-code`** — A Claude plan, reached only through the `claude` CLI on the payer's login. The hub's own client never sends a plan's token; the removed `claude-max` provider did, and naming it now fails load.

### `workforce-orchestrator` — Multi-Agent Coordination

Manages teams of agents working together:

| Module | Purpose |
|--------|---------|
| `orchestrator` | `Orchestrator` — top-level coordination |
| `team` | `Team` struct, member management, team patterns |
| `task` | Task decomposition, assignment, tracking |
| `router` | Message routing between team members |
| `escalation` | Agent → human escalation protocol |
| `model_router` | Model selection per agent/task based on complexity |
| `swarm` | Swarm-style coordination (emergent, no fixed lead) |
| `conflict` | Conflict detection and resolution between agents |

**Team patterns:**
- `hierarchical` — Lead agent decomposes tasks, delegates to members, synthesises results
- `pipeline` — Agents process sequentially (output of one → input of next)
- `parallel` — All agents work independently, results merged
- `swarm` — Emergent coordination without fixed roles

### `workforce-memory` — Vector Memory

Persistent memory with semantic search:

| Module | Purpose |
|--------|---------|
| `store` | `MemoryStore` trait — the storage abstraction |
| `types` | `Memory`, `MemoryFilter`, `MemorySearchResult`, `Visibility`, `MemoryLayer` |
| `in_memory` | In-memory implementation (for dev/testing) |
| `weaviate` | Weaviate vector DB implementation |
| `consolidation` | Memory consolidation — merge/compress old memories |
| `visibility` | Access control for shared memories |
| `layers` | Memory layer management (working, episodic, semantic, procedural) |
| `schema` | Weaviate schema management |

**Memory layers:**
- `working` — Current session context
- `episodic` — Event-specific memories ("what happened on date X")
- `semantic` — Facts and knowledge ("user prefers dark mode")
- `procedural` — How-to knowledge ("to deploy, run X then Y")

### `workforce-skills` — Pluggable Capabilities

Extend agents with installable skill packages:

| Module | Purpose |
|--------|---------|
| `definition` | `SkillDefinition` — metadata, tools, events, policies |
| `registry` | `SkillRegistry` — install, uninstall, lookup |
| `plugin` | Plugin runtime — load external skill packages |
| `events` | `EventSource` trait — inbound event adapters |
| `adapters/slack` | Slack event source adapter |
| `sdk` | Skill SDK for building custom skills |
| `distribution` | Skill distribution and versioning |

### `workforce-distributed` — Distributed Execution

Hub-and-spoke architecture using NATS leaf nodes:

| Module | Purpose |
|--------|---------|
| `node` | `NodeInfo`, `NodeStatus`, `TrustLevel` |
| `leaf` | `LeafNodeConfig` — generates NATS leaf node configuration |
| `affinity` | Task-to-node affinity rules (capability matching, locality) |
| `credentials` | Credential management for node authentication |
| `context` | Distributed execution context (spans, correlation) |
| `offline` | Offline queue for disconnected nodes |
| `swarm` | Distributed swarm coordination across nodes |

**Trust levels:** `Untrusted`, `Basic`, `Standard`, `Elevated`, `Full`. Controls what tools and resources a node can access.

### `workforce-hub` — The Server

The integration crate that wires everything together:

| Module | Purpose |
|--------|---------|
| `main` | Binary entry point (re-exports CLI) |
| `serve` | `AgentService` — core service tying identity, sessions, LLM, tools, policy |
| `api` | Admin API routes (dashboard, teams, tasks, cost, audit) |
| `gateway` | WebSocket + REST gateway for external clients |
| `slack` | Slack bot — Socket Mode connection, event handling, threading |
| `auth` | Bearer token authentication middleware |
| `jwt` | JWT token generation and validation |
| `state` | `HubState` — shared state for admin API |
| `scheduler` | Cron/interval/one-shot job scheduler |
| `heartbeat` | Periodic agent check-ins |
| `memory` | Memory manager — daily notes, MEMORY.md management |
| `subagent` | Sub-agent manager — spawn, track, and announce background tasks |
| `subagent_tool` | `subagent` tool — spawn/status/list/cancel/result actions |
| `compaction_tool` | `compaction` tool — view/configure/trigger context compaction |
| `logs_tool` | `system.logs` tool — agent access to hub runtime logs |
| `worker` | Background worker for async tasks |
| `team_runtime` | `TeamRuntime` — runs multi-agent teams from config |
| `webhooks` | Inbound webhook handler for external events |
| `fs_watcher` | File system watcher for workspace changes |
| `event_manager` | Central event routing and dispatch |

### `workforce-cli` — Command Line Interface

The `workforce` binary with subcommands:

| Command | Description |
|---------|-------------|
| `workforce chat` | Interactive chat session (no infrastructure needed) |
| `workforce run "<task>"` | One-shot task execution |
| `workforce serve` | Start the daemon (admin API + gateway + Slack + scheduler) |
| `workforce team "<task>"` | Run a task using a configured team |
| `workforce init` | Generate default config files |
| `workforce status` | Show system status |
| `workforce node config` | Generate NATS leaf node config |
| `workforce node start` | Start a local node |
| `workforce node info` | Show node information |
| `workforce skill list/search/install` | Manage skills |
| `workforce plugin list/install` | Manage plugins |

## Key Design Decisions

### Hub-and-Spoke with NATS
The hub runs the core services (admin API, gateway, Slack bot). Remote nodes connect via NATS leaf nodes. This allows agents to execute on machines with specific capabilities (GPU, local filesystem access) while being centrally coordinated.

### Policy-First Security
Every tool invocation goes through the policy engine before execution. Policies are YAML files with glob patterns, conditions, and hierarchical scopes. This means you can allow an agent to read files but deny shell access, or allow shell access but block specific dangerous commands.

### Identity from Files
Agent identity comes from markdown files in a workspace directory (SOUL.md, AGENTS.md, USER.md, MEMORY.md, IDENTITY.md, TOOLS.md). These are assembled into the system prompt at runtime. This makes identity portable, version-controllable, and human-readable.

### Separation of Concerns
The crate split is deliberate:
- **core** can be used by any Rust project that needs workforce types
- **policy-engine** can evaluate policies without any LLM or network dependency
- **agent-runtime** can run agents without the server infrastructure
- **hub** is the only crate that ties everything together

## Ports

| Port | Service | Config Key |
|------|---------|------------|
| 8090 | Admin API | `[hub] listen` |
| 8091 | Gateway (WS + REST) | `[gateway] listen` |
| 4222 | NATS (optional) | `[nats] url` |
| 8080 | Weaviate (optional) | `[weaviate] url` |
