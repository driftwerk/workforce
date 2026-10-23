# Use Case Guides

Practical guides for common Workforce setups.

## Guide 1: Local Agent Testing with Persistent Memory

Run a local agent that remembers across sessions using workspace identity files.

### Setup

```bash
# Build
cargo build --release

# Create a workspace
mkdir -p ~/my-agent/memory
cd ~/my-agent

# Initialise
~/path/to/workforce init
```

### Create Identity Files

**SOUL.md** — Who the agent is:
```markdown
# SOUL.md
You're a helpful coding assistant. You prefer concise answers.
You suggest tests for any code you write.
```

**USER.md** — About the human:
```markdown
# USER.md
- Name: Alice
- Prefers: Rust, TypeScript
- Working on: a REST API project
```

**MEMORY.md** — Long-term memory:
```markdown
# MEMORY.md
## Project Context
- Using Axum 0.8 for the API
- PostgreSQL with deadpool for connection pooling
- Tests use testcontainers
```

### Configure the LLM

Edit `workforce.toml`:
```toml
[llm]
provider = "anthropic"           # Or "openrouter"
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
max_rounds = 50
```

Set the API key:
```bash
export ANTHROPIC_API_KEY=sk-ant-api03-...
```

### Start Chatting

```bash
workforce --workdir ~/my-agent chat
```

The agent's system prompt is assembled from SOUL.md + USER.md + MEMORY.md + IDENTITY.md + TOOLS.md (whichever exist in the workspace). The agent can read and write files in the workspace.

### How Memory Works

- **SOUL.md, USER.md, IDENTITY.md** — Loaded into the system prompt at session start
- **MEMORY.md** — Loaded into system prompt (truncated to 4000 chars if large)
- **memory/*.md** — Available via filesystem tools (agent can read/write these)
- **memory/active-task.md** — Convention for tracking in-progress work

The agent can update these files during a session using its filesystem tools:
```
you> Remember that we decided to use JWT for auth
agent> I've noted that in memory/2026-02-10.md.
```

### Adding Policies

Create `policies/project.yaml`:
```yaml
rules:
  - name: "allow-read"
    scope: global
    tool: "filesystem.read"
    action: allow

  - name: "allow-write"
    scope: global
    tool: "filesystem.write"
    action: allow

  - name: "allow-safe-exec"
    scope: global
    tool: "exec.run"
    action: allow
    conditions:
      - field: command
        operator: not_equals
        value: "rm -rf /"

  - name: "deny-all-else"
    scope: global
    tool: "*"
    action: deny
    priority: -1
```

Run with the policy:
```bash
workforce --workdir ~/my-agent --policy policies/project.yaml chat
```

---

## Guide 2: Dev Team Setup

Configure a team of agents that collaborate on tasks.

### Define the Team

Edit `workforce.toml`:

```toml
[llm]
provider = "anthropic"
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
max_rounds = 50

[[teams]]
id = "backend"
name = "Backend Team"
lead = "architect"
pattern = "hierarchical"
require_review = true

[[teams.members]]
id = "architect"
role = "lead"
model = "claude-sonnet-4-20250514"
capabilities = ["design", "code-review", "decompose"]

[[teams.members]]
id = "rust-dev"
role = "backend-dev"
capabilities = ["code-write", "test", "debug"]

[[teams.members]]
id = "reviewer"
role = "reviewer"
capabilities = ["code-review", "security"]
```

### Run a Team Task

```bash
export ANTHROPIC_API_KEY=sk-ant-api03-...

workforce team "Design and implement a rate limiter middleware for Axum"
```

**What happens:**
1. The lead agent (`architect`) receives the task
2. It decomposes the task into subtasks based on member capabilities
3. Each subtask is assigned to a team member
4. Members execute their subtasks independently
5. The lead synthesises all results into a final response

**Output:**
```
[Final response from architect with the complete solution]

📋 Subtask Summary:
  1. rust-dev (Implement rate limiter struct): 1523 chars
  2. rust-dev (Write middleware integration): 892 chars
  3. reviewer (Security review): 445 chars

📊 Team 'Backend Team': 3 subtasks, 12000 in / 3500 out tokens
```

### Team Patterns

**Hierarchical** (default): Lead decomposes → members execute → lead synthesises.
```toml
pattern = "hierarchical"
```

**Pipeline**: Sequential processing. Each agent passes output to the next.
```toml
pattern = "pipeline"
```

**Parallel**: All agents work on the full task. Results are merged.
```toml
pattern = "parallel"
```

---

## Guide 3: Production Daemon with Slack

Run Workforce as a long-running service connected to Slack.

### Prerequisites

1. Create a Slack app at https://api.slack.com/apps
2. Enable Socket Mode
3. Add bot scopes: `chat:write`, `app_mentions:read`, `channels:history`, `im:history`
4. Install to workspace
5. Get the Bot Token (`xoxb-...`) and App Token (`xapp-...`)

### Configuration

```toml
[hub]
listen = "0.0.0.0:8090"
data_dir = "/opt/workforce/data"

[llm]
provider = "anthropic"
default_model = "claude-sonnet-4-20250514"

[gateway]
listen = "0.0.0.0:8091"

[slack]
bot_token = "xoxb-..."
app_token = "xapp-..."
channels = []  # Empty = all channels
```

### Start the Daemon

```bash
workforce serve --config workforce.toml
```

This starts:
- **Admin API** at `:8090` — dashboard, team status, cost tracking, audit log
- **Gateway** at `:8091` — WebSocket + REST for web/mobile clients
- **Slack bot** — Socket Mode connection, responds to messages and mentions

### Systemd Service

```bash
sudo cp deploy/workforce.service /etc/systemd/system/
sudo useradd -r -s /bin/false workforce
sudo mkdir -p /opt/workforce/{data,workspace}
sudo chown workforce:workforce /opt/workforce -R
sudo cp target/release/workforce /usr/local/bin/
sudo cp workforce.toml /opt/workforce/

# Set environment
sudo tee /opt/workforce/.env << EOF
ANTHROPIC_API_KEY=sk-ant-api03-...
WORKFORCE_GATEWAY_TOKEN=your-secret-token
SLACK_BOT_TOKEN=xoxb-...
SLACK_APP_TOKEN=xapp-...
EOF
sudo chmod 600 /opt/workforce/.env

sudo systemctl enable --now workforce
sudo journalctl -u workforce -f
```

### Docker Deployment

```bash
cd deploy

# Create .env file
cat > .env << EOF
ANTHROPIC_API_KEY=sk-ant-api03-...
WORKFORCE_GATEWAY_TOKEN=your-secret-token
RUST_LOG=info
EOF

# Copy config
cp ../workforce.toml .

# Start (basic)
docker compose -f docker-compose.prod.yml up -d

# Start with NATS for distributed agents
docker compose -f docker-compose.prod.yml --profile distributed up -d

# Start with Weaviate for vector memory
docker compose -f docker-compose.prod.yml --profile memory up -d

# Start with everything
docker compose -f docker-compose.prod.yml --profile distributed --profile memory up -d
```

### Admin API Endpoints

Once running, the admin API provides:

| Endpoint | Description |
|----------|-------------|
| `GET /api/dashboard` | System overview (agents, sessions, uptime) |
| `GET /api/teams` | List configured teams |
| `GET /api/tasks?status=running` | List tasks with optional status filter |
| `GET /api/cost` | Cost summary (today, this week, per agent) |
| `GET /api/audit?query=deploy` | Search audit log |

### Gateway WebSocket

Connect from a client:
```javascript
const ws = new WebSocket('ws://localhost:8091/ws?token=your-secret-token');

ws.onopen = () => {
    ws.send(JSON.stringify({
        type: 'chat',
        content: 'Hello!',
        channel: 'web'
    }));
};

ws.onmessage = (event) => {
    const msg = JSON.parse(event.data);
    if (msg.type === 'message') {
        console.log('Agent:', msg.content);
    }
};
```

See [Gateway Protocol](GATEWAY_PROTOCOL.md) for the full message format.

---

## Guide 4: Distributed Agents with NATS

Run agents across multiple machines using hub-and-spoke NATS topology.

### Hub Setup

On the central server:

```bash
# Start NATS with JetStream
docker run -d --name nats -p 4222:4222 -p 7422:7422 -p 8222:8222 \
    nats:2.10-alpine --js --sd /data

# Configure workforce to use NATS
cat >> workforce.toml << EOF
[nats]
url = "nats://localhost:4222"
EOF

# Start the hub
workforce serve
```

### Leaf Node Setup

On a remote machine (e.g., a GPU server):

```bash
# Generate NATS leaf node config
workforce node config \
    --hub-url nats-leaf://hub.example.com:7422 \
    --node-id gpu-node-1 \
    --output nats-leaf.conf

# Start NATS leaf node
nats-server -c nats-leaf.conf

# Start the workforce node
workforce node start \
    --hub-url nats-leaf://hub.example.com:7422 \
    --capabilities gpu,ml,python \
    --paths /data/models,/data/datasets
```

### How It Works

1. Hub runs the admin API, gateway, Slack bot, and scheduler
2. Leaf nodes connect to the hub via NATS
3. Tasks are routed to nodes based on capability matching (affinity rules)
4. Each node has a trust level that controls what tools it can use
5. If a node disconnects, tasks are queued until it reconnects (offline mode)

### Trust Levels

| Level | Access |
|-------|--------|
| `Untrusted` | No tool access |
| `Basic` | Read-only filesystem |
| `Standard` | Filesystem + limited exec |
| `Elevated` | Full filesystem + exec |
| `Full` | Everything including admin |
