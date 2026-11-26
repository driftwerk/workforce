# A plan's ticket starts in the worktree it already has (DW-766)

## Brief

Design brief, verbatim:

> Please design a full solution correctly and ensure you don't miss anything or cause any bugs and you handle any edge cases.
> Please design a robust, elegant, production quality engineering solution that employs idiomatic code using a functional programming style and follows the existing codebase tech patterns.
> Please make sure to design the most elegant and best solution possible from both a code and an architectural perspective
> Please ensure you specify that there should be no todos or partial implementations and make sure to design appropriate tests.
> Please ensure the design specifies to not write any comments that are not absolutely needed and to not create excess code duplication.

Implementation brief, verbatim:

> Please implement all changes correctly and ensure you don't miss anything or cause any bugs and you handle any edge cases.
> Please implement robust, elegant, production quality idiomatic code using a functional programming style and follows the existing codebase tech patterns.
> Please make sure to implement the most elegant and best solution possible from both a code and an architectural perspective
> Please ensure you do not introduce todos or partial implementations and make sure to implement appropriate tests that all pass and the code all compiles and there are no fmt or linting/format/build issues.
> Please do not add any comments that are not absolutely needed and please make sure to not create excess code duplication.

Code rules: no TODOs, no partial implementations, no comment that isn't strictly needed, no duplicated code. Idiomatic, functional-style Rust in the crates' own patterns (`DeskError` variants answered with a `code`, `MachineRefused`, `StartRefused`). Every behaviour below has a test in "Tests".

D = `crates/desk/src`, H = `crates/hub/src/desk_crew`.

## What happens today

- A plan's ticket gets its own worktree when another ticket of the plan works in the same folder (`Start::worktree`). Its branch is `branch_for(key)` (`man-515`), its folder `<repo>/.worktrees/man-515`.
- **On a machine** (`plan_spawn_on`, D/service/plans.rs) the start body carries `worktree: "man-515"`. The machine's Desk starts it through `start_session` → `worktree::create`, which refuses any existing folder: `"{path} already exists"` (D/worktree.rs:80). The machine answers 400, `machine_call` (H/mod.rs) makes it `MachineRefused::Refused`, and `plan_spawn_on` makes it `StartRefused::Failed("devbox didn't start MAN-515: … already exists")`. Every later start of the ticket fails the same way, forever: the worktree is there from an earlier session of the same ticket.
- **Locally** (`plan_spawn`) the plan side looks first with `worktree::existing` and starts in any folder at that path, whatever it is: not checked to be a worktree, nor on the ticket's branch, nor free. The session's record has no branch then.
- **Retries** (`start_failed`, H/plan.rs) already wait 1, 2, 4, 8 minutes and pause the plan at the 5th failure (`FAILS_BEFORE_PAUSE`), saying why on the Floor. The count is forgotten when the plan's scope or where the ticket runs changes. A boss at Act told "MAN-515 didn't start" that edits the plan's scope in answer resets the count, is told again, and so on: a loop with the boss's turn as its period, which is what a start every ~60 s looks like. Nothing in the code resets it otherwise (a pause for failing starts is resumed only by the person's Continue). The loop ends here because the failure itself goes, and a failure no retry can clear pauses the plan at once, which no edit resumes.

## Decisions

1. **The ticket's own worktree is reused, with a fresh session in it.** Its uncommitted work stays as it is. The session is told the ticket was worked on before (`AGAIN_NOTE`, as today), so it checks what is done first.
2. **An earlier session isn't taken up.** `plan_spawn_on` deliberately never takes up an older session of the ticket's key (tested in `a_machine_start_never_answered_is_found_there_and_never_made_twice`); a session taken up would get no prompt, and on a machine a resume or a message would need a relay call Desk's native side doesn't carry. One that is stopped, interrupted or closed doesn't hold the worktree: its work is in the worktree, which is reused. One still running in it does (decision 4): the person decides whether the ticket goes to it.
3. **"The ticket's own" is decided by git and by the Desk that holds the folder,** in one place, `worktree::create`, on whichever computer the session starts (the same code on this computer and on a machine):
   - the folder is `<repo>/.worktrees/<slug(branch)>`, Desk's own layout;
   - git lists it as a worktree of that repository (`git worktree list --porcelain`, paths compared canonicalised);
   - it is on `refs/heads/<branch>`;
   - no running session of that Desk (`SessionState::is_running`) works in it or below it: checked in `start_session` under the same state lock that inserts the new record, so two starts can't both have it.
   Not its own:
   - the folder is there but isn't a worktree of the repository;
   - it is a worktree on another branch, or detached;
   - the branch is checked out in another worktree (git would refuse the add anyway);
   - a running session works in it.
   Each is said in a sentence naming the folder and what holds it.
4. **What isn't its own is a failure no retry clears**: a new `DeskError::WorktreeTaken(String)`, answered as a 409 with `code: "worktree_taken"` (`worktree::TAKEN`), as `NotHeld` and `CrossesBoss` are. On a machine `machine_call` reads that code into a new `MachineRefused::WorktreeTaken`, and `plan_spawn_on` keeps it as `StartRefused::Failed(DeskError::WorktreeTaken("devbox didn't start MAN-515: …"))`. Locally `plan_spawn`'s error is already that. The hub's `start_failed` pauses the plan at once on it (`pause_plan_failing`, "MAN-515 can't start: …"), so the person reads it once on the Floor with Continue, and the boss is told once. It isn't tried again until the person continues.
5. **A worktree whose folder was deleted but is still registered** is pruned first (`git worktree prune`, only when the folder isn't there), so it is made again on the branch instead of failing with "missing but already registered worktree".
6. **Every named-branch start reuses this way, not only a plan's.** New session with a branch and the crew's hand-outs go through `worktree::create` too: a start on a branch whose own free worktree is there starts in it instead of failing. `create_at` (a handoff's new branch from a commit) and `create_fresh` (a separate copy) stay strict.
7. **Locally the plan starts through the same path:** `plan_spawn` sends the branch and `start_session` reuses or makes the worktree. `worktree::existing` goes. The record keeps its branch.
8. **Ordinary failures keep backing off** as today (doubling, pause at 5). A machine whose Desk is older than this still answers "already exists" as a plain refusal; that backs off and pauses at the 5th failure until Desk there is updated.

## Change

### D/worktree.rs
- `#[derive(Debug, thiserror::Error)] pub enum Unmade { #[error("{0}")] Taken(String), #[error(transparent)] Failed(#[from] eyre::Report) }`.
- `pub const TAKEN: &str = "worktree_taken";`
- `create(cwd, branch) -> Result<PathBuf, Unmade>`: validate and place (shared with `create_from`), prune when the folder is missing, read the worktree list once, then:
  - folder there: `Ok(path)` when the list has it on the branch, else `Taken` saying what it is;
  - folder missing: `Taken` when the branch is checked out at another path, else `git worktree add` as today.
- A small pure parser of `git worktree list --porcelain` into `(PathBuf, Option<String>)` (path, branch), and a pure function deciding from that list, the folder and the branch what the folder is (`Free`, `Own`, or `Taken(why)`), tested without git.
- `create_from` keeps the strict refusal for `create_at` and `create_fresh`; `existing` goes.

### D/service.rs
- `DeskError::WorktreeTaken(String)`.
- `start_session`: `worktree::create` errors map `Unmade::Taken` → `WorktreeTaken`, `Unmade::Failed` → `Invalid` as today. At the record's insertion, a running record (other than itself) whose cwd is the worktree or below it refuses with `WorktreeTaken` naming that session.

### D/api.rs
- `WorktreeTaken` → 409 `{ error, code: worktree::TAKEN }`.

### D/crew.rs
- `MachineRefused::WorktreeTaken(String)`, displayed as its words.

### D/service/plans.rs
- `plan_spawn`: worktree branch always passed when the ticket has its own worktree; no `existing` lookup.
- `plan_spawn_on`'s `refused`: `WorktreeTaken(why)` → `Failed(DeskError::WorktreeTaken("{machine} didn't start {key}: {why}"))`, sure (nothing was made).

### H/mod.rs
- `machine_call`: `code == worktree::TAKEN` → `MachineRefused::WorktreeTaken`.

### H/plan.rs
- `plain`: `WorktreeTaken(why)` → `why`.
- `start_failed(p, t, scope, e: DeskError)`: whether it is retried comes from the error (`!matches!(e, WorktreeTaken(_))`); one not retried pauses at once with "{key} can't start: {why}", the rest as today.

## Tests

D/worktree.rs:
- `a_branchs_own_worktree_is_reused_with_its_work`: create, write an uncommitted file, create again: the same folder, the file kept (replaces the "twice errors" assertion).
- `a_folder_that_isnt_the_branchs_worktree_is_taken`: a plain folder at the path; a worktree there on another branch; the branch checked out elsewhere: each `Unmade::Taken`, saying what holds it.
- `a_deleted_worktree_still_registered_is_made_again`: `rm -rf` the folder, create again: made, on the branch.
- The porcelain parser and the decision function on fixed text (detached, prunable, several worktrees).

D (service):
- `a_running_session_in_a_worktree_holds_it`: a session started on branch `x`, another start on `x` while it runs is `WorktreeTaken`; once it is stopped, the next start runs in the same folder.
- D/api.rs: `WorktreeTaken` answers 409 with `code: "worktree_taken"`.

D/service/plans.rs:
- `a_plans_ticket_starts_in_its_own_worktree_from_before`: a ticket whose `.worktrees/<key>` exists on its branch with uncommitted work starts in it, the work kept, its record on the branch.

H/tests/plans.rs:
- `a_machine_ticket_whose_worktree_there_is_taken_pauses_its_plan_once`: devbox answers `session_start` with 409 `worktree_taken`: the plan pauses at once saying why, one start was sent, the boss is told once.
- `a_machine_ticket_that_keeps_failing_to_start_backs_off_and_pauses`: devbox refuses every start plainly: the starts come further apart each time and the plan pauses at the 5th.
