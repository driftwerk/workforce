# workforce-local-mcp

A local MCP (Model Context Protocol) server that gives Claude Code durable
memory across sessions, a fast code graph over your repos, and optional
read/write access to Jira, Linear, Datadog, Metabase, Postmark, and SendGrid — all
running on your machine, no cluster, no network calls except the ones you
explicitly enable.

Runs as a single static binary (~40 MB). All data stays under
`~/Library/Application Support/workforce-local/` (macOS) — SQLite
databases for the code graph and memories, an ONNX embedding model
cached on first use, and optional project configs. Credentials live in
the OS keychain, never in env vars or plaintext files.

## Install

### Homebrew (recommended)

```bash
brew tap driftwerk/tap
brew install workforce-local-mcp
```

See the [public tap](https://github.com/driftwerk/homebrew-tap) for
post-install caveats, platform coverage (macOS Apple Silicon + Linux
x86_64), and upgrade / uninstall instructions.

### From a release tarball (manual)

Release tarballs are attached to each
[GitHub Release on the tap](https://github.com/driftwerk/homebrew-tap/releases).
Pick the one for your platform, then:

```bash
VERSION=0.1.4
TARGET=aarch64-apple-darwin      # or x86_64-unknown-linux-gnu

curl -L -o workforce-local-mcp.tar.gz \
    "https://github.com/driftwerk/homebrew-tap/releases/download/local-mcp-v${VERSION}/workforce-local-mcp-${VERSION}-${TARGET}.tar.gz"

tar xzf workforce-local-mcp.tar.gz
cd "workforce-local-mcp-${VERSION}-${TARGET}"
./install.sh
```

`install.sh` copies the binary to `~/.local/bin`, copies the bundled
SKILL.md directories into `~/.claude/skills/`, strips the macOS
quarantine flag, and prints the `claude mcp add` command.

Verify with:
```bash
workforce-local-mcp --help
```

## Set up

### 1. Register the MCP server with Claude Code

```bash
# --scope user makes it available in every project, not just the
# current directory. Without this flag, Claude Code would only see
# the server when a session is started from within the dir where you
# ran the add command.
claude mcp add workforce-local --scope user -- "$(which workforce-local-mcp)"
claude mcp list    # should show workforce-local: … ✓ Connected
```

### 2. (Optional) Configure adapter credentials

Only needed if you want Jira / Linear / Datadog / Metabase / Postmark / SendGrid tools.
Everything else works without credentials.

```bash
workforce-local-mcp config jira \
    --email you@example.com \
    --site https://yoursite.atlassian.net
# Prompts for API token (hidden input)

workforce-local-mcp config linear
# Prompts for API key

workforce-local-mcp config datadog --site eu1
# Prompts for API key + application key

workforce-local-mcp config metabase --site-url https://mb.example.com
# Prompts for API key

workforce-local-mcp config postmark
# Prompts for server token (required, hidden) + account token (optional, hidden)

workforce-local-mcp config sendgrid --from-email noreply@example.com
# Prompts for API key (hidden); --from-email sets the default sender
```

All commands accept `--workspace <name>` to save the credentials under a
named workspace alongside existing ones, and `--set-default <name>` to
change which workspace is used when a tool call omits `workspace`.

Credentials are stored in the macOS Keychain under the service name
`workforce-local-mcp`. View with `workforce-local-mcp config show`
(secrets redacted as `****`). Remove with
`workforce-local-mcp config clear jira|linear|datadog|metabase|postmark|sendgrid|workforce|all`.

On the first write, macOS will prompt you to allow the binary to access
the keychain — click **Always Allow** so subsequent updates don't
re-prompt.

### 3. (Optional) Connect remote workforce hubs

The `wf.*` tools let Claude Code act on a workforce install as you:
author and run workflows, playbooks and WASM skills, manage vault
secrets and schedules, and chat with the install's agents. Each hub
hands you a personal token — run `cmd: remote-mcp token` in Slack or
web chat, or use the "Remote MCP tokens" card in My Settings — and the
reply includes the exact line to paste here:

```bash
workforce-local-mcp config workforce add myhub --url https://hub.example.com
# Prompts for the wfr_… token (hidden). The first hub becomes active.
workforce-local-mcp config workforce add harbor --url https://workforce.harbor.example
workforce-local-mcp config workforce list
workforce-local-mcp config workforce use harbor   # or call the `wf.use` tool in-session
workforce-local-mcp config workforce remove harbor
```

One hub is active at a time. Its tools appear as `wf.<name>` (e.g.
`wf.workflow`, `wf.vault_set`, `wf.agent_chat`) under stable names, so
switching hubs with `wf.use` swaps what they point at without renaming
anything; the server tells Claude Code the tool list changed. `wf.hubs`
shows which hub is active and whether it is connected. The token is
stored alongside the adapter credentials (keychain / secrets file) and
is removed with `config workforce remove <name>` or
`config clear workforce`.

### 4. Try it

Open a fresh Claude Code session inside a repo you want to index:

```bash
cd ~/some/repo
claude
```

In the session, type `/mcp` — you should see `workforce-local` connected.
Then try:

- > Index this codebase and tell me what the main entry point is.
- > Remember that we picked SQLite over DuckDB because of concurrent
  > writer restart limitations.
- > What do I know about this project's storage choices?  _(new session)_
- > How does the indexer decide what to parse? (design-shaped —
  > Claude will use `context.fuse` to cross-reference code + notes)

## Tools

The server exposes 14 core tools unconditionally, plus up to 121 adapter
tools when all credentials are configured. Counts below show what's
available when everything's set up; adapters with no credentials
contribute zero tools (so `tools/list` stays clean).

| Namespace | Count | What it does |
|-|-|-|
| `codegraph.*`  | 5  | Index a repo, find symbols, walk the call graph |
| `memory.*`     | 4  | Store / recall notes across sessions |
| `project.*`    | 2  | Manage multi-repo projects (shared graph) |
| `impact.*`     | 1  | BFS across the shared graph for impact analysis |
| `context.fuse` | 1  | Concurrent code + memory fan-out in one call |
| `jira.*`       | 18 | Full Jira REST coverage (search, CRUD, transitions, …) |
| `linear.*`     | 22 | Full Linear GraphQL coverage |
| `datadog.*`    | 31 | Logs, metrics, monitors, incidents, dashboards |
| `metabase.*`   | 22 | Query cards/dashboards, run native SQL, manage collections |
| `postmark.*`   | 28 | Transactional send, message status, bounces, stats, templates, suppressions |
| `sendgrid.*`   | 16 | Transactional send, email activity, stats, templates, senders, suppressions |
| `wf.hubs`, `wf.use` | 2 | Inspect / switch the active remote workforce hub |
| `wf.*`         | varies | The active hub's remote MCP tools (workflows, playbooks, skills, vault, schedules, `agent_chat`) |
| `ping`         | 1  | Liveness check |

Run `/mcp` in Claude Code (or send a `tools/list` over stdio) for the
complete, schema-annotated catalog.

## Highlights

### Local semantic memory

Memories are stored in SQLite with FTS5 keyword search plus 384-dim
vector embeddings (BGE-small-en-v1.5 via `fastembed-rs`). Hybrid scoring
(60% vector, 40% BM25) means paraphrased queries still hit relevant
notes — e.g. a query for "payment provider fault tolerance throttling"
finds a stored note about "circuit breaker… Stripe 5xx responses" with
zero keyword overlap.

Disable embeddings for the fastest startup (keyword-only recall) via
`WORKFORCE_LOCAL_EMBEDDINGS=off`. The model is ~130 MB and downloads on
first use to `<data_dir>/embeddings/`.

### Tree-sitter code graph

Indexes Rust, TypeScript, JavaScript, Python, Go, Ruby, and Kotlin. Call
graph extracted heuristically from the AST. Stored in SQLite; typical
100 k-file repo indexes in ~1 second per 1000 files.

### Multi-repo projects

Declare a project as a TOML file under
`~/Library/Application Support/workforce-local/projects/`:

```toml
name = "myproject"
[[repos]]
name = "backend"
path = "/path/to/backend-repo"
[[repos]]
name = "web"
path = "/path/to/frontend-repo"
```

Then `project.index myproject` writes every repo into a shared code
graph. `impact.cross_repo entity_id="..."` and
`context.fuse project="myproject"` operate across the whole project.

### `context.fuse`: one call, everything

The highest-leverage tool. Takes a natural-language query, runs the code
graph search and memory recall *concurrently*, returns a unified
payload:

```json
{
  "query": "how is indexing serialised per repo",
  "repo_id": "workforce-abcd1234",
  "code_matches":   [ ... top entities ... ],
  "graph_context":  { "answer": "...", "suggestion": "..." },
  "memory_context": [ ... relevant notes ... ],
  "confidence": 0.56,
  "freshness":  "fresh",
  "duration_ms": 12
}
```

## Where things live

All under `~/Library/Application Support/workforce-local/`:

| Path | What |
|-|-|
| `.workforce/knowledge/codegraph.db` | SQLite code graph |
| `memory.db`                         | SQLite FTS5 memory store |
| `embeddings/`                       | BGE-small ONNX model cache |
| `projects/`                         | Project TOML files (optional) |

Credentials under macOS Keychain service `workforce-local-mcp`.

Override with env vars: `WORKFORCE_LOCAL_DATA_DIR`,
`WORKFORCE_LOCAL_WORKSPACE`, `WORKFORCE_LOCAL_PROJECTS_DIR`,
`WORKFORCE_LOCAL_EMBEDDINGS=off`.

## Troubleshooting

**"not opened because the developer cannot be verified"** — shouldn't
happen for official release builds; they're signed with a Developer ID
Application cert and notarized by Apple. If you see this message:
- You're running a binary built locally (`cargo install --git` or
  `cargo build`) rather than a release download. Local builds are
  unsigned by design.
- Your first-run Gatekeeper check couldn't reach Apple's servers to
  verify notarization. Connect to the internet and retry.

Escape hatch either way:
```bash
xattr -d com.apple.quarantine ~/.local/bin/workforce-local-mcp
```
The install script runs this automatically.

**"User canceled the operation" during `config jira`** — you denied the
Keychain prompt. Re-run and click **Always Allow**. The "Always Allow"
persists per binary signature, so it'll ask again on each upgrade.

**Memory recall misses paraphrased queries** — check
`WORKFORCE_LOCAL_EMBEDDINGS` isn't set to `off`. First use downloads
a ~130 MB ONNX model; watch
`~/Library/Application Support/workforce-local/embeddings/` fill up.

**Claude Code doesn't pick the tools you expect** — tighten the
descriptions in the bundled `skills/<name>/SKILL.md` frontmatter.
Symlink them into `~/.claude/skills/` so Claude reads them at session
start.

### Reset

Full nuke:
```bash
rm -rf ~/Library/Application\ Support/workforce-local
workforce-local-mcp config clear all
```

### Talk to the server without Claude Code

```bash
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | workforce-local-mcp 2>/dev/null \
  | tail -1 \
  | jq '.result.tools | length'
```

That prints the number of tools the server exposes — a quick
sanity check that the server, skills, and credentials are all wired up.

## Status & caveats

- **Apple Silicon only** today (`aarch64-apple-darwin`). The
  `fastembed` embedding backend pulls in ONNX runtime via `ort`, which
  doesn't ship a prebuilt x86_64 macOS binary — cross-compiling the
  runtime from source wasn't worth the CI fragility for the shrinking
  Intel-Mac audience. Intel users can still build from source
  (`cargo install --git …`) if ONNX runtime is otherwise available
  locally. If anyone actively needs Intel releases, file an issue —
  workaround is to switch fastembed to its pure-Rust `ort-tract`
  backend.
- **Linux** is plausible (the `keyring` crate covers Secret Service)
  but untested in CI.
- **Official releases are signed and notarized** with a Developer ID
  Application cert. Source builds (`cargo install --git`,
  `cargo build`) are unsigned and may require
  `xattr -d com.apple.quarantine …` on first run.
- **Adapter coverage is full** for Jira / Linear / Datadog / Metabase /
  Postmark / SendGrid today. Adding new adapters means adding them to
  `workforce-skills` in the main workforce repo and rebuilding.
- **Cross-repo HTTP boundary extraction** (Rails routes ↔ React fetch
  calls, etc.) is planned but not shipped — `impact.cross_repo` today
  only walks call-graph edges within a single repo.

## Maintainer: cutting a release

The `.github/workflows/release.yml` workflow ships signed + notarized
macOS binaries. It needs these repo secrets set (once):

| Secret | What |
|-|-|
| `APPLE_CERTIFICATE` | base64-encoded Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password for the `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: <Name> (TEAMID)` |
| `APPLE_ID` | Apple ID email |
| `APPLE_PASSWORD` | app-specific password (from appleid.apple.com) |
| `APPLE_TEAM_ID` | e.g. `ABCDE12345` |

With those set, tag and push:

```bash
git tag local-mcp-v0.1.0
git push origin local-mcp-v0.1.0
```

The workflow builds on `macos-14` (ARM64) and `macos-13` (Intel) in
parallel, signs the binary with a hardened runtime + timestamp,
submits to Apple's notary service (blocks until accepted), and
uploads a GitHub Release with the two tarballs + sha256 sums. Bare
CLI binaries can't be stapled — Gatekeeper verifies notarization
online on first run.

If the secrets aren't set, the workflow skips signing / notarization
and ships unsigned tarballs. That's the mode for manual dispatch test
runs.

## License

MIT. See [LICENSE](../../LICENSE) in the workforce repo root.
