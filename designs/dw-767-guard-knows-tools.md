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

MCP servers' own tools (`mcp.<server>.<tool>`), WASM and external skills stay held: what a third-party server claims of its tools (`readOnlyHint`) is its own word.

## Decisions

1. **A tool says where its call reaches.** A new `Tool` method beside `surface_effect`, `reads_outside_content` and `standing_choice`, input-aware like them:

   ```rust
   pub enum Reach { Reads, Stays }
   fn reach(&self, _input: &Value) -> Option<Reach> { None }
   ```

   `Reads`: the call only looks. `Stays`: what it does stays with the agent: a consultation, a file it makes for its own conversation. `None`, the default, keeps today's judgement, so a tool that says nothing is held as before. `delegate_call_facts!` forwards it, so no wrapper (`ConnectionWrite`, `mcp_turn`, `sessions_only`, `boss_links::hold`) drops it.

2. **The declaration only places what nothing else holds.** `Call` carries `reach`; `Call::told_by(tool)` fills it and `standing` from the tool, and `guarded_call` (R/tools/dispatch.rs) uses it. `assess` reads it only in its last step, beside `KEPT_READS` and `OWN_NAMESPACES`. Credentials, unseen URLs, writes somewhere new, outbound delivery, commands, identity files, persistence, setup and standing automation are all decided before it, so no declaration lifts them.

3. **Where a module already declares its reads, the guard uses it.** `Knowledge::reads` (H/monitor/scope.rs `knowledge()`) adds `aws::read_tool_names()` ("every `aws.*` tool is read-only by construction") and `bank::READ_TOOLS`. They are added to the guard's knowledge only, not to `scope::reads()`: that list is also what a monitor's turn is offered, and it stays as it is.

4. **Each tool declares itself where it is defined:**
   - `advisor`: `Stays`. Its answer is advice, never an action, from a provider the agent already uses. Its in-process loop's own tools (`filesystem.read`/`grep`/`glob`, `web.fetch`, `web.search`) run under a branch of the asking turn's guard (`ContentGuard::branch`: armed and scoped as the turn is, with its seen URLs, arming apart from it), through a new `security::guarded_tool::GuardedTool` wrapper (`build_advisor_tools` wraps each with one branch per consultation): a call the branch holds is refused to the advisor with the hold's reason (nobody watches an advisor round) and doesn't run, and what a call returns arms the branch as a runner's result arms a turn. That keeps a steered advisor from fetching a URL the turn never saw, whether the turn read outside content first or the advisor did, and what the advisor reads never arms the turn itself, so it asks the person nothing new. The arming rule is one `ContentGuard::returned(tool, outside, success, output)`, which the runner's `arm_on_result` now calls too. The Claude Code advisor round is already guarded through `RoundTools::bind`.
   - `XeroTool`, `FreeAgentTool`: `Reads` for a spec built with `ToolSpec::read`, from `spec.require_approval`.
   - `StoreTool`, `PipelineTool`: `Reads` for an op whose `access()` is `Access::Read`.
   - `GoogleTool`: `Reads` for a name in `OUTSIDE_READS` or a new `READS` beside it (its reads of the person's own account).
   - `SpotifyTool`: `Reads` for `spotify.devices` and `spotify.now_playing`, from a `reads` flag on its `Spec`.
   - `DeskWorkTool`, the `database.*` reads, `security.audit`, `http.endpoints`, `activity_history`, `costs.*`, `heartbeat.list`/`read`, `policy.proposals`, `pricing.list`, `relationship_status`, `sentinel.*`, `github.repos`, `training_citations`, `approval.status`, `read_source.list`, `messaging.get_mode`/`list_modes`, `slack.app_scaffold`, `view_attachment`, `get_user_photo`: `Reads`.
   - `budget`: `Reads` for `action: "status"` only.
   - `chart`, `diagram`, `generate_report`, the image, speech and video generators and editors: `Stays`. They make a file the system delivers in the turn's own conversation.

5. **Existing grammar takes the rest.**
   - `obscura` is a browser: the guard's browser rule (`BROWSER_READS`, "would act on a web page") applies to every browser namespace (`BROWSERS = ["browser", "obscura"]`), so its looks run and its actions are held as acting on a page.
   - `watch` joins `STANDING_RULES` (it polls and messages later, on its own).
   - `know_how` joins `PERSISTING` with `search` and `list` as its reads: what it learns is shared with everyone the agent works for.

6. **What is held says why, plainly.** The held reason becomes

   > `{signal}. This turn read outside content (from {armed}), which could be what is asking for this`

   and `Unvetted`'s description

   > `` `{tool}` isn't known to only read or to stay within the agent's own work ``

   so a person reads what the tool might do and why they are asked.

## Not changed

- A monitor's turn: its tool set (`turn_tools`), its refusing guard and what it tells the owner.
- What is held for a specific reason, and the order it is decided in.
- `subagent`, `investigate` and `coding.fanout` stay held: the turns they start don't run under the caller's guard.
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
