# Workflows

TOML-defined, step-by-step task execution for repeatable multi-step processes. Workflows complement SKILL.md files — skills define *how* to do a specific thing, workflows define *what order* to do things in.

## When to Use Workflows

**High impact:**

- Complex multi-step investigations (state persists across sessions, branches on findings)
- Multi-agent handoff chains (auto-route between teams based on outcomes)
- Repeatable processes (incident response, PR review pipelines, onboarding)
- Webhook-triggered automation (Linear issue labeled "bug" auto-starts bug-fix workflow)

**Don't use workflows for:**

- Simple "pick up ticket, push a PR" tasks — the agent handles these fine with SKILL.md
- One-off creative/exploratory tasks
- Tasks where a human is actively directing the agent

## Quick Start

### 1. Create a Workflow

Create a `.toml` file in your workflows directory (default: `.driftwerk/workflows/`):

```toml
[workflow]
id = "bug-fix"
name = "Bug Fix"
description = "Standard bug fix workflow"

[workflow.context]
ticket_id = { required = true, description = "Linear ticket identifier" }
repo = { required = true, description = "owner/repo format" }

[[step]]
id = "investigate"
name = "Investigate the issue"
task = """
Read the ticket. Search the codebase for related code.
Try to reproduce the issue. Determine root cause.
"""
branches = [
  { when = "already-fixed", goto = "close" },
  { when = "design-issue", goto = "escalate" },
]

[[step]]
id = "implement"
name = "Implement the fix"
task = "Fix the root cause. Write tests. Run the test suite."

[[step]]
id = "open-pr"
name = "Open pull request"
task = "Commit changes. Push branch. Open PR linking to the ticket."
pause = "awaiting-review"
branches = [
  { when = "changes-requested", goto = "implement" },
]

[[step]]
id = "merge"
name = "Merge and close"
task = "Merge the PR. Update the Linear ticket to Done."

[[step]]
id = "escalate"
name = "Escalate to product"
exit = true
handoff = "escalation_to_product"
task = "Package investigation findings and escalate."

[[step]]
id = "close"
name = "Close as already fixed"
exit = true
task = "Document where the fix exists. Close the ticket."
```

### 2. Start It

**Via Slack** (tell the agent):
```
Run the bug-fix workflow for DW-100 in driftwerk/workforce
```
The agent calls the `workflow` tool internally.

**Via CLI:**
```bash
workforce workflow validate .driftwerk/workflows/bug-fix.toml
workforce workflow list
```

**Via REST API:**
```bash
curl -X POST http://localhost:8090/api/workflows/bug-fix/run \
  -H "Content-Type: application/json" \
  -d '{"payload": {"ticket_id": "DW-100", "repo": "driftwerk/workforce"}}'
```

### 3. Monitor Progress

```bash
# List running instances
workforce workflow instances --status running

# Check a specific instance
workforce workflow status <instance-id>
```

Or via API:
```bash
curl http://localhost:8090/api/workflow-instances
curl http://localhost:8090/api/workflow-instances/<instance-id>
```

## TOML Format Reference

### Workflow Metadata

```toml
[workflow]
id = "bug-fix"              # Unique identifier (used in commands and API)
name = "Bug Fix"             # Human-readable name
description = "..."          # What this workflow does
version = "1.0"              # Optional, defaults to "1.0"
agent = "developer"          # Optional default agent for all steps
max_concurrent = 1           # Optional, instances that may be active at once
```

`max_concurrent` counts running and paused instances. A schedule slot that fires at the cap is skipped. A webhook or HTTP-trigger start at the cap is queued instead (status `queued`, FIFO by creation time) and promoted to `running` when a sibling instance finishes, so no event is lost.

### Context (Inputs)

Declare expected inputs for documentation and validation:

```toml
[workflow.context]
ticket_id = { required = true, description = "Linear ticket identifier" }
repo = { required = true, description = "owner/repo format" }
severity = { required = false, description = "Optional severity override" }
```

Context values are passed as the workflow payload and available to all steps as variables.

### Steps

Steps execute top-to-bottom by default. Each step runs as an independent agent conversation.

```toml
[[step]]
id = "investigate"           # Unique step ID
name = "Investigate"         # Human-readable name
task = "..."                 # Natural language instruction for the agent
```

The `task` field is the prompt sent to the agent. Multi-line strings use triple quotes:

```toml
task = """
Read the ticket. Search the codebase for related code.
Try to reproduce the issue.
"""
```

### Flow Control

| Feature | Syntax | Behavior |
|---------|--------|----------|
| Sequential | Steps in file order | Default: advance to next step |
| Branches | `branches = [{ when, goto }]` | Agent picks outcome, engine routes |
| Unconditional jump | `goto = "step-id"` | Always go to target instead of next |
| Pause | `pause = "reason"` | Workflow pauses, resumes externally |
| Exit | `exit = true` | Terminal step, workflow completes |
| Handoff | `handoff = "type"` | Exit creates a HandoffPackage |

#### Branches

The engine appends outcome labels to each step's prompt:

```
When you complete this step, end your response with one of these outcomes:
OUTCOME: already-fixed
OUTCOME: design-issue
(or no OUTCOME line to continue to the next step)
```

The engine parses the last `OUTCOME: <label>` line from the response. If no outcome line is found, the workflow falls through to the next step.

```toml
[[step]]
id = "investigate"
name = "Investigate"
task = "Look into the issue."
branches = [
  { when = "already-fixed", goto = "close" },
  { when = "design-issue", goto = "escalate" },
]
# If no OUTCOME line → falls through to the next step
```

#### Pause and Resume

A paused step stops execution until an external signal resumes it. This is useful for human review gates.

```toml
[[step]]
id = "open-pr"
name = "Open pull request"
task = "Open PR linking to the ticket."
pause = "awaiting-review"
branches = [
  { when = "changes-requested", goto = "implement" },
]
```

- The workflow executes the step, then pauses.
- An external signal (API call, agent tool, or another webhook) resumes with an outcome.
- The outcome routes via branches, or falls through to the next step.

Resume via API:
```bash
curl -X POST http://localhost:8090/api/workflow-instances/<id>/resume \
  -H "Content-Type: application/json" \
  -d '{"outcome": "changes-requested"}'
```

Or via the agent tool:
```
Resume workflow instance abc-123 with outcome "approved"
```

#### Goto

Unconditional jump — always goes to the target step instead of falling through:

```toml
[[step]]
id = "rollback"
name = "Rollback"
task = "Revert the deploy."
goto = "postmortem"
# Always goes to "postmortem", never falls through to the next step
```

#### Exit and Handoff

Terminal steps end the workflow. Optionally create a HandoffPackage for cross-team routing:

```toml
[[step]]
id = "escalate"
name = "Escalate to product"
exit = true
handoff = "escalation_to_product"
task = "Package investigation findings and escalate."
```

### Step Overrides

```toml
[[step]]
id = "review"
name = "Review the PR"
task = "Run a structured code review."
skill = "review-pr"          # Inject SKILL.md content as additional context
agent = "reviewer"           # Override which agent executes this step
```

## Structured Outputs

By default, each step's full agent response is passed to subsequent steps as context (truncated to 4000 characters from the tail). For steps that produce specific data — a PR number, a branch name, a summary — you can declare **structured outputs** to pass clean, unambiguous values instead of raw prose.

### Declaring Outputs

Add `outputs` to a step definition:

```toml
[[step]]
id = "implement"
name = "Implement"
agent = "brian"
outputs = [
  { name = "pr_number", description = "PR number that was created" },
  { name = "branch_name", description = "Feature branch name" },
  { name = "summary", description = "Brief summary of changes made" },
]
task = "Implement the changes and create a PR..."
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Key name for the output value |
| `description` | No | Hint shown to the agent describing what to emit |

### How It Works

1. The engine appends instructions to the agent's task prompt:
   ```
   Before you finish, you MUST emit a structured output block with these fields:
   OUTPUTS:
   pr_number: <PR number that was created>
   branch_name: <Feature branch name>
   summary: <Brief summary of changes made>
   END_OUTPUTS
   ```

2. The agent includes the block in its response:
   ```
   OUTPUTS:
   pr_number: 42
   branch_name: feature/dw-365-activity-monitor
   summary: Added agent activity monitoring with session history tracking
   END_OUTPUTS
   ```

3. The engine parses the key-value pairs and stores them on the step record.

4. Subsequent steps see clean structured data in their "Previous steps" context:
   ```
   ### implement (outcome: continued)
   pr_number: 42
   branch_name: feature/dw-365-activity-monitor
   summary: Added agent activity monitoring with session history tracking
   ```

### Variable Access

Extracted outputs are merged into the workflow's context variables under `steps.<step_id>`:

```json
{
  "request": "Implement DW-365",
  "steps": {
    "implement": {
      "pr_number": "42",
      "branch_name": "feature/dw-365-activity-monitor",
      "summary": "Added agent activity monitoring"
    }
  }
}
```

This means subsequent steps can reference outputs directly from the `## Context` JSON block without parsing previous step prose.

### When to Use Outputs

**Use outputs** when a step produces specific data that subsequent steps need:
- PR numbers, branch names, ticket IDs
- File paths, test results, CI status
- Summaries that downstream agents should act on

**Skip outputs** when the full response is the context:
- Analysis or investigation steps where the narrative matters
- Steps where the next agent needs to see the full reasoning
- Terminal steps (exit/handoff) with no subsequent consumers

If the agent fails to emit the `OUTPUTS:` block, the engine falls back to the truncated full response — so outputs degrade gracefully.

## Webhook Triggers

Workflows can auto-start when a webhook event matches defined trigger rules. This connects external events (Linear, GitHub, GitLab, Stripe) directly to workflow execution. A trigger that fires while the workflow is at its `max_concurrent` queues the start rather than dropping it (see [Workflow Metadata](#workflow-metadata)).

### Defining Triggers

Add `[[trigger]]` sections to your workflow TOML:

```toml
[workflow]
id = "bug-fix"
name = "Bug Fix"

# Auto-start when a Linear issue is moved to "Todo" with a "bug" label
[[trigger]]
source = "linear"
event_type = "Issue.update"

[trigger.match]
"data.state.name" = "Todo"
"data.labels.name" = "bug"

# Also auto-start from GitHub issues labeled "bug"
[[trigger]]
source = "github"
event_type = "issues"

[trigger.match]
"action" = "labeled"
"label.name" = "bug"

[[step]]
# ... steps as usual
```

### Trigger Fields

| Field | Required | Description |
|-------|----------|-------------|
| `source` | Yes | Webhook source: `"github"`, `"linear"`, `"gitlab"`, `"stripe"` |
| `event_type` | Yes | Event type to match (see below) |
| `[trigger.match]` | No | Dot-path conditions against the payload (AND logic) |

### Event Types

**Linear:**
- `Issue.create`, `Issue.update`
- `Comment.create`, `Comment.update`
- `Project.create`, `Project.update`

**GitHub** (from `X-GitHub-Event` header):
- `pull_request`, `pull_request_review`
- `push`
- `issues`, `issue_comment`
- `check_run`, `check_suite`

### Match Rules

Match rules use dot-separated paths into the webhook JSON payload:

```toml
[trigger.match]
"data.state.name" = "Todo"           # payload.data.state.name == "Todo"
"action" = "labeled"                  # payload.action == "labeled"
"data.labels.name" = "bug"           # any element in payload.data.labels[].name == "bug"
```

- All rules must match (AND logic)
- If a path traverses a JSON array, any matching element is sufficient
- If `[trigger.match]` is omitted, any event with matching source + event_type triggers the workflow
- The full webhook payload becomes the workflow's context variables

### Setting Up Webhooks

Configure your external service to send webhooks to:

```
POST https://your-gateway:8091/webhooks/{source}
```

For example:
- **Linear**: Settings > Webhooks > URL: `https://host:8091/webhooks/linear`
- **GitHub**: Repo > Settings > Webhooks > URL: `https://host:8091/webhooks/github`

Signature verification is supported for GitHub (`X-Hub-Signature-256`) and Linear (`Linear-Signature`).

## Slack Thread Control

When a workflow is running in a Slack thread, you can control it by replying in the thread with these commands:

| Command | Effect |
|---------|--------|
| `stop` / `cancel` / `abort` | Immediately stops the workflow. Terminal — cannot be resumed. |
| `pause` | Pauses after the current step completes. |
| `resume` / `continue` | Resumes a paused workflow from where it left off. |
| `approved` / `yes` / `lgtm` | Resumes a paused workflow with outcome `approved`. |
| `rejected` / `no` | Resumes a paused workflow with outcome `rejected`. |
| `goto <step_id>` | Jumps to a specific step (e.g., `goto implement`). |
| `skip` | Skips the current step and advances to the next one. |

These commands work for any workflow started from a Slack team channel. The system matches the command to the workflow instance associated with the thread.

### Examples

Stop a runaway workflow:
```
stop
```

Skip the review step and go straight to merge approval:
```
skip
```

Jump back to the implement step after reviewing:
```
goto implement
```

## REST API

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/workflows` | List available workflow definitions |
| `POST` | `/api/workflows/{id}/run` | Start a workflow |
| `GET` | `/api/workflow-instances` | List instances (optional `?status=` and `?workflow=` filters) |
| `GET` | `/api/workflow-instances/{id}` | Get instance details |
| `POST` | `/api/workflow-instances/{id}/resume` | Resume a paused instance |
| `POST` | `/api/workflow-instances/{id}/abort` | Abort a running or paused instance |

### Start a Workflow

```bash
curl -X POST http://localhost:8090/api/workflows/bug-fix/run \
  -H "Content-Type: application/json" \
  -d '{
    "payload": {
      "ticket_id": "DW-100",
      "repo": "driftwerk/workforce"
    },
    "channel_id": "C0123SLACK",
    "thread_id": "1234567890.123456"
  }'
```

Response:
```json
{
  "instance_id": "550e8400-e29b-41d4-a716-446655440000",
  "workflow_id": "bug-fix",
  "status": "running"
}
```

### Resume a Paused Instance

```bash
curl -X POST http://localhost:8090/api/workflow-instances/550e8400.../resume \
  -H "Content-Type: application/json" \
  -d '{"outcome": "approved"}'
```

## Agent Tool

Agents can manage workflows via the built-in `workflow` tool. Users interact through natural language:

| Action | Example prompt |
|--------|---------------|
| List | "What workflows are available?" |
| Start | "Run the bug-fix workflow for DW-100 in driftwerk/workforce" |
| Status | "Check the status of workflow instance abc-123" |
| Resume | "Resume workflow abc-123 with outcome approved" |
| Abort | "Abort workflow instance abc-123" |
| Instances | "Show me all paused workflow instances" |

## CLI Commands

```bash
# List available definitions
workforce workflow list

# Validate a TOML file
workforce workflow validate .driftwerk/workflows/bug-fix.toml

# Show definition details
workforce workflow show bug-fix

# Scaffold a new workflow
workforce workflow create my-workflow

# List instances (with optional filters)
workforce workflow instances
workforce workflow instances --status paused
workforce workflow instances --workflow bug-fix

# Check instance status
workforce workflow status <instance-id>

# Push/pull workflows to/from the server
workforce workflow push --gateway https://host:8091 --token $TOKEN
workforce workflow pull --gateway https://host:8091 --token $TOKEN
```

## Hot Reload

The hub watches the workflows directory for changes. When you add, edit, or delete a `.toml` file, definitions are automatically reloaded within a few seconds. No restart required.

## Starter Templates

Four templates are included in `.driftwerk/workflows/`:

### bug-fix.toml
Investigate > implement > PR > review > merge, with escalation branches and a webhook trigger for Linear issues labeled "bug".

### feature-impl.toml
Ticket pickup > design review (pause) > implement > PR > review (pause) > merge, with escalation for unclear tickets.

### pr-review.toml
Automated review > feedback > re-review cycle, using the `review-pr` skill. Handles author abandonment and gate failures.

### incident-response.toml
Alert assessment > diagnosis > fix/rollback/config-fix > deploy > postmortem, with SEV1 paging via handoff.

## Architecture

```
                    ┌─────────────┐
                    │  Webhook    │  POST /webhooks/{source}
                    │  (Linear,   │──────────────────────────┐
                    │   GitHub)   │                          │
                    └─────────────┘                          ▼
                                                   ┌─────────────────┐
┌──────────┐   ┌──────────────┐                   │ Trigger Matching │
│  Slack   │──▶│ Agent calls   │                   │ (source +        │
│  / API   │   │ workflow tool │                   │  event_type +    │
└──────────┘   └──────┬───────┘                   │  match rules)    │
                      │                            └────────┬────────┘
                      ▼                                     │
              ┌───────────────┐◀────────────────────────────┘
              │ WorkflowRuntime│
              │               │
              │  load definition
              │  create instance
              │  execute_loop:
              │    prepare_step
              │    → AgentService.process_message()
              │    complete_step
              │    → route via branches
              │    save to SQLite
              │    deliver status to Slack
              └───────────────┘
```

- **WorkflowDefinition**: TOML parsing, validation, step/branch/trigger structs
- **WorkflowInstance**: Runtime state (persisted to SQLite)
- **WorkflowExecutor**: Stateless engine — takes (definition + instance), returns (updated instance + transition)
- **WorkflowRuntime**: Hub integration — holds `Arc<AgentService>`, executes steps, manages persistence
- **WorkflowTool**: Agent-callable tool delegating to the runtime
- **Webhook triggers**: Checked on every inbound webhook, auto-start matching workflows
- **FsWatcher**: Hot-reloads definitions when TOML files change on disk
