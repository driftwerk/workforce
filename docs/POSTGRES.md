# Postgres backend (optional)

Workforce historically used a mix of SQLite databases and on-disk files
for state. This is fine for embedded single-host deployments but can't
be shared across hub instances. The Postgres backend is an opt-in
alternative for production deployments that need centralised data.

This is a **multi-PR migration**. The first PR shipped the
cross-cutting infrastructure plus a backend-agnostic
`TelemetryStore` trait. The telemetry backend (`cost_records`,
`task_telemetry`, `audit_log`, workflow instances, security events,
etc.) is now fully usable against Postgres via
`[telemetry] backend = "postgres"`. SQLite remains the default.

## When to enable

Enable Postgres if:

- You run more than one hub instance and want them to share telemetry,
  cost, audit, and workflow state
- You want to query cost / usage / audit data from outside the hub
  (BI tools, custom dashboards, ad-hoc `psql`)
- You want centralised backups instead of one-`.db`-file-per-host

Stick with SQLite if:

- You run a single hub instance
- You want zero ops overhead and no external services
- You're developing locally

The Postgres backend is **strictly opt-in**. Default builds and default
configs continue to use SQLite with no behavioural change.

## Tenancy model

**One workforce installation per tenant.** Each customer organisation
runs its own workforce hub against its own Postgres database. There is
no shared multi-tenant deployment, no row-level isolation, no
`tenant_id` columns. Standing up a second customer means standing up a
second workforce installation pointed at its own database.

Multiple hub *instances* sharing one Postgres database is fine and
expected (that's the operational point of moving off SQLite) — but
they all belong to the same organisation.

## Configuration

Add a `[postgres]` block to your `workforce.toml`:

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
# password = "${POSTGRES_PASSWORD}"        # env var expansion

# Optional knobs:
pool_size = 16
application_name = "workforce-hub-prod"
```

For container-based deploys, `${VAR}` expansion in `workforce.toml`
makes it easy to inject the password (and host, port, etc.) from a
mounted Kubernetes Secret or a Docker `--env-file` without rewriting
the config. See [CONFIGURATION.md](CONFIGURATION.md#environment-variable-interpolation).

When `enabled = false` (or the block is omitted), every store falls
back to its SQLite/file backend regardless of any per-store backend
setting.

When `enabled = true`, every store that supports a Postgres backend —
`telemetry`, `identity`, `pairing_store`, `codegraph` — defaults to
`backend = "postgres"` automatically. Setting any of those tables'
`backend` field explicitly overrides the default (use this to keep a
single store on SQLite while the rest move to Postgres). The default
is applied once at config-load time, before deserialisation.

### Enabling the Postgres telemetry backend

With `[postgres] enabled = true` in place, telemetry already uses the
Postgres backend by default — no additional config is needed. To force
SQLite for just telemetry while leaving the rest on Postgres, set:

```toml
[telemetry]
backend = "sqlite"
```

On next startup the hub will:

1. Build a Postgres connection pool (liveness-check via `SELECT 1`).
2. Create the `telemetry` schema if missing and run the embedded
   refinery migrations against it (`V001__init.sql`).
3. Pin `search_path` to `telemetry, public` on every connection it
   hands out, so subsequent queries don't need schema qualifiers.
4. Serve every telemetry / cost / audit / workflow read and write
   through `PostgresTelemetryStore` instead of SQLite.

If the Postgres pool fails to come up, the hub logs the failure and
falls back to the local `{hub.data_dir}/telemetry.db` SQLite file.
Postgres is opt-in — unavailable Postgres never fails startup.

### Architectural note: per-store pools

Each store that opts into Postgres builds its **own** deadpool pool
from the shared `[postgres]` connection metadata. The pool is not
literally shared between stores. This is deliberate: every store
pins `search_path` to its own schema on every connection via a
deadpool `post_create` hook, so unqualified table names always
resolve. Pools are cheap; Postgres sees one TCP connection per
checked-out client either way.

## Roadmap

Phases land as separate PRs and can be deployed independently. Each
phase keeps its store's existing SQLite backend working unchanged —
swapping is a config change, never a build change.

| Phase | Store                 | Trait status                          | Status  |
|-------|-----------------------|---------------------------------------|---------|
| 0     | Shared infrastructure | n/a                                   | ✅      |
| 1A    | Telemetry             | `TelemetryStore` extracted            | ✅      |
| 1B    | Telemetry (Postgres)  | `PostgresTelemetryStore` impl         | ✅      |
| 2     | Memory                | covered by Weaviate — **dropped**     | n/a     |
| 3     | Codegraph             | `CodeGraphBackend` + `Postgres*` impl | ✅      |
| 4     | Identity + pairing    | `Postgres{Identity,Pairing}Store`     | ✅      |

## Out of scope

The vault has its own opt-in Postgres backend (`[vault] backend =
"postgres"`, schema `vault`): rows are encrypted in the hub and the key
never reaches the database — see [VAULT.md](VAULT.md#storage-backends)
and its Troubleshooting section.

These stores stay file/SQLite-only by design:

- **Per-agent session JSONL** (`{workspace}/sessions/*.jsonl`) —
  immutable append-only chat transcripts; designed as local audit
  trail.
- **Skills directory** (`{data_dir}/skills/`) — static config, not a
  data store. Distributed via git/config.

## Vector search

Postgres holds **relational and structural metadata only**. Vector
search (memory embeddings, code-knowledge summary embeddings) continues
to use the existing Weaviate path and is not migrated to pgvector in
this roadmap. If you currently run Weaviate, keep running it.

## Schema and migrations

Each store that gains a Postgres backend owns its own embedded
refinery migrations under its own Postgres schema (`telemetry`,
`codegraph`, `identity`, `pairing`). Per-schema isolation means each
store can be migrated and rolled back independently. Migrations run
at hub startup and are idempotent.

Current migration state per schema:

- **`telemetry`** — `V001__init.sql` (all base tables) and
  `V002__cost_records_cache_columns.sql` (adds
  `cache_creation_input_tokens` + `cache_read_input_tokens` to
  `cost_records` for the cache-aware billing change in `30b8c2a`).
- **`metabase_schema`** — `V001__init.sql` (instances, databases,
  table usage) and `V002__database_engine.sql` (adds `databases.engine`
  so every replica publishes the dialect hint).
- **`identity`**, **`pairing`**, **`codegraph`** — `V001__init.sql`
  embedded inside the respective crate.

## Migration from existing SQLite data

Each store that gains a Postgres backend ships a one-shot
`workforce migrate-<store>` CLI subcommand that reads from the
existing SQLite database and writes to Postgres in batches. Fresh
deployments can skip this and just point at an empty Postgres database
— the refinery migrations create the schema on first connect.

### `workforce migrate-codegraph`

Same idempotent semantics as the others. Copies the SQLite code
graph (`codegraph.db`) into the Postgres `codegraph` schema. The
default source path is
`{hub.data_dir}/.workforce/knowledge/codegraph.db` — use `--from`
to override.

```bash
workforce migrate-codegraph --config workforce.toml
```

After running, flip the config:

```toml
[codegraph]
backend = "postgres"
```

Note that vector embeddings (the `CodeEntity` and `CodeSummary`
Weaviate collections) are unaffected — only the structural metadata
graph (entities, relationships, summaries, file index, index state)
moves to Postgres. Weaviate continues to be queried in parallel for
semantic search.

### `workforce migrate-identity` and `workforce migrate-pairing`

Same idempotent semantics as `migrate-telemetry`. Each command reads
`[postgres]` from the config, runs the destination refinery
migrations against its own schema (`identity` or `pairing`), and
copies every row from the SQLite source.

```bash
workforce migrate-identity --config workforce.toml
workforce migrate-pairing --config workforce.toml
```

After each run, flip the corresponding config key:

```toml
[identity]
backend = "postgres"

[pairing_store]
backend = "postgres"
```

`identity` covers users + teams (`identities`, `teams`); `pairing`
covers channel pairing requests, the channel allowlist, device
pairing requests, approved devices, and node pairing requests
(5 tables).

> **Note:** the existing `[pairing]` block in `workforce.toml`
> controls pairing *runtime policy* (`code_ttl_secs`, allowlist
> mode, etc.) and is unchanged. The new `[pairing_store]` block
> only controls *where* the pairing data is persisted.

### `workforce migrate-telemetry`

Copies a SQLite `telemetry.db` into the Postgres telemetry backend.

```bash
# Stop the hub first so the source file isn't being written to.
workforce migrate-telemetry --config workforce.toml

# Custom source (default is {hub.data_dir}/telemetry.db):
workforce migrate-telemetry --config workforce.toml --from /backup/telemetry.db
```

Behaviour:

- Reads `[postgres]` from the config. Fails if absent or
  `enabled = false`.
- Runs the destination refinery migrations (safe to run against an
  empty or partially-migrated database).
- Copies every table in dependency order (parents before children),
  preserving original primary-key IDs so foreign-key links like
  `round_telemetry.task_telemetry_id` stay intact.
- Idempotent: re-running after a partial/aborted migration is safe.
  Every insert uses `ON CONFLICT (...) DO NOTHING` so already-copied
  rows are skipped. Counts in the report are rows-read-from-source,
  not rows-newly-inserted.
- Bumps every `BIGSERIAL` sequence past the max copied ID so
  subsequent live writes don't collide with migrated IDs.
- Prints a per-table row count at the end.

Recommended workflow:

```bash
# 1. Stop the hub.
systemctl stop workforce-hub   # or equivalent

# 2. Back up the source file (optional but wise).
cp data/telemetry.db data/telemetry.db.bak

# 3. Run the migration.
workforce migrate-telemetry --config workforce.toml

# 4. Flip the config to point at Postgres.
#    Add to workforce.toml:
#        [telemetry]
#        backend = "postgres"

# 5. Start the hub. It will read/write against Postgres going forward.
systemctl start workforce-hub
```

The SQLite file is left in place and unmodified — the migration is
read-only on the source. If anything goes wrong you can flip
`backend` back to `"sqlite"` and you're back where you started.

## Building

The Postgres dependencies (`tokio-postgres`, `deadpool-postgres`,
`refinery`) live behind a `postgres` cargo feature on `workforce-common`.
The `workforce-cli` binary always enables this feature so any binary
shipped from a normal `cargo build` includes Postgres support. Library
consumers (`cargo build -p workforce-common`) get a slim build by
default.

## Testing

The Postgres backend's parity tests and the telemetry-migration test
run unconditionally against an **in-process embedded Postgres** via
the [`postgresql_embedded`](https://crates.io/crates/postgresql_embedded)
crate with the `bundled` feature. No Docker, no environment variables,
no manually-provisioned database.

```bash
cargo test -p workforce-common --features postgres --lib \
    telemetry_postgres::tests telemetry_migrate::tests
```

The first run downloads/initialises the platform-native Postgres
binary (~5–10s); subsequent runs reuse the cache and complete in under
a second. One shared server instance runs per test binary, with each
test creating a unique `test_db_<uuid>` database, running the refinery
migrations against it, executing the test, and dropping the database
on cleanup.

### Overriding with an external Postgres

Set `WORKFORCE_TEST_POSTGRES_URL` to a `postgres://user:pass@host:port/db`
URL and the test harnesses will use that server instead of spawning
`postgresql_embedded`. Each test still creates and drops its own
uniquely-named database against that server, so tests from multiple
crates and multiple runs don't collide.

Two situations where this matters:

- **CI** — Postgres tests are skipped on CI entirely via the
  `WORKFORCE_SKIP_POSTGRES_TESTS` env var (see below). The test job
  in `.github/workflows/pr-check.yml` sets the var so the workflow
  doesn't have to install or start Postgres at all. Local dev
  coverage is unchanged.
- **Local multi-crate runs** — running
  `cargo test --features .../postgres` across several crates in
  parallel spawns one embedded Postgres *per test binary*. On macOS
  the System V SHM limit is small and 3+ concurrent instances will
  exhaust it. Point the env var at a local Docker Postgres (or any
  other running server) to share one instance across all binaries:
  ```bash
  WORKFORCE_TEST_POSTGRES_URL=postgres://postgres:pass@localhost:5432/postgres \
      cargo test --features workforce-common/postgres,workforce-identity/postgres,workforce-pairing/postgres --workspace --lib postgres
  ```
  Single-crate runs (e.g. `cargo test -p workforce-common ...`) work
  fine with the embedded default.

### Skipping the Postgres tests entirely

Set `WORKFORCE_SKIP_POSTGRES_TESTS` to any value (CI uses `1`) and
every Postgres parity / migration test in the workspace returns
immediately with a `[postgres-test] ... skipping` log line. Use it
for:

- **CI**, where the overhead of installing and starting Postgres in
  every test job isn't worth it.
- **Quick local iteration on unrelated code**, when you don't want
  to wait for `postgresql_embedded` to download/start on first run.

The skip env var takes **precedence over** `WORKFORCE_TEST_POSTGRES_URL` —
if both are set, tests still skip. Unset it (or simply don't set it
in the first place) to run the tests normally.

> **Trade-off:** with the env var set on CI, regressions in
> `PostgresTelemetryStore` / `PostgresIdentityStore` /
> `PostgresPairingStore` / `PostgresCodeGraphBackend` are not caught
> by CI. Devs who edit any `*postgres*.rs` file should run the parity
> tests locally before pushing:
>
> ```
> cargo test --workspace --features workforce-common/postgres,workforce-identity/postgres,workforce-pairing/postgres,workforce-codebase-knowledge/postgres
> ```

## Operating in production

This section is a runbook for hub operators standing up workforce
against a real Postgres. Everything here assumes
`[postgres] enabled = true` and at least one per-store `backend =
"postgres"` flipped on.

### Verifying migrations ran

After hub startup, confirm refinery applied every migration cleanly:

```sql
SELECT version, name, applied_on
  FROM telemetry.refinery_schema_history
  ORDER BY version;
SELECT version, name, applied_on
  FROM identity.refinery_schema_history
  ORDER BY version;
SELECT version, name, applied_on
  FROM pairing.refinery_schema_history
  ORDER BY version;
SELECT version, name, applied_on
  FROM codegraph.refinery_schema_history
  ORDER BY version;
```

Each schema should show exactly the migrations listed in the
"Schema and migrations" section above. A missing row means the hub
either failed to start or fell back to SQLite for that store — check
the hub logs for `Postgres pool init failed` or `falling back to
SQLite`.

### Backups

There are no cross-schema foreign keys, so each schema can be
backed up and restored independently:

```bash
# Whole-database dump (recommended):
pg_dump --format=custom --file=workforce.dump workforce

# Per-schema (cherry-pick what you need):
pg_dump --format=custom --schema=telemetry --file=telemetry.dump workforce
pg_dump --format=custom --schema=identity  --file=identity.dump  workforce
pg_dump --format=custom --schema=pairing   --file=pairing.dump   workforce
pg_dump --format=custom --schema=codegraph --file=codegraph.dump workforce
```

Restore with `pg_restore --dbname=workforce <file>`. Schema order
doesn't matter — there are no inter-schema dependencies.

### Pool sizing

`[postgres] pool_size` is per *store* (each store builds its own
deadpool — see "Architectural note: per-store pools" above), so a
hub with all four stores on Postgres will hold up to
`4 × pool_size` connections at peak. With the default `pool_size =
16`, a single hub can use up to 64 connections.

Sizing guidance:

- A single hub with default settings is fine on a `max_connections =
  100` Postgres.
- Multiple hubs sharing one Postgres should size their pools so the
  *sum* across all hubs stays under `max_connections`, leaving
  headroom (~20%) for `psql`, monitoring, and migrations.
- Tune `pool_size` down on small setups (e.g. `pool_size = 4` for
  a low-traffic single hub) before raising `max_connections` on the
  Postgres side.

### Monitoring queries

Useful one-liners for spot-checking spend, latency, and audit
volume:

```sql
-- Total LLM cost over the last 24h, by model
SELECT model, ROUND(SUM(cost_usd)::numeric, 4) AS cost_usd
  FROM telemetry.cost_records
  WHERE timestamp >= NOW() - INTERVAL '24 hours'
  GROUP BY model
  ORDER BY cost_usd DESC;

-- Slowest task telemetry rows in the last hour
SELECT agent_id, model, duration_ms, rounds, completed_at
  FROM telemetry.task_telemetry
  WHERE completed_at >= NOW() - INTERVAL '1 hour'
  ORDER BY duration_ms DESC
  LIMIT 20;

-- Audit log volume by actor in the last 24h
SELECT actor, COUNT(*) AS events
  FROM telemetry.audit_log
  WHERE timestamp >= NOW() - INTERVAL '24 hours'
  GROUP BY actor
  ORDER BY events DESC;
```

### Secrets

Keep the Postgres password out of `workforce.toml` with `${VAR}`
expansion from a container secret or env file:

```toml
[postgres]
enabled = true
host = "db.internal"
database = "workforce"
user = "workforce"
password = "${WORKFORCE_PG_PASSWORD}"
```

`[postgres]` supports `${VAR}` expansion only, not `${vault:...}`: the
vault may itself live in Postgres (`[vault] backend = "postgres"`), so it
cannot hold the password needed to reach it.

### Rolling back to SQLite

The Postgres backends are strictly opt-in. To roll any store back to
SQLite:

1. Stop the hub.
2. Flip the relevant `backend` key back to `"sqlite"` in
   `workforce.toml` (or remove it — SQLite is the default).
3. Restart. The hub will read/write the local `.db` file again.

The Postgres data is left intact, so you can flip back later. The
SQLite file under `{hub.data_dir}` is still on disk from before the
migration unless you explicitly removed it — verify it's the version
you want before serving traffic.
