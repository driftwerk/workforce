# Jira Cloud Skill

Agent-callable tools for the Jira Cloud REST API v3 (and the `/rest/agile/1.0`
endpoints for boards and sprints). Supports multiple Atlassian tenants with
project-key auto-routing.

## Authentication

Jira Cloud uses HTTPS Basic auth with `email:api_token`. Generate an API token
at https://id.atlassian.com/manage-profile/security/api-tokens.

## Vault setup

### Single tenant

```text
vault.set('skills/jira/email',     '<you@example.com>')
vault.set('skills/jira/api_token', '<token>')
vault.set('skills/jira/site',      'https://<tenant>.atlassian.net')
vault.set('skills/jira/default_project', 'ENG')   # optional
```

### Multi tenant

```text
vault.set('skills/jira/workspaces/acme/email',         '<you@acme.com>')
vault.set('skills/jira/workspaces/acme/api_token',     '<token>')
vault.set('skills/jira/workspaces/acme/site',          'https://acme.atlassian.net')
vault.set('skills/jira/workspaces/acme/default_project', 'ENG')          # optional
vault.set('skills/jira/workspaces/acme/project_keys',  'ENG,INFRA')      # optional, enables auto-routing

vault.set('skills/jira/workspaces/client/email',       '<you@client.com>')
vault.set('skills/jira/workspaces/client/api_token',   '<token>')
vault.set('skills/jira/workspaces/client/site',        'https://client.atlassian.net')
vault.set('skills/jira/workspaces/client/project_keys','OPS')

vault.set('skills/jira/default_workspace', 'acme')
```

A single configured workspace is the default automatically; set
`default_workspace` only when there are several.

When `project_keys` are configured, calling `jira.get_issue` with `OPS-42`
auto-routes to the `client` workspace; `ENG-7` routes to `acme`. The explicit
`workspace` parameter on every tool always wins over auto-routing.

After adding or changing credentials run `system.enable_skill('jira')` to
reload.

## Tool catalogue

| Tool | Description |
|---|---|
| `jira.search` | JQL search via the new `/rest/api/3/search/jql` endpoint, with cursor pagination (`next_page_token`). |
| `jira.get_issue` | Full issue + comments + rendered description. |
| `jira.create_issue` | Create issue. Descriptions accept markdown (rendered to ADF) or a raw ADF object. |
| `jira.update_issue` | Update fields on an existing issue. Custom fields via the `fields` passthrough. |
| `jira.delete_issue` | Permanent delete (requires the right permission). |
| `jira.assign_issue` | Assign by `accountId`; pass `null` to unassign. |
| `jira.add_comment` | Add a comment. Bodies accept markdown (rendered to ADF) or a raw ADF object. |
| `jira.list_comments` | List comments with rendered HTML. |
| `jira.list_transitions` | List available workflow transitions. |
| `jira.transition_issue` | Move an issue through a transition by id or name. |
| `jira.add_attachment` | Upload a local file (multipart). |
| `jira.list_projects` | List accessible projects. |
| `jira.get_project` | Project detail with issue types. |
| `jira.search_users` | Find users by name/email; returns `accountId`. |
| `jira.list_boards` | List Jira Software boards (Agile API). |
| `jira.list_sprints` | List sprints on a board, optionally filtered by state. |
| `jira.add_worklog` | Log time on an issue. |
| `jira.api` | Escape hatch for arbitrary REST calls. Path must include `/rest/api/3` or `/rest/agile/1.0`. |

## Markdown and ADF

Jira REST v3 requires `description` and comment `body` fields to be
[Atlassian Document Format](https://developer.atlassian.com/cloud/jira/platform/apis/document/structure/)
(JSON), not markdown or plain strings. The skill accepts three input shapes
on every text field and converts as needed:

- **Markdown string** (recommended): rendered to ADF before it hits the wire.
  Supported subset: paragraphs, headings (`#` through `######`), `**bold**`,
  `_italic_`, `~~strike~~`, `` `inline code` ``, fenced code blocks with
  optional language hints, `[links](url)`, bullet and ordered lists (including
  nested), blockquotes, hard and soft breaks. Tables degrade to a placeholder;
  images render as hyperlinked alt-text; raw HTML and math are dropped. Use a
  raw ADF object if you need anything outside this subset.
- **Raw ADF object**: passed through unchanged.
- **`null` or empty / whitespace-only string**: produces an empty ADF doc.

When reading, `jira.get_issue` and `jira.list_comments` request
`expand=renderedFields` / `expand=renderedBody` so the rendered HTML is
returned in `description_html` / `body_html`.

## Custom fields

`jira.create_issue` and `jira.update_issue` accept a `fields` object that is
merged into the request. Use this for custom fields like
`customfield_10035`. Use `jira.get_issue` first to discover the field IDs on
your tenant.

## Example agent prompts

- "Show me open issues assigned to me in the ENG project."
- "Create a bug in ENG titled 'login button broken' with priority High."
- "Transition ENG-42 to Done."
- "Comment on ENG-42: 'rolled out the fix to staging'."
- "What sprints are active on the Platform board?"
- "Log 1h 30m on ENG-42 for code review."
