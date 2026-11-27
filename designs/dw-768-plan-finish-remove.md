# A finished plan finishes, and a plan can be ended or removed (DW-768)

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

Code rules:
- No TODOs and no partial implementations.
- No comment that isn't strictly needed, and no duplicated code.
- Idiomatic, functional-style Rust and TypeScript, in the crates' and the app's own patterns.
- Every behaviour below has a test under "Tests".

D = `crates/desk/src`, H = `crates/hub/src/desk_crew`, A = the Desk app (`driftwerk/workforce-desk`).

## What happens today, and why 23 of 23 stayed Running

The only way a plan becomes Done is its boss's closing turn, `plan_turn_now` (H/plan.rs). That turn runs only when something about the plan is *gathered* in `plan_loop`. Gathered means one of these:
- a ticket line from a `DeskEvent::Ticket` whose state the loop saw change;
- the person's own plan action (`plan_line`);
- a failure note;
- the restart turn.

Nothing looks at a plan's state. A plan settles for good, and is never closed, when its last ticket's change produces no turn that can close it. Each of these does that:

1. **A goal plan whose goal is judged.** Since DW-728 (23 Nov) `crew.plan` requires a `result`, so every newer plan is a goal plan. Its closing needs `p.met.is_some() || settled && p.goal_done()`, and `goal_done()` is `met.is_some()` for a plan with a result. Only a met reading or the person's words set `met`. A judged part is never read met. Moving the goal ticket to Done by hand or by Linear sync doesn't set it. So a judged goal whose tickets are all Done gets one `settled_unmet` line in the boss's next turn, and then stays Running for ever. `finish_plan` refuses the same plan: "Its goal isn't met yet".
2. **Paused when the last ticket settled.** Closing needs `p.state == Running`. Desk's own resume (`resume_if_lifted`, the Restoring and plan-limit lifts) isn't a turn trigger: `plan_line` reacts only to the person's actions. So nothing closes it afterwards.
3. **Settled while the hub was down.** `plan_states` seeds the states as they are at boot, so no ticket line is ever made. `reconcile_plans` says only Doing tickets whose session ended.
4. **Lagged events.** A bulk Linear sync announces many tickets at once. Desk's event channel holds 256, and the loop awaits a store read per ticket event. On `RecvError::Lagged` it only marks itself dirty, so the ticket lines (and with them the closing) are lost.
5. **No boss turn possible.** `plan_turn_now` returns before closing, and so never finishes the plan, in any of these cases:
   - `boss_of_plan` is none (its boss doesn't oversee the plan's group now);
   - the boss is at Watch;
   - the boss has no person.

The code proves all five possible. The live Desk's database isn't read for this, and hub.log has no goal or settled lines for the plan, so which one hit "Mantis bugs" can't be shown; 1 is taken as the working cause, being the only one that strands a plan even when every event is heard. The fix covers all five: a runner pass closes by state, and the log lines below make the next one visible.

There is also no way to end or remove a plan from its row:
- `PlanRow` has only Pause and Resume.
- Drop is on the open card only, and it drops every ticket not done.
- Nothing deletes a plan.
- `BoardPlans` keeps dropped plans off the board entirely (`plansOn`) and shows done ones only while opened.

## Decisions

1. **A plan finishes by its state, every runner pass, not by an event.** `run_plans` looks at every running and paused plan (a cheap check of its summary first: its goal met, or every ticket but the dropped done) and starts the closing turn of each that `plan::closing` says is closing. The closing turn and `finish_plan` read the same rule. A missed event, a pause by Desk, a restart, a lagged event stream or a boss out of reach can't strand it: the next pass sees it.
2. **What closing means** (`plan::closing`, one pure function in D/plan.rs, answering `Option<Closing>`):
   - the plan is Running, or Paused by Desk (its budget, its boss held, failing starts). A plan the person paused is never finished under them; resumed, it closes on the next pass. Proposed, Done and Dropped never close.
   - `Closing::Now`: its goal is met, or every ticket is settled (Done or Dropped, `plan::settled`) and it has no goal.
   - `Closing::AfterTurn`: every ticket is settled and its goal isn't met (judged or measured alike).

   With `AfterTurn` the boss has one turn: it is told the tickets are settled but the goal isn't read met (`settled_unmet`, with where each part stands), and that Desk finishes the plan once the turn ends if nothing more is to be done, its reply then the closing summary leading with "Not read met: …". After the turn, if the plan is still `AfterTurn` (the boss added no ticket and nothing met the goal) and no question of the boss's about it is open, Desk finishes it as Done, `met` left unset. A question still open holds it: a runner pass starts no turn for it while the person hasn't answered. An info log line says when a plan is settled with its goal unmet, and when a plan finishes (`a plan finished`, with who finished it), so the next stall is visible in hub.log.
3. **Closing goes through one path, whatever can or can't take a turn.** The changes to `plan_turn_now`'s closing:
   - It runs the boss's closing turn when its boss can take one.
   - Otherwise Desk finishes the plan itself. "Otherwise" is any of: no boss for the plan, the boss at Watch, no person, or the boss held by the breaker (which today finishes it silently).
   - `finish_plan` says whether *this call* made the plan Done (`Option<PlanDetail>`, none when it was Done already).
   - Only on that transition is the person told, once: the boss's closing reply in its DM (`post_plan`), or, when there was no reply or no turn, Desk's own plain words in that DM, e.g. `“Mantis bugs” is done: 23 of 23 tickets done.` With no boss at all there is no DM, and the Floor's `plan_done` activity says it.
   - Then the closer looks at once (`look_at_finished_sessions`), so the plan's finished sessions close through the one close gate (`closing.rs`). That gate covers: in the boss's charge and started by it, at rest for `SETTLE`, nothing asked or waiting, the person not in it for 2 hours, not reopened, nothing uncommitted or unpushed, and the boss's level. Paused sessions, and sessions the person is in, stay. Nothing else closes sessions.
   - The closing set (`GoalChecks::closing`, renamed in meaning to "plans whose closing turn started") keeps one closing turn per plan per hub run. Restarts are safe, because the transition, not the turn, decides the telling.
4. **What a plan leaving the running set does, once, in Desk** (`plan_closed`, shared by finish, drop and end, in D/service/plans.rs):
   - its budget ends (`plan_budget_ends`, as today);
   - its open watches end (`watches_end_plan`, "its plan ended"), so the boss stops watching at once instead of at the watch's next poll;
   - its open asks (`boss_open_items.plan = id`, not closed) close as the boss's, "its plan ended".
   The hub forgets what it keeps per plan when it hears the plan closed or removed (`forget_plan`): the goal checks' `closing`, `pulled`, `pulls`, `heads` and notes for it, and `plan_failed`'s entries of its tickets (kept again). `unmet_said` goes: the settled-unmet line is said in the one turn before the plan finishes, so it needs no memory of having been said.
5. **End plan reuses the `dropped` state; it is not a new state.** `dropped` already means "stopped before it was done" everywhere: watches, adoption (`ADOPTABLE`), the boss's context, Floor, budgets. What becomes of the tickets is the action's, not the state's. A new state would need every `Done | Dropped` site, both SQL `IN ('done','dropped')` clauses and the app's unions to learn it, which is exactly the class of miss this ticket is about.

   `Desk::end_plan(id, by)`, for a Running or Paused plan:
   - every start under way for its tickets is ended (as drop does);
   - each ticket it took on and isn't Done is let go where it is (`plan_let_go` with `OnDrop::Leave`, as drop does);
   - the live sessions of its own tickets in Doing or Review are let go from the boss's charge, to finish as the person's (`OnDrop::Leave`'s behaviour);
   - its tickets are left exactly where they are: no column changes;
   - then `dropped`, `plan_closed`, and the note `ended the plan, leaving its tickets as they are: …` (`plan_dropped`).
   Ending one already dropped answers it as it is. Done or proposed is a 409 ("Only a running or paused plan can be ended."). Drop is refactored to share the start-cutting and let-go steps with end, with no copy.
6. **Remove, in one transaction, for a Done or Dropped plan only** (`Desk::remove_plan`, `DeskStore::remove_plan`). Anything else is a 409: "End the plan first: only a finished or ended plan can be removed."
   - Every ticket still the plan's is let go as `release_ticket` does: run columns, `adopted` and `held_back` cleared. The tickets stay on their board: they are the board's record, synced issues included.
   - The needs the plan added are removed, except one its issue has upstream too. That rule is the one `plan_let_go` uses, computed by the same function (`plan_unneeded`, extracted from `plan_let_go`). A kept need's `plan` is cleared.
   - `boss_watches.plan` and `boss_open_items.plan` are cleared for it (they ended and closed when it did).
   - The plan row is deleted; its revisions, readings and facts cascade.
   - Desk then forgets its awaiting card and breaker subject, announces each released ticket and `DeskEvent::PlanRemoved { id }`, and notes `removed the plan: …` (`plan_removed`, a new `Kind`). A handover note naming `plan:<id>` resumes nothing once it is gone: `resume_plan` answers NotFound and logs, as today.
7. **The person's routes:** `POST /api/desk/plans/{id}/end` and `POST /api/desk/plans/{id}/remove`.
   - `end` answers the plan; `remove` answers `{ "removed": "<id>" }`.
   - They are POSTs like pause, resume and drop. The broker's negative tests keep `DELETE /api/desk/plans/p1` and `…/finish` refused.
   - Both go on the native broker allowlist (`routes.rs`, `brokered-requests.json`, `brokered.test.ts`).
8. **The boss's way: `crew.plan_update` with `end` or `remove`**, each `said_schema` (the person's words asking). That extends the existing grammar rather than adding a tool.
   - `remove` on an open plan ends it first.
   - Each takes only `plan_id`, `why` and itself.
   - The quote is checked in the person's DM with a new `Said::Asking`: the whole of one message counts however short ("yes, remove it"), or at least 2 words and 6 characters of one, within 24 hours. It reads as a request, not consent.
   - An unbound boss at Act fully may end or remove its own plan without a quote. This follows "Act fully = no restraints", and the note says it did.
   - `crew.board` lists finished plans under `finished` (id, goal, state), so "remove the Mantis bugs plan" finds the id.
9. **The board (A):**
   - `BoardPlans` shows the open plans by default (proposed, running, paused), plus the one opened.
   - A `chip-btn` "Finished · n" with `aria-pressed` (the `search-filters` pattern) shows the done and ended ones instead.
   - `plansOn` keeps dropped plans now. The board's plan select and its tickets' plan chips leave out ended plans, but the one shown.
   - The row's actions live in a new `PlanRowMenu` (so DW-760's `PlanRow`/`PlanCard` edits merge cleanly): a ⋯ `icon-btn-sm` (`MoreIcon`, `Menu`).
     - Running or paused: "End plan…", which opens `Confirm` (tone danger): "End “goal”? Its tickets stay where they are, and its sessions working now carry on as yours. It can't be resumed." Action "End plan".
     - Done or dropped: "Remove", one click, no confirmation: nothing is lost, since its tickets stay on the board.
   - `planStore` gets `end`, and `remove`, which forgets the plan in `plans`, `details`, `unread`, `refusals`, `cards`, `seen` and `looking`, and closes it on the board if shown.
   - `planStore` also handles `planRemoved` the same way.
   - `hub/types.ts`: the `planRemoved` event. `client.ts`: `endPlan`, `removePlan`.

## Tests

D (Postgres, on CI), in `service/plans_end_tests.rs` (new):
- `plan::closing` (pure): running settled with no goal, and paused by Desk: Now; paused by the person, proposed, done, dropped, tickets open, no tickets: none; a goal unmet, settled: AfterTurn; met, open or settled: Now.
- **lifecycle:** a plan runs, its last ticket goes Done, `finish_plan` makes it Done once (a second call answers none), its watch is ended and its open ask closed; remove lets its tickets go (still on the board), and leaves no `plan_revisions`, `boss_watches.plan`, `boss_open_items.plan`, `ticket_needs.plan` or `tickets.plan` row for it; `PlanRemoved` is announced; it is gone after.
- end of a running plan leaves Todo, Doing and Done tickets where they are, its session's charge let go, its watch ended; end twice answers once; a later plan adopts its Todo ticket.
- end of a proposed or done plan is refused; a plan the person paused isn't finished (`finish_plan` refused), one paused by Desk is; a paused plan ends with its tickets left; remove of a missing plan is NotFound.
- a goal not read met, settled, is `AfterTurn`, and `finish_plan` finishes it with `met` unset.
- `plan_api` routes: `end` and `remove` answer 401, 409, 200 and 404.

H:
- `tests/plan_finish.rs` (new):
  - a plan settled before the crew ran finishes on a runner pass, its closing summary asked once over further passes;
  - a plan the person paused stays Paused over passes while one Desk paused finishes; resumed by the person, it finishes;
  - a boss at Watch: Desk finishes it with its own line in the DM, once, and the boss takes no turn;
  - `crew.plan_update end` is refused without the person's words, with words they never said, and with other changes in the call; with their words it ends the plan leaving its tickets; `crew.board` lists it under `finished_plans`; end again is refused, pointing at remove; remove with their words takes it off, its tickets kept as no plan's.
- `tests/goals.rs`: a goal unmet with every ticket settled: its turn is told `settled_unmet` and "Not read met:", the plan finishes, and a later turn finds nothing to do.
- `tests/api/desk_plan_routes.rs`: `end` and `remove` are 404 on a hub without Desk.

A (vitest), in `crew/plans.e2e.test.tsx` against its fake hub:
- a running plan's ⋯ offers "End plan…", which asks first and ends it, its tickets as they were; ended, it leaves the list, a "Finished · 1" chip shows it, its ⋯ offers only Remove, and Remove (announced as `planRemoved`) takes it off the board, the store forgetting it;
- a plan that finished while the person was asked to end it closes the dialog and shows Done under Finished.
- `planFrom.test.tsx`: `plansOn` keeps an ended plan.
- `brokered.test.ts`: the two new routes.
