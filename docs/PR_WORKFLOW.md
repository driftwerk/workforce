# PR Workflow

The PR workflow is a set of three chained skills that automate the pull request lifecycle: **review**, **prepare**, and **merge**. Each skill is a SKILL.md file containing step-by-step instructions the agent follows using the built-in GitHub tools.

## Overview

```
PR opened / user request
        │
        ▼
┌──────────────┐     writes      ┌──────────────────────────────┐
│  review-pr   │ ──────────────► │ .local/reviews/pr-{N}.json   │
└──────────────┘                 └──────────────────────────────┘
        │                                      │
        ▼                               reads  │
┌──────────────┐     writes      ┌─────────────▼────────────────┐
│  prepare-pr  │ ──────────────► │ .local/preparations/pr-{N}.json │
└──────────────┘                 └──────────────────────────────┘
        │                                      │
        ▼                               reads  │
┌──────────────┐     writes      ┌─────────────▼────────────────┐
│  merge-pr    │ ──────────────► │ .local/merges/pr-{N}.json    │
└──────────────┘                 └──────────────────────────────┘
```

The skills can be run as a full pipeline or individually. `merge-pr` works standalone without prior review or preparation — it falls back to live CI checks.

## Skills

### review-pr

**Purpose:** Read-only code review. Fetches the PR diff, checks CI, and produces a structured review with findings.

**What it does:**
1. Fetches PR metadata and diff in parallel
2. Reads `.driftwerk/pr-workflow.toml` from the repo for sensitive path configuration
3. Fetches full file content for sensitive or ambiguous files
4. Checks CI status; if failed, fetches workflow logs for context
5. Produces a structured review: summary, findings table (BLOCKER / IMPORTANT / NIT), security analysis, test coverage assessment, recommendation (APPROVE / REQUEST_CHANGES / NEEDS_DISCUSSION)
6. Writes artifact to `.local/reviews/pr-{N}.json`

**Tools used:** `github.get_pull`, `github.pr_diff`, `github.get_file`, `github.ci_status`, `github.workflow_runs`, `github.workflow_run_failed_logs`, `exec.run`

### prepare-pr

**Purpose:** Fix review findings, rebase, run quality gates, and push.

**What it does:**
1. Reads the review artifact from `review-pr`
2. Reads `.driftwerk/pr-workflow.toml` for gate commands (or auto-detects from project type)
3. Clones/fetches the repo and checks out the PR branch
4. Rebases onto the base branch (aborts on conflicts)
5. Addresses BLOCKER and IMPORTANT findings from the review
6. Runs gate commands; auto-fixes formatting failures if a format command is configured
7. Pushes with `--force-with-lease`
8. Writes artifact to `.local/preparations/pr-{N}.json`
9. Posts a summary comment on the PR

**Tools used:** `github.get_pull`, `github.pr_diff`, `github.ci_status`, `github.pr_comment`, `github.get_file`, `exec.run`

### merge-pr

**Purpose:** Safety-checked merge with SHA pinning.

**What it does:**
1. Reads the preparation artifact (if available)
2. Verifies PR is open, not draft, and head SHA matches preparation artifact
3. Checks CI — waits up to 2 minutes for pending checks, blocks on failures
4. Reads merge config from `.driftwerk/pr-workflow.toml` (method, branch deletion)
5. Executes SHA-pinned merge to prevent race conditions
6. Deletes head branch (if configured)
7. Posts a confirmation comment
8. Writes artifact to `.local/merges/pr-{N}.json`

**Tools used:** `github.get_pull`, `github.ci_status`, `github.pr_merge`, `github.pr_comment`, `github.branch_delete`, `github.get_file`, `exec.run`

## Repository Configuration

Each repository can include a `.driftwerk/pr-workflow.toml` file to customize the workflow behavior. The agent fetches this file from the base branch during each skill's execution.

### Full Schema

```toml
[review]
# Glob patterns for files that should always be fetched in full during review.
# Use this for security-sensitive or configuration files where diff context
# alone is insufficient.
sensitive_paths = [
    "src/auth/**",
    "migrations/**",
    "*.toml",
    ".github/**",
]

[prepare]
# Commands to run as quality gates. All must pass before pushing.
gate_commands = [
    "cargo fmt -- --check",
    "cargo clippy --all-targets --all-features --no-deps -- -D warnings",
    "cargo test --workspace",
]

# Auto-fix command to run when a formatting gate fails.
# If set, the agent runs this, then re-checks the gate.
format_command = "cargo fmt"

[merge]
# Merge method: "squash" (default), "merge", or "rebase"
method = "squash"

# Whether to delete the head branch after merge. Default: true
delete_branch = true
```

### Example: Rust Project

```toml
# .driftwerk/pr-workflow.toml

[review]
sensitive_paths = ["Cargo.toml", "Cargo.lock", "migrations/**", "src/auth/**"]

[prepare]
gate_commands = [
    "cargo fmt -- --check",
    "cargo clippy --all-targets --all-features --no-deps -- -D warnings",
    "cargo test --workspace",
]
format_command = "cargo fmt"

[merge]
method = "squash"
delete_branch = true
```

### Example: Node.js Project (pnpm)

```toml
# .driftwerk/pr-workflow.toml

[review]
sensitive_paths = ["package.json", "pnpm-lock.yaml", ".env.example"]

[prepare]
gate_commands = [
    "pnpm lint",
    "pnpm test",
    "pnpm build",
]
format_command = "pnpm lint --fix"

[merge]
method = "squash"
delete_branch = true
```

### Example: Python Project

```toml
# .driftwerk/pr-workflow.toml

[review]
sensitive_paths = ["pyproject.toml", "requirements*.txt", "alembic/**"]

[prepare]
gate_commands = [
    "ruff check .",
    "mypy src/",
    "pytest",
]
format_command = "ruff format ."

[merge]
method = "squash"
delete_branch = true
```

### Defaults (No Config File)

When `.driftwerk/pr-workflow.toml` is absent, the agent auto-detects the project type by checking for marker files in the repository:

| Marker File | Gate Commands | Format Command |
|-------------|---------------|----------------|
| `Cargo.toml` | `cargo fmt -- --check`, `cargo clippy ...`, `cargo test --workspace` | `cargo fmt` |
| `package.json` + `pnpm-lock.yaml` | `pnpm test` | — |
| `package.json` (no pnpm) | `npm test` | — |
| `pyproject.toml` | `pytest` | — |

If no marker file is found, the agent asks the user what gate commands to run.

The default merge method is `squash` and the default for branch deletion is `true`.

## Usage Examples

### Triggering via Direct Request

Ask the agent to review, prepare, or merge a PR directly:

```
You:   "Review PR #42 on acme/backend"
Agent: [follows review-pr procedure]
       - Fetches PR metadata and diff
       - Checks CI status
       - Produces structured review with findings
       - Writes artifact to .local/reviews/pr-42.json
       - Presents the review

You:   "Prepare PR #42 on acme/backend"
Agent: [follows prepare-pr procedure]
       - Reads review artifact
       - Rebases onto main
       - Fixes BLOCKER/IMPORTANT findings
       - Runs gate commands
       - Pushes with --force-with-lease
       - Comments on the PR

You:   "Merge PR #42 on acme/backend"
Agent: [follows merge-pr procedure]
       - Reads preparation artifact
       - Verifies CI passes
       - Executes SHA-pinned merge
       - Deletes branch
       - Comments on the PR
```

### Full Pipeline

```
You:   "Run the full PR workflow for PR #42 on acme/backend"
Agent: [runs review-pr, then prepare-pr, then merge-pr in sequence]
```

### Standalone Merge

`merge-pr` works without prior review or preparation:

```
You:   "Merge PR #42 on acme/backend"
Agent: [skips artifact checks, verifies CI directly, merges]
```

### Triggering via GitHub Webhook

When a GitHub webhook is configured, the agent receives PR events automatically:

```
[Webhook delivers: PR #42 opened on acme/backend]
Agent: "I received a notification that PR #42 was opened on acme/backend.
        Title: Add user authentication
        Branch: feature/auth → main
        Author: alice

        Would you like me to review this PR?"
```

The agent can then be instructed to proceed with the workflow.

## Webhook Setup

To receive GitHub events automatically, configure a webhook on your GitHub repository:

1. Go to your repository **Settings → Webhooks → Add webhook**

2. Set the payload URL to your workforce gateway:
   ```
   https://<your-host>/webhooks/github
   ```

3. Set content type to `application/json`

4. Select events to trigger:
   - Pull requests
   - Check runs / Check suites (for CI status updates)
   - Push events (optional)

5. Add a webhook secret for signature verification

The webhook handler verifies GitHub's `X-Hub-Signature-256` HMAC-SHA256 signature and formats the event into a structured message for the agent.

## Artifact Formats

### Review Artifact (`.local/reviews/pr-{N}.json`)

```json
{
  "pr_number": 42,
  "repo": "acme/backend",
  "recommendation": "REQUEST_CHANGES",
  "findings": [
    {
      "id": "F1",
      "severity": "BLOCKER",
      "file": "src/main.rs",
      "title": "Unwrap on user input can panic",
      "fix": "Use map_err and return a 400"
    },
    {
      "id": "F2",
      "severity": "NIT",
      "file": "src/lib.rs",
      "title": "Unused import",
      "fix": "Remove use std::io"
    }
  ],
  "ci_status": "pass",
  "ci_failures": [],
  "reviewed_at": "2026-02-14T10:30:00Z"
}
```

### Preparation Artifact (`.local/preparations/pr-{N}.json`)

```json
{
  "pr_number": 42,
  "repo": "acme/backend",
  "head_sha": "abc123def456",
  "base_branch": "main",
  "gates_passed": true,
  "findings_addressed": ["F1"],
  "prepared_at": "2026-02-14T10:35:00Z"
}
```

### Merge Artifact (`.local/merges/pr-{N}.json`)

```json
{
  "pr_number": 42,
  "repo": "acme/backend",
  "merge_sha": "789def012abc",
  "merge_method": "squash",
  "base_branch": "main",
  "head_branch": "feature/auth",
  "delete_branch": true,
  "merged_at": "2026-02-14T10:40:00Z"
}
```

## Prerequisites

The PR workflow requires:

1. **GitHub skill enabled** — The agent needs a GitHub token with repo access:
   ```
   vault.set("skills/github/token", "ghp_your_token_here")
   system.enable_skill("github")
   ```
   Or set `GITHUB_TOKEN` in the environment.

2. **git binary** — Required by all three skills (declared in `requires.bins`). Included in the default Docker image.

3. **exec.run tool** — Used for git operations and file writes in the agent workspace. Available by default.

## Skill Files

The skill definitions are located in the workspace:

| Skill | Path |
|-------|------|
| review-pr | `workforce/skills/review-pr/SKILL.md` |
| prepare-pr | `workforce/skills/prepare-pr/SKILL.md` |
| merge-pr | `workforce/skills/merge-pr/SKILL.md` |

These are bundled into the Docker image at `/app/bundled-skills/skills/` and scanned at startup. The agent receives the full skill instructions in its system prompt.

## Customization

### Modifying a Skill

Edit the SKILL.md file in the `workforce/skills/` directory. The markdown body contains the step-by-step procedure the agent follows. Changes take effect after rebuilding and redeploying.

### Adding a New Skill

Create a new directory under `workforce/skills/` with a `SKILL.md` file:

```
workforce/skills/my-skill/
  SKILL.md
```

The SKILL.md must have YAML frontmatter:

```markdown
---
name: my-skill
version: 1.0.0
description: What this skill does
author: your-org
requires:
  bins: [git]
tools:
  - github.get_pull
  - exec.run
---

# My Skill

Step-by-step instructions for the agent...
```

The skill will be discovered at startup and its instructions injected into the agent's system prompt.

### Installing External Skills at Runtime

Skills can also be installed from git repositories without rebuilding:

```
system.install_external_skill({ github_repo: "acme/my-skill" })
```

`github_repo` (`owner/repo@ref#subpath=dir`) reads a private GitHub repository with the caller's GitHub token, and `source` takes a skill directory in the caller's workspace; another git URL, or a directory elsewhere on the hub, needs `skills_manage`. A skill a member installs is theirs and private until they share it. A WASM skill (`Skill.toml`) installs with `wasm_skill.install` instead; see [Choosing an install tool](SKILLS.md#choosing-an-install-tool).

These are installed to `{data_dir}/skills/` and take priority over bundled skills with the same name.

## Related Documentation

- [SKILLS.md](SKILLS.md) — Skill system overview, built-in skills, external skills, system tools
- [VAULT.md](VAULT.md) — Credential storage for GitHub tokens
- [TOOLS.md](TOOLS.md) — Built-in tool reference including GitHub tools
- [GATEWAY_PROTOCOL.md](GATEWAY_PROTOCOL.md) — Gateway API for webhook integration
