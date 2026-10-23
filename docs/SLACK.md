# Slack Integration

The Slack skill connects agents to Slack workspaces via Socket Mode. Agents can receive messages, respond in channels and threads, react with emoji, and read message history. Multiple agents can be configured with their own Slack Apps and routed to different channels or workspaces.

## Quick Start

1. **Create a Slack App** at [api.slack.com/apps](https://api.slack.com/apps)
2. Enable **Socket Mode** and generate an app-level token (`xapp-...`)
3. Add bot scopes: `chat:write`, `reactions:write`, `reactions:read`, `channels:history`, `groups:history`, `im:history`, `mpim:history`, `app_mentions:read`
4. Subscribe to events: `message.channels`, `message.groups`, `message.im`, `message.mpim`, `app_mention`, `reaction_added`
5. Install the app to your workspace and copy the bot token (`xoxb-...`)
6. Configure tokens via environment variables or `workforce.toml`

## Architecture

```
Slack API (Socket Mode WebSocket)
    │
    ▼
SlackTransport ─── WebSocket connection + reconnection
    │
    ▼
SlackEventHandler ─── Event parsing, activation gating, bot filtering
    │
    ▼
SlackAction (mpsc channel)
    │
    ▼
Dispatch Loop (main.rs) ─── Agent routing, session key, command parsing
    │
    ├── Commands (/new, /reset, /status, ...) → CommandResult
    │
    └── Agent Runner → LLM generates response
        │
        ▼
    SlackSurface ─── Send reply, add/remove reactions
```

Each workspace connection runs its own WebSocket transport loop. Events are parsed, filtered, and forwarded as `SlackAction` values to a shared dispatch channel. The dispatch loop routes each action to the correct agent based on `agent_bindings` configuration.

## Configuration

### Tokens

Tokens can be set via environment variables (recommended for production) or directly in `workforce.toml`.

**Single agent (unsuffixed):**

```bash
SLACK_BOT_TOKEN=xoxb-your-bot-token
SLACK_APP_TOKEN=xapp-your-app-token
```

Creates a workspace with ID `"default"`.

**Multi-agent (suffixed by agent ID):**

```bash
SLACK_BOT_TOKEN_ALLAN=xoxb-allans-bot-token
SLACK_APP_TOKEN_ALLAN=xapp-allans-app-token
SLACK_BOT_TOKEN_ARCHITECT=xoxb-architects-bot-token
SLACK_APP_TOKEN_ARCHITECT=xapp-architects-app-token
```

Creates workspaces with IDs `"allan"` and `"architect"`. Each agent needs its own Slack App so it appears as a distinct bot identity in Slack.

Environment variables override any values set in `workforce.toml`.

### TOML Configuration

```toml
[slack]
response_mode = "mention_only"      # How the agent decides to respond
thread_isolation = true             # Separate session per thread
activation_keywords = ["deploy"]    # Keywords that trigger response

[slack.agent_bindings]
allan = "allan"                     # workspace "allan" → agent "allan"
default = "allan"                   # workspace "default" → agent "allan"

# Workspace entries (usually set via env vars instead)
[[slack.workspaces]]
id = "allan"
bot_token = "xoxb-..."
app_token = "xapp-..."
channels = []                       # Empty = all channels
```

### Configuration Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `response_mode` | String | `"mention_only"` | Global response behaviour (see below) |
| `thread_isolation` | Bool | `true` | Whether each thread gets its own session |
| `activation_keywords` | List | `[]` | Keywords that trigger a response regardless of mode |
| `agent_bindings` | Map | `{}` | Maps workspace/channel IDs to agent IDs |
| `workspaces[].id` | String | required | Unique workspace identifier |
| `workspaces[].bot_token` | String | required | Slack bot token (`xoxb-...`) |
| `workspaces[].app_token` | String | required | Slack app-level token (`xapp-...`) |
| `workspaces[].channels` | List | `[]` | Channel allowlist (empty = all channels) |

## Response Modes

The response mode controls when the agent generates a text response. Set globally via `slack.response_mode` or per-channel via group overrides.

| Mode | Behaviour |
|------|-----------|
| `mention_only` | Respond when @mentioned, in DMs, when activation keywords match, or in threads where the agent is already engaged. This is the default. |
| `all` | Respond to every message in allowed channels. |
| `quiet` | Only add emoji reactions, never send text responses. Useful for observer agents. |

### Activation Keywords

Keywords provide a way to trigger responses without @mentioning the bot. When any keyword appears in a message (case-insensitive), the agent responds regardless of response mode.

```toml
[slack]
activation_keywords = ["help", "deploy", "review"]
```

## Thread Isolation

When `thread_isolation = true` (default), each Slack thread gets its own independent session context. This means:

- Conversations in different threads don't bleed into each other
- Each thread has its own message history and context window
- Top-level channel messages start fresh sessions

Session key format:
- With isolation: `slack:{workspace}:{channel}:{thread_ts}`
- Without isolation: `slack:{workspace}:{channel}`

Thread isolation can be overridden per-channel using group overrides.

## Multi-Agent Setup

Each agent that needs a distinct identity in Slack requires its own Slack App. This gives each agent its own bot name, avatar, and token pair.

### Step-by-step

1. Create a Slack App for each agent (e.g., "Allan", "Architect")
2. Set up tokens using suffixed env vars:
   ```bash
   SLACK_BOT_TOKEN_ALLAN=xoxb-...
   SLACK_APP_TOKEN_ALLAN=xapp-...
   SLACK_BOT_TOKEN_ARCHITECT=xoxb-...
   SLACK_APP_TOKEN_ARCHITECT=xapp-...
   ```
3. Configure agent bindings to route workspaces to agents:
   ```toml
   [slack.agent_bindings]
   allan = "allan"
   architect = "architect"
   ```

### Agent Routing

When a message arrives, the dispatch loop resolves which agent should handle it:

1. **Channel binding** — check `agent_bindings` for the channel ID
2. **Workspace binding** — check `agent_bindings` for the workspace ID
3. **Default agent** — fall back to the default agent from the agent registry

### Shared Channels (Claim-Based Coordination)

When multiple agents share the same channel, the `ChannelCoordinator` prevents duplicate responses using a claim protocol:

1. If a message @mentions a specific agent, that agent claims it; others stand down
2. If no agent is mentioned, the first agent to process the message claims it
3. If a `default_agent` is configured for the shared channel, it gets priority for unclaimed messages
4. Claims expire after 5 minutes

```toml
[[slack.shared_channels]]
channel_id = "C_DEV"
default_agent = "allan"

[[slack.shared_channels.agents]]
id = "allan"
bot_user_id = "U_ALLAN_BOT"

[[slack.shared_channels.agents]]
id = "architect"
bot_user_id = "U_ARCHITECT_BOT"
```

## Team Channels

Team channels enable multi-agent collaboration in a single Slack channel. A team channel has a lead agent, multiple agents with distinct bot identities, and optionally a workflow that orchestrates their work.

### Configuration

```toml
[[slack.team_channels]]
team_id = "dev-team"
channel_id = "C0FAKECHAN6"
lead = "martin"
workflow_id = "dev-pipeline"

[slack.team_channels.agent_identities]
martin = { workspace_id = "martin" }
brian = { workspace_id = "brian" }
sarah = { workspace_id = "sarah" }
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `team_id` | String | Yes | Unique identifier for the team |
| `channel_id` | String | Yes | Slack channel ID |
| `lead` | String | No | Agent ID of the team lead (handles triage/routing) |
| `workflow_id` | String | No | Workflow definition ID to run for generic messages |
| `agent_identities` | Map | No | Maps agent IDs to their Slack workspace/bot identity |

### How It Works

When a message arrives in a team channel:

1. **@mention a specific agent** — routes directly to that agent (bypasses workflow)
2. **Generic message + `workflow_id` set** — starts the configured workflow
3. **Generic message + no `workflow_id`** — uses the orchestrator to pick an agent

### Per-Agent Bot Identity

Each agent in a team channel posts messages using its own Slack bot token. This means each agent appears with its own name and avatar — Martin posts as Martin, Brian posts as Brian, etc.

This requires each agent to have its own Slack App (see [Multi-Agent Setup](#multi-agent-setup)). The `agent_identities` map links each agent to its workspace, which determines which bot token is used for posting.

During workflow execution, step outputs and status messages are posted using the correct agent's bot identity based on the step's `agent` field.

### Workflow Integration

When `workflow_id` is set, generic messages in the team channel start a structured workflow instead of ad-hoc delegation. The workflow:

- Runs each step with the assigned agent
- Posts step outputs to the Slack thread using each agent's bot identity
- Supports all [Slack Thread Control](WORKFLOWS.md#slack-thread-control) commands (stop, pause, resume, goto, skip)
- Pauses at human review gates and resumes when the human replies

See [Workflows](WORKFLOWS.md) for the full workflow TOML format and control commands.

## Agent Tools

When a Slack surface is registered, agents get access to these tools for interacting with Slack programmatically during their reasoning:

### `messaging.send`

Send a message to a channel or thread.

```json
{
  "channel": "C0123ABC",
  "text": "Hello from the agent!",
  "thread_id": "1234567890.123456",
  "platform": "slack"
}
```

- `channel` (required) — Slack channel ID
- `text` (required) — Message content
- `thread_id` (optional) — Thread timestamp for threaded replies
- `platform` (optional) — Auto-detected if omitted

### `messaging.read`

Read recent messages from a channel.

```json
{
  "channel": "C0123ABC",
  "limit": 20,
  "platform": "slack"
}
```

- `channel` (required) — Slack channel ID
- `limit` (optional) — Number of messages (default 20, max 100)
- `platform` (optional) — Auto-detected if omitted

Returns an array of messages with `user`, `text`, `timestamp`, `message_id`, and `thread_id`.

### `reactions.add`

Add an emoji reaction to a message.

```json
{
  "channel": "C0123ABC",
  "message_id": "1234567890.123456",
  "emoji": "thumbsup"
}
```

Accepts emoji in multiple formats: Unicode (`👍`), shortcodes (`:thumbsup:`), or plain names (`thumbsup`).

### `reactions.remove`

Remove a previously added emoji reaction.

```json
{
  "channel": "C0123ABC",
  "message_id": "1234567890.123456",
  "emoji": "thumbsup"
}
```

## Processing Indicators

The agent automatically adds emoji reactions to show processing state:

| Stage | Emoji | Meaning |
|-------|-------|---------|
| Processing | 👀 `eyes` | Agent is working on the message |
| Complete | ✅ `white_check_mark` | Response sent successfully |
| Error | ❌ `x` | Something went wrong |

The `eyes` reaction is removed once the agent finishes processing. This gives users visual feedback without cluttering the channel.

## Bot Commands

Users can send commands to the agent in Slack. Commands start with `/` and are handled before reaching the LLM.

| Command | Aliases | Description |
|---------|---------|-------------|
| `/new [model]` | `/n` | Start a new session, optionally switch model |
| `/reset` | `/r`, `/clear` | Clear session history |
| `/status` | `/s` | Show current session info |
| `/compact` | `/gc` | Trigger context window compaction |
| `/context` | `/ctx` | Show context window usage |
| `/model [name]` | `/m` | Show or switch the current model |
| `/help` | `/h`, `/?` | List available commands |
| `/health` | — | Show model health status |
| `/wake` | `/w` | Trigger immediate heartbeat check |
| `/heartbeat` | `/hb` | Show heartbeat status and history |

Commands return ephemeral responses that aren't stored in session history. Unknown commands are passed through to the LLM as regular messages.

## Group Chat Configuration

For fine-grained control over how the agent behaves in specific channels, use the groups config:

```toml
[groups]

[groups.defaults]
activation = "mention"              # Default activation mode
response_mode = "auto"              # Default response delivery
thread_isolation = true

[[groups.channels]]
id = "C_GENERAL"
activation = "smart"                # Smart activation for this channel
keywords = ["help", "question"]
agent = "allan"

[[groups.channels]]
id = "C_ANNOUNCEMENTS"
activation = "quiet"                # Only observe, no text responses
```

### Activation Modes

| Mode | Behaviour |
|------|-----------|
| `mention` | Respond when @mentioned or in active threads (default) |
| `always` | Respond to every message |
| `smart` | Respond to questions, keywords, mentions, and active threads |
| `quiet` | Only observe and react with emoji |
| `keyword` | Only respond when activation keywords match |

### Response Delivery Modes

| Mode | Behaviour |
|------|-----------|
| `auto` | Reply in thread if message was threaded, inline otherwise (default) |
| `reply` | Always respond in a thread |
| `inline` | Always respond directly in the channel |

## Message Formatting

Agent responses are automatically formatted for Slack:

- Markdown headers (`#`, `##`, `###`) are converted to bold text (`*text*`)
- Markdown tables are converted to bulleted lists (Slack doesn't support tables)
- Code blocks are preserved as-is
- Long messages are split into multiple messages at paragraph breaks, preserving code blocks

Maximum message length for Slack is 4000 characters. Messages exceeding this are chunked automatically.

## Socket Mode Details

The Slack skill uses Socket Mode (WebSocket) rather than the Events API (HTTP webhooks). This means:

- **No public URL required** — the bot connects outbound to Slack
- **Works behind firewalls and NAT** — no inbound connections needed
- **Automatic reconnection** — reconnects with 5-second delay on disconnect
- **Envelope acknowledgment** — each event is acknowledged within 3 seconds

### Required Slack App Settings

**OAuth Scopes (Bot Token):**
- `chat:write` — Send messages
- `reactions:write` — Add emoji reactions
- `reactions:read` — Read emoji reactions
- `channels:history` — Read public channel messages
- `groups:history` — Read private channel messages
- `im:history` — Read DM messages
- `mpim:history` — Read group DM messages
- `app_mentions:read` — Receive @mention events

**Event Subscriptions (Socket Mode):**
- `message.channels` — Messages in public channels
- `message.groups` — Messages in private channels
- `message.im` — Direct messages
- `message.mpim` — Multi-person / group DMs (see [SLACK_GROUP_DM.md](SLACK_GROUP_DM.md))
- `app_mention` — When the bot is @mentioned
- `reaction_added` — When a reaction is added to a message

**App Home:**
- Enable the **Messages Tab** to allow DMs with the bot

## Troubleshooting

**"Sending messages to this app has been turned off"**
Enable the Messages Tab in your Slack App's App Home settings.

**Bot doesn't respond to messages**
- Check response mode — in `mention_only` mode, the bot only responds to @mentions and DMs
- Verify the bot is in the channel (invite it with `/invite @botname`)
- Check channel allowlist — if `channels` is non-empty, only listed channels are monitored

**Bot doesn't respond in a group DM (multi-person DM)**
Adding the `mpim:*` scopes is not enough — you must also subscribe to the
`message.mpim` event and reinstall the app, and the bot must be a member of the
group DM. See [SLACK_GROUP_DM.md](SLACK_GROUP_DM.md).

**Multiple agents responding to the same message**
Configure shared channels with `ChannelCoordinator` to use claim-based routing.

**Agent says it can't read channel history**
Ensure `messaging.read` tool is registered (requires workforce version with messaging tools) and that the bot has `channels:history` / `groups:history` / `im:history` scopes.
