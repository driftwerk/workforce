# Configuration

Workforce uses a three-layer configuration system that separates global defaults (CI-managed), per-agent runtime config (agent-managed), and secrets (encrypted vault).

## Config Architecture

### Three Layers

| Layer | File | Managed By | Survives CI Deploy? |
|-------|------|-----------|-------------------|
| 1. Global defaults | `workforce.toml` | Infra CI / humans | No (overwritten) |
| 2. Per-agent config | `{workspace}/config.toml` | Agent tools / humans | Yes (persistent volume) |
| 3. Secrets | `{workspace}/vault.enc` | Agent tools / CLI | Yes (persistent volume) |

**Layer 1: `workforce.toml`** — CI-managed global defaults. Contains hub settings, NATS, Weaviate, memory, gateway, budget, knowledge, session lifecycle, session policies, heartbeat, groups, teams, and agent definitions. Overwritten on every infra deploy.

**Layer 2: `{workspace}/config.toml`** — Per-agent runtime configuration. Contains LLM settings (model, max_tokens, failover), compaction config, memory config, and routing config. Written by agent tools (`system.set_model`, etc.) or human edit. Plain TOML — easy to inspect and diff. Survives CI deploys because agent workspaces live on persistent volumes.

**Layer 3: Vault** — Secrets only. API keys, OAuth tokens, credentials. Encrypted at rest with AES-256-GCM. See [VAULT.md](VAULT.md) for details.

### Config Merge Priority (highest wins)

```
1. workforce.toml [llm]              → global defaults (lowest priority)
2. workforce.toml [[agents]] section → per-agent static defaults
3. {workspace}/config.toml           → per-agent runtime config (agent-managed)
4. Vault secrets                     → API keys/tokens only (not config values)
5. Per-session model override        → transient, lives in session object (highest priority)
```

For any LLM setting (model, max_tokens, failover, etc.), the effective value is resolved by checking each layer in reverse order — the first non-empty value wins.

### What Goes Where

| Setting | Where | Why |
|---------|-------|-----|
| Hub listen address | workforce.toml | Global infrastructure |
| NATS/Weaviate URLs | workforce.toml | Global infrastructure |
| Budget limits | workforce.toml | Global policy |
| Session lifecycle | workforce.toml | Global policy |
| Default LLM model | workforce.toml `[llm]` | Global default |
| Agent's active model | `{workspace}/config.toml` | Per-agent override |
| Agent's failover chain | `{workspace}/config.toml` | Per-agent override |
| Agent's compaction settings | `{workspace}/config.toml` | Per-agent override |
| Agent's passive memory settings | `{workspace}/config.toml` | Per-agent override |
| API keys / OAuth tokens | Vault | Secrets |
| Session model override | Session object (in-memory) | Transient per-session |

### Hot-reload

Editing `{workspace}/config.toml` (via the UI or API) applies on the **next turn** without a hub restart. The agent re-reads the file and swaps its runtime config: the `[memory]`, `[compaction]`, `[spill]`, and `[advisor]` sections, plus the LLM provider/model. Background-task model tiers re-resolve against the live provider.

A few fields are baked into a resource built at startup (a store connection or a background task) and **still require a restart**; an edit to them is applied where possible and the deferred field names are returned in the save API response as `restart_required`:

- `memory.backend`, `memory.db_path` — the store connection
- `memory.recall_min_certainty` — applied to the store at construction
- `memory.consolidation`, `memory.consolidation_interval_secs` — the consolidation background task

`[llm]` sub-config beyond provider/model (e.g. `[llm.image]`, failover health), `[knowledge]`, `[temporal]`, vault paths, and the workspace path are not hot-reloaded.

---

## Environment Variable Interpolation

Any string value in `workforce.toml` may reference a process environment variable using `${VAR}` or `${VAR:-default}` syntax. Expansion happens once at config load, before deserialisation, so any field — typed as `String`, `Option<String>`, list, or nested table — is eligible. This is the recommended way to inject deployment-specific values (database hosts, OIDC client ids, gateway tokens) without templating the file.

```toml
[postgres]
enabled  = true
host     = "${POSTGRES_HOST}"
port     = 5432
database = "${POSTGRES_DB:-workforce}"
user     = "${POSTGRES_USER:-workforce}"
password = "${POSTGRES_PASSWORD}"

[gateway]
auth_token = "${WORKFORCE_GATEWAY_TOKEN}"

[gateway.oidc]
enabled       = true
issuer_url    = "https://accounts.google.com"
client_id     = "${OIDC_CLIENT_ID}"
client_secret = "${OIDC_CLIENT_SECRET}"
```

Rules:

* `${VAR}` — substituted with the value of env var `VAR`. If `VAR` is unset, the placeholder is replaced with an empty string.
* `${VAR:-default}` — substituted with `VAR` if set, otherwise the literal `default`.
* Multiple placeholders per string are allowed.
* Non-string scalars (integers, booleans, floats, datetimes) are not touched.
* Vault references (`${vault:dotted.path}`) are resolved per-field at access time, not by this generic pass — see `[vault]` and [VAULT.md](VAULT.md).
* Per-skill `config.toml` files (in `{data_dir}/skills/<skill>/config.toml`) use the same expansion mechanism.

The legacy per-field env overrides (`WORKFORCE_NATS_URL`, `WEAVIATE_API_KEY`, `ANTHROPIC_API_KEY`, etc.) still apply on top of this expansion — they're useful when you can't edit the TOML.

---

## Health & Readiness Endpoints

The Admin API server (`[hub] listen` — port 8090 by default) exposes a no-auth `/api/health` endpoint that always returns `200 OK`. Use it for:

* **Container health checks** — `HEALTHCHECK CMD curl -f http://127.0.0.1:8090/api/health` in a Dockerfile.
* **Kubernetes liveness / readiness probes** — `httpGet: { path: /api/health, port: 8090 }`.
* **Load balancer health checks** — point your LB target group at port 8090 path `/api/health`.

`/api/health` checks only that the HTTP server is accepting requests; it does **not** verify Postgres, NATS, or Weaviate connectivity. Workforce reaches degraded operation gracefully (postgres falls back to SQLite, NATS / Weaviate are optional), so the readiness signal is "the process can answer HTTP" rather than "every backend is healthy."

The separate `/api/heartbeat` family of routes under `[gateway]` (port 8091) is unrelated — those endpoints power the user-facing heartbeat-checks feature for monitoring external services and require gateway authentication. **Do not** point health probes at `/api/heartbeat`.

---

## workforce.toml Reference

The main configuration file. Loaded on hub startup via `workforce serve --config <path>`.

### `[hub]` — Admin API Server

```toml
[hub]
listen = "0.0.0.0:8090"    # HTTP listen address
data_dir = "./data"          # Data directory for persistence
log_level = "info"           # trace | debug | info | warn | error
# policy_path = "policies/default.yaml"  # Policy file (auto-detected if unset)
# drain_timeout_secs = 60   # Graceful shutdown timeout
```

| Field | Type | Default | Env Override |
|-------|------|---------|--------------|
| `listen` | String | `"0.0.0.0:8090"` | `WORKFORCE_LISTEN` |
| `data_dir` | String | `"./data"` | — |
| `log_level` | String | `"info"` | `WORKFORCE_LOG_LEVEL` |
| `policy_path` | Option\<String\> | None (auto-detect) | — |
| `drain_timeout_secs` | Option\<u64\> | 60 | — |

### `[nats]` — NATS Connection

```toml
[nats]
url = "nats://localhost:4222"
# credentials = "/path/to/nkey"
```

| Field | Type | Default | Env Override |
|-------|------|---------|--------------|
| `url` | String | `"nats://localhost:4222"` | `WORKFORCE_NATS_URL` |
| `credentials` | Option\<String\> | None | — |
| `stream_replicas` | usize | `1` | — |

`url` accepts a comma-separated list for a clustered NATS deployment
(`nats://a:4222,nats://b:4222,nats://c:4222`); the client uses every entry as a
reconnect seed. `stream_replicas` sets the JetStream replica count for the
workforce streams and the leader-election bucket — leave `1` on a single NATS
server (which rejects higher values), set `3` on a 3-node cluster.

NATS is optional — only needed for distributed multi-agent mode.

### `[weaviate]` — Vector Memory

```toml
[weaviate]
url = "http://localhost:8080"
enabled = true
# api_key = "..."  # Or set WEAVIATE_API_KEY env var
```

| Field | Type | Default | Env Override |
|-------|------|---------|--------------|
| `url` | String | `"http://localhost:8080"` | `WORKFORCE_WEAVIATE_URL` |
| `enabled` | bool | `false` | — |
| `api_key` | Option\<String\> | None | `WEAVIATE_API_KEY` |

### `[memory]` — Memory Store

```toml
[memory]
backend = "weaviate"  # sqlite | weaviate | in_memory
# db_path = "data/memory.db"
# embedding_provider = "none"
# embedding_model = "text-embedding-3-small"
# embedding_api_url = "https://api.openai.com/v1/embeddings"

# Passive memory (both default to off)
# passive = true                      # Post-turn memory extraction
# passive_model = "cheap"             # Model for extraction LLM call
# passive_min_turn_tokens = 200       # Skip trivial turns
# consolidation = true                # Periodic memory consolidation
# consolidation_interval_secs = 14400 # Every 4 hours
# consolidation_lookback_secs = 43200 # 12-hour session scan window

# Cross-agent recall isolation
# agent_recall_scope = "shared"       # shared (default) | isolated
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `backend` | String | `"sqlite"` | Vector store backend |
| `db_path` | String | `"data/memory.db"` | SQLite database path |
| `embedding_provider` | String | `"none"` | `"openai"` for vector search, `"none"` for keyword-only |
| `embedding_model` | String | `"text-embedding-3-small"` | Embedding model name |
| `embedding_api_url` | String | `"https://api.openai.com/v1/embeddings"` | Embedding endpoint |
| `embedding_api_key` | Option\<String\> | None | API key (prefer env var) |
| `passive` | bool | `false` | Enable post-turn memory extraction |
| `passive_model` | Option\<String\> | None | Model for extraction (falls back to compaction_model) |
| `passive_min_turn_tokens` | u32 | `200` | Minimum turn tokens to trigger extraction |
| `passive_prompt` | Option\<String\> | None | Custom extraction system prompt |
| `consolidation` | bool | `false` | Enable periodic memory consolidation |
| `consolidation_interval_secs` | u64 | `14400` | Consolidation interval in seconds |
| `consolidation_lookback_secs` | u64 | `43200` | Session scan lookback window in seconds |
| `consolidation_model` | Option\<String\> | None | Model for the consolidation merge phase (Model engine only) |
| `consolidation_source` | String | `"model"` | Consolidation engine: `"model"` (an agent turn) or `"claude_code"` (a toolless CLI subprocess — flat-rate under a subscription; the hub prefetches episodic memories, embeds them in the prompt, and applies the returned JSON op plan in-process, with `forget` restricted to prefetched ids). Passive extraction always stays on `passive_model` — a subprocess per inbound message is not viable |
| `consolidation_cc_failover` | bool | `false` | When Claude Code is unavailable, fall back to the Model engine instead of skipping the run (skips are always logged) |
| `agent_recall_scope` | String | `"shared"` | Which agents' memories auto-recall injects: `"shared"` (own + team/global) or `"isolated"` (own only). Stops cross-agent context bleed in multi-agent deployments |

See [MEMORY.md — Passive Memory](MEMORY.md#passive-memory) for details on how these features work.

### `[postgres]` — Shared Postgres Connection (optional)

Workforce can persist telemetry, identity, pairing and codegraph data
in Postgres instead of per-host SQLite files. Add a `[postgres]` block
to enable it. When `[postgres] enabled = true` and a per-store `backend`
is **not explicitly set** (see `[telemetry]`, `[identity]`,
`[pairing_store]`, `[codegraph]` below), it defaults to `"postgres"` —
one switch flips every supported store. To keep a specific store on
SQLite while postgres is enabled, set its `backend = "sqlite"`
explicitly. When `enabled = false` (or the block is omitted), every
store falls back to its file/SQLite backend regardless of the per-store
setting.

```toml
[postgres]
enabled = true

# Either supply a full URL...
url = "postgres://workforce:secret@db.internal:5432/workforce"

# ...or the individual fields:
# host = "db.internal"
# port = 5432
# database = "workforce"
# user = "workforce"
# password = "${vault:postgres.password}"  # supports vault expansion

pool_size = 16
application_name = "workforce-hub-prod"
```

| Field | Type | Default |
|-------|------|---------|
| `enabled` | bool | `false` |
| `url` | Option\<String\> | None |
| `host` / `port` / `database` / `user` / `password` | Option | None |
| `pool_size` | u32 | `16` |
| `application_name` | String | `"workforce"` |

> See [POSTGRES.md](POSTGRES.md) for the full setup, migration,
> tenancy, and operations story.

### `[telemetry]` — Cost / Audit / Workflow Store

```toml
[telemetry]
backend = "sqlite"  # sqlite | postgres
activity_retention_days = 0
cost_retention_days = 0
```

| Field | Type | Default |
|-------|------|---------|
| `backend` | String | `"sqlite"` (or `"postgres"` when `[postgres] enabled = true` and not explicitly set) |
| `activity_retention_days` | u32 | `0` (forever) — days a task's telemetry (with its rounds and tool calls) is kept after it completed. A shorter window shortens the usage tools (90 days) and cost analytics (three months) |
| `cost_retention_days` | u32 | `0` (forever) — days a cost record is kept. A scheduled job's costs are never pruned (its budget is lifetime); a shorter window shortens the monthly cost view (36 months), per-person billing history and stats ranges |

The leader prunes every 6 hours, starting three minutes after boot, in
batches of 5,000 rows and at most 100 batches a pass; a larger backlog goes
over the following passes. The same task prunes resolved or expired routed
approvals (which hold the tool call's full arguments) `[approvals]
retention_days` (default `30`, `0` keeps them) after their expiry; a pending
request is never pruned, and `approval.status` on a pruned request answers
"unknown request".

`"sqlite"` writes to `{hub.data_dir}/telemetry.db`. `"postgres"`
requires `[postgres] enabled = true` and routes every cost record,
task telemetry, audit log, workflow instance, model health event and
security event through the Postgres `telemetry` schema. Falls back to
SQLite if the pool fails to come up — Postgres never fails startup.

### `[identity]` — Users + Teams Store

```toml
[identity]
backend = "sqlite"  # sqlite | postgres
```

| Field | Type | Default |
|-------|------|---------|
| `backend` | String | `"sqlite"` (or `"postgres"` when `[postgres] enabled = true` and not explicitly set) |

`"sqlite"` writes to `{hub.data_dir}/identity.db`. `"postgres"` uses
the Postgres `identity` schema. Same fallback behaviour as
`[telemetry]`.

### `[pairing_store]` — Pairing Requests + Allowlists Store

```toml
[pairing_store]
backend = "sqlite"  # sqlite | postgres
```

| Field | Type | Default |
|-------|------|---------|
| `backend` | String | `"sqlite"` (or `"postgres"` when `[postgres] enabled = true` and not explicitly set) |

> **Note:** This is **distinct from `[pairing]`**, which holds runtime
> policy (`code_ttl_secs`, allowlist mode, etc.). `[pairing_store]`
> only chooses *where* the pairing data is persisted; it does not
> change pairing behaviour.

`"sqlite"` writes to `{hub.data_dir}/pairing.db`. `"postgres"` uses
the Postgres `pairing` schema (5 tables: channel pairing requests,
channel allowlist, device pairing requests, approved devices, node
pairing requests).

### `[codegraph]` — Code Knowledge Structural Store

```toml
[codegraph]
backend = "sqlite"  # sqlite | postgres
```

| Field | Type | Default |
|-------|------|---------|
| `backend` | String | `"sqlite"` (or `"postgres"` when `[postgres] enabled = true` and not explicitly set) |

`"sqlite"` writes to `{hub.data_dir}/.workforce/knowledge/codegraph.db`.
`"postgres"` uses the Postgres `codegraph` schema. Vector embeddings
(the `CodeEntity` and `CodeSummary` Weaviate collections) are
unaffected — only structural metadata (entities, relationships,
summaries, file index, index state) moves. Weaviate is still queried
in parallel for semantic search.

### `[llm]` — LLM Provider (Global Defaults)

These are the global defaults. Per-agent overrides live in `{workspace}/config.toml`.

```toml
[llm]
provider = "anthropic"                      # anthropic | openrouter | grok | gemini | ollama | chatgpt | claude-code | codex
default_model = "claude-sonnet-4-20250514"  # Default model for all agents
max_tokens = 8192                           # Max tokens per response
max_rounds = 50                             # Max tool execution rounds per turn
failover = ["claude-haiku-4-5-20251001", "xai:grok-3"]
accounts = ["ANTHROPIC_API_KEY_2"]          # Additional key env vars for pool

[llm.retry]
max_retries = 3              # Retries before failover
initial_backoff_ms = 1000    # Initial retry delay
backoff_multiplier = 2.0     # Exponential backoff factor
max_backoff_ms = 30000       # Maximum retry delay

[llm.health]
failure_threshold = 3        # Failures in window to mark degraded
window_secs = 300            # Failure counting window
recovery_secs = 120          # Seconds before retrying degraded model

[llm.aliases]
fast = "claude-haiku-4-5-20251001"
smart = "claude-sonnet-4-20250514"
```

| Field | Type | Default | Env Override |
|-------|------|---------|--------------|
| `provider` | String | `"openrouter"` | `WORKFORCE_LLM_PROVIDER` |
| `api_key` | Option\<String\> | None | See API key resolution |
| `default_model` | String | `"anthropic/claude-sonnet-4"` | `WORKFORCE_LLM_MODEL` |
| `max_tokens` | u32 | `8192` | — |
| `max_rounds` | u32 | `50` | — |
| `failover` | Vec\<String\> | `[]` | — |
| `accounts` | Vec\<String\> | `[]` | — |
| `aliases` | HashMap | `{}` | — |

**API key resolution (in order):**

| Provider | Env Var | Fallback |
|----------|---------|----------|
| `anthropic` | `ANTHROPIC_API_KEY` | `WORKFORCE_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` | `WORKFORCE_API_KEY` |

**Providers:**

- **`anthropic`** — Direct Anthropic Messages API. Pay per token. Set `ANTHROPIC_API_KEY`.
- **`openrouter`** — OpenRouter proxy. Supports multiple providers. Set `OPENROUTER_API_KEY`.
- **`claude-code`** — A Claude plan, run only through the `claude` CLI on the payer's login (`claude_code.setup_token` or My Settings). The removed `claude-max` provider, and a Claude subscription token (`sk-ant-oat…`) given as an API key, fail load.

**Multi-account pool:** The `accounts` array lists env var names containing additional API keys/tokens for the primary provider. When one token hits a rate limit, the pool rotates to the next. Example: `accounts = ["ANTHROPIC_API_KEY_2", "ANTHROPIC_API_KEY_3"]`. These are **global** keys — all agents get them. For per-agent tokens, see [Token Pool Architecture](#token-pool-architecture) below.

**Failover chain:** Models tried in order when the primary fails. Can include cross-provider models with `provider:model` syntax (e.g., `"xai:grok-3"`). See [FAILOVER.md](FAILOVER.md) for details.

**Subscription logins (`[llm.logins]`):** which stored login pays for a `chatgpt`, `claude-code` or `codex` call. Optional: by default every tier is tried and a shared login serves anyone, so where a login is stored decides whom it serves. Set globally under `[llm]` or per agent in its workspace `config.toml` (`[llm.logins]`, hot-reloaded) or under `[agents.llm.logins]`. A block replaces the inherited one whole: one with only `shared_serves` resets `tiers` to the default.

```toml
[llm.logins]
tiers = ["agent_user", "global_user", "agent_shared", "system_shared"]   # default
shared_serves = "anyone"                                                # default; owner | agent_users | anyone
```

| Field | Type | Default |
|-------|------|---------|
| `tiers` | Vec\<String\> | `["agent_user", "global_user", "agent_shared", "system_shared"]` |
| `shared_serves` | String | `"anyone"` |

- **`tiers`** — tried in order. `agent_user` / `global_user`: the payer's own login in the agent / global vault at `users/{uid}/<key>`. `agent_shared` / `system_shared`: the bare key in the agent / global vault (`system_shared` is the install's own login). Keys: `claude_code/oauth_token`, `codex/auth_json` (serves `chatgpt` and `codex`). An empty list or a repeated tier is refused at load.
- **`shared_serves`** — whom the two shared tiers may pay for: `owner` (the agent's background work and its owner or owning team's members), `agent_users` (also every identified person), `anyone` (also callers with no identity). Outside callers (`ext:*`, `games`) are never served by a shared login.
- The hub's own non-agent work (knowledge summariser, artifact tagger) ignores `[llm.logins]` and tries the payer's `global_user` login, then `system_shared`.

See `end-user-docs/concepts/subscriptions.mdx`.

### `[subscription]` — Subscription Install Settings

Install-wide, in `workforce.toml`; read at boot.

```toml
[subscription]
chatgpt_direct = false            # allow the chatgpt provider
operator = "<identity uuid>"      # whose plan pays for the hub's own work
max_concurrent_cli_turns = 4      # claude-code / codex agent turns generating at once; 0 = unbounded
max_concurrent_completions = 4    # one-shot claude-code / codex completions at once; 0 = unbounded
```

| Field | Type | Default |
|-------|------|---------|
| `chatgpt_direct` | bool | `false` |
| `operator` | Option\<Uuid\> | None |
| `max_concurrent_cli_turns` | u32 | `4` |
| `max_concurrent_completions` | u32 | `4` |

- **`chatgpt_direct`** — kill switch for the direct `chatgpt` provider. Off, a config with a `chatgpt` route still loads (with a warning) and every call on it is refused with a reason; the ChatGPT login keeps serving `codex`.
- **`operator`** — the person whose plan pays for the hub's own work (knowledge summariser, tagger) and for background work of an agent with no single owner (team-owned or unowned). Background work still runs as its own identity; budgets are not charged to the operator. An `operator` naming no identity is logged as a warning at boot.
- **`max_concurrent_cli_turns`** — agent turns on a CLI route generating at once; a turn waiting on a tool call or an approval holds no slot.
- **`max_concurrent_completions`** — one-shot CLI calls (tool-less internal calls on `claude-code` or `codex`) at once. Coding rounds (`/cc`, `/cx`, `coding.session`) use their own pools.

### `[compaction]` — Context Window Management

```toml
[compaction]
max_context_tokens = 200000          # Max context window size in tokens
compaction_threshold = 0.8           # Fraction of max before compaction triggers (80%)
urgent_compaction_threshold = 0.95   # Fraction for synchronous compaction (95%)
keep_recent_messages = 20            # Messages always kept during compaction
enable_memory_flush = true           # Flush important context to memory files
prune_tool_outputs = true            # Truncate large tool outputs first
prune_tool_output_max_chars = 2000   # Max chars per tool output
max_compaction_passes = 3            # Safety valve for compaction iterations
min_messages_to_compact = 5          # Minimum messages to justify compaction
compaction_model = "claude-haiku-4-5-20251001"  # Model for compaction LLM calls
```

| Field | Type | Default |
|-------|------|---------|
| `max_context_tokens` | u32 | `200000` |
| `compaction_threshold` | f32 | `0.8` |
| `urgent_compaction_threshold` | f32 | `0.95` |
| `keep_recent_messages` | usize | `20` |
| `enable_memory_flush` | bool | `true` |
| `prune_tool_outputs` | bool | `true` |
| `prune_tool_output_max_chars` | usize | `2000` |
| `max_compaction_passes` | u32 | `3` |
| `min_messages_to_compact` | usize | `5` |
| `compaction_model` | Option\<String\> | `"claude-haiku-4-5-20251001"` |

Compaction runs when estimated token count exceeds `max_context_tokens * compaction_threshold`. Between the normal threshold and `urgent_compaction_threshold`, compaction is deferred to background. Above the urgent threshold, it blocks synchronously.

See [COMPACTION.md](COMPACTION.md) for the full compaction algorithm.

### `[spill]` — Tool-Output Auto-Spill (Data Plane)

```toml
[spill]
enabled = true            # Spill large tabular tool outputs to the artifact store
threshold_bytes = 32768   # Outputs at/above this size are candidates (32 KB)
skip_tools = ["memory", "knowledge", "sessions", "session", "notes", "context", "jira", "linear", "github"]
```

| Field | Type | Default |
|-------|------|---------|
| `enabled` | bool | `true` |
| `threshold_bytes` | usize | `32768` |
| `skip_tools` | Vec\<String\> | content-tool prefixes above |

Any tool output (built-in, adapter, or MCP) at/above the threshold that parses as a row
table is staged as a `tabular_artifact` and replaced with a digest envelope before it
reaches context, persistence, or traces — the model transforms it via `data.query` /
exports via `export_xlsx` instead of reading rows. `skip_tools` prefixes (matched against
`name`, `name.…`, `name_…`) exempt content tools whose output the model must read
verbatim. Hot-reloads; watch `workforce.tool_output.spilled{tool}` after changes.


### `[budget]` — Mandatory LLM Spend Governance

Enforced at a single admission point before every LLM turn (chat, scheduler
jobs, heartbeats, monitors, workflows, curation, consolidation). Lives in the
same infra-owned class as `[permissions]`/`[approvals]`: the whole table is
outside the overlay allowlist, so no runtime writer, tool, or API can change
it — only a config-file deploy. `0` disables an individual ceiling. Breaches
deny the turn with a message naming the scope, spend, and config key; a soft
warning fires once per scope per day at `soft_warn_ratio`.

The pre-2026-08 `daily_limit` / `per_agent_limit` keys were never enforced
and are deliberately NOT aliased — they are inert if still present.

```toml
[budget]
enabled = true
hub_daily_usd = 1000.0
agent_daily_usd = 200.0
identity_daily_usd = 50.0
job_total_usd = 25.0                # cumulative $ per owned scheduler job
job_max_runs = 100                  # total fires per owned recurring job
job_expiry_days = 7                 # owned recurring jobs auto-expire
min_recurring_interval_secs = 300   # floor for owned schedules (every + cron)
confirm_recurring_daily_usd = 5.0   # projected $/day above this → user confirm
warn_turn_usd = 1.0                 # per-task forecast warn threshold
soft_warn_ratio = 0.8
exempt_identities = ["system:consolidation"]

# Per-agent tightening (min-merged — can only lower the hub-wide values).
[budget.agents.atlas]
agent_daily_usd = 100.0
```

| Field | Type | Default |
|-------|------|---------|
| `enabled` | bool | `true` |
| `hub_daily_usd` | f64 | `1000.0` |
| `agent_daily_usd` | f64 | `200.0` |
| `identity_daily_usd` | f64 | `50.0` |
| `job_total_usd` | f64 | `25.0` |
| `job_max_runs` | u64 | `100` |
| `job_expiry_days` | u32 | `7` |
| `min_recurring_interval_secs` | u64 | `300` |
| `confirm_recurring_daily_usd` | f64 | `5.0` |
| `warn_turn_usd` | f64 | `1.0` |
| `soft_warn_ratio` | f64 | `0.8` |
| `exempt_identities` | Vec\<String\> | `["system:consolidation"]` |
| `agents` | map | `{}` |

System-registered jobs (monitor sweep, consolidation, briefings, workflow
cron triggers — anything with no creator identity) are exempt from the
interval floor and the per-job caps; owned jobs (created by a person or by an
agent on a person's behalf) get the caps applied at creation and are
auto-disabled — with a DM to the owner — on expiry, run-limit, cumulative
budget, or five consecutive failed runs. Creations projected above
`confirm_recurring_daily_usd` (and any job using the conversation session
context) return a `needs_confirmation` estimate the user must approve; the
confirmed create is additionally gated by a constitutional Ask prompt so the
approval is a real button click, not model judgement.

### `[gateway]` — WebSocket + REST Gateway

```toml
[gateway]
listen = "0.0.0.0:8091"
# auth_token = "your-token"  # Or WORKFORCE_GATEWAY_TOKEN env var
cors = true
```

| Field | Type | Default | Env Override |
|-------|------|---------|--------------|
| `listen` | String | `"0.0.0.0:8091"` | — |
| `auth_token` | Option\<String\> | None (auto-generated) | `WORKFORCE_GATEWAY_TOKEN` |
| `cors` | bool | `true` | — |

### `[knowledge]` — Codebase Knowledge Engine

```toml
[knowledge]
enabled = true
root = "/app/repos/workforce"   # Directory to index (default: agent workspace)
index_on_startup = true
parse_concurrency = 0           # 0 = auto (num_cpus * 2)
generate_summaries = false
max_file_size = 1048576         # 1MB
maintenance_interval_secs = 14400  # 4 hours

[knowledge.summariser]
provider = "ollama"             # ollama | anthropic | openrouter | gemini | grok | vllm | cerebras
url = "http://ollama:11434"     # Base URL (alias: ollama_url)
model = "qwen2.5-coder:7b"
# api_key = "..."               # Last resort; prefer the global vault's knowledge/summariser/api_key
```

| Field | Type | Default | Env Override |
|-------|------|---------|--------------|
| `summariser.provider` | String | `"ollama"` | `SUMMARISER_PROVIDER` |
| `summariser.url` | String | `"http://localhost:11434"` | `SUMMARISER_URL` (fallback: `WORKFORCE_KNOWLEDGE_OLLAMA_URL`) |
| `summariser.model` | String | `"qwen2.5-coder:7b"` | `WORKFORCE_KNOWLEDGE_SUMMARISER_MODEL` |
| `summariser.api_key` | Option\<String\> | None | `SUMMARISER_API_KEY` |

### `[session_lifecycle]` — Session Management

```toml
[session_lifecycle]
daily_reset_hour = 4           # Reset all sessions at 4 AM (user timezone)
idle_expiry_minutes = 120      # Expire idle sessions after 2 hours
archive_expired = true         # Archive before clearing
check_interval_secs = 300      # Check every 5 minutes
```

| Field | Type | Default |
|-------|------|---------|
| `daily_reset_hour` | Option\<u32\> | None (disabled) |
| `idle_expiry_minutes` | Option\<u32\> | None (disabled) |
| `archive_expired` | bool | `true` |
| `check_interval_secs` | u64 | `300` |

### `[session_policies]` — Per-Type/Channel Reset Rules

Fine-grained overrides that layer on top of `session_lifecycle`.

Resolution order: channel-specific → context-type → global.

```toml
[session_policies.types.dm]
daily_reset = false           # DMs don't reset daily

[session_policies.types.group]
idle_expiry_minutes = 60      # Groups expire faster

[session_policies.channels.cli]
daily_reset = false           # CLI sessions persist
idle_expiry_minutes = 0       # Never idle-expire
```

### `[heartbeat]` — Periodic Health Checks

```toml
[heartbeat]
enabled = true
interval_secs = 1800          # 30 minutes
active_start = "08:00"        # Active hours (user timezone)
active_end = "23:00"
deliver = "actionable_only"   # all | actionable_only
channels = ["slack-general"]
history_size = 20
max_checks_per_cycle = 4
dedup_window_secs = 3600
log_to_daily_notes = false
skip_during_active_sessions = false
backoff_multiplier = 2.0
max_backoff_secs = 7200
```

### `[groups]` — Group Chat Behaviour

```toml
[groups]
default_activation = "mention_only"  # always | mention_only | keyword
default_response_mode = "in_channel"

[[groups.channels]]
id = "C0FAKECHAN4"
activation = "always"
response_mode = "thread"
```

### `[pairing]` — Device/API Pairing

```toml
[pairing]
# Channel access control, device pairing, node pairing config
```

### `[sentinel]` — Security Configuration

```toml
[sentinel]
# Trust classification, owner IDs
```

### `[dm_routing]` — Per-User Agent Assignment

```toml
[dm_routing]
default_agent = "allan"

[dm_routing.user_routes]
U12345 = "architect"
U67890 = "allan"
```

### `[briefing]` — Daily Briefing

```toml
[briefing]
enabled = true
cron = "0 9 * * 1-5"          # Weekdays at 9 AM
timezone = "Europe/London"
channel_id = "C0GENERAL"
# model = "claude-sonnet-4-20250514"
```

### `[scheduler]` — Scheduled Job Policy

```toml
[scheduler]
pinned_sessions = true   # false: recurring jobs never keep a session across runs
```

| Field | Type | Default |
|-------|------|---------|
| `pinned_sessions` | bool | `true` |

A pinned job replays all its earlier runs on every run. With `pinned_sessions = false` a recurring job that is pinned runs each time in a fresh session instead (its run record says so), and no recurring job can be pinned. The stored job is not changed, so setting it back restores the pin. `workforce.toml` is the kill switch for every agent; an agent's own `config.toml` can switch it off for that agent and is read each time a job runs. One-shot jobs are not affected.

> **Rolling deploys:** do not set `pinned_sessions = false` until every replica runs a build that knows it. An older replica ignores the setting, so while one can hold the scheduler lead a pinned job runs fresh under one leader and in its session under the next, and the session misses the runs in between.

A pinned session is bounded by `[compaction] chat_rollover_max_messages` (default 200; `0` disables): once it replays that many messages, the next run seals it and continues in a successor that carries a short dated tail of the last runs. The sealed session is kept like any other.

A job stored by an earlier build with a conversation's key (a surface name such as `web`, a DM id) keeps a session of that name per person it runs as (`scheduler:{person}:{key}`), so it is never shared between people; moving it to fresh and pinning it again gives it an empty session of its own. Delivery is unchanged.

### `[agent_comms]` — Agent-to-Agent Communication

```toml
[agent_comms]
oversight_mode = "auto"        # auto | require_approval | notify
auto_approve_pairs = ["allan:architect"]
```

### `[vault]` — Secrets Vault

```toml
[vault]
enabled = true
# path = "{data_dir}/vault.enc"
# key_file = "{data_dir}/vault-key.txt"
# backend = "file"             # or "postgres" (needs [postgres]; migrate first)
# sync_interval_ms = 1000      # postgres: peer-change poll interval
# audit_retention_days = 365   # postgres: audit record retention
```

See [VAULT.md](VAULT.md) for full vault documentation.

### `[skills]` — Skills System

```toml
[skills]
# skills_dir = "{data_dir}/skills"
enabled = ["slack-integration", "github-tools"]
```

### `[github]` — GitHub Integration

```toml
[github]
# GitHub integration settings
```

### `[[agents]]` — Agent Definitions

```toml
[[agents]]
id = "allan"
name = "Allan"
workspace = "/app/data/agents/allan"
default = true
capabilities = ["code-write", "test", "debug"]

# Per-agent LLM overrides (optional — falls back to global [llm])
[agents.llm]
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
failover = ["claude-haiku-4-5-20251001"]

# Per-agent compaction overrides (optional — falls back to global [compaction])
[agents.compaction]
max_context_tokens = 200000
compaction_threshold = 0.85

# Per-agent memory overrides (optional — falls back to global [memory])
[agents.memory]
passive = true
consolidation = true
agent_recall_scope = "isolated"  # lock this agent to its own memories only

# Per-agent vault config (optional)
[agents.vault]
enabled = true

# Per-agent access control
[agents.access]
mode = "open"  # open | allowlist | blocklist
```

| Field | Type | Default |
|-------|------|---------|
| `id` | String | Required |
| `name` | Option\<String\> | None |
| `model` | Option\<String\> | Uses `[llm].default_model` (legacy, prefer `llm.default_model`) |
| `workspace` | Option\<String\> | None |
| `system_prompt` | Option\<String\> | None (uses identity files) |
| `capabilities` | Vec\<String\> | `[]` |
| `default` | bool | `false` |
| `llm` | Option\<AgentLlmConfig\> | Inherits from `[llm]` |
| `compaction` | Option\<AgentCompactionConfig\> | Inherits from `[compaction]` |
| `vault` | Option\<AgentVaultConfig\> | Inherits from `[vault]` |
| `access` | AgentAccessConfig | `{ mode: "open" }` |
| `user_map` | HashMap | `{}` |

### `[[teams]]` — Team Definitions

```toml
[[teams]]
id = "backend"
name = "Backend Dev Team"
lead = "architect"
pattern = "hierarchical"  # hierarchical | pipeline | parallel | swarm
require_review = true

[[teams.members]]
id = "architect"
role = "lead"
model = "claude-sonnet-4-20250514"
capabilities = ["design", "code-review"]
```

---

## Per-Agent Workspace Config (`config.toml`)

Each agent can have a `config.toml` in their workspace directory (e.g., `/app/data/agents/allan/config.toml`). This file overrides global and per-agent defaults from `workforce.toml`.

### Why Workspace Config?

- **Survives CI deploys** — workforce.toml is overwritten by infra CI, but agent workspaces are on persistent volumes
- **Transparent** — plain TOML, easy to inspect with `cat`, and `settings` shows what is in effect
- **Agent-managed** — agents write here via `system.set_model`, `system.set_failover_chain`, etc.
- **Human-editable** — SSH in and edit directly, or ask the agent to change settings

### Full Reference

```toml
# /app/data/agents/allan/config.toml
# Per-agent configuration — managed by agent tools or human edit.
# Overrides global defaults from workforce.toml.
# This file survives CI deploys.

[llm]
provider = "anthropic"
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
max_rounds = 50
failover = ["claude-haiku-4-5-20251001", "xai:grok-3"]
accounts = ["ANTHROPIC_API_KEY_2"]

[llm.retry]
max_retries = 3
initial_backoff_ms = 1000
backoff_multiplier = 2.0
max_backoff_ms = 30000

[llm.health]
failure_threshold = 3
window_secs = 300
recovery_secs = 120

[llm.aliases]
fast = "claude-haiku-4-5-20251001"
smart = "claude-sonnet-4-20250514"

[compaction]
max_context_tokens = 200000
compaction_threshold = 0.85
keep_recent_messages = 20
enable_memory_flush = true
prune_tool_outputs = true
prune_tool_output_max_chars = 2000
max_compaction_passes = 3
min_messages_to_compact = 5
urgent_compaction_threshold = 0.95
compaction_model = "claude-haiku-4-5-20251001"

[memory]
passive = true
passive_model = "cheap"
passive_min_turn_tokens = 200
consolidation = true
consolidation_interval_secs = 14400
consolidation_lookback_secs = 43200
consolidation_model = "cheap"

[llm.routing]
enable_adaptive_routing = true
investigation_switch_round = 5
coding_switch_round = 10
read_only_threshold = 3
fast_round_threshold_ms = 30000
escalate_on_haiku_failure = true
```

All fields are optional. Only include the fields you want to override — unset fields inherit from the agent's section in `workforce.toml`, then from the global defaults.

**Important: replace, not merge.** Workspace config fields like `failover` and `accounts` **replace** the global value entirely — they don't append to it. If the global `accounts = ["ANTHROPIC_API_KEY_2"]` and you set `accounts = ["ANTHROPIC_API_KEY_ALLAN"]` in workspace config, the agent gets ONLY `ANTHROPIC_API_KEY_ALLAN`. Include all desired entries: `accounts = ["ANTHROPIC_API_KEY_2", "ANTHROPIC_API_KEY_ALLAN"]`.

### Overridable Fields

| Section | Field | Type | Description |
|---------|-------|------|-------------|
| `[llm]` | `provider` | String | LLM provider override |
| `[llm]` | `default_model` | String | Active model |
| `[llm]` | `max_tokens` | u32 | Max tokens per response |
| `[llm]` | `max_rounds` | u32 | Max tool rounds per turn |
| `[llm]` | `failover` | Vec\<String\> | Failover model chain |
| `[llm]` | `accounts` | Vec\<String\> | Token pool env var names |
| `[llm]` | `aliases` | HashMap | Model aliases |
| `[llm.retry]` | `max_retries` | u32 | Retry count |
| `[llm.retry]` | `initial_backoff_ms` | u64 | Initial backoff |
| `[llm.retry]` | `backoff_multiplier` | f64 | Backoff factor |
| `[llm.retry]` | `max_backoff_ms` | u64 | Max backoff |
| `[llm.health]` | `failure_threshold` | u32 | Failures to mark degraded |
| `[llm.health]` | `window_secs` | u64 | Failure window |
| `[llm.health]` | `recovery_secs` | u64 | Recovery delay |
| `[llm.routing]` | `enable_adaptive_routing` | bool | Enable model routing |
| `[llm.routing]` | `investigation_switch_round` | u32 | Round to switch for investigation |
| `[llm.routing]` | `coding_switch_round` | u32 | Round to switch for coding |
| `[llm.routing]` | `read_only_threshold` | u32 | Read-only rounds before switch |
| `[llm.routing]` | `fast_round_threshold_ms` | u64 | Fast round threshold |
| `[llm.routing]` | `escalate_on_haiku_failure` | bool | Re-escalate on Haiku failure |
| `[compaction]` | `max_context_tokens` | u32 | Context window size |
| `[compaction]` | `compaction_threshold` | f32 | Trigger threshold |
| `[compaction]` | `urgent_compaction_threshold` | f32 | Urgent threshold |
| `[compaction]` | `keep_recent_messages` | usize | Messages to keep |
| `[compaction]` | `enable_memory_flush` | bool | Memory flush on compaction |
| `[compaction]` | `prune_tool_outputs` | bool | Truncate tool outputs |
| `[compaction]` | `prune_tool_output_max_chars` | usize | Max tool output chars |
| `[compaction]` | `max_compaction_passes` | u32 | Safety valve |
| `[compaction]` | `min_messages_to_compact` | usize | Min messages |
| `[compaction]` | `compaction_model` | String | Model for compaction |
| `[memory]` | `passive` | bool | Enable post-turn extraction |
| `[memory]` | `passive_model` | String | Model for extraction |
| `[memory]` | `passive_min_turn_tokens` | u32 | Minimum turn tokens |
| `[memory]` | `passive_prompt` | String | Custom extraction prompt |
| `[memory]` | `consolidation` | bool | Enable periodic consolidation |
| `[memory]` | `consolidation_interval_secs` | u64 | Consolidation interval |
| `[memory]` | `consolidation_lookback_secs` | u64 | Session scan lookback window |
| `[memory]` | `consolidation_model` | String | Model for merge phase |
| `[memory]` | `agent_recall_scope` | String | `shared` (default) or `isolated` cross-agent recall |
| `[scheduler]` | `pinned_sessions` | bool | `false` stops this agent's recurring jobs keeping a session across runs |

---

## Per-Session Model Override

A single per-session override exists: model selection. This is stored in the session object (JSONL in `{data_dir}/sessions/`), not in config files or vault. It's transient — it lives for the session's lifetime and is cleared on session reset.

**Tool:** `system.set_session_model`

```json
{ "model": "claude-haiku-4-5-20251001" }
```

This sets `session.model_override` on the current session. When set, it takes priority over all config layers for that session only. Use `{"model": "reset"}` to clear the override and revert to the agent's default.

### Model Resolution Priority Chain

When the hub needs to determine which model to use for an agent turn:

```
1. Session model override (if set)          → session.model_override
2. Runtime in-memory override (if set)      → active_model RwLock
3. Workspace config.toml [llm].default_model
4. workforce.toml [[agents]] llm.default_model
5. workforce.toml [llm].default_model       → global default
```

Steps 2-4 are resolved at startup into `effective_llm.default_model`. Agent tools update both the in-memory RwLock (step 2, immediate effect) and workspace config.toml (step 3, durable). Step 1 is checked per-turn at runtime.

---

## Agent Runtime Tools

These tools let agents manage their own configuration at runtime. Changes take effect immediately (both in-memory and persisted to disk).

### LLM Tools

| Tool | What It Changes | Where It Writes | Scope |
|------|----------------|----------------|-------|
| `system.llm_status` | — (read-only) | — | View current LLM state |
| `system.set_model` | Default model | `{workspace}/config.toml` [llm] | Per-agent, durable |
| `system.set_session_model` | Session model override | Session object | Per-session, transient |
| `system.set_max_tokens` | Max tokens per response | `{workspace}/config.toml` [llm] | Per-agent, durable |
| `system.set_max_rounds` | Max tool rounds | `{workspace}/config.toml` [llm] | Per-agent, durable |
| `system.set_failover_chain` | Failover models | `{workspace}/config.toml` [llm] | Per-agent, durable |
| `system.set_routing` | Adaptive routing config | `{workspace}/config.toml` [llm.routing] | Per-agent, durable |
| `system.routing_status` | — (read-only) | — | View routing state |
| `system.config` | Any setting by dotted key (`get` / `set` / `unset`, e.g. `llm.voice_model`) | `{workspace}/config.toml`, comments kept, admitted + revisioned + hot-applied like a UI save | Per-agent, durable |
| `system.list_models` | — (read-only) | — | List available models |

### Token & Account Tools (Vault)

| Tool | What It Changes | Where It Writes | Scope |
|------|----------------|----------------|-------|
| `system.add_llm_token` | Add API key to pool | Vault (`llm/tokens/{name}`) | Per-agent, secret |
| `system.remove_llm_token` | Remove API key | Vault | Per-agent, secret |
| `system.list_llm_tokens` | — (read-only) | — | List tokens (no values) |
| `system.add_account` | Add provider account | Vault (`llm/accounts/{name}`) | Per-agent, secret |
| `system.remove_account` | Remove account | Vault | Per-agent, secret |
| `system.list_accounts` | — (read-only) | — | List accounts |

### Compaction Tool

| Action | What It Changes | Where It Writes |
|--------|----------------|----------------|
| `compaction status` | — (read-only) | — |
| `compaction set` | Compaction settings | `{workspace}/config.toml` [compaction] |
| `compaction reset` | Clear overrides | Removes from `{workspace}/config.toml` |
| `compaction compact` | Trigger compaction | In-memory (session) |

### Config Inspection

| Tool | Description |
|------|-------------|
| `settings` | Effective settings, where each value comes from and whether it is in effect |

`settings(action="show")` gives a summary: each section's state, live facts (this turn's model, the vision strategy), every value that is not a built-in default and every default that holds because an override is not in effect. `settings(action="show", section="llm")` lists one section (`section="all"` for everything) and `settings(action="explain", key="llm.vision.model")` shows one setting's chain from default to runtime. Each row has a `source` (`default`, `install`, `install_agent`, `agent`, `runtime`, `environment`, `vault`) and, where it matters, a `status` (`active`, `off`, `kill_switched`, `shadow`, `inert`) with the reason. Secrets are shown only as set or not set. Install-wide tables are shown to admins.

---

## Config Merge Examples

### Example 1: Global default only

**workforce.toml:**
```toml
[llm]
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
```

**No workspace config.toml exists.**

**Effective:** model = `claude-sonnet-4-20250514`, max_tokens = `8192`

### Example 2: Agent section override

**workforce.toml:**
```toml
[llm]
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192

[[agents]]
id = "architect"
[agents.llm]
default_model = "claude-sonnet-4-20250514"
max_tokens = 16384
```

**Effective for architect:** model = `claude-sonnet-4-20250514`, max_tokens = `16384`

### Example 3: Workspace config override

**workforce.toml:**
```toml
[llm]
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
```

**`/app/data/agents/allan/config.toml`:**
```toml
[llm]
default_model = "claude-haiku-4-5-20251001"
```

**Effective for allan:** model = `claude-haiku-4-5-20251001`, max_tokens = `8192` (inherited from global)

### Example 4: All layers combined

**workforce.toml:**
```toml
[llm]
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192

[[agents]]
id = "allan"
[agents.llm]
max_tokens = 12000
```

**`/app/data/agents/allan/config.toml`:**
```toml
[llm]
default_model = "claude-haiku-4-5-20251001"
```

**Effective for allan:** model = `claude-haiku-4-5-20251001` (workspace), max_tokens = `12000` (agent section)

The workspace config only overrides `default_model`. `max_tokens` falls through to the agent section in workforce.toml.

---

## Redeploys and Persistent Data

A redeploy replaces the container and whatever config files your deployment
tooling ships with it (`workforce.toml`, compose files, `nats.conf`). Anything
on the persistent data volume survives:

| Path | Survives? | Why |
|------|-----------|-----|
| Environment / secrets file (e.g. `hub.env`) | Yes | Owned by the operator, not the image |
| `/app/data/**` | Yes | Persistent volume |
| Agent workspaces (`/app/data/agents/*/`) | Yes | On persistent volume |
| `{workspace}/config.toml` | Yes | On persistent volume |
| `{workspace}/vault.enc` | Yes | On persistent volume |
| Sessions, daily notes, memory files | Yes | On persistent volume |

### Making Config Changes Permanent

- **Global settings:** edit `workforce.toml` wherever your deployment keeps it, then redeploy
- **Per-agent settings:** use agent tools (`system.set_model`, etc.) or edit `{workspace}/config.toml` — changes survive redeploys
- **Secrets:** use agent tools (`system.add_llm_token`) or the CLI (`workforce vault set`) — the vault is on the persistent volume

## Vault-to-Workspace Migration

On startup, the hub automatically migrates legacy config overrides from vault to workspace config.toml. This is a one-time migration for agents that previously stored config in the vault.

### Migrated Keys

| Vault Key | Workspace Config Field |
|-----------|----------------------|
| `llm/active_model` | `[llm].default_model` |
| `llm/max_tokens` | `[llm].max_tokens` |
| `llm/max_rounds` | `[llm].max_rounds` |
| `llm/failover_chain` | `[llm].failover` |
| `llm/active_routing` | `[llm.routing].*` |
| `compaction/config` | `[compaction].*` |

After migration, these keys are deleted from the vault. Secret entries (`llm/tokens/*`, `llm/accounts/*`) remain in the vault.

---

## Token Pool Architecture

Each agent's LLM token pool is assembled from two sources at startup:

### 1. Global tokens (env vars → config accounts)

Defined in `hub.env` as environment variables and referenced by name in config:

```
# hub.env
ANTHROPIC_API_KEY=sk-ant-api03-...       # Primary key (loaded as "default")
ANTHROPIC_API_KEY_2=sk-ant-api03-...     # Pool key

# workforce.toml (or workspace config.toml)
[llm]
accounts = ["ANTHROPIC_API_KEY_2"]
```

The primary key (`ANTHROPIC_API_KEY`) is always loaded as the default provider. The `accounts` array names additional env vars to add to the pool. All agents share these tokens.

### 2. Per-agent tokens (vault)

Added via `system.add_llm_token` and stored encrypted in the agent's vault (`llm/tokens/{name}`). These are loaded automatically at startup — no config.toml entry needed.

```
# Allan's vault contains:
llm/tokens/ANTHROPIC_API_KEY_3 → sk-ant-api03-...
llm/tokens/ANTHROPIC_API_KEY_4 → sk-ant-api03-...
```

Only that agent gets these tokens. The name is a friendly label chosen by the agent — it doesn't need to match an env var.

### Per-agent global token overrides

To give a specific agent its own global-style token (via env var), add the env var to `hub.env` and reference it in the agent's workspace config:

```
# hub.env
ANTHROPIC_API_KEY_ALLAN=sk-ant-api03-...

# /app/data/agents/allan/config.toml
[llm]
accounts = ["ANTHROPIC_API_KEY_2", "ANTHROPIC_API_KEY_ALLAN"]
```

**Important:** workspace config `accounts` **replaces** the global list — include all desired accounts, not just the new one.

### Resulting pool at startup

| Source | Example | Available to |
|--------|---------|-------------|
| Primary env var (`ANTHROPIC_API_KEY`) | "default" | All agents |
| Config `accounts` env vars | "ANTHROPIC_API_KEY_2" | Agents whose effective config includes it |
| Vault `llm/tokens/*` | "ANTHROPIC_API_KEY_3" | Only that agent |

---

## Environment Variable Reference

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENROUTER_API_KEY` | OpenRouter API key |
| `ANTHROPIC_API_KEY_2`, `_3`, ... | Additional pool keys (named in `accounts`) |
| `WORKFORCE_API_KEY` | Generic fallback API key |
| `WORKFORCE_GATEWAY_TOKEN` | Gateway auth token |
| `WORKFORCE_NATS_URL` | Override NATS URL |
| `WORKFORCE_WEAVIATE_URL` | Override Weaviate URL |
| `WORKFORCE_LISTEN` | Override admin API listen address |
| `WORKFORCE_LOG_LEVEL` | Override log level |
| `WORKFORCE_LLM_PROVIDER` | Override LLM provider |
| `WORKFORCE_LLM_MODEL` | Override default model |
| `WORKFORCE_VAULT_KEY` | Vault master key (base64) |
| `WEAVIATE_API_KEY` | Weaviate authentication key |
| `RUST_LOG` | Standard Rust log filter |
| `SLACK_BOT_TOKEN` | Slack bot token |
| `SLACK_APP_TOKEN` | Slack app token |
