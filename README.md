# Workforce

A self-hosted platform for running AI agents as a team: agents with their own
identity, memory, skills and permissions, reachable over Slack, Microsoft
Teams, a web UI, WebSocket/REST and MCP, with workflows, schedules, approvals
and a policy engine governing what each agent may do.

Workforce is written in Rust and is **source-available** under the
[Elastic License 2.0](LICENSE): free to self-host and modify, including inside
a commercial organisation; not to be offered to others as a hosted service.
See the [licence FAQ](LICENSE-FAQ.md).

## Quick start

You need Rust (the toolchain in `rust-toolchain.toml` installs itself through
`rustup`), Node.js 20+ for the web UI, and an LLM API key.

```bash
git clone https://github.com/driftwerk/workforce
cd workforce
make build                      # builds the web UI, then target/release/workforce

mkdir ../my-workforce && cd ../my-workforce
../workforce/target/release/workforce init      # writes workforce.toml and a default agent
export ANTHROPIC_API_KEY=sk-ant-...             # or configure another provider in workforce.toml
../workforce/target/release/workforce serve
```

`serve` starts:

| Listener | Default | What |
|---|---|---|
| Admin API | `http://localhost:8090` | Dashboard, teams, tasks, cost |
| Gateway | `http://localhost:8091` | Web UI, WebSocket and REST chat |

The gateway token is printed on start-up. Everything runs in one process with
SQLite storage; NATS, Postgres, Weaviate and vellev are optional and enabled in
`workforce.toml` when you need them.

For a one-off conversation without the daemon:

```bash
workforce chat
```

### Docker

```bash
cd deploy
cp ../workforce.toml .           # or the one `workforce init` wrote
cp .env.example .env             # set your keys
docker compose -f docker-compose.prod.yml up -d                          # Workforce only
docker compose -f docker-compose.prod.yml --profile distributed up -d    # plus NATS
docker compose -f docker-compose.weaviate.yml up -d                      # vector memory
```

### systemd

```bash
sudo cp deploy/workforce.service /etc/systemd/system/
sudo useradd -r -s /bin/false workforce
sudo mkdir -p /opt/workforce/{data,workspace}
sudo chown workforce:workforce /opt/workforce -R
sudo cp target/release/workforce /usr/local/bin/
sudo systemctl enable --now workforce
```

## Architecture

```
┌──────────────────────────────────────────────┐
│                Workforce hub                 │
│                                              │
│  ┌──────────┐  ┌──────────┐  ┌───────────┐   │
│  │  Slack / │  │ Gateway  │  │  Admin    │   │
│  │  Teams   │  │ WS, REST │  │  API      │   │
│  │          │  │ UI, MCP  │  │ :8090     │   │
│  └────┬─────┘  └────┬─────┘  └───────────┘   │
│       │             │                        │
│  ┌────▼─────────────▼────────────────────┐   │
│  │            Agent service              │   │
│  │  Sessions · Agent runner (LLM+tools)  │   │
│  │  Identity · Memory · Skills · Vault   │   │
│  │  Policy engine · Scheduler · Workflows│   │
│  └───────────────────────────────────────┘   │
│                                              │
│  ┌───────────────────────────────────────┐   │
│  │ Optional: NATS (multi-node), Postgres,│   │
│  │ Weaviate / vellev (vector memory)     │   │
│  └───────────────────────────────────────┘   │
└──────────────────────────────────────────────┘
```

More in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
[docs/CONFIGURATION.md](docs/CONFIGURATION.md) and the user documentation in
[end-user-docs/](end-user-docs/), which the hub also serves at `/docs`.

## Channels

| Channel | Connection | Config |
|---|---|---|
| CLI | `workforce chat` | — |
| Slack | Socket Mode | `[slack]` |
| Microsoft Teams | Bot Framework | `skills/teams-integration/config.toml` |
| WebSocket | `ws://host:8091/ws?token=…` | `[gateway]` |
| REST | `POST /api/chat` | `[gateway]` |
| MCP | `https://host/mcp` with a bearer token | `[remote_mcp]` |

See [docs/GATEWAY_PROTOCOL.md](docs/GATEWAY_PROTOCOL.md) for the WebSocket
message format.

## Crates

| Crate | What it does |
|---|---|
| `cli` | The `workforce` binary: `serve`, `chat`, `init`, `vault`, `workflow`, … |
| `hub`, `hub-core` | The hub: agent service, gateway, channels, scheduler, workflows, admin API |
| `agent-runtime` | The LLM tool loop: providers, tools, streaming, compaction, routing |
| `skills` | Integrations (Slack, Teams, GitHub, Jira, Linear, Google, Metabase, Xero, …) |
| `common`, `core` | Shared configuration, sessions, identity and domain types |
| `memory`, `lessons`, `self-learning`, `training` | Agent memory, lessons learned, self-improvement and training corpora |
| `orchestrator`, `playbook` | Teams, TOML workflows and interactive playbooks |
| `policy-engine`, `permissions`, `identity`, `pairing` | What agents and people may do, and who they are |
| `codebase-knowledge`, `knowledge-service`, `projects` | Code indexing, search and project workspaces |
| `wasm-runtime`, `wasm-skill-sdk` | Sandboxed WASM skills and the SDK for writing them (MIT) |
| `mcp-client`, `mcp-store`, `local-mcp`, `oauth-client` | MCP client and server pieces, and a local MCP server for Claude Code |
| `nats-bus`, `distributed`, `worker` | Multi-node operation and remote execution hosts |
| `postgres`, `vellev`, `crypto` | Storage backends and the vault's encryption |
| `decide`, `sift`, `games` | Typed decisions, checked data programs, team games |

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Report
security issues privately as described in [SECURITY.md](SECURITY.md).

## Licence

Copyright (c) 2026 Kingsley Hendrickse. Licensed under the
[Elastic License 2.0](LICENSE), except `crates/wasm-skill-sdk`, which is
[MIT](crates/wasm-skill-sdk/LICENSE). See [NOTICE](NOTICE) for bundled
third-party assets.
