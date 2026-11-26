# A button answer waits for the boss's turn instead of bouncing (DW-763)

## Brief

Please design a full solution correctly and ensure you don't miss anything or cause any bugs and you handle any edge cases.
Please design a robust, elegant, production quality engineering solution that employs idiomatic code using a functional programming style and follows the existing codebase tech patterns.
Please make sure to design the most elegant and best solution possible from both a code and an architectural perspective
Please ensure you specify that there should be no todos or partial implementations and make sure to design appropriate tests.
Please ensure the design specifies to not write any comments that are not absolutely needed and to not create excess code duplication.

## Root cause

`CrewHub::answer_item` (`crates/hub/src/desk_crew/items.rs`) closes the item, then spawns one `run_dm_turn` carrying the person's answer. `run_turn_with` (`deliver.rs`) begins a live turn on the boss's DM session. `LiveTurns::begin` refuses a second turn on a session that already has one (`TurnBusy`), so while any turn runs in that DM the answer's turn fails at once. That turn might be the person's own typed turn, a wake-up, a check-in posted to the DM, or a relayed message. Across instances, `TurnClaimHeld` refuses it the same way. The spawned task treats every error as final and calls `answer_lost`: the item reopens and the person is told "didn't reach … answer it again". So the click is safe but wasted.

`run_turn_with` also turns `TurnBusy` into a string (`bail!("{busy}")`), so a caller can't tell a busy DM from a real failure.

## Design

The answer waits its turn in the background, as `turn_loop::next_turn::answer_left` already does for an unread follow-up. It reaches the boss as a turn of its own in the DM once the DM lets go.

1. **The busy refusal is typed.** `run_turn_with` returns `Err(busy.into())` instead of `bail!("{busy}")`. The message reads the same in logs. `next_turn::held` (TurnBusy or TurnClaimHeld) becomes `pub(crate)` and is the single test for "another turn holds the session". It is reused, not copied.
2. **`LiveTurns::ended(session_id)`** (hub-core) resolves once no turn is live for the session on this instance, or at once if none is. It awaits the entry's existing `settled` token, which `LiveTurn::drop` cancels after unregistering. No polling is needed on a single instance.
3. **`CrewHub::run_dm_turn_queued(service, owner, prompt, author, within)`** (`deliver.rs`): it runs `run_dm_turn` and, while the error is `held`, waits for the DM to let go and tries again. The wait is `LiveTurns::ended` when the turn is live here, or a short recheck sleep when another instance holds the claim. It stops at the first of these:
   - the turn ran: its result is returned;
   - a non-busy error: returned as is;
   - `within` passed: an error saying the DM stayed busy;
   - the hub drains (`GatewayState::drain_token`): an error saying the hub is shutting down.
4. **`CrewHub::answer_delivered(service, owner, closed, answer, within)`** (`items.rs`) is the body `answer_item` spawns. It builds the `ANSWER_LEAD` message, runs `run_dm_turn_queued` with `Speaker::you()`, and calls `answer_lost` only when that fails. `answer_item` spawns it with `mind::WAIT_MAX`, the bound everything else already uses for a busy boss.

The person's answer is their own turn. It never takes the boss's mind and is never held by the plan guard. `persons` is already true for `Speaker::you()`, so nothing changes there.

## Edge cases

- **Several answers while busy.** Each waits on its own. After the DM lets go, one wins and the others see busy again and wait again. Every answer is delivered. They are answers to different items, so no order between them is promised and no per-boss queue state is added.
- **The person types meanwhile.** Their turn and a queued answer race for the free DM. Whichever loses waits. Neither is lost.
- **The DM rolls over while waiting.** Each attempt resolves the DM through `get_or_create`, and the wait looks the session up again with `sessions.find`.
- **No retry duplicates a message.** A busy refusal comes before the prompt is saved: `begin_live_turn` refuses it, and the claim is checked before the turn records anything, which `answer_left` already relies on.
- **The hub restarts mid-wait.** The wait ends as soon as the drain begins: the item is reopened and the person is told, through `answer_lost`. The process keeps running while in-flight turns drain, which is far longer than the reopen takes.
  - Waiting widens the window in which an answer is in flight from an instant to up to `WAIT_MAX`.
  - A hard kill (crash, SIGKILL) inside that window still loses the answer: the item stays closed as answered. Making the answer itself durable would need a delivery state on open items. That is out of proportion to this fix and is not added.
  - `answer_lost`'s wording ("didn't reach … just now") still holds after a long wait: it says when the delivery gave up.
- **The boss stays busy past the bound, or the agent is gone.** `answer_lost` runs as today: the item reopens when nothing of its key opened since, else the person is asked to say it again.
- **Other paths do not change.** A session's question (`answer_wait`) and a line ask (`line_answer`) go to the session, not the DM, so they keep their behaviour.

## Tests

- hub-core `live_turns`: `ended` resolves at once with no turn, stays pending while one is live, and resolves once it drops.
- `desk_crew/tests/goals.rs`: the old `a_click_the_boss_cant_take_reopens_its_item_and_says_so` becomes `a_click_while_the_boss_is_mid_turn_reaches_it_once_the_turn_ends`. It holds a live DM turn, clicks, and checks that the item stays closed and nothing says "didn't reach". It then drops the turn and checks that the DM gets the person's answer message (`ANSWER_LEAD…: Merge`).
- `desk_crew/tests/items.rs`: with the DM held, `answer_delivered` with a short `within` reopens the item and tells the person. With a long `within`, cancelling the drain token does the same.

## Rules

- No TODOs and no partial implementations.
- Idiomatic, functional-style Rust that follows the crate's patterns: typed errors downcast with `chain`/`downcast_ref`, `tokio::select!` bounds, the existing `eventually` test helpers.
- No comments beyond what is strictly needed. No duplicated logic: reuse `next_turn::held`, `answer_lost` and `run_dm_turn`.
- The Desk app is unchanged: `answer_item` still answers "Answered." at once.
