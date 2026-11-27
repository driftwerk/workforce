# A plan held by the plan limit says so, and goes on when it lifts (DW-760)

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

Code rules: no TODOs, no partial implementations, no comment that isn't strictly needed, no duplicated code. Idiomatic, functional-style Rust and TypeScript in the code's own patterns. Every behaviour below has a test in "Tests".

G = `crates/hub-core/src/hub_core`, D = `crates/desk/src`, H = `crates/hub/src/desk_crew`, A = the Desk app (`workforce-desk/src`).

## What happens today

- The plan guard (G/plan_guard.rs) holds background work while any live window of the plan its next turn runs on is at `[budget.plan] pause_at_percent` (80) or more. A boss's own turns (its DM wakes, notices, check-ins) are background work: `run_turn_with` (H/deliver.rs) asks `plan_admits`, and on `Admission::Waits(why)` returns `Err(why)`. Nothing is posted and nobody is told. Only a runaway is told, once.
- The plan's runner (H/plan.rs `run_plan`) keeps starting tickets; their sessions ask and notify, and every notice wakes the boss, which is held again. On 24-25 Nov that was 134 holds over 13 hours, and 3 of 14 tickets were done.
- A plan has pause reasons (D/plan.rs `Paused`). `BossHeld` (the breaker holds the boss) already pauses its plans in `run_plan` and is lifted in `lift_hold`. There is no reason for the plan limit.
- `[budget.plan]` is config-file-only. `CostGovernor` reads it once at boot, so the person has no way to change the threshold while Desk runs.

## Decisions

1. **The plan limit pauses the plan, as `BossHeld` does.** A new `Paused::PlanLimit` (`plan_limit`). A plan whose boss's own turns are held by the plan guard starts nothing more ("Nothing will move until …"), its card and the Floor say why, and it goes on by itself when the limit stops holding the boss. A person's pause is never replaced (`pause_plan_saying` already keeps `Person`). A boss held by the breaker keeps `BossHeld`: that check runs first in `run_plan`.
2. **What starts it is a real hold, not a guess.** The plan guard's rule is per turn: the plan of the model that turn resolves to (the session's override, prompt routing). So the runner never predicts. Each time `run_turn_with` judges a boss's own wake, the hub notes what the guard said of it, per boss, in memory (`CrewHub::wakes`):
   `Wake { login: Option<Login>, held: bool, at }`, kept per boss and per login (`CrewHub::limits`, a `Noted`), each newer wake on a login replacing the older:
   - `held` is read off the admission itself: `Admission::Waits(why)` with `why` starting `plan_guard::WAITS_PREFIX`, the guard's own words for the threshold. One source: what the guard said is what is noted.
   - not held: the guard has a reading of that login's plan under the threshold, or the turn is on no plan at all (`login: None`).
   - nothing is noted when the guard has no reading of it (after a restart it is blind until a call reads the plan).
   - only the last hour's wakes count (`recent()`).
   A dollar budget or a runaway that holds the turn is not the plan limit: `plan_holds` is `None`, and the plan isn't paused for it.
3. **Whether the limit still holds is asked of the guard, never polled through `check`.** `check` also judges runaways and records a stop on its first finding, which must reach the real turn. A new pure `PlanGuard::holds(cfg, login, now) -> Option<Held>` answers "does the threshold hold background work on this login now", and `check` uses it, so the rule is in one place. `Held` carries the standing, the threshold and `until`: the window's reset, or (with none said) when the window read lapses (`read_at + minutes`, the guard's own `live` rule). `until` is always known.
4. **The limit is the person's, changeable while Desk runs.** The person's threshold is kept by the desk beside its breaker rules: `state` key `hub.plan.pause_at`, loaded in `breaker_load`, cached in `BreakerRun`, set by `Desk::set_plan_pause_at`. It reaches the governor through the existing runtime path for the person's limits: `Ceilings` (installed by `[desk]` alone) gains `fn plan_pause_at(&self) -> Option<f64>` (default `None`, so other `Ceilings` impls are unchanged), and `CostGovernor::plan_pause_at()` is it when set, else `[budget.plan]`'s, an `f64` passed to `PlanGuard::check`/`holds` (no config is copied per admit). Every plan-guard check, jobs and monitors included, reads it: "your 80% limit" is one limit. A hub without Desk is unchanged. No migration: the `state` table exists.
5. **Pause, tell once, lift, tell again: the runner does it.** Every pass (`run_plans`, every 30 s and on every Desk event):
   - `run_plan`, after the `BossHeld` check: `Limit::Holds(held)` for the boss pauses the plan with `Paused::PlanLimit`, its words, and `until` (`Desk::pause_plan_limit`), and asks the person (6). The runner returns: nothing starts.
   - `lift_hold` for a plan paused `PlanLimit`: `Holds(held)` keeps it paused and refreshes its words and `until` only when they changed (a five-hour window took over from the weekly one, the percentage rose): an unchanged pass writes nothing. Every held pass asks (6), which the key keeps to once a window. `Lifted` resumes it (`resume_plan(id, &[PlanLimit])`), closes its open ask and tells the person (7). `Unknown` (nothing noted since the hub started) resumes only once its kept `until` has passed. A restart doesn't resume it while the guard is blind, and nothing is said twice: the going on of a pause (its plan and `until`) is said once (`Noted::resumed`), so a pause that comes back in the same window and lifts again says nothing new.
   - `Limit` for a boss: held while any login its recent own wakes were held on still is (`plan_holds` again, the fullest); else `Lifted` with the standing of its latest wake on a plan; with no recent wake `Unknown`. Before: a held wake asked `CostGovernor::plan_holds(login, now)` again: `Some` → `Holds`, `None` → `Lifted(standing)`. An unheld wake → `Lifted`. Nothing noted → `Unknown`. `Desk::resume_plan_limit` says whether it resumed, so only a real resume is told.
6. **The person is told once, straight away, in plain words, in the DM and on the plan card.** When `run_plan` pauses a plan for the limit (it wasn't paused for it before) the hub opens an `Ask` item keyed `plan-limit:{plan}:{until}` (`workforce_desk::plan::limit_key`), only if no item ever had that key (`item_ever_keyed`). So it is once per plan per window, across restarts:
   > “Eden” is paused: your weekly ChatGPT plan is at 88%, over your 80% limit. Nothing will move until Fri 09:00 unless you raise the limit.

   Its options are `Raise to 95%` and `Keep` (8). `cant_decide` says "Only you can change your plan limit.", so Act fully still shows the buttons (`OpenItem::offer`). It goes out through `post_item`, the same card and buttons as any needs-you. That is routed by the person's own cadence for needs-you: right away by default, and held in their quiet hours only if they chose that. With no raise left to offer (the plan is at 100%) the same item opens with only `Keep`, its words ending "Nothing will move until Fri 09:00.": one path, said once by the same key. Like every ask, an unanswered one follows the person's reminder setting (`wake.remind_minutes`); it is never said per held turn.
7. **The resume is said.** When the runner lifts a `PlanLimit` pause it closes the plan's open limit asks (`item_close_of_key_prefix(boss, "plan-limit:{plan}:", Lapsed, "The plan limit no longer holds it.")`), so their buttons go dead. Then it tells the DM:
   > “Eden” goes on: your weekly ChatGPT plan is at 12%, under your 80% limit.

   It says "reset at 09:00" when nothing was read since the window reset, and "your limit is 95% now" when the person raised it.
8. **Raise and Keep are answered by Desk, never by the boss.** A boss can't change its own limits (H/own_settings.rs). `answer_item` keeps the limit first (`keep_plan_limit`: only an answer among the item's own options counts; a raise to no more than the limit already is changes nothing and says so; a raise that can't be kept refuses the answer and leaves the item open), then closes the item and says what came of it (`plan_limit_answered`). `goes_to_boss` is false for them:
   - `Raise to N%`: `Desk::set_plan_pause_at(N)`, then lifts at once the boss's plans paused `PlanLimit` that no longer hold (the same lift as 5, without waiting for the next pass). It answers "Raised: background work on your plans now pauses from 95%."
   - `Keep`: nothing changes. It answers "Kept at 80%. “Eden” goes on by itself after Fri 09:00."
   - The raise offered (`raise_to(held)`) is the first of 95 and 100 above both the plan's use and the threshold. A raise that wouldn't lift the pause is never offered.
9. **The card and the Floor say "Paused by plan limit until Fri 09:00".** The hub's words for the pause (`paused_why`, in the person's time zone, `desk.timezone()`) are `by plan limit until Fri 09:00: your weekly ChatGPT plan is at 88%, over your 80% limit`. The desk returns them for `PlanLimit` as it does for `StartFailing`. The app joins them to "Paused" with a space for `plan_limit` and with ": " for every other reason (`pausedLabel`), The chip, the progress line and the Floor's plan line cut the words at ": " ("Paused by plan limit until Fri 09:00"); the card's note and the board row's title say them whole. So the Floor needs the reason: `floor::PlanProgress` gains `paused`. An older Desk shows "Paused: by plan limit until …", still plain words. The time is "09:00" today, else "Fri 09:00".
10. **The plan card carries the ask's buttons.** `PlanSummary` gains `ask: Option<MentionedItem>`: for a plan paused `PlanLimit`, its open item keyed `plan-limit:{plan}:`, as the chat carries it (`mention_at(true)`, so the options show at any level). The card's paused note shows them with the chat's own `Answers` (exported from A/rooms/Overseer.tsx), which answer through `answerItem`, the existing open-item path. Answered in the chat, the card's buttons go (the item is closed). Answered on the card, the chat's do.
11. **The boss warns before the person leaves a plan running.** `context::block` (every boss turn, the person's own included) gains a line from `CrewHub::plan_limit_line` whenever the plan its own wakes run on is within 10 points of the threshold, or held. It reads `limit_of` for the boss: nothing is looked up when nothing is noted.
    - Near: "Your own turns (check-ins, notices, wake-ups) run on your weekly ChatGPT plan, now at 76%. They pause from 80%, the person's limit, until it resets Fri 09:00, and your plans pause with them. If the person asks you to keep a plan going, or to watch one while they are away, tell them plainly it will pause at 80% unless they raise the limit; only they can."
    - Held: "Your own turns are paused now: your weekly ChatGPT plan is at 88%, over the person's 80% limit, until Fri 09:00. Your plans are paused with them. If the person asks you to keep one going, tell them plainly it can't until then unless they raise the limit; only they can."

    The person's own turns are never held, so this reaches them when they ask.

## Changes

### hub-core

- G/plan_guard.rs
  - `pub struct Held { pub standing: PlanStanding, pub at: f64, pub until: DateTime<Utc> }`.
  - `PlanGuard::holds(&self, cfg, login, now) -> Option<Held>`: `None` unless `cfg.enabled`. The fullest live window (as `standing`, from the freshest book) at `cfg.pause_at()` or more. `until` is its `resets_at`, else `book.at + minutes`. `standing` becomes the projection of the same reading (one private `fullest(login, now) -> Option<(PlanStanding, DateTime<Utc>)>` shared by both, so the live-window rule isn't duplicated).
  - `check` uses `holds`: `Verdict::Waits(waits_why(&held))`, and `waits_why` takes `&Held`.
- G/cost_governor.rs
  - `Ceilings::plan_pause_at(&self) -> Option<f64> { None }`.
  - `fn plan_pause_at(&self) -> f64`: the ceilings' when they say one, else `[budget.plan]`'s.
  - `admit` passes `&self.plan_config()` to `check`.
  - `pub fn plan_holds(&self, login: &Login, now) -> Option<Held>`.
  - `pub fn plan_threshold(&self) -> Option<f64>`: the threshold in force (the person's or the config's), `None` while the guard is off; the words the boss and the person read use it.

### desk crate

- D/plan.rs
  - `Paused::PlanLimit` (`plan_limit`, why: "the plan limit holds its boss").
  - `Plan::paused_until: Option<DateTime<Utc>>` (`#[serde(skip_serializing)]`, as `paused_why`).
  - `Plan::unpause(&mut self)` clears `paused`, `paused_why` and `paused_until`. Every place that clears them (resume, pass, approve, continue) calls it.
  - `pub fn limit_key(plan: Uuid, until: DateTime<Utc>) -> String` and `pub fn limit_prefix(plan: Uuid) -> String`.
- `crates/desk/migrations/V097__plan_paused_until.sql`: `ALTER TABLE plans ADD COLUMN paused_until TIMESTAMPTZ;` (additive), and its line in `migrations.lock`. D/plan_store.rs reads and writes it.
- D/service/plans.rs
  - `pause_plan_saying` takes `until`; a pause for the same reason keeps the newer `(said, until)`.
  - `pub async fn pause_plan_limit(&self, id, said: &str, until, by)`. It answers whether the pause is new: `(PlanDetail, bool)`, the bool being that it wasn't paused `PlanLimit` before.
  - `paused_why` returns `p.paused_why` for `PlanLimit`.
  - `PlanSummary::ask` for a plan paused `PlanLimit` (the newest open item keyed from `limit_prefix`, whatever its boss: `OpenItemStore::items_keyed_from`, so a plan passed to another boss still carries it). Absent from JSON when none.
  - `floor_plans` fills `PlanProgress::paused`.
- D/floor.rs: `PlanProgress::paused: Option<Paused>` (skipped when none).
- D/service/breaker.rs
  - `BreakerRun::plan_pause_at: Option<f64>`, loaded from `state` `hub.plan.pause_at` in `breaker_load`.
  - `pub fn plan_pause_at(&self) -> Option<f64>`.
  - `pub async fn set_plan_pause_at(&self, percent: f64) -> Result<f64, DeskError>`: refuses outside 1-100 (`Invalid`), keeps it, then caches it.

### hub

- `crates/hub/src/desk_ceilings.rs`: `plan_pause_at` → `self.desk.plan_pause_at()`.
- H/deliver.rs
  - `wake_login(service, owner, work, on) -> Option<Login>` is the login half of `plan_admits`. `plan_admits` is `wake_login` then `preflight_on`.
  - `held_by_plan` notes the wake (`self.note_wake(agent_id, login, now)`) before matching the admission.
- H/plan_limit.rs (new, `mod plan_limit;`) holds everything the limit does on Desk, so no other module grows:
  - `pub(super) struct Noted { wakes, resumed }`, `struct Wake { login, held, at }`, `enum Limit { Holds(Held), Lifted(Option<PlanStanding>), Unknown }`, `enum Kept { Raised(u8), Already(f64), Unchanged }`.
  - `CrewHub::note_wake`, `CrewHub::limit_of(boss, now) -> Limit`.
  - `CrewHub::held_by_limit(&Plan) -> bool` (pause, ask once).
  - `CrewHub::lift_limit(&Plan)` (lift, close, tell) and `lift_limits` (every plan the limit pauses, after a Raise).
  - `CrewHub::limit_ask_past(&OpenItem)` for `past()`: a limit ask whose plan is no longer paused `PlanLimit` is closed.
  - `CrewHub::keep_plan_limit(&OpenItem, &str) -> Option<Result<Kept, String>>` and `CrewHub::plan_limit_answered(&OpenItem, Kept) -> String`.
  - `CrewHub::plan_limit_line(&AgentService) -> Option<String>`.
  - The pure helpers: `raise_to(&Held) -> Option<u8>`, `answer_of(&OpenItem, &str) -> Option<Answer>` (`Raise(u8)` / `Keep`), `paused_words(&Held, tz)`, `told_words(goal, &Held, raise, tz)`, `resumed_words(goal, Option<PlanStanding>, at, tz)`, `near_words` / `held_words`, `at_time(until, now, tz)` ("09:00" today, else "Fri 09:00").
- H/mod.rs: `limits: Mutex<plan_limit::Noted>` on `CrewHub`.
- Every share is said in whole points by one helper, `plan_guard::points`.
- The app's `Answers` (and what it remembers of answers) moves to `rooms/Answers.tsx`, shared by the chat and the plan card; an answer given in one shows as given in the other.
- H/plan.rs
  - `run_plan`, after the breaker check: `if let Limit::Holds(held) = self.limit_of(&p.boss, now) { self.pause_for_limit(&p, &held).await; return; }`.
  - `lift_hold` calls `self.lift_limit(p, now)` for `Paused::PlanLimit`.
- H/items.rs
  - `answer_item` calls `keep_plan_limit` before closing the item, `plan_limit_answered` after.
  - `goes_to_boss` is false for an answer `answer_of` reads.
- H/context.rs: `plan_limit_line` joins the goals block (never dropped for room).

### Desk app

- A/hub/types.ts
  - `BossPlanPaused` gains `"plan_limit"`.
  - `BossPlan.ask?: MentionedItem | null`.
  - `PlanProgress.paused?: BossPlanPaused | (string & {}) | null`.
- A/crew/plan.ts
  - `PAUSED_WHY.plan_limit = "by plan limit"`.
  - `pausedLabel(p, word = "Paused")`: `${word} ${why}` for `plan_limit`, else `${word}: ${why}`. `resumable` stays false for it.
- A/crew/PlanCard.tsx: the chip, the progress line and the note use `pausedLabel`. A plan paused `plan_limit` with an `ask` shows the ask's `Answers` in its note.
- A/crew/PlanRow.tsx: the chip's title uses `pausedLabel`.
- A/floor/FloorSection.tsx: the plan line says `· ${pausedLabel(plan, "paused")}`.
- A/rooms/Overseer.tsx: `Answers` is exported.
- `hub.ref` pins the hub PR's head.

Styles: no new class. The note, chip and buttons use the card's existing `pc-note`, `chip pc-chip-blocked`, and `dm-inline` / `btn btn-sm` from `Answers`.

## Edge cases

- **A new window holds the plan** (the week reset into a five-hour window over the limit): its ask of the older window is closed as lapsed before the new one is asked.
- **A boss whose wakes run on two plans:** held while either login it was held on in the last hour still is, so its plans don't flip between paused and going on.
- **A plan's group gets a boss of its own while the limit pauses it** (DW-754's `follow_group`): it isn't handed over while paused `PlanLimit`. It stays the boss's whose limit holds it, its ask on its card, and goes on when that lifts; then, running, it passes as any running plan does (paused `NoBoss` for Continue).
- **A plan passed to another boss while the limit pauses it:** a paused plan keeps its reason through `pass_plan`; its ask is found by key whatever its boss. While the new boss's own wakes say nothing (`Unknown`), it never goes on by the old boss's window: once that lapses, a pause whose ask was never the plan's boss's (`item_ever_keyed(boss, limit_key)`) becomes `NoBoss`, waiting for the person's Continue as a plan passed on does, and nothing is said of it going on.
- **Two plans of one boss:** each is paused and told, by its own key. One Raise lifts both: the threshold is the person's.
- **The plan is at 100%:** no raise is offered; the ask has only Keep. It resumes once the window resets.
- **The threshold is raised while another window of the plan is also over it:** `holds` reads the fullest live window, so it stays paused, with new words, and a raise to 100 is offered next time (a new key, since `until` differs) only if the window changes. The Raise answer says the new threshold; the plan is lifted only if nothing holds it.
- **The window resets with no call since:** `holds` finds no live window (its reset has passed) → `Lifted` on a noted `Held`, and `until` passed on `Unknown`.
- **The person paused the plan first:** `pause_plan_saying` keeps `Person`. No item is opened, because `pause_plan_limit` says the pause wasn't new as `PlanLimit`.
- **The boss moved to another model or plan with room:** the next own wake is `Ran` → `Lifted`.
- **The plan is dropped or done while paused:** `lift_hold` only sees paused plans. Its ask is closed by the standard `past` sweep: `limit_ask_past` closes a limit ask whose plan is no longer paused `PlanLimit` (resumed, dropped, done or gone).
- **The breaker holds the boss too:** `BossHeld` wins (checked first). When it lifts, the next pass pauses for the limit if that still holds.
- **The hub restarts while paused:** the pause is kept with `until`. No word is said twice: the key is in the item store, and resume words only follow a real lift.
- **A Raise on an item whose plan was already lifted:** the threshold is still kept (they chose it), and the lift is idempotent (`resume_plan` with `PlanLimit` on a running plan is a no-op detail read).
- **A Raise to a value at or below the current threshold** (a stale card): kept as asked. It never lowers below what the button said; `set_plan_pause_at` keeps exactly N.

## Tests

hub-core
- `plan_guard`: `holds` at, under and over the threshold. `until` from `resets_at` and from the read time. `None` when disabled and once the window reset. `check` waits exactly when `holds` is `Some`.
- `cost_governor`: a `Ceilings` with `plan_pause_at` 95 lets an 88% plan's background work run, and `plan_holds` follows it. With none, 80 holds.

desk
- `plan_tests` / `plan_store`: `Paused::PlanLimit` round-trips with `paused_until`. `unpause` clears all three.
- `service/plans`: `pause_plan_limit` is new once, then refreshes words and `until` without being new. A person's pause is kept. Resume clears `paused_until`. `PlanSummary::ask` is the open `plan-limit:` item only.
- `breaker`: `set_plan_pause_at` refuses 0 and 101, keeps 95 across `breaker_load`.

hub
- `plan_limit` units: `raise_to` (88/80 → 95, 96/80 → 100, 100 → none, 97/95 → 100). `answer_of` reads "Raise to 95%" and "keep" and nothing else. The words for the ask, the notice, the paused line, resume (under, reset, raised) and the two context lines. `at_time` today / another day.
- `tests/plans.rs`, beside the `BossHeld` test:
  - a boss whose own wake is held on a ChatGPT plan at 88% pauses its running plan `plan_limit` with "by plan limit until …". One ask with `Raise to 95%` / `Keep` is in its DM, and a second pass and a second held wake open no other.
  - a new reading at 12% resumes it, closes the ask, and says "goes on" once.
  - `Unknown` after a restart keeps it paused until `until` passes, then resumes it.
  - the plan is never paused for a dollar-budget hold.
- `items`: "Raise to 95%" on the ask sets the desk's threshold, lifts the plan at once, and never reaches the boss. "Keep" closes it and changes nothing.
- `context`: the near line at 72% of 80 and none at 60%. The held line while held.

Desk app (vitest)
- `plan.test.ts`: `pausedLabel` for `plan_limit` and others.
- `PlanCard.test.tsx`: a `plan_limit` pause shows "Paused by plan limit until Fri 09:00" and the ask's two buttons, and a press calls `answerItem(id, "Raise to 95%")`.
- `FloorSection.test.tsx`: "· paused by plan limit until …".
- `PlanRow` title.
