# Jira Integration

The Jira skill gives agents access to the Jira Cloud REST API v3 (and the
`/rest/agile/1.0/` endpoints for boards and sprints). It supports multiple
Atlassian tenants in a single workforce deployment with project-key prefix
auto-routing — an issue key like `ENG-123` is automatically dispatched to the
workspace that owns the `ENG` prefix.

## Quick Start

1. Generate an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>.
2. Store credentials in the vault:
   ```bash
   workforce vault set skills/jira/email     you@acme.com
   workforce vault set skills/jira/api_token <token>
   workforce vault set skills/jira/site      https://acme.atlassian.net
   ```
3. Enable the skill via an agent or CLI:
   ```
   You:   "Enable Jira tools."
   Agent: system.enable_skill("jira")    # loads 18 tools
   ```
4. Use it:
   ```
   You:   "Show me open issues assigned to me in the ENG project."
   Agent: jira.search(jql="project = ENG AND assignee = currentUser() AND statusCategory != Done")
   ```

## Authentication

Jira Cloud uses HTTPS Basic auth where the username is your Atlassian account
email and the password is the API token. The skill stores both in the vault
and constructs the `Authorization: Basic base64(email:api_token)` header on
each request.

API tokens are scoped to the user that created them and inherit that user's
permissions. To restrict access to a subset of projects or operations, create
a service account user in Atlassian and generate the token from there.

## Sites and Tenants

Each Atlassian tenant has its own subdomain — `https://<your-site>.atlassian.net`.
The `site` field in the vault is the full URL, not just the subdomain.

If you have multiple tenants (multiple Atlassian Cloud organisations), use the
multi-tenant pattern below. Each tenant becomes a named workspace with its
own credentials.

## Vault Setup

### Single tenant

```bash
workforce vault set skills/jira/email           you@acme.com
workforce vault set skills/jira/api_token       <token>
workforce vault set skills/jira/site            https://acme.atlassian.net
workforce vault set skills/jira/default_project ENG               # optional
```

`default_project` is used by `jira.create_issue` when the caller omits
`project_key`.

### Multi tenant

```bash
workforce vault set skills/jira/workspaces/acme/email           you@acme.com
workforce vault set skills/jira/workspaces/acme/api_token       <token>
workforce vault set skills/jira/workspaces/acme/site            https://acme.atlassian.net
workforce vault set skills/jira/workspaces/acme/default_project ENG
workforce vault set skills/jira/workspaces/acme/project_keys    ENG,INFRA

workforce vault set skills/jira/workspaces/client/email         you@client.com
workforce vault set skills/jira/workspaces/client/api_token     <token>
workforce vault set skills/jira/workspaces/client/site          https://client.atlassian.net
workforce vault set skills/jira/workspaces/client/project_keys  OPS

workforce vault set skills/jira/default_workspace               acme
```

After adding or rotating credentials, run `system.enable_skill("jira")` (or
ask an agent to do it). The skill auto-discovers all configured workspaces.

## Multi-Tenant Routing

Every Jira tool accepts an optional `workspace` parameter. Resolution
precedence:

1. **Explicit `workspace` parameter** — wins, errors if the name is unknown.
2. **`project_key` prefix match** — `project_key="ENG"` looks up `ENG` in the
   prefix map and routes to whichever workspace owns it.
3. **`issue_key` prefix match** — `issue_key="OPS-42"` extracts `OPS` and
   routes the same way.
4. **Default workspace** — `default_workspace` when it names a configured
   workspace, else `default`, else the only workspace. With several
   workspaces and no usable default the call errors and lists what exists.

The `project_keys` vault entry is a comma-separated list of uppercase
prefixes. Routing is case-insensitive (`eng-1` and `ENG-1` resolve the same).

If a key prefix isn't in any workspace's `project_keys`, the request falls
back to the default workspace — so a typo on the agent's part still hits a
real tenant rather than erroring.

## Atlassian Document Format (ADF)

Jira REST v3 requires `description` and comment `body` fields to be
[Atlassian Document Format](https://developer.atlassian.com/cloud/jira/platform/apis/document/structure/),
not plain strings. The skill auto-wraps a plain string into a minimal ADF doc
so agents can pass natural text:

```json
{
  "type": "doc",
  "version": 1,
  "content": [
    { "type": "paragraph", "content": [{ "type": "text", "text": "<your text>" }] }
  ]
}
```

If you need rich content (links, headings, code blocks), pass a complete ADF
object directly — the skill detects objects and passes them through unchanged.

When reading, `jira.get_issue` and `jira.list_comments` request
`expand=renderedFields` / `expand=renderedBody` so the rendered HTML is
returned in `description_html` / `body_html` for display.

## Custom Fields

Jira custom fields have opaque IDs like `customfield_10035`. Both
`jira.create_issue` and `jira.update_issue` accept a `fields` object that is
merged into the request body, so agents can set any custom field without the
skill needing to enumerate them:

```json
{
  "issue_key": "ENG-42",
  "fields": {
    "customfield_10035": "v2.3.0",
    "customfield_10010": [{ "value": "platform" }]
  }
}
```

Use `jira.get_issue` first to discover the field IDs on your tenant, or query
`/rest/api/3/field` via `jira.api` for the full field list.

## Tools

| Tool | Endpoint | Notes |
|------|----------|-------|
| `jira.search` | `POST /rest/api/3/search/jql` | New endpoint replacing the deprecated `/search`. Use `next_page_token` from the response for pagination. |
| `jira.get_issue` | `GET /rest/api/3/issue/{key}?expand=renderedFields,changelog` | Returns summary, description (HTML), status, assignee, reporter, labels, comment count. |
| `jira.create_issue` | `POST /rest/api/3/issue` | Required: `summary`, `issue_type`. Auto-wraps `description` to ADF. |
| `jira.update_issue` | `PUT /rest/api/3/issue/{key}` | Only sends supplied fields. Use `fields` for custom fields. |
| `jira.delete_issue` | `DELETE /rest/api/3/issue/{key}` | Requires delete permission. |
| `jira.assign_issue` | `PUT /rest/api/3/issue/{key}/assignee` | Pass `account_id` (use `jira.search_users` to find IDs). `null` unassigns. |
| `jira.add_comment` | `POST /rest/api/3/issue/{key}/comment` | Auto-wraps body to ADF. |
| `jira.list_comments` | `GET /rest/api/3/issue/{key}/comment?expand=renderedBody` | Returns rendered HTML. |
| `jira.list_transitions` | `GET /rest/api/3/issue/{key}/transitions` | Returns ids and display names. |
| `jira.transition_issue` | `POST /rest/api/3/issue/{key}/transitions` | Accepts a transition id (preferred) or name. Looks up the id by name automatically. |
| `jira.add_attachment` | `POST /rest/api/3/issue/{key}/attachments` | Multipart upload. Sends `X-Atlassian-Token: no-check` automatically. |
| `jira.list_projects` | `GET /rest/api/3/project/search` | Optional substring `query` filter. |
| `jira.get_project` | `GET /rest/api/3/project/{keyOrId}` | Includes available issue types. |
| `jira.search_users` | `GET /rest/api/3/user/search` | Returns `accountId`, `displayName`, `emailAddress`. |
| `jira.list_boards` | `GET /rest/agile/1.0/board` | Agile API. |
| `jira.list_sprints` | `GET /rest/agile/1.0/board/{boardId}/sprint` | Filter by `state` (active/closed/future). |
| `jira.add_worklog` | `POST /rest/api/3/issue/{key}/worklog` | `time_spent` uses Jira's duration format (`3h 30m`, `1d 2h`). |
| `jira.api` | any | Escape hatch — pass `method`, `path`, optional `body` and `query`. |

## Example Agent Prompts

```
"List the bugs in INFRA opened in the last 7 days."
  → jira.search(jql="project = INFRA AND issuetype = Bug AND created >= -7d")

"Create a story in ENG titled 'add cache layer' assigned to me."
  → jira.search_users(query="me@acme.com")
  → jira.create_issue(project_key="ENG", summary="add cache layer",
                      issue_type="Story", assignee_account_id="<id>")

"Move ENG-42 to In Review."
  → jira.list_transitions(issue_key="ENG-42")
  → jira.transition_issue(issue_key="ENG-42", transition="In Review")

"What sprints are active on the Platform board?"
  → jira.list_boards()
  → jira.list_sprints(board_id=12, state="active")
```

In a multi-tenant setup, agents don't need to specify the workspace — the
prefix in the issue key (`OPS-42` vs `ENG-42`) routes to the right tenant
automatically.

## Security Considerations

- **External content wrapping.** Every string field returned from Jira that
  contains user-authored content (summaries, descriptions, comment bodies,
  rendered HTML) is passed through `wrap_external_content()` before being
  exposed to the agent. This protects against prompt-injection attempts in
  Jira data.
- **Default policies.** All Jira tools default to `allow`. If you want write
  operations gated, override the policy in your agent config (mirror the
  Datadog skill's approach).
- **Token scope.** API tokens inherit the user's permissions. Use a service
  account with least-privilege access for production deployments.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Authentication failed` | Wrong email or expired token | Re-generate the token at id.atlassian.com and `vault set skills/jira/api_token`. |
| `Forbidden (permissions)` | The user lacks permission on this project | Check the project's permission scheme in Jira admin. |
| `Not found` on a known issue | Wrong tenant — issue exists in another workspace | Pass `workspace=<name>` explicitly, or add the project key to the right workspace's `project_keys`. |
| `Validation error` on create | Required custom field missing | Use `jira.get_issue` on a similar issue to see what fields the project requires, then pass them via `fields`. |
| Skipped workspace at startup | Missing `email` or `site` for that workspace | Hub logs `Skipping Jira workspace: missing <field>`. Set the missing key and re-run `system.enable_skill("jira")`. |

## Related Documentation

- [SKILLS.md](SKILLS.md) — Built-in skill catalogue and lifecycle
- [VAULT.md](VAULT.md) — Vault encryption and CLI
- [DATADOG.md](DATADOG.md) — Sister observability skill with the same multi-account pattern
- [POLICY_ENGINE.md](POLICY_ENGINE.md) — Tool access control
