# Every merge at Act fully goes through review (DW-765)

## Brief

Please design a full solution correctly and ensure you don't miss anything or cause any bugs and you handle any edge cases.
Please design a robust, elegant, production quality engineering solution that employs idiomatic code using a functional programming style and follows the existing codebase tech patterns.
Please make sure to design the most elegant and best solution possible from both a code and an architectural perspective
Please ensure you specify that there should be no todos or partial implementations and make sure to design appropriate tests.
Please ensure the design specifies to not write any comments that are not absolutely needed and to not create excess code duplication.

## What happened

26 Nov, Eden plan, mantis-crm/monorepo#878 (MAN-544). The PR was green, mergeable, and reviewed clean inside the MAN-544 Codex session itself. The boss never called `crew.github_merge` (0 calls since #528). It told the session to "mark the PR ready for review, and merge it". That worked, but it went around `ready()` in `review.rs`, which needs a clean review recorded at the exact head, every check green, and the merge pinned to that sha.

**Why.** `crew.github_merge` accepts only a review recorded with `crew.record_review`, from a separate session started with `review_of`.
- Starting that session, recording its verdict and merging were all left to the model, through `LOOP`. The model skipped the first step.
- Nothing stopped a session merging its own PR. At Act fully, `gh pr merge` (class `GhPrMerge`) went to the boss's model like any other uncached request, and the model allowed it.

## Decisions

### 1. Desk starts the review itself

**Where.** In the plan watch (`plan_watch.rs`), for each ticket whose session rests (`Stands::Rests`).

**Which pull request.** It is found from the session's own folder, never from its account's summary:
- the folder's `(origin, branch)` (`workspace_of`) is read through `branch_pull`;
- so the review is only of a pull request `ready()` would accept: the session's branch, in its origin, not a fork.

**When.** Every one of these holds:
- the boss is at Act fully now (`level_now`);
- the ticket is in Doing or Review, and its session is the boss's own;
- the pull request is open;
- its checks are all `success` at a full-SHA head.

Below Act fully nothing changes, because the boss can't merge there.

**Deciding the step.** A pure function, `review::step(head, newest, at_head) -> Step`, decides what happens. It is tested on its own. `at_head` is `Reviewing::{None, Open, Ended}`, built from the linked review sessions started at that head: whether one is still in Desk and not Closed, or all of them are gone.
- `Merge`: a clean review is recorded at the head. The boss is told once per head (decision 3).
- `Settled`: changes were asked for at the head. Nothing more happens until a new head.
- `Underway`: a review session at the head is still open. The record sweep (decision 2) carries it on.
- `Lost`: every review session at the head ended without a verdict. The boss is told once and may start another itself. Desk never loops on starting new ones.
- `Start`: start the review.

**How it starts.** Desk builds the boss's own `Action::StartSession` and runs it through `gate` → `perform` as the boss, the way the plan watch already resumes sessions (`start_again`).
- **Folder:** the implementing session's folder.
- **Worktree:** `plan::review_worktree(pr, head)`, i.e. `review-<number>-<short head>`, created **detached at the reviewed head** by a new `worktree::detached(cwd, name, commit, from)`. Desk always names it: a worktree the model names in `review_of` (a ticket's own, say) is ignored.
  - It goes in the main checkout's `.worktrees/`, rooted at the parent of `--git-common-dir`, so a plan ticket's own worktree never nests one.
  - One already there at that commit, detached (`branch --show-current` empty), is reused, which covers a start that was cut off. One at another commit, or on a branch, is refused.
  - A head the repository hasn't got (pushed from another clone, GitHub's "Update branch", a bot) is fetched first from `origin`'s `refs/pull/<n>/head` into `refs/remotes/origin/pull/<n>/head`, so the review can read it and its closing sees it as pushed. The fetch can't wait on anyone: `credential.interactive=never`, `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND="ssh -o BatchMode=yes"` unless the repository or environment sets its own SSH command, and a 60 s limit.
  - Desk's start path picks this for every `review_of`. The review's folder is therefore at exactly the head it reviews, which is what `crew.record_review` checks, and it never touches the implementing session's checkout.
- **Desk's own `.worktrees/` is nobody's work.** `workspace_of` drops `.worktrees/` from `uncommitted` only when every entry in it is a worktree `git worktree list` registers. Otherwise a review worktree, or a plan ticket's, would keep the main checkout's session from ever closing; a file someone keeps there is still their work.
- **Goal:** `review::goal(pr, key)`, and Desk's `REVIEW_VERDICT` is appended as before.
- **Link:** `review_of` now also carries `for_session` (`#[serde(default)]`), the session whose work the PR is. `pr_review_sessions` keeps it, together with the head the review was started at.
  - Additive migration `V096__pr_review_heads.sql`: `ADD COLUMN IF NOT EXISTS head_sha TEXT`, `for_session UUID`, `settled_at TIMESTAMPTZ` (when Desk was done with the link: recorded, moved past, or its session gone), and an index on `for_session`. A relink clears `settled_at`.
  - A launch that fails deletes its link (`pr_review_unlink`) and removes the worktree, if this start made it, so a start that never ran blocks nothing and leaves nothing.
  - When the boss starts a review itself (`crew.start_session` with `review_of`), `review_head` fills in `for_session`. It is the one session the boss runs whose `(origin, branch)` is the PR's head repository and branch, the same test `ready()` applies. When several are, it is the one whose ticket names the PR (a PR link, or `github.com/o/r/pull/n` or `o/r#n` in its title or body); when that is still not one, it is left empty. A model never supplies it: like `head_sha`, it is removed from what the model sends.
- **The session is told.** The CI-green line Desk already sends the implementing session (once per head) gains, at Act fully: "Desk has it reviewed at that head, and your boss merges it once the review is clean: don't merge it yourself."

**Limits.**
- **One review per head.**
  - `step` never starts a second review while one at that head is open, and never restarts one that ended without a verdict (`Lost`).
  - Desk's start path refuses a second review session at a head while the first is still open in Desk (not Closed), whoever starts it, and says to nudge or record the open one. This runs under the hub's `starting` lock, which every boss start takes, and under Desk's own `review_starts` lock from the check to the link, so no two starts race past it, whichever way they reach Desk.
- **Slots and the plan's session budget: a review uses its ticket's slot.**
  - The person's `parallel` limit binds reviews as it binds every start. `slot_free` applies, and a review it refuses is tried again next minute (its once-key is released).
  - A ticket in Doing keeps its slot while its session rests (`plan::holding`), so its review takes that slot.
  - A ticket in Review is held while its session **or a review session linked to it by `for_session`** is busy. The runner's `busy` closure for `plan::held` is given that set, computed before the call (`CrewHub::reviewing(boss)`). So a review never makes the boss run more than N, and the runner never fills a slot a working review holds.
- **Spend.**
  - The breaker refuses the start, as for any start.
  - `gate` must say `Run`, which requires `hand_out_work`. Otherwise the boss is told once to start the review itself.

### 2. Desk records the verdict when the review session ends

**Where.** Each minute, for every boss at any level, its unsettled review links are swept on their own (`sweep_reviews` over `pr_reviews_unsettled(boss)`, a database read only). This runs whatever state the implementing session is in, whatever the ticket's state, and after the implementing session is gone. A plan ticket's slot is still held while its review is busy (`reviews_work`).

**When.** A review session is recorded once its turn has ended, or once it is closed:
- it is in the boss's charge, live, or closed while its link is unsettled (`record_of` reads a closed session's transcript then);
- it is not busy, and nothing is pending or asked;
- it has rested for `close_settle`;
- it is recorded once per turn, keyed by its prompt count.

**How.**
- `crew.record_review`'s checks move into one function, `CrewHub::record_of(service, boss, session) -> Result<PrReview, String>`, which the tool and the sweep both call. Its GitHub reads go through `as_person`. Nothing is duplicated.
  - `record_of` checks the link, the charge, the turn ended, the folder's HEAD, the PR's head on GitHub, and the `REVIEW:` line, then saves the review.
  - The tool keeps `session_seen`, its scope check, for itself.
- **Outcomes:**
  - **Clean:** the boss is told the merge line (decision 3), only while it is at Act fully. Below it the review is recorded and closed, and nothing merges.
  - **Changes:** Desk tells the implementing session on its line, as the boss (`Action::Nudge`, gated). It passes the review's final reply, fenced as the reviewer's words, and says that its next head is reviewed again. The boss's plan turn is told in one line.
  - **No verdict line:** the review session is nudged once to end with `REVIEW: clean|changes <head>`. The next time its turn ends without one, the boss is told once, with `NO_VERDICT`. A closed one with no verdict is settled and the boss told once.
  - **Head moved on, or the folder moved:** nothing is recorded; the link is settled and the session closed. The next green head is reviewed afresh (decision 1).
  - **Session gone from Desk:** the link is settled.
  - **Anything else (GitHub unreadable, say):** tried again with a doubling wait from one minute. After `RECORD_TRIES` (5) the boss is told once why, and the link is settled, so nothing holds the review open: the boss may still record it with `crew.record_review` while it is open, or close it.
- **Closing.** A review session's whole job is its verdict.
  - Once a verdict is recorded, or once the PR's head has moved past the head it reviewed, the sweep closes it as the boss (`Action::Close`). It then removes its worktree with `git worktree remove`, without `--force`, and only while its HEAD is still the reviewed head, so a worktree holding work stays.
  - In `close_held`, a review session's open pull request doesn't keep it open: it carries no work, as `superseded` already says. Its unsettled link does: a boss can't close a review before its verdict is recorded or its head moved on. The person's own close always goes, and the sweep then records it from what it said.

### 3. The boss merges: it decides, Desk tells it exactly when

**Choice.** At Act fully the boss decides the merge, and Desk does not merge on its own.

**Why the boss decides.**
- The person's rule is that at Act fully the boss acts like a capable engineer and ships reviewed, green PRs **itself**, with the person's directives binding both ways.
- Only the boss's model reads directives. A directive such as "releases need my OK" must turn a merge into `crew.ask_person`; the existing test `a_directive_that_releases_need_the_persons_ok_has_the_boss_ask_instead_of_merge` covers this. If Desk merged whenever `ready()` held, it would ignore directives. It would also need a second, Desk-side directive reader, which would duplicate the boss.
- The model never failed at merging. It failed because nothing it could merge existed: there was no recordable review.
- Desk now makes every precondition true without the model, and then tells the boss once per head, in plain words, that the PR is ready. It tells it once again if a plan turn sees the same green, clean head after a restart. `ready()` still guards the merge itself.

**The merge line**, keyed `merge:<pr>:<head>`, says:
- "Desk recorded a clean review of `<pr>` at `<short head>`; its checks passed. Merge it with crew.github_merge (repo, number) unless one of the person's directives says otherwise, then move `<key>` to Done."
- For a draft, it adds: "It's a draft: have its session mark it ready (`gh pr ready <n>`) first."

After the merge, `Landed::Merged` and the close sweep carry on as they do now, and the next ticket starts.

### 4. A session's own merge is refused, in favour of `crew.github_merge`

- **Classifier** (`class.rs`):
  - `gh pr merge` stays `GhPrMerge`;
  - `gh api` writing `repos/<o>/<r>/pulls/<n>/merge`, and `gh api graphql` with a `mergePullRequest` mutation, are now `GhPrMerge` too, rather than `Other`.
  - A session's push straight to a default branch that pull requests merge into is denied the same way, in favour of review, with no model call, unless the person is in the session. That holds only where there is a pull-request flow (`Facts::pr_flow`): `origin` is a GitHub repository and its default branch is already there (`origin/HEAD` names it and `origin/<default>` exists). The push must target that branch exactly (`boss::pushes_the_default`), read straight from the command. A branch that only reads as a default one (`release-…`, `production`), a repository not on GitHub, a new repository's first push, or a push through a shell or wrapper stays `DefaultBranchPush`. It goes to the boss's model as before, never cached, so the person's directives weigh in.
- **Decision** (`decide.rs::decide_request`, reached only at Act fully):
  - while the person is in the session (`person_in_for`, as `close_held` reads it), its merge is theirs. It is left to them, never denied over their own word;
  - otherwise a `GhPrMerge` request is denied by the boss's decision without a model call. `Verdict::Deny` is recorded as the boss's decision, as every verdict is.
  - Why: "At Act fully a pull request is merged only by your boss, with crew.github_merge, once a clean review is recorded at its head."
  - Instead: "Don't merge it yourself: tell your boss on your Desk line that it is ready to merge; your boss merges it once it has a clean review at its head."
- Below Act fully, Act's judge already gives a merge to the person, and that is unchanged.
- **Before it runs, whatever the mode.** Desk refuses a session's own merge before the tool runs, not only when it asks:
  - **Claude and Devin:** in the `ToolStart` hook (`PreToolUse`), beside the breaker (`past_review_refused`). The call is refused when the session is in a boss's charge, the boss acts fully, and the person isn't in the session (`PERSON_NOW_MINUTES`), and it is a `Bash` call whose command does one of these:
    - `merges_a_pull`: a `gh pr merge` or a merging `gh api`, anywhere in the line, inside `sh -c`/`bash -l -c … args` and `eval`, or behind a wrapper (`env`, `command`, `exec`, `nohup`, `nice`, `time`, `timeout`, `xargs`). It is denied with `plan::own_merge_refused()`.
    - pushes past review, as above. It is denied with `plan::default_push_refused()`. The repository is read off the async workers.

    This holds in `bypassPermissions` too, since a hook's deny binds every mode.
  - **Codex** has no pre-tool hook. Its exec-policy rules (`prefix_rule`) are read only from `CODEX_HOME/rules`, which every Codex session on the machine shares, so they can't be set for one session. Desk instead puts a `gh` shim first on the app-server's `PATH` (`gh_shim.rs`).
    - The shim asks Desk's hook route `/gh/{id}/{token}`, never through a proxy (`--noproxy '*'`), only for a call spelled as a merge: `gh pr merge`, `gh api` of a `…/merge` endpoint, or a GraphQL `mergePullRequest`. Everything else (`gh pr list --search merged`, `--json mergeable`) runs the real `gh` unasked.
    - Desk answers with the same check. An allowed call execs the real `gh`. A refused one prints the reason and exits 1. If Desk can't be asked, the merge doesn't run.
    - **Its PATH holds whatever the person's profiles say.** By default Codex runs each command in a login shell. Its profiles (`/etc/profile`'s `path_helper`, `~/.zprofile`'s `brew shellenv`) would put `/opt/homebrew/bin` ahead of the shim. So Desk starts every app-server with `-c allow_login_shell=false -c features.shell_snapshot=false` (`codex::COMMAND_SHELL`, CLI 0.166). Commands then run in a plain shell, with no snapshot of a login shell replayed before them. The `PATH` Desk gives is already the person's login one, so their tools are still found. A command that asks for a login shell is refused by Codex with its own reason.
    - The real-Codex test proves it with profiles that put another `gh` first, and with snapshots switched on in Codex's own config.
  - **A guard for honest mistakes, not a boundary.** The shim, and the hook's reading of a command, stop a session that merges the plain way. They don't stop a determined one: a `gh` alias (`gh alias set m 'pr merge'`), a query read from a file (`-F query=@file`), `--input` with a body, the real `gh` by its absolute path, `curl` with `gh auth token`, or a `~/.zshenv` or `BASH_ENV` that puts another `gh` first. The boundary is GitHub's own: branch protection and the merge going through `crew.github_merge`'s checks. The boss's wording keeps a session from being told to merge.
- **What stays the model's:** a merge Desk can't see as one: `curl` to the API, a script file, an unquoted `gh api repos/{owner}/{repo}/…/merge` (it doesn't split as a plain command), or a GraphQL query read from a file (`-F query=@file`). These reach the boss's decision as `Other`, which no standing verdict answers. `SYSTEM` tells the model a session's own merge, however it is run, is DENY.
- **Wrappers in the classifier:** a wrapped command (`env`, `timeout`, `xargs`, …) keeps its inner class when that is a risk of its own (`GhPrMerge`, `DefaultBranchPush`). Otherwise it is `Other`, so a standing verdict never allows what a wrapper runs.

### 5. Wording

- **`LOOP`:**
  - Desk starts the review when a plan's PR is green at a new head, and records its verdict.
  - Merge with `crew.github_merge` when told it is green and reviewed clean.
  - Start a review yourself only for a PR that isn't a plan ticket's.
  - **Never tell a session to merge its own pull request:** Desk refuses a session's merge.
- **`ACT_FULLY`'s merge clause:** "merging one that is green with a clean review at its head yourself (crew.github_merge), never by telling its session to".
- **The line turn's level line at Act fully** (`line.rs::level_line(Full)`): adds "A session's pull request is merged by you with crew.github_merge, never by the session."
- **`ready()`'s "no review" refusal:** says Desk starts a plan ticket's review once its checks pass, and for any other PR to start one with `crew.start_session` and `review_of`.

## Code rules

- No TODOs, no partial implementations, no comment that is not strictly needed, and no duplicated code.
- `record_of` is the single recording path.
- `step` is the single review decision.
- `worktree::detached` sits beside `create_at`, and the start path chooses between them by `review_of`.
- Idiomatic, functional-style Rust in the crate's patterns: `Action` through `gate`/`perform`, `plan_watch.done` once-keys, `text_enum!`, and `eventually` in tests.
- `cargo fmt`, `clippy -D warnings` (all targets) and the targeted tests pass. CI is the full gate.

## Tests

- **`review::step`** (unit): every arm.
  - Clean at head → Merge.
  - Changes at head → Settled.
  - A review recorded at an older head plus a session started at this head → Underway.
  - Nothing → Start.
- **Classifier** (unit):
  - `gh pr merge 7 --squash` and `gh api -X PUT repos/acme/web/pulls/7/merge` are `GhPrMerge`;
  - `gh api repos/acme/web/pulls/7` (a read) stays `GhRead`.
- **`worktree::detached`** (unit, real git):
  - made at the commit;
  - found again at that commit;
  - refused when present at another;
  - a head the repository hasn't got is fetched from `origin`'s pull ref and kept under `refs/remotes/origin/`.
- **Store** (Postgres):
  - V096 applies twice;
  - a link keeps its head and `for_session`; it is unsettled until `pr_review_settle`, and a relink unsettles it;
  - `pr_review_of`, `pr_reviews_for` and `pr_reviews_at` read them back, and `pr_review_unlink` forgets one.
- **Desk start path** (`charge_tests`):
  - a review start with a worktree is made detached at the head and linked with head and `for_session`;
  - a second review start at the same head is refused while the first is open, and allowed once it is closed, its worktree found again;
  - a boss naming the ticket's own worktree gets Desk's review worktree instead;
  - a launch that fails leaves no link and no worktree it made;
  - in `bypassPermissions`, a session's own merge is denied before it runs under a boss acting fully, a read isn't, the shim's route answers the same, a wrong token is refused, and the person typing in the session lets it go.
- **`gh` shim** (unit, real shell and server): a merge spelling runs only as Desk says; anything else runs as asked and unasked, `gh pr list --search merged` and `--json mergeable` among them.
- **Real Codex** (`codex_real_tests`): with every profile putting another `gh` first, and Codex's shell snapshots on, `command -v gh` in a Codex session finds Desk's shim.
- **Push past review** (`charge_tests`, bypass mode): refused for a GitHub `origin` that has `main`; not for a release branch, a repository not on GitHub, a first push, or while the person types. In the hub (`tests/standing.rs`): denied at once with no model call, while a release branch is the boss's call. A local `origin` is still the model's (the standing test).
- **`pushes_the_default`** (unit): every spelling of a push straight to `origin`'s default branch, and none of the others.
- **Giving up** (hub): a review whose pull request can't be read is settled after the last try, its boss told, and nothing holds it open.
- **`worktree::remove`** (unit): kept when its HEAD has moved off the reviewed head.
- **Classifier**: `gh api graphql` with `mergePullRequest` is `GhPrMerge`; `merges_a_pull` finds a merge wherever a line runs it, wrappers and split `-c` flags included; a wrapped command keeps only a risky class; `-F query=@file` is `Other`.
- **`workspace_of`** (unit): a plain file under `.worktrees/` is uncommitted work, a registered worktree there isn't.
- **Decision** (hub, `tests/ships.rs`): a session's `gh pr merge` at Act fully is denied at once with the `crew.github_merge` instead, and the boss's model is never asked.
- **Lifecycle** (hub, `tests/ships.rs`, from the ticket), run at Act fully:
  - a plan of two tickets in `acme/web`, parallel 1;
  - DES-1's session opens #7 on `dw-761`, CI is green, and its turn ends;
  - **without any model call**, a review session starts in a detached worktree at the head, linked to #7 and DES-1's session;
  - it ends with `REVIEW: clean <head>`, and Desk records it;
  - the boss's plan turn is told it is ready, and the scripted model calls `crew.github_merge`, which squashes at the head (one PUT, with `sha`);
  - the scripted model moves DES-1 to Done, and DES-2's session starts;
  - throughout, the model is never asked to start or record a review.
- **Lifecycle, changes** (hub):
  - the review ends `REVIEW: changes <head>`;
  - Desk records it and tells DES-1's session the review's words;
  - nothing merges;
  - a new green head starts a second review.
- **Head moving mid-review** (hub): a review whose PR is pushed again is closed unrecorded, its worktree removed, and the new head reviewed.
- **Closed before recorded** (hub): the boss can't close the review; the person closes it; Desk records it from what it said, and the boss merges.
- **Level drop** (hub): the boss drops to Act mid-review; the review is still recorded and closed, and nothing merges.
