# A plan's sessions start in the plan's group (DW-754)

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

Code rules: no TODOs, no partial implementations, no comment that isn't strictly needed, no duplicated code. Idiomatic, functional-style Rust and TypeScript in each crate's and app's own patterns (`DeskError` answered with its status, pure functions beside the service that uses them, the existing `GroupChoice` picker and design tokens in Desk). No migration: `plans.group_id` exists. Every behaviour below has a test in "Tests".

D = `crates/desk/src`, H = `crates/hub/src/desk_crew`, A = the Desk app (`driftwerk/workforce-desk`).

## What happens today

- `NewPlan.group` is "the boss's group; none is the main boss's". `crew.plan` (H/plan.rs `propose`) sets it to the named board's group, else the boss's own (`boss_group`), and `propose_plan` refuses a board in any other group ("That board isn't in the group you run.").
- Every session a plan starts goes in `plan.group`: local ones through `NewSession.group` (`plan_spawn`), machine ones through `plan_link_on`.
- Mantis has no boss of its own, so the main boss runs its work, and every plan the main boss makes is in Ungrouped: its sessions land in Ungrouped whatever the work is.
- Nothing changes a plan's group. `move_plan` refuses a board of another group.
- A deleted group's boards become ungrouped (`put_layout_in`). Its plans keep the deleted id. `Seen::sees` says no overseer sees a gone group, so `boss_of_plan` finds nobody, `run_plan` returns without pausing (the boss is still registered), and the plan stalls in silence.
- Three places on the machine path read `p.group.is_none()` as "a plan of the main overseer": `plan_spawn_on`'s `held` (the plan's own grant there), `watch_machines`' `to_start` (the machines its grant is wanted on), and the group every relay call carries (`desk.sessions`, `desk.session_start`, `desk.checkout`). A main-boss plan in an open group (a group with no boss of its own, like Mantis) gets no plan grant, and its calls carry `mantis`, which no machine has a grant for: each start there is refused. Today that only happens when the main boss names a Mantis board by hand. Once plans go in their groups, it would happen every time.

## Decisions

1. **A plan's group is the group its work belongs to**, not its boss's. It is the sidebar group its sessions start in, the Floor section and plan list it shows in, and the group whose budget holds its starts (`plan_starts_held`). Mantis's group budget now holds Mantis plans, which is the point of a group budget.
2. **The group and the board are separate, within one boss's part of Desk.** A plan's group and its board's group (and the groups of the boards of tickets it takes on) must be in the same scope (`scope::scope_of`, with `Desk::bossed()`), not equal. A new plan with no board named still goes on its group's own board (`plan_board(group)`), so a fresh plan has both together. Changing a plan's group never re-files its tickets. That keeps their keys, their worktree branches (`branch_for(key)`), their session names and the issues they took on. It also lets the plan move when it has tickets filed in another tracker, which `move_plan` refuses.
3. **What the person or the boss can choose:** Ungrouped and the groups in the plan's boss's scope. For the main boss that is Ungrouped plus every open group. A group boss's plan is fixed to its group. A group with a boss of its own (or a sealed one) is refused in plain words that name its boss: "Acme has a boss of its own, Ada: a plan of Desk's stays in a group Desk runs."
4. **Changing the group moves the plan's sessions.** Each of the plan's tickets in Doing or Review that a session has, local or on a machine, has its sidebar item moved to the new group, in one layout change. A session the person has moved out of the plan's old group stays where they put it. Starts serialise with the change on the start lock (`plan_starting`), and each start reads its plan's group again under that lock, so no session started meanwhile lands in the old group.
5. **When nothing is said, the plan's work picks its group**, through one pure function (`plan_group::pick`). Each ticket votes for at most one group, taking the first of these that gives an answer:
   1. the session it takes on, if that session is in a group;
   2. its folder: the group that the most of this computer's open sessions working in it (or a worktree or subfolder of it) are in, counting only sessions in a group the boss sees;
   3. its folder's or repository's path segments: a group whose name, folded to lowercase letters and digits, is one of them (`~/dev/mantis/monorepo` and `github.com/mantis/monorepo` both vote Mantis).

   Only groups the boss sees can win. The group with the most votes wins. A tie, or no votes at all, leaves the boss's own group. The board's group wins before any of this: the board the boss names, or else the board of the first ticket the plan takes on. The boss can also say `group` itself, and what it says comes first. Its tool's answer always says which group the plan is in and why ("It is in Mantis: its folders are Mantis's (~/dev/mantis/monorepo). Tell the person; they can change it on its card."), and the plan card shows it.
6. **A plan follows its group's boss.** When a running plan's boss no longer sees its group but the group's scope has a boss (Mantis was given a boss, or a group was deleted and its plans went to Ungrouped), the runner hands the plan to that boss with the existing `pass_plan`: noted as handed over, paused with `NoBoss` until the person says Continue. A plan whose boss has left the hub is paused as today. A scope with no boss (a sealed group) leaves the plan waiting, as the Floor already shows ("no one's to lead").
7. **A deleted group's plans go to Ungrouped** in the same transaction that ungroups its boards. Each one is announced again, with its Floor need, its event group and its breaker facts made current.
8. **The machine path asks who runs the plan, not whether it has a group.** One helper, `Desk::relay_group(&Plan) -> Option<&str>`, returns the plan's group when that group is a boundary (it has a boss of its own, or is sealed), and `None` when the plan is in the main overseer's part. The three machine-path places use it in place of `p.group`: `held` becomes `relay_group(p).is_none() && boss_safe(engine)`, the relay group becomes `relay_group(p)`, and `watch_machines` filters and keys on it. Sidebar placement (`plan_link_on`, `NewSession.group`) keeps `p.group`.

### Every use of a plan's group, by what it means

| Use | Where | Means |
|---|---|---|
| Session's sidebar group | D `plan_spawn` (`NewSession.group`), D `plan_link_on` | work group |
| Floor section, need group, event group | D `floor_plans`, `plan_awaits`, `plan_note_why` | work group |
| Plan list filter | D `plans(state, group)` | work group |
| Budget pools that hold its starts | D `plan_starts_held` (4 calls), H `run_plan`, H actions.rs | work group |
| Breaker's plan facts | D breaker.rs `PlanFacts` | work group |
| Machine of its group's sessions | H `group_machine` (propose, update) | work group |
| Which boss runs it | H `boss_of_plan`, board.rs, goal_check.rs, plan_watch.rs, scope.rs (`seen.sees`) | scope (unchanged: an open group is in the main overseer's sight) |
| Plan's own grant on a machine | D `plan_spawn_on` `held`, H `watch_machines` `to_start` | grant group (decision 8) |
| Group a relay call carries | D `plan_spawn_on` (`desk.sessions`, `desk.session_start`), D `plan_checkout` (`desk.checkout`), H `watch_machines` (`desk.sessions`) | grant group (decision 8) |
| Board, adopted tickets, session tickets | D `propose_plan`, `move_plan`, `adoptable`, `take_sessions_on` | same scope (decision 2); session tickets are made on `plan_board(p.group)` as today, so new work lands in the plan's group |

## Changes

### D/plan.rs

- `Plan.group`'s doc: "The group its work belongs to: its sessions start there; none is Ungrouped."

### D/plan_group.rs (new, pure, unit-tested)

```rust
pub struct Work<'a> { pub folder: &'a str, pub repo: Option<&'a str>, pub session: Option<&'a str> }
pub struct Working<'a> { pub cwd: &'a str, pub group: &'a str }
pub struct Choice<'a> { pub id: &'a str, pub name: &'a str }
pub enum Why { Session, Folder(String), Named(String) }
pub struct Picked { pub group: String, pub why: Why }

pub fn pick(work: &[Work], working: &[Working], groups: &[Choice]) -> Option<Picked>
```

`groups` holds only the groups the boss sees. `Work.session` is the group of the session a ticket takes on. Each ticket's vote is `vote` (decision 5's three signals, in order, each an `Option` chained with `or_else`). Votes are folded into a `BTreeMap<&str, (usize, Why)>`, where the first ticket's reason is the one kept. The result is the unique maximum. `within(folder, cwd)` is true when `cwd` is `folder` or under it (a `/` boundary, trailing slashes ignored); a session at a parent folder, like the home folder, never votes. `deepest(path)` gives a folder's or repository's segments, deepest first, without a `.git` ending. `fold(s)` keeps ASCII alphanumerics, lowercased. A group whose folded name is empty never matches.

### D/service/plans.rs

- `NewPlan.group`'s doc: "The group its work belongs to; none is Ungrouped."
- A private `in_scope(&self, a: Option<&str>, b: Option<&str>) -> bool`, which is `scope_of(a, &bossed) == scope_of(b, &bossed)`, used by:
  - `propose_plan`: the board's group is in the plan's scope (words: "That board isn't in a group the plan's boss runs."), and `Plan.group` is `new.group` (no longer `b.group`);
  - `move_plan`: the target board's group is in the plan's scope (same words with the board's name);
  - `adoptable`: the ticket's board's group is in the plan's scope (its words become "…isn't on a board of a group the plan's boss runs.").
- `pub fn relay_group<'a>(&self, p: &'a Plan) -> Option<&'a str>` (decision 8), used by `plan_spawn_on` (`held`, the `read` closure, `session_start`) and `plan_checkout`.
- `PlanSummary.group_name`: what the person calls its group (none in Ungrouped), so the boss and the card name it.
- `named_group(group) -> Result<String, DeskError>`: the group's name, Ungrouped for none, `Invalid` for a group the sidebar hasn't. `propose_plan` and `regroup_plan` both check with it first.
- `pub fn work_group(&self, tickets: &[DraftTicket], sees: impl Fn(Option<&str>) -> bool) -> Option<Picked>` gathers `pick`'s inputs from the layout's groups (filtered by `sees`), the views of the sessions the tickets take on (`self.view(id)?.group`), and `self.list()`'s open sessions (cwd and group). Then it calls `pick`.
- `pub async fn regroup_plan(&self, id: Uuid, group: Option<String>, by: &By) -> Result<PlanDetail, DeskError>`:
  1. under `plan_write`, then `plan_starting` (`move_plan`'s order);
  2. `DeskError::Invalid` for a group the layout doesn't have, and for a plan that is done or dropped ("A finished plan stays where it was.");
  3. the same group again returns the plan as it is, with no write and no event;
  4. not `in_scope(p.group, group)`: `DeskError::Conflict`, naming the group's boss (`boss_of(&scope)`) or saying it is sealed;
  5. it reads the plan's moving sessions: its tickets in Doing or Review whose session assignee's sidebar item (`session:{assignee.id}`) is in the plan's old group;
  6. it writes `p.group` and `updated_at` (`plan_saved`: stored, Floor need, announced; the breaker's `PlanFacts` are read again on its next pass), and notes `Kind::PlanChanged` "moved the plan to Mantis: <goal>" (the group as the person names it, Ungrouped for none);
  7. one `change_layout` moves every moving item (`put` to the group, or `remove` for Ungrouped). A layout that fails is logged, as in `plan_link_on`: the plan's group is already the new one, and the next start places its session there.
- `plan_start` and `plan_restart_start` read the plan again under `plan_starting` (`self.plan_of(p.id)`), and use that plan for the launch, so its group (and state) are the ones current at the start.

### D/service/groups.rs and D/service/board.rs

- `put_layout_in` also runs `UPDATE plans SET group_id = NULL, updated_at = now() WHERE group_id IS NOT NULL AND NOT group_id = ANY($1) RETURNING id`. It returns `Orphaned { boards: Vec<Uuid>, plans: Vec<Uuid> }` instead of the boards alone, and `put_layout`/`put_layout_with` pass that on.
- `change_layout_as` announces both: `boards_ungrouped` as today, and the new `plans_ungrouped(&[Uuid])`. For each plan, that re-reads it and runs `plan_awaits` and the `Plan` event (the shared tail of `plan_saved`, split into `plan_heard(&Plan)` so it isn't duplicated). `orphans_heard(&Orphaned)` replaces each `boards_ungrouped` call (the layout change, a machine's grant write, and the start's layout prune).

### D/plan_api.rs

- `POST /api/desk/plans/{id}/group` takes `{ "group": "<id>" | null }`. A body without `group` is 400 ("say the group: its id, or null for Ungrouped"). It calls `regroup_plan(id, group, &By::you())` and answers the `PlanDetail`.

### H/plan.rs

- `crew.plan` gets `group` ("The sidebar group the work belongs to, by name, like Mantis, or Ungrouped. Leave it out and Desk picks it from the plan's board, the tickets it takes on, and its folders and repositories, and says which"). `crew.plan_update` gets `group` ("Move the plan to this group (by name, or Ungrouped): its running sessions move there, and the ones it starts from now on start there"). Both descriptions say "the plan's group" where they now say "your group's board".
- `group_named(&self, b, named: &str) -> Result<Option<String>, String>` resolves a group by id or name, case-insensitively. It accepts "Ungrouped" for a boss whose scope is Ungrouped, and accepts only groups the boss sees. Otherwise it refuses, listing the names it can use: "There's no group called Mantsi you run: name one of Ungrouped, Mantis, Starmoire." `board_group_of` uses it for a named group.
- `propose` decides the group before `plan_machine`, so `group_machine` follows it. The rule is `named group` → `board's group` (named, or the first one taken on) → `desk.work_group(...)` → `boss_group`. A named group with a board in another scope is refused by `propose_plan`. The answer's `note` gains the group sentence (decision 5). The DM's card header keeps its words (Desk reads them as the card's own, checked by Desk's plan.test.ts); the card shows the group itself.
- `update` with `group` calls `regroup_plan`. With `board` or `new_board` in the same call it is refused ("Say group or board, not both."). The change is posted to the DM ("{name} moved the plan to Mantis"), and the note says its running sessions moved.
- `plan_view` gains `"group"`: the group's name, or Ungrouped, so the boss reads where each plan is.
- `run_plan` (decision 6): when `boss_of_plan` finds nobody and the boss is still registered, `desk.boss_of(&scope_of(p.group))` names the group's boss. If that boss is someone else, `pass_plan(p.id, &boss, &By::desk("Its group has a boss of its own now."))` hands the plan over and it is posted to the new boss's DM.
- `watch_machines`: `to_start` filters on `self.desk.relay_group(&s.plan).is_none()`, and each machine's `desk.sessions` call carries `relay_group`.

### A (Desk app)

- `src/crew/PlanGroup.tsx` (new) is the plan's group line in the card's `pc-meta`: a `chip chip-group` chip with its `GroupDot` ("● Mantis" / "Ungrouped"; a group this sidebar hasn't heard of yet by the hub's `groupName`) and, for an open plan whose boss is the main one, a "Change" link that opens the existing `GroupChoice`. Its choices are Ungrouped plus each group with no boss of its own (`bossOfGroup(manager, id) === null` and not `isSealed`). A pick calls `store.regroup(plan.id, group)`. A refusal shows in the card's existing refusal line (`refusals`). No new tokens: chips, `chip-row`, `chip-btn`, `link small`, and "Change group" styled as Open on board (`.pc-group-change` joins `.pc-open-board`'s rule in crew.css).
- `PlanCard.tsx` changes in one line only: `<PlanGroup plan={plan} />` inside `pc-meta`, so DW-760's notice-line work doesn't conflict.
- `planStore.regroup(id, group)`, `DeskHub.regroupPlan(id, group)` (`POST /api/desk/plans/{id}/group`), the broker route `route("POST", "/api/desk/plans/{}/group")` in `src-tauri/src/broker/routes.rs`, and its entry in `src/hub/brokered-requests.json`.
- `BossPlan.group`'s doc says what it is now, and `BossPlan.groupName?` is added.
- `hub.ref` pins the hub PR's head.

## Edge cases

- A group the layout doesn't have, a finished plan, the same group again, and a group across a boss boundary are each answered before anything is written.
- A plan whose group is deleted while a start is under way: the start's session is put in a group that no longer exists, which `plan_link_on` and `plan_spawn` already filter out (`layout().groups.any(..)`), so it lands in Ungrouped with its plan.
- A ticket the person moved to another group stays there. Tickets in To do, Backlog or Done have no session to move.
- A machine session's item moves like a local one. Its plan's grant still holds it, because the grant's membership follows the boss's sight, not the plan's group (`plan_sessions`).
- A regroup into a group whose budget is held: the next pass pauses the plan for its budget, as for any plan in that group.
- A boss naming both a group and a board in another scope is refused with `propose_plan`'s words. A group and an adopted ticket in another scope are refused with `adoptable`'s.
- Auto-pick never picks a group the boss doesn't see, and never picks on a tie.

## Tests

D/plan_group.rs (unit):
- `a_tickets_session_then_its_folder_then_its_path_names_its_group`: each signal on its own, and the order between them.
- `the_most_votes_win_and_a_tie_or_nothing_picks_no_group`.
- `a_folder_matches_its_worktrees_and_subfolders_but_not_a_sibling_with_the_same_prefix` (`/a/mantis` vs `/a/mantis-old`).
- `a_group_the_boss_doesnt_see_or_an_empty_name_never_wins`.

D/service/plans_group_tests.rs (new, Postgres):
- `a_plans_group_lifecycle_propose_regroup_start_and_a_deleted_group`: groups Mantis and Atlas, the main boss. `work_group` on a board-less draft in `…/mantis/monorepo` picks Mantis (`Why::Named`). Proposed with that group, the plan and its new board are in Mantis. Regroup to Atlas: the plan is in Atlas, its board and keys are unchanged, and a `Plan` event and a `PlanChanged` note are heard. Approve and start a ticket: its session is in Atlas. Regroup to Mantis: the running session moves to Mantis, and a session the person put in another group stays. Delete Mantis: the plan is in Ungrouped, it is announced again, and its Floor need's group is none.
- `a_plan_doesnt_move_across_a_boss_boundary_nor_to_a_group_there_isnt_nor_once_finished`: Acme bossed by Ada (`set_bosses`), and a sealed group (`bound_groups`).
- `a_start_reads_its_plans_group_as_it_starts`: regroup between the runner's read of the plan and its start; the session lands in the new group.
- `a_plan_takes_on_tickets_and_boards_of_any_group_its_boss_runs`: propose in Mantis on an Ungrouped board, with an adopted ticket on an Atlas board. A board in a bossed group is refused.
- `the_persons_route_regroups_a_plan_and_refuses_what_it_cant`: `POST …/group` with an id, with null, without `group` (400), and across a boss (409).
- `a_plans_machine_calls_carry_its_group_only_when_that_is_a_boundary`.

H/tests/groups.rs (its fixture has bossed Acme and Atlas and open Infra):
- `the_main_boss_puts_a_plan_in_its_works_group_and_moves_it_only_among_its_own`: a plan in `…/infra/web` with no group is in Infra and the note says why; "ungrouped" by name; Acme (bossed) refused with the names it may use ("Ungrouped and Infra"); `crew.plan_update` with group and board refused, with group alone moved.
- `a_running_plan_whose_group_gets_a_boss_passes_to_that_boss`: Mia is made Infra's boss; the main boss's running Infra plan becomes Mia's, paused `NoBoss`.
- `pointed_at` gains `crew.plan` with another boss's group: refused in the scope words, naming only the boss's own groups.

H/tests/plans.rs:
- `a_main_boss_plan_in_an_open_group_starts_on_a_machine_in_its_own_grant`: with the fake relay in H/tests/plans.rs. The `session_start` call carries the plan's key as its group, `desk.sessions` carries none, and its session shows in Infra.

Updated existing tests: plans_adopt_tests.rs' refusal of a ticket on another boss's board (the plan's group is now one with a boss of its own, and the words are the new ones). The ones that set `NewPlan.group` as the boss's group keep their meaning.

A: `PlanGroup.test.tsx`: shows the group, lists only open groups and Ungrouped, sends the pick, picking where it is sends nothing, a refusal keeps the choice open, no picker for a group boss's plan or a finished one, and a group named by the hub alone. `brokered.test.ts` gains `regroupPlan`, and `brokered-requests.json` and the native allowlist (`routes.rs`, checked against it) gain the route.

Targeted test runs only, on the devbox: `cargo test -p workforce-desk plan_group plans_group_tests`, `cargo test -p workforce-hub --test … plans`, and Desk `vitest` for `PlanGroup` and the broker list. CI is the gate.
