# Policy Engine

The policy engine controls what tools agents are allowed to use. Every tool invocation is evaluated against a set of rules before execution. If no rule matches, the action is **denied by default** — this is a whitelist model.

## Decisions

Every policy evaluation results in one of three decisions:

- **Allow** — The tool call proceeds normally.
- **Deny** — The tool call is blocked. The agent receives an error message with the reason.
- **Require Approval** — The tool call runs only once a human approves it. It is refused if they deny it, nobody answers in time, or there is nobody to ask. This is the human-in-the-loop mechanism for sensitive operations.

## Rules

Rules are defined in YAML files. Each rule has:

| Field | Required | Description |
|---|---|---|
| `name` | Yes | Human-readable name for logging and debugging |
| `scope` | Yes | Who this rule applies to (see [Scopes](#scopes)) |
| `tool` | Yes | Tool name pattern to match (see [Tool Patterns](#tool-patterns)) |
| `action` | Yes | `allow`, `deny`, or `require_approval` |
| `conditions` | No | Fine-grained parameter checks (see [Conditions](#conditions)) |
| `priority` | No | Integer, higher = evaluated first. Default: `0` |

### Example

```yaml
rules:
  # Allow all filesystem operations globally
  - name: "allow-filesystem"
    scope: global
    tool: "filesystem.*"
    action: allow

  # Deny shell execution by default
  - name: "deny-exec"
    scope: global
    tool: "exec.*"
    action: deny

  # But allow exec for the trusted-dev agent
  - name: "allow-exec-for-trusted"
    scope:
      agent: "trusted-dev"
    tool: "exec.*"
    action: allow

  # Require human approval before sending email
  - name: "approve-email"
    scope: global
    tool: "email.send"
    action: require_approval
```

## Tool Patterns

Tool names follow a `category.action` convention. Patterns support:

| Pattern | Matches |
|---|---|
| `filesystem.read` | Exactly `filesystem.read` |
| `filesystem.*` | Any tool starting with `filesystem.` (read, write, edit, list) |
| `*` | All tools |

### Built-in Tools

Run `workforce tools` to list all available tools. The current built-in set:

| Tool | Description |
|---|---|
| `exec.run` | Execute shell commands |
| `filesystem.read` | Read file contents (with optional offset/limit) |
| `filesystem.write` | Create or overwrite files |
| `filesystem.edit` | Make precise edits to existing files |
| `filesystem.list` | List directory contents |
| `memory_search` | Search across memory files and vector store |
| `memory` | Store, recall, and manage memories (requires Weaviate) |
| `scheduler` | Create, list, and remove scheduled jobs (requires serve mode) |

Custom tools added via skills or plugins follow the same naming convention and are subject to the same policy rules.

## Scopes

Scopes determine who a rule applies to. There are four levels, from broadest to most specific:

| Scope | Specificity | YAML Syntax | Description |
|---|---|---|---|
| Global | 0 | `scope: global` | Applies to all agents |
| Team | 1 | `scope: { team: "backend-dev" }` | Applies to agents in a specific team |
| Role | 2 | `scope: { role: "developer" }` | Applies to agents with a specific role |
| Agent | 3 | `scope: { agent: "trusted-dev" }` | Applies to a single agent by ID |

### Scope Composition

Rules are evaluated in order of **priority first**, then **specificity**. The first matching rule wins.

This means:
- An agent-specific rule overrides a team rule, which overrides a global rule (when priorities are equal).
- A high-priority global rule can override an agent-specific rule (when the global rule has higher priority).

```yaml
rules:
  # Global: deny dangerous tools for everyone
  - name: "global-deny-dangerous"
    scope: global
    tool: "dangerous.*"
    action: deny

  # Agent-specific: allow dangerous tools for admin
  # (wins over global because higher specificity, same priority)
  - name: "admin-allow-dangerous"
    scope:
      agent: "admin"
    tool: "dangerous.*"
    action: allow
```

To make a global rule **override** more specific rules, give it a higher priority:

```yaml
rules:
  # This ALWAYS wins because priority 100 > default 0
  - name: "absolute-deny"
    scope: global
    tool: "nuclear.*"
    action: deny
    priority: 100

  # This won't help — the high-priority global rule is checked first
  - name: "admin-allow-nuclear"
    scope:
      agent: "admin"
    tool: "nuclear.*"
    action: allow
    priority: 0
```

## Conditions

Conditions allow rules to inspect the **parameters** of a tool call for fine-grained control. Each condition checks a field in the tool's input against a value using an operator.

| Field | Description |
|---|---|
| `field` | Path to the parameter (supports dot-notation for nested fields) |
| `operator` | Comparison operator (see below) |
| `value` | Value to compare against |
| `action` | Decision if this condition matches (`allow`, `deny`, `require_approval`) |

### Operators

| Operator | Description | Value Type |
|---|---|---|
| `equals` | Exact match | Any JSON value |
| `not_equals` | Not equal | Any JSON value |
| `starts_with` | String prefix match | String |
| `contains` | String substring match | String |
| `in` | Value is in a list | Array |
| `not_in` | Value is not in a list | Array |
| `less_than` | Numeric comparison | Number |
| `greater_than` | Numeric comparison | Number |

### How Conditions Work

1. The engine finds the first matching rule (by tool pattern, scope, priority).
2. If the rule has conditions, each condition is evaluated in order against the tool's parameters.
3. If a condition matches, **its** action is used (not the rule's default action).
4. If no condition matches, the **rule's default action** is used.

This lets you create rules where the default is restrictive but specific parameter values are allowed (or vice versa).

### Examples

**Allow filesystem writes only within the workspace:**

```yaml
rules:
  - name: "restrict-writes-to-workspace"
    scope: global
    tool: "filesystem.write"
    action: deny                          # Default: deny writes
    conditions:
      - field: "path"
        operator: starts_with
        value: "/workspace"
        action: allow                     # But allow if path is in /workspace
```

**Auto-approve small expenses, require approval for large ones:**

```yaml
rules:
  - name: "expense-approval"
    scope: global
    tool: "expenses.submit"
    action: require_approval              # Default: need approval
    conditions:
      - field: "amount"
        operator: less_than
        value: 50
        action: allow                     # Under $50: auto-approve
```

**Allow emails to known recipients, require approval for unknown:**

```yaml
rules:
  - name: "email-allowlist"
    scope: global
    tool: "email.send"
    action: require_approval              # Default: require approval
    conditions:
      - field: "recipient"
        operator: in
        value: ["alice@example.com", "bob@example.com"]
        action: allow                     # Known recipients: allow
```

**Nested field access (dot-notation):**

```yaml
rules:
  - name: "restrict-api-domain"
    scope: global
    tool: "api.call"
    action: deny
    conditions:
      - field: "target.domain"
        operator: equals
        value: "internal.example.com"
        action: allow
```

This would match parameters like `{"target": {"domain": "internal.example.com"}}`.

## Default Behaviour

If **no rule matches** a tool invocation, the policy engine returns **Deny** with the message:

> No policy rule matches tool '{tool}' for agent '{agent_id}'

This means every tool must be explicitly allowed by at least one rule. The `workforce init` command creates a default policy at `policies/default.yaml` that allows all tools:

```yaml
rules:
  - name: "allow-all"
    scope: global
    tool: "*"
    action: allow
```

Replace this with more specific rules as you lock things down.

## Loading Policies

Policies can be loaded from:

1. **CLI flag:** `workforce run --policy policies/strict.yaml "do something"`
2. **YAML string:** `PolicyEngine::from_yaml(yaml_str)`
3. **File path:** `PolicyConfig::from_file("policies/default.yaml")`

When no policy file is specified, the CLI defaults to allowing all tools.

## Evaluation Flow

```
Tool invocation
       │
       ▼
Sort rules by priority (desc), then specificity (desc)
       │
       ▼
Find first rule where:
  ├─ Tool pattern matches
  └─ Scope matches (global / team / role / agent)
       │
       ├─ No rule found → DENY (default)
       │
       ▼
  Rule has conditions?
       │
       ├─ No → Use rule's action
       │
       ▼
  Evaluate conditions in order:
       │
       ├─ Condition matches → Use condition's action
       │
       └─ No condition matches → Use rule's action
       │
       ▼
  Return: Allow / Deny / RequireApproval
```

## Logging

Every evaluation is logged via `tracing` with:
- Agent ID
- Tool name
- Matched rule name (or "default deny")
- Decision
- Evaluation time (microseconds)

This makes it easy to audit what happened and why.
