---
name: Workforce Remote
description: Use when the user wants to act on a workforce install from Claude Code — author, validate, run or inspect workflows and playbooks, build/test/install WASM skills, manage vault secrets, create or run scheduled jobs, or ask a workforce agent a question. Backed by the workforce-local-mcp server's wf.* tools, which proxy the active remote hub's MCP endpoint as the connected user; wf.hubs lists hubs and wf.use switches between them.
---

# Workforce Remote

The `wf.*` tools are the active workforce hub's own MCP tools, proxied
through `workforce-local-mcp` with the user's personal token, so every
call runs under that user's permissions on the hub.

## Setup (once per hub)

The hub mints the token and prints the exact registration line. In a
Slack thread or web chat with any agent on that hub:

```
cmd: remote-mcp token laptop
```

or use the "Remote MCP tokens" card in the hub's My Settings page. Then:

```bash
workforce-local-mcp config workforce add myhub --url https://hub.example.com
# prompts for the wfr_… token (hidden)
```

Repeat per install (driftwerk, harbor, contoso). The first hub
registered is active.

## Working with hubs

- `wf.hubs` — registered hubs, which is active, connected or not, and the
  remote tool count. Run this first when a `wf.*` tool is missing or
  fails with "unreachable".
- `wf.use {name}` — switch the active hub. The `wf.*` tool set is rebuilt
  from the new hub and Claude Code is told the list changed; tool names
  stay the same across hubs, only the target changes.
- If no hub is active or the token expired, tell the user to register or
  re-mint a token; do not retry blindly.

## Typical remote tools

Names are the hub's tool names with dots replaced by underscores, under
the `wf.` prefix. What is present depends on the hub's allowlist and the
user's role:

| Tool | Use |
|-|-|
| `wf.workflow` | list / validate / save / start / status / abort workflows (action-dispatch) |
| `wf.playbook` | list / validate / save / start / continue / exit playbooks |
| `wf.wasm_skill_*` | scaffold, build, test, install, list, publish WASM skills |
| `wf.vault_set`, `wf.vault_list`, `wf.vault_get`, `wf.vault_delete` | manage secrets the user may reach |
| `wf.scheduler`, `wf.reminder` | create, list, run and inspect scheduled jobs |
| `wf.agent_list`, `wf.agent_chat`, `wf.agent_poll`, `wf.agent_history` | talk to the hub's agents; `agent_chat` returns `status: running` with a `turn_id` when a turn outlives the wait, poll it with `agent_poll` |
| `wf.identity_whoami` | confirm who the hub thinks you are |

A result whose text is a JSON object with `code: "approval_required"`
means the hub's permission model needs a human approval for that action;
relay the reason to the user rather than retrying.
