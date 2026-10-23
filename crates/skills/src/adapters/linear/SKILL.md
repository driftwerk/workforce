# Linear Skill

Agent-callable tools for interacting with the Linear GraphQL API. Supports issues, comments, workflow states, teams, projects, labels, members, and **documents**.

## Authentication

Authentication uses a Linear API key (no Bearer prefix). Configure via:

- Environment variable: `LINEAR_API_KEY`
- Secrets vault: `skills/linear/api_key`

## Multi-workspace support

Configure multiple workspaces via the `workspaces` config. Use the `workspace` parameter on any tool to target a specific workspace. If omitted, the default workspace is used.

## Document tools

### `linear.document_get`

Get a specific document by UUID or URL.

| Parameter   | Type   | Required | Description                          |
|-------------|--------|----------|--------------------------------------|
| `id`        | string | yes      | Document UUID or Linear document URL |
| `workspace` | string | no       | Workspace name (multi-workspace)     |

The `content` field in the response contains the document body as Markdown.

---

### `linear.document_list`

List documents, optionally filtered by project.

| Parameter    | Type    | Required | Description                              |
|--------------|---------|----------|------------------------------------------|
| `project_id` | string  | no       | Filter by project UUID                   |
| `limit`      | integer | no       | Max results (default: 50, max: 100)      |
| `workspace`  | string  | no       | Workspace name (multi-workspace)         |

---

### `linear.document_create`

Create a new document.

| Parameter    | Type   | Required | Description                              |
|--------------|--------|----------|------------------------------------------|
| `title`      | string | yes      | Document title                           |
| `content`    | string | no       | Document body in Markdown format         |
| `project_id` | string | no       | Associate with a project UUID            |
| `icon`       | string | no       | Icon emoji or identifier                 |
| `color`      | string | no       | Color hex code                           |
| `workspace`  | string | no       | Workspace name (multi-workspace)         |

---

### `linear.document_update`

Update an existing document.

| Parameter   | Type   | Required | Description                              |
|-------------|--------|----------|------------------------------------------|
| `id`        | string | yes      | Document UUID to update                  |
| `title`     | string | no       | New title                                |
| `content`   | string | no       | New body in Markdown format              |
| `icon`      | string | no       | New icon emoji or identifier             |
| `color`     | string | no       | New color hex code                       |
| `workspace` | string | no       | Workspace name (multi-workspace)         |

---

### `linear.document_delete`

Delete a document by UUID.

| Parameter   | Type   | Required | Description                              |
|-------------|--------|----------|------------------------------------------|
| `id`        | string | yes      | Document UUID to delete                  |
| `workspace` | string | no       | Workspace name (multi-workspace)         |

---

### `linear.document_search`

Search documents by title.

| Parameter    | Type    | Required | Description                              |
|--------------|---------|----------|------------------------------------------|
| `query`      | string  | yes      | Search term (matched against title)      |
| `project_id` | string  | no       | Narrow search to a specific project      |
| `limit`      | integer | no       | Max results (default: 25, max: 100)      |
| `workspace`  | string  | no       | Workspace name (multi-workspace)         |

## Content format

Linear stores document content as Slate.js JSON internally but exposes it as Markdown via the `content` field. When creating or updating documents, provide content as Markdown — Linear handles the conversion automatically.
