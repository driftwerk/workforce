# Datadog Skill

Agent-callable tools for the Datadog REST API. Covers metrics, logs, monitors,
dashboards, events, incidents, SLOs, hosts, downtimes, and Synthetics across
multiple Datadog accounts and regions.

## Authentication

Datadog requires two headers on most APIs:

- `DD-API-KEY` — per-org API key (org-scoped)
- `DD-APPLICATION-KEY` — per-user Application key (scoped to your user, may have
  capability scopes attached)

Both are required. Generate them at:

- API key: https://app.datadoghq.com/organization-settings/api-keys
- App key: https://app.datadoghq.com/personal-settings/application-keys

## Sites (regions)

Each account is bound to one Datadog region. The `site` config field maps to
the right API base URL.

| Site key | Base URL |
|---|---|
| `us1` / `datadoghq.com` (default) | `https://api.datadoghq.com` |
| `us3` / `us3.datadoghq.com` | `https://api.us3.datadoghq.com` |
| `us5` / `us5.datadoghq.com` | `https://api.us5.datadoghq.com` |
| `eu1` / `datadoghq.eu` | `https://api.datadoghq.eu` |
| `ap1` / `ap1.datadoghq.com` | `https://api.ap1.datadoghq.com` |

Both the short key (`eu1`) and the hostname form (`datadoghq.eu`) are
accepted — use whichever matches what Datadog's UI shows for your account.
Full API hostnames (`api.datadoghq.eu`) and pasted URLs
(`https://api.datadoghq.eu/`) also work. Unknown values error at skill
load time with a clear message.

For FedRAMP or custom domains, set `base_url` directly to bypass site
resolution entirely.

## Vault setup

### Single account

```text
vault.set('skills/datadog/api_key', '<api-key>')
vault.set('skills/datadog/app_key', '<app-key>')
vault.set('skills/datadog/site',    'us1')        # optional, default us1
```

The API key is a bare 32-char hex string with no distinguishing prefix,
so the ingress secret scanner can't recognise it the way it does
prefixed tokens. When configuring over chat, hand it to the agent with
`/secret datadog_api_key <api-key>` first, then reference
`[[secret:datadog_api_key]]` in the `vault.set` call — that keeps the
value out of the model.

### Multi account

```text
vault.set('skills/datadog/accounts/prod-us/api_key', '<api-key>')
vault.set('skills/datadog/accounts/prod-us/app_key', '<app-key>')
vault.set('skills/datadog/accounts/prod-us/site',    'us1')

vault.set('skills/datadog/accounts/prod-eu/api_key', '<api-key>')
vault.set('skills/datadog/accounts/prod-eu/app_key', '<app-key>')
vault.set('skills/datadog/accounts/prod-eu/site',    'eu1')

vault.set('skills/datadog/default_account', 'prod-us')  # only with several accounts
```

After adding or changing credentials, run `system.enable_skill('datadog')` to
reload.

## Tool catalogue (31 tools)

### Metrics
| Tool | Description |
|---|---|
| `datadog.metrics_query` | Timeseries query (v2) — returns one or more series |
| `datadog.metrics_scalar` | Scalar query (v2) — returns one value per group |
| `datadog.metrics_list` | List metric names that have reported recently |
| `datadog.metrics_metadata` | Metadata for a single metric |

### Logs
| Tool | Description |
|---|---|
| `datadog.logs_search` | Search logs (v2) with cursor pagination |
| `datadog.logs_aggregate` | Aggregate logs by computed metrics + groupings |

### Monitors
| Tool | Description |
|---|---|
| `datadog.monitors_list` | List monitors |
| `datadog.monitors_get` | Full details for one monitor |
| `datadog.monitors_search` | Search by query language |
| `datadog.monitors_create` | Create a monitor |
| `datadog.monitors_update` | Update a monitor |
| `datadog.monitors_delete` | Delete a monitor |
| `datadog.monitors_mute` | Mute a monitor |
| `datadog.monitors_unmute` | Unmute a monitor |

### Dashboards
| Tool | Description |
|---|---|
| `datadog.dashboards_list` | List dashboards |
| `datadog.dashboards_get` | Full dashboard definition |
| `datadog.dashboards_create` | Create a dashboard (raw widgets array) |

### Events / incidents / SLOs
| Tool | Description |
|---|---|
| `datadog.events_list` | List events in a time range |
| `datadog.events_post` | Post a custom event |
| `datadog.incidents_list` | List incidents (v2) |
| `datadog.incidents_get` | Single incident |
| `datadog.slos_list` | List SLOs |
| `datadog.slos_get` | Single SLO |

### Hosts / downtimes / synthetics
| Tool | Description |
|---|---|
| `datadog.hosts_list` | List hosts with optional tag filter |
| `datadog.hosts_mute` | Mute a host |
| `datadog.hosts_unmute` | Unmute a host |
| `datadog.downtimes_list` | List downtimes |
| `datadog.downtimes_create` | Schedule a downtime |
| `datadog.downtimes_cancel` | Cancel a downtime |
| `datadog.synthetics_list` | List Synthetic tests |

### Escape hatch
| Tool | Description |
|---|---|
| `datadog.api` | Arbitrary REST request — use sparingly |

## Default policy split

This skill ships with a deliberate split between read and write tools because
Datadog ops actions touch live production:

- **`allow`**: all read/query/list/get/search tools.
- **`require_approval`**: `monitors_create`/`update`/`delete`/`mute`/`unmute`,
  `dashboards_create`, `events_post`, `hosts_mute`/`unmute`,
  `downtimes_create`/`cancel`, and `datadog.api` (since it can be a write).

Override per-skill policies in your agent config if you want a tighter or
looser default.

## Time ranges

Time-range parameters (`from`, `to`, `start`, `end`) accept any of:

- Unix epoch seconds — integer or numeric string
- Relative shorthand: `now`, `now-15m`, `now-1h`, `now-2d`, `now-1w`
- ISO-8601 strings — preserved on v2 logs/events search; converted to epoch
  elsewhere

## Pagination

- **Logs / events search (v2)** use cursor pagination — pass the `cursor`
  returned in the response back into the next call.
- **Monitors** use offset/limit (`page` + `page_size`).
- **Hosts** use `start` + `count`.

## Custom monitor options

`monitors_create` and `monitors_update` accept a passthrough `options` object
so you can set thresholds, notifications, evaluation delay, etc., without the
skill having to model the full schema. See the
[Datadog monitor API reference](https://docs.datadoghq.com/api/latest/monitors/)
for the available fields.

## Custom dashboard widgets

`dashboards_create` accepts a passthrough `widgets` array of raw Datadog
widget JSON. Datadog's widget schema is large; copy the shape from a working
dashboard or the Datadog widget reference rather than constructing it from
scratch.

## Example agent prompts

- "Query `system.cpu.user` averaged by host over the last hour."
- "Search the last 15 minutes of error logs from the `web` service."
- "List monitors that are currently in alert in the `prod-eu` account."
- "Mute the host `i-0abc123` for 30 minutes." → triggers `require_approval`.
- "Schedule a 1-hour downtime for monitor 12345 starting now."
- "Show me dashboards I have access to."
