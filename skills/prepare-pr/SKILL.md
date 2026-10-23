---
name: prepare-pr
version: 1.0.0
description: Rebase, fix review findings, run gates, and push
author: driftwerk
requires:
  bins: [git]
tools:
  - github.get_pull
  - github.pr_diff
  - github.pr_comment
  - github.get_file
  - exec.run
  - jobs
---

# Prepare PR

Rebase a pull request branch, address review findings, run quality gates, and push the result.

> **Prerequisite:** enable the GitHub skill first — run `system.enable_skill('github')`. This workflow's steps call `github.*` tools, which aren't loaded until then.

## Constraints

- Always rebase onto the base branch before running gates.
- Never force push without `--force-with-lease`.
- If rebase produces conflicts, **stop immediately** and report the conflicting files. Do not attempt to resolve merge conflicts automatically.
- Commit fixes with clear, descriptive messages (e.g. `fix: address review finding F1 — handle error in parse_config`). Use `exec.run` for all git add/commit operations.
- If a gate command fails and cannot be fixed, **stop** and report the failure. Do not push broken code.

## Input

The invoker provides:

- `pr_number` — the pull request number (required)
- `owner/repo` — the repository in `owner/repo` format (required)

## Procedure

### Step 1 — Read review artifact

Check for a review artifact from a prior `review-pr` run:

```
exec.run: cat .local/reviews/pr-{number}.json
```

If the file exists, parse it and extract findings (especially BLOCKER and IMPORTANT severity). These are the findings to address in Step 6.

If the file does not exist, proceed without prior findings. The preparation will focus on rebasing and running gates.

### Step 2 — Fetch PR metadata

```
github.get_pull: owner=<owner> repo=<repo> pull_number=<pr_number>
```

Extract: head branch, base branch, head SHA, clone URL.

### Step 3 — Read workflow config

Attempt to read `.driftwerk/pr-workflow.toml` from the base branch:

```
github.get_file: owner=<owner> repo=<repo> path=.driftwerk/pr-workflow.toml ref=<base_branch>
```

If the file exists, parse it for:
- `prepare.gate_commands` — list of commands to run as quality gates (e.g. `["cargo fmt -- --check", "cargo clippy ...", "cargo test"]`)
- `prepare.format_command` — optional auto-fix command to run when a formatting gate fails (e.g. `"cargo fmt"`)

If the file does not exist, infer gate commands from project type by checking for marker files (fetch each via `github.get_file` or check the repo contents):

| Marker File | Gate Commands | Format Command |
|-------------|---------------|----------------|
| `Cargo.toml` | `cargo fmt -- --check`, `cargo clippy --all-targets --all-features --no-deps -- -D warnings`, `cargo test --workspace` | `cargo fmt` |
| `package.json` (with `pnpm-lock.yaml`) | `pnpm test` | — |
| `package.json` (without pnpm lock) | `npm test` | — |
| `pyproject.toml` | `pytest` | — |

If no marker file is found, ask the user what gate commands to run before proceeding.

### Step 4 — Clone and checkout

Set up the working tree. If the repo is not already cloned into the workspace, submit a background job to clone it, wait for completion, then fetch and checkout the PR head branch:

```
jobs: action=exec command="git clone <clone_url> ." label="clone repo"
```

Wait for the clone job to complete:

```
jobs: action=result job_id=<job_id>
```

If the clone succeeded (or if the workspace already contains a clone — check for `.git` directory via `exec.run: test -d .git`), fetch and checkout:

```
jobs: action=exec command="git fetch origin && git checkout <head_branch> && git pull origin <head_branch> && git fetch origin <base_branch>" label="fetch and checkout"
```

Wait for the fetch job to complete:

```
jobs: action=result job_id=<job_id>
```

If any job fails, report the error and **STOP**.

### Step 5 — Rebase onto base branch

Submit a background rebase job:

```
jobs: action=exec command="git rebase origin/<base_branch>" label="rebase"
```

Wait for the job to complete:

```
jobs: action=result job_id=<job_id>
```

Inspect the exit code and output from the result:

- If the rebase **succeeded** (exit code 0), continue to Step 6.
- If the rebase **failed** (non-zero exit code), it likely produced conflicts:
  1. Run `exec.run: git rebase --abort` to clean up.
  2. Report the conflicting files to the user (parse the output for `CONFLICT` lines).
  3. **STOP.** Do not proceed to further steps.

### Step 6 — Address review findings

If a review artifact was loaded in Step 1 and it contains BLOCKER or IMPORTANT findings:

For each finding:
1. Read the relevant file in the workspace via `exec.run: cat <file_path>`.
2. Determine the appropriate fix based on the finding description and suggested fix.
3. Apply the fix by editing the file via `exec.run` (use `sed`, a heredoc write, or another appropriate method).
4. Stage and commit the fix:
   ```
   exec.run: git add <file_path>
   exec.run: git commit -m "fix: address review finding <ID> — <short description>"
   ```

Track which finding IDs were addressed.

NITs are optional. Address them if the fix is trivial and low-risk, but do not spend time on subjective style changes.

### Step 7 — Run gate commands

Submit a background jobs chain to run all gate commands as a DAG pipeline with streaming output:

```
jobs: action=chain label="Quality gates" steps=[
  {
    "id": "fmt",
    "command": "<fmt_check_command>",
    "label": "Format check",
    "stream": true
  },
  {
    "id": "clippy",
    "command": "<clippy_command>",
    "label": "Clippy",
    "depends_on": ["fmt"],
    "stream": true
  },
  {
    "id": "test",
    "command": "<test_command>",
    "label": "Tests",
    "depends_on": ["clippy"],
    "timeout_secs": 600,
    "stream": true,
    "retry": {"max_attempts": 2, "backoff": "exponential", "retry_on": ["non_zero_exit"]}
  }
]
```

For a Rust project the commands are:
- `fmt`: `cargo fmt -- --check`
- `clippy`: `cargo clippy --all-targets --all-features --no-deps -- -D warnings`
- `test`: `cargo test --workspace`

Monitor the chain status:

```
jobs: action=chain_status chain_id=<chain_id>
```

**Handling failures:**

1. If the `fmt` step fails and a `prepare.format_command` is configured:
   - Run the format command via a background job to auto-fix formatting:
     ```
     jobs: action=exec command="<format_command>" label="auto-format"
     ```
   - Wait for it to complete via `jobs: action=result job_id=<job_id>`.
   - If it succeeded, stage and commit the formatting changes:
     ```
     exec.run: git add -A
     exec.run: git commit -m "style: apply auto-formatting"
     ```
   - Re-submit the full chain (Step 7) and wait again.
   - If the fmt step still fails, **STOP** and report the failure.

2. If `clippy` or `test` fails:
   - Retrieve the step output: `jobs: action=result job_id=<step_job_id>`
   - Analyse the failure, fix the code issues via `exec.run`, commit the fix, and re-submit the chain.
   - Repeat until all gates pass.
   - If a failure cannot be fixed, **STOP** and report the failure with full output.

Do not push code until ALL gates pass.

### Step 8 — Push

Submit a background push job:

```
jobs: action=exec command="git push origin <head_branch> --force-with-lease" label="push"
```

Wait for the job to complete:

```
jobs: action=result job_id=<job_id>
```

If the push job fails (e.g. due to a concurrent update), report the error and **STOP**. Do not retry with `--force`.

Capture the new head SHA after a successful push:

```
exec.run: git rev-parse HEAD
```

### Step 9 — Write preparation artifact

Create the artifact directory and write the preparation artifact:

```
exec.run: mkdir -p .local/preparations
```

Write the following JSON to `.local/preparations/pr-{number}.json`:

```json
{
  "pr_number": 123,
  "repo": "owner/repo",
  "head_sha": "<new SHA after rebase and fixes>",
  "base_branch": "main",
  "gates_passed": true,
  "findings_addressed": ["F1", "F2"],
  "prepared_at": "2024-01-01T00:00:00Z"
}
```

The `prepared_at` field should be the current UTC timestamp.

### Step 10 — Comment on PR

Post a summary comment on the pull request:

```
github.pr_comment: owner=<owner> repo=<repo> pull_number=<pr_number>
```

The comment body should include:

```
**Preparation complete** (prepare-pr)

- Rebased onto `origin/<base_branch>`
- Findings addressed: <list of finding IDs, or "None">
- Gates passed: <yes/no>
- New head SHA: `<sha>`
```

Present the preparation summary to the user, including the list of findings addressed, gate results, and the new head SHA.
