# Datadog Integration

The Datadog skill gives agents 31 tools across the Datadog REST API:
metrics queries, log search, monitor management, dashboards, events,
incidents, SLOs, hosts, downtimes, and Synthetics. It supports multiple
Datadog accounts in a single deployment, each bound to its own region
(US1/US3/US5/EU1/AP1).

## Quick Start

1. Generate API and Application keys in the Datadog UI:
   - API key: <https://app.datadoghq.com/organization-settings/api-keys>
   - App key: <https://app.datadoghq.com/personal-settings/application-keys>
2. Store credentials in the vault:
   ```bash
   workforce vault set skills/datadog/api_key <api-key>
   workforce vault set skills/datadog/app_key <app-key>
   workforce vault set skills/datadog/site    us1
   ```
3. Enable the skill via an agent or CLI:
   ```
   You:   "Enable Datadog tools."
   Agent: system.enable_skill("datadog")    # loads 31 tools
   ```
4. Use it:
   ```
   You:   "Show me CPU usage averaged by host over the last hour."
   Agent: datadog.metrics_query(query="avg:system.cpu.user{*} by {host}",
                                from="now-1h", to="now")
   ```

## Authentication

Datadog requires two headers on most API endpoints:

- **`DD-API-KEY`** — org-scoped API key. Used for both metric submission and
  reads.
- **`DD-APPLICATION-KEY`** — user-scoped Application key. Required for
  monitors, dashboards, SLOs, incidents, and most read endpoints beyond raw
  metric submission. Application keys can be created with restricted scopes
  (e.g. `monitors_read` only) — if a tool returns `403 Forbidden` despite
  valid credentials, check that your Application key has the right scope.

Both keys are mandatory. The skill validates that both are present before
constructing the client.

## Sites and Regions

Each Datadog account is bound to one region. The `site` field maps to the
right API base URL:

| Accepted `site` values | Base URL |
|------------------------|----------|
| `us1` / `datadoghq.com` (default) | `https://api.datadoghq.com` |
| `us3` / `us3.datadoghq.com` | `https://api.us3.datadoghq.com` |
| `us5` / `us5.datadoghq.com` | `https://api.us5.datadoghq.com` |
| `eu1` / `datadoghq.eu` | `https://api.datadoghq.eu` |
| `ap1` / `ap1.datadoghq.com` | `https://api.ap1.datadoghq.com` |

Both the short key (`eu1`) and the hostname form (`datadoghq.eu`) are
accepted — use whichever Datadog's UI shows for your account. Full API
hostnames (`api.datadoghq.eu`) and pasted URLs work too. **Unknown values
error at skill load time** rather than silently routing to the wrong region.

For FedRAMP, `ddog-gov.com`, or custom domains, set `base_url` directly to
bypass site resolution entirely.

Cross-region calls are not possible — an account is bound to one region. To
work across regions, configure multiple accounts.

## Vault Setup

### Single account

```bash
workforce vault set skills/datadog/api_key  <api-key>
workforce vault set skills/datadog/app_key  <app-key>
workforce vault set skills/datadog/site     us1               # optional, default us1
workforce vault set skills/datadog/base_url https://api.ddog-gov.com  # optional override
```

### Multi account

```bash
workforce vault set skills/datadog/accounts/prod-us/api_key <api-key>
workforce vault set skills/datadog/accounts/prod-us/app_key <app-key>
workforce vault set skills/datadog/accounts/prod-us/site    us1

workforce vault set skills/datadog/accounts/prod-eu/api_key <api-key>
workforce vault set skills/datadog/accounts/prod-eu/app_key <app-key>
workforce vault set skills/datadog/accounts/prod-eu/site    eu1

workforce vault set skills/datadog/accounts/staging/api_key <api-key>
workforce vault set skills/datadog/accounts/staging/app_key <app-key>
workforce vault set skills/datadog/accounts/staging/site    us1

workforce vault set skills/datadog/default_account          prod-us
```

After adding or rotating credentials, run `system.enable_skill("datadog")`.
The skill auto-discovers all configured accounts.

If a per-account vault entry is incomplete (missing `app_key`), that account
is skipped at startup with a warning — the rest of the skill still loads.
Errors in one account never break the others.

## Multi-Account Selection

Every Datadog tool accepts an optional `account` parameter:

- If supplied, it must match a configured account or the call errors.
- If omitted, the call goes to `default_account`.

Unlike the Jira skill, Datadog has no natural prefix to auto-route from
(no analog to `ENG-123`). Agents must specify the account explicitly when
working with non-default accounts.

## Default Policy Split

This is the only built-in skill that ships with a non-uniform default policy.
Read tools default to `allow`; write/destructive tools default to
`require_approval` because Datadog ops actions touch live production:

| Default `allow` (19 tools) | Default `require_approval` (12 tools) |
|---|---|
| `metrics_query`, `metrics_scalar`, `metrics_list`, `metrics_metadata` | `monitors_create`, `monitors_update`, `monitors_delete` |
| `logs_search`, `logs_aggregate` | `monitors_mute`, `monitors_unmute` |
| `monitors_list`, `monitors_get`, `monitors_search` | `dashboards_create` |
| `dashboards_list`, `dashboards_get` | `events_post` |
| `events_list` | `hosts_mute`, `hosts_unmute` |
| `incidents_list`, `incidents_get` | `downtimes_create`, `downtimes_cancel` |
| `slos_list`, `slos_get` | `datadog.api` |
| `hosts_list`, `downtimes_list`, `synthetics_list` | |

The escape hatch `datadog.api` is gated as a write because it can call any
endpoint. Override these defaults in your agent config if you want a tighter
or looser policy.

## Time-Range Parameters

Tools that take `from`/`to`/`start`/`end` accept any of:

- **Unix epoch seconds** — integer or numeric string
- **Relative shorthand** — `now`, `now-15m`, `now-1h`, `now-2d`, `now-1w`,
  `now-30s`, `now+1h`. Units: `s`/`m`/`h`/`d`/`w`.
- **ISO-8601 strings** — e.g. `2026-01-01T00:00:00Z`. These are passed
  through unchanged for v2 logs/events search and converted to epoch seconds
  for v1 endpoints that need integers.

For v2 metrics endpoints (`metrics_query`, `metrics_scalar`), epoch seconds
are converted to milliseconds before posting to the API.

## Pagination

Different Datadog endpoints use different pagination styles. The skill
exposes them as-is:

- **Logs / events search (v2)** — cursor pagination. Pass `cursor` from the
  response back into the next call.
- **Monitors** — offset/limit (`page` + `page_size`).
- **Hosts** — offset/limit (`start` + `count`).
- **Incidents** — page-size only.

## Custom Monitor Options

Datadog monitors have a large `options` object covering thresholds,
notification settings, evaluation delay, recovery thresholds, etc.
`monitors_create` and `monitors_update` accept a passthrough `options`
object so agents can configure any field without the skill needing to
model the full schema:

```json
{
  "type": "metric alert",
  "query": "avg(last_5m):avg:system.cpu.user{*} by {host} > 90",
  "name": "High CPU",
  "message": "CPU is high on {{host.name}} @slack-ops",
  "options": {
    "thresholds": { "critical": 90, "warning": 80 },
    "notify_no_data": true,
    "no_data_timeframe": 10,
    "evaluation_delay": 60
  }
}
```

See the [Datadog monitor API
reference](https://docs.datadoghq.com/api/latest/monitors/) for the full
options schema.

## Custom Dashboard Widgets

Datadog's widget JSON schema is large. `dashboards_create` accepts a
passthrough `widgets` array so you can copy the shape from a working
dashboard or the widget reference rather than constructing it from scratch:

```json
{
  "title": "Service Health",
  "layout_type": "ordered",
  "widgets": [
    {
      "definition": {
        "type": "timeseries",
        "requests": [{ "q": "avg:system.cpu.user{*}" }],
        "title": "CPU"
      }
    }
  ]
}
```

## Tools

### Metrics

| Tool | Endpoint | Notes |
|------|----------|-------|
| `datadog.metrics_query` | `POST /api/v2/query/timeseries` | Datadog query syntax (`avg:system.cpu.user{*} by {host}`). Returns one or more series. |
| `datadog.metrics_scalar` | `POST /api/v2/query/scalar` | One value per group. `aggregator` defaults to `avg`. |
| `datadog.metrics_list` | `GET /api/v1/metrics` | List metric names that have reported since `from`. |
| `datadog.metrics_metadata` | `GET /api/v1/metrics/{name}` | Unit, type, description for a single metric. |

### Logs

| Tool | Endpoint | Notes |
|------|----------|-------|
| `datadog.logs_search` | `POST /api/v2/logs/events/search` | Cursor pagination via `cursor`. Defaults: `from=now-15m`, `to=now`, `limit=50`. |
| `datadog.logs_aggregate` | `POST /api/v2/logs/analytics/aggregate` | Computed aggregations + group-by. |

### Monitors

| Tool | Endpoint | Notes |
|------|----------|-------|
| `datadog.monitors_list` | `GET /api/v1/monitor` | Filter by name, tags, monitor_tags. |
| `datadog.monitors_get` | `GET /api/v1/monitor/{id}` | Full details. |
| `datadog.monitors_search` | `GET /api/v1/monitor/search` | Datadog monitor search query language. |
| `datadog.monitors_create` | `POST /api/v1/monitor` | Required: `type`, `query`, `name`, `message`. |
| `datadog.monitors_update` | `PUT /api/v1/monitor/{id}` | Only sends supplied fields. |
| `datadog.monitors_delete` | `DELETE /api/v1/monitor/{id}` | |
| `datadog.monitors_mute` | `POST /api/v1/monitor/{id}/mute` | Optional `scope` and `end` time. |
| `datadog.monitors_unmute` | `POST /api/v1/monitor/{id}/unmute` | Optional `scope` or `all_scopes`. |

### Dashboards

| Tool | Endpoint | Notes |
|------|----------|-------|
| `datadog.dashboards_list` | `GET /api/v1/dashboard` | |
| `datadog.dashboards_get` | `GET /api/v1/dashboard/{id}` | Full definition. |
| `datadog.dashboards_create` | `POST /api/v1/dashboard` | Required: `title`, `layout_type`. Pass `widgets` as raw JSON. |

### Events / incidents / SLOs

| Tool | Endpoint | Notes |
|------|----------|-------|
| `datadog.events_list` | `GET /api/v1/events` | Required: `from`, `to`. Optional priority/sources/tags filters. |
| `datadog.events_post` | `POST /api/v1/events` | Required: `title`, `text`. |
| `datadog.incidents_list` | `GET /api/v2/incidents` | |
| `datadog.incidents_get` | `GET /api/v2/incidents/{id}` | |
| `datadog.slos_list` | `GET /api/v1/slo` | Optional `query` filter. |
| `datadog.slos_get` | `GET /api/v1/slo/{id}` | |

### Hosts / downtimes / synthetics

| Tool | Endpoint | Notes |
|------|----------|-------|
| `datadog.hosts_list` | `GET /api/v1/hosts` | Tag filter via `filter`. |
| `datadog.hosts_mute` | `POST /api/v1/host/{name}/mute` | Optional `end` and `message`. |
| `datadog.hosts_unmute` | `POST /api/v1/host/{name}/unmute` | |
| `datadog.downtimes_list` | `GET /api/v2/downtime` | `current_only=true` filters to active. |
| `datadog.downtimes_create` | `POST /api/v2/downtime` | Provide either `monitor_id` **or** `scope`, plus `start` + `end`. |
| `datadog.downtimes_cancel` | `DELETE /api/v2/downtime/{id}` | |
| `datadog.synthetics_list` | `GET /api/v1/synthetics/tests` | API + Browser tests. |

### Escape hatch

| Tool | Notes |
|------|-------|
| `datadog.api` | Arbitrary REST request. Pass `method`, `path` (including `/api/v1` or `/api/v2` prefix), optional `body` and `query`. Use sparingly — prefer the dedicated tools so policies and field wrapping apply. |

## Example Agent Prompts

```
"What's the p95 latency on the checkout service over the last 4 hours?"
  → datadog.metrics_query(query="p95:trace.servlet.request{service:checkout}",
                          from="now-4h", to="now")

"Show me error logs from the web service in the last 15 minutes."
  → datadog.logs_search(query="service:web status:error",
                        from="now-15m", to="now")

"List monitors that are currently in alert in prod-eu."
  → datadog.monitors_search(query="status:alert", account="prod-eu")

"Mute the host i-0abc123 for 30 minutes."
  → datadog.hosts_mute(host_name="i-0abc123", end="now+30m")
  → triggers require_approval

"Schedule a 1-hour downtime for monitor 12345 starting now with message 'deploy'."
  → datadog.downtimes_create(monitor_id=12345, start="now", end="now+1h",
                             message="deploy")
  → triggers require_approval

"Post a deploy event tagged env:prod and service:web."
  → datadog.events_post(title="Deploy v2.3.0", text="...",
                        tags=["env:prod","service:web"], alert_type="info")
  → triggers require_approval
```

## Security Considerations

- **External content wrapping.** Every string field returned from Datadog
  that contains user-authored content (monitor names/messages, dashboard
  titles, log messages, event text, incident summaries, SLO descriptions)
  is passed through `wrap_external_content()` before being exposed to the
  agent. This protects against prompt-injection attempts in Datadog data.
- **Default policies.** Write tools require approval out of the box. Don't
  blindly relax these — Datadog ops actions affect live production.
- **Application key scopes.** Restrict the Application key to the minimum
  scopes the deployment needs (e.g. `monitors_read` for read-only agents).
  The skill will return clean `403 Forbidden` errors that explicitly
  mention scopes when a write tool hits a read-only key.
- **Rate limits.** Datadog enforces per-endpoint rate limits with
  `X-RateLimit-Remaining` headers and `Retry-After` on 429 responses. The
  skill surfaces 429 errors with the retry window so agents can back off.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Authentication failed` | Wrong API or App key | Re-generate keys in the Datadog UI and re-set the vault entries. |
| `Forbidden (check API/Application key scopes)` | App key has restricted scopes | Either grant the missing scope or use a different App key. |
| `Bad request: Invalid query` on metrics | Datadog query syntax error | Test the query in the Datadog UI's metric explorer first. |
| `Rate limited by Datadog: … (quota <name> N per Ps, M left, resets in Rs)` | Hit the per-endpoint rate limit; the quota headers say which and when it clears | Back off until the reset and retry. Don't loop tightly. |
| Skipped account at startup | Missing `app_key` for that account | Hub logs `Skipping Datadog account: missing app_key`. Set the missing key and re-enable the skill. |
| Cross-region call fails | Each account is bound to one region | Configure separate accounts per region and pass `account=<name>` explicitly. |
| `Not found` on a known monitor | Wrong account | Pass `account=<name>` explicitly — there's no auto-routing for Datadog. |

## Related Documentation

- [SKILLS.md](SKILLS.md) — Built-in skill catalogue and lifecycle
- [VAULT.md](VAULT.md) — Vault encryption and CLI
- [JIRA.md](JIRA.md) — Sister skill with the same multi-tenant pattern
- [POLICY_ENGINE.md](POLICY_ENGINE.md) — Tool access control and policy overrides
