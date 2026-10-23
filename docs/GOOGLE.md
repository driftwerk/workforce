# Google Workspace Integration

The Google skill gives agents native access to Gmail, Calendar, Drive, Sheets,
Docs, Tasks, Chat, Slides, Contacts, and People — all through pure-Rust HTTP
clients with OAuth 2.0 tokens stored in the workforce vault. No external
binaries required.

## Quick Start

Setup has two phases: a one-time admin step that creates an OAuth client
for the whole organization, and a per-user step that connects each
employee's Google account. The per-user step is fully agent-driven — a
chat agent walks the user through device-flow OAuth without any CLI
commands.

### Step 1 — Create an OAuth Client (~5 minutes, one-time per org)

This has to be done once by an admin with access to Google Cloud Console.
All users share the same OAuth client — you do NOT register per user.

1. Go to <https://console.cloud.google.com/apis/credentials> and select or
   create a project.
2. Configure the OAuth consent screen:
   - **User type: Internal** if every user is on the same Google Workspace
     domain (e.g. all `@company.com`). Internal apps skip Google's
     verification review entirely — any scope, any number of users, no
     publishing step.
   - **User type: External** otherwise. While the app's Publishing status
     is **Testing** (the default after creation), only accounts on the
     **Test users** allowlist can authorize. *Every* hub user must be
     added there — non-test-users hit `unauthorized_client` at token
     exchange after appearing to consent successfully. Add each user
     under "OAuth consent screen → Test users" before they try to log in.
   - **External + In production** is only viable for sensitive scopes
     (Gmail/Drive/Calendar/etc.) after Google's verification process
     (weeks, not minutes). The Testing-mode + Test users path is what
     most deployments use.
3. Create credentials → **OAuth client ID**. Pick the application type
   that matches your deployment:
   - **Desktop app** (recommended for most setups): uses the device flow
     (`google.setup(action='login', no_browser=true)`). Works from any
     network including private/headless. Narrower Gmail scope (no
     permanent delete, no forwarding-address creation).
   - **Web application**: uses the hub-hosted web-redirect flow.
     Adds the broad `mail.google.com/` scope (full
     Gmail incl. permanent delete, forwarding). Requires the hub to be
     reachable on a public HTTPS URL and the **Authorized redirect URI**
     to be set to `https://<your-hub-host>/api/auth/google/callback`.

   Name the client "Workforce" or similar. Copy the Client ID and Client
   Secret.
4. Enable the APIs the skill talks to: **APIs & Services → Library** → enable
   Gmail API, Google Calendar API, Google Drive API, Google Sheets API,
   Google Docs API, Google Tasks API, Google Chat API, Google Slides API,
   and People API.
5. Store the client credentials in the **hub's global vault** — these are
   organization-wide, shared by every user, and must be set from the hub
   host (not via the agent-callable `vault.set` tool, which writes to a
   per-agent vault that the OAuth callback handler cannot reach):

   ```bash
   workforce vault set skills/google/oauth_client/client_id     'YOUR_CLIENT_ID.apps.googleusercontent.com'
   workforce vault set skills/google/oauth_client/client_secret  'YOUR_CLIENT_SECRET'
   ```

   Web application clients also need the redirect URI in the vault:

   ```bash
   workforce vault set skills/google/oauth_client/redirect_uri \
     'https://<your-hub-host>/api/auth/google/callback'
   ```

   These three paths (`skills/google/oauth_client/*`) are reserved
   org-wide. Trying to set them through `vault.set` from an agent is
   rejected with a pointer back to this CLI.

### Step 2 — Connect a User's Google Account (per user, in chat)

The agent drives this entirely via the `google.setup` tool. The user never
has to run a CLI command.

**a. Start the device flow** (non-blocking — returns immediately):

```json
google.setup(action="login", no_browser=true, workspace="default")
```

Response:

```json
{
  "status": "pending_authorization",
  "workspace": "default",
  "user_code": "ABCD-EFGH",
  "verification_url": "https://www.google.com/device",
  "expires_in_seconds": 1800,
  "poll_interval_seconds": 5,
  "next_step": "Present the user_code and verification_url to the user verbatim..."
}
```

**b. Show the user the code and URL.** The agent says something like:

> Visit <https://www.google.com/device> on any device, sign into your
> Google account, and enter the code `ABCD-EFGH`.

**c. Poll until authorized** (typical total time: 30–60 seconds):

```json
google.setup(action="login_poll", workspace="default")
```

Each call blocks for up to ~25 seconds, polling Google every 5 seconds.
Possible statuses:

| status | meaning |
|--------|---------|
| `authorized` | User finished; tokens stored; all `google.*` tools now work |
| `pending` | User hasn't finished yet — call login_poll again |
| `expired` | Device code timed out — restart with `action='login'` |
| `denied` | User clicked Cancel — restart if they changed their mind |
| `no_pending` | No login in progress — call `action='login'` first |

**d. Verify:**

```json
google.setup(action="status", workspace="default")
```

`authenticated: true` means ready.

### Multi-Account / Multi-User

Use distinct workspace names to connect more than one account (e.g. the
user's email local-part or a role name):

```json
google.setup(action="login", no_browser=true, workspace="alice")
google.setup(action="login", no_browser=true, workspace="bob")
```

Set a default for tools that don't pass an explicit `account`:

```bash
workforce vault set skills/google/default_workspace alice
```

### CLI Alternative (Developer Use Only)

`workforce google login [--workspace NAME] [--no-browser]` runs the same
OAuth flow from a terminal. Useful for local development or initial
bootstrap, but not something an end user should ever need — the agent
tool is the supported path for production.

## Authentication

The skill uses **OAuth 2.0 Authorization Code with PKCE** (loopback redirect)
as the primary flow, with **Device Authorization Grant** as the headless
fallback. Both flows:

1. Direct the user to Google's consent screen
2. Exchange the authorization code for an access token + refresh token
3. Store both tokens in the workforce vault (AES-256-GCM encrypted)
4. Automatically refresh the access token when it expires (~1 hour lifetime)

Access tokens are refreshed lazily — before each API call, the skill checks
the expiry timestamp and refreshes if within 60 seconds of expiry. Concurrent
refresh requests are serialized per-workspace to avoid token endpoint storms.

### OAuth Client Credentials

The skill looks for credentials in this order:

1. **Vault** — `skills/google/oauth_client/client_id` and `client_secret`
2. **Bundled** — compile-time env vars `WORKFORCE_GOOGLE_CLIENT_ID` and
   `WORKFORCE_GOOGLE_CLIENT_SECRET` (for pre-configured builds)

If neither is available, the skill loads but tools return an error directing
the user to run `workforce google login`.

### Scopes

All scopes are requested upfront so users only see the consent screen once:

| Scope | Services |
|-------|----------|
| `https://www.googleapis.com/auth/gmail.modify` | Gmail (read/send/label/trash/drafts) |
| `https://www.googleapis.com/auth/gmail.settings.basic` | Gmail settings (filters, vacation, send-as) |
| `https://www.googleapis.com/auth/calendar` | Calendar |
| `https://www.googleapis.com/auth/drive` | Drive |
| `https://www.googleapis.com/auth/spreadsheets` | Sheets |
| `https://www.googleapis.com/auth/documents` | Docs |
| `https://www.googleapis.com/auth/tasks` | Tasks |
| `https://www.googleapis.com/auth/contacts` | Contacts |
| `https://www.googleapis.com/auth/directory.readonly` | Directory (org contacts) |
| `https://www.googleapis.com/auth/chat.messages` | Chat messages |
| `https://www.googleapis.com/auth/chat.spaces` | Chat spaces |
| `https://www.googleapis.com/auth/presentations` | Slides |
| `https://www.googleapis.com/auth/userinfo.email` | Email address |
| `https://www.googleapis.com/auth/userinfo.profile` | Profile info |

> **Unverified app warning.** If your Google Cloud project is not verified,
> users will see a "This app isn't verified" warning during consent. Click
> **Advanced → Go to [app name] (unsafe)** to proceed. For production use,
> submit your app for verification in the Google Cloud Console under **OAuth
> consent screen → Publish App**.

## Multi-Account Setup

The skill supports multiple Google accounts (workspaces), matching the
GitHub/Linear/Jira multi-workspace pattern.

### Adding Multiple Accounts

```bash
workforce google login --workspace work
workforce google login --workspace personal
```

### Setting the Default

```bash
workforce vault set skills/google/default_workspace work
```

### Using a Specific Account

Every Google tool accepts an optional `account` parameter:

```
google.calendar.events(account="personal")
google.gmail.send(account="work", to="team@company.com", subject="Update", body="...")
```

When `account` is omitted, the default workspace is used.

### Listing Accounts

```bash
workforce google list
```

```
🔐 2 Google account(s):

  • work — alice@company.com
  • personal — alice@gmail.com
```

### Removing an Account

```bash
workforce google logout --workspace personal
```

## Vault Schema

All Google skill state lives under the `skills/google/` vault prefix:

```
skills/google/oauth_client/client_id                   # OAuth client ID (one per org)
skills/google/oauth_client/client_secret               # OAuth client secret (one per org)
skills/google/default_workspace                        # e.g. "alice"
skills/google/workspaces/<name>/email                  # e.g. "alice@company.com"
skills/google/workspaces/<name>/refresh_token          # Long-lived OAuth refresh token
skills/google/workspaces/<name>/access_token           # Short-lived access token (cache)
skills/google/workspaces/<name>/expires_at             # Access token expiry (unix seconds)
skills/google/workspaces/<name>/scopes                 # Space-joined scope list
skills/google/workspaces/<name>/pending/device_code    # Short-lived, only set during a login flow
skills/google/workspaces/<name>/pending/user_code      # Shown to the user at google.com/device
skills/google/workspaces/<name>/pending/verification_url
skills/google/workspaces/<name>/pending/interval_secs  # Min seconds between token polls
skills/google/workspaces/<name>/pending/expires_at     # Unix seconds when device_code expires
```

Single-account setups use `default` as the workspace name.

## CLI Commands

```
workforce google login [--workspace NAME] [--no-browser] [--client-id ID] [--client-secret SECRET]
workforce google logout [--workspace NAME]
workforce google list
```

| Flag | Description |
|------|-------------|
| `--workspace NAME` | Workspace name (default: `default`) |
| `--no-browser` | Use device flow instead of browser loopback |
| `--client-id` | Override OAuth client ID for this login |
| `--client-secret` | Override OAuth client secret for this login |

## Agent Setup Tool

The `google.setup` tool is the supported path for connecting a user's
Google account from chat. See **Quick Start → Step 2** for the end-to-end
flow; this section is a reference for all actions.

| Action | Blocking? | Description |
|--------|-----------|-------------|
| `login` (default `no_browser=true`) | No | Start device-flow OAuth. Returns `user_code` + `verification_url`. |
| `login` (`no_browser=false`) | Yes (≤5 min) | Start loopback OAuth. Only works if the tool's process has a browser (local dev). |
| `login_poll` | Yes (≤25 sec) | Poll Google for a pending device-flow login. Idempotent; safe to call repeatedly. |
| `status` | No | Report whether a workspace is authorized, and whether a device-flow login is in progress. |
| `list` | No | List authorized accounts. |
| `logout` | No | Revoke and remove all tokens (and any pending state) for a workspace. |
| `refresh` | No | Force-refresh the access token (rarely needed; refresh is automatic). |

Pending device-flow state lives in the vault under
`skills/google/workspaces/<name>/pending/*` and is cleared automatically
on authorize / expire / deny / logout. A second `login` call for the same
workspace discards the previous pending state and requests a fresh code.

## Tools

### Calendar

| Tool | Description |
|------|-------------|
| `google.calendar.events` | List events (with time range, search, pagination) |
| `google.calendar.event_get` | Get a single event by ID |
| `google.calendar.create` | Create an event (with attendees, location, recurrence) |
| `google.calendar.update` | Partial update of an event |
| `google.calendar.delete` | Delete an event |
| `google.calendar.colors` | List available event color IDs |
| `google.calendar.calendars` | List the user's calendars |
| `google.calendar.freebusy` | Query free/busy for time ranges |
| `google.calendar.search` | Search events by text query |

### Gmail

| Tool | Description |
|------|-------------|
| `google.gmail.search` | Search messages (Gmail query syntax) |
| `google.gmail.get` | Get a message by ID (full/metadata/minimal/raw); adds an `attachments` list (`part_id`, filename, mime type, size) when the response has a payload |
| `google.gmail.send` | Send an email (plain text or HTML, with CC/BCC) |
| `google.gmail.modify` | Add/remove labels on a message |
| `google.gmail.trash` | Move to trash |
| `google.gmail.untrash` | Remove from trash |
| `google.gmail.archive` | Archive (remove INBOX label) |
| `google.gmail.mark_read` | Mark as read |
| `google.gmail.mark_unread` | Mark as unread |
| `google.gmail.attachment` | Render an attachment into the agent's view by `message_id` + `part_id` (documents, PDFs, spreadsheets, images, audio/video) |
| `google.gmail.thread_get` | Get a thread by ID |
| `google.gmail.thread_modify` | Modify labels on an entire thread |
| `google.gmail.drafts_list` | List drafts |
| `google.gmail.draft_get` | Get a draft |
| `google.gmail.draft_create` | Create a draft |
| `google.gmail.draft_send` | Send a draft |
| `google.gmail.draft_delete` | Delete a draft |
| `google.gmail.labels_list` | List all labels |
| `google.gmail.label_get` | Get a label |
| `google.gmail.label_create` | Create a label |
| `google.gmail.label_delete` | Delete a label |
| `google.gmail.label_update` | Update a label |
| `google.gmail.history` | List mailbox changes since a history ID |
| `google.gmail.filters_list` | List filters |
| `google.gmail.filter_create` | Create a filter |
| `google.gmail.filter_delete` | Delete a filter |
| `google.gmail.sendas_list` | List send-as aliases |
| `google.gmail.vacation_get` | Get vacation/auto-reply settings |
| `google.gmail.vacation_set` | Update vacation settings |
| `google.gmail.forwarding_list` | List forwarding addresses |
| `google.gmail.batch_modify` | Modify labels on multiple messages |

### Drive

| Tool | Description |
|------|-------------|
| `google.drive.search` | Search files (Drive query syntax) |
| `google.drive.ls` | List files (alias for search) |
| `google.drive.get` | Get file metadata |
| `google.drive.download` | Download file content (text or base64) |
| `google.drive.upload` | Upload a file (text or base64 content) |
| `google.drive.mkdir` | Create a folder |
| `google.drive.rename` | Rename a file |
| `google.drive.move` | Move a file to a different folder |
| `google.drive.copy` | Duplicate a file |
| `google.drive.delete` | Permanently delete a file |
| `google.drive.share` | Grant a permission |
| `google.drive.permissions` | List sharing permissions |
| `google.drive.drives` | List shared drives |
| `google.drive.comments_list` | List comments on a file |

### Sheets

| Tool | Description |
|------|-------------|
| `google.sheets.get` | Read a range (A1 notation) |
| `google.sheets.update` | Write values to a range |
| `google.sheets.append` | Append rows |
| `google.sheets.clear` | Clear a range |
| `google.sheets.metadata` | Get spreadsheet metadata (tabs, properties) |
| `google.sheets.create` | Create a new spreadsheet |
| `google.sheets.add_tab` | Add a tab |
| `google.sheets.delete_tab` | Delete a tab |
| `google.sheets.rename_tab` | Rename a tab |
| `google.sheets.batch_update` | Raw batchUpdate (advanced) |

### Docs

| Tool | Description |
|------|-------------|
| `google.docs.cat` | Read a doc as plain text |
| `google.docs.info` | Get doc metadata (title, revision, tabs) |
| `google.docs.create` | Create a new doc |
| `google.docs.append` | Append text at the end |
| `google.docs.insert` | Insert text at a character index |
| `google.docs.find_replace` | Find and replace text |
| `google.docs.copy` | Duplicate a doc |
| `google.docs.batch_update` | Raw batchUpdate (advanced) |

### Tasks

| Tool | Description |
|------|-------------|
| `google.tasks.lists` | List task lists |
| `google.tasks.lists_create` | Create a task list |
| `google.tasks.list` | List tasks in a list |
| `google.tasks.get` | Get a task |
| `google.tasks.add` | Create a task |
| `google.tasks.update` | Update a task |
| `google.tasks.delete` | Delete a task |
| `google.tasks.done` | Mark as completed |
| `google.tasks.undo` | Mark as needs-action |
| `google.tasks.clear` | Clear completed tasks |

### Chat

| Tool | Description |
|------|-------------|
| `google.chat.spaces_list` | List spaces |
| `google.chat.spaces_create` | Create a space |
| `google.chat.spaces_find` | Find a space by name |
| `google.chat.messages_list` | List messages in a space |
| `google.chat.messages_send` | Send a message |
| `google.chat.dm_send` | Send a direct message |
| `google.chat.reactions_create` | Add a reaction |
| `google.chat.reactions_list` | List reactions |
| `google.chat.reactions_delete` | Remove a reaction |
| `google.chat.threads_list` | List threads in a space |

### Slides

| Tool | Description |
|------|-------------|
| `google.slides.info` | Get presentation metadata |
| `google.slides.create` | Create a presentation |
| `google.slides.list_slides` | List slides |
| `google.slides.read_slide` | Read text content of a slide |
| `google.slides.add_slide` | Add a slide |
| `google.slides.delete_slide` | Delete a slide |
| `google.slides.copy` | Duplicate a presentation |
| `google.slides.export` | Export as PDF or other format |
| `google.slides.update_notes` | Set speaker notes |
| `google.slides.replace_text` | Find and replace across all slides |
| `google.slides.batch_update` | Raw batchUpdate (advanced) |

### Contacts & People

| Tool | Description |
|------|-------------|
| `google.contacts.list` | List contacts |
| `google.contacts.search` | Search by name/email |
| `google.contacts.get` | Get a contact |
| `google.contacts.create` | Create a contact |
| `google.contacts.update` | Update a contact (requires etag) |
| `google.contacts.delete` | Delete a contact |
| `google.contacts.directory_search` | Search organization directory |
| `google.contacts.other_list` | List auto-saved "other" contacts |
| `google.people.me` | Get the authenticated user's profile |
| `google.people.get` | Get a person by resource name |
| `google.people.search` | Search people |

## Example Agent Prompts

```
"What's on my calendar today?"
  → google.calendar.events(time_min="2026-04-12T00:00:00Z", time_max="2026-04-12T23:59:59Z")

"Send Alice a meeting invite for tomorrow at 2pm."
  → google.calendar.create(summary="Meeting with Alice",
      start="2026-04-13T14:00:00+01:00", end="2026-04-13T15:00:00+01:00",
      attendees=["alice@company.com"])

"Find the Q1 report in Drive."
  → google.drive.search(q="name contains 'Q1 report'")

"Read the first sheet of spreadsheet ID abc123."
  → google.sheets.get(sheet_id="abc123", range="Sheet1!A1:Z100")

"Send an email to the team about the deployment."
  → google.gmail.send(to="team@company.com", subject="Deployment complete",
      body="The v2.3 release has been deployed to production.")

"Show my unread emails from today."
  → google.gmail.search(q="is:unread newer_than:1d")

"What tasks do I have?"
  → google.tasks.list()

"Create a task to review the PR."
  → google.tasks.add(title="Review PR #42", due="2026-04-13T17:00:00Z")

"Who is alice@company.com?"
  → google.contacts.search(query="alice@company.com")
```

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `no refresh token for workspace` | Account not authorized yet | `google.setup(action='login', no_browser=true, workspace='<name>')` and walk the user through the device flow |
| `No Google OAuth client configured` | Missing client ID/secret | Admin: run Quick Start Step 1 and store client_id/client_secret in the vault |
| `status='no_pending'` from `login_poll` | No device-flow login in progress | Call `action='login'` first; then `login_poll` |
| `status='expired'` from `login_poll` | User took longer than 30 min | Restart with `action='login'` |
| `token refresh failed: invalid_grant` | Refresh token revoked | Re-authorize: `google.setup(action='login', no_browser=true, workspace='<name>')` |
| `Google API error 403: Forbidden` | API not enabled or insufficient scopes | Enable the API in Google Cloud Console, then re-authorize to pick up new scopes |
| `Google API error 401` after fresh login | Clock skew between server and Google | Check `date` on the server; NTP should keep it synced |
| "This app isn't verified" warning | Google Cloud project not published | Internal apps don't need publishing. For External, click Advanced → Go to app, or submit for verification |
| `loopback login failed, falling back to device flow` | Port binding failed | Only affects `no_browser=false`; device flow (`no_browser=true`, the default) works everywhere |

## Migration from gog

The previous Google skill used the `gog` CLI binary with system keyring
storage. Tokens stored in the keyring are not accessible to the new skill.
On first load after upgrading, existing users will see:

```
WARN: Legacy Google account found (skills/google/account). Run `workforce google login` to re-authorize.
```

Re-authorization takes ~30 seconds. After `workforce google login`, the old
`skills/google/account` vault key can be removed.

## Security

- **Tokens at rest** are encrypted with AES-256-GCM in the workforce vault.
- **Access tokens** expire after ~1 hour and are refreshed automatically.
- **Refresh tokens** persist until revoked by the user or Google.
- **OAuth client secrets** for "Desktop app" type clients are not truly
  secret per Google's guidelines — they are embedded in the binary or stored
  in the vault. This is the standard model for installed applications.
- **Scope** is broad (full Gmail, Calendar, Drive, etc.). To restrict access,
  use a Google account with limited permissions or create a more restrictive
  OAuth client.

## Related Documentation

- [SKILLS.md](SKILLS.md) — Built-in skill catalogue and lifecycle
- [Google OAuth 2.0 Documentation](https://developers.google.com/identity/protocols/oauth2)
- [Google API Library](https://console.cloud.google.com/apis/library)
