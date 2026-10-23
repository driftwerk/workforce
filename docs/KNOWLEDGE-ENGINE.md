# Codebase Knowledge Engine (CKE)

The Codebase Knowledge Engine gives agents deep understanding of codebases through AST parsing, relationship graphs, semantic search, and LLM-generated summaries. Agents can ask natural language questions about code, trace call graphs, analyse change impact, and get architecture overviews — all grounded in actual parsed source code.

## Architecture Overview

```
                         workforce.toml
                              |
                    [knowledge] config
                              |
                   +----- AgentRegistry -----+
                   |                         |
            KnowledgeService A         KnowledgeService B
            (root: /repo-a)            (root: /repo-b)
                   |                         |
        +----------+----------+    +----------+----------+
        |          |          |    |          |          |
    CodeGraph  QueryEngine  Vector  CodeGraph  QueryEngine  Vector
    (SQLite)   (graph+vec)  Store   (SQLite)   (graph+vec)  Store
                           (Weaviate)                      (Weaviate)
        |
   IndexingPipeline
        |
   +----+----+----+----+
   |    |    |    |    |
 Crawler Parser Extractor RelBuilder SummaryGen
         (tree-sitter)
```

### Components

| Component | Purpose |
|-----------|---------|
| **KnowledgeService** | Orchestrates graph, query engine, vector store, and indexing for one repo |
| **CodeGraph** | SQLite database storing entities, relationships, and summaries |
| **QueryEngine** | Combines graph traversal + vector semantic search to answer questions |
| **CodeVectorStore** | Weaviate integration for semantic similarity search |
| **IndexingPipeline** | Crawls files, parses ASTs, extracts symbols, builds relationships |
| **FileCrawler** | Discovers source files respecting .gitignore and exclude patterns |
| **SymbolExtractor** | Extracts functions, structs, classes, etc. from tree-sitter ASTs |
| **RelationshipBuilder** | Builds call graphs, import chains, and inheritance relationships |
| **SummaryGenerator** | Generates LLM summaries via any supported provider (Ollama, Anthropic, OpenRouter, Gemini, Grok, vLLM, Cerebras) |

### Data Flow

1. **Indexing**: Crawler finds files -> Parser creates ASTs -> Extractor pulls entities -> RelBuilder connects them -> Entities stored in SQLite + Weaviate
2. **Querying**: Question -> QueryPlanner extracts patterns -> Graph search + vector search -> Traverse relationships -> Verify freshness -> Synthesise answer
3. **Maintenance**: FS watcher detects changes -> Incremental re-index -> Update stale entities

---

## Configuration

### Global Config (`workforce.toml`)

```toml
[knowledge]
enabled = true                      # Enable/disable the knowledge engine
root = "/app/repos/workforce"       # Directory to index (default: agent workspace)
data_dir = "/app/knowledge"         # Where to store the SQLite DB (default: same as root)
index_on_startup = true             # Trigger full index when hub starts
parse_concurrency = 0               # Concurrent parsing workers (0 = auto: num_cpus * 2)
generate_summaries = true           # Generate LLM summaries during indexing
max_file_size = 1048576             # Max file size to index in bytes (default: 1MB)
maintenance_interval_secs = 14400   # Periodic maintenance interval (default: 4 hours)
exclude_patterns = ["**/fixtures/**"]  # Additional glob patterns to exclude

[knowledge.summariser]
provider = "ollama"                 # ollama | anthropic | openrouter | gemini | grok | vllm | cerebras
url = "http://ollama:11434"         # Base URL (alias: ollama_url for backward compat)
model = "qwen2.5-coder:7b"         # Model for summary generation
# api_key = "..."                   # Or set SUMMARISER_API_KEY env var

[weaviate]
enabled = true                      # Enable Weaviate vector store
url = "http://weaviate:8080"        # Weaviate HTTP URL
api_key = "your-api-key"            # Optional: API key for authentication
```

### Config Fields Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable/disable the knowledge engine |
| `root` | string | agent workspace | Directory to index |
| `data_dir` | string | same as `root` | Where the SQLite DB is stored (can be a separate volume) |
| `index_on_startup` | bool | `false` | Trigger full index on startup |
| `parse_concurrency` | int | `0` (auto) | Concurrent file parsing workers. 0 = num_cpus * 2 |
| `generate_summaries` | bool | `false` | Generate LLM summaries during indexing |
| `exclude_patterns` | string[] | `[]` | Additional glob patterns to exclude |
| `max_file_size` | int | `1048576` | Max file size in bytes |
| `maintenance_interval_secs` | int | `14400` | Periodic maintenance interval in seconds |
| `summariser.provider` | string | `"ollama"` | LLM provider: `ollama`, `anthropic`, `openrouter`, `gemini`, `grok`, `vllm`, `cerebras` |
| `summariser.url` | string | `"http://localhost:11434"` | Base URL for the LLM endpoint. Alias: `ollama_url` |
| `summariser.model` | string | `"qwen2.5-coder:7b"` | Model name for summaries |
| `summariser.api_key` | Option\<string\> | None | API key for authenticated endpoints, read when the global vault holds none at `knowledge/summariser/api_key` (preferred: it rotates without a restart) |

### Per-Agent Knowledge Config

Agents can override knowledge settings to index different repositories. Agents with the same effective `root` automatically share a single `KnowledgeService` instance.

```toml
# Two agents on client-A's repo — they share one KnowledgeService
[[agents]]
id = "dev-a"
workspace = "/app/data/agents/dev-a"
[agents.knowledge]
root = "/app/repos/client-a"

[[agents]]
id = "reviewer-a"
workspace = "/app/data/agents/reviewer-a"
[agents.knowledge]
root = "/app/repos/client-a"        # same root = shared service

# Separate agent on client-B — gets its own isolated KnowledgeService
[[agents]]
id = "dev-b"
workspace = "/app/data/agents/dev-b"
[agents.knowledge]
root = "/app/repos/client-b"
data_dir = "/app/knowledge/client-b"

# Agent with knowledge disabled
[[agents]]
id = "intern"
workspace = "/app/data/agents/intern"
[agents.knowledge]
enabled = false
```

Per-agent overridable fields:

| Field | Type | Description |
|-------|------|-------------|
| `root` | string | Override the directory to index |
| `data_dir` | string | Override where the SQLite DB is stored |
| `enabled` | bool | Disable knowledge for this agent |
| `exclude_patterns` | string[] | Replace the global exclude patterns |

All other fields (`index_on_startup`, `parse_concurrency`, `generate_summaries`, `max_file_size`, `maintenance_interval_secs`, `summariser`) inherit from the global `[knowledge]` section.

### How Sharing Works

The `AgentRegistry` groups agents by their effective knowledge root:

1. Computes each agent's effective knowledge config (per-agent overrides merged over global)
2. Canonicalises the root path (handles symlinks, trailing slashes)
3. Creates one `KnowledgeService` per unique root
4. Agents with the same canonical root share via `Arc` (zero overhead)
5. Starts one FS watcher and one startup indexing task per unique root

This means:
- Agents on the same project share indexing, FS watching, and the SQLite DB
- Agents on different projects get completely isolated knowledge bases (separate DBs, separate Weaviate queries)
- No cross-contamination between projects is possible

---

## Agent Tools

The CKE exposes 6 tools to agents. All tools are automatically registered when the knowledge engine is enabled.

### `knowledge.ask`

Ask a natural language question about the codebase.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `question` | string | yes | Natural language question. Use backticks around identifiers for best results. |
| `repo` | string | no | Repo or project to query, by name (e.g. `"workforce"`). Omit when only one repo is indexed. The other query tools (`knowledge.graph` / `explain` / `overview` / `impact`) take the same argument. |
| `scope` | string | no | Directory path within the repo to limit search (e.g., `"src/auth"`) |
| `depth` | `"quick"` \| `"thorough"` | no | `quick` = graph-only (fast). `thorough` = adds live file verification. Default: `quick` |

**How it works:**
1. QueryPlanner converts question into retrieval strategy
2. Graph search for pattern/name matches
3. Vector semantic search (if Weaviate enabled)
4. Traverse relationships (callers, callees, dependencies)
5. Freshness check against current source
6. Live file verification (thorough mode only)
7. Confidence scoring
8. Synthesise markdown answer with sources

**Example:**
```
knowledge.ask("How does the authentication middleware work?", repo: "workforce", scope: "src/auth")
```

### `knowledge.explain`

Get a detailed explanation of a specific code entity.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `entity` | string | yes | Entity name, qualified name, or ID from a previous query |
| `aspect` | `"full"` \| `"flow"` \| `"dependencies"` \| `"usage"` \| `"testing"` | no | Focus area. Default: `full` |

**Aspects:**
- `full` — everything: signature, docs, dependencies, callers, tests, summary
- `flow` — what it calls and what calls it
- `dependencies` — what it depends on
- `usage` — who uses this entity
- `testing` — test coverage for this entity

### `knowledge.graph`

Execute direct graph queries on the knowledge graph.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | enum | yes | Query type (see below) |
| `entity` | string | yes | Entity name or ID |
| `depth` | int | no | Max traversal depth. Default: 2 |

**Query types:**
| Query | Description |
|-------|-------------|
| `callers_of` | Who calls this entity |
| `callees_of` | What does this entity call |
| `imports_of` | Who imports this |
| `implementations_of` | What implements this trait/interface |
| `tests_for` | Tests for this entity |
| `depends_on` | What does this depend on |
| `affected_by` | All entities affected by a change to this |

### `knowledge.impact`

Analyse the impact of a proposed change before making it.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `target` | string | yes | Entity that will be changed |
| `change_description` | string | yes | What will change (e.g., "Changing the return type") |
| `scope` | `"direct"` \| `"full"` | no | `direct` = 1 hop. `full` = up to 3 hops. Default: `full` |

**Returns:** Affected entities classified by severity (breaking / requires_update / informational), grouped by file, with test impact and complexity estimate.

### `knowledge.overview`

Get an architecture overview of the codebase or a specific directory.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `scope` | string | no | `"full"` for entire repo, or a directory path. Default: `full` |

**Returns:** Entity counts by kind per directory, key types and functions, module summaries.

### `knowledge.index`

Trigger or check the status of codebase indexing.

**Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `action` | `"status"` \| `"trigger"` \| `"trigger_full"` | yes | What to do |

**Actions:**
- `status` — Check index state (entity/relationship counts, last commit, indexing in progress)
- `trigger` — Start indexing if needed (incremental if already indexed, full if not)
- `trigger_full` — Force complete re-index

---

## Indexing

### Supported Languages

Full AST parsing (tree-sitter):

| Language | Extensions |
|----------|-----------|
| Rust | `.rs` |
| TypeScript | `.ts`, `.tsx` |
| JavaScript | `.js`, `.jsx`, `.mjs`, `.cjs` |
| Python | `.py`, `.pyi` |
| Go | `.go` |
| Ruby | `.rb` |

Metadata-only (detected but not parsed for symbols): Markdown, TOML, YAML, JSON.

### Entity Types (16)

| Kind | Description |
|------|-------------|
| File | Source file |
| Module | Module / package / namespace |
| Function | Function definition |
| Method | Class/struct method |
| Struct | Struct definition |
| Class | Class definition |
| Interface | Interface / protocol |
| Trait | Trait definition (Rust) |
| Enum | Enum definition |
| EnumVariant | Enum case / variant |
| TypeAlias | Type alias |
| Constant | Const value |
| Variable | Variable declaration |
| Import | Import statement |
| Test | Test function (detected by naming/attributes) |
| Macro | Macro definition (Rust) |

### Relationship Types (11)

| Relationship | Description | Confidence |
|-------------|-------------|------------|
| Contains | Parent contains child in AST | 1.0 |
| DefinedIn | Entity defined in file | 1.0 |
| Implements | Struct/class implements trait/interface | 1.0 |
| Extends | Class extends another | 1.0 |
| TypeOf | Entity has type X | 0.8 |
| Imports | File imports from another | 1.0 |
| DependsOn | Package depends on package | 1.0 |
| Calls | Function calls another | 0.6-1.0 |
| CalledBy | Inverse of Calls (materialised) | 0.6-1.0 |
| TestsFor | Test function tests entity | 0.8 |
| DocumentedBy | Entity has doc comment | 1.0 |

### Default Exclude Patterns

The crawler always excludes:
- `**/vendor/**`
- `**/node_modules/**`
- `**/target/**`
- `**/.git/**`
- `**/generated/**`
- `**/*.min.js`
- `**/*.min.css`

Plus any patterns in `exclude_patterns` config.

The crawler also respects `.gitignore`, global gitignore, and `.git/info/exclude`.

### Incremental Indexing

After the initial full index, subsequent indexes are incremental:

1. Compares current HEAD against the last indexed commit using `git diff --name-status`
2. Categorises changes into added, modified, deleted, and renamed files
3. Only re-parses and re-indexes changed files
4. Falls back to full index if git is unavailable or no previous commit is stored

### Entity ID Generation

Entity IDs are deterministic BLAKE3 hashes of `"{repo_id}:{file_path}:{kind}:{name}"` (first 16 hex chars). This ensures:
- Same entity always gets the same ID across re-indexes
- No collisions between repos (scoped by `repo_id`)
- Stable references for graph queries and tool results

### Repo ID Derivation

Each `KnowledgeService` derives a `repo_id` from its root path:
```
{directory_name}-{blake3_hash_first_8_chars}
```
For example, root `/app/repos/workforce` produces `workforce-a1b2c3d4`. This scopes all entities, queries, and vectors to the correct repo.

---

## Vector Search (Weaviate)

When Weaviate is enabled, the CKE maintains two vector collections for semantic search.

### Collections

**`CodeEntity`** — vectorised code entities
- Searchable content: combined name + signature + doc_comment + summary
- Vectoriser: text2vec-transformers
- Filterable by: `repoId`, `kind`, `filePath`, `language`

**`CodeSummary`** — vectorised LLM summaries
- Searchable content: summary text
- Filterable by: `repoId`, `granularity`, `filePath`

### How Vector Search Integrates

In `knowledge.ask`, the query engine:
1. First performs graph-based pattern matching
2. Then performs semantic vector search against the question text
3. Deduplicates results (same entity found via both paths)
4. Combines scores for ranking

Vector search is non-blocking — if Weaviate is unavailable, queries fall back to graph-only search.

---

## Re-indexing Triggers

The knowledge index is kept fresh through multiple triggers:

| Trigger | Type | When |
|---------|------|------|
| Startup | Full index | Hub start (if `index_on_startup = true`) |
| FS watcher | Incremental | File changes detected (5s min interval, 2s debounce) |
| GitHub webhook | Full index | Push event received |
| Agent tool | Full or incremental | Agent calls `knowledge.index` |
| Maintenance pass | Incremental | Periodic background pass (default: every 4 hours) |

### Concurrency Protection

- Each `KnowledgeService` has an `indexing_lock` (`RwLock<bool>`) that prevents concurrent indexing
- SQLite uses `PRAGMA busy_timeout=5000` so concurrent readers/writers retry for up to 5 seconds instead of immediately failing
- In multi-agent setups, agents sharing a root share the same lock (no duplicate indexing)

---

## SQLite Schema

The knowledge graph is stored in a SQLite database at `{data_dir}/.workforce/knowledge/codegraph.db`.

### Tables

| Table | Purpose | Primary Key |
|-------|---------|-------------|
| `repositories` | Tracked repository metadata | `id` |
| `entities` | Code entities (functions, structs, etc.) | `id` |
| `relationships` | Edges between entities (calls, imports, etc.) | `id` |
| `summaries` | LLM-generated summaries | `(entity_id, granularity)` |
| `index_state` | Per-repo indexing state | `repo_id` |
| `file_index` | Per-file content hashes for change detection | `(repo_id, file_path)` |

All entity and relationship tables include `repo_id` for multi-repo isolation. Every query filters by `repo_id`.

### Pragmas

```sql
PRAGMA journal_mode=WAL;       -- Write-Ahead Logging for concurrent reads
PRAGMA foreign_keys=ON;        -- Enforce referential integrity
PRAGMA busy_timeout=5000;      -- Retry on lock contention for 5 seconds
```

---

## Summary Generation

When `generate_summaries = true`, the indexing pipeline generates LLM summaries for code entities.

### Granularity Levels

| Level | Description |
|-------|-------------|
| Function | 1-2 sentence summary of what the function does |
| File | 2-3 sentences on file responsibility, key types/functions |
| Module | 3-5 sentences on module role and key abstractions |
| Architecture | 5+ sentences on system design and data flow |

### Summariser Config

The summariser supports all agent-runtime providers. It uses the same `LlmProvider` infrastructure, so any provider that works for agent LLM calls also works for summaries.

```toml
[knowledge.summariser]
provider = "ollama"                  # ollama | anthropic | openrouter | gemini | grok | vllm | cerebras
url = "http://ollama:11434"          # Base URL for the LLM endpoint
model = "qwen2.5-coder:7b"          # Model name
# api_key = "..."                    # Or set SUMMARISER_API_KEY env var
```

**Environment variable overrides** (take precedence over TOML):

| Env Var | Overrides | Description |
|---------|-----------|-------------|
| `SUMMARISER_URL` | `summariser.url` | Base URL for the LLM endpoint |
| `SUMMARISER_API_KEY` | `summariser.api_key` | API key / Bearer token for authenticated endpoints; the last resort behind the global vault's `knowledge/summariser/api_key` |
| `SUMMARISER_PROVIDER` | `summariser.provider` | LLM provider name |
| `WORKFORCE_KNOWLEDGE_SUMMARISER_MODEL` | `summariser.model` | Model name |
| `WORKFORCE_KNOWLEDGE_OLLAMA_URL` | `summariser.url` | Legacy alias (lower precedence than `SUMMARISER_URL`) |

**Provider examples:**

```toml
# Ollama (local, no auth)
[knowledge.summariser]
model = "qwen2.5-coder:7b"

# Ollama on AWS behind auth
# Set SUMMARISER_API_KEY=<bearer-token>
[knowledge.summariser]
provider = "ollama"
url = "https://my-ollama.example.com"
model = "qwen2.5-coder:32b"

# Anthropic
# Set SUMMARISER_API_KEY=sk-ant-api...
[knowledge.summariser]
provider = "anthropic"
model = "claude-haiku-4-5-20251001"

# OpenRouter
# Set SUMMARISER_API_KEY=sk-or-...
[knowledge.summariser]
provider = "openrouter"
model = "qwen/qwen-2.5-coder-32b-instruct"

# vLLM on AWS
# Set SUMMARISER_URL=https://my-vllm.example.com SUMMARISER_API_KEY=<token>
[knowledge.summariser]
provider = "vllm"
model = "Qwen/Qwen2.5-Coder-32B-Instruct"

# Gemini
# Set SUMMARISER_API_KEY=<google-api-key>
[knowledge.summariser]
provider = "gemini"
model = "gemini-2.0-flash"
```

**Backward compatibility:** Existing configs using `ollama_url` continue to work — it is a serde alias for the `url` field. The legacy `WORKFORCE_KNOWLEDGE_OLLAMA_URL` env var also still works but is overridden by `SUMMARISER_URL` if both are set.

Summaries are stored in the `summaries` table with the source hash of the code they were generated from. When source code changes, summaries are marked as stale (`is_stale = 1`) and regenerated on the next maintenance run.

---

## Confidence Scoring

Query results include a confidence score (0.0 - 1.0) based on:

| Factor | Weight | Description |
|--------|--------|-------------|
| Source freshness | High | 1.0 if index is fresh, 0.5 if stale |
| Vector similarity | Medium | Best semantic search score (0.7 default for graph-only) |
| Graph completeness | Medium | 0.9 if relationships found, 0.6 if none |
| Live verified | High | True if source hash matches current file (thorough mode) |
| Corroboration | Low | 0.8 if multiple entities found, 0.5 if single match |

Results with confidence below 0.65 are flagged as unreliable.

---

## Maintenance

A deterministic background pass (`MaintenanceRunner::run_pass`, spawned from
`cli::main` on `[knowledge].maintenance_interval_secs`, default 4 hours) keeps
every indexed repo fresh. For each target it:

1. **Pulls** the repo into its canonical checkout
2. **Re-indexes** via `trigger_for`, which picks the right action per repo: an
   incremental pass on a commit delta, or summaries-only
   (`NextAction::ResumeSummaries`) to drain the summary backlog when the code
   graph is already at HEAD
3. **Re-vectorises** changed entities

The pass runs entirely in code — no LLM agent turn is in the indexing loop —
and produces a `PassReport` (surfaced by `knowledge.status`).

### One index per repo

The hub keeps exactly one index per upstream repo. The maintenance pass owns
all hub indexing: it pulls each repo (standalone clones plus every project repo,
deduplicated by `owner/repo`) into a canonical checkout under
`{data_dir}/knowledge/checkouts/` and indexes that. The default agent service
does **not** also index its `[knowledge] root`, so no second, never-pulled copy
is created; orphan rows left by earlier layouts are dropped on startup
(`MaintenanceRunner::prune_orphans`).

Knowledge queries (and `knowledge.index`) select a repo by name (the `repo`
argument) and resolve to that repo's canonical index through the `RepoResolver`;
with a single indexed repo the argument may be omitted. In single-agent setups
(no canonical resolver) `repo` is instead a workspace-relative path, preserving
the legacy behaviour; the old `path` key is still accepted as an alias. A GitHub
push triggers a canonical maintenance pass rather than re-indexing the service
root.
