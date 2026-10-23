# Advisor

The advisor is a voluntary second-opinion surface. Agents can invoke an `advisor()` tool that forwards the current session to a configured reviewer and returns written advice — guidance, not a directive. The reviewer is **purely advisory**: it cannot edit, write, or execute. The calling agent owns every action.

## Overview

Three backends:

```
Agent calls advisor(question?)
  |
  v
+------------------+
|  AdvisorSource   |
+------------------+
  |
  +-- "model"        -> LLM via the agent's own provider (no tools)
  |                     FailoverProvider owns retries.
  |
  +-- "claude_code"  -> contained CLI subprocess: none of its own tools or
  |                     settings; read-only file, web and knowledge
  |                     tools served through the gate.
  |                     CC's own retry handles 429 / transients.
  |                     Subscription-friendly. Never billed silently.
  |
  +-- "agent"        -> Another configured agent's LLM (no tools)
                        Target must opt in via `advisor_mode = true`.
```

Defence-in-depth keeps the advisory-only contract honest:

- **Model** / **Agent** — no tool definitions forwarded, so there's nothing to call.
- **Claude Code** — read-only tool allowlist, plus a `max_turns` budget.
- All three paths get an appended system directive stating the rules in plain English.

## Quick start — Claude Code as advisor

This is the primary case (subscription-cheap, the same OAuth token your `/cc` chat uses). Paste into the per-agent `{workspace}/config.toml`:

```toml
# ── Advisor — second-opinion / review tool ───────────────────────────
# Adds an `advisor` tool to this agent. Purely advisory — the reviewer
# cannot edit, write, or execute. The agent calls it voluntarily; if
# the configured backend is unavailable, the agent surfaces the reason
# to the user and continues without advice.

[advisor]
enabled = true
source = "claude_code"            # "model" | "claude_code" | "agent"

# Payload forwarded to the advisor. Default "transcript" everywhere.
# payload = "transcript"          # "transcript" | "summary"
# payload_max_tokens = 200000     # tail-preferred truncation past this cap

# Per-turn invocation cap (silent refusal once reached).
# per_turn_invocation_limit = 3

# ── source = "claude_code" ───────────────────────────────────────────
# The Claude login is resolved like every subscription call, in the
# agent's [llm.logins] tier order (by default):
#   agent  vault `users/{your-uid}/claude_code/oauth_token`
#   → global vault `users/{your-uid}/claude_code/oauth_token`
#   → agent  vault `claude_code/oauth_token`           (the agent's shared login)
#   → global vault `claude_code/oauth_token`           (the install's login)
#
# Cost guard: CC failures DO NOT silently fall back to a paid model.
# Flip the next line on if you want failover to the model below.
cc_allow_failover_to_models = false
# cc_fallback_model = "expensive"

# How many turns CC may take (each tool call consumes one turn).
# Five gives the read-only tools (file read, grep, glob, web fetch and
# search) headroom to read before synthesising.
# cc_max_turns = 5
# cc_effort = "high"

# Optional extra guidance prepended to the advisor's prompt.
# system_prompt_suffix = "Focus on security and concurrency."
```

## Source = `"model"`

Direct LLM call through this agent's provider. `FailoverProvider` owns failover and retries — no extra knobs needed at the advisor layer.

```toml
[advisor]
enabled = true
source = "model"
model = "expensive"               # tier key or specific model id

# Optional:
# payload = "transcript"
# payload_max_tokens = 200000
# per_turn_invocation_limit = 3
# system_prompt_suffix = "..."
```

The `model` field accepts either a tier key (resolved via this agent's `TierRegistry`) or a concrete model id. When unset, the agent's default model is used.

## Source = `"agent"`

Dispatch to a different configured agent. No tools are forwarded, so the advisory-only contract holds by construction. The target's `LlmProvider` is used (its model, its vault, its tier).

```toml
[advisor]
enabled = true
source = "agent"
agent_id = "reviewer"

# Optional payload / limit knobs as above.
```

The target agent must opt in via `advisor_mode = true` in its `[[agents]]` entry in `workforce.toml`:

```toml
[[agents]]
id = "reviewer"
advisor_mode = true               # opt-in: may be called as another agent's advisor
# ...
```

Without that opt-in the dispatch is refused with `AgentNotFound`. Self-references (`agent_id` pointing at the calling agent) are also refused.

## Configuration reference

All fields live under `[advisor]`. The per-agent `{workspace}/config.toml` overrides the global `workforce.toml` `[advisor]` section field-by-field.

| Field | Default | Notes |
| --- | --- | --- |
| `enabled` | `false` | Opt-in. When false, the tool is not registered. |
| `source` | `"model"` | `"model"`, `"claude_code"`, or `"agent"`. |
| `payload` | `"transcript"` | `"transcript"` or `"summary"`. |
| `payload_max_tokens` | `200000` | Tail-preferred truncation when exceeded. |
| `per_turn_invocation_limit` | `3` | Calls above this return `TurnLimitExceeded`. |
| `model` | `None` | Tier key or model id (model source). |
| `cc_allow_failover_to_models` | `false` | CC cost guard. Off = surface failure, don't bill API. |
| `cc_fallback_model` | `None` | Used when `cc_allow_failover_to_models = true`. |
| `cc_max_turns` | `5` | CC turn budget. Each tool call consumes one. |
| `cc_effort` | `"high"` | `--effort` passed to the CC CLI. |
| `agent_id` | `None` | Target agent id for `source = "agent"`. |
| `system_prompt_suffix` | `None` | Extra guidance appended to the advisor's prompt. |

## Tool API

When enabled, the agent gets an `advisor` tool. The schema:

```json
{
  "question": "string (optional) — focused question, or empty for general review",
  "session_id": "string (optional) — defaults to the in-scope session"
}
```

Successful result:

```json
{
  "advice": "...the advisor's written response...",
  "source": "claude_code" | "model" | "agent:<id>",
  "model": "claude-code" | "<model name>" | null,
  "duration_ms": 1234
}
```

Unavailable result (the agent is instructed to relay this to the user and continue without advice):

```json
{
  "advisor_unavailable": "Claude Code advisor failed and `cc_allow_failover_to_models` is false (cost guard). Exact failure: exit 1 — ...",
  "failure_kind": {"kind": "failover_disabled", "detail": "..."}
}
```

## Failure handling

The advisor is a checkpoint, never a hard dependency. Every failure is non-blocking: the tool returns `advisor_unavailable` with a typed `failure_kind` and the agent continues. Failure kinds:

- `no_login` — CC: no Claude login of the payer's resolves on this agent; the notice says how to connect one.
- `cli_missing` — CC: `claude` binary not on `PATH`.
- `rate_limit` — backend reported 429.
- `auth_failed` — invalid or expired credential.
- `agent_not_found` — target agent missing or `advisor_mode = false`.
- `failover_disabled` — CC failed and `cc_allow_failover_to_models = false`.
- `turn_limit_exceeded` — `per_turn_invocation_limit` reached for this user turn.
- `misconfigured` — e.g. `source = "agent"` with no `agent_id`, self-reference, advisor not enabled.
- `transient` / `unknown` — anything else, with the underlying detail attached.

The system prompt explicitly instructs the agent to relay the failure reason to the user verbatim and skip the advisor for the rest of the turn — no retry loops.

## Cost attribution

| Source | Where cost is recorded | Attribution |
| --- | --- | --- |
| `model` | `cost_tracker.record` with `task_type = "advisor"`, `description = "advisor:model"` | `identity_id = current_identity_id()` if a human is in scope; agent-only fallback. |
| `agent` | Same as `model`, `description = "advisor:agent"` | Same human-first rule. Recorded against the **calling** agent (it's spending the budget), even though the target agent's `LlmProvider` made the call. |
| `claude_code` | Tracing log only (`target = "advisor"`) — matches `claude_code_chat` / `claude_pilot` | `cc.cost_usd` is `$0` for Max subscriptions and a real cost otherwise; the standard cost tracker can't capture that cleanly, so the CC value is logged natively. |

The human-user-first rule means a user-provided CC key has its cost attributed to that user, while a shared key has cost attributed to whichever human instantiated the call.

## Per-turn invocation limit

Counted from the session transcript — every tool message named `advisor` after the most recent user message counts. The cap survives process restarts and is enforced before any backend dispatch. Once the limit is reached, further calls return `TurnLimitExceeded` with `human_reason` text the agent relays to the user.

## Workflows

Advisor is **interactive-only** for now. Workflow steps run in channels prefixed `workflow:`; the tool detects this and refuses with `Misconfigured`. To revisit later, the workflow runtime would need to opt advisor into the step's `allowed_tools` whitelist.

## System-prompt injection

When `effective_advisor.enabled` is true and the `advisor` tool is registered on the agent, an instructional block is appended to the agent's system prompt (in the cached portion, so prompt-cache reuse holds). It tells the agent:

- When to call `advisor` — before substantive work, when stuck, before declaring done.
- That the result is guidance, not a directive.
- To relay `advisor_unavailable` failures to the user and continue without retrying for the same reason.

Without this snippet, voluntary tools are rarely invoked.

## Anthropic TOS note

Claude Max OAuth tokens may **only** flow through the `claude` CLI subprocess — using the Messages API with a Max token violates Anthropic's TOS. The CC advisor backend deliberately spawns the CLI; do not refactor it to call the API directly.

## Related docs

- [FAILOVER.md](./FAILOVER.md) — model-level failover used by the `model` source.
- [CONFIGURATION.md](./CONFIGURATION.md) — config layering (global → agent → workspace).
