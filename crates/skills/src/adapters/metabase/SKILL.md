# Metabase Skill

Agent-callable tools for the Metabase REST API. Supports cards (saved questions), dashboards, collections, databases, schema introspection, global search, and raw SQL execution — across **multiple Metabase instances** via a single `instance` parameter.

## Authentication

API key only (Metabase v0.49+). Sent as the `X-API-KEY` header. Generate at `/admin/settings/authentication/api-keys`.

- Single instance: `skills/metabase/api_key` + `skills/metabase/site_url`
- Multi-instance: `skills/metabase/instances/<name>/api_key` + `skills/metabase/instances/<name>/site_url`
- Default selection: `skills/metabase/default_instance` (only with several instances)

Per-instance row cap (optional): `skills/metabase/instances/<name>/max_query_rows` — clamped to a hard ceiling of 50000 regardless of what's configured.

## Multi-instance support

Every tool accepts an optional `instance` parameter. Resolution:

1. Explicit `instance: "<name>"` → that instance.
2. Otherwise → `default_instance` if it names a configured instance, else the instance named `default`, else the sole instance.

There is no prefix auto-routing (Metabase has no natural routing keys).

## Row cap

`metabase.card_query`, `metabase.dashboard_query`, and `metabase.dataset_query_native` enforce a row cap. Calls without an explicit `limit` default to 2000; explicit limits are honored up to `min(instance.max_query_rows, 50000)` — large results are staged as `tabular_artifact`s instead of entering the agent's context. The cap is returned as `row_cap` on every query response.

## Tools

### Cards (saved questions)

| Tool | Parameters | Notes |
|---|---|---|
| `metabase.card_list` | `collection_id?`, `archived?`, `limit?`, `instance?` | Filters client-side by `collection_id`. |
| `metabase.card_get` | `id`, `instance?` | Returns full card. |
| `metabase.card_query` | `id`, `parameters?`, `limit?`, `instance?` | Executes the card; rows wrapped with external-content markers. |
| `metabase.card_create` | `name`, `dataset_query`, `description?`, `collection_id?`, `display?`, `visualization_settings?`, `instance?` | **Mutating.** |
| `metabase.card_update` | `id`, plus any of the create fields | **Mutating.** Partial update. |
| `metabase.card_archive` | `id`, `archived?` (default `true`), `instance?` | **Mutating.** Sends `{archived}`. |

### Dashboards

| Tool | Parameters | Notes |
|---|---|---|
| `metabase.dashboard_list` | `collection_id?`, `limit?`, `instance?` | Uses `/api/search?models=dashboard`. |
| `metabase.dashboard_get` | `id`, `instance?` | Includes dashcards. |
| `metabase.dashboard_query` | `id`, `parameters?`, `limit?`, `max_cards?`, `instance?` | Executes each dashcard's card. Per-card row cap. |
| `metabase.dashboard_create` | `name`, `description?`, `collection_id?`, `collection_position?`, `dashcards?`, `parameters?`, `tabs?`, `instance?` | **Mutating.** `dashcards` lays out cards; `parameters` are dashboard filters; `tabs` for multi-tab layouts. |
| `metabase.dashboard_update` | `id`, `name?`, `description?`, `collection_id?`, `collection_position?`, `dashcards?`, `parameters?`, `tabs?`, `instance?` | **Mutating.** `dashcards` **replaces** the whole layout — prefer the add/remove tools for single-card edits. |
| `metabase.dashboard_archive` | `id`, `archived?`, `instance?` | **Mutating.** |
| `metabase.dashboard_add_cards` | `id`, `cards` (each needs `card_id`; optional `row`/`col`/`size_x`/`size_y`/`series`/`parameter_mappings`/`visualization_settings`), `instance?` | **Mutating.** Read-merge-write: appends to the existing layout. Cards without an explicit position auto-stack below current cards. |
| `metabase.dashboard_remove_card` | `id`, `dashcard_id`, `instance?` | **Mutating.** Read-merge-write: drops one card by its dashcard id, preserving the rest. |

### Collections

| Tool | Parameters | Notes |
|---|---|---|
| `metabase.collection_list` | `archived?`, `instance?` | |
| `metabase.collection_get` | `id` (int or `"root"`), `instance?` | |
| `metabase.collection_items` | `id` (int or `"root"`), `models?`, `instance?` | Item types: `card`, `dashboard`, `collection`, `dataset`, `snippet`. |
| `metabase.collection_create` | `name`, `description?`, `parent_id?`, `color?`, `instance?` | **Mutating.** |
| `metabase.collection_update` | `id`, `name?`, `description?`, `parent_id?`, `archived?`, `instance?` | **Mutating.** |

### Databases & schema

| Tool | Parameters | Notes |
|---|---|---|
| `metabase.database_list` | `instance?` | |
| `metabase.database_tables` | `database_id`, `instance?` | Slimmed (id/name/schema/description per table). |
| `metabase.table_fields` | `table_id`, `instance?` | Slimmed (id/name/type/semantic_type/description per field). |

### Native query

| Tool | Parameters | Notes |
|---|---|---|
| `metabase.dataset_query_native` | `database_id`, `query` (SQL), `template_tags?`, `parameters?`, `limit?`, `instance?` | **Mutating execute.** Row-capped. Respects the database permissions of the API key. |

### Search

| Tool | Parameters | Notes |
|---|---|---|
| `metabase.search` | `query`, `models?`, `limit?`, `instance?` | Global search. `models` filters by type (`card`, `dashboard`, `collection`, `dataset`, `table`, `database`, `segment`, `metric`). |

## Security

- **External content wrapping**: card/dashboard descriptions and query result rows are wrapped via `wrap_external_content` before being returned to the agent, to defend against prompt injection via Metabase data.
- **Policies**: all tools default to `allow` in `SkillPolicy`. Operators who want a confirmation gate on mutating tools (`*_create`, `*_update`, `*_archive`, `dataset_query_native`) should downgrade those to `require_approval` in their deployment config.
- **Row cap**: 2000 rows by default; explicit limits up to a 50000 hard ceiling (results stage as artifacts, never inline in context). Per-instance overrides may lower the cap.

## Reloading

After changing vault credentials, run `system.enable_skill('metabase')` to reload.
