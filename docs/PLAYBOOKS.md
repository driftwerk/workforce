# Playbooks

Playbooks are **interactive, human-in-the-loop flows** — the sibling to
[workflows](WORKFLOWS.md). Where a workflow runs autonomously (a fresh agent per
step, handing context forward as variables), a playbook runs **in your own chat
session**: the agent injects a scripted prompt for each step, you chat with it
freely until you're happy, then you advance to the next step. The whole
conversation stays present the entire time.

Use a playbook when you want to *drive* the agent through a multi-step task
(design → implement → review, triage → diagnose → fix) and stay in control at
each step — not hand the whole thing off.

## Running a playbook (Slack)

Everything is driven by `cmd:` commands (which work in threads, unlike Slack
slash commands) or the always-visible buttons:

| Action | Command | Button |
|---|---|---|
| Start | `cmd: playbook start <id>` | — |
| List available | `cmd: playbook list` | — |
| Show progress | `cmd: playbook status` | — |
| Advance to next step | `cmd: continue` | **▶ Continue** |
| Stop (keep the conversation) | `cmd: exit` | **Exit** |

`/continue`, `/exit`, `/playbook …` also work. A bare word ("continue with that
approach") is **never** treated as a command — only the explicit prefix or the
button advances a step.

Flow:

1. `cmd: playbook start design-impl-review` — the agent runs step 1 and posts a
   `▶ Step 1/3` divider.
2. Chat with it as much as you like — refine the design, ask questions.
3. Click **▶ Continue** (or `cmd: continue`) — it advances to step 2, carrying
   everything so far.
4. Repeat. After the last step's Continue, the playbook completes.
5. **Exit** at any point leaves your conversation intact — you just drop the
   step overlay and keep talking normally.

## Artifacts — surviving compaction

A real implementation step reads files, edits code, and runs tools — enough to
push the session past its rollover/compaction threshold, at which point the raw
discussion is summarised away. So a playbook step can't rely on scrollback to
carry its output to a later step.

If a step declares `artifact = "design.md"`, two things happen:

- An instruction to **write that file** is auto-appended to the step's prompt.
- On Continue, the file is **pinned**: its path + a synopsis are recorded, later
  steps are told to read it, and the synopsis is kept in the agent's system
  prompt every turn.

So the substance survives even after the conversation compacts. Artifacts live
in a per-session scratch dir under the agent's workspace; the agent just writes
to the relative path the prompt gives it.

> Artifact pinning is best-effort: it depends on the agent actually writing the
> file. If a step's file is missing at Continue, the next step is still told
> where it was expected, marked `(not written)`.

## Authoring playbooks

### TOML format

Playbooks live in `.driftwerk/playbooks/<id>.toml`:

```toml
[playbook]
id = "design-impl-review"          # unique, kebab-case; must match the filename
name = "Design → Implement → Review"
description = "Human-in-the-loop feature build."

[[step]]
id = "design"                      # unique within the playbook
title = "Design the implementation" # short label shown on the ▶ Step N divider
artifact = "design.md"             # optional: a file this step should produce
prompt = """
You are designing a code implementation. Ask the user what they need, investigate
the codebase, and produce a plan. Talk it through until they Continue.
"""

[[step]]
id = "implement"
title = "Implement the design"
prompt = "Implement the design (read design.md). ..."
```

**Fields**

- `[playbook]`: `id` (required, must equal the filename), `name` (required),
  `description` (optional).
- `[[step]]` (≥1, in order): `id` (required, unique), `title` (required, short),
  `prompt` (required), `artifact` (optional filename).

**Validation rules**: at least one step; unique step ids; non-empty prompts; the
`[playbook].id` must match the id it's saved under.

Definitions **hot-reload** — drop a `.toml` in `.driftwerk/playbooks/` and it's
picked up without a restart.

### By the agent (`playbook` tool)

The agent can author playbooks for you. Ask it to "create a playbook that …"
and it uses the `playbook` tool:

| Action | Purpose |
|---|---|
| `schema` | The authoring guide (format + rules) |
| `validate` | Check a TOML draft |
| `save` | Validate, write to disk, hot-reload |
| `list` / `get` | Inspect existing playbooks |
| `delete` | Remove a definition |

(There is no tool `start`/`stop` — playbooks are user-driven via `cmd:`.)

### CLI

```bash
# Local definition files
workforce playbook list                         # list definitions in .driftwerk/playbooks
workforce playbook validate path/to/file.toml   # validate a TOML file
workforce playbook show <id>                     # show a definition's steps
workforce playbook create <id>                   # scaffold a new .toml

# Sync with a running hub (--gateway/--token, or WORKFORCE_GATEWAY_URL/TOKEN env)
workforce playbook list   --gateway <url> --token <tok>   # list remote
workforce playbook show <id> --gateway <url> --token <tok>
workforce playbook pull   [ids...] --gateway <url> --token <tok>   # download (all if no ids)
workforce playbook push   [paths...] --gateway <url> --token <tok> # upload (all in dir if none)
workforce playbook delete <id> --gateway <url> --token <tok>
```

`--playbooks-dir` overrides the directory (default `.driftwerk/playbooks`).

### REST (admin-scoped)

The hub's gateway exposes definition CRUD, mirroring `/api/workflows/*`:

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/playbooks/export` | all definitions as `{id: toml}` |
| GET | `/api/playbooks/{id}/source` | one definition's raw TOML |
| PUT | `/api/playbooks/{id}` | save (body = TOML) |
| DELETE | `/api/playbooks/{id}` | delete |

## Playbooks vs workflows

| | Playbook | Workflow |
|---|---|---|
| Execution | one persistent chat session | fresh agent per step |
| Context handoff | full conversation + pinned artifacts | declared output variables |
| Control | you advance each step (Continue/Exit) | autonomous (+ optional pause gates) |
| Driven from | Slack `cmd:` / buttons | trigger / `workflow` tool / schedule |
| Best for | guided, interactive, you-in-the-loop work | hands-off multi-step automation |

The two compose: a workflow step could hand off to a playbook for a
human-in-the-loop stretch, and vice versa.
