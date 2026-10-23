---
name: Adapter Integrations
description: Use when the user asks about Jira issues, Linear tickets, Datadog logs/monitors/metrics, or Metabase cards/dashboards/queries. Backed by the workforce-local-mcp server's jira.*, linear.*, datadog.*, and metabase.* tools which reuse the workforce-skills adapters. Each adapter only activates when its credentials are set — configure them with `workforce-local-mcp config <adapter>`.
---

# Adapter Integrations

Four third-party integrations exposed as MCP tools, forwarding to the
same workforce-skills adapters the hub uses. Each adapter is
independently optional — it only shows up as callable when its
credentials are present at server startup.

## One-time setup

Credentials live in the OS keychain on macOS / Windows and in a
mode-0600 TOML file on Linux (incl. WSL), not in environment
variables or config files. Set them with the binary's `config`
subcommand:

```bash
# Jira Cloud
workforce-local-mcp config jira \
    --email you@example.com \
    --site https://yoursite.atlassian.net
# Interactive prompt: "Jira API token:" (hidden input)

# Linear
workforce-local-mcp config linear
# Prompts for API key; optional --default-team-id flag

# Datadog
workforce-local-mcp config datadog --site eu1
# Prompts for API key and Application key

# Metabase
workforce-local-mcp config metabase --site-url https://mb.yourcompany.com
# Prompts for API key (mb_...). Optional --max-query-rows for the row cap
# applied to query tools (default 2000, hard max 2000).
```

Verify with `workforce-local-mcp config show` (secrets redacted as
`****`). Remove individual adapters with `config clear jira` etc., or
wipe everything with `config clear all`.

### Multiple workspaces per adapter

All four adapters support as many workspaces / accounts / instances
as you want — one set of creds per Linear workspace, per Jira
Cloud site, per Datadog account, per Metabase instance. Add
them with `--workspace <name>`:

```bash
workforce-local-mcp config linear --workspace work
workforce-local-mcp config linear --workspace personal
workforce-local-mcp config linear --workspace client-acme

# Same shape for the other three:
workforce-local-mcp config jira     --workspace work \
    --email you@work.com --site https://work.atlassian.net
workforce-local-mcp config datadog  --workspace prod-us --site us1
workforce-local-mcp config metabase --workspace staging \
    --site-url https://mb-staging.example.com

# Pick which one is used when a tool call omits `workspace`:
workforce-local-mcp config linear --set-default work
```

Without `--workspace`, the `config` command writes to the legacy
flat path (same as it always did); the loader treats those as an
implicit workspace named `default`. Running `config show` prints
every adapter grouped by workspace, with the currently-selected
default called out.

Clearing a single workspace is scoped:
```bash
workforce-local-mcp config clear linear --workspace personal  # just personal
workforce-local-mcp config clear linear                       # all linear workspaces + legacy
```

### Targeting a workspace from tool calls

Every adapter tool already exposes an optional selector parameter in
its input schema — you can set it in your prompt or trust Claude to
pick it up from the tool description:

| Adapter  | Parameter    | Examples                                      |
|----------|--------------|-----------------------------------------------|
| Jira     | `workspace`  | `"work"`, `"personal"`                        |
| Linear   | `workspace`  | `"work"`, `"personal"`, `"client-acme"`       |
| Datadog  | `account`    | `"prod-us"`, `"prod-eu"`, `"staging"`         |
| Metabase | `instance`   | `"prod"`, `"staging"`                         |

Omitting the parameter uses whatever `--set-default` last pointed at,
falling back to a workspace literally named `default`. So:

> List the 3 most recent Linear issues in the `personal` workspace.

turns into `linear.issue_list { workspace: "personal", limit: 3 }`
and hits that workspace's API key. Without the "personal" qualifier,
it hits the default.

After changing credentials, restart the MCP server — Claude Code
spawns it fresh each session, so this usually just means starting a
new Claude session.

### Piped setup (for scripts)

The secret prompts accept piped stdin so the commands compose with
password managers or vault exports:

```bash
op read "op://Private/Jira/api_token" \
  | workforce-local-mcp config jira --email you@example.com --site https://you.atlassian.net
```

### Legacy env vars (still work)

Setting `JIRA_EMAIL` + `JIRA_API_TOKEN` + `JIRA_SITE` (or the Linear /
Datadog equivalents) in the environment still works as a fallback.
You'll see a one-time deprecation warning on startup pointing at the
`config` command.

## Tools

Each configured adapter exposes the full surface from the underlying
`workforce-skills` client. Run `/mcp` in a Claude Code session (or
send `tools/list` over stdio) for the complete, schema-annotated
list. A summary of what's available:

### Jira (18 tools, prefix `jira.`)

Search, fetch, create / update / delete issues, comments,
transitions, worklogs, attachments, boards, sprints, projects, user
search, plus a generic `jira.api` escape hatch for any REST v3 call
not covered by a typed tool.

### Linear (22 tools, prefix `linear.`)

Issue list / search / get / create / update / link / bulk-update,
comment list / add, project list / get, team / state / label /
member list, plus full document CRUD (`linear.document_*`).

### Datadog (31 tools, prefix `datadog.`)

Logs (search, aggregate), metrics (query, scalar, list, metadata),
monitors (list, get, search, create, update, delete), events,
incidents, synthetics, SLOs, dashboards — the full SRE workflow.

### Metabase (22 tools, prefix `metabase.`)

Cards (list, get, query, create, update, archive), dashboards
(list, get, query, create, update, archive), collections,
databases + schemas + tables (introspection), native SQL queries,
search. Queries that return rows are capped at 2000 by default
(tune with `config metabase --max-query-rows`).

### Core tools you'll reach for most

- `jira.search` / `linear.issue_search` — find issues by JQL or
  full-text.
- `jira.get_issue` / `linear.issue_get` — fetch by key / identifier.
- `jira.add_comment` / `linear.comment_add` — leave a comment,
  markdown supported.
- `jira.transition_issue` / `linear.issue_update` — move through
  workflow states.
- `datadog.logs_search` — last-N-minutes log query with cursor
  pagination.
- `datadog.monitors_list` — pre-check before creating new monitors.
- `datadog.metrics_query` — v1 timeseries (e.g.
  `avg:system.cpu.user{*}`).

## When to use

- **User mentions an issue key like `ENG-123` or `DW-42`**: call
  `jira.get_issue` or `linear.issue_get` directly — don't ask them
  to paste the body.
- **"Find me tickets about X"**: prefer `jira.search` (JQL) or
  `linear.issue_search` (full-text) over grep-style approaches.
- **Incident / oncall work**: `datadog.logs_search` with a tight
  time window, or `datadog.monitors_list` to see what's alerting.
- **"What does the X dashboard show?"**: `metabase.dashboard_query`
  returns the live result set for every card on the dashboard.
- **"Show me rows from the Y table"**: `metabase.native_query` with
  raw SQL, or `metabase.card_query` to rerun an existing saved
  question.
- **After code changes**: offer to comment on the matching Jira /
  Linear issue with what changed, so the ticket stays in sync.

## Notes

- Multi-workspace is first-class in the CLI (see above) — you can
  hold N sets of creds per adapter and switch between them per
  tool call, or rely on the configured default.
- Routing hints (Jira project-key prefixes, Linear team-key prefixes
  that auto-route "ENG-42" → workspace "acme") aren't wired through
  the CLI yet — today the agent must target a workspace explicitly
  or let the default handle it. Configure those via the skill's TOML
  config in the hub if you need them.
- These tools make live API calls — network failures surface as
  `isError: true` with the underlying adapter's error message.
