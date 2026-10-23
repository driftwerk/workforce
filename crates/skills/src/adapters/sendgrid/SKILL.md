# SendGrid Skill

Agent-callable tools for the Twilio SendGrid v3 API — transactional email
sending, email activity, stats, templates, sender identities and
suppression management. Supports multiple SendGrid accounts (or subusers)
side-by-side as workspaces.

## Authentication

SendGrid is API-key only: every call carries `Authorization: Bearer SG.…`.
There is no OAuth flow for the API. Create keys in SendGrid under
**Settings → API Keys**. A restricted key with *Mail Send*, *Template Engine
(read)*, *Stats (read)*, *Suppressions* and *Sender Authentication (read)*
covers every typed tool; `sendgrid.api` needs whatever the endpoint you call
requires.

Two facts shape how you provision keys:

- A key holding `mail.send` can send as **any** verified sender identity in
  the account — keys cannot be pinned to a From address. The From must be a
  verified single sender or on an authenticated domain, else SendGrid
  returns 403.
- Keys created by a teammate stay live after the teammate is removed. Audit
  keys when people leave.

## Vault setup

Per-user (each person's own key, saved from **My Settings**) and shared
(agent-wide) credentials use the same paths; the per-user tier wins when
both exist.

### Single workspace

```text
vault.set('skills/sendgrid/api_key',    '<SG....>')
vault.set('skills/sendgrid/from_email', 'noreply@example.com')   # optional default From
vault.set('skills/sendgrid/from_name',  'Example')                # optional
vault.set('skills/sendgrid/base_url',   'https://api.eu.sendgrid.com')  # optional, EU residency
```

### Multi workspace

```text
vault.set('skills/sendgrid/workspaces/<name>/api_key',    '<SG....>')
vault.set('skills/sendgrid/workspaces/<name>/from_email', 'noreply@example.com')
vault.set('skills/sendgrid/workspaces/<name>/from_name',  'Example')
vault.set('skills/sendgrid/workspaces/<name>/base_url',   'https://api.eu.sendgrid.com')
vault.set('skills/sendgrid/default_workspace', '<name>')  # only with several workspaces
```

After adding or changing credentials, call `system.enable_skill('sendgrid')`.

## Tool catalogue

### Sending (`require_approval`)

| Tool | What it does |
|---|---|
| `sendgrid.email_send` | One message: `to`/`cc`/`bcc`, `subject` + `text`/`html`, or `template_id` + `dynamic_template_data`. `from` defaults to the workspace sender. Attach staged artifacts via `attach_artifact_ids`. |
| `sendgrid.email_send_batch` | Shared envelope + up to 1000 `messages`, one personalization each (own recipients, subject, template data, send_at). |

The approval prompt names the resolved From, the recipient count and the
subject before anything is sent.

### Delivery insight

| Tool | Endpoint |
|---|---|
| `sendgrid.messages_search` | `GET /v3/messages` — needs the Email Activity history add-on |
| `sendgrid.message_get` | `GET /v3/messages/{msg_id}` — full event history |
| `sendgrid.stats_global` | `GET /v3/stats` |
| `sendgrid.stats_categories` | `GET /v3/categories/stats` |

### Senders and templates

| Tool | Endpoint |
|---|---|
| `sendgrid.senders_list` | `GET /v3/verified_senders` |
| `sendgrid.domains_list` | `GET /v3/whitelabel/domains` |
| `sendgrid.templates_list` | `GET /v3/templates` |
| `sendgrid.template_get` | `GET /v3/templates/{id}` |

### Suppressions

`kind` is one of `bounces`, `blocks`, `spam_reports`, `invalid_emails`,
`unsubscribes` (global).

| Tool | Policy |
|---|---|
| `sendgrid.suppressions_list` | allow |
| `sendgrid.suppression_get` | allow |
| `sendgrid.unsubscribe_groups_list` | allow |
| `sendgrid.suppression_delete` | require_approval |
| `sendgrid.unsubscribes_add` | require_approval |

### Escape hatch (`require_approval`)

`sendgrid.api` — any method and `/v3/...` path with optional `query` and
`body`.

## Gotchas

- **403 on send** almost always means the From is not a verified sender
  identity, or the key lacks `mail.send`. `sendgrid.senders_list` and
  `sendgrid.domains_list` show what is usable.
- **`send_at`** may be at most 72 hours ahead; pair it with a `batch_id` if
  you need to cancel (`sendgrid.api` → `/v3/user/scheduled_sends`).
- **Rate limits** surface as the typed rate-limit signal with a retry hint
  from `Retry-After` or `X-RateLimit-Reset`.
- **Email Activity** (`messages_search`, `message_get`) returns 403 without
  the paid history add-on.
- **Regional accounts** must set `base_url` to `https://api.eu.sendgrid.com`.
  `base_url` and `default_workspace` are operator (shared-tier) settings: a
  per-user value is ignored at load, so nobody can point other users' keys
  at another host.
- **`sendgrid.api`** can reach `/v3/mail/send` too, which is why it also
  requires approval. Batch sends are capped at 1000 recipients across
  to/cc/bcc.
