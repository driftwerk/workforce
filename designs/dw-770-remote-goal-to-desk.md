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

- **Where it is offered.** It is offered in every private turn of the person's (`PromptPrivacy::Private`, `current_identity_id`) on a hub that keeps Desk work (`desk_knows::installed`), off Desk itself, whether or not Desk has shared anything yet. That way it can say what's missing. Like the reads, it is never offered to an automated run or a run made for others. It is registered beside the `desk_work.*` reads in `turn_loop/tools.rs::desk_tools`, which builds both from one `Work`.
- **Input:**
  - `person_said`: the message of theirs this turn answers (the newest user message of the session as the turn holds it), whole and exactly as they wrote it (`compared_words` equal both ways). A part is refused, because "keep it on track but not tonight" cut short changes what they asked. An earlier message is refused: only the turn's own message is the person asking now.
  - It must stand on its own (`goal::stands_alone`) and not be a plain reply (`goal::plainly` is `Unclear`): a bare "yes", "ok" or "no" only answers something said before it, and is never handed on. At most `handed::WORDS_MAX` (1 000) characters.
  - `session` (optional): the Desk session it is about, by the name or id `desk_work` reads show. It is resolved through `Work::session_named` and must be held. Otherwise the tool answers `NOT_SHARED`. It is sent as the person's Desk names it: a session a machine's Desk shared (its source `<device>/<machine>`, `KnowsStore::session_source`) as `<machine>/<id>`.
- **It refuses, saying why and what fixes it, in plain words:**
  - Not the person's own conversation: "I can only hand your Desk a request from our own conversation (a DM with me): here, others' words could reach your Desk. Ask me in a DM."
  - No place to answer to (no messaging context): "I can't hand your Desk this from here: Desk's word on it would have nowhere to come back to. Ask me in our DM in Slack, or wherever else you chat with me."
  - Not the message this turn answers, or only part of it; a bare reply; too long: each in its own words.
  - Desk never linked this agent (it never pushed anything and never checked in): "Your Desk hasn't linked me yet: in Desk, open my settings → What they see, and choose the work I may see. Then ask me again."
  - Already handed, the same words about the same session still open: it answers with the open request's id and changes nothing.
  - Five requests open from this person to this agent: "5 of your requests are still waiting for your Desk: …".
- **Queued, and how soon it lands, said truthfully** from the person's last check-in (`checked_in`): within 2 minutes, "Handed to your Desk…"; older, "Your Desk last checked in at … UTC, so it's closed or your computer is asleep…"; never, "Your Desk shares your work with me but hasn't checked in for requests yet…".
- **The guard (DW-714 / DW-767).** It declares no read or stay, so a turn armed by outside content holds it for the person, as any unvetted call (tested). It never arms anything itself, and builds on nothing #537 adds.
- **The origin** is what `watch` and `scheduler` use: the turn's messaging context (`SchedulerHost::get_messaging_context` under `CURRENT_SESSION_KEY`), the session's platform and workspace, and the agent's session id (`CURRENT_SESSION_ID`), so Desk's words back are also kept in that session for the agent to know.

### 2. Held on the agent's hub: `desk_knows.handed` (K/migrations_knows/V003)

`handed` (id, person, agent, agent_name, words, session, session_name, platform, workspace, channel, thread, conversation, asked_at, taken_at, taken_by, heard_at, said, ended_at, ended), with a partial unique index on the open `(person, agent, md5(words), coalesce(session, ''))`; and `checked_in (person, source, agent, at)`.

`KnowsStore`:

- `hand(&Handed) -> Handing { Kept, Already(id), Full }`: under a per-(person, agent) advisory lock in Postgres, so two turns at once can neither both keep the same request nor pass the cap of five.
- `handed_waiting(&Reader)`.
- `take_handed(&Owner, now, delivering)`: records the check-in; renews the take of those this Desk says it is still delivering (only its own, `taken_by`); then answers and marks taken the open ones never heard of, asked within `HANDED_LIFE` (24 h), and not taken or taken more than `RETAKE` (10 min) ago. A Desk that died mid-delivery gets it again; a second Mac of the person's never takes one the first is delivering.
- `handed_heard(&Reader, id, now) -> Heard { Unknown, Spent, Say(Handed) }`: counts one word back, under `MAX_SAID` (48). An ended request is `Unknown`: its person was already told it was dropped.
- `checked_in(&Reader)`, `session_source(&Reader, session)`.
- `handed_expired(before, now)`: ends the open ones never heard of, asked before `before` and not taken since `before`: a request Desk took has a full day from its take.

`forget_person` drops both tables' rows. `prune` drops requests ended before its cutoff, and any asked before the earlier of the cutoff and `HANDED_KEPT` (30 days), so Desk's stall words keep a row to land on.

### 3. Routes on the agent's hub (K/routes.rs)

Both use the `wfk_` token alone, through `authenticate(.., serving = true)`, and only the Desk on the person's own computer (`engine == HOME_ENGINE`; a machine's token is 403). The person, source and agent are the token's.

- `POST /api/desk-knows/handed` `{delivering}`: answers `{"handed": [{id, words, session, platform, askedAt}]}`. Nothing the hub chose to call anything goes to Desk's boss.
- `POST /api/desk-knows/handed/{id}/say` `{text}`: plain text, at most 1 500 characters. Posted where the person asked, as the agent (`desk_knows::handed::tell`), and counted. `404` when the id isn't the token owner's (or the request ended), `429` once `MAX_SAID` is spent, `502` when no surface took it.

`tell` is one function, used by the route, the expiry sweep and the Mac's own local agents. It sends one `OutboundMessage` through the one surface the agent's `surface_registry` resolves for the place it was asked, and keeps the words as the agent's own message in its session there.

**The expiry sweep** (`desk_knows::handed::start(&state)`, once per process): every minute, `handed_expired(now − HANDED_LIFE, now)`, each told where it was asked: "Your Desk didn't pick up your request “…” within a day, so I've dropped it…".

### 4. Taken on the person's computer (K/sync.rs)

Every pass, after the shares, on the Desk of the person's own computer only (`!desk.knows_on_machine()`), for each agent its rules name:

- `Target::Remote(link)` takes with `POST /handed`; `Target::Local(owner)` from the store in process. Both say which requests of that agent's this Desk is delivering (`desk_crew::delivering`).
- Each is built by one constructor (`desk_handed`): the agent's name from Desk's own record (this hub's agent by its name, another hub's by its id from the link key, clipped), the place through `place_of`'s closed set. Then `CrewHub::take_handed(person, handed)`.
- Then the words Desk owes (`handed_owed`: at most 10 of each agent's, so one whose hub is down never holds up another's) go out by `/say` or by `tell`. Each is marked sent when the hub took it, or can never take it (`404`, `429`, `400`). A failure counts a try and backs that agent off; a word tried 50 times, or older than 30 days, is given up. The Desk's own rows are pruned hourly past 30 days.

### 5. Held by the boss (H/handed.rs, D/handed.rs, D migration V100)

`handed` keyed on `(agent, id)` (one agent's hub can never collide with another's), `session TEXT` as Desk names it (`<id>` or `<machine>/<id>`); `handed_says (agent, handed) → handed` with `tries`.

`CrewHub::take_handed`:

1. **Once.** A request whose turn runs in this process is skipped. `handed_keep` inserts it unless kept, and says what became of one kept before (`Kept`): never delivered nor answered is delivered now; delivered with nothing said back is "Your Desk boss X has your request in your Desk DM, but Desk stopped before it answered…"; otherwise done.
2. **Only the person's.** The link's person must be Desk's own owner (`crew.person`). Another's is "This Desk isn't yours…"; with no owner known yet, "Your Desk can't tell yet whose it is…". Longer than `WORDS_MAX`: "Your request is too long…".
3. **The boss.** A session on this computer goes to its boss (`overseer_of_session`), a machine's (`<machine>/<id>`, `relay::machine_session`) to the boss of its sidebar group (`session:<machine>/<id>`). A session Desk no longer has, an id it can't read, a group no boss oversees, or no main boss, each says so and names the fix.
4. **Their words, relayed, bounded.** One DM turn of the boss, queued as another boss's message is (`run_dm_turn_queued`, under its mind):
   - Its message is the person's words, exactly, authored `Speaker::overseer("<hub>/<agent>", "Allan, for Kingsley in Slack")`, marked `desk-relay:` (`deliver::RELAY_MARK`). It is told, outside the message (`run_turn_with`'s `told`), whose request it is and to keep it with `crew.plan` agreed in their words.
   - **Never the person's typing.** The turn passes no `theirs`, so the words never enter `person_turns`: no `persons_words_in_turn` (no order to another agent as theirs), no `order_from_turn`, no cadence change. `talk_on` leaves every relayed message out of the person's words, so they are never a reply to what the boss proposed (`replied_to`), a short consent, or an agreement read from the DM.
   - **They agree one thing.** `crew.plan`'s `agreed` is checked against the DM (`agreed_at`) and, failing that, against Desk's own record of the requests delivered to that boss in the last day (`handed_agreed`): the quote is in a request's words, the request says the goal and agrees it (`handed::agrees`: `says_goal` and `agreement` yes), and no plan carries it yet. So one request agrees its own goal, once, from any of the boss's turns of the next day; never another goal, a change to a goal, `met`, or `start_now`.
   - **The guard.** Below Act fully, `crew.plan` in the relayed turn itself is still held: Desk's own crew notes are fenced into every boss turn and arm the outside-content guard, and no one is on a relayed turn to answer. That is DW-767's (#537). Because the agreement is Desk's record, not the turn's, the boss saves the goal in its next turn that isn't held (the person's next word in Desk, or its own turn at Act fully) with no retyping; once #537 stops Desk's own notes arming the guard, it saves it in the relayed turn.
   - **Delivered when it is in the DM.** The turn signals once its message is kept (`told_when_asked`); only then is the request marked delivered (at the time the turn began). A turn that never began leaves it undelivered.
5. **Said back in Desk's words.** After the turn, one word back (`handed_say`), templated, never the boss's answer or an error: "Your Desk boss X has it: it keeps “<agreed words>” as a goal…"; "Your Desk boss X has your request in your Desk DM. It isn't kept as a goal yet…"; "Your Desk boss X couldn't take your request just now. Ask me again in a while."

`handed::covers(h, plan, sessions)`, pure, in D: the same boss; the plan running or paused; the request names one of its sessions (in Desk's own id form), or the plan's `agreed` words are in the request's words, agreed at or after its delivery.

### 6. A stall flagged back (H/goal_check.rs)

When the stalls woke the boss this time (`goal::wakes`), every request that `covers` the plan gets one word back: the roundup's own line, said by the boss (`note_line`). `last_sent` and `session_ids` are shared with `goal_note`.

### 7. The other dead ends point to the tool

- `watch` of `desk_work.*` answers "Desk's work isn't polled: … hand it to their Desk boss with desk_work.keep_on_track."
- `watch` of `crew.*` answers `CREW_WATCH` ("already wakes you") only for an agent that is a boss (`overseer_named`); any other gets the `desk_work.*` answer.
- `monitor`'s `NOT_ON_DESK` adds: "to have Desk keep work on track, desk_work.keep_on_track hands it to their Desk boss".

No Desk app change is needed: the Mac's hub process takes, delivers and sends, its page calls no new route, and the message renders through the existing relayed-author path.

## Edge cases

- The person asks twice: the same open words about the same session answer the open id. Different words are a second request, up to five open.
- The person says "yes" to the agent: never handed, and their earlier message isn't either (it isn't what the turn answers).
- Desk is closed: the request waits up to a day, then the place it was asked is told and it is dropped.
- Desk takes a request and dies before delivering: it is taken again after 10 min, and Desk's own key keeps it to one delivery. While a turn on it runs, Desk renews its take, so neither the sweep nor a second Mac takes it.
- A machine's Desk never takes or answers a request: it doesn't ask, and the routes refuse it.
- The agent stops accepting Desk work, or `[desk_knows]` is turned off: `authenticate(serving)` refuses take and say alike, and the person's Desk retries with back-off, to its limit.
- The person unlinks the agent: its words are dropped. `forget_person` drops everything.
- An agent's hub is down: its words wait with their own back-off; every other agent's go.
- The person's words carry a qualifier ("…but not tonight"): they don't agree the goal (`agreement` no), so the boss asks them.
- A plan already running on the session: the request names the session, so its stalls are flagged even though no new goal was agreed.
- The boss is busy: the turn is queued until the DM is free (`run_dm_turn_queued`).

## Tests

- K (unit), the tool: refusals (not own conversation, never linked, part of a message, an earlier message, a bare "yes"/"ok"/"no", too long, an unshared session); the truthful queued word for a fresh, stale and absent check-in; the same words twice; a sixth refused; a machine's session in Desk's form; the tool held once the guard is armed; the sweep tells a never-taken request once, gives a taken one a day from its take, and keeps the words in the agent's session.
- K (unit + Postgres), the store contract: dedupe and cap; take once, again after `RETAKE`, never past `HANDED_LIFE`; a delivering Desk keeps it from another; heard to `MAX_SAID`, `Unknown` to anyone else and once ended; expiry from the take; `session_source`; prune keeps a heard row; `forget_person`.
- K (routes): a machine's token can't take or say; another person's request is 404; past the cap is 429; a say posts only where it was asked; a bad take body is 400.
- D (Postgres): `handed_keep` once per `(agent, id)`, delivered, `handed_owed` per agent, tries to a limit, prune with its words.
- D (unit): `covers` for session (both id forms) and agreed words; `agrees` only the goal said.
- H, the lifecycle (`desk_crew/tests/handed.rs`), through the real `KnowsSync` over a memory store:
  1. A Slack DM turn of the agent calls `desk_work.keep_on_track` with the person's message.
  2. The sync takes it; the boss's DM holds it as the person's words, relayed by "…, in Slack", and the boss's turn was told it.
  3. Its word back is posted in the Slack DM thread it came from, and kept in the agent's session there.
  4. The relayed words are not the person's in the DM (`agreed_at` fails), and agree no other goal; `crew.plan` agreed on them saves their goal once.
  5. CI stays slow past `stall_minutes`, and "No progress on" is posted back in that same thread.
- H: a session gone, an unreadable session, too long, another person's link, no main boss, a group with no boss: each said back, nothing delivered. A request taken twice is delivered once; one delivered before Desk stopped is said to wait in the DM. A word the hub can't post is kept and backed off; one it never knew is done. A machine's Desk takes nothing.
- `watch` and `monitor`: the new answers.
