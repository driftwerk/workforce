---
name: Codebase Knowledge
description: Use when the user asks about code structure, call graphs, symbol definitions, impact of a change, or "where is X defined / used". Backed by the workforce-local-mcp server's codegraph.*, project.*, impact.*, and context.fuse tools which query a locally indexed SQLite code graph built from tree-sitter ASTs across Rust, TypeScript, JavaScript, Python, Go, Ruby, and Kotlin.
---

# Codebase Knowledge

Query a locally indexed code graph. Useful for finding symbol definitions,
walking call graphs, multi-repo impact analysis, and getting a structural
overview of a file — faster and more precise than `grep` or reading files
blind.

## Prerequisites

The `workforce-local-mcp` MCP server must be configured. Verify with
`/mcp` — you should see the `codegraph.*`, `project.*`, `impact.*`, and
`context.fuse` tools.

## Picking the right tool

**If the user mentions a project by name** (e.g. "index the foo
project", "what's in the bar project?") use the `project.*`
tools with that project name — do NOT use `codegraph.index_repo`
with a single path, which would only cover one repo. Projects are
declared in TOML files and bundle multiple repos into a shared graph.

**If the user asks about the current repo / this codebase** (not
naming a project), use the single-repo `codegraph.*` tools.

**If unsure which project a request belongs to**, call `project.list`
first to see what's configured.

## Single-repo workflow

1. **Index the repo once** (fast — ~1s per 1000 files):
   `codegraph.index_repo` with no args indexes the server's workspace; pass
   `path="..."` for a specific repo, `incremental=true` for a delta pass.

2. **Look up a symbol** by exact name:
   `codegraph.find_symbol name="ToolRegistry"`. Each match includes an
   `entity_id` plus file, line range, kind, signature, and `repo_id`.

3. **Walk the call graph** using that `entity_id`:
   - `codegraph.callers entity_id="..." max_depth=3` — "what calls this?"
   - `codegraph.callees entity_id="..." max_depth=3` — "what does this call?"
   Results carry `distance` (BFS hop count) and the relationship kind.

4. **Get a file's structure** without reading it:
   `codegraph.file_graph file_path="src/foo.rs"` returns every entity
   (function, struct, trait, etc.) with signatures and line numbers.

## Multi-repo workflow (projects)

A project bundles a set of related repos into a shared code graph so
queries and impact analysis can span them. Project TOML files live in
`$WORKFORCE_LOCAL_PROJECTS_DIR` (defaults to `<data_dir>/projects`):

```toml
name = "myproject"
[[repos]]
name = "backend"
path = "/path/to/backend-repo"
[[repos]]
name = "web"
path = "/path/to/frontend-repo"
```

- `project.list` — show all configured projects and their repos.
- `project.index name="myproject"` — full-index every repo into the shared
  graph (sequential; partial failures captured in-line).
- `project.index name="myproject" incremental=true` — quick refresh
  that only reprocesses files changed since the last index. Use this
  when the user asks to "refresh" / "reindex" / "update the index"
  and there's an existing index. ~1s per repo vs ~30s for full.
  Falls back to full for any repo that has never been indexed.
- `impact.cross_repo entity_id="..." cross_boundary_only=true` — BFS from
  the entity, grouped by repo. Use `cross_boundary_only=true` for "what
  breaks in *other* repos if I change this?".

Note: today's cross-repo BFS only walks edges the indexer already emits
(intra-file calls, imports). HTTP-boundary edges (backend route ↔ frontend
fetch call) are a future addition; until then, cross-repo impact is useful
mainly for shared libraries / direct imports across repos.

## Design-shaped questions: `context.fuse`

For natural-language questions where you want full context in one
round-trip, use `context.fuse query="..."`:

- Runs the code-graph query and memory recall **concurrently**.
- Returns `code_matches`, `graph_context` (answer + suggestion),
  `memory_context`, `confidence`, `freshness`.
- Restrict sources with `include=["memory"]` etc; `scope="direct"` skips
  graph traversal for faster results.

This is usually the right first call for "how does X work?", "what
touches Y?", or "why is Z the way it is?" questions.

## When to reach for this vs. other tools

- Use `codegraph.*` instead of `grep` when you need precise symbol
  matching or callers/callees.
- Use `codegraph.file_graph` before reading a large file — you get the
  skeleton in one call.
- Use `context.fuse` when the question has both a code and a history
  dimension ("what did we decide about X?" mixed with "where is X?").
- Re-run `index_repo` / `project.index` after large refactors or branch
  switches; incremental indexing keeps the graph fresh.

## Notes

- Entity IDs are 16-char hashes; pass them verbatim between tools. Don't
  try to synthesize them.
- `find_symbol` matches exact names. For partial matches or full-text,
  fall back to `grep`.
- Call-graph edges are extracted heuristically from the AST; cross-file
  unresolved calls are omitted rather than guessed.
