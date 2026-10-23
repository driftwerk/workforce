# CLI Reference

The `workforce` binary provides all commands for running agents, managing teams, and operating the server.

## Global Options

```
workforce [OPTIONS] <COMMAND>
```

| Option | Description | Default |
|--------|-------------|---------|
| `--provider <PROVIDER>` | LLM provider: `anthropic`, `openrouter`, `grok`, `gemini` or `ollama` | `anthropic` |
| `--api-key <KEY>` | API key (prefer env vars instead) | From env |
| `--workdir <PATH>` | Working directory for file operations | `.` |
| `--policy <PATH>` | Policy file path | Default allow-all |

## Commands

### `workforce chat`

Interactive chat session with an agent. No server infrastructure needed — connects directly to the LLM.

```bash
workforce chat [OPTIONS]
```

| Option | Description | Default |
|--------|-------------|---------|
| `-a, --agent <ID>` | Agent ID | `default` |
| `-m, --model <MODEL>` | LLM model override | From config/default |

**In-session commands:**
- `/quit`, `/exit`, `/q` — End the session
- `/stats` — Show token usage
- `/clear` — (Not supported — session history is append-only)

**Examples:**
```bash
# Basic chat with Anthropic
export ANTHROPIC_API_KEY=sk-ant-api03-...
workforce chat

# Chat with a specific model
workforce chat --model claude-opus-4-20250514
```

### `workforce run`

Execute a one-shot task and exit. The agent runs tools in a loop until it produces a final response.

```bash
workforce run [OPTIONS] <TASK>
```

| Option | Description | Default |
|--------|-------------|---------|
| `-a, --agent <ID>` | Agent ID | `default` |
| `-m, --model <MODEL>` | LLM model override | From config/default |
| `--max-rounds <N>` | Max tool execution rounds | `20` |

**Examples:**
```bash
# One-shot task
workforce run "List all Rust files and count total lines"

# With custom working directory
workforce --workdir /path/to/project run "Find all TODO comments"

# Limit tool rounds
workforce run --max-rounds 5 "Quick summary of this project"
```

**Output:** The agent's response is printed to stdout. Stats (rounds, tool calls, tokens) are printed to stderr.

### `workforce serve`

Start the agent daemon. This runs the full server stack:
- Admin API on port 8090
- WebSocket + REST gateway on port 8091
- Slack bot (if configured)
- Scheduler
- Heartbeat system

```bash
workforce serve [OPTIONS]
```

| Option | Description | Default |
|--------|-------------|---------|
| `-c, --config <PATH>` | Config file path | `workforce.toml` |

**Examples:**
```bash
# Start with default config
workforce serve

# Start with custom config
workforce serve --config /opt/workforce/production.toml
```

**What starts:**
```
🚀 Starting Workforce Agent Service
  Provider: anthropic, Model: claude-sonnet-4-20250514
  Admin API: http://0.0.0.0:8090
  Gateway:   http://0.0.0.0:8091
  Slack:     connected (socket mode)
──────────────────────────────────────────────────
  ✓ Agent is ready. Waiting for messages...
```

Press Ctrl+C for graceful shutdown.

### `workforce team`

Run a task using a team of agents from the config. The lead agent decomposes the task, delegates subtasks to members, and synthesises the final response.

```bash
workforce team [OPTIONS] <TASK>
```

| Option | Description | Default |
|--------|-------------|---------|
| `-t, --team <ID>` | Team ID from config | First team |
| `-c, --config <PATH>` | Config file path | `workforce.toml` |

**Examples:**
```bash
# Run with the first configured team
workforce team "Design and implement a REST API for user management"

# Run with a specific team
workforce team --team backend "Add pagination to the list endpoints"
```

**Output:** The lead agent's synthesised response, followed by a subtask summary showing each member's contribution.

### `workforce init`

Generate default configuration files in the current directory.

```bash
workforce init
```

**Creates:**
- `workforce.toml` — Annotated default config
- `policies/default.yaml` — Default policy rules

### `workforce status`

Show system status (NATS connection, running agents, registered nodes).

```bash
workforce status
```

### `workforce node`

Node management for distributed execution.

```bash
workforce node <SUBCOMMAND>
```

| Subcommand | Description |
|------------|-------------|
| `config` | Generate NATS leaf node configuration |
| `info` | Show local node information |
| `start` | Start the local node (connect to hub) |
| `stop` | Stop the local node |
| `status` | Show node status |

**Examples:**
```bash
# Generate leaf node config for connecting to a remote hub
workforce node config --hub-url nats-leaf://hub.example.com:7422

# Start local node with specific capabilities
workforce node start --hub-url nats-leaf://hub:7422 --capabilities gpu,docker

# Start with filesystem paths exposed to agents
workforce node start --paths /data/project,/data/shared
```

### `workforce skill`

Skill management.

```bash
workforce skill <SUBCOMMAND>
```

| Subcommand | Description |
|------------|-------------|
| `list` | List installed skills |
| `search <QUERY>` | Search the skill registry |
| `show <NAME>` | Show skill details |
| `install <SOURCE>` | Install a skill (name or git URL) |
| `uninstall <NAME>` | Uninstall a skill |

### `workforce plugin`

Plugin management.

```bash
workforce plugin <SUBCOMMAND>
```

| Subcommand | Description |
|------------|-------------|
| `list` | List installed plugins |
| `install <SOURCE>` | Install a plugin |
| `uninstall <NAME>` | Uninstall a plugin |
| `status <NAME>` | Show plugin status |

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error (missing config, API failure, etc.) |

## Tips

- Use `RUST_LOG=debug` for verbose output when debugging
- The `--provider` flag accepts aliases: `claude` for `anthropic`, `xai` for `grok`, `google` for `gemini`
- File tools are sandboxed to `--workdir` — agents can't read/write outside it
- Stats are always on stderr, agent output on stdout — so you can pipe: `workforce run "summarise" > output.txt`
