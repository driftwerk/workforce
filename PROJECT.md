# Workforce — AI Agent Platform

Multi-crate Rust workspace. Agents connect to Slack, run LLM tool loops, and manage persistent state.

## Crates

- **hub** — Core orchestration: Slack dispatch, agent lifecycle, tool registration, knowledge service, scheduler. The brain that wires everything together.
- **agent-runtime** — LLM runner: tool execution loop, streaming progress, context compaction, adaptive model routing.
- **skills** — Adapters for external services. Each skill = TOML config + tools + surface.
  - `slack/` — Socket Mode transport, event handling, multi-workspace surfaces
  - `linear/`, `google/`, `github/` — Issue tracking, docs, PRs
- **common** — Shared types: ChannelSurface (messaging trait), Session, AgentIdentity, config loading, security scanning.
- **orchestrator** — Multi-agent coordination: team context, conversation orchestrator, TOML workflow engine (DW-359).
- **codebase-knowledge** — Code indexing via tree-sitter: entity graph (SQLite), semantic search (Weaviate), impact analysis.
- **memory** — Persistent memory layers: episodic, semantic, team, organisation. Vector-backed search.
- **core** — Domain types: AgentConfig, Task, Participant, Team.
- **cli** — Binary entry point. Starts hub, NATS leader election, scheduled jobs.
- **pairing** — Device pairing for mobile app auth.
- **distributed** — NATS-based leader election and inter-node communication.

## Data Flow

```
Slack event → SlackEventHandler → SlackAction enum → dispatch loop (slack_manager.rs)
  → AgentService.process_message (serve.rs) → Runner (runner.rs) LLM + tool loop
  → send_to_surfaces → SlackSurface.send_message → Slack API
```

## Key Types

- `AgentService` (hub/serve.rs) — Per-agent state: sessions, tool registry, surfaces, active runners. Central type.
- `Runner` (agent-runtime/runner.rs) — LLM call loop with tool dispatch and compaction. Runs in spawned task.
- `ChannelSurface` (common/channels.rs) — Trait for message destinations. Implemented by SlackSurface.
- `SlackAction` (skills/slack/event_handler.rs) — Enum of all Slack events. All variants carry workspace_id.
- `SlackManager` (hub/slack_manager.rs) — Manages workspace connections, config reload, dispatch loop.
- `KnowledgeService` (hub/knowledge/service.rs) — Wraps CodeGraph + VectorStore + QueryEngine.
- `AgentIdentity` (common/identity.rs) — Assembles system prompt from SOUL/AGENTS/MEMORY/PROJECT/etc.
- `WorkflowEngine` (orchestrator/workflow/engine.rs) — State machine for TOML-defined workflows.

## Patterns

- **Errors:** `eyre::Result` everywhere, `thiserror` for typed errors in library crates
- **Async:** tokio runtime, `mpsc` channels between components, `Arc<T>` for shared state
- **Config:** TOML files in `skills/` dirs, deserialized via serde. Vault tokens override config values.
- **Tools:** Implement `async_trait Tool`, return `ToolResult`. Registered per-agent in AgentService.
- **Surfaces:** One `SlackSurface` per workspace. Filter by `workspace_id` when sending — never broadcast to all.
- **Sessions:** Keyed by `platform:workspace:channel:thread`. Persistent via filesystem.
- **Compaction:** Emergency at 95%, normal at 70% of max_context_tokens. Keeps first 2 + last N messages.
- **Identity files:** SOUL.md, AGENTS.md, MEMORY.md, TOOLS.md, IDENTITY.md, PROJECT.md — all loaded into system prompt.

## Build & Test

```
cargo build                    # full workspace build
cargo test                     # all tests
cargo clippy -- -W clippy::all # lint
cargo fmt                      # format
```

## Gotchas

- `send_to_surfaces` loops ALL surfaces — always filter by workspace_id first
- Vault tokens override per-workspace config — use `slack.<workspace_id>.bot_token` keys
- `AgentIdentity` caches the system prompt — call `reload()` after file changes
- Session keys must be unique per conversation thread (duplicate keys = shared state)
- `BOOTSTRAP_FILES` loads PROJECT.md from the workspace root, not from the repo
- Knowledge engine root (`/app/repos/workforce/`) is separate from working copy (`repos/driftwerk/workforce/`)
- `SlackAction` variants all carry `workspace_id` — use it for routing, not just logging
- Never hold a `MutexGuard` across `.await` — use scoped locks or restructure
