# Skills

The workforce skill system provides two types of skills:

- **Built-in skills** are compiled into the hub binary and provide native tool implementations (e.g., GitHub). They are managed via vault credentials and loaded at startup or runtime.
- **External skills** are SKILL.md-based packages installed from git repositories or local directories. They provide agent instructions, tool declarations, and optional per-skill configuration.

Both types can be managed by agents at runtime using the `system.*` administration tools — no SSH or restarts required.

## Quick Start

The fastest way to enable a skill is through an agent conversation:

```
You:   "Enable GitHub tools. My token is ghp_abc123..."
Agent: vault.set("skills/github/token", "ghp_abc123...")   # encrypted on disk
Agent: system.enable_skill("github")                        # loads 13 tools
Agent: "GitHub tools are now available. I can list repos, create issues, manage PRs, and more."
```

The token is AES-256-GCM encrypted in the vault. On the next restart, the skill auto-loads from the vault — no manual configuration needed.

## Built-in Skills

Built-in skills are compiled into the hub binary and provide native Rust tool implementations. They are loaded at startup or enabled at runtime by agents.

### Loading Order

At startup, the hub loads built-in skills in this order:

1. **Vault credentials** (preferred): For each known skill, check the vault for required credentials. If found, load the skill from the vault.
2. **Config / environment variables** (fallback): If the vault has no credentials for a skill, fall back to `workforce.toml` config or environment variables (e.g., `GITHUB_TOKEN`).

This means existing deployments using `GITHUB_TOKEN` in `hub.env` continue to work unchanged, but new deployments can use the vault exclusively.

### Runtime Loading

Built-in skills can be loaded and unloaded at runtime without restarting the hub. The `system.enable_skill` and `system.disable_skill` tools add or remove tools from the live registry. In-flight tool calls are not affected — the registry uses `Arc<dyn Tool>` references, so removing a tool only prevents new calls.

### Credential Storage

Skill credentials are stored in the vault using a consistent path convention:

```
skills/{skill_name}/{credential_name}
```

For example:
- `skills/github/token` — GitHub Personal Access Token

See [VAULT.md](VAULT.md) for details on vault encryption, key management, and CLI usage.

### Available Built-in Skills

#### GitHub

Interact with GitHub repositories, issues, pull requests, and files.

| Field | Value |
|-------|-------|
| Skill name | `github` |
| Tool prefix | `github.*` |
| Tools loaded | 13 |
| Required credentials | `skills/github/token` (GitHub PAT) |

**Vault setup:**
```bash
# Via CLI
workforce vault set skills/github/token ghp_your_token_here

# Via agent
vault.set("skills/github/token", "ghp_your_token_here")
```

**Config fallback** (`workforce.toml`):
```toml
[github]
# token = "ghp_..."              # or set GITHUB_TOKEN env var
# base_url = "https://api.github.com"   # default; change for GitHub Enterprise
# default_owner = "my-org"       # optional default repo owner
```

**Environment variable fallback:**
```bash
GITHUB_TOKEN=ghp_your_token_here
```

**Tools provided:**

| Tool | Description |
|------|-------------|
| `github.list_repos` | List repositories for a user or organization |
| `github.get_repo` | Get repository details |
| `github.list_issues` | List issues with filtering |
| `github.create_issue` | Create a new issue |
| `github.update_issue` | Update an existing issue |
| `github.list_pulls` | List pull requests |
| `github.get_pull` | Get pull request details |
| `github.create_pull` | Create a pull request |
| `github.list_branches` | List repository branches |
| `github.get_file` | Get file contents from a repository |
| `github.search_code` | Search code across repositories |
| `github.search_commits` | Search commits by author, org/repo, and date (default branch) |
| `github.list_commits` | List commits on a branch |
| `github.create_comment` | Comment on an issue or pull request |

See [JIRA.md](JIRA.md), [DATADOG.md](DATADOG.md), and [METABASE.md](METABASE.md) for the multi-tenant skills documented below.

#### Jira

Interact with Jira Cloud issues, comments, transitions, attachments, projects, users, boards, sprints, and worklogs across multiple Atlassian tenants.

| Field | Value |
|-------|-------|
| Skill name | `jira` |
| Tool prefix | `jira.*` |
| Tools loaded | 18 |
| Required credentials | `skills/jira/email`, `skills/jira/api_token`, `skills/jira/site` |
| Multi-tenant | Yes — named workspaces with project-key prefix auto-routing |
| Auth | HTTPS Basic (`email:api_token`) |

**Vault setup (single tenant):**
```bash
workforce vault set skills/jira/email     you@acme.com
workforce vault set skills/jira/api_token <token>
workforce vault set skills/jira/site      https://acme.atlassian.net
```

**Vault setup (multi tenant):**
```bash
workforce vault set skills/jira/workspaces/acme/email        you@acme.com
workforce vault set skills/jira/workspaces/acme/api_token    <token>
workforce vault set skills/jira/workspaces/acme/site         https://acme.atlassian.net
workforce vault set skills/jira/workspaces/acme/project_keys ENG,INFRA   # enables auto-routing
workforce vault set skills/jira/default_workspace            acme      # only with several workspaces
```

Generate API tokens at https://id.atlassian.com/manage-profile/security/api-tokens.

When `project_keys` are configured, calling `jira.get_issue` with `OPS-42` auto-routes to whichever workspace owns the `OPS` prefix. The explicit `workspace` parameter on every tool always wins.

**Tools provided:**

| Tool | Description |
|------|-------------|
| `jira.search` | JQL search via the v3 `/search/jql` endpoint with cursor pagination |
| `jira.get_issue` | Full issue + comments + rendered description |
| `jira.create_issue` | Create issue (plain-text descriptions auto-wrapped to ADF) |
| `jira.update_issue` | Update fields, including custom fields via passthrough |
| `jira.delete_issue` | Permanent delete |
| `jira.assign_issue` | Assign by `accountId`; pass `null` to unassign |
| `jira.add_comment` | Add a comment (auto-ADF wrapping) |
| `jira.list_comments` | List comments with rendered HTML |
| `jira.list_transitions` | Available workflow transitions for an issue |
| `jira.transition_issue` | Move an issue through a transition by id or name |
| `jira.add_attachment` | Upload a local file as an attachment |
| `jira.list_projects` | List accessible projects |
| `jira.get_project` | Project detail with issue types |
| `jira.search_users` | User picker — returns `accountId` |
| `jira.list_boards` | Jira Software (Agile) boards |
| `jira.list_sprints` | Sprints on a board, optionally filtered by state |
| `jira.add_worklog` | Log time on an issue |
| `jira.api` | Escape hatch for arbitrary REST calls |

See [JIRA.md](JIRA.md) for the full guide: ADF handling, custom fields, multi-tenant routing, and example agent prompts.

#### Datadog

Query Datadog metrics and logs and manage monitors, dashboards, downtimes, incidents, SLOs, and hosts across multiple Datadog accounts and regions.

| Field | Value |
|-------|-------|
| Skill name | `datadog` |
| Tool prefix | `datadog.*` |
| Tools loaded | 31 |
| Required credentials | `skills/datadog/api_key`, `skills/datadog/app_key` |
| Multi-tenant | Yes — named accounts, each bound to one Datadog region |
| Auth | `DD-API-KEY` + `DD-APPLICATION-KEY` headers |
| Default policies | Read tools `allow`; write/destructive tools `require_approval` |

**Sites supported:** `us1` (default), `us3`, `us5`, `eu1`, `ap1`. Each account binds to one region.

**Vault setup (single account):**
```bash
workforce vault set skills/datadog/api_key <api-key>
workforce vault set skills/datadog/app_key <app-key>
workforce vault set skills/datadog/site    us1
```

**Vault setup (multi account):**
```bash
workforce vault set skills/datadog/accounts/prod-us/api_key <api-key>
workforce vault set skills/datadog/accounts/prod-us/app_key <app-key>
workforce vault set skills/datadog/accounts/prod-us/site    us1

workforce vault set skills/datadog/accounts/prod-eu/api_key <api-key>
workforce vault set skills/datadog/accounts/prod-eu/app_key <app-key>
workforce vault set skills/datadog/accounts/prod-eu/site    eu1

workforce vault set skills/datadog/default_account prod-us   # only with several accounts
```

Generate keys at:
- API key: https://app.datadoghq.com/organization-settings/api-keys
- App key: https://app.datadoghq.com/personal-settings/application-keys

**Tool families (31 tools total):**

| Family | Tools | Description |
|--------|-------|-------------|
| Metrics | `metrics_query`, `metrics_scalar`, `metrics_list`, `metrics_metadata` | v2 timeseries/scalar queries, metric discovery |
| Logs | `logs_search`, `logs_aggregate` | v2 log search with cursor pagination, aggregations |
| Monitors | `monitors_list`/`get`/`search`/`create`/`update`/`delete`/`mute`/`unmute` | Full CRUD + mute/unmute |
| Dashboards | `dashboards_list`/`get`/`create` | List, view, create with raw widget JSON |
| Events | `events_list`, `events_post` | List events, post custom events |
| Incidents | `incidents_list`, `incidents_get` | v2 incident management API |
| SLOs | `slos_list`, `slos_get` | Service-level objectives |
| Hosts | `hosts_list`, `hosts_mute`, `hosts_unmute` | Host inventory + alert silencing |
| Downtimes | `downtimes_list`, `downtimes_create`, `downtimes_cancel` | v2 downtimes |
| Synthetics | `synthetics_list` | API + Browser test inventory |
| Escape hatch | `datadog.api` | Arbitrary REST call for endpoints not yet wrapped |

**Default policy split.** Read/query tools default to `allow`. Write/destructive tools (`monitors_create`/`update`/`delete`/`mute`/`unmute`, `dashboards_create`, `events_post`, `hosts_mute`/`unmute`, `downtimes_create`/`cancel`, and `datadog.api`) default to `require_approval` because Datadog ops actions touch live production.

See [DATADOG.md](DATADOG.md) for the full guide: time-range parsing, pagination, custom monitor options, and example agent prompts.

#### Metabase

Query Metabase cards and dashboards, run raw SQL, and manage collections across multiple Metabase deployments.

| Field | Value |
|-------|-------|
| Skill name | `metabase` |
| Tool prefix | `metabase.*` |
| Tools loaded | 22 |
| Required credentials | `skills/metabase/api_key`, `skills/metabase/site_url` |
| Multi-tenant | Yes — named instances selected via an `instance` tool parameter |
| Auth | `X-API-KEY` header (Metabase v0.49+) |
| Default policies | All tools `allow`; mutating tools are marked in their descriptions |

**Vault setup (single instance):**
```bash
workforce vault set skills/metabase/api_key  <mb_...>
workforce vault set skills/metabase/site_url https://mb.example.com
```

**Vault setup (multi instance):**
```bash
workforce vault set skills/metabase/instances/prod/api_key    <mb_...>
workforce vault set skills/metabase/instances/prod/site_url   https://mb.prod.example.com

workforce vault set skills/metabase/instances/staging/api_key  <mb_...>
workforce vault set skills/metabase/instances/staging/site_url https://mb.staging.example.com

workforce vault set skills/metabase/default_instance          prod   # only with several instances
```

Generate API keys in Metabase at `/admin/settings/authentication/api-keys` (v0.49+).

**Tool families (22 tools total):**

| Family | Tools | Description |
|--------|-------|-------------|
| Cards | `card_list`, `card_get`, `card_query`, `card_create`, `card_update`, `card_archive` | Saved questions — list/read/execute/CRUD |
| Dashboards | `dashboard_list`, `dashboard_get`, `dashboard_query`, `dashboard_create`, `dashboard_update`, `dashboard_archive` | Dashboards + run-every-card-in-dashboard |
| Collections | `collection_list`, `collection_get`, `collection_items`, `collection_create`, `collection_update` | Folder-like grouping. `id` accepts integer or `"root"`. |
| Schema | `database_list`, `database_tables`, `table_fields` | Database + table + column introspection |
| Query | `dataset_query_native` | Raw SQL execution against a database |
| Search | `search` | Global search across cards, dashboards, collections |

**Row cap.** `card_query`, `dashboard_query`, and `dataset_query_native` enforce a hard ceiling of 2000 rows per call. Per-instance overrides (`skills/metabase/instances/<name>/max_query_rows`) can lower but not raise the cap. This protects the agent's context window from oversized result sets.

**Default policy.** All mutating tools (`*_create`, `*_update`, `*_archive`, `dataset_query_native`) default to `allow` but are clearly marked in their descriptions. Deployments that want a confirmation gate should downgrade these to `require_approval`, following the same pattern as Datadog's write tools.

See [METABASE.md](METABASE.md) for the full guide: dataset_query shape, template tags, row-cap mechanics, example agent prompts, and troubleshooting.

#### Postmark

Send transactional email and inspect delivery status, bounces, stats, templates, message streams, and suppressions across multiple Postmark servers.

| Field | Value |
|-------|-------|
| Skill name | `postmark` |
| Tool prefix | `postmark.*` |
| Tools loaded | 28 |
| Required credentials | `skills/postmark/server_token` |
| Optional credentials | `skills/postmark/account_token` (enables `servers_list`, `domains_list`, `domain_get`, `postmark.api` with `token_type=account`) |
| Multi-tenant | Yes — named workspaces selected via a `workspace` tool parameter |
| Auth | `X-Postmark-Server-Token` + optional `X-Postmark-Account-Token` headers |
| Default policies | Reads `allow`; writes (`email_send*`, `bounce_activate`, `suppressions_*`, `postmark.api`) `require_approval` |

**Vault setup (single workspace):**
```bash
workforce vault set skills/postmark/server_token  <pm_...>
workforce vault set skills/postmark/account_token <pm_...>   # optional
```

**Vault setup (multi workspace):**
```bash
workforce vault set skills/postmark/workspaces/prod/server_token  <pm_prod_...>
workforce vault set skills/postmark/workspaces/prod/account_token <pm_acct_...>

workforce vault set skills/postmark/workspaces/staging/server_token <pm_staging_...>
# staging has no account_token; account-scoped tools error for this workspace

workforce vault set skills/postmark/default_workspace prod   # only with several workspaces
```

Generate tokens in Postmark: Server token → Servers → *your server* → API Tokens; Account token → Account → API Tokens.

**Tool families (28 tools total):**

| Family | Tools | Description |
|--------|-------|-------------|
| Messages outbound | `messages_outbound_search`, `message_outbound_get`, `message_outbound_dump`, `messages_outbound_opens`, `messages_outbound_clicks` | Status of sent messages + raw MIME + tracking events |
| Messages inbound | `messages_inbound_search`, `message_inbound_get` | Inbound message search + details |
| Email send | `email_send`, `email_send_batch`, `email_send_template` | Transactional sending. `require_approval`. |
| Bounces | `bounces_list`, `bounce_get`, `bounce_activate` | Inspect + re-activate. Activate is `require_approval`. |
| Stats | `delivery_stats`, `stats_outbound`, `stats_bounces`, `stats_opens`, `stats_clicks` | Delivery rollups over a time range |
| Templates | `templates_list`, `template_get` | Template inspection (read-only) |
| Streams & suppressions | `message_streams_list`, `suppressions_dump`, `suppressions_add`, `suppressions_delete` | Stream listing + suppression list mgmt. Add/delete are `require_approval`. |
| Account-scoped | `servers_list`, `domains_list`, `domain_get` | Require `account_token`; error cleanly when missing. |
| Escape hatch | `postmark.api` | Arbitrary REST call with `token_type: "server" | "account"`. `require_approval`. |

**Date filters are Eastern Time**, not UTC — Postmark's API convention. Pagination cap: `count + offset <= 10_000` on messages and bounces endpoints. `/email/batch` is capped at 500 messages per request. `/email` requires at least one of `HtmlBody` or `TextBody`.

See [`crates/skills/src/adapters/postmark/SKILL.md`](../crates/skills/src/adapters/postmark/SKILL.md) for the full tool catalogue, token routing details, gotchas, and example agent prompts.

#### SendGrid

Send transactional email and inspect delivery, stats, templates, sender identities and suppressions via the Twilio SendGrid v3 API across multiple accounts.

| Field | Value |
|-------|-------|
| Skill name | `sendgrid` |
| Tool prefix | `sendgrid.*` |
| Tools loaded | 16 |
| Required credentials | `skills/sendgrid/api_key` |
| Optional credentials | `skills/sendgrid/from_email`, `skills/sendgrid/from_name` (default sender), `skills/sendgrid/base_url` (`https://api.eu.sendgrid.com` for EU accounts) |
| Multi-tenant | Yes — named workspaces selected via a `workspace` tool parameter |
| Auth | `Authorization: Bearer SG.…` — SendGrid has no OAuth for its API |
| Default policies | Reads `allow`; writes (`email_send*`, `suppression_delete`, `unsubscribes_add`, `sendgrid.api`) `require_approval` |

**Vault setup (single workspace):**
```bash
workforce vault set skills/sendgrid/api_key    <SG....>
workforce vault set skills/sendgrid/from_email noreply@example.com   # optional default From
workforce vault set skills/sendgrid/from_name  Example               # optional
```

**Vault setup (multi workspace):**
```bash
workforce vault set skills/sendgrid/workspaces/prod/api_key    <SG.prod...>
workforce vault set skills/sendgrid/workspaces/prod/from_email noreply@example.com
workforce vault set skills/sendgrid/workspaces/eu/api_key      <SG.eu...>
workforce vault set skills/sendgrid/workspaces/eu/base_url     https://api.eu.sendgrid.com
workforce vault set skills/sendgrid/default_workspace prod   # only with several workspaces
```

Per-user keys: each user saves their own key (and optionally their own From) from **My Settings**; the per-user tier wins over the shared one on every request. The API key and sender resolve per request, but `base_url` and `default_workspace` are read from the shared tiers only — a per-user `base_url` is ignored so one caller can never redirect other users' keys.

**Tool families (16 tools total):**

| Family | Tools | Description |
|--------|-------|-------------|
| Email send | `email_send`, `email_send_batch` | Transactional sending (≤1000 recipients per request). `require_approval`; the approval prompt names the From, recipient count and subject. |
| Activity | `messages_search`, `message_get` | Email Activity feed (needs the paid history add-on) |
| Stats | `stats_global`, `stats_categories` | Aggregate statistics by day / week / month |
| Senders | `senders_list`, `domains_list` | Verified sender identities and authenticated domains |
| Templates | `templates_list`, `template_get` | Dynamic and legacy template inspection |
| Suppressions | `suppressions_list`, `suppression_get`, `unsubscribe_groups_list`, `suppression_delete`, `unsubscribes_add` | `kind` ∈ bounces, blocks, spam_reports, invalid_emails, unsubscribes. Delete/add are `require_approval`. |
| Escape hatch | `sendgrid.api` | Arbitrary v3 call. `require_approval`. |

**Sender identity.** A key with `mail.send` can send as *any* verified sender in the account; SendGrid rejects unverified From addresses with 403. `senders_list` / `domains_list` show what is usable.

See [`crates/skills/src/adapters/sendgrid/SKILL.md`](../crates/skills/src/adapters/sendgrid/SKILL.md) for the full tool catalogue and gotchas.

## External Skills

External skills are directory-based packages containing a `SKILL.md` file. They can be installed from git repositories or local paths, and provide agent instructions and tool declarations.

### SKILL.md Format

Every external skill must have a `SKILL.md` file at its root. The file uses YAML frontmatter for metadata, with the markdown body providing instructions for the agent.

```markdown
---
name: git-operations
version: 1.0.0
description: Git operations for agents
author: acme
requires:
  bins: [git]
  env: [GITHUB_TOKEN]
tools:
  - git.clone
  - git.commit
  - git.push
  - git.branch
  - git.diff
---
# Git Operations Skill

Use these tools to work with git repositories.

## Usage

Use `git.clone` to clone a repository, `git.commit` to commit changes, etc.
```

**Frontmatter fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Unique skill name |
| `version` | yes | Semantic version string |
| `description` | yes | Human-readable description |
| `author` | no | Author or organisation |
| `requires.bins` | no | Binary names that must exist on PATH |
| `requires.env` | no | Environment variables that must be set |
| `requires.config` | no | Config keys that must be present |
| `tools` | no | Tool names this skill provides |

### Directory Structure

Skills are installed to and discovered from the skills directory:

```
{data_dir}/skills/
  git-operations/
    SKILL.md            # Required — metadata + agent instructions
    config.toml         # Optional — per-skill configuration
    ...                 # Any other files the skill needs
  docker-tools/
    SKILL.md
    ...
```

### Discovery Paths

The hub reads external skills from four roots, highest precedence first:

1. **Skills directory:** `skills_dir` (default `{data_dir}/skills/`), where every hub surface installs
2. **Operator:** `~/.workforce/skills/`
3. **Bundled:** `/app/bundled-skills/skills/` (baked into the image)
4. **Working directory:** `./skills/`

When two roots hold a skill with the same name, the earlier root's copy wins; within one root, the directory that sorts first wins.

Every agent in the hub process shares one live set of these skills (`crates/hub/src/external_skills.rs`). Installing, editing or removing a skill through the Skills page, the API or the agent tools re-reads the roots straight away, so every agent that can use the skill sees the change on its next message with no restart. If the skills directory exists but cannot be read, the previous set is kept; any other root that cannot be read is skipped with a warning. Writers outside the hub process — `workforce skill install`, `workforce skill catalog-install`, or a manual edit on the data volume — take effect when the hub next starts.

The skills directory is configured in `workforce.toml`:

```toml
[skills]
skills_dir = "./data/skills"    # default: {data_dir}/skills
# enabled = ["git-operations"]  # optional: restrict which skills are eligible
```

### Requirements & Gating

Skills can declare requirements in the `requires` section of their SKILL.md. The gating system checks these before marking a skill as available:

- **`bins`** — verifies each binary exists on `PATH`
- **`env`** — verifies each environment variable is set
- **`config`** — verifies each config key is present (reserved for future use)

A skill is still installed even if requirements are not met — it is just marked as `unavailable` until the requirements are satisfied.

### Per-Skill Configuration

Skills can have a `config.toml` alongside their `SKILL.md`. Values support variable expansion:

```toml
# config.toml for a hypothetical skill
api_key = "${vault:skills.myskill.api_key}"    # resolved from vault
base_url = "${API_BASE_URL:-https://api.example.com}"  # env var with default
log_level = "${LOG_LEVEL:-info}"                # env var with default
```

**Expansion order:**
1. Vault references (`${vault:path}`) are resolved first
2. Environment variables (`${VAR}` or `${VAR:-default}`) are resolved second

| Pattern | Description |
|---------|-------------|
| `${vault:skills.myskill.key}` | Resolve from encrypted vault |
| `${MY_ENV_VAR}` | Resolve from environment |
| `${MY_ENV_VAR:-fallback}` | Environment with default value |

### CLI Commands

External skills can also be managed via the CLI:

```bash
# List all discovered skills
workforce skill list

# Search skills by name, description, or tool names
workforce skill search git

# Show detailed info about a skill
workforce skill show git-operations

# Install from git URL
workforce skill install https://github.com/user/my-skill.git

# Install from local path
workforce skill install /path/to/local/skill

# Uninstall by name
workforce skill uninstall git-operations

# Generate a config.toml template for a skill
workforce skill init my-skill
```

## System Administration Tools

Eleven `system.*` tools let agents manage both built-in and external skills at runtime. These tools are registered automatically when the vault is enabled.

### Access Control

System tools are subject to the policy engine like any other tool. By default (allow-all policy), any agent can call them. To restrict access to specific agents, configure policy rules:

```yaml
# policies/default.yaml — only Allan can manage skills
- name: deny-system-tools
  scope: global
  tool: "system.*"
  action: deny

- name: allow-system-for-allan
  scope:
    agent: "allan"
  tool: "system.*"
  action: allow
  priority: 10
```

See [POLICY_ENGINE.md](POLICY_ENGINE.md) for the full policy configuration reference.

### `system.list_skills`

List all available built-in skills, their status, and credential requirements.

**Input:** none

**Response:**
```json
{
  "skills": [
    {
      "name": "github",
      "description": "Interact with GitHub repositories, issues, pull requests, and files",
      "status": "loaded",
      "loaded_tools": ["github.list_repos", "github.get_repo", "..."],
      "loaded_source": "vault",
      "credentials": [
        {
          "vault_path": "skills/github/token",
          "description": "GitHub Personal Access Token (PAT)",
          "configured": true,
          "required": true
        }
      ]
    }
  ]
}
```

**Status values:**

| Status | Meaning |
|--------|---------|
| `loaded` | Skill is active and its tools are available |
| `available` | All required credentials are in the vault, but the skill is not yet loaded |
| `needs_credentials` | One or more required credentials are missing from the vault |

### `system.enable_skill`

Enable a skill by loading its tools from vault credentials.

**Input:**
```json
{ "skill": "github" }
```

**Success response:**
```json
{
  "skill": "github",
  "enabled": true,
  "tools_loaded": 13,
  "tool_names": ["github.list_repos", "github.get_repo", "..."]
}
```

**Notes:**
- If the skill is already loaded, it will be automatically reloaded with the latest vault credentials. The response includes `"reloaded": true` to indicate this happened.

**Failure cases:**
- Missing credentials: `"Missing vault credential 'skills/github/token'. Use vault.set to store your GitHub PAT first."`
- Unknown skill: `"Unknown built-in skill: foo"`

### `system.disable_skill`

Remove a skill's tools from the runtime. Vault credentials are preserved — the skill can be re-enabled with `system.enable_skill`.

**Input:**
```json
{ "skill": "github" }
```

**Success response:**
```json
{
  "skill": "github",
  "disabled": true,
  "tools_removed": 13,
  "removed_tools": ["github.list_repos", "github.get_repo", "..."]
}
```

**Failure case:**
- Skill not loaded: `"Skill 'github' is not currently loaded"`

### `system.skill_status`

Check the detailed status of a specific skill.

**Input:**
```json
{ "skill": "github" }
```

**Response:**
```json
{
  "name": "github",
  "description": "Interact with GitHub repositories, issues, pull requests, and files",
  "status": "loaded",
  "credentials_ready": true,
  "credentials": [
    {
      "vault_path": "skills/github/token",
      "description": "GitHub Personal Access Token (PAT)",
      "configured": true,
      "required": true
    }
  ],
  "loaded_tools": ["github.list_repos", "github.get_repo", "..."],
  "loaded_source": "vault"
}
```

### External Skill Tools

The following six tools let agents install, uninstall, discover and publish external skills at runtime, and install skills of either kind from a catalog. These complement the CLI commands described in the [External Skills](#external-skills) section above. An install or uninstall takes effect for every agent that can use the skill on its next message, with no restart. Which tool installs what is summarised in [Choosing an install tool](#choosing-an-install-tool).

#### `system.install_external_skill`

Install or update an external (`SKILL.md`) skill from its repository or a directory. Priced `skills_author` (any user) for a GitHub repository or a directory, and `skills_manage` for a git URL on another host. A member's install belongs to them and is private unless they pass `visibility = "team"` or `visibility = "everyone"`; a `source` directory must resolve inside their workspace roots. A holder of `skills_manage` installs for everyone unless they pass `visibility = "private"`, and may name any directory on the hub. A WASM skill (`Skill.toml`) installs with `wasm_skill.install` instead, and a skill a configured catalog lists installs by name with `system.install_catalog_skill`.

The capability gate prices the call with `system_tool::external_install_capability`, which reads the input with the tool's own parser, so only another git host needs `skills_manage` before the tool runs. Inside the tool, standing comes from the live capability matrix: holding `skills_manage` outright (the gate passes `Ask`, but in-tool standing needs `Allow`) makes the caller a skills manager, so a role with `Ask` on `skills_manage` installs privately and cannot read another git host.

**Input** (exactly one of `github_repo` and `source`, and optionally `visibility`):
```json
{ "github_repo": "acme/skills@main#subpath=hello-world" }
```
```json
{ "source": "https://gitlab.example.com/acme/skills.git#subpath=hello-world" }
```
```json
{ "source": "/srv/skills/hello-world" }
```
```json
{ "source": "skills/hello-world", "visibility": "everyone" }
```

`github_repo` takes the grammar `wasm_skill.install` takes: `owner/repo` names the repository, `@<ref>` a branch, tag or 40-hex commit (the default branch when it is left out), and `#subpath=<dir>` the skill's directory inside it. A commit is checked out and verified. `source` takes another git URL, with an optional `#subpath=<dir>`, or the path of a skill directory; an `https://github.com/owner/repo` URL is treated as that repository. A relative path resolves against the caller's workspace for everyone. `visibility` is `private`, `team` (anyone in a team with the owner, at run) or `everyone`; on a reinstall it applies only when the caller controls who reaches the skill (its owner, or a skills manager).

A GitHub repository is read with the caller's GitHub token for its owner, resolved by `user_config::resolve_github_pat` as `wasm_skill.install` resolves it: a value the caller stored wins over a shared one (the agent's, then the global one), and within each, `skills/github/workspaces/<owner>/token` wins over `skills/github/token`. Any other host is cloned without a token.

Installing a skill that is already installed replaces it and keeps its `config.toml`, owner and sharing. The skill is cloned and checked before anything is written, so a failed clone, a missing subpath or a WASM skill leaves the installed copy untouched. A name belongs to one skill: the caller must manage whatever already holds the name and whatever the directory it would replace holds, a member cannot take a built-in skill's name or directory, and an owned install cannot take a name a WASM skill holds.

**Success response:**
```json
{
  "installed": true,
  "name": "hello-world",
  "version": "2.0.0",
  "description": "Says hello",
  "author": "acme",
  "tools": [],
  "available": true,
  "gating": "all requirements met",
  "path": "/app/data/skills/hello-world",
  "replaced": true,
  "previous_version": "1.0.0",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "owner": "3f0c2a4e-…",
  "visibility": "private",
  "shared_with": 0,
  "sharing": "Only you can use `hello-world`. Share it with `cmd: skill share hello-world <user>` (add `manage` to let them edit it), or publish it to everyone with `cmd: skill visibility hello-world everyone`."
}
```

An install for everyone has `"owner": null`, `"visibility": "everyone"` and `"sharing": "Every user can use `hello-world`: it was installed for the whole org."`. `previous_version` is `null` for a new install, and `commit` is `null` for a skill installed from a directory. If a skill's requirements (required binaries, env vars) are not met, or its files fail the security scan, it is still installed but `available` will be `false` and `gating` will give the reason. If the installed `SKILL.md` cannot be read back, the response is `{ "installed": true, "warning": "...", "path": "..." }` instead.

**Failure cases** for the input, returned as they are:
- Both inputs: ``"Pass either `github_repo` or `source`, not both."``
- Neither input: ``"Pass `github_repo` (`owner/repo[@ref][#subpath=dir]`) for a GitHub repository, or `source` for a skill directory in your workspace. With the skills_manage capability, `source` may also be any directory on the hub or a git URL on another host."``
- A malformed `github_repo`: ``"`https://github.com/acme/skills` is not a GitHub repository reference: `github_repo` takes `owner/repo`, optionally followed by `@<ref>` and `#subpath=<dir>`. Pass a git URL as `source` instead."``
- GitHub shorthand passed as `source`: ``"`acme/skills#subpath=hello-world` is not a directory on this hub. To install from GitHub, pass it as `github_repo` instead of `source`."``
- An unknown visibility: ``"`public` is not a visibility — use `private`, `team` or `everyone`."``
- `team` on a skill nobody owns, from any surface (`PUT /api/{kind}/{name}/visibility` 409, `cmd: skill visibility`, `skill.visibility`): ``"Skill `hello-world` has no owner, so it has no team; install it again with `visibility = \"private\"` to make it a skill of your own."`` The existing ownerless note still covers `private` and `everyone`.
- A member naming another git host: ``"`https://gitlab.example.com/acme/skills.git#subpath=hello-world` is on a git host other than GitHub, and installing from one needs the skills_manage capability (administrators by default). Pass a GitHub repository as `github_repo`, or save the skill in your workspace and pass that directory as `source`."``
- A member's `source` outside their workspace roots: ``"Path '../outside/hello-world' resolves to /app/data/workspace/../outside/hello-world, outside the allowed directories [/app/data/workspace]. Use a relative path — it resolves against your workspace at /app/data/workspace."``
- A `source` directory whose resolved path would be read as a git URL: ``"`hello.git/` resolves to `/app/data/workspace/hello.git`, which reads as a git URL rather than a directory. Pass a directory whose path does not end in `.git`."``

**Failure cases** for the install (each prefixed `"Failed to install skill from '<github_repo or source>': "`):
- A WASM skill: ``"`hello-counter` is a WASM skill: its directory has a Skill.toml and no SKILL.md, and system.install_external_skill installs only external (SKILL.md) skills. Install it with wasm_skill.install and `github_repo = "acme/skills@0123456789abcdef0123456789abcdef01234567#subpath=hello-counter"`, pinned to the commit that source points at now."`` From a directory the call names `project_path`; from another git host it lists the three `wasm_skill.install` sources.
- Clone failure: `"git clone failed: ..."`, followed on its own line by what to do next:
  - GitHub with no token: ``"No GitHub token is set for `acme`, so only public repositories can be read. If the repository is private, store a token that can read it with vault.set at `skills/github/workspaces/acme/token` (for that organisation) or `skills/github/token` (for any), then install again. A token you store is your own; an administrator can store one for everyone with scope `global`."``
  - GitHub with a token: ``"The GitHub token set for `acme` was used: check the repository name and ref, and that the token can read the repository."``
  - Another host: ``"This repository was cloned without credentials, because only github.com repositories are read with a GitHub token, so it must be public. Pass a GitHub repository as `github_repo`."``
- Subpath not found: `"Subpath 'hello-world' not found in cloned repository"`
- Already installed under another directory: `"Skill 'hello-world' is already installed in '/app/data/skills/greeter'. Uninstall it first."`
- The directory it would replace holds another skill: `"Skill directory '/app/data/skills/hello-world' holds skill 'greeter', not 'hello-world'. Uninstall it first."`
- A member naming a built-in skill's name or directory: ``"`github` shares its name or directory with the built-in `github` skill, so only someone with the skills_manage capability can install it. Rename the skill, and the directory you install it from."`` The check ignores case.
- An owned install naming a WASM skill's name: ``"The name `hello-world` belongs to a WASM skill. Pick a different name for your external skill."``
- The name belongs to someone else's skill: ``"The name `hello-world` belongs to another user's skill. Pick a different name, or ask its owner to share it with you (`cmd: skill share hello-world <you> manage`)."``
- The name belongs to an ownerless skill: ``"The name `hello-world` is an org-wide skill installed for everyone, so only someone with the skills_manage capability can replace it. Pick a different name."``
- The directory it would replace is someone else's (a broken skill, or a directory holding only `config.toml`): ``"Skill `hello-world` belongs to another user. Ask them to share it with you (`cmd: skill share hello-world <you> manage`)."``, or for a directory without an owner ``"Skill `hello-world` has no owner — it was installed for the whole org, so only someone with the skills_manage capability can change it."``
- The directory's access record cannot be read, for anyone but a skills manager (whose install replaces it): ``"The access record in `hello-world` cannot be read (/app/data/skills/hello-world/.workforce-access.json does not parse: …), so the skill there cannot be replaced. An admin can remove that directory on the Skills page."``
- No manifest: `"No SKILL.md or Skill.toml found in '...'"`

#### `system.uninstall_external_skill`

Uninstall an external (SKILL.md) skill by name, however it was installed. Priced `skills_author`; needs manage access to the skill (owner, a `manage` grantee, or a holder of `skills_manage`). Standing is judged on the directory the removal takes: its live skill, or its access record when the skill there is not live. A WASM skill is removed with `wasm_skill.uninstall`.

**Input:**
```json
{ "name": "hello-world" }
```

**Success response:**
```json
{
  "uninstalled": true,
  "name": "hello-world"
}
```

**Failure cases:**
- Not installed: ``"Failed to uninstall skill 'hello-world': No external skill named `hello-world` is installed."``, followed by `" Available to you: …"` when the caller can use other external skills
- No manage access: ``"Failed to uninstall skill 'hello-world': Skill `hello-world` belongs to another user. Ask them to share it with you (`cmd: skill share hello-world <you> manage`)."``
- An ownerless skill, for a caller without `skills_manage`: ``"Failed to uninstall skill 'hello-world': Skill `hello-world` has no owner — it was installed for the whole org, so only someone with the skills_manage capability can change it."``
- A skill read from another root (bundled, `~/.workforce/skills` or `./skills`): ``"Failed to uninstall skill 'hello-world': Skill `hello-world` is read from `/app/bundled-skills/skills/hello-world`, outside the skills directory the hub installs into, so it cannot be uninstalled here."``
- A name that is not one directory name never reaches outside the skills directory: the removal is refused rather than resolved.

#### `system.list_external_skills`

List the external (SKILL.md) skills the caller can use now, with their version, requirements, path, owner and visibility. Lists only the skills the caller can use: their own, ones shared with them, and ones for everyone (a holder of `skills_manage` sees every one). WASM skills are listed by `wasm_skill.list`, and what a catalog offers by `system.list_catalog_skills`. Optionally filter by a search query.

**Input:**
```json
{ "query": "hello" }
```

The `query` field is optional. Omit it to list all external skills.

**Response:**
```json
{
  "skills": [
    {
      "name": "hello-world",
      "version": "1.0.0",
      "description": "Says hello",
      "author": "acme",
      "tools": [],
      "available": true,
      "gating": "all requirements met",
      "path": "/app/data/skills/hello-world",
      "body": "...",
      "owner": null,
      "visibility": "everyone",
      "shared_with": 0,
      "your_access": "run"
    }
  ],
  "count": 1,
  "skills_dir": "/app/data/skills"
}
```

#### `system.list_catalog_skills`

List the skills in the catalogs configured under `[[skills.catalogs]]`, external and WASM, with whether each is installed and at which version. Not gated.

**Input:**
```json
{ "catalog": "example" }
```

The `catalog` field is optional. Omit it to list every catalog.

**Response:**
```json
{
  "catalogs": [
    {
      "name": "example",
      "repo": "https://github.com/acme/skills",
      "skills": [
        {
          "catalog": "example",
          "name": "hello-world",
          "kind": "external",
          "description": "Says hello",
          "version": "2.0.0",
          "installed": true,
          "installed_version": "1.0.0"
        }
      ]
    }
  ]
}
```

A catalog whose manifest cannot be fetched has an `error` and no `skills`; the other catalogs still list.

With no `catalog` named and no catalogs configured, `catalogs` is empty.

**Failure cases:**
- A `catalog` named when none is configured: `"No [[skills.catalogs]] is configured on this hub."`
- Unknown catalog: `"Unknown catalog 'missing'. Configured: example"`

#### `system.install_catalog_skill`

Install a skill from a catalog, or update an installed one to the catalog's version. Gated by `skills_manage`: a catalog install is for everyone. The skill is live for every agent that can use it when the tool returns. An updated external skill keeps its `config.toml`, owner and sharing; an updated WASM skill keeps its config, owner and sharing.

**Input:**
```json
{ "skill": "hello-world", "catalog": "example" }
```

`catalog` may be omitted when only one catalog is configured.

**Success response:**
```json
{
  "catalog": "example",
  "skill": "hello-world",
  "kind": "external",
  "version": "2.0.0",
  "installed": true,
  "replaced": true,
  "previous_version": "1.0.0",
  "active": true,
  "activation": "live on every agent now"
}
```

`replaced` and `previous_version` appear only on an update, and `built_from_source: true` only for a WASM skill the hub compiled. `active` is `false` when agents cannot use the skill yet, and `activation` then says why, for example `"installed, but agents cannot use it until this is resolved: 1 requirement(s) not met: ..."`.

**Failure cases** (each prefixed `"Failed to install 'hello-world' from the catalog: "`):
- Several catalogs and none named: ``"Several catalogs are configured (example, acme); pass `catalog` to choose one."``
- Name mismatch: `"catalog entry 'hello-world' does not match the skill's manifest name 'greeter' — fix the catalog so they match"`, before anything is written
- A WASM entry built from source, when `[skills.wasm] allow_build_on_install = false`, when `require_sha256_pin = true` and the catalog's `ref` is not a commit, or when the caller may not compile on this hub

#### `system.publish_external_skill`

Open a pull request against a configured catalog repository holding an installed external skill's files and its `catalog.json` entry, so a skill written in chat lands under source control. It publishes exactly what is installed. Not gated: it writes nothing to the hub, it needs manage access to the skill (owner, a `manage` grantee, or a holder of `skills_manage`), and it pushes with the caller's own GitHub token for the catalog repository's owner. It shares `crates/hub/src/skill_publish.rs` with `wasm_skill.publish`.

**Input:**
```json
{ "skill": "hello-world", "catalog": "example", "base": "main", "summary": "First version" }
```

Only `skill` is required. `catalog` may be omitted when one catalog is configured, and `base` defaults to the catalog's ref (a catalog pinned to a commit needs `base`).

**Success response:**
```json
{
  "skill": "hello-world",
  "version": "1.0.0",
  "previous_version": null,
  "catalog": "example",
  "repo": "https://github.com/acme/skills",
  "subpath": "workforce/external/hello-world",
  "branch": "skill/hello-world-v1.0.0",
  "base": "main",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "pr_url": "https://github.com/acme/skills/pull/12",
  "pr_number": 12,
  "files": { "added": 2, "updated": 0, "removed": 0 },
  "published": true
}
```

When the catalog already holds this version with identical files and entry, the response is `{ "skill", "version", "catalog", "published": false, "note": "The catalog already holds this version with identical source." }`. A new skill is published under `<manifest dir>/external/<name>`; an existing entry keeps its `subpath`. The entry records `kind: "external"`, the version, the description and the `SKILL.md`'s required binaries as `requires_bins`.

**What is published.** Every regular file in the installed directory except hidden entries (any name starting with `.`) at any depth, which covers `.git`, `.workforce-access.json`, `.env`, editor and scratch files; `target/` at any depth; symbolic links, which are never followed; and `config.toml` at the top of the directory, named explicitly because it is not hidden and holds the operator's settings. A nested `config.toml` is published. The same rule governs the repository side, so a top-level `config.toml` and hidden files in the catalog directory are never touched. WASM's lister is a whitelist of known project files; a `SKILL.md` skill's files cannot be enumerated, so this lister is a blacklist, and anything else left in the installed directory reaches the pull request.

**Failure cases:**
- No manage access: ``"Skill `hello-world` belongs to another user. Ask them to share it with you (`cmd: skill share hello-world <you> manage`)."``
- A name that is not one directory name: ``"'../hello-world' cannot name a catalog directory: a skill's name must be one directory name, with no `/` or `\` and not `.` or `..`. Rename the skill in its SKILL.md, install it again and publish again."`` The check runs before the catalog repository is touched.
- A version that is not semver: ``"'hello-world' has version 'latest' in its SKILL.md, which is not semver (such as 1.0.0), so the catalog cannot compare it. Set a semver version, install it again and publish again."``
- A WASM entry of the same name: `"The catalog lists 'hello-world' as a WASM skill; an external (SKILL.md) skill cannot take its entry."`
- The catalog's version is not semver: `"The catalog's version 'latest' for 'hello-world' is not semver, so the installed version cannot be compared with it; fix the entry first."`
- The version is in the catalog with different files: `"'hello-world' v1.0.0 is already in the catalog with different files. Bump version in SKILL.md, install it again (system.install_external_skill) and publish again."`
- The catalog is ahead: `"The catalog holds 'hello-world' v1.1.0, newer than the installed v1.0.0. Install the catalog version, or bump version in SKILL.md past it, install it again and publish again."`
- The branch exists: `"Branch skill/hello-world-v1.0.0 already exists on https://github.com/acme/skills: this version was already pushed. Merge or delete that branch, or bump the version."`
- No GitHub token: `"No GitHub token to publish to acme/skills. Store one at skills/github/workspaces/acme/token (or skills/github/token): publishing writes to the catalog repo with your own credential, not the catalog's read token."`
- The pull request fails after the push: `"Branch skill/hello-world-v1.0.0 was pushed but opening the pull request failed; open it by hand from that branch: …"`

### Choosing an install tool

| Job | Tool |
|---|---|
| Install from a catalog (either kind, by name) | `system.list_catalog_skills` → `system.install_catalog_skill` |
| Install from anywhere (a repository or path) | `system.install_external_skill` for a `SKILL.md`, `wasm_skill.install` for a `Skill.toml` |
| Publish an installed skill to a catalog | `system.publish_external_skill` for a `SKILL.md`, `wasm_skill.publish` for a `Skill.toml` |

A record at `<skill dir>/.workforce-access.json` holds an external skill's owner, visibility and grantees; a directory without one is for everyone.

"Installing skills" on the end-user [Skills page](../end-user-docs/concepts/skills.mdx) explains how to tell the kinds apart, how each is updated, listed and removed, and who can install and use it. `install_guide_and_the_skills_page_agree` checks that the page and the install guide in every agent's prompt (`INSTALL_GUIDE` in `crates/hub/src/system_tool.rs`) name the same tools and vault paths.

Both repository installs and catalog installs place an external skill in a directory named after the skill, so installing it again from either replaces the other's copy and keeps its `config.toml`. The exception is a skill installed from a local directory whose name differs from the skill's: that copy is refused with "already installed in", and must be uninstalled first.

## Common Workflows

### First-Time Setup (Agent-Assisted)

```
You:   "What skills are available?"
Agent: system.list_skills()
Agent: "There's one built-in skill: GitHub. It needs a Personal Access Token.
        Would you like to set one up?"

You:   "Yes, here's my token: ghp_abc123..."
Agent: vault.set("skills/github/token", "ghp_abc123...")
Agent: system.enable_skill("github")
Agent: "Done — 13 GitHub tools are now loaded. I can list repos, manage issues,
        create PRs, and more. The token is encrypted in the vault and will
        persist across restarts."
```

### Rotating a Credential

```
You:   "I rotated my GitHub token. New one is ghp_newtoken..."
Agent: vault.set("skills/github/token", "ghp_newtoken...")
Agent: system.enable_skill("github")
Agent: "GitHub tools reloaded with the new token."
```

### Checking Status

```
You:   "Is GitHub configured?"
Agent: system.skill_status("github")
Agent: "GitHub is loaded with 13 tools, credentials sourced from the vault."
```

### Disabling a Skill

```
You:   "Disable GitHub tools for now."
Agent: system.disable_skill("github")
Agent: "GitHub tools removed. Your credentials are still in the vault —
        just say 'enable GitHub' when you want them back."
```

### Installing an External Skill

```
You:   "Install the hello-world skill from acme/skills"
Agent: system.list_catalog_skills({})
Agent: [no configured catalog lists hello-world, so it installs from the repository]
Agent: system.install_external_skill({ github_repo: "acme/skills#subpath=hello-world" })
Agent: "Installed hello-world v1.0.0 from commit 0123456. All requirements are met, and
        it is live from your next message."
```

### Installing from a Private Repository

```
You:   "Install hello-world from acme/private-skills"
Agent: system.install_external_skill({ github_repo: "acme/private-skills#subpath=hello-world" })
Agent: "The install failed: git clone failed: repository not found. No GitHub token is set
        for acme, so only public repositories can be read. If the repository is private,
        store a token that can read it at skills/github/workspaces/acme/token and I will
        install it again."

You:   "Here's a token: ghp_abc123..."
Agent: vault.set("skills/github/workspaces/acme/token", "ghp_abc123...")
Agent: system.install_external_skill({ github_repo: "acme/private-skills#subpath=hello-world" })
Agent: "Installed hello-world v1.0.0 from commit 0123456."
```

### Updating an Installed Skill

```
You:   "hello-world has a new version on main, update it"
Agent: system.install_external_skill({ github_repo: "acme/skills@main#subpath=hello-world" })
Agent: "Updated hello-world from v1.0.0 to v2.0.0 at commit 89abcde. It kept its
        config.toml and is live on every agent now."
```

### Installing from a Catalog

```
You:   "What can we install from the catalog?"
Agent: system.list_catalog_skills({})
Agent: "The example catalog has hello-world v2.0.0 (you have v1.0.0 installed)."

You:   "Update it."
Agent: system.install_catalog_skill({ skill: "hello-world" })
Agent: "Updated hello-world from v1.0.0 to v2.0.0. It kept its config.toml and is live
        on every agent now."
```

### Writing Your Own External Skill

```
You:   "Write me a skill that says hello to new starters"
Agent: filesystem.write({ path: "skills/hello-world/SKILL.md", content: "---\nname: hello-world\n..." })
Agent: system.install_external_skill({ source: "skills/hello-world" })
Agent: "Installed hello-world v1.0.0. It is yours and private: only you can use it. Share it
        with `cmd: skill share hello-world <user>`, or publish it to everyone with
        `cmd: skill visibility hello-world everyone`."

You:   "cmd: skill share hello-world @bob"
       "Bob can now use `hello-world`. Add `manage` to let them edit it."
```

### Publishing an External Skill

```
You:   "Add hello-world to the example catalog"
Agent: system.publish_external_skill({ skill: "hello-world", summary: "Welcomes new starters" })
Agent: "Opened https://github.com/acme/skills/pull/12 from branch skill/hello-world-v1.0.0,
        adding workforce/external/hello-world with its catalog entry. Once it is merged,
        it installs by name from the catalog. Your installed copy stays yours, and a
        catalog install over it keeps you as its owner."

You:   "It is merged. I changed the wording, publish again"
Agent: system.publish_external_skill({ skill: "hello-world" })
Agent: "The catalog already holds hello-world v1.0.0 with different files. I will bump
        version in SKILL.md to 1.0.1, install it again and publish again."
```

### Listing External Skills

```
You:   "What external skills do we have installed?"
Agent: system.list_external_skills({})
Agent: "There are 2 external skills installed:
        - git-operations v1.0.0 (ready) — 3 tools
        - docker-tools v0.2.0 (unavailable — missing binary 'docker')"
```

### Removing an External Skill

```
You:   "Remove the docker-tools skill"
Agent: system.uninstall_external_skill({ name: "docker-tools" })
Agent: "docker-tools has been uninstalled."
```

## Adding a New Built-in Skill

To add a new built-in skill to the system:

### 1. Create the Skill Adapter

Add a new module under `workforce-skills/src/adapters/` that implements the skill's tools. Each tool must implement the `Tool` trait from `workforce-agent`.

### 2. Register in the Skill Catalog

In `crates/hub/src/skill_loader.rs`, add an entry to `builtin_skill_catalog()`:

```rust
pub fn builtin_skill_catalog() -> Vec<BuiltinSkillMeta> {
    vec![
        // Existing GitHub entry...
        BuiltinSkillMeta {
            name: "jira",
            description: "Manage Jira issues, sprints, and projects",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/jira/url",
                    description: "Jira instance URL",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/jira/token",
                    description: "Jira API token",
                    required: true,
                },
            ],
            tool_prefix: "jira",
        },
    ]
}
```

### 3. Add Vault Loading

Add a match arm in `load_skill_from_vault()`:

```rust
pub fn load_skill_from_vault(
    skill_name: &str,
    vault: &Vault,
    config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<Vec<String>> {
    match skill_name {
        "github" => load_github_from_vault(vault, config, registry),
        "jira" => load_jira_from_vault(vault, config, registry),
        _ => Err(eyre::eyre!("Unknown built-in skill: {}", skill_name)),
    }
}
```

### 4. Optionally Add Config Fallback

If you want the skill to also load from `workforce.toml` or environment variables, add a config struct to `settings.rs` and a loading function in `load_builtin_skill_tools()`.

### 5. Test

```bash
cargo test -p workforce-hub
cargo clippy --all-targets --all-features --no-deps -- -D warnings
```

## Architecture

### Key Components

| Component | Location | Role |
|-----------|----------|------|
| `SkillStateTracker` | `hub/src/system_tool.rs` | Tracks which skills are loaded at runtime |
| `SystemToolContext` | `hub/src/system_tool.rs` | Shared context for all system tools |
| `BuiltinSkillMeta` | `hub/src/skill_loader.rs` | Metadata catalog of available skills |
| `ToolRegistry` | `agent-runtime/src/tools/registry.rs` | Runtime tool store with add/remove support |
| `Vault` | `hub/src/vault.rs` | AES-256-GCM encrypted credential storage |
| `installer` | `skills/src/installer.rs` | Install/uninstall external skills from git/local |
| `scanner` | `skills/src/scanner.rs` | Discover SKILL.md-based skills from filesystem |
| `ExternalSkills` | `hub/src/external_skills.rs` | The live external skill set every agent shares, refreshed after each write |
| `SkillCatalog` | `hub/src/skill_catalog.rs` | List and install skills from `[[skills.catalogs]]` |
| `gating` | `skills/src/gating.rs` | Verify skill requirements (bins, env vars, config) |

### Thread Safety

All shared state uses `std::sync::RwLock`:

- **ToolRegistry** — concurrent reads for tool lookup, exclusive writes for register/unregister
- **Vault** — concurrent reads for `get`/`list`, exclusive writes for `set`/`delete`
- **SkillStateTracker** — concurrent reads for status checks, exclusive writes for load/unload

### Data Flow

```
Agent calls system.enable_skill("github")
  |
  v
SystemEnableSkillTool::execute()
  |
  +-- If already loaded: auto-unload (unload_skill + mark_unloaded)
  +-- skill_loader::load_skill_from_vault("github", vault, config, registry)
  |     |
  |     +-- vault.get("skills/github/token")  →  decrypt from vault
  |     +-- GithubSkill::new(config)          →  initialize HTTP client
  |     +-- skill.tools()                      →  create 13 tool instances
  |     +-- registry.register(tool)            →  add each tool to live registry
  |
  +-- SkillStateTracker::mark_loaded("github", tool_names, "vault")
  |
  v
Tools immediately available for the agent's next turn
```

## Related Documentation

- [VAULT.md](VAULT.md) — Vault encryption, key management, CLI commands, agent tools
- [POLICY_ENGINE.md](POLICY_ENGINE.md) — Access control for tools including `system.*`
- [TOOLS.md](TOOLS.md) — Tool system overview and built-in tool reference
- [CONFIGURATION.md](CONFIGURATION.md) — `workforce.toml` configuration reference
