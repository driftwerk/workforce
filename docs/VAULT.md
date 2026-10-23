# Secrets Vault

The secrets vault provides AES-256-GCM encrypted storage for sensitive values like API tokens, passwords, and credentials. Secrets are encrypted by the hub before they are stored — in an encrypted file per vault, or as encrypted rows in the shared Postgres database — and decrypted in memory at runtime. The vault integrates with skill config files (`${vault:path}` references) and exposes tools that agents can call directly.

## Quick Start

```bash
# Initialize vault (creates vault.enc + vault-key.txt)
workforce vault init

# Store a secret
workforce vault set slack.bot_token xoxb-123456789

# Retrieve it
workforce vault get slack.bot_token
```

## Configuration

```toml
[vault]
enabled = true
# path = "./data/vault.enc"          # default: {data_dir}/vault.enc
# key_file = "./data/vault-key.txt"  # default: {data_dir}/vault-key.txt
# backend = "file"                   # "file" (default) or "postgres"
# sync_interval_ms = 1000            # postgres: how often peers' changes are picked up
# audit_retention_days = 365         # postgres: audit records older than this are pruned
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `true` | Whether to open the hub-level (global) vault |
| `path` | `{data_dir}/vault.enc` | The global vault's file (file backend; also where `vault migrate` imports it from) |
| `key_file` | `{data_dir}/vault-key.txt` | Master key file |
| `backend` | `"file"` | Where vaults are stored: `"file"` or `"postgres"` |
| `sync_interval_ms` | `1000` | Postgres: interval at which each process polls for other processes' writes |
| `audit_retention_days` | `365` | Postgres: audit records older than this are deleted |

Agents may override `enabled`, `path` and `key_file` per agent under `[[agents]] vault.*`. `backend` is structural: it cannot be overridden by an agent, workspace config or overlay.

## Storage Backends

### File (default)

Each vault is one encrypted file: the global vault at `[vault] path`, each agent's at `{workspace}/vault.enc`. The file is decrypted into memory when opened and rewritten whole on every write.

Before each write, a process reloads the file if another process has replaced it, so it doesn't overwrite that change. But its reads stay as they were loaded until it next writes or syncs, and two writes at the same moment still lose one. So **one process should write a given file**; to run several hubs, use Postgres. Within a process, every opener of a file shares one in-memory vault.

### Postgres

`backend = "postgres"` keeps every vault as encrypted rows in the shared database (schema `vault`), so any number of hub processes can serve the same vaults:

- Writes are per-key transactions — no process can overwrite another's change.
- Each process keeps a decrypted cache of the vaults it has open and polls `vault_scopes.generation` every `sync_interval_ms`; a process sees its own writes immediately and other processes' within about a second.
- OAuth refreshes are single-flight across every process (a Postgres advisory lock), so a rotating refresh token is never spent twice.
- Every change is audited in the same transaction.

It requires `[postgres] enabled = true` and fails closed: an unreachable database, a missing passphrase or a passphrase that doesn't match the vault keyring stops the hub from starting rather than falling back to files. Existing vault files are moved with `workforce vault migrate` — see [Troubleshooting](#troubleshooting). A hub on Postgres refuses to open a vault whose file still exists on the host but was never imported:

- For the global vault it refuses to start.
- For an agent's own vault that agent runs without its vault, and the hub logs the fix.

The Postgres password in `[postgres]` supports `${VAR}` expansion only — it cannot come from the vault, which may itself live in Postgres.

## Master Key

The master key is resolved in this order:

1. **Environment variable** `WORKFORCE_VAULT_KEY` (highest priority)
2. **Key file** at the configured `key_file` path (default: `{data_dir}/vault-key.txt`)

With the file backend, a missing key is generated and saved to the key file. With the Postgres backend a key is **never** generated — several processes booting at once would each mint a different one — so a missing key is a startup error. The Postgres store takes its key from `[vault]` even when `[vault] enabled = false`, since agents' vaults still need it.

### Key File

`workforce vault init` generates a random 32-byte base64-encoded master key and writes it to `vault-key.txt` with mode `0600` (owner read/write only on Unix). Keep this file secure — anyone with the key can decrypt the vault.

### Environment Variable

For containerised deployments, pass the key via environment variable:

```bash
WORKFORCE_VAULT_KEY=<base64-encoded-key>
```

This takes precedence over the key file.

## Layered Resolution (Scoping)

Beyond flat key/value storage, the vault resolves a *logical* path across up to
four **tiers** so the same credential can be set org-wide, per-agent, or
per-user. There are two physical vaults — the **agent vault** (an agent's own
`vault.enc`) and the optional **global vault** (the hub-wide vault the My
Settings UI writes to) — and an optional per-user namespace (`users/{uid}/…`)
within each. That yields four slots for any logical path `P`:

| # | Tier (`VaultSource`) | Physical key | Set by |
|---|----------------------|--------------|--------|
| 1 | `AgentPerUser` | `agent_vault` → `users/{uid}/P` | a per-user override on the agent |
| 2 | `GlobalPerUser` | `global_vault` → `users/{uid}/P` | the My Settings UI (per-user) |
| 3 | `AgentSpecific` | `agent_vault` → `P` | an agent's shared credential |
| 4 | `Global` | `global_vault` → `P` | an org-wide default |

The per-user tiers (1, 2) only exist when an identity is in scope
(`CURRENT_USER_ID`, set for Slack/OIDC requests). System jobs with no identity
see only the shared tiers (3, 4). In single-vault deployments (no global vault)
tiers 2 and 4 transparently collapse onto the agent vault.

A per-user entry always records its owner as `created_by` (`user:<uid>`) and
the real writer as `updated_by`; the UI addresses per-user entries through
`created_by`.

### Two resolution orders

There are **two** resolution functions, and they order the *per-user* tiers
differently. This is intentional — pin it, don't "fix" one into the other (see
`scoping_matrix_tests` in `user_config.rs`).

**1. Raw `SecretStore::get` / `get_layered`** — strict tier order:

```
AgentPerUser (1) → GlobalPerUser (2) → AgentSpecific (3) → Global (4)
```

Used by MCP `PatAuth`, the `check_skill_credentials` "configured?" probe, and
WASM scoped reads. Defined once in `user_config::layered_tiers`.

**2. Skill credential reads** — `resolve_workspace_or_flat`
(`crates/skills/src/adapters/auth.rs`), used by every built-in adapter (Jira,
GitHub, Linear, Datadog, …), and its single-form twin `user_config::resolve_flat`
(used by the Claude OAuth token). These check the **per-user overlay first**,
and within per-user prefer the **global** vault (so a token set in the My
Settings UI always wins):

```
per-user·global (2) → per-user·agent (1) → agent-shared (3) → global-shared (4)
```

i.e. for a credential a *skill* consumes, the effective priority is **slot 2 >
slot 1 > slot 3 > slot 4** — per-user always beats shared, and within per-user,
global beats agent. The divergence from raw `get` only surfaces when the *same*
per-user token exists in both vaults at once.

`resolve_github_pat` (`user_config.rs`) is the GitHub-specific application of
this: it resolves `skills/github/workspaces/{owner}/token` then
`skills/github/token`, each across the per-user-then-shared ladder.

### Cross-form precedence (workspace vs flat)

Skill credentials have two shapes — a **workspace** form
(`skills/jira/workspaces/{name}/api_token`) and a legacy **flat** form
(`skills/jira/api_token`). `resolve_workspace_or_flat` ranks **all per-user
tiers (either form) above all shared tiers**, preferring the workspace form
within each band:

```
ws·user-global → ws·user-agent → flat·user-global → flat·user-agent
  → ws·agent-shared → ws·global-shared → flat·agent-shared → flat·global-shared
```

This closes the "cross-form shadow trap": a per-user *flat* override can no
longer be masked by an agent-vault *workspace* value.

### Writes

The legacy `SecretStore::set` defaults to **tier 2 (`GlobalPerUser`)** when an
identity is in scope (so per-user creds are agent-portable) and **tier 3
(`AgentSpecific`)** for system contexts. Callers wanting an explicit tier use
`set_at_tier(path, value, VaultSource::…)`. The `vault.set` agent tool writes to
the caller's user scope by default; pass `scope='agent_shared'` for a shared
credential.

`UserAwareVault` writes land in the user's own namespace
(`users/{uid}/…`); paths that already carry that user's prefix pass through
unchanged, and no view reaches another user's `users/…` entries.
Its strict view (`for_user_strict`) never falls back to the shared path — MCP
servers with a per-user binding use it, so one user is never handed another
user's token.

Several keys written together (`SecretStore::set_many`, used for OAuth token
rotation) and moves between tiers (`SecretStore::move_at_tier`) are a single
transaction on Postgres and a single file write on the file backend. A move
between two different vault files writes the destination first, so a failure
leaves a copy, never a loss. Moves and renames sync their vaults first, so
they move the latest value any process wrote and see a destination another
process just created.

### Debugging shadowing

`vault.inspect` (agent tool) shows the resolved entry **and** any
lower-tier entries being shadowed, which is the fastest way to diagnose "I set a
credential but the wrong one is being used" — almost always a higher tier
holding a stale value. A guardrail test (`no_raw_vault_github_token_reads_in_hub`)
also rejects new code that reads GitHub tokens off a raw `Vault` instead of
through these resolvers.

### Integration credentials

Keys that are not a skill's — decision backends, the embeddings endpoint, and any
integration added later — are declared once in `crates/hub/src/credentials.rs` as a
`VaultCredential` (path, catalog label, description, scope). That one descriptor is both
the entry in the vault editor's dropdown (`GET /api/vault/catalog`) and what the reader
resolves, so the two never disagree. Readers call `credentials::Caller::resolve` on every
use, never at boot, which is the skill-read order above for `either`/`per_user` scopes
and the shared tiers alone (3, 4) for `shared` ones. The caller's person is explicit —
none for system work, which then sees only shared tiers even inside an outer identity —
and the agent and global vaults come from the service the work runs in.

| Path | Scope | Read by |
|------|-------|---------|
| `decision/jev/api_key` | either (all four tiers; members may add their own) | the Jev decision backend, per request, for the person in scope |
| `decision/kev/api_key` | shared | the Kev decision backend (its server's `KEV_API_KEY`) |
| `embeddings/openai/api_key` | shared, global vault only (org-wide prefix) | the `[embeddings] provider = "openai"` embedder, per request |
| `embeddings/memory/api_key` | shared, global vault only (org-wide prefix) | the SQLite memory store's `[memory] embedding_provider = "openai"` embedder, per request |
| `tools/brave_api_key` | either (all four tiers; members may add their own) | `web.search`, per call, for the person in scope; with no key the call fails naming this path |
| `vision/<provider>/api_key` (anthropic, openrouter, gemini) | either | the `[llm.vision]` describer, per description, for the person whose turn it is |
| `transcription/<provider>/api_key` (openai, groq, gemini, openrouter) | either | the `[llm.transcription]` transcriber, per transcription |
| `image/<provider>/api_key` (gemini, openrouter) | either | `generate_image`, per request |
| `video/gemini/api_key` | either | `generate_video` (Veo), once per clip |
| `speech/<provider>/api_key` (fish_audio, elevenlabs, openai, soniox) | either | `generate_speech`, per clip |
| `knowledge/summariser/api_key` | shared, global vault only | the knowledge summariser, per summary |

The media rows are one credential per modality and provider
(`credentials::Modality`): the reader picks the path from the provider the agent's
`[llm.*]` config routes that modality to, so a key is only ever sent to the provider
that issued it. Providers that take no bearer key (Bedrock and Vertex credential chains,
Ollama and vLLM endpoints, subscription logins) have no entry and are built as before.
Each reader's pre-vault source (`credentials::Legacy`: the config `api_key`, the
modality's and provider's environment variables, an `llm/accounts/*` entry) is read
after the vault, in the order that reader always used. The description and transcript
caches are kept apart by the key that paid, and a person with no key is refused before
the cache is read;

Any other `[[decision.backends]]` name reads `decision/<name>/api_key` from the shared
tiers. A decision backend takes a keyless request only when it is on a private network
(or `requires_key = false` says so); a caller with no key is otherwise not sent to it.
Members may save any catalogued credential whose scope allows per-user values; nothing
else needs listing for that. A key rotated in the vault is used on the next request; the install config's
`api_key` / the environment variable remains only as the last resort for installs that
predate the vault entry.

## CLI Commands

Local commands act on the file in `--data-dir <path>` (default: `./data`). With
`--config <workforce.toml>` they act on the configured backend instead — the
global vault, or an agent's own vault with `--agent <id>`. Direct writes to a
vault *file* while a hub runs are refused unless
`WORKFORCE_ALLOW_LOCAL_VAULT_WRITE=1`; writes to the Postgres backend are safe
while hubs run. `--gateway <url> --token <token>` acts on a running hub over
its API instead.

### `vault init`

Creates a new vault file and master key file.

```bash
workforce vault init
workforce vault init --data-dir /app/data
```

- Generates a random master key and writes it to `vault-key.txt` (mode 0600)
- Creates an empty encrypted `vault.enc`
- Skips either file if it already exists

### `vault set`

Store a secret by dot-notation path.

```bash
# Inline value
workforce vault set slack.bot_token xoxb-123456789

# Read from stdin (useful for piping)
echo "xoxb-123456789" | workforce vault set slack.bot_token
cat /path/to/token.txt | workforce vault set github.pat

# The configured backend, one agent's vault
workforce vault set skills/jira/api_token abc --config workforce.toml --agent sarah
```

Overwrites if the path already exists.

### `vault get`

Retrieve a secret value.

```bash
workforce vault get slack.bot_token
# Output: xoxb-123456789
```

Exits with code 1 if the path is not found.

### `vault list`

List all secret paths (never shows values).

```bash
# List all
workforce vault list

# Filter by prefix
workforce vault list slack.
```

### `vault delete`

Remove a secret.

```bash
workforce vault delete slack.bot_token
```

Exits with code 1 if the path is not found.

### `vault rotate-key`

File backend (`--data-dir`): re-encrypt the vault file with a new randomly
generated master key and write the new key to `vault-key.txt`. Update
`WORKFORCE_VAULT_KEY` afterwards if you use it, and restart every hub using the
file: a hub still on the old key can't read the re-keyed file, so it refuses
vault writes (rather than overwriting it) until it restarts.

Postgres backend (`--config`): seal every secret under a new data key in one
transaction. Audit records keep the key they were written with; that key is
kept (wrapped) until retention prunes its last record. To also move to a new
passphrase, pass `--new-key-file <file>` (or set `WORKFORCE_VAULT_NEW_KEY`): the
new data key and every retained key are wrapped under both passphrases, so
hubs keep working while the new passphrase rolls out. `--config` needs
`[vault] backend = "postgres"`. A rotation is refused while another passphrase
still opens the vault key (a passphrase change in progress), unless it rotates
onto that passphrase; finish the change with `retire-key` first.

```bash
workforce vault rotate-key --config workforce.toml
workforce vault rotate-key --config workforce.toml --new-key-file /run/secrets/new-vault-key
```

### `vault retire-key`

After every hub runs with the new passphrase: delete every key wrap the
configured passphrase cannot open, so only it opens the vault. Refuses if that
would leave the active key without a wrap.

```bash
workforce vault retire-key --config workforce.toml
```

### `vault migrate`

Import every vault file into Postgres. The files are frozen first (hubs on the
file backend then refuse vault writes), then each is imported in its own
transaction that commits only if the file is unchanged. Every file is
decrypted before anything is frozen, so a wrong key stops the migration with
nothing changed. Safe to re-run: vaults already on Postgres are skipped.
`--dry-run` decrypts and counts without freezing or importing.

A hub on Postgres may already have created a vault empty, because its file
wasn't on that host. If that vault is still empty, it is imported. If the hub
has stored secrets in it, the vault is reported as NOT imported, neither side
changes, and the command exits non-zero. The cutover runbook's Troubleshooting
section has the fix.

```bash
workforce vault migrate --config workforce.toml --dry-run
workforce vault migrate --config workforce.toml
```

The vaults covered are the global vault, every configured agent's (the implicit
`default` agent's when none are configured), and any `vault.enc` left under
`{data_dir}/agents/` by an agent no longer configured.

### `vault export`

Rollback: freeze the Postgres vaults, write each back to its file, append the
audit records written in Postgres to each file's log, and unfreeze the files.
Then set `backend = "file"` and redeploy.

### `vault verify`

Compare every vault file with its Postgres copy — paths only, never values.
Flags a frozen file that changed after it was imported (a write that got past
the freeze) and vaults that were never imported; exits non-zero for the first,
and for the second while the config selects Postgres.

### `vault audit`

Show who changed which path and when — never values — for the global vault or
`--agent <id>`'s, from whichever backend is configured.

```bash
workforce vault audit --config workforce.toml --since 2026-09-01T00:00:00Z --limit 20
```

## Skill Config Integration

Skill config files (`config.toml`) can reference vault secrets using `${vault:path}` syntax:

```toml
# skills/slack-integration/config.toml

[[workspaces]]
id = "default"
bot_token = "${vault:slack.bot_token}"
app_token = "${vault:slack.app_token}"

response_mode = "mention_only"
thread_isolation = true
```

### Expansion Order

When a skill config is loaded:

1. **Vault references** (`${vault:path}`) are expanded first
2. **Environment variables** (`${VAR}` or `${VAR:-default}`) are expanded second

This means you can mix both in the same config file:

```toml
# Vault for sensitive tokens
bot_token = "${vault:slack.bot_token}"

# Env var for non-sensitive config
log_level = "${SLACK_LOG_LEVEL:-info}"

# Env var with fallback default
webhook_url = "${WEBHOOK_URL:-https://hooks.example.com}"
```

If a vault path is not found, it resolves to an empty string.

### Supported Patterns

| Pattern | Description |
|---------|-------------|
| `${vault:slack.bot_token}` | Resolve from vault |
| `${SLACK_BOT_TOKEN}` | Resolve from environment |
| `${SLACK_BOT_TOKEN:-fallback}` | Environment with default |

Vault path names support alphanumeric characters, underscores, and dots: `[a-zA-Z0-9_.]`.

## Agent Tools

When `vault.enabled = true`, four tools are registered for agents to call at runtime.

### `vault.get`

Retrieve a secret by path.

**Input:**
```json
{ "path": "slack.bot_token" }
```

**Success response:**
```json
{ "path": "slack.bot_token", "value": "xoxb-123456789" }
```

**Failure response:** `Secret 'slack.bot_token' not found`

### `vault.set`

Store a secret.

**Input:**
```json
{ "path": "slack.bot_token", "value": "xoxb-123456789" }
```

**Response:**
```json
{ "path": "slack.bot_token", "stored": true }
```

Changes are encrypted and persisted immediately.

### `vault.list`

List secret paths (never exposes values).

**Input:**
```json
{ "prefix": "slack." }
```

`prefix` is optional. Omit it to list all paths.

**Response:**
```json
{ "paths": ["slack.app_token", "slack.bot_token"], "count": 2 }
```

### `vault.delete`

Delete a secret by path.

**Input:**
```json
{ "path": "slack.bot_token" }
```

**Success response:**
```json
{ "path": "slack.bot_token", "deleted": true }
```

**Failure response:** `Secret 'slack.bot_token' not found`

## Encryption Details

### File backend

- **Cipher:** AES-256-GCM
- **Key derivation:** Argon2id (default cost) from the master key
- **Salt:** 16 bytes, random per vault file
- **Nonce:** 12 bytes, random per write

The file is `magic "WFV\x02" (4) || salt (16) || nonce (12) || ciphertext + GCM tag`;
the plaintext is the JSON map of path → entry (value, `created_at`,
`updated_at`, `created_by`, `updated_by`). Legacy v1 files (no magic, plaintext
path → value) open with `legacy` metadata and are written as v2 on the next
write. Every write goes through a uniquely named `0600` temp file that is
fsynced, renamed over the vault file, then the directory is fsynced — readers
only ever see the old or the new file, and memory changes only once the file
has. A `<file>.frozen` marker (written by `vault migrate`) makes the file
refuse writes. Each file's audit log is `<name>-audit.jsonl` beside it
(`vault.enc` → `vault-audit.jsonl`).

### Postgres backend

- **Key hierarchy:** the master key (passphrase) → Argon2id → a key-encryption
  key (KEK) that wraps a random 256-bit data key (DEK). The DEK exists in the
  database only wrapped; several wraps per DEK allow a rolling passphrase
  change. HKDF-SHA256 derives the row cipher key and a lookup MAC key from the
  DEK.
- **Rows:** the path is never stored — rows are looked up by
  `HMAC-SHA256(mac_key, scope ‖ 0 ‖ path)`. The sealed plaintext is the path plus
  its entry; AES-256-GCM authenticates it with `key id ‖ scope ‖ path MAC` as
  associated data, and the decrypted path must reproduce the lookup MAC. A row
  moved to another path, vault or key fails to open.
- **Audit:** each record (actor, action, path, value fingerprints) is sealed the
  same way; only its vault and timestamp are in the clear.
- **What the database sees:** vault ids (`global`, `agent:<id>`), generation
  counters, audit timestamps, row counts and ciphertext sizes. Someone who can
  read the database learns no paths or values. Someone who can write it can
  delete rows or replay an older ciphertext of the same row, but cannot read,
  forge or move one.

### Concurrency

- File backend: reads are concurrent; writes to one file are serialised within
  the process. Across processes, a write first reloads a file another process
  replaced, but two simultaneous writes are last-writer-wins.
- Postgres backend: each commit locks its vaults' rows in a fixed order, so
  concurrent writers in any process never lose each other's changes and never
  deadlock. A cache only installs a newer generation, so a slow reload never
  overwrites fresher state.

## Security Considerations

- **Key file permissions:** Created with mode `0600` — ensure this is maintained. Do not commit key files to version control.
- **Environment variable:** `WORKFORCE_VAULT_KEY` is readable by any process in the same environment. Use container secrets or mounted files in production where possible.
- **In-memory secrets:** Vaults are decrypted in memory while the hub is running. Secrets are accessible to any code in the process.
- **Keep the key away from database backups:** with the Postgres backend a database backup plus the master key is enough to read every secret. Store them separately.
- **Database access:** do not point a `database.*` connection at the hub's own database with a role that can read the `vault` schema. The rows are ciphertext, but there is no reason to expose them.
- **No secret expiration:** The vault does not enforce rotation policies. Rotate secrets and keys on your own schedule.
- **Backup:** Back up the master key with either backend, plus the vault files (file backend) or the database (Postgres backend). Without the key, nothing can be decrypted.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `vault migrate`: "… kept changing during the import" | A process without the freeze is writing that file (an old-build replica, or a host CLI) | Find it and stop or upgrade it, then re-run `vault migrate` |
| New tasks exit: "vault `global` still lives in …, which was never imported" | The config was flipped before `vault migrate` ran (or it ran against another data dir) | Run `vault migrate`; the old tasks keep serving until the new ones are healthy |
| "the configured vault passphrase does not open the active vault key" | This task's passphrase differs from the one that created the keyring | Fix `WORKFORCE_VAULT_KEY` or the key file and redeploy |
| Log: "Agent vault not opened; the agent runs without its own secrets" | That agent's `vault.enc` was never imported (for example an agent re-created after cutover, whose workspace still holds an old vault file) | Run `vault migrate` (safe while hubs run), then restart the hub |
| Every vault write answers 503 long after the window | Files still frozen (the flip never completed), or Postgres frozen (after an export) | Complete the rollout, or run `vault export` / `vault migrate` to make the side you run on writable |
| `vault verify`: "changed after it was imported" | A write reached a file past the freeze | Its paths are listed; see who wrote them with `workforce vault audit` and re-apply them through the UI or CLI on Postgres |
| **Postgres already holds secrets**: `vault migrate` marks a vault "NOT imported: Postgres already holds N secret(s) a hub wrote after creating this vault empty" | A hub on Postgres opened this vault on a host without its file, created it empty, and has stored secrets in it since. Neither side was changed. | `workforce vault verify --config …` lists the paths on each side. If the Postgres-side secrets can be recreated (usually OAuth tokens), delete each path listed under "only in Postgres" or "different" with `workforce vault delete <path> --config … [--agent <id>]`. Then re-run `vault migrate`, which imports the file once that vault is empty. Finally, reconnect or re-enter anything you still need. |
| `vault rotate-key`: "refusing to rotate: another passphrase also opens the vault key" | A passphrase change is still rolling out | Finish it (every hub on the new passphrase, then `vault retire-key`) and rotate again, or rotate onto that passphrase with `--new-key-file` |

## Key rotation (Postgres backend)

- **Same passphrase, new data key:** `workforce vault rotate-key --config /app/workforce.toml`. Every secret is re-sealed in one transaction; writes pause for its duration.
- **New passphrase:**
  1. Put the new passphrase in the secret store alongside the old one.
  2. Run `workforce vault rotate-key --config /app/workforce.toml --new-key-file <file>` (or set `WORKFORCE_VAULT_NEW_KEY`). Both passphrases then open the vault.
  3. Roll every hub onto the new passphrase.
  4. Run `workforce vault retire-key --config /app/workforce.toml` with the new passphrase configured.

  Between steps 2 and 4, a plain `rotate-key` is refused: it would lock out whichever passphrase it didn't wrap the new key for.

