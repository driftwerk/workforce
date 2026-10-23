---
name: merge-pr
version: 1.0.0
description: Deterministic merge with safety checks
author: driftwerk
requires:
  bins: [git]
tools:
  - github.get_pull
  - github.pr_merge
  - github.pr_comment
  - github.branch_delete
  - github.get_file
  - exec.run
  - jobs
---

# Merge PR

Merge a pull request with deterministic safety checks and SHA-pinned merge execution.

> **Prerequisite:** enable the GitHub skill first — run `system.enable_skill('github')`. This workflow's steps call `github.*` tools, which aren't loaded until then.

## Constraints

- **NEVER** merge if any CI check is in a `failure` state.
- **NEVER** merge draft PRs.
- Always use the `sha` parameter when calling `github.pr_merge` to ensure the merge targets the exact commit that was validated. This prevents race conditions where new commits are pushed between validation and merge.
- If the merge API call fails, report the error to the user. Do **NOT** retry automatically — the failure may indicate a concurrent update, protected branch rule, or other condition that requires human judgment.
- This skill can be used standalone (without prior `review-pr` or `prepare-pr` runs). When used standalone, skip artifact validation steps and proceed directly to safety checks.

## Input

The invoker provides:

- `pr_number` — the pull request number (required)
- `owner/repo` — the repository in `owner/repo` format (required)

## Procedure

### Step 1 — Read preparation artifact

Check for a preparation artifact from a prior `prepare-pr` run:

```
exec.run: cat .local/preparations/pr-{number}.json
```

If the file exists, parse it and extract:
- `head_sha` — the expected head SHA after preparation
- `gates_passed` — whether all gates passed
- `findings_addressed` — which review findings were fixed

If the file does not exist, proceed without artifact validation. The merge will rely solely on live CI status and PR state checks.

### Step 2 — Fetch PR metadata and verify state

```
github.get_pull: owner=<owner> repo=<repo> pull_number=<pr_number>
```

Verify the following conditions. If any fail, **STOP** and report the reason:

1. **PR state is `open`** — cannot merge a closed or already-merged PR.
2. **PR is not a draft** — draft PRs are not ready for merge.
3. **SHA consistency** (only if prep artifact exists) — the current head SHA from `github.get_pull` must match the `head_sha` recorded in the preparation artifact. If they differ, it means new commits were pushed after preparation, and the PR needs to be re-prepared. **STOP** and report the mismatch.

Extract from metadata: title, body, author login, head SHA, head branch, base branch, number.

### Step 3 — Check CI status

Submit a background CI monitor job to wait for all checks to complete:

```
jobs: action=watch_ci owner=<owner> repo=<repo> ref=<head_sha> label="CI status"
```

When the job completes, retrieve the result:

```
jobs: action=result job_id=<job_id>
```

Evaluate the CI outcome from the result:

- If **all checks** passed (`success` or `neutral`) — proceed to merge.
- If **any check** is `failure` — **STOP** and report the failing checks. Do not merge.
- If the job timed out or returned `pending` — **STOP** and report that CI has not completed.

### Step 4 — Read merge config

Attempt to read `.driftwerk/pr-workflow.toml` from the base branch:

```
github.get_file: owner=<owner> repo=<repo> path=.driftwerk/pr-workflow.toml ref=<base_branch>
```

If the file exists, parse the `[merge]` section for:
- `method` — one of `squash`, `merge`, `rebase` (default: `squash`)
- `delete_branch` — whether to delete the head branch after merge (default: `true`)

If the file does not exist, use defaults: method `squash`, delete branch `true`.

### Step 5 — Execute merge

Build the merge parameters:

- `sha`: current head SHA (from Step 2) — this is the SHA-pinned safety check
- `merge_method`: from config (Step 4)
- `commit_title`: `{PR title} (#{number})`
- `commit_message`: the PR body, followed by a blank line and a co-author trailer for the PR author:
  ```
  Co-authored-by: {author_login} <{author_login}@users.noreply.github.com>
  ```

Execute the merge:

```
github.pr_merge: owner=<owner> repo=<repo> pull_number=<pr_number> sha=<head_sha> merge_method=<method> commit_title=<title> commit_message=<message>
```

If the merge **succeeds**, capture the merge SHA from the response and proceed to Step 6.

If the merge **fails**, report the error to the user and **STOP**. Do not retry. Common failure reasons include:
- Head SHA has changed (concurrent push)
- Required status checks have not passed
- Branch protection rules prevent the merge
- Merge conflicts with the base branch

### Step 6 — Delete branch (if configured)

If `delete_branch` is `true` (from config in Step 4), delete the head branch:

```
github.branch_delete: owner=<owner> repo=<repo> branch=<head_branch>
```

If deletion fails (e.g. branch is protected or already deleted), log the error but do not treat it as a failure — the merge itself already succeeded.

### Step 7 — Post merge confirmation comment

Post a comment on the merged PR:

```
github.pr_comment: owner=<owner> repo=<repo> pull_number=<pr_number>
```

Comment body:

```
Merged via review-pr -> prepare-pr -> merge-pr workflow.
Merge SHA: <merge_sha>
Method: <merge_method>
```

If this skill was used standalone (no prior review/prepare artifacts), adjust the comment:

```
Merged via merge-pr (standalone).
Merge SHA: <merge_sha>
Method: <merge_method>
```

### Step 8 — Write merge artifact

Create the artifact directory and write the merge artifact:

```
exec.run: mkdir -p .local/merges
```

Write the following JSON to `.local/merges/pr-{number}.json`:

```json
{
  "pr_number": 123,
  "repo": "owner/repo",
  "merge_sha": "<sha>",
  "merge_method": "squash",
  "base_branch": "main",
  "head_branch": "feature/example",
  "delete_branch": true,
  "merged_at": "2024-01-01T00:00:00Z"
}
```

The `merged_at` field should be the current UTC timestamp.

### Step 9 — Output results

Present the merge result to the user:

- Merge SHA
- Merge method used
- Whether the branch was deleted
- Link to the PR (if available)
- Path to the saved merge artifact
