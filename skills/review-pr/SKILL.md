---
name: review-pr
version: 1.0.0
description: Structured code review for pull requests
author: driftwerk
requires:
  bins: [git]
tools:
  - github.get_pull
  - github.pr_diff
  - github.get_file
  - github.workflow_runs
  - github.workflow_run_failed_logs
  - exec.run
  - jobs
---

# Review PR

Perform a structured, read-only code review for a GitHub pull request.

> **Prerequisite:** enable the GitHub skill first — run `system.enable_skill('github')`. This workflow's steps call `github.*` tools, which aren't loaded until then.

## Constraints

- This skill is **READ-ONLY**. Never push, merge, comment, or modify code.
- Prefer batch metadata fetching. Use a single `github.get_pull` call to retrieve PR metadata rather than multiple calls.
- Only fetch full file contents (via `github.get_file`) for files flagged as sensitive or where the diff context is insufficient to understand the change. Do not fetch every changed file in full.
- Finding severity definitions:
  - **BLOCKER** — Must be fixed before merge. Bugs, security vulnerabilities, data loss risks, broken builds.
  - **IMPORTANT** — Should be fixed before merge. Logic issues, missing error handling, poor patterns that will cause future problems.
  - **NIT** — Optional. Style preferences, naming suggestions, minor readability improvements.

## Input

The invoker provides:

- `pr_number` — the pull request number (required)
- `owner/repo` — the repository in `owner/repo` format (required)

## Procedure

### Step 1 — Fetch PR metadata and diff

Fetch the pull request metadata and diff in parallel:

```
github.get_pull:  owner=<owner> repo=<repo> pull_number=<pr_number>
github.pr_diff:   owner=<owner> repo=<repo> pull_number=<pr_number>
```

Extract from metadata: title, body, author, base branch, head branch, head SHA, labels, file count, additions, deletions.

### Step 2 — Check for repository workflow config

Attempt to read `.driftwerk/pr-workflow.toml` from the base branch:

```
github.get_file: owner=<owner> repo=<repo> path=.driftwerk/pr-workflow.toml ref=<base_branch>
```

If the file exists, parse it and read the `review.sensitive_paths` config. This is a list of glob patterns (e.g. `["src/auth/**", "migrations/**", "*.toml"]`) identifying files whose full content should always be fetched for review context.

If the file does not exist, proceed without sensitive path configuration.

### Step 3 — Fetch full content for sensitive or ambiguous files

Compare the list of changed files from the diff against `review.sensitive_paths` (if configured). For any matching files, and for any files where the diff context alone is insufficient to understand the change, fetch the full file content:

```
github.get_file: owner=<owner> repo=<repo> path=<file_path> ref=<head_branch>
```

Keep these fetches to a minimum. The diff is the primary review input.

### Step 4 — Check CI status

Submit a background CI monitor job and wait for it to complete:

```
jobs: action=watch_ci owner=<owner> repo=<repo> ref=<head_sha> label="CI status"
```

When the job completes, retrieve the result:

```
jobs: action=result job_id=<job_id>
```

Parse the result to determine the overall CI status: `pass`, `fail`, or `pending`.

If any checks have failed, fetch the workflow runs and failed logs to understand the failure:

```
github.workflow_runs: owner=<owner> repo=<repo> branch=<head_branch>
github.workflow_run_failed_logs: owner=<owner> repo=<repo> run_id=<failed_run_id>
```

Record the overall CI status (`pass`, `fail`, `pending`) and any failure details.

### Step 5 — Analyze and produce structured review

Review the diff, file contents, CI status, and PR metadata. Produce a structured review with the following sections:

#### A) Summary

1-3 sentence TL;DR of what this PR does and whether it is ready to merge. Include a clear recommendation up front.

#### B) What changed

Describe the changes at a high level. Group related changes together. Call out what is done well — good patterns, thorough error handling, clean abstractions.

#### C) Security

Identify any security concerns: credential exposure, injection risks, authorization gaps, unsafe deserialization, etc. If none are found, state "None identified."

#### D) Findings

Present findings as a table:

| ID | Severity | File | Description | Suggested Fix |
|----|----------|------|-------------|---------------|
| F1 | BLOCKER | src/main.rs | Unwrap on user input can panic | Use `map_err` and return a 400 |
| F2 | NIT | src/lib.rs | Unused import | Remove `use std::io` |

Assign sequential IDs starting from F1. If there are no findings, state "No findings."

#### E) Tests

Assess test coverage for the changes. Note:
- Are new code paths tested?
- Are edge cases covered?
- Are there any obvious gaps?
- If no tests exist for the changed code, flag this.

#### F) CI Status

Report the CI status: pass, fail, or pending. If failed, summarize what failed and why based on the logs fetched in Step 4.

#### G) Recommendation

Provide exactly one of:
- **APPROVE** — No blockers, code is ready to merge (may have NITs).
- **REQUEST_CHANGES** — Has BLOCKER or multiple IMPORTANT findings that must be addressed.
- **NEEDS_DISCUSSION** — Architectural or design questions that need human input before proceeding.

### Step 6 — Write review artifact

Write the structured review as a JSON artifact to the workspace using `exec.run`:

```
exec.run: mkdir -p .local/reviews
```

Write the following JSON to `.local/reviews/pr-{number}.json`:

```json
{
  "pr_number": 123,
  "repo": "owner/repo",
  "recommendation": "APPROVE",
  "findings": [
    {
      "id": "F1",
      "severity": "IMPORTANT",
      "file": "src/main.rs",
      "title": "Unwrap on user input can panic",
      "fix": "Use map_err and return a 400"
    }
  ],
  "ci_status": "pass",
  "ci_failures": [],
  "reviewed_at": "2024-01-01T00:00:00Z"
}
```

Use `exec.run` to write this file (e.g. via a heredoc or echo command). The `reviewed_at` field should be the current UTC timestamp.

### Step 7 — Output results

Present the full structured review (sections A through G) to the user. End with the recommendation and the path to the saved artifact.
