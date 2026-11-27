# An agent elsewhere hands "keep this on track" to the person's Desk boss (DW-770)

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

Code rules: no TODOs, no partial implementations, no comment that isn't strictly needed, no duplicated code. Idiomatic, functional-style Rust in the code's own patterns. Every behaviour below has a test in "Tests".

K = `crates/hub/src/desk_knows`, H = `crates/hub/src/desk_crew`, D = `crates/desk/src`.

## What happened (25 Nov, 18:44 UTC)

Kingsley, in his Slack DM with Allan (an agent on the Driftwerk hub), asked "ok keep that video creation on Track in desk please". Allan said the tracking connection failed and asked him to type the goal into Desk himself.

What a remote agent can reach of the person's Desk, from the code:

- **Sees (K)** is one way. The person's own computer pushes what an agent may know (`POST /api/desk-knows/push`, `wfk_` token) every 30 s. The agent reads it with `desk_work.overview/session/search/transcript`, only in the person's own conversation (`is_own_conversation`: their screens, or a Slack DM, a DM thread included). The hub has no route that sends Desk anything.
- **Remote bosses (`crates/hub/src/boss_links`)** send `crew.*` down an engine's uplink only to an agent the person made the boss of a Desk group, in that group's boss conversation. Allan is not Kingsley's boss, so none of this applies to him.
- **`watch`** polls a closed allow-list of read-only tools. `desk_work.*` isn't on it, so it is refused with "`desk_work.session` is not one of them (it may change state, access credentials, or be an external/MCP tool that cannot run unattended)". Its description says "never promise to watch something without calling this", and Allan's "I won't pretend it's being monitored" echoes it. `crew.*` returns `CREW_WATCH`, "Desk already wakes you for every session you oversee", which is untrue for an agent on another hub.
- **`monitor`** with `surface = "desk"` on a hub without Desk is refused: "this hub doesn't run Desk … save it on the hub your Desk runs".
- **The outside-content guard (DW-714)**: in that same conversation, reading `desk_work.session` armed the guard (DW-767 PR #537 names Allan, 25 Nov). Every unvetted call after it (`watch`, `scheduler`, `monitor` included) was held for the person.

**Likely root cause.** No supported call exists that hands Desk anything. Every path Allan could try (`watch` on `desk_work.*`, `watch` on `crew.*`, `monitor` told in Desk, a `scheduler` job) either can't reach Desk at all or is refused, some of them after a guard hold. "The tracking connection failed" is the model's own wording of one of those refusals. It is not a network failure: no code path makes a connection from the hub to Desk.

**What confirms which call it was** (production, not read here):

- The tool calls of Allan's turn, from the sessions store. Run against the Driftwerk hub's Postgres, sessions schema:
  ```sql
  SELECT a.ts, a.role, a.tool_data->>'name' AS tool, left(a.tool_data->>'input', 300) AS input,
         left(a.tool_data->>'output', 400) AS output, a.tool_data->>'success' AS ok
  FROM session_archive a JOIN sessions s ON s.id = a.session_id
  WHERE s.agent_id = 'allan' AND s.channel LIKE 'slack:%'
    AND a.ts BETWEEN '2026-11-25 18:40:00+00' AND '2026-11-25 18:50:00+00'
  ORDER BY a.ts;
  ```
- The hub log for the same window, for a guard hold or a capability refusal:
  `grep -E 'agent=allan|"agent":"allan"' hub.log | grep -E '2026-11-25T18:4[0-9]' | grep -E 'tool=(watch|monitor|scheduler|reminder|desk_work)|sentinel.audit|permissions.gate|outside content'`

Whichever call it was, the fix below is the same. Every one of those paths now points to the supported tool, and the tool itself says plainly what stops it.

## Decisions

### 1. The tool: `desk_work.keep_on_track`

An agent hands the person's own request to their Desk boss: "keep this on track and flag stalls". Desk's word on it comes back to the conversation it was asked in.

- **Where it is offered.** It is offered in every private turn of the person's (`PromptPrivacy::Private`, `current_identity_id`) on a hub that keeps Desk work (`desk_knows::installed`), whether or not Desk has shared anything yet. That way it can say what's missing. Like the reads, it is never offered to an automated run or a run made for others. It is registered beside the `desk_work.*` reads in `turn_loop/tools.rs::session_registry`, which builds both from one `Work`.
- **Input:**
  - `person_said`: the person's message asking for it, whole and exactly as they wrote it. It is checked word for word against one of their own messages in this conversation from the last 24 hours (`compared_words` equal: a part is refused, because "keep it on track but not tonight" cut short changes what they asked).
  - `session` (optional): the Desk session it is about, by the name or id `desk_work` reads show. It is resolved through `Work::session_id` and must be held (`holds_session`). Otherwise the tool answers `NOT_SHARED`.
- **It refuses, saying why and what fixes it, in plain words:**
  - Not the person's own conversation: "I can only hand your Desk a request from our own conversation (a DM with me): here, others' words could reach your Desk. Ask me in a DM."
  - No place to answer to (no messaging context): "I can't hand your Desk this from here: Desk's word on it would have nowhere to come back to. Ask me in our DM in Slack, or wherever else you chat with me."
  - The words aren't a message of theirs: "“…” isn't one of the person's messages here from the last day, word for word. Pass only what they wrote, whole." (a part of one: "“…” is only part of their message: pass their whole message, exactly as they wrote it.")
  - Desk never linked this agent (it never pushed anything and never checked in): "Your Desk hasn't linked me yet: in Desk, open my settings → What they see, and choose the work I may see. Then ask me again."
  - Already handed, the same words still open: it answers with the open request's id and changes nothing.
  - Five requests open from this person to this agent: "Five of your requests are still waiting for your Desk: …".
- **Queued, and how soon it lands, said truthfully** from the person's last check-in (`checked_in`):
  - within 2 minutes: "Handed to your Desk. Its boss confirms here, and Desk tells you here when it stalls."
  - older: "Your Desk last checked in at 17:02 UTC, so it's closed or your computer is asleep. It picks this up once Desk is open on your computer. If that isn't within a day, I'll tell you here and drop it."
  - never, though it shares work: "Your Desk shares your work with me but hasn't checked in for requests yet: update Desk on your computer and open it. If that isn't within a day, I'll tell you here and drop it."
- **The guard (DW-714 / DW-767).** It declares no read or stay (`Tool::reach` in #537), so a turn armed by outside content holds it for the person, as any send. It never arms anything itself. It builds on nothing #537 adds, and #537's per-item fencing of `desk_work.*` reads leaves the person's own work unarmed, so it runs unheld after reading it.
- **The person's words are read from the turn's own session**, as the turn holds it when its tools are built: the message the turn answers is in it before the session store has it (it is saved when the turn ends).
- **The origin** is what `watch` and `scheduler` use: the turn's messaging context (`SchedulerHost::get_messaging_context` under `CURRENT_SESSION_KEY`), plus the session's platform (`slack`, `teams`, …) for posting back on that platform's surfaces alone. Nothing Desk sends ever names a place.

### 2. Held on the agent's hub: `desk_knows.handed` (K/migrations_knows/V003)

```sql
CREATE TABLE handed (
    id          UUID PRIMARY KEY,
    person      UUID        NOT NULL,
    agent       TEXT        NOT NULL,
    agent_name  TEXT        NOT NULL,
    words       TEXT        NOT NULL,
    session     TEXT,
    session_name TEXT,
    platform    TEXT        NOT NULL,
    channel     TEXT        NOT NULL,
    thread      TEXT,
    asked_at    TIMESTAMPTZ NOT NULL,
    taken_at    TIMESTAMPTZ,
    taken_by    TEXT,
    heard_at    TIMESTAMPTZ,
    said        INT         NOT NULL DEFAULT 0,
    ended_at    TIMESTAMPTZ,
    ended       TEXT
);
CREATE INDEX handed_waiting ON handed (person, agent) WHERE ended_at IS NULL AND heard_at IS NULL;
CREATE TABLE checked_in (person UUID, source TEXT, agent TEXT, at TIMESTAMPTZ NOT NULL, PRIMARY KEY (person, source, agent));
```

`KnowsStore` gains six operations, in memory and in Postgres:

- `hand(&Handed)`
- `handed_waiting(&Reader)`
- `take_handed(&Owner, now)`: records the check-in, then answers and marks taken the open ones never heard of, asked within `HANDED_LIFE` (24 h), and not taken or taken more than `RETAKE` (10 min) ago. A Desk that died mid-delivery gets it again. Desk's own ids make a second take harmless.
- `handed_heard(&Reader, id, now) -> Heard { Unknown, Spent, Say(Handed) }`: counts one word back, under `MAX_SAID` (48).
- `checked_in(&Reader)`: the newest over the person's sources.
- `handed_expired(before)`: ends and answers the open ones never heard of, asked before `before`.

`forget_person` drops both tables' rows. `prune` drops requests ended before its cutoff.

### 3. Routes on the agent's hub (K/routes.rs)

Both use the `wfk_` token alone, through `authenticate(.., serving = true)`. The person, source and agent are the token's.

- `POST /api/desk-knows/handed`: only the Desk on the person's own computer takes requests (`engine == HOME_ENGINE`). A machine's token is refused 403, "Only the Desk on your own computer takes requests." It answers `{"handed": [{id, words, session, sessionName, agentName, place, askedAt}]}`. `place` is the platform's name as people say it ("Slack").
- `POST /api/desk-knows/handed/{id}/say` `{text}`: the text is plain, at most 1 500 characters, with no NUL. It is posted to where the person asked, as the agent, on that platform's surfaces (`desk_knows::handed::tell`), and counted.
  - `404` when the id isn't the token owner's.
  - `429` once `MAX_SAID` is spent.
  - `502` when no surface took it. The count is kept, so a broken channel can't loop forever.

`tell` is one function, used by the route, the expiry sweep and the Mac's own local agents. It sends one `OutboundMessage` through the one surface the agent's `surface_registry` resolves for the place it was asked (its platform, channel and workspace, kept from the session's channel `platform:workspace:channel…`), so a hub with several workspaces posts once, in the right one.

**The expiry sweep** (`desk_knows::handed::start(&state)`, beside `boss_links::wake::start` in `gateway::router`, once per process): every minute, `handed_expired(now − HANDED_LIFE)`. It tells each one where it was asked: "Your Desk didn't pick up your request “…” within a day, so I've dropped it. Open Desk on your computer and ask me again."

### 4. Taken on the person's computer (K/sync.rs)

Every pass, after the shares, on the Desk of the person's own computer only (`!desk.knows_on_machine()`), for each agent its rules name:

- `Target::Remote(link)` takes with `POST /handed`. `Target::Local(owner)` takes from the store in process.
- Each request goes to `CrewHub::take_handed(&From { agent, person }, handed)`.
- Then the words Desk owes (`handed_says_unsent`) go out the same way, by `/say` or by `tell` in process. Each is marked sent when the hub took it, or when the hub says it can never take it (`404`, `429`, `400`). `401`/`403` keep it and back off as a failed share does.

A take or a say failing never holds up the shares (its own back-off per agent).

### 5. Held by the boss (H/handed.rs, D/handed.rs, D migration V099)

```sql
CREATE TABLE IF NOT EXISTS handed (
    id UUID PRIMARY KEY, agent TEXT NOT NULL, agent_name TEXT NOT NULL, place TEXT NOT NULL,
    words TEXT NOT NULL, session UUID, asked_at TIMESTAMPTZ NOT NULL, taken_at TIMESTAMPTZ NOT NULL,
    boss TEXT, delivered_at TIMESTAMPTZ
);
CREATE TABLE IF NOT EXISTS handed_says (
    id BIGSERIAL PRIMARY KEY, handed UUID NOT NULL REFERENCES handed (id) ON DELETE CASCADE,
    text TEXT NOT NULL, at TIMESTAMPTZ NOT NULL, sent_at TIMESTAMPTZ
);
```

`CrewHub::take_handed`:

1. **Once.** `handed_keep` inserts it unless its id is kept, and says what became of one kept before (`Kept`):
   - never delivered nor answered (Desk stopped after keeping it): delivered now;
   - delivered with nothing said back, and no turn on it running in this process (Desk stopped mid-turn): "Your Desk boss X has your request in your Desk DM, but Desk stopped before it answered: answer there, or ask me again.";
   - otherwise done.
2. **Only the person's.** `from.person` must be `crew.person(..)`. A link made by someone else is answered "This Desk isn't yours…" and nothing is delivered.
3. **The boss.**
   - When a session is named, it goes to that session's boss (`overseer_of_session`). A session Desk doesn't have any more, or one in a group no boss oversees, says so and names the fix: "No boss oversees “X”'s group in Desk: open a group's boss settings and turn on Oversees the crew."
   - When no session is named, it goes to the main boss (`overseer_for(Ungrouped)`). With none: "You have no boss in Desk yet: open an agent's settings in Desk and turn on Oversees the crew. Then ask me again."
4. **As the person's own request, attributed.** One DM turn of the boss, queued as another boss's message is (`run_dm_turn_queued`, under its mind):
   - Its prompt and its kept message are the person's words, exactly. Every check of the person's words (a goal agreed in `crew.plan`, an order kept from the turn, `consent`/`agreement`) reads them as if typed in the DM.
   - Its author is `Speaker::overseer("<hub>/<agent>", "Allan, for Kingsley in Slack")`. Desk shows the message as relayed by that name.
   - The turn's `theirs` is the words, so the turn is the person's, as `run_dm_turn_for_them`'s is.
   - The turn is told, outside the message (`channel_system_prompt`, never kept): "This is Kingsley's own request, in their words, passed on by Allan, their agent elsewhere, from their Slack conversation with it. They want this kept on track and any stall flagged: keep it as a goal with crew.plan, its goal in their own words and agreed: their words, exactly as here[, on the session “X”]. Desk tells Allan of each stall, and Allan tells them in Slack." The goal is in their words because `goal::agreement` reads a "keep …" as unclear unless it says the goal.
   - Below Act fully, `crew.plan` in this turn is held for the person's OK: Desk's own notes on the crew are fenced into every boss turn and arm the outside-content guard, and no one is on a relayed turn to answer. That is the guard's (DW-767, #537), not changed here; the word back says so ("if it waits on your OK, give it in your Desk DM with X").
   - `run_turn_with` gains `told: Option<&str>` for this. Every other caller passes `None`.
5. **Said back.** After the turn, one word back is kept (`handed_say`):
   - when a running plan of that boss covers it (`handed::covers`): "Your Desk boss Desk has it: it keeps “goal” as a goal, and Desk tells you here when it stalls.";
   - else: "Your Desk boss Desk has your request and answered: “…”. It isn't kept as a goal yet, so Desk can't flag stalls on it until it is: if it waits on your OK, give it in your Desk DM with Desk.";
   - a turn that failed: "Your Desk boss Desk couldn't take your request just now (…). It is in your Desk DM with Desk: answer there, or ask me again."

   A refusal in steps 2–3 is kept the same way. The request is held with no boss, so it is never delivered again.

`handed::covers(h, plan, sessions)`, pure, in D, answers whether a plan carries a request. All of these must hold:

- the same boss;
- the plan running or paused;
- either the request names one of the plan's sessions, or the plan's `agreed` words are in the request's words (`goal::says_goal`), agreed at or after its delivery.

### 6. A stall flagged back (H/goal_check.rs)

In `goal_lines`, once what the boss was told is kept: when the stalls woke the boss this time (`goal::wakes(stalls, said)`, the boss's own cadence of one, two, four and eight times the window), every request that `covers` the plan gets one word back. It is the roundup's own line, said by the boss (`note_line`, which `goal_note` already writes): "Desk (your boss): No progress on “goal” for 25 min; I last sent its sessions work at 18:50."

`last_sent`, the read `goal_note` makes, is shared by both, so it isn't repeated. A failing part's wake, and a message that hasn't gone in, are the boss's to act on and are never flagged.

### 7. The other dead ends point to the tool

- `watch` of `desk_work.*` answers "Desk's work isn't polled: to have Desk keep it on track and tell the person of each stall, hand it to their Desk boss with desk_work.keep_on_track."
- `watch` of `crew.*` answers `CREW_WATCH` only on a hub that runs Desk (`desk_crew::for_agent`). Elsewhere it gives the same answer as `desk_work.*`.
- `monitor`'s `NOT_ON_DESK` adds: "to have Desk keep work on track, desk_work.keep_on_track hands it to their Desk boss".

No Desk app change is needed. The Mac's hub process takes, delivers and sends, and its page calls no new route. The message renders through the existing relayed-author path.

## Edge cases

- The person asks twice: the same open words answer the open id. Different words are a second request, up to five open.
- Desk is closed: the request waits up to a day, then the place it was asked is told and it is dropped.
- Desk takes a request and dies before delivering: it is taken again after 10 min, and Desk's own id keeps it to one delivery.
- A machine's Desk never takes a request: it doesn't ask, and the route refuses it.
- The agent stops accepting Desk work, or `[desk_knows]` is turned off: `authenticate(serving)` refuses take and say alike, and the person's Desk retries with back-off.
- The person unlinks the agent: the token is revoked, Desk's says get 401, and nothing more is posted. `forget_person` drops everything.
- The person's words carry a qualifier ("…but not tonight"): they are delivered whole, so the boss's `agreement` reads them as the person's own DM message would.
- A plan already running on the session: the request names the session, so its stalls are flagged even though no new goal was agreed.
- The boss is busy: the turn is queued until the DM is free (`run_dm_turn_queued`), as another boss's message is.
- A turn armed by outside content: the tool is held for the person's Allow, as any send.

## Tests

- K (unit), the tool:
  - refuses off the person's own conversation, with no place to answer, with words that aren't a whole message of theirs, and before Desk ever linked the agent;
  - queues with the truthful word for a fresh check-in, a stale one, and none;
  - the same words twice answer the open id;
  - a sixth open request is refused;
  - an unshared session answers `NOT_SHARED`.
- K (unit), the store, in memory:
  - take records the check-in, answers once and again only after `RETAKE`, never past `HANDED_LIFE`, and never one heard of;
  - heard counts to `MAX_SAID`, and is `Unknown` to anyone else;
  - expired ends once;
  - `forget_person` drops it.
- K (Postgres), the same contract against `PostgresKnows`.
- K (routes):
  - a machine's token can't take;
  - another person's id is 404;
  - a say past the cap is 429;
  - a say posts to the origin's platform only.
- K (sync):
  - a pass on the person's computer takes from its agents' hub, hands each to the crew, and sends the owed words. A 404 marks them sent; a 401 keeps them.
  - a machine's pass takes nothing.
- D (Postgres): `handed_keep` once, `handed_say`/unsent/sent, and `covers` for session and agreed-words links.
- H, the lifecycle (`desk_crew/tests/handed.rs`):
  1. A Slack DM turn of Allan's calls `desk_work.keep_on_track` with the person's message.
  2. The person's Desk takes it (`take_handed` → `CrewHub::take_handed`). The boss's DM holds it as the person's words, relayed by "Allan, for … in Slack", and the boss's turn was told it.
  3. The boss saves the goal with `crew.plan` agreed on those exact words.
  4. Its ack goes back and is posted in the Slack DM thread it came from.
  5. CI stays red past `stall_minutes`, the stall wakes the boss, and "No progress on" is posted back in that same thread, once per spell.
- H: a refusal for no boss is said back, and nothing is delivered. A request taken twice is delivered once.
- `watch` and `monitor`: the new answers, and `CREW_WATCH` only where Desk runs.
