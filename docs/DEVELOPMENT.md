# Development Guide

How to build, test, extend, and contribute to the Workforce framework.

## Prerequisites

- **Rust** (stable, 2021 edition) — `rustup install stable`
- **Docker** — optional, for running NATS and Weaviate locally

## Building

```bash
# Debug build (faster compilation)
cargo build

# Release build (optimized)
cargo build --release

# Check only (no codegen, fastest feedback)
cargo check

# Build a specific binary
cargo build --bin workforce
cargo build --bin workforce-hub
```

### Binaries Produced

| Binary | Crate | Description |
|--------|-------|-------------|
| `workforce` | `crates/cli` | Main CLI — all user commands |
| `workforce-hub` | `crates/hub` | Standalone hub server |

## Testing

```bash
# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p workforce-core
cargo test -p workforce-policy
cargo test -p workforce-common

# Run a specific test
cargo test -p workforce-policy -- engine::tests::wildcard_tool_matching

# Run tests with output
cargo test -- --nocapture

# Run only doc tests
cargo test --doc
```

### Test Coverage by Crate

| Crate | Tests | What's Covered |
|-------|-------|----------------|
| `workforce-core` | ✅ | Message creation, serialization, task lifecycle, policy decisions |
| `workforce-common` | ✅ | Config parsing, session lifecycle, identity loading, cost tracking, persistence (SQLite), audit log, locking, tenancy |
| `workforce-policy` | ✅ | Rule matching, wildcards, scope overrides, conditions (all operators), priority ordering, YAML parsing |
| `workforce-nats` | ✅ | Envelope serialization, subject formatting, conversation messages, in-memory publisher |
| `workforce-agent` | ✅ | Tool registry, tool definition listing, LLM message/response types |
| `workforce-orchestrator` | ✅ | Task lifecycle, team creation, task routing, model routing, swarm queue, conflict management, escalation |
| `workforce-memory` | ✅ | In-memory store, Weaviate store, memory types, visibility, consolidation, schema |
| `workforce-skills` | ✅ | Skill definitions, registry, plugin manifest, SDK, events, Slack adapter |
| `workforce-distributed` | ✅ | Node registry, leaf config, affinity selection, context transfer, credential store, offline queue |

### Integration Tests

Integration tests (requiring external services) are not yet set up, but the workspace `Cargo.toml` includes `testcontainers-modules` for NATS and Postgres:

```toml
[workspace.dev-dependencies]
testcontainers-modules = { version = "0.11.6", features = ["postgres", "nats"] }
wiremock = "0.6.2"
```

You can write integration tests that spin up NATS/Postgres in containers:

```rust
#[tokio::test]
async fn test_with_nats() {
    let nats_container = testcontainers_modules::nats::Nats::default();
    // ...
}
```

### Postgres Backend Tests

The Postgres backend parity + migration tests live in
`crates/{common,identity,pairing,codebase-knowledge}/src/*postgres*.rs`.
By default they run against an **in-process embedded Postgres**
([`postgresql_embedded`](https://crates.io/crates/postgresql_embedded))
that downloads on first run (~5–10s) and is then cached, so no Docker
or env vars are needed.

```bash
# Run the embedded path for one crate:
cargo test -p workforce-common --features postgres --lib telemetry_postgres
```

Two env vars override the default:

| Env var | Effect |
|---|---|
| `WORKFORCE_TEST_POSTGRES_URL` | Use an external Postgres instead of `postgresql_embedded`. Each test still creates and drops a unique `test_db_<uuid>` against that server. |
| `WORKFORCE_SKIP_POSTGRES_TESTS` | Skip every Postgres parity / migration test. Takes precedence over the URL var. |

CI sets `WORKFORCE_SKIP_POSTGRES_TESTS=1` so the workflow doesn't have
to install or start Postgres. The trade-off is that **regressions in
`Postgres*Store` / `PostgresCodeGraphBackend` are not caught by CI**.

> **Contributor rule:** any change to `crates/*/src/*postgres*.rs`,
> the `crates/common/migrations/telemetry/V*.sql` migrations, or the
> embedded refinery migrations in identity / pairing / codegraph
> **must** be validated locally before pushing:
>
> ```bash
> cargo test --workspace --features \
>   workforce-common/postgres,workforce-identity/postgres,workforce-pairing/postgres,workforce-codebase-knowledge/postgres
> ```
>
> On macOS, running across multiple crates exhausts the System V SHM
> limit if each binary spawns its own embedded Postgres. Point
> `WORKFORCE_TEST_POSTGRES_URL` at a local Docker Postgres to share
> one server across binaries.

See [POSTGRES.md](POSTGRES.md) for the architectural rationale, the
per-store-pool design, and the migration roadmap.

## Project Structure

```
workforce/
├── Cargo.toml              # Workspace root
├── workforce.toml           # Default config (for local dev)
├── config/
│   └── default.toml         # Fallback config defaults
├── crates/
│   ├── core/                # Foundation types (no deps)
│   ├── common/              # Config, sessions, identity, persistence
│   ├── nats-bus/            # NATS messaging abstraction
│   ├── policy-engine/       # YAML policy rules engine
│   ├── agent-runtime/       # LLM clients, tools, runner loop
│   ├── orchestrator/        # Teams, tasks, routing
│   ├── memory/              # Vector memory (Weaviate + in-memory)
│   ├── skills/              # Skill/plugin definitions
│   ├── distributed/         # Node management, leaf nodes
│   ├── hub/                 # Main service (API, gateway, Slack)
│   └── cli/                 # CLI binary
├── docs/                    # Documentation
│   ├── ARCHITECTURE.md
│   ├── CONFIGURATION.md
│   ├── CLI.md
│   ├── GUIDES.md
│   ├── DEVELOPMENT.md
│   └── GATEWAY_PROTOCOL.md
└── deploy/                  # Deployment configs (if present)
```

## Adding a New Crate

1. **Create the crate:**

```bash
mkdir -p crates/my-feature/src
```

2. **Create `crates/my-feature/Cargo.toml`:**

```toml
[package]
name = "workforce-my-feature"
version.workspace = true
edition.workspace = true

[dependencies]
# Use workspace dependencies
serde = { workspace = true }
eyre = { workspace = true }
workforce-core = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
```

3. **Add to workspace `Cargo.toml`:**

```toml
[workspace]
members = [
    # ... existing crates
    "crates/my-feature",
]

# If other crates need to depend on it:
[workspace.dependencies]
workforce-my-feature = { path = "crates/my-feature" }
```

4. **Create `crates/my-feature/src/lib.rs`:**

```rust
pub mod my_module;
```

5. **Verify:**

```bash
cargo check
cargo test -p workforce-my-feature
```

## Adding a New Tool

Tools are the primary way agents interact with the world. Each tool implements the `Tool` trait from `workforce-agent`.

### 1. Create the Tool

In `crates/agent-runtime/src/tools/`, create a new file (e.g., `http.rs`):

```rust
use std::time::Instant;
use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use super::{Tool, ToolResult};

pub struct HttpFetch;

#[derive(Deserialize)]
struct HttpFetchInput {
    url: String,
    method: Option<String>,
}

#[async_trait]
impl Tool for HttpFetch {
    fn name(&self) -> &str {
        "http.fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return the response body."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch"
                },
                "method": {
                    "type": "string",
                    "description": "HTTP method (GET, POST, etc.)",
                    "default": "GET"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: HttpFetchInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let client = reqwest::Client::new();
        let resp = client.get(&input.url).send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;

        Ok(ToolResult::success(
            serde_json::json!({
                "status": status,
                "body": body,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}
```

### 2. Register in the Module

Add to `crates/agent-runtime/src/tools/mod.rs`:

```rust
pub mod http;
```

### 3. Register in Default Tools

Add to `default_tools()` in `crates/agent-runtime/src/runner.rs`:

```rust
pub fn default_tools(allowed_paths: Vec<std::path::PathBuf>) -> ToolRegistry {
    use crate::tools::http::HttpFetch;
    // ... existing tools ...
    registry.register(Arc::new(HttpFetch));
    registry
}
```

### 4. Add Policy Rules

Update your policy file to allow/restrict the new tool:

```yaml
rules:
  - name: "allow-http"
    scope: global
    tool: "http.*"
    action: allow
```

### Key Points

- Tool names use dot notation (`namespace.action`) — e.g., `filesystem.read`, `exec.run`
- Names are converted to underscores for the LLM API (`filesystem_read`) and back automatically
- The `input_schema` must be valid JSON Schema — the LLM uses it to generate correct inputs
- Return `ToolResult::success(...)` or `ToolResult::failure(...)` with timing info
- Sandbox tools appropriately (file operations check `allowed_paths`)

## Credentials

Integrations resolve their keys through the layered vault, never from the install config
or the environment at boot. A new integration that needs an API key:

1. Declares it as a `VaultCredential` in `crates/hub/src/credentials.rs` and adds it to
   `CATALOG`, which puts it in the vault editor's dropdown. Pick the scope by who pays and
   who may hold it: `Either` when people may bring their own key, `Shared` for an
   operator's service key. Members can save an `Either` or `PerUser` credential in their
   own tiers without further wiring.
2. Reads it with `credentials::Caller` on every use: `Caller::for_service(service, person)`
   where the work belongs to a known person (an inbound message's sender, a workflow's
   run-as identity), `Caller::in_scope(..)` inside a turn, `Caller::new(None, ..)` for
   system work. A client built once that needs a key per request takes
   `credentials::source(..)`, an `ApiKeySource` it asks each time (`web.search`, the
   embedders), or a `KeySlot` (`credentials::slot(..)`), which also carries the sentence
   said when there is no key (the media clients). A provider client that bakes its key
   in at construction goes behind `KeyedProvider`. Vault reads are in-memory, so per-call
   reads are cheap and a rotated key applies at once.
3. Treats a missing key as "this integration is not available to this caller": skip it
   or fall back, log at debug, and never send a request without the key it needs.

A config `api_key` or environment variable may stay as the last-resort fallback for
existing installs. Infrastructure secrets that no user supplies (the vault master key,
gateway/JWT secrets, NATS and Postgres credentials, AWS role credentials) stay in the
environment. See `VAULT.md` → Integration credentials.

## Adding a New LLM Provider

LLM providers implement the `LlmProvider` trait:

```rust
// In crates/agent-runtime/src/llm/provider.rs

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<LlmResponse>;
}
```

### Steps

1. Create a new client file in `crates/agent-runtime/src/llm/` (e.g., `my_provider.rs`)
2. Implement the provider's API client
3. Implement `LlmProvider` for your client
4. Add to `ProviderKind` enum and `create_provider()` factory
5. Add the module to `crates/agent-runtime/src/llm/mod.rs`

The key requirement: convert between Workforce's internal types (`LlmMessage`, `ContentBlock`, `LlmToolDef`) and your provider's API format, then return an `LlmResponse`.

## Writing Tests

### Unit Tests

Place unit tests in the same file, inside a `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_sync_test() {
        let result = my_function(42);
        assert_eq!(result, 84);
    }

    #[tokio::test]
    async fn my_async_test() {
        let result = my_async_function().await;
        assert!(result.is_ok());
    }
}
```

### Test Utilities

The workspace includes these test dependencies:

| Crate | Use |
|-------|-----|
| `tokio-test` | Async test runtime |
| `mockall` | Mock traits for testing |
| `wiremock` | Mock HTTP servers |
| `testcontainers-modules` | Docker containers for integration tests |
| `fake` | Generate fake test data |

### Using InMemory Implementations

Several crates provide in-memory implementations for testing:

```rust
use workforce_common::session::InMemorySessionStore;
use workforce_memory::in_memory::InMemoryStore;
use workforce_nats::client::InMemoryPublisher;
use workforce_common::persistence::SqliteStore;

// In-memory session store
let sessions = InMemorySessionStore::new();

// In-memory memory store
let memory = InMemoryStore::new();

// In-memory NATS publisher (captures published messages)
let publisher = InMemoryPublisher::new();

// In-memory SQLite (for persistence tests)
let store = SqliteStore::in_memory().unwrap();
```

## Linting and Formatting

```bash
# Format all code
cargo fmt

# Check formatting
cargo fmt --check

# Run clippy lints
cargo clippy

# Run clippy with all targets
cargo clippy --all-targets
```

## Running Infrastructure Locally

### NATS

```bash
# Docker
docker run -d --name nats -p 4222:4222 -p 8222:8222 nats:latest -js

# Verify
curl http://localhost:8222/healthz
```

### Weaviate

```bash
docker run -d --name weaviate \
  -p 8080:8080 \
  -e AUTHENTICATION_ANONYMOUS_ACCESS_ENABLED=true \
  -e PERSISTENCE_DATA_PATH=/var/lib/weaviate \
  semitechnologies/weaviate:latest
```

### Both (Docker Compose)

If a `docker-compose.yml` exists in `deploy/`:

```bash
cd deploy
docker compose up -d
```

## Architecture Decisions

| Decision | Rationale |
|----------|-----------|
| Monorepo with workspace | Clear dependency boundaries, single build, shared deps |
| `eyre` for errors | Nice error context and backtraces for debugging |
| `async-trait` | Trait objects for LLM providers, tools, memory stores |
| NATS for messaging | Lightweight, supports JetStream persistence, leaf nodes for distributed |
| SQLite for local persistence | Zero-config, embedded, good enough for single-node |
| YAML for policies | Human-readable, easy to edit, supports complex conditions |
| TOML for config | Rust ecosystem standard, clear structure |
| Tool name dot notation | `namespace.action` groups related tools, converts to underscores for LLM APIs |
| In-memory fallbacks | Every external dependency (NATS, Weaviate) has an in-memory fallback |
