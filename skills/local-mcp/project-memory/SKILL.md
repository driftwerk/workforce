---
name: Project Memory
description: Use when the user wants to remember a decision, fact, or lesson across Claude Code sessions — or when they reference something they told you earlier that you don't see in current context. Backed by the workforce-local-mcp server's memory.* tools which persist notes in a local SQLite FTS5 store; also integrated into context.fuse for combined code + memory queries.
---

# Project Memory

Durable note-taking for cross-session context. Use it like a personal
notebook: record non-obvious decisions, gotchas, and project facts;
recall them later by keyword.

## Prerequisites

The `workforce-local-mcp` MCP server must be configured. Verify with
`/mcp` — you should see `memory.remember`, `memory.recall`,
`memory.forget`, `memory.list`.

## When to remember

Save things that would be hard to rediscover:

- Design decisions with non-obvious rationale ("we picked X over Y
  because Z broke during the 2026-02 incident").
- Invariants and gotchas ("this function must be called under lock L").
- Project facts tied to a specific repo or initiative.

Do **not** save things that are already in the code or git log — those
can be re-derived. Keep entries short and self-contained; tag
aggressively so `recall` works later.

## When to recall

- At the start of a session, if the user references earlier work or the
  task sounds like it might have prior context.
- Before making non-trivial architectural decisions — prior notes may
  already constrain the choice.
- When the user asks "what did we decide about X?" or similar.

For mixed code + history questions ("what do we remember about how the
auth middleware works?"), use `context.fuse` — it runs the code-graph
query and memory recall concurrently and returns both in one payload.

## Tools

- `memory.remember content="..." tags=["auth","decision"] importance=0.7`
  — records a note. Returns `memory_id`.
- `memory.recall query="..." limit=10` — BM25 keyword search. Optionally
  filter by `tags` or `layer`.
- `memory.list tags=["..."]` — browse without searching.
- `memory.forget memory_id="..."` — soft-delete an outdated note.

## Layers

Pick a layer based on scope:

- `semantic` (default) — domain knowledge, architecture, facts.
- `episodic` — personal experience, "what happened last time".
- `working` — short-lived session scratch.
- `team` / `organisation` — shared (reserved for future; not yet
  synchronised in local mode).

## Notes

- Phase 1 search is BM25 keyword; choose evocative wording so later
  keyword queries will hit.
- Use `tags` liberally — they're the fastest filter axis.
- Memory IDs are UUIDs; pass them verbatim to `forget`.
