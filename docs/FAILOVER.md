# LLM Failover

Workforce includes a multi-layered resilience system that keeps agents running when LLM providers hit rate limits, experience outages, or become overloaded. This document covers every aspect of failover: what it does, how to configure it, and how agents can manage it at runtime.

## Overview

The system has three layers, each solving a different problem:

```
Request
  |
  v
FailoverProvider          Model-level failover
  |                       Tries different models in order
  |                       (e.g., Opus -> Sonnet -> Haiku -> Grok)
  v
PooledProvider            Token-level failover
  |                       Rotates between accounts for the same provider
  |                       (e.g., Anthropic account A -> account B -> account C)
  v
LLM Provider              Actual API call
                          (Anthropic, Grok, OpenRouter)
```

**FailoverProvider** handles model-level resilience. When Claude Opus returns a 429, it tries Sonnet, then Haiku, then Grok -- walking down a configurable chain of models that can span multiple providers.

**PooledProvider** handles token-level resilience. Rate limits are per account (all models on the same API key share one bucket), so the only way to avoid rate limits within the same provider is to have multiple accounts. The pool rotates between them.

**LLM Provider** is the actual HTTP client (Anthropic, OpenRouter, Grok). It forwards `retry-after` headers back to the pool so it knows exactly how long to wait.

## Quick Start

Minimal failover configuration in `workforce.toml`:

```toml
[llm]
provider = "anthropic"
default_model = "claude-opus-4-20250514"
failover = ["claude-sonnet-4-5-20250514", "claude-haiku-4-5-20251001"]
```

This gives you: Opus as the primary model, falling back to Sonnet, then Haiku, all on the same Anthropic account.

## Configuration

### Basic Failover Chain

The `failover` field is an ordered list of models to try when the primary fails:

```toml
[llm]
provider = "anthropic"
default_model = "claude-opus-4-20250514"
failover = [
    "claude-sonnet-4-5-20250514",
    "claude-haiku-4-5-20251001",
]
```

The primary model (`default_model`) is always tried first. On failure, models in `failover` are tried in order.

### Cross-Provider Failover

Chain entries can include a provider prefix to route to a different LLM provider:

```toml
[llm]
provider = "anthropic"
default_model = "claude-opus-4-20250514"
failover = [
    "claude-sonnet-4-5-20250514",
    "claude-haiku-4-5-20251001",
    "grok:grok-4",
    "openrouter:anthropic/claude-sonnet-4",
]
```

Entries without a prefix use the primary provider. Entries with a `provider:model` prefix route to that external provider.

**Supported provider prefixes:**

| Prefix | Provider | Env Var Required |
|--------|----------|------------------|
| *(none)* | Primary provider (from `llm.provider`) | *(already configured)* |
| `grok` | Grok (xAI) | `XAI_API_KEY` |
| `openrouter` | OpenRouter | `OPENROUTER_API_KEY` |
| `gemini` | Google Gemini | `GOOGLE_API_KEY` |
| `ollama` | Ollama (local) | `OLLAMA_URL` |

External providers are created automatically from env vars at startup.

### Multiple Accounts

Rate limits are **per API key** -- each key has its own rate-limit bucket. All models on the same key share that bucket, so failing over from Opus to Haiku on the same key doesn't help if the key itself is rate-limited.

To solve this, add further keys:

```toml
[llm]
provider = "anthropic"
default_model = "claude-opus-4-20250514"
failover = ["claude-sonnet-4-5-20250514", "claude-haiku-4-5-20251001"]
accounts = ["ANTHROPIC_API_KEY_2", "ANTHROPIC_API_KEY_3"]
```

The `accounts` field lists env var names containing additional API keys. These are loaded into the token pool at startup. When one account hits a 429, the pool automatically rotates to the next available account.

Set the env vars on the server:

```bash
# In hub.env
ANTHROPIC_API_KEY=sk-ant-api03-primary-key...
ANTHROPIC_API_KEY_2=sk-ant-api03-backup-key...
ANTHROPIC_API_KEY_3=sk-ant-api03-another-key...
XAI_API_KEY=xai-...
```

### How Pool + Chain Work Together

When a request fails:

1. **PooledProvider** tries all available accounts for the current model
2. If all accounts are rate-limited, the error propagates up
3. **FailoverProvider** catches the error and moves to the next model in the chain
4. The next model goes through the pool again (if it uses the primary provider)
5. External provider models (e.g., `grok:...`) bypass the pool and call directly

Example with 2 accounts and a chain of `[opus, sonnet, haiku, grok:grok-4]`:

```
opus (account A) -> 429
opus (account B) -> 429
  (all accounts exhausted for opus)
sonnet (account A) -> 429  (account A still rate-limited)
sonnet (account B) -> 429  (account B still rate-limited)
  (all accounts exhausted for sonnet)
haiku (account A) -> 429
haiku (account B) -> 429
  (all accounts exhausted for haiku)
grok:grok-4 -> success!
```

### Model Aliases

Map friendly names to full model identifiers:

```toml
[llm.aliases]
fast = "claude-haiku-4-5-20251001"
smart = "claude-opus-4-20250514"
default = "claude-sonnet-4-5-20250514"
```

Aliases work in the failover chain too:

```toml
failover = ["fast", "grok:grok-4"]
```

### Per-Agent Configuration

Each agent can override the global LLM config:

```toml
[llm]
provider = "anthropic"
default_model = "claude-sonnet-4-5-20250514"
failover = ["claude-haiku-4-5-20251001"]

[[agents]]
id = "researcher"
llm.default_model = "claude-opus-4-20250514"
llm.failover = ["claude-sonnet-4-5-20250514", "grok:grok-4"]
llm.accounts = ["ANTHROPIC_API_KEY_RESEARCHER"]

[[agents]]
id = "assistant"
# Uses global config (sonnet with haiku fallback)
```

Agent-level fields are optional; unset fields fall back to the global `[llm]` section.

### Retry Configuration

Control retry behavior for transient errors:

```toml
[llm.retry]
max_retries = 3          # Retry attempts before failover (default: 3)
initial_backoff_ms = 1000  # Initial backoff in ms (default: 1000)
backoff_multiplier = 2.0   # Exponential factor (default: 2.0)
max_backoff_ms = 30000     # Max backoff cap in ms (default: 30000)
```

Note: **Rate-limited (429) and overloaded (529) errors skip retries entirely** and failover immediately. Retries only apply to server errors (5xx) and timeouts, where a brief wait might help.

### Health Tracking

Health tracking marks models as "degraded" after repeated failures, pushing them to the end of the chain:

```toml
[llm.health]
failure_threshold = 3    # Failures to mark degraded (default: 3)
window_secs = 300        # Time window for counting failures (default: 300)
recovery_secs = 120      # Seconds before retrying degraded model (default: 120)
```

A degraded model isn't removed -- it's pushed to the end of the chain so healthy models are tried first. After `recovery_secs`, the system retries the degraded model. A single success clears the degraded status.

## Error Handling

Errors are classified to determine whether to retry, failover, or fail immediately:

| Error | HTTP Status | Behavior |
|-------|-------------|----------|
| Rate Limited | 429 | **Failover immediately** -- skip retries (rate window is minutes) |
| Overloaded | 529 | **Failover immediately** -- server can't handle load |
| Server Error | 500, 502, 503 | **Retry with backoff**, then failover |
| Timeout | *(timeout)* | **Retry with backoff**, then failover |
| Client Error | 400, 401, 403, 404, 422 | **Fail immediately** -- no retry or failover (user error) |

### retry-after Header

When the Anthropic API returns a 429, it includes a `retry-after` header indicating how many seconds to wait. This flows through:

1. **Anthropic API** returns `retry-after: 45` header
2. **Node.js proxy** extracts and forwards the header
3. **Anthropic client** (Rust) reads the header and appends `[retry-after: 45s]` to the error message
4. **PooledProvider** parses `[retry-after: 45s]` and uses 45s as the cooldown (instead of the default 60s)

This means the pool knows exactly when an account will be available again, instead of guessing.

### Streaming

Streaming calls (`create_message_stream`) also walk the failover chain, under the same retry and failover rules whether a call fails before its first event or part-way through. Most failures (429s, connection errors) happen before streaming starts, so moving on is invisible. When a provider breaks off after content has been streamed — an overloaded model, a 5xx, a stalled stream — the chain first sends a `Restart` event that voids everything the abandoned attempt streamed, then retries or fails over. Every surface takes the abandoned text back: the runner reports a `StreamDiscard { reason: Interrupted }` and clients rewind to where the model call began (`stream_discard` on the WebSocket protocol). If the chain is exhausted, no `Restart` is sent and the turn fails with the last error.

Nothing acts on a call's output until its stream completes — tools run only on the finished response — so regenerating a call, even on a different model, can never repeat a side effect. Only the failover layer starts over after content: the account pool rotates to its next account only while nothing has streamed, and otherwise hands the error up.

## Agent Tools

Agents can manage failover at runtime through system tools. These are gated by the policy engine (`system.*` tools can be restricted per agent).

### system.llm_status

View the current LLM configuration and health:

```json
// No input required
{}
```

Returns:
```json
{
  "active_model": "claude-opus-4-20250514",
  "is_model_override": false,
  "default_model": "claude-opus-4-20250514",
  "provider": "Anthropic",
  "failover_chain": ["claude-sonnet-4-5-20250514", "grok:grok-4"],
  "failover_chain_details": [
    {"entry": "claude-sonnet-4-5-20250514", "model": "claude-sonnet-4-5-20250514", "provider": "Anthropic"},
    {"entry": "grok:grok-4", "model": "grok-4", "provider": "grok"}
  ],
  "external_providers": ["grok"],
  "model_health": [
    {"model": "claude-opus-4-20250514", "recent_failures": 2, "is_degraded": false}
  ],
  "token_pool": {
    "size": 2,
    "entries": [
      {"name": "default", "is_rate_limited": false},
      {"name": "backup-1", "is_rate_limited": true, "rate_limited_secs_remaining": 23}
    ]
  }
}
```

### system.set_failover_chain

Update the failover chain at runtime:

```json
{
  "chain": ["claude-sonnet-4-5-20250514", "claude-haiku-4-5-20251001", "grok:grok-4"]
}
```

The chain is persisted to the vault and restored on restart. External provider prefixes must have a corresponding account (add one first with `system.add_account`).

### system.add_account

Add a backup account for any provider:

```json
{
  "name": "backup-anthropic",
  "provider": "anthropic",
  "api_key": "sk-ant-api03-..."
}
```

Or for an external provider:

```json
{
  "name": "grok-main",
  "provider": "grok",
  "api_key": "xai-..."
}
```

**Supported providers:** `anthropic`, `grok`, `openrouter`, `gemini`, `ollama`, `vllm`, `cerebras`. A Claude subscription token (`sk-ant-oat…`) and the removed `claude-max` provider are refused: a Claude plan runs only through the `claude-code` provider.

The tool validates the key with a test API call (max_tokens=1) before accepting it. The key is stored encrypted in the vault.

- **Primary providers** (anthropic): added to the token pool for automatic rotation on 429
- **External providers** (grok, openrouter): registered as failover chain providers, available for `provider:model` chain entries

There is no limit on the number of accounts.

### system.remove_account

Remove an account by name:

```json
{
  "name": "backup-anthropic"
}
```

Cannot remove the last primary account. Removes from pool, external providers, and vault.

### system.list_accounts

List all accounts across all providers:

```json
// No input required
{}
```

Returns:
```json
{
  "primary_accounts": [
    {"name": "default", "provider": "Anthropic", "type": "primary_pool", "is_rate_limited": false},
    {"name": "backup-anthropic", "provider": "Anthropic", "type": "primary_pool", "is_rate_limited": true, "rate_limited_secs_remaining": 15}
  ],
  "external_accounts": [
    {"name": "grok", "provider": "grok", "type": "external_provider"}
  ],
  "total_count": 3
}
```

Never exposes API key values.

### Legacy Tools

The following tools still work but are superseded by the more general account tools above:

- `system.add_llm_token` -- adds an API key for the primary provider to the pool (use `system.add_account` instead)
- `system.remove_llm_token` -- removes a token from the pool
- `system.list_llm_tokens` -- lists pool tokens

### Other LLM Tools

- `system.set_model` -- change the active model at runtime (validated with test call, persisted to vault)
- `system.list_models` -- query available models from the provider's API

## Vault Persistence

Runtime changes made via agent tools are persisted in the encrypted vault and restored on restart:

| Vault Path | Contents | Set By |
|------------|----------|--------|
| `llm/active_model` | Model override string | `system.set_model` |
| `llm/tokens/{name}` | API key (legacy) | `system.add_llm_token` |
| `llm/accounts/{name}` | JSON: `{provider, api_key}` | `system.add_account` |
| `llm/failover_chain` | JSON array of chain entries | `system.set_failover_chain` |

## Full Example

A production configuration with maximum resilience:

```toml
[llm]
provider = "anthropic"
default_model = "claude-opus-4-20250514"

# Model failover chain (tried in order)
failover = [
    "claude-sonnet-4-5-20250514",
    "claude-haiku-4-5-20251001",
    "grok:grok-4",
]

# Additional Anthropic accounts (env var names)
accounts = ["ANTHROPIC_API_KEY_2"]

# Friendly aliases
[llm.aliases]
fast = "claude-haiku-4-5-20251001"
smart = "claude-opus-4-20250514"

# Retry policy (for 5xx/timeouts only; 429s skip retries)
[llm.retry]
max_retries = 2
initial_backoff_ms = 500

# Health tracking
[llm.health]
failure_threshold = 3
window_secs = 300
recovery_secs = 120
```

Server env vars:

```bash
ANTHROPIC_API_KEY=sk-ant-api03-primary...
ANTHROPIC_API_KEY_2=sk-ant-api03-backup...
XAI_API_KEY=xai-...
```

This gives you:
- Opus as primary, with Sonnet, Haiku, and Grok as fallbacks
- Two Anthropic accounts that rotate on 429s
- Accurate cooldown timing from `retry-after` headers
- Health tracking that pushes degraded models to the end
- Everything configurable at runtime via agent tools
