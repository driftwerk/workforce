# The outside-content guard knows the agent's everyday tools (DW-767)

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

Code rules: no TODOs, no partial implementations, no comment that isn't strictly needed, no duplicated code. Idiomatic, functional-style Rust in the crates' own patterns. Every behaviour below has a test in "Tests".

R = `crates/agent-runtime/src`, H = `crates/hub/src`, S = `crates/skills/src/adapters`.

## What happens today

Once a turn reads outside content (any `web.fetch`/`web.search` result, a fenced mail, a connector read), `ContentGuard` (R/security/monitor.rs, DW-714) weighs every later call with `assess`. A call that isn't held for a specific reason (credentials, an unseen URL, a write somewhere new, commands, identity files, what later turns trust, setup, standing automation) runs only when the guard can place it as a read or as the agent's own work: `KEPT_READS`, `SETUP_READS`, `PERSISTING` reads, `SurfaceEffect::Reads`, `Knowledge::reads` (the hub's names), `OWN_NAMESPACES`. Anything else is `AnomalySignal::Unvetted`, and the person is asked to Allow it with:

> `advisor` isn't known only to read or to keep to the agent's own work, and this turn is processing outside content (web.fetch) that may be steering it

Measured with a temporary harness (the hub's real composed registry from `serve::testing::plain_service`, each tool's own `surface_effect` and `standing_choice`, plus every built-in adapter's static tool names, plus the config-gated tools by name), these are held as Unvetted after a web read:

- The agent's own consultation: `advisor`.
- Hub and runtime reads: `activity_history`, `costs.analyze`, `costs.forecast`, `heartbeat.list`, `heartbeat.read`, `policy.proposals`, `pricing.list`, `relationship_status`, `sentinel.status`, `sentinel.history`, `sentinel.integrity`, `sentinel.scan`, `security.audit`, `http.endpoints`, `github.repos`, `training_citations`, `approval.status`, `read_source.list`, `messaging.get_mode`, `messaging.list_modes`, `slack.app_scaffold`, `budget` (its `status`), `view_attachment`, `get_user_photo`, `desk_work.*`, `database.{list_instances,schemas,tables,columns,query,explain}`, `know_how` (`search`/`list`).
- The store and pipelines' reads: `store.{get,list,query,described,collections,content_read,content_stat,content_list}`, `pipeline.{list,get,item,items,runs,facts}`.
- What the agent makes for its own conversation: `chart`, `diagram`, `generate_report`, `generate_image`, `generate_speech`, `generate_video`, `compose_video`, `trim_video`, `video_to_gif`.
- Connector reads: every `aws.*` tool, `bank.*`, the `xero.*` and `freeagent.*` reads, the Google reads that aren't outside content (calendars, colours, free/busy, contacts, people, Drive listings and metadata, Docs/Slides/Sheets metadata, Gmail labels/drafts/filters/settings, Tasks, Chat space listings), `spotify.devices`, `spotify.now_playing`, the `obscura.*` looks.
- Rightly held, but said vaguely: `xero.invoice_email`, the Google and Spotify writes, `obscura.click`/`fill`/`type`/…, `watch`, `know_how` `add`, `store`/`pipeline` writes, `github.clone`, `knowledge.index`, `subagent`, `investigate`, `coding.fanout`, `pairing.*` reads, the credential and setup flows.

The person's own Desk arms the guard too. Every `desk_work.*` result and the `[DESK WORK]` digest are fenced with `wrap_external_content` (H/desk_knows/tools.rs `wrapped`), so after reading one of the person's own sessions their later calls are held (Allan, 25 Nov: `subagent` `list`, `view_attachment` `desk:<uuid>`, `advisor`, `compose_video` and a `notes` write, each "processing outside content (desk_work.session)").

MCP servers' own tools (`mcp.<server>.<tool>`), WASM and external skills stay held: what a third-party server claims of its tools (`readOnlyHint`) is its own word.

## Decisions

1. **A tool says where its call reaches.** A new `Tool` method beside `surface_effect`, `reads_outside_content` and `standing_choice`, input-aware like them:

   ```rust
   pub enum Reach { Reads, Stays }
   fn reach(&self, _input: &Value) -> Option<Reach> { None }
   ```

   `Reads`: the call only looks. `Stays`: what it does stays with the agent: a consultation, a file it makes for its own conversation. `None`, the default, keeps today's judgement, so a tool that says nothing is held as before. `delegate_call_facts!` forwards it, so no wrapper (`ConnectionWrite`, `mcp_turn`, `sessions_only`, `boss_links::hold`) drops it.

2. **The declaration only places what nothing else holds.** `Call` carries `reach`; `Call::told_by(tool)` fills it and `standing` from the tool, and `guarded_call` (R/tools/dispatch.rs) uses it. `assess` reads it only in its last step, beside `KEPT_READS` and `OWN_NAMESPACES`. Credentials, unseen URLs, writes somewhere new, outbound delivery, commands, identity files, persistence, setup and standing automation are all decided before it, so no declaration lifts them. Nor does a declaration place a call whose leaf or action is named as a change (`outward`, exposed as `monitor::names_a_change`): that call is held as a send. The listed reads and own namespaces keep their place (`filesystem.write` is the agent's own work), so the check applies to the declaration alone.

3. **Where a module already declares its reads, the guard uses it.** `Knowledge::reads` (H/monitor/scope.rs `knowledge()`) adds `aws::read_tool_names()` ("every `aws.*` tool is read-only by construction") and `bank::READ_TOOLS`. They are added to the guard's knowledge only, not to `scope::reads()`: that list is also what a monitor's turn is offered, and it stays as it is.

4. **Each tool declares itself where it is defined:**
   - `advisor`: `Stays`. Its answer is advice, never an action, from a provider the agent already uses. `AdvisorTool::execute` runs the whole consultation under a branch of the asking turn's guard (`ContentGuard::branch`: linked to the turn, so it is armed and given URLs whenever the turn is, even after the branch was made, and arming apart from it). Every route uses that guard:
     - the in-process loop's tools (`filesystem.read`/`grep`/`glob`, `web.fetch`, `web.search`) run through a new `security::guarded_tool::GuardedTool` wrapper (`build_advisor_tools`): a call the branch holds is refused to the advisor with the hold's reason (nobody watches an advisor round) and doesn't run, and what a call returns arms the branch as a runner's result arms a turn;
     - the Claude Code round's tools go through `RoundTools::bind`, which captures the branch and arms it on each result.

     When the branch is armed, the advice reaches the turn fenced with `wrap_external_content` (`for_the_turn`), so the turn is armed by it and may open its URLs as by any fence. A steered advisor can't fetch a URL the turn never saw, and advice built from a page arms the turn, so what the turn does next is held as after any outside read. The arming rule is one `ContentGuard::returned(tool, outside, success, output)`, which the runner's `arm_on_result` now calls too.
   - `XeroTool`, `FreeAgentTool`: `Reads` for a spec built with `ToolSpec::read`, from a `reads` flag on the spec.
   - `StoreTool`, `PipelineTool`: `Reads` for an op whose `access()` is `Access::Read`.
   - `GoogleTool`: `Reads` for a name in `OUTSIDE_READS` or a new `READS` beside it (its reads of the person's own account). `OTHERS_WORDS`, a part of `READS` (`drive.search`, `contacts.*`, `people.get`/`search`), returns what others wrote, so it arms the turn as `OUTSIDE_READS` does (`reads_outside_content`).
   - `SpotifyTool`: `Reads` for `spotify.devices` and `spotify.now_playing`, from a `reads` flag on its `Spec`.
   - `DeskWorkTool`, the `database.*` introspection (`list_instances`, `schemas`, `tables`, `columns`; `query` and `explain` return rows others wrote and stay held), `security.audit`, `http.endpoints`, `activity_history`, `costs.*`, `heartbeat.list`/`read`, `policy.proposals`, `pricing.list`, `relationship_status`, `sentinel.*`, `github.repos`, `training_citations`, `approval.status`, `read_source.list`, `messaging.get_mode`/`list_modes`, `slack.app_scaffold`, `view_attachment`, `get_user_photo`: `Reads`. `view_attachment` returns what someone else sent, so it also arms the turn (`reads_outside_content`), as `aws.s3.get`/`collect` do (`aws::BODY_TOOLS` joins the hub's `Knowledge::outside_reads`).
   - `budget`: `Reads` for `action: "status"` only.
   - `chart`, `diagram`, `generate_report`, the image, speech and video generators and editors: `Stays`. They make a file the system delivers in the turn's own conversation.

5. **Existing grammar takes the rest.**
   - `obscura` is a browser: the guard's browser rule (`BROWSER_READS`, "would act on a web page") applies to every browser namespace (`BROWSERS = ["browser", "obscura"]`), so its looks run and its actions are held as acting on a page.
   - `watch` joins `STANDING_RULES` (it polls and messages later, on its own).
   - `know_how` joins `PERSISTING` with `search`, `list` and `pending` as its reads: what it learns is shared with everyone the agent works for.

6. **What is held says why, plainly.** The held reason becomes

   > `{signal}. This turn read outside content (from {armed}), which could be what is asking for this`

   and `Unvetted`'s description

   > `` `{tool}` isn't known to only read or to stay within the agent's own work ``

   so a person reads what the tool might do and why they are asked.

7. **The person's own Desk is theirs, not outside content: Desk judges what each exchange carries in, by where it came from.** The hub no longer guesses from who asked. Desk (crates/desk, released to the app through its `hub.ref`) sends each exchange with `outside`: why it may carry outside content, each reason once, empty when it is the person's own work. `Desk::exchange_outside` (crates/desk/src/service/knows_sync.rs) works it out over the exchange's whole step and command lists:
   - **What it did** (`knows_outside::read_outside`): a step that reads the web or hands work on (`webfetch`, `web_fetch`, `websearch`, `web_search`, `google_web_search`, Claude Code's `task`/`agent`, case-insensitive), any engine's MCP step (`<server>__<tool>`), a command that fetches (`curl`, `wget`, `http`, `https`, `xh`, `nc`, `ncat`, `netcat`, by word, quoted too, as `bash -c "curl …"` gives it), and `gh` only where it reads what others wrote (`issue`, `pr view`, `pr diff`, `pr list`, `pr checkout`, `api`, `release view`, `gist`, `search`, `repo view`, `run view` with or without `--log`/`--log-failed`), its flags and the values of `-R`/`--repo`/`--hostname` passed over. The matching steps and commands are what it names.
   - **What a message on Desk's line carried in**, found by the `[Desk m:…]` mark its ask carries. Each line message keeps `outside` (migration V099). A boss's message carries what its turn had read: Desk asks the hub (`Desk::read_outside_with`, set in `desk_driver` to the calling turn's `ContentGuard::armed`), so a plan start, a steer, a handout and an Act-fully follow-up from an unarmed boss turn carry nothing. A boss's message to a session from before V099 is taken as carrying some: the turn that sent it was never recorded anywhere Desk can read, so nothing narrower can be known of it; after decision 7's stubs, such an exchange costs a one-line mention in a search and arms only the turn that opens its transcript.
   - **What a boss or an agent typed in** (`desk.prompt`, `Desk::prompt_for`): its words go in under a `[Desk m:…]` mark of their own, kept in `typed_prompts` (V099) with what its turn had read, so the exchange is asked by Desk, never the person, and carries that in.
   - **A message from another session** carries in what that session's exchange carried: the sender's exchange begun before the message is judged the same way, up to four hops, and past them it is taken as carrying some.
   - **What a session was started on** (`session_starts`, V099), for its first exchange: what the boss's turn starting it had read, and, for a plan's ticket, the ticket's writers (`knows_outside::ticket_outside`). A boss's ticket, an edit of one and a boss's comment keep what its turn had read (`written_outside`, V099, by the ticket's or the comment's id; a reason once kept stays). A ticket made here is the person's when the person, Desk or a boss whose turn had read nothing wrote it and its bosses' comments; not when a session or another agent wrote it, a boss's turn had read outside content, or a boss's ticket or comment is from before Desk kept that. A synced issue is the person's only when Desk knows who they are there (`Sync::me`), they wrote it (`reporter_id`, from Linear's `creator.id`, Jira's reporter `accountId`, GitHub's author login), every one of its comments is a person's that is theirs (`Author::kind` `Person` and `Author::id`; Linear's `botActor` makes a comment an app's even when it names a user), or posted from Desk as above, and read (as many as the issue counts), and no one else edited its description in its history as last read. Gap: neither Linear's nor Jira's issue read names who last edited a description, so an edit is seen only once the issue's history has been read (it is read with its comments) and only while it is among the kept [`HISTORY_MAX`] newest changes. A ticket handed to a working session carries the same into its line message.
   - **A plan's start on one of the person's machines** carries the same judgement there: the hub sends `outside` with `desk.session_start` (null for nothing), Desk's native relay forwards it (`START_FIELDS`), and the machine keeps it as the start's (`api.rs` `Start`, `create_held`). A start handed for a boss without it (an older hub or Desk) is taken as carrying some. A boss on another hub, served on this one, is taken as carrying some in whatever it says: its turn isn't seen here (`served::SERVED_UNSAID`).
   - **An engine's own prompt** (a wake-up, a compaction: asker `unknown`) carries in what an earlier exchange of its session did.
   - An ask fenced as outside content (`<<<EXTERNAL_UNTRUSTED_CONTENT_`) that no message or start explains carries some.

   Every exchange an agent was sent before this change is sent once more, saying what it carries (`knows_sent.outside_said`, false on every earlier row), within the same caps and daily spend; the hub keeps one copy per ref. The hub (`desk_knows/tools.rs`) labels an exchange with Desk's reasons, or with "Desk didn't say" when an older Desk sent none (fail-safe). Flagged exchanges keep their label. Each result is built part by part (`Record`): a part is fenced with `wrap_external_content` when its item has a caveat, carries a fence or reads like an injection (`scan_for_injection`), and every other part with `wrap_own_content`, so one caveated item no longer fences the whole result. The digest goes through the same rule. So that one turn's read doesn't spread from session to session, a search names an exchange that may carry outside content in one line (its session, time, ref, and how to open it), never its words, so only the turn that opens its transcript reads it and is armed. The overview, a session's page and the digest list no exchanges. Sessions' accounts, needs and decisions are Desk's own summaries and carry no caveat. Desk also harvests Codex's `web_search_call` as a `web_search` step.

8. **A sub-agent is the agent's own work, guarded as the turn that spawned it.** `subagent` declares `Stays`. Its job runs detached, in a fresh turn:
   - `AgentJobConfig` gains `guarded_as`: the spawning turn's `ContentScope` when outside content armed it (its seen URLs and places included, its refusals left out), captured by the `subagent` tool like `user_id`. `ServiceAgentExecutor` runs a job with one under a guard made from it, so every call the sub-agent makes is held as the turn's would be.
   - `ServiceAgentExecutor` keeps, through a guard keeper, what armed the job's guard; the job's output keeps it as `JobOutput::read_outside`, and its words are stored as the sub-agent wrote them. Only where a model reads them are they fenced (`JobOutput::text_for_a_turn`, and `value_for_a_turn` for a package, its strings fenced in place so it keeps its shape): the `subagent` `result` action, `jobs` `result`, the completion injected into the session that submitted it, a chain step's upstream context, and an inline `investigate` report. People are never shown the fences: the channel announcement and the API's job views read the output as it is.
   - The runner arms a turn by what joins it mid-run: an injected follow-up or job completion that carries a fence arms its guard (`absorb_injected_followups`).
   - A worker's staged findings are read as outside content: `data.query` and `data.inspect` fence what they return, so querying them arms the turn.
   - A `spawn` naming a non-blank `channel_id` announces its result there, so it declares that as a `surface_effect` write on the turn's platform (`messaging_routing::writes`): somewhere new is held as any send is. A blank `channel_id` is none.

9. **A tool of the agent's own namespaces named as a change runs after outside content only once it is looked at.** `OWN_NAMESPACES` (`monitor::keeps_its_own`) place a call as the agent's own work whatever it is named; a registry test lists every such tool and action that `names_a_change` flags, and fails on one not on its explicit `LOCAL_WRITES` list.

## Not changed

- A monitor's turn: its tool set (`turn_tools`), its refusing guard and what it tells the owner.
- What is held for a specific reason, and the order it is decided in.
- `investigate` and `coding.fanout` stay held: their jobs don't carry the caller's guard.
- A `notes` write after outside content is still held as persistence: what later turns trust. Reading the person's own Desk no longer arms the turn, so it isn't held there.
- MCP, WASM and external skills' tools stay held.

## Tests

- R/security/monitor.rs:
  - after a web read, a call declaring `Reads` or `Stays` runs; the same call undeclared is `Unvetted`;
  - a declaration never lifts a hold: a `Stays` call naming an unseen URL, a secret, a write to `#general`, an `exec.run`, a `memory` remember, a standing rule (`scheduler` `create`) and a setup write (`agent.update`) are each still held with their own signal;
  - `obscura.snapshot` runs, `obscura.click` is held as acting on a web page;
  - `watch` is held as standing automation; `know_how` `search` runs and `add` is held as persistence;
  - the held reason and the `Unvetted` description read as above.
- H/monitor/scope.rs: after a web read, the hub's knowledge lets `aws.ecs.list_clusters`, `aws.s3.get` and `bank.transactions` run; `every_tool_a_turn_is_offered_passes_the_guard` still holds and the monitor's offered set is unchanged.
- H (composed): on `plain_service`, after a web read, every registered tool this change declares runs without asking, `store` and `pipeline` following `access()` and `budget` reading only at `status`; `advisor`'s declaration is `Stays`; a real outward send (`messaging.send` elsewhere), a vault read (`vault.get`), a command (`exec.run`) and a standing-rule change (`scheduler` `create`) are still held.
- R/security/guarded_tool.rs: without a guard the tool is returned as it is; under an armed guard a call of a URL the turn never saw is refused and doesn't run, one it saw runs; a fenced result arms an unarmed guard and its URLs become ones the turn may open. R/security/monitor.rs: `returned` arms on a fence or a successful outside read, not on a failed one; a branch starts armed and scoped as the turn is and arms apart from it.
- H (composed): `build_advisor_tools` under an armed turn's guard refuses a `web.fetch` of a URL the turn never saw.
- R/tools/registry.rs: a connector write wrapped in `ConnectionWrite` answers `reach` as the tool it wraps.
- S: `XeroTool`/`FreeAgentTool` reads declare `Reads` and writes (`xero.invoice_email`) don't; every `GoogleTool` `READS` name is a real Google tool and none is mutating (the `WRITE_OPS` check); `spotify.devices` reads and `spotify.play` doesn't.
- H/desk_knows/tools.rs: each `desk_work.*` read of the person's own sessions leaves a guard unarmed; a search names a caveated exchange without its words and stays unarmed; a transcript showing a flagged exchange, or one Desk said fetched the web, arms it (`desk_work.transcript`); an exchange is labelled with Desk's reasons, not the hub's guess, and with "Desk didn't say" when it carries none, and an account isn't; a result with one caveated item fences only that item; text carrying a fence, or reading like an injection, is fenced as outside content.
- crates/desk: every step and command is read for outside content however many there are, quoted commands too, and `gh` only where it reads others' words, past `-R`/`--repo`/`--hostname` values; a ticket made here is the person's unless a session, a boss's armed turn or a boss from before Desk kept it wrote it or its bosses' comments; a synced issue is the person's only when they wrote it, every comment is a person's that is theirs and read, and no one else edited its description; a Linear comment with a `botActor` is an app's; an agent's `desk.prompt` goes in marked, kept with what its turn read, and its exchange is Desk's; an exchange sends its reasons redacted; a Codex `web_search_call` is a `web_search` step. Against Postgres: boss-run sessions on the person's own tickets (a plan start, a steer, a wake-up) carry nothing; a session started on a teammate's ticket, one that fetched the web and the wake-up after it, one a boss's armed turn sent to, and one another session passed web results to each carry that in; a boss's words carry what its hub says its turn read, unless they say what they carry; a prompt an armed agent typed in carries what it read and an unarmed one's nothing; an exchange sent before Desk said what it carries goes once more, then never again.
- H/desk_knows/loop_tests.rs, end to end against Postgres: eight boss-run sessions on the person's own tickets and one that read a page; round after round the boss reads the overview and searches, which arms nothing (the page read is only named), and steers every session, whose next exchanges carry nothing; the turn that opens the page read's transcript is armed, only the session it steers next carries that in, and the next turn's search of it stays unarmed.
- Desk app (`crew_relay.rs`): a plan's start to a machine forwards its `outside`, and `null` as said.
- H/subagent_tool.rs: a sub-agent spawned under an armed guard carries its scope to its job, where a fetch of a URL the turn saw runs and one it didn't is held; one spawned under no armed guard carries none; a spawn naming a channel writes there on the turn's platform, and a blank or missing one writes nowhere.
- H/job_runner: a run that read outside content is stored as it was written, with what it read; the completion injected into its session is fenced and arms it, and the one posted to people carries no fence; one that read none reads the same both ways.
- R/runner.rs: a fenced job completion injected into a running turn arms it, and an `exec.run` after is held.
- H/findings.rs: a `data.query` of a worker's staged findings arms the turn.
- H (composed): every tool of the agent's own namespaces, and each of its actions, that is named as a change is on `LOCAL_WRITES`.
- H/advisor.rs: an unarmed turn whose advisor's research read a page is armed by the advice and an `exec.run` after is held; advice that read nothing leaves it unarmed.
- R/security/monitor.rs: a branch made before the turn read outside content is armed with it and may open the URLs it saw; a declared call named as a change (`issue.comment`, `send_reply`) is held as a send.
- H (composed): a sub-agent job guarded as an armed turn, whose model asks for a `scheduler` `create`, creates no job, and the same job unguarded creates it; after a web read, `subagent` (`list`, `spawn`), `view_attachment` (`desk:<uuid>`) and `advisor` run, a `spawn` naming another channel is held as a send, `database.query`/`explain` and `budget` without `status` are held, a `notes` `write_daily` is held as persistence, no registered tool declaring `reach` is named as a change, and `view_attachment` arms the turn.
- H/monitor/scope.rs: `aws.s3.get`/`collect` arm a guard, `aws.s3.list` doesn't.
- S/google: no `READS` name is named as a change, and `OTHERS_WORDS` is part of `READS`.
- R/tools/video_compose.rs: after a web read, `compose_video` (`segments: "none"`), `trim_video` and `video_to_gif` run.
