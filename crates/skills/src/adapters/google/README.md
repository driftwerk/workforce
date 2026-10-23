# Google Workspace Skill

Pure-Rust Google OAuth 2.0 + REST client covering Gmail, Calendar, Drive,
Sheets, Docs, Tasks, Chat, Slides, Contacts, and People.

See [docs/GOOGLE.md](../../../../docs/GOOGLE.md) for the full setup guide,
multi-account configuration, tool reference, and troubleshooting.

## Quick Start

```bash
# 1. Store your Google Cloud OAuth client credentials
workforce vault set skills/google/oauth_client/client_id     'YOUR_CLIENT_ID'
workforce vault set skills/google/oauth_client/client_secret  'YOUR_SECRET'

# 2. Authorize
workforce google login                    # browser flow
workforce google login --no-browser       # headless/container flow

# 3. Verify
workforce google list
```
