# Metabase Integration

The Metabase skill gives agents 22 tools across the Metabase REST API:
cards (saved questions), dashboards, collections, databases, schema
introspection, global search, and raw SQL execution. It supports multiple
Metabase deployments (e.g. prod, staging, per-client) in a single workforce
deployment via an optional `instance` parameter on every tool.

## Quick Start

1. Generate an API key in Metabase at
   `/admin/settings/authentication/api-keys` (requires Metabase v0.49+).
2. Store credentials in the vault:
   ```bash
   workforce vault set skills/metabase/api_key  <mb_...>
   workforce vault set skills/metabase/site_url https://mb.example.com
   ```
3. Enable the skill:
   ```
   You:   "Enable Metabase tools."
   Agent: system.enable_skill("metabase")    # loads 22 tools
   ```
4. Use it:
   ```
   You:   "Run card 142 and tell me what the weekly signup trend looks like."
   Agent: metabase.card_query(id=142)
   ```

## Authentication

Metabase v0.49+ supports scoped API keys. The skill sends them as the
`X-API-KEY` header on every request — no session cookies, no OAuth.

API keys are scoped to the user or group they are created under and inherit
those permissions. For production deployments, create a dedicated service
user with read-only access to the databases and collections the agent needs,
then generate the key from that user. If agents will mutate cards or run
native SQL, grant the matching permissions explicitly — the skill will
surface `403 Forbidden` errors with the affected resource in the message.

Metabase versions before v0.49 do not support API keys. The skill is not
compatible with those deployments.

## Instances

Each Metabase deployment (a distinct `site_url`) is a separate **instance**.
The skill supports two modes:

- **Single-instance** — one API key + site URL stored at flat vault paths.
  Every tool call targets that one deployment.
- **Multi-instance** — named instances under
  `skills/metabase/instances/<name>/...`. Every tool accepts an optional
  `instance: "<name>"` parameter.

Instance resolution precedence:

1. **Explicit `instance` parameter** — wins; errors if the name is unknown.
2. **`default_instance`** — falls back to the configured default.

Unlike Jira and Linear, Metabase has no natural routing keys (e.g. ticket
prefixes), so there is no prefix auto-routing. Agents must pass `instance`
explicitly when targeting a non-default deployment, or the workforce
operator must configure `default_instance` appropriately per environment.

## Vault Setup

### Single instance

```bash
workforce vault set skills/metabase/api_key  <mb_...>
workforce vault set skills/metabase/site_url https://mb.example.com
```

Optional: override the row cap for `card_query` / `dashboard_query` /
`dataset_query_native`:

```bash
workforce vault set skills/metabase/max_query_rows 500
```

### Multi instance

```bash
workforce vault set skills/metabase/instances/prod/api_key         <mb_...>
workforce vault set skills/metabase/instances/prod/site_url        https://mb.prod.example.com

workforce vault set skills/metabase/instances/staging/api_key      <mb_...>
workforce vault set skills/metabase/instances/staging/site_url     https://mb.staging.example.com
workforce vault set skills/metabase/instances/staging/max_query_rows 500

workforce vault set skills/metabase/default_instance               prod
```

After adding or rotating credentials, run `system.enable_skill("metabase")`
(or ask an agent to do it). The skill auto-discovers all configured
instances from the vault.

## Row Cap and Query Safety

`metabase.card_query`, `metabase.dashboard_query`, and
`metabase.dataset_query_native` are the only tools that return row data.
All three enforce a **hard ceiling of 2000 rows** on every call,
regardless of what the caller asks for. The effective cap is:

```
min(caller-supplied limit, instance.max_query_rows, 2000)
```

A `limit` of `0` or omitted means "use the cap". The cap is echoed on every
response as `row_cap` so the agent can reason about truncation. The
`data.rows_truncated` flag from Metabase is preserved when present.

This matters because a well-meaning "select everything" query against a
50M-row table would otherwise blow out the agent's context window in one
tool call. The cap is enforced server-side via Metabase's `constraints`
object — it's not a client-side slice after the fact.

To raise the effective cap for a specific instance, set
`skills/metabase/instances/<name>/max_query_rows`. Values above 2000 are
silently clamped down.

## Dataset Queries

`card_create` and `card_update` take a `dataset_query` object. This is
Metabase's native query shape, not a flat SQL string. The two common forms:

**Native (raw SQL):**

```json
{
  "database": 3,
  "type": "native",
  "native": {
    "query": "SELECT count(*) FROM users WHERE created_at > {{since}}",
    "template-tags": {
      "since": {
        "id": "abc-123",
        "name": "since",
        "display-name": "Since",
        "type": "date"
      }
    }
  }
}
```

**Structured (MBQL query builder):**

```json
{
  "database": 3,
  "type": "query",
  "query": {
    "source-table": 42,
    "aggregation": [["count"]],
    "filter": [">", ["field", 101, null], "2024-01-01"]
  }
}
```

For `metabase.dataset_query_native` and parameterised card queries, pass
runtime values via the `parameters` array:

```json
"parameters": [
  { "type": "date/single", "target": ["variable", ["template-tag", "since"]], "value": "2024-01-01" }
]
```

See [Metabase's API reference](https://www.metabase.com/learn/metabase-basics/administration/administration-and-operation/metabase-api)
for the full MBQL schema and parameter types.

## Tools

| Tool | Endpoint | Notes |
|------|----------|-------|
| `metabase.card_list` | `GET /api/card` | Filters by `collection_id` client-side. `f=archived` vs `f=all`. |
| `metabase.card_get` | `GET /api/card/{id}` | Description wrapped. |
| `metabase.card_query` | `POST /api/card/{id}/query` | Row-capped. `parameters` array supported. |
| `metabase.card_create` | `POST /api/card` | **Mutating.** Requires `name` + `dataset_query`. |
| `metabase.card_update` | `PUT /api/card/{id}` | **Mutating.** Partial update — null fields are skipped. |
| `metabase.card_archive` | `PUT /api/card/{id}` | **Mutating.** Sends `{archived: true/false}`. |
| `metabase.dashboard_list` | `GET /api/search?models=dashboard` | Stable cross-version list path. |
| `metabase.dashboard_get` | `GET /api/dashboard/{id}` | Includes `dashcards`. |
| `metabase.dashboard_query` | `POST .../dashcard/{dc}/card/{c}/query` | Executes every dashcard; per-card row cap. |
| `metabase.dashboard_create` | `POST /api/dashboard` | **Mutating.** Accepts `dashcards`/`parameters`/`tabs`/`collection_position`. |
| `metabase.dashboard_update` | `PUT /api/dashboard/{id}` | **Mutating.** Partial update; `dashcards` is a **full-layout replacement** (new cards use negative temp ids). |
| `metabase.dashboard_archive` | `PUT /api/dashboard/{id}` | **Mutating.** |
| `metabase.dashboard_add_cards` | `GET` then `PUT /api/dashboard/{id}` | **Mutating.** Read-merge-write append; auto-stacks positionless cards below the layout. |
| `metabase.dashboard_remove_card` | `GET` then `PUT /api/dashboard/{id}` | **Mutating.** Read-merge-write removal by dashcard id; errors if absent. |
| `metabase.collection_list` | `GET /api/collection` | |
| `metabase.collection_get` | `GET /api/collection/{id}` | Accepts integer ID or `"root"`. |
| `metabase.collection_items` | `GET /api/collection/{id}/items` | `models` filter (card, dashboard, collection, dataset, snippet). |
| `metabase.collection_create` | `POST /api/collection` | **Mutating.** |
| `metabase.collection_update` | `PUT /api/collection/{id}` | **Mutating.** Can archive via `archived: true`. |
| `metabase.database_list` | `GET /api/database` | |
| `metabase.database_tables` | `GET /api/database/{id}/metadata` | Slimmed: id/name/schema/description per table. |
| `metabase.table_fields` | `GET /api/table/{id}/query_metadata` | Slimmed: id/name/type/semantic_type/description per field. |
| `metabase.dataset_query_native` | `POST /api/dataset` | **Mutating execute.** Raw SQL. Row-capped. |
| `metabase.search` | `GET /api/search` | Global search. `models` filter. |

All 22 tools accept an optional `instance` parameter.

## Example Agent Prompts

```
"Find the top dashboards about revenue."
  → metabase.search(query="revenue", models=["dashboard"])

"Run card 142 and summarise what the trend looks like."
  → metabase.card_query(id=142)

"Create a new question from this SQL in the Analytics collection."
  → metabase.collection_list()                       # find Analytics collection ID
  → metabase.database_list()                         # find target DB ID
  → metabase.card_create(name="Weekly signups",
                         collection_id=<id>,
                         dataset_query={ "database": <db>, "type": "native",
                                         "native": { "query": "SELECT ..." } })

"Archive every card in the 'Sandbox' collection."
  → metabase.collection_items(id=<sandbox-id>, models=["card"])
  → metabase.card_archive(id=<each>)  # one call per card

"What columns does the users table have? It's in database 3."
  → metabase.database_tables(database_id=3)
  → metabase.table_fields(table_id=<users-id>)
```

In a multi-instance setup, add `instance="staging"` (etc.) to any of these
calls to route away from the default deployment.

## Security Considerations

- **External content wrapping.** Card and dashboard `description` fields,
  plus every row returned by `card_query` / `dashboard_query` /
  `dataset_query_native`, are passed through `wrap_external_content()`
  before being exposed to the agent. This defends against prompt-injection
  attempts embedded in Metabase data. Query result rows cross the
  trust boundary as a single `rows_wrapped` string, not the raw `rows`
  array — there is no unwrapped row surface for an agent to read around the
  markers.
- **Row cap.** Hard ceiling of 2000 rows on every query-returning tool. Per-
  instance overrides may lower but never raise the cap.
- **Default policies.** All 22 tools default to `allow`. Mutating tools
  (`*_create`, `*_update`, `*_archive`, `dataset_query_native`) are clearly
  marked in their descriptions — operators who want a confirmation gate
  should downgrade those to `require_approval` in their agent config,
  mirroring the Datadog skill's pattern (see [DATADOG.md](DATADOG.md)).
- **API-key scope.** Metabase API keys inherit the permissions of the user
  or group they are created under. Use a least-privilege service account
  for production deployments — the skill will return actionable `403
  Forbidden` errors naming the gated resource when the key is too narrow.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Authentication failed — check Metabase api_key` | Wrong or revoked key, or Metabase < v0.49 | Re-generate the key at `/admin/settings/authentication/api-keys`. Upgrade to v0.49+ if older. |
| `Forbidden (check collection/database permissions)` | API key's user/group lacks access to that collection or database | Grant the scope in Metabase admin → Permissions, then retry. |
| `Not found` on a known resource | Wrong instance in a multi-instance setup | Pass `instance="<name>"` explicitly, or set `default_instance`. |
| Query returned fewer rows than requested | The 2000-row hard cap or a lower per-instance `max_query_rows` | Check `row_cap` on the response. Raise `max_query_rows` (up to 2000) or narrow the query. |
| `dashboard_query` returns empty `results` | Dashboard has no dashcards, or dashcards are missing `card_id` | Call `dashboard_get` first to inspect the dashcards payload. |
| Skipped instance at startup | Missing `site_url` for that instance | Hub logs `Skipping Metabase instance: missing site_url`. Set the key and re-run `system.enable_skill("metabase")`. |
| `card_create` 400 errors | Malformed `dataset_query` object | Fetch an existing working card via `card_get` and mirror its `dataset_query` shape. |

## Related Documentation

- [SKILLS.md](SKILLS.md) — Built-in skill catalogue and lifecycle
- [VAULT.md](VAULT.md) — Vault encryption and CLI
- [JIRA.md](JIRA.md) — Sister multi-tenant skill with prefix auto-routing
- [DATADOG.md](DATADOG.md) — Sister multi-account skill; same policy-split pattern
- [POLICY_ENGINE.md](POLICY_ENGINE.md) — Tool access control
