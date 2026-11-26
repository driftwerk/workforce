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

Revision 2 takes in the independent review of #532 (findings 1-14). Revision 3 takes in its re-review (2, N1-N8).

1. **One wait, shared.** `crate::live_turns::when_free(service, session, within, stop, attempt)` runs `attempt` and, each time it fails because another turn holds the session (`session_turn_claim::held`, checked with `chain().any` for `TurnBusy` or `TurnClaimHeld`), waits for the session to be let go, then tries again.
   - Letting go means the local live turn ending (`LiveTurns::ended`, hub-core, awaiting the entry's `settled` token), then no claim on the session (`turn_claim_holder` is `None`, looked at every 500 ms on the blocking pool, as the store's read is synchronous Postgres).
   - So a retry never sets up a turn (live-turn entry, NATS control subscription, crew wake, typing frame, turn-end hooks) while a claim still holds the session.
   - It gives up past `within`, or when `stop` resolves.
   - The crew passes `GatewayState::drain_token` as `stop`. `answer_left` passes `shutdown::begun()`: the shutdown flag is a process-wide `CancellationToken`, cancelled where the drain token is, so a service with no gateway state still stops waiting when the drain begins.
2. **The busy refusal is typed.** `run_turn_with` returns `TurnBusy` as an error, not a string.
   - `hops` and `listed` are inserted only once `begin_live_turn` has succeeded.
   - A refused turn therefore never removes the hop of the turn that won (`MAX_HOPS` holds), and never leaks `listed`.
3. **`CrewHub::run_dm_turn_queued(service, owner, turn)`** is `when_free` on the boss's DM, looked up again each time, so a rollover is followed. Its bound is the mind's wait (`mind_wait_max`, the one knob tests shorten); its stop is the hub draining. `turn` is a closure, so each caller builds its own turn.
4. **A button answer** (`answer_delivered`, spawned by `answer_item`) goes into the turn running in the DM first, the way the person's typed follow-up does.
   - It is sent as `TurnControl::FollowUp` through `gateway::steer`, local or over NATS, with place DM and `queue: false`, and goes into the boss's picture.
   - Its id is the answer's own (`answer_id`: a v5 UUID of the item id and its `closed_at`), the same on every try and new each time the item is answered. The turn keeps the follow-up under that id, so a try after a NATS reply that came too late (`CONTROL_WAIT`) finds it in the DM (`reached`) and doesn't say it again.
   - Once it is taken in, or its own DM turn ran, the item is noted delivered (`OpenItemStore::item_delivered`, `delivered_at`).
   - Only when no turn takes it in (no live turn, a turn in another place, a turn already answered and wound up) does it run as a DM turn of its own, through `run_dm_turn_queued`.
   - Clicks during a turn are therefore batched into that turn, in the order they land. A click followed by typed "hold off" reaches the boss in that order.
   - The message text is unchanged (`ANSWER_LEAD…”: answer`), so `answered_text` still reads it.
5. **A follow-up injected into a crew turn is the person's.** `next_turn::heard_in` gives a turn on the DM channel with no Desk context (the crew's own turns) the person's DM turn as its context, and the person's trust (`None`, as the hub's other turns for the person) rather than the crew turn's `Paired`.
   - Its follow-ups are marked as theirs, kept at their trust, and the next turn they make runs at it.
   - One the run never read becomes the DM's next turn (`answer_left`), as it already does for a Desk turn, instead of being kept unanswered.
6. **`answer_left`** waits through `when_free` for up to 20 min, event-driven, instead of 40 tries 250 ms apart. A queued turn that takes the session first no longer leaves the person's follow-up unanswered.
7. **Settled meanwhile.** Before each try, and before reopening, the item is read again (`still_owed`). An answer whose item no longer stands as it closed (state, `closed_why` and `closed_at`), or whose proposal was applied (`OpenItemStore::item_applied`), goes no further: it is neither delivered nor reopened.
8. **Restarts.**
   - The answer's task holds a `shutdown::Settling` guard. The CLI's drain waits on `shutdown::settling()` alongside in-flight turns, so the reopen-and-tell that a drain triggers completes before the process goes.
   - A hard kill is covered at the next start. `items_loop` first runs `answers_lost_before(STARTED)`, `STARTED` being the process's first crew start (a `LazyLock`), so a crew started again in the same process never sweeps answers this process holds. It reads `OpenItemStore::items_answered_between(STARTED − mind wait, STARTED)`: items the person closed with `You answered: …`, nothing applied, not delivered, and no open item of the same key (that question is asked again anyway, so no repeated "didn't reach" after each restart).
   - Each one whose answer is for the boss (`goes_to_boss`: not a session's wait, not a line, not a preference kept or forgotten via `preferences::overrule_of`), not on its way in this process (`DELIVERING`, the answers `answer_item` has spawned, by `answer_id`, held by a guard; it covers a hub and Postgres clock apart), and not in the boss's DM since it was given (`reached`: by id or words) goes to `answer_lost`.
   - Schema: one additive migration, `V095__answer_delivered.sql` (`boss_open_items.delivered_at`). `item_reopen` clears it.
9. **Crew messages** (`message_agent`) use `run_dm_turn_queued` on both branches.
   - The person's go-ahead is queued the same way. If it can't be delivered, the person is told in that agent's DM.
   - Another boss's message that finally can't be delivered is told to the sending boss: a `picture::What::Undelivered` row (boss: the sender, no session), which its every turn reads among what the person told it ("your message to Scout “…” never reached it…").
   - Both hold a `shutdown::Settling` guard.
   - Another boss's message takes the receiver's mind inside each try, so the wait for the DM is outside the mind and never holds it.
   - The watcher's check-in (`watcher.rs`) is left as is.

## Edge cases

- **Several clicks during a turn.** All go into that turn, unless it can't take them. Queued ones then each wait, and the first to run opens a turn that the rest join.
- **A turn from another place, or one already answered**, refuses the follow-up. The answer then waits and runs as the DM's next turn.
- **Another instance holds the claim.** The follow-up reaches its live turn over NATS. If that instance has no live turn (a stale claim), the answer waits for the claim to go.
- **No retry duplicates a message.** A busy refusal comes before anything is saved.
- **Restart sweep, rollover and multi-instance.** A delivered answer is noted on its item, so a DM rollover never makes it look lost. One saved but not yet noted (a kill in between) is still found in the current DM. The crew runs where Desk runs, a single hub, so no other instance holds queued answers.
- **A kill between the DM save and the note.** If the DM rolled over in that window, the answer is reopened: a repeated question, never a lost one.
- **The same item answered again.** A new `closed_at` gives it a new `answer_id`, and the older answer's tries see the item no longer stands as they knew it.

## Tests

- hub `live_turns`:
  - `let_go` waits for the live turn's end, then for the claim's release;
  - `when_free` retries after `TurnClaimHeld` and after a busy turn;
  - it gives up past its wait, or when stopped;
  - it never retries a real failure.
- hub-core `live_turns`: `ended`.
- `turn_loop`:
  - `answer_left` waits out a turn holding its session;
  - `heard_in` makes a crew DM turn's unread follow-up the DM's next turn.
- `desk_crew/tests/goals.rs`:
  - clicks behind a turn that can't take them all reach the boss once it ends, with no bounce;
  - clicks into a turn that takes follow-ups join it, ahead of words typed after them.
- `desk_crew/tests/items.rs` (shared `answered` and `reopened` helpers):
  - lost answer: the item reopens and the person is told;
  - busy past the wait, or a drain: reopened and told;
  - an applied ask goes no further;
  - the start sweep reopens a lost answer, and leaves one the DM holds and a preference's answer alone;
  - a delivered answer stays answered after its DM rolls over;
  - an answer already taken in under its id isn't said again; answered anew it has a new id, and `item_reopen` clears the delivery;
  - an answer in flight, one a crew restart in the process would sweep, and one whose question is open again are never bounced; the in-flight one arrives once the turn ends;
  - an answer given again since (same words, new `closed_at`) is no longer owed.
- `desk_crew/tests.rs`: an overseer's message that can't reach the agent is a `What::Undelivered` row in the overseer's picture, and holds the drain.
- `turn_loop`: `heard_in` gives the person's trust for a crew DM turn, the turn's own otherwise; `keep_heard` keeps those follow-ups with the person's Desk mark, id and trust.
- desk `picture`: every `What` round-trips.
- The claim paths are covered through `let_go`/`when_free` with a claim seam. The hub tests' session store always grants claims, so the whole crew path can't hold a claim there.

## Rules

- No TODOs and no partial implementations.
- Idiomatic, functional-style Rust that follows the crate's patterns: typed errors downcast with `chain`/`downcast_ref`, `tokio::select!` bounds, the existing `eventually` test helpers.
- No comments beyond what is strictly needed. No duplicated logic: one `held`, one `when_free`, and reuse of `answer_lost`, `run_dm_turn` and `gateway::steer`.
- The Desk app is unchanged: `answer_item` still answers "Answered." at once.
