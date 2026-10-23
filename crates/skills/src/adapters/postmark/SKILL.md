# Postmark Skill

Agent-callable tools for the Postmark REST API — transactional email sending,
delivery status lookup, bounce inspection, stats, templates, message streams,
suppression management, and (with an account token) server + domain
inspection. Supports multiple Postmark servers side-by-side.

## Authentication

Postmark uses two distinct tokens:

| Header | Scope | Where to get it |
|---|---|---|
| `X-Postmark-Server-Token` | Per-server: send, messages, stats, bounces, templates, streams, suppressions | Postmark → Servers → *your server* → API Tokens |
| `X-Postmark-Account-Token` | Account-wide: servers list, domains, sender signatures, data removal | Postmark → Account → API Tokens |

The **server token is required**. The **account token is optional**; without
it, the four account-scoped tools (`servers_list`, `domains_list`,
`domain_get`, `postmark.api` with `token_type=account`) return a clear runtime
error but every other tool keeps working.

## Vault setup

### Single workspace

```text
vault.set('skills/postmark/server_token',  '<pm_...>')
vault.set('skills/postmark/account_token', '<pm_...>')   # optional
vault.set('skills/postmark/base_url',      'https://api.postmarkapp.com')  # optional
```

### Multi workspace

Each Postmark server becomes one named workspace. You can mix server-only and
server+account workspaces freely.

```text
vault.set('skills/postmark/workspaces/prod/server_token',  '<pm_prod_...>')
vault.set('skills/postmark/workspaces/prod/account_token', '<pm_acct_...>')
vault.set('skills/postmark/workspaces/staging/server_token', '<pm_staging_...>')

vault.set('skills/postmark/default_workspace', 'prod')  # only with several workspaces
```

Tools accept an optional `workspace` parameter that targets a named
workspace. Routing is always explicit — Postmark IDs have no stable
per-tenant prefix, so there is no auto-routing by message ID.

After adding or changing credentials run `system.enable_skill('postmark')` to
reload.

## Tool catalogue

### Messages — outbound

| Tool | Purpose |
|---|---|
| `postmark.messages_outbound_search` | Search sent messages by recipient, sender, tag, subject, status, stream, date range. |
| `postmark.message_outbound_get` | Full details of one sent message. |
| `postmark.message_outbound_dump` | Raw MIME body (30-day retention). |
| `postmark.messages_outbound_opens` | Open events (global or scoped to a MessageID). |
| `postmark.messages_outbound_clicks` | Click-tracking events (global or scoped). |

### Messages — inbound

| Tool | Purpose |
|---|---|
| `postmark.messages_inbound_search` | Search inbound messages by recipient, sender, tag, subject, mailbox hash, status. |
| `postmark.message_inbound_get` | Full details of one inbound message. |

### Email send (`require_approval`)

| Tool | Purpose |
|---|---|
| `postmark.email_send` | Single `/email` POST. Requires at least one of `HtmlBody` or `TextBody`. |
| `postmark.email_send_batch` | Up to 500 emails in one `/email/batch` POST. |
| `postmark.email_send_template` | `/email/withTemplate` POST. One of `TemplateId` or `TemplateAlias` plus a `TemplateModel`. |

### Bounces

| Tool | Purpose |
|---|---|
| `postmark.bounces_list` | List bounces with filters (`type`, `inactive`, `emailFilter`, `tag`, `messageID`, date range). |
| `postmark.bounce_get` | Details of one bounce by integer ID. |
| `postmark.bounce_activate` | Re-activate a suppressed recipient. `require_approval`. |

### Stats

| Tool | Purpose |
|---|---|
| `postmark.delivery_stats` | `/deliverystats` rollup by bounce type + inactive count. |
| `postmark.stats_outbound` | `/stats/outbound` sending summary. |
| `postmark.stats_bounces` | `/stats/outbound/bounces` by type. |
| `postmark.stats_opens` | `/stats/outbound/opens` totals + uniques. |
| `postmark.stats_clicks` | `/stats/outbound/clicks` totals + uniques. |

### Templates

| Tool | Purpose |
|---|---|
| `postmark.templates_list` | List templates with optional filter. |
| `postmark.template_get` | Get one template by numeric ID or alias. |

### Streams & suppressions

| Tool | Purpose |
|---|---|
| `postmark.message_streams_list` | List streams (transactional, broadcast, inbound). |
| `postmark.suppressions_dump` | Dump all suppressions on a stream. |
| `postmark.suppressions_add` | Add addresses to a stream's suppression list. `require_approval`. |
| `postmark.suppressions_delete` | Remove addresses (note: HTTP POST, not DELETE). `require_approval`. |

### Account-scoped (requires `account_token`)

| Tool | Purpose |
|---|---|
| `postmark.servers_list` | List Postmark servers on the account. |
| `postmark.domains_list` | List sender domains. |
| `postmark.domain_get` | Get DKIM/SPF/ReturnPath verification flags for one domain. |

### Escape hatch (`require_approval`)

| Tool | Purpose |
|---|---|
| `postmark.api` | Arbitrary REST call. `token_type: "server"` (default) or `"account"`. |

## Default policies

Reads are `allow`. Writes and side-effecting operations are
`require_approval`: `email_send`, `email_send_batch`, `email_send_template`,
`bounce_activate`, `suppressions_add`, `suppressions_delete`, `postmark.api`.

## Gotchas

- **Date filters are Eastern Time.** `fromdate` / `todate` on messages,
  bounces, opens, clicks, and stats — all ET, not UTC. Postmark is explicit
  about this. Pass ISO date / datetime strings.
- **Pagination cap.** `count + offset` must be ≤ 10 000 on
  `/messages/outbound`, `/messages/inbound`, and `/bounces`. To scan
  further, narrow the date window and paginate within each window. The tools
  return a clear error when you exceed this upfront.
- **Batch ceiling.** `/email/batch` is capped at 500 messages per request.
- **Body requirement.** `/email` requires at least one non-empty `HtmlBody`
  or `TextBody`. Sending both is recommended.
- **Suppression delete is POST**, not HTTP DELETE — the tool handles this.
- **Retention.** Messages and bounces: 45 days default (configurable 7–365
  in Postmark). Raw MIME dumps: 30 days.
- **Rate limits.** Postmark returns 429 without publishing a numeric cap.
  Back off and retry; the client surfaces the `Retry-After` header in the
  error message.
- **Status latency.** A `MessageID` returned by `/email` is queryable
  seconds later via `message_outbound_get`. Opens and clicks depend on
  recipient behavior and can lag minutes to hours — or never fire.

## `postmark.api` token routing

The escape hatch takes a `token_type` parameter:

- `token_type: "server"` (default) — attaches `X-Postmark-Server-Token`.
- `token_type: "account"` — attaches `X-Postmark-Account-Token`. Errors
  cleanly if the workspace has no account token.

## Example agent prompts

- "Search Postmark outbound to jane@example.com in the last 7 days."
- "Get full details of Postmark MessageID `a1b2c3d4-...`"
- "What's the bounce list for hard bounces this week?"
- "Show delivery stats for the last 30 days."
- "Send a transactional Postmark email from `alerts@acme.com` to
  `oncall@acme.com` with subject 'Heartbeat OK'." *(`require_approval`)*
- "List suppressions on the outbound stream." *(requires stream_id)*
- "Which sender domains are unverified?" *(requires `account_token`)*
