# Workforce Desk — local multi-agent app (design proposal)

Status: being built since 2026-10-27 (uncommitted). Hub side on `feat/desk`
(`crates/desk`, `crates/hub/src/desk_{driver,tools}.rs`, `packages/rich`); app in the
local repo `workforce-desk` (Tauri app, `desk-core`, `desk-hubd`, `desk-tuple`, `desk-voice`).
§7 lists where the build departs from this proposal.

Legend: **[exists]** code is there today · **[change]** extend existing code · **[new]** to build.

## 1. Recommendation in one paragraph

A Tauri 2 app (unison-editor's stack) that is **a client of N workforce hubs**, one of which is a
**local hub it supervises** on loopback. Local-only agents are simply agents on the local hub, so
they get the whole harness (memory, models, vault, skills, workflows, scheduler, coding) with no new
mechanism and never appear remotely. Remote hubs (Driftwerk, Acme/Atlas…) are paired as a
device, exactly like the iPhone. Claude Code and Codex sessions are **owned by the local hub**
(PTY running the real TUI + hooks for state), get workforce's tools over the existing Session MCP,
and can be started and driven by any agent — local or remote — as tools. Voice, Tuple and the
quick-ask palette all just make hub turns addressed by agent name. The chat surface is Slack-like
(rooms, DMs, threads, @mentions across local and remote agents) and renders Edge's rich blocks —
tables, charts, forms, maps, diagrams, images — inline, from one renderer shared with Edge and the
web UI.

```
                ┌──────────────────────── Desk (Tauri app, menu bar) ─────────────────────────┐
                │ roster · chat panes · terminal panes · diff · approvals · voice I/O · Tuple │
                └───────┬──────────────────────────────┬─────────────────────────┬─────────────┘
          loopback WS/REST (JWT)              /ws + REST (wfp_)        /mcp (wfr_) tools
                        │                              │                          │
   ┌────────────────────▼─────────────────┐   ┌────────▼──────────┐   ┌───────────▼───────┐
   │ local hub  (workforce serve, launchd) │   │ remote hub: yours │   │ remote hub: theirs│
   │ local agents · SQLite memory · vault  │   │ allan, sarah…     │   │ atlas…          │
   │ desk sessions: claude / codex PTYs    │   └────────▲──────────┘   └────────▲──────────┘
   │ worktrees · Session MCP · spaces      │──── node uplink /ws/agent (wfp_, identity-bound) ┘
   └───────────────────────────────────────┘   remote agents call node.<laptop>.* tools here
```

## 2. Components

### 2.1 Local hub — [exists] + [change]
- Runs `workforce serve` unchanged as a **LaunchAgent** (`~/Library/LaunchAgents/xyz.driftwerk.desk.hub.plist`),
  not in-process. Why: hub boot is ~2.5k lines inline in `crates/cli/src/main.rs:3687-6194` (no lib
  fn); crash isolation (known intermittent hub SIGABRT); agents and sessions survive app quit
  (herdr-server / codex app-server-daemon model); same binary as prod. In-process embedding = later
  refactor (`hub::boot`), not a blocker.
- Single-process mode already works: NATS → standalone (`hub/src/state.rs:217-240`), memory
  `sqlite` default, sessions `jsonl`, vault file backend, Postgres opt-in.
- Config generated from `WorkforceConfig::default_toml()` (`settings.rs:~10980`), **not** the
  repo-root `workforce.toml` (it sets `[weaviate] enabled = true` → `state.rs:492` upgrades to
  Weaviate and fails closed). Bind admin/gateway to `127.0.0.1` on free ports.
- **[change]** vault master key from the macOS keychain (passed as `WORKFORCE_VAULT_KEY`) instead
  of `vault-key.txt`; one owner identity created on first run; Desk keeps the admin token in keychain.
- Local agents: created through the existing `/api/agents/*` CRUD + identity files; per-agent
  `config.toml` hot-reloads. Models: the normal provider grammar incl. `claude-code`, `codex`,
  `chatgpt` providers; logins in the local layered vault.

### 2.2 Remote hubs — [exists] + [change]
- Pair Desk as a device: `/api/pair/device/request` → approve → `/api/auth/link-device` (binds
  identity) → `wfp_`. Chat panes use the same `/ws` protocol as the web UI (`stream_delta`,
  `tool_start/end`, `permission_prompt` → `permission_response`). **Not** `agent.chat`/`agent.poll`
  (poll returns "unknown turn" on 2-replica hubs).
- `wfr_` token only for tool proxying (`wf.*`), reusing local-mcp's `hubs.rs`/`remote.rs` store, moved
  to keychain.
- **[change]** `/ws` anchors every connection on one session per `(agent, channel, owner)` with
  the iOS channel, so Desk would share the iPhone's session. Add a `channel=desk` (and per-pane
  session id) to the WS handshake.

### 2.3 Roster and name addressing — [new]
- Roster = local hub agents ∪ each remote hub's `/api/agents` ∪ live desk sessions (each CC/Codex
  session gets a name, e.g. `cc-auth-fix`).
- Entry: name, home (`local` / hub label), kind (workforce / claude-code / codex), state
  (idle / working / blocked / done), unread.
- Address by name everywhere: `@atlas …` in the quick-ask palette (global hotkey), voice
  "Atlas, …", Tuple wake phrase. Collision → `atlas@acme`; unqualified resolves local > pinned >
  ask.

### 2.4 Claude Code / Codex sessions ("desk sessions") — [change] hub, [new] module
Owned by the local hub (`[desk] enabled` kill switch, off by default), not by the app, so that
(a) any agent can start/drive/read them as tools, (b) they survive the window closing,
(c) one place owns worktrees.

- **Interactive mode (default)**: `portable-pty` running the user's installed `claude` / `codex` TUI
  in a worktree from `projects::work_session` [exists] (bare base + worktrees + credential helper).
  Desk attaches over the local hub WS (binary frames); the hub keeps a scrollback ring buffer so
  re-attach / a new window gets history.
- **State, Claude Code**: per-spawn `--settings '<json>'` injecting `http` hooks (SessionStart,
  UserPromptSubmit, Notification `permission_prompt|idle_prompt|agent_needs_input`, Stop,
  StopFailure, SessionEnd) that POST to a loopback endpoint with a per-session token. No edits to
  `~/.claude/settings.json`. **Verified 2026-10-26** on CLI 2.1.310, real interactive TUI in a
  PTY (`hooktest2/run.py` in the session scratchpad):
  - `command` hooks: SessionStart, UserPromptSubmit, PreToolUse, PermissionRequest,
    PostToolUse, Stop, SessionEnd all fire. Payload carries `session_id`, `transcript_path`,
    `cwd`, `permission_mode`, `prompt`.
  - `http` hooks: the same events fire **except SessionStart** → use a `command` hook (or a
    tiny `desk-hook` binary) for SessionStart, `http` for the rest.
  - **Approve from outside works**: a PermissionRequest hook answering
    `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}`
    let the command run with no keystroke (0.6 s later).
  - **Scrub the environment**: a child spawned from inside another CC session inherits
    `CLAUDE_CODE_CHILD_SESSION` etc. → transcript saving silently OFF and auto mode on. Desk
    spawns with every `CLAUDE*` var removed and an explicit `--permission-mode`.
  - Folder-trust dialog defaults to "No, exit": pre-trust Desk's worktree root or surface the
    dialog in the session card.
  - Open: while a PermissionRequest hook is pending the TUI shows no dialog, so a slow human
    approval elsewhere leaves the terminal silent. Pick per session: answer instantly with no
    decision (TUI dialog, Desk mirrors it) vs hold with a long hook `timeout` (Desk/voice/phone
    approve). Notification `idle_prompt` not yet observed.
- **State, Codex**: hub runs `codex app-server daemon`, launches the TUI with
  `codex --remote unix://…` so the hub subscribes to the same app-server events
  (`turn/*`, `item/*`, `…/requestApproval`) — structured state with no screen scraping.
  Fallback: Codex `hooks.json` + `[features] hooks = true`.
- **Other CLIs** (Gemini, OpenCode, Goose…): ACP via the `agent-client-protocol` 3.5 crate;
  herdr-style screen manifests only as last resort.
- **Driven mode** (an agent or voice is the driver): CC `-p --input-format stream-json
  --output-format stream-json` + control protocol; Codex app-server `turn/start` / `turn/steer` /
  `turn/interrupt`. This is what `coding.session` / `coding_engine/{claude_code,codex}.rs` already do
  [exists]. Hand-off between modes by session id (`--session-id` / `--resume`, Codex
  `thread/resume`); one mode at a time per session.
- **Avoid `--bare`**: it skips OAuth/keychain and is slated to become the `-p` default → pin flags
  explicitly and watch CLI releases.
- **ToS**: always the user's own installed `claude` with their own login (permitted, individual
  scale). Never put a subscription OAuth token in workforce's own HTTP client; if Desk is ever
  shipped publicly, no claude.ai login flow inside it.
- **Session registry + restore after restart** [new]: the hub persists every session it hosts
  (`desk_sessions`: engine, CLI session id, cwd/worktree, branch, name, goal note, last status,
  last prompt, pane layout). At login the LaunchAgent hub offers "N sessions were open" →
  restore all/some = respawn each in its cwd with `claude --resume <id>` / `codex resume <id>`
  plus Desk's hooks. Sessions that were mid-turn come back idle, flagged "interrupted" with a
  one-click "continue". Not restored: processes the session had started (dev servers, watchers)
  and in-flight tool calls; scrollback comes back from the transcript, not the PTY.
- **Adopt sessions Desk didn't start**: `claude agents --json` (verified 10/27, CLI 2.1.310) lists
  every live CC session on the machine, interactive in any terminal and `--bg`, with `pid`,
  `cwd`, `sessionId`, `name`, `status` busy/idle. Desk polls it, shows them on the Floor
  read-only (state from transcript tail), and records them in the registry, so iTerm sessions
  are restorable too (restored into Desk). Codex: `~/.codex/sessions` rollouts.
- **"What is each one doing"**: per session card = goal note (user-set at start, editable),
  status ring, last prompt, current tool, pending question, and a 2-line "where it's at" digest
  regenerated on Stop from the transcript tail by the local hub's cheapest model.
- **Tools exposed** [new]: `desk.session.start {name, repo, kind, prompt}`, `.prompt`, `.read`,
  `.wait {until: idle|blocked|done}`, `.approve`, `.stop`, `.list` — herdr's socket-API verbs as
  workforce tools, so Allan/Atlas can fan out CC sessions.

### 2.5 Workforce harness inside CC/Codex — [exists] + [change]
- Every desk session gets the local hub's Session MCP (`McpSession`, loopback, random bearer,
  gated executor under the turn's principal — `mcp_session.rs`, `mcp_turn.rs:283`) via
  `--mcp-config` (CC) / `-c mcp_servers.workforce=…` (Codex).
- Tools: memory, notes, sessions, knowledge/codegraph, vault-backed adapters (Jira, Linear,
  Datadog, Metabase via the layered vault), agent messaging, `space.*`, `desk.session.*`, plus `wf.*`
  remote proxies.
- **One local memory = the hub's.** `workforce-local-mcp`'s separate `memory.db` is imported once
  and retired for desk sessions; local-mcp stays for standalone CC sessions but points at the
  local hub as its default hub.
- Check before building: the subscription-agents work flagged the Session MCP `DirectExecutor` as
  ungated and the `MCP_REGISTRY` OnceLock as serving the first agent's registry to every agent —
  confirm the B3 MCP-boundary fix is in workforce-public.

### 2.6 Shared context ("spaces") — [new]
Today memory is per (agent, user) (`memory_tool.rs:269-760`); there is no cross-agent recall path;
`agent_inbox.rs` is in-memory. A **space** is an opt-in shared context (typically per project):
- shared memory scope: memories written with `scope = space:<id>` are recalled by every member
  (uses `Memory.visibility`/team plumbing, `memory/src/types.rs:282`);
- blackboard: one shared notes document + an append-only activity log (session started, PR opened,
  decision recorded) shown in Desk's right rail;
- persistent inbox: `agent_inbox` → SQLite, addressed by roster name; delivered to CC/Codex sessions
  as a queued prompt when idle, or read via `inbox.read`;
- cross-session read: `sessions.read` across members of the space for the same user.
- Membership per agent in its `config.toml` (`[spaces] join = ["workforce"]`, hot-reload); desk
  sessions join the space of their repo by default.

### 2.7 Remote agent acts on the laptop ("node uplink") — [change]
Only existing path: `workforce local-agent connect` → `/ws/agent`, carrying `exec.run` only.
- **Precondition — identity binding (verified gap)**: `handle_local_agent_ws`
  (`gateway.rs:2895`) records no identity, the node id is client-chosen, and `select-agent local`
  (`commands.rs:1838`) only checks `is_connected`. Bind the node to the token's linked identity at
  hello; only that identity's sessions may target it; reject id collisions across identities.
- Generalise from `exec.run` to a tunnelled MCP tool surface over the same WS, reusing
  `McpEndpoint` (the worker `bridge.rs` already tunnels MCP the other way). Advertised tools chosen
  per hub by the user: `desk.session.*`, `fs.read/write` (allowed paths), `exec.run`,
  `local_agent.ask`.
- Every call is answered by the local hub's policy gate; approvals surface in Desk.
- Remote agents see `node.<laptop>.*` only in sessions of the bound user.
- Rollout note: enabling the uplink on a client's hub
  is an explicit decision, after DW.

### 2.8 Voice — [new] in the app, reusing hub voice config
Lives in the app process (mic permission belongs to the app bundle).
- Audio: VoiceProcessingIO (AEC + NS + AGC) through a small Swift shim; capture **and** TTS playback
  through the same unit so AEC has its echo reference (`cpal` has no voice-processing mode).
- Cascade (default, local): Silero VAD → **sherpa-onnx keyword spotting** with the roster names as
  plain-text keywords (regenerated when the roster changes, no training) → streaming STT
  (sherpa-onnx Parakeet; Apple SpeechAnalyzer optional) → hub turn on the named agent with
  `voice_turn` [exists: `voice_model`, `voice_thinking_budget_tokens`, TTS normalise] on a
  per-turn voice channel (don't repeat the shared-anchor-session trap) → streamed reply → TTS:
  Kokoro via sherpa-onnx locally, or the hub's `[llm.speech]` providers (Fish, ElevenLabs, OpenAI,
  Soniox; keys from the layered vault). Not Piper for bundling (maintained fork is GPL-3.0).
- Push-to-talk global hotkey as well as wake-by-name. After a name, stay in conversation until
  silence / "thanks".
- Long turns (CC/Codex take minutes): spoken ack immediately, completion announced when the turn
  ends; barge-in stops TTS and truncates to what was heard; suppress keyword hits while TTS plays.
- Engines behind a `VoiceEngine` trait: `cascade`; `gpt-live` (OpenAI client delegation → named
  agent); `gemini-live` (async tools); `dessa` slot.
- **Block Dessa (announced 2026-10-23)**: waitlist + researcher early access only; no licence, API,
  weights or tool calling published; claims to run on M4 Max. Not usable yet, and without tool
  calling it can only be the conversational surface, delegating via a cascade. Keep the slot.
- Reuse: `voice-bridge` `vad.rs` / `chunker.rs` / END_MARKERS live in private
  `workforce-edge-private` → lift into a public `workforce-voice` crate (already the ESP32 plan).

### 2.9 Tuple — [new], thin
- Listen: Desk installs `~/.tuple/triggers/call-capture-started` (and `-complete`); on start Desk
  runs `tuple capture follow --wake-on <name> …` (names from the roster) → finalized transcript
  chunk + recent call context → the named agent.
- Reply: text via Tuple MCP `message_user` (Tuple chat panel); spoken via TTS into a virtual mic
  (BlackHole aggregate selected as Tuple's input). Suppress wake hits during playback
  (self-trigger).
- Launch: register `[harness.workforce]` in `~/.tuple/cli.toml` (command template
  `desk ask --prompt {PROMPT}`) so Tuple's "Connect a Local Agent" opens a Desk session with call
  context.
- Agents get Tuple's MCP (`tuple mcp`) through the hub's upstream `[mcp]` [exists].
- Limits: Tuple has no bot participant or audio-injection API, so the agent speaks as you; Capture
  notifies participants.

### 2.10 Credentials — per house rule
- Integration keys: local hub layered vault (4 levels) + UI presets, never env/config.
- Infra secrets: vault master key, admin token, `wfp_`/`wfr_` per remote hub → macOS keychain.
- Interactive CLI sessions use the CLIs' own logins; hub-driven turns use vault
  `claude_code/oauth_token` / `codex/auth_json` as today.

### 2.11 Rich content in chat — [exists] in Edge, [change] to share it
Edge already renders six fenced-JSON block kinds inside replies
(`workforce-edge-public/crates/edge/widget-ui/src/app/rich/`): `wf-table` (sort/filter/page, large
results by `artifact_id`), `wf-chart` (ECharts: pie/bar/line/area/scatter), `wf-form` (typed
fields, client validation, server-backed typeahead; submit hands summary + exact values back to the
conversation), `wf-map`, `wf-diagram` (SVG/Excalidraw export), `wf-image`. Validated before render,
skeleton while streaming, falls back to the code block when invalid; heavy renderers are lazy
chunks. The model is taught the contract by `edge/src/rich_output.rs::system_block`, only for the
features the agent has enabled.

Core today: the hub has the Rust port of the `wf-chart` validator + SVG renderer
(`agent-runtime/src/tools/chart/`, golden corpus `fixtures/charts/`) for Slack/email/iOS; the web
UI renders delivered files only, not `wf-*` fences; core agents are never taught the fences.

- **[change] one renderer**: extract `widget-ui/src/app/rich/` into a shared package
  (`@workforce/rich`, Elastic-2.0 like both repos) consumed by the Edge widget, core `ui/` and
  Desk. Its host contexts (`EdgeApiContext` for table pages / typeahead, `EdgeSubmitContext` for
  form submit) become an interface each host implements. Edge is React 19, core `ui/` React 18 →
  core `ui/` moves to 19 as part of the extraction; Desk starts on 19.
- **[change] teach core agents**: move `rich_output.rs` into the core hub; per-agent
  `[rich] tables/charts/forms/maps/diagrams/images` in `config.toml` (hot-reload, the Edge flag
  names). The block is injected only when the turn's channel declares it can render fences —
  Desk and the web UI advertise `rich=[…]` in the `/ws` handshake; Slack/email keep getting
  rendered images via the existing `chart` tool path.
- **[change] form submit over `/ws`**: a submitted form becomes the next user message carrying
  both the human summary and the machine values (Edge has its own route today); the agent's next
  turn supersedes the locked form, as in Edge.
- **[change] table pages / typeahead**: the spilled-table page route Edge uses must exist on the
  core hub's REST API (check what core already serves for `artifact_id` tabular spills).
- Remote hubs render rich blocks only once they run a hub with these changes: your own hub is a normal
  deploy; a client's hub is an explicit decision. Until then remote replies degrade to
  markdown + delivered images.

### 2.12 Rooms (Slack-like surface) — [new]
Single-user: you plus agents, not a team chat; no change to visibility rules.
- **DMs and rooms** in the left sidebar. A DM = you + one agent (local, remote, or a desk
  session). A room = you + several members across hubs; each member replies under its own name
  and avatar.
- **Routing**: `@name` addresses a member (roster name resolution, §2.3); agents can @-mention
  each other — delivered through the persistent inbox (§2.6) for local members and as a hub turn
  on the remote hub for remote members; their replies are posted back into the room. Unaddressed
  messages go to the room's default member (configurable) or nobody (notes to self).
- **Threads** off any message; long runs (desk sessions, workflows) post a single status message
  that updates in place (working → blocked → done) with the detail in its thread.
- Unread counts, mention badges, search across rooms (local hub `sessions` search + each remote
  hub's), pinned messages, rich blocks (§2.11) everywhere.
- **A room is the face of a space** (§2.6): its members share the space's memory scope,
  blackboard and activity log; creating a room creates a space, and vice versa.
- **Storage**: rooms, membership and the room timeline live on the local hub (SQLite). Each
  remote member's turns stay in that hub's own session (`channel=desk`, per-room session id), so
  the remote hub keeps its history and audit; the room stores references + rendered copies.
- **Loop guard**: agent→agent mentions carry a hop count and a per-room turn budget, so two agents
  can't ping-pong indefinitely.

## 3. The app (Tauri) — keep unison-editor's patterns, fix its defects

Keep: Tauri 2, React + zustand, xterm.js with the **DOM renderer** (webviews cap WebGL contexts),
Monaco (diff view), CSS-variable theming shared by UI/xterm/Monaco, shortcut registry that feeds the
command palette, per-workspace layout state saved debounced, per-panel ErrorBoundary,
macOS GUI PATH import (use the login shell `$SHELL -l -i`), sign + notarize release workflow.

Chat UI: extract the web UI's chat components (`ui/`) into a shared package and use them — don't
fork a second chat UI. Match `ui/`'s React version.

Requirements that unison-editor gets wrong (`ucm_pty.rs`, `commands.rs`, `mcp_client.rs`):
- own the `Child`: kill, wait, exit code, restart policy (it drops `_child` at spawn);
- `HashMap<SessionId, Session>` with cancellation tokens + JoinHandles, no singletons, no
  hard-coded ports, no bind-and-release port races;
- one Tauri `ipc::Channel` per session for bytes (not global `emit` of `Vec<u8>` as JSON arrays);
- backend scrollback ring buffer (here: in the hub);
- async JSON-RPC reader that routes responses by id and streams notifications (no sync commands
  holding a mutex around `read_line`);
- split the god `App.tsx`; one settings store (files, not localStorage + files); `csp` set; tests
  in CI.

Layout: left sidebar = rooms + DMs (unread, mentions) over the roster (Local / per hub /
Sessions, state dots); centre tabs + splits of room/chat (threads, rich blocks), terminal and diff
panes; right rail = space blackboard + activity + approvals inbox; menu-bar
status; notifications on blocked/done; ⌥Space quick-ask.

### 3.1 Identity and theming — [new]
unison-editor is borrowed for **engineering patterns only**; Desk does not look like it (no VS Code
grey/blue IDE chrome). Own identity:
- Wordmark `desk` + signal dot; app mark = two stacked bars. Type: Bricolage Grotesque (agent
  names, room titles, headings), Figtree (UI, messages), Geist Mono (sessions, terminals, code) —
  all OFL, bundled, no network fetch.
- Default themes **Night** (warm ink `#111210`, signal chartreuse `#D4FF3A`) and **Day** (paper
  `#F6F6F1`, ink buttons, signal as highlight), plus **Match system**.
- Every agent has a persistent mark colour (same on every hub, in voice and Tuple); status is a
  ring around the mark (dashed = working, solid amber = needs you, green = done), not a badge.
- Conversation first: messages without bubbles, rich blocks as raised cards, CC/Codex terminals
  as framed session cards beside the room (pop-out to their own window), not IDE panes.

Theming model (fixes unison-editor's dozens-of-explicit-keys JSON in localStorage):
- A theme is a small TOML file in `~/Library/Application Support/Desk/themes/<name>.toml`,
  watched and hot-applied. Required: `background`, `signal`; `text` automatic (overridable).
  Everything else — surface/raised/sunken/border, muted/faint, signal-soft, status colours,
  xterm 16-colour ANSI palette, Monaco theme, ECharts palette for rich blocks — is **derived**
  (OKLCH mixing toward text/background), and any token can be set under `[colors.override]`.
  `[type]` (display/ui/mono fonts, size) and `[shape]` (density compact|cozy, corners
  sharp|soft|round).
- One token set → CSS variables on `:root` (UI + `@workforce/rich` read the same vars), the xterm
  `ITheme`, `monaco.editor.defineTheme`, and an ECharts theme — so nothing is themed twice.
- Settings › Appearance: theme cards, three pickers, derived-token strip with per-token override,
  live preview, WCAG contrast checks (text/bg, muted/surface, faint/bg, button text/signal; warn
  below 4.5:1), Import (VS Code JSON, Ghostty, iTerm2 `.itermcolors`, base16 YAML → mapped to the
  three inputs + overrides), Export, Duplicate, Show in Finder.
- Optional per-hub tint (thin coloured edge on remote-hub content) so it is always clear where an
  agent runs.

Repo: new `driftwerk/workforce-desk`, depending on workforce-public via pinned git deps
(`gateway-client` protocol types etc.), like workforce-edge. Hub changes land in workforce-public.

## 4. Phases

1. **Foundation** — Desk shell; local hub LaunchAgent with generated loopback config + keychain;
   pair remote hubs (hub switcher); roster; chat panes (shared ui components, `channel=desk`);
   hub `[desk]` sessions: CC/Codex PTY panes in worktrees with hook/app-server state;
   session registry + restore after restart + adopt existing sessions; notifications; DMs; rich blocks (§2.11: shared `@workforce/rich` package, core `ui/` → React
   19, `rich_output` in the core hub, `rich=[…]` handshake, form submit over `/ws`).
2. **Harness in sessions** — Session MCP into every desk session; `desk.session.*` tools so agents
   drive sessions; driven/interactive hand-off; local-mcp convergence (one memory).
3. **Spaces + rooms** — shared memory scope, blackboard + activity log, persistent inbox,
   cross-session read; rooms as their UI (§2.12: multi-agent rooms, @mention routing, threads,
   search, loop guard).
4. **Node uplink** — identity binding first, then tunnelled MCP tools; your own hub first, clients' hubs by decision.
5. **Voice** — `workforce-voice` crate lift; VPIO shim; KWS by name; cascade; push-to-talk;
   ack/announce; barge-in.
6. **Tuple** — triggers, `capture follow --wake-on`, `message_user`, harness registration,
   BlackHole path.
7. **Speech-to-speech engines** — GPT-Live / Gemini Live delegation; Dessa when it ships with a
   usable API; diff-review → PR pane.

## 5. Not verified yet

- CC hooks in the TUI: verified 10/26 (§2.4); still open: approval UX while a PermissionRequest
  hook is pending, and Notification `idle_prompt` timing.
- Codex TUI `--remote` and a host sharing one live thread; app-server is labelled experimental.
- `claude --resume <id>` in a PTY with per-spawn `--settings` hooks re-attached (expected, not
  run); whether CC `--bg` sessions survive a reboot (they don't need to — the registry covers it).
- Whether the B3 Session-MCP boundary fix (gated executor, per-agent registry) is in workforce-public.
- Tuple Capture transcript latency; Tuple "managed session" / `set_attention` semantics (docs 404).
- VoiceProcessingIO details from Rust/Swift shim (Apple page didn't load); sherpa-onnx Kokoro TTS
  quality; Silero licence badge conflict.
- Block Dessa: licence, API, tool calling, release date — none published.
- Cost of moving core `ui/` from React 18 to 19 (fumadocs, xyflow, radix versions).
- Whether core already serves Edge's spilled-table page route for `artifact_id` tables/typeahead.
- How the Edge widget's form-submit message is shaped server-side, so the `/ws` port matches it.

## 7. As built — departures from the proposal

- **Local hub storage**: `desk-hubd` runs an embedded Postgres (`postgresql_embedded`,
  bundled) and points every store with a Postgres backend at it (house rule: all DB data in
  Postgres). Memory stays SQLite (no Postgres memory backend); Weaviate off at runtime.
- **Desk registry and rooms** are Postgres tables in the `desk` schema, owned by
  `crates/desk`; `[desk]` refuses to start without `[postgres]`.
- **Worktrees**: plain `git worktree add <repo>/.worktrees/<branch>` by the hub, not
  `projects::work_session` (that is for hub-cloned remote repos).
- **Codex**: one `codex app-server --listen unix://…` per session (WebSocket over a Unix
  socket, verified) — no port races; the TUI attaches with `--remote unix://…`, the hub joins
  as an observer. Approvals stay in the Codex TUI. Turn-level behaviour unverified until the
  local Codex login is fixed (`refresh token was already used`).
- **CC replies**: the Stop hook fires before the transcript holds the reply; the card uses
  the hook's `last_assistant_message`.
- **Approvals**: default `desk` (hook held up to `approval_timeout_secs`, answered from the
  card/Floor/tools; on timeout the TUI asks); per session `terminal` answers at once.
- **Agent tools**: `desk.sessions/start/prompt/read/wait/approve/stop`, `ShellExec` class.
  `desk.prompt` types into the live TUI (bracketed paste + Enter).
- **Rooms routing** runs in the app (it holds every hub's device token): each member gets its
  own conversation per room on channel `desk:<slug>`; the hub enforces the loop guard
  (≤3 hops, ≤12 agent messages since you last spoke).
- **Auth**: `/api/desk/*` is system-admin only (static token or SystemAdmin JWT); chat uses a
  paired-device token, because `/ws` refuses the admin token.
- **Secrets**: keychain in the app; with `DESK_HOME` set (dev), a 0600 `secrets.toml`, since
  every rebuilt unsigned binary is a new keychain identity.
- **Tuple** (CLI as of 10/27): `tuple --format json capture follow --batch 15s --wake-on <name>…`
  spans calls (no triggers needed). Requests = the principal's speech starting with a roster
  name, or their `agent_prompt`; others' speech is context only. Replies via
  `tuple notifications send`. There is no `message_user` any more.
- **Terminal screens**: each PTY's output is parsed into a `vt100` model (plus the modes it
  leaves out: `?1004`, `?2031`, kitty keyboard flags, modifyOtherKeys, the title). Attaching
  sends the screen with its history and modes; a viewer that falls behind gets the screen again
  (prefixed with CAN). One viewer is *active* (`{type:"claim"}`; the first to attach while none
  is); only its keys and size count, others mirror it via `{type:"size"}` / `{type:"role"}`.
  With no active viewer the hub answers DA1/DA2/DSR/DECRQM as xterm.js 6 would, and leaves
  XTVERSION, `CSI ? u` and OSC 7501 unanswered as xterm.js does.
- **PTY holder**: `workforce pty-holder --socket <path>` (token in `WORKFORCE_PTY_HOLDER_TOKEN`)
  owns the PTYs when `[desk] pty_holder_socket`/`pty_holder_token` are set; a restarted hub takes
  back the running sessions (same processes, hook port and tokens kept in the holder's per-PTY
  meta), Codex app-servers and terminals. Without it terminals are the hub's own, as before.
- **Terminals** (`/api/desk/terminals`): plain login shells with extra env from Desk; cwd
  followed (`proc_pidinfo`, OSC 7) and kept with the env in `desk.terminals`; reopened there
  after a reboot unless restore is `off`.
  Each shell gets `DESK_TERMINAL_ID`, `DESK_HOOK_TOKEN`, `DESK_HOOK_BASE` and `DESK_HOOK_URL`
  (`<hook base>/hook/terminal/<id>/<token>`); Desk's plugin dir is appended to the person's
  `CLAUDE_CODE_PLUGIN_DIRS`. A `claude` started there posts its hooks to that URL: SessionStart
  registers a Desk-origin session with `terminalId` (or takes back the one with that
  `session_id`), later hooks drive it like a Desk-launched session (approvals held up to a day),
  SessionEnd or the shell ending stops it, and the `claude agents` adopter leaves it alone.
- **Desk state** (`/api/desk/state/{key}`): a JSON value per key in `desk.state`, committed
  before the answer, announced as `{type:"state"}` (re-read on any `snapshot`).
- **Security fix found on the way**: `/ws/agent` accepted any token when device pairing was
  configured (`validate_token_async(..).is_ok()` on an `Ok(None)`); fixed with tests on
  `fix/ws-agent-token-auth`, not pushed.

## 6. Sources

- herdr: https://github.com/herdrdev/herdr · https://herdr.dev/docs/agents/ · https://herdr.dev/docs/socket-api/
- Munder Difflin (the "Dunder Mifflin" tool): https://github.com/HarnessMD/munder-difflin
- vibe-kanban executors (Rust precedent for CC stream-json, Codex app-server, ACP): https://github.com/BloopAI/vibe-kanban
- Claude Code headless / hooks: https://code.claude.com/docs/en/headless · https://code.claude.com/docs/en/hooks
- Codex app-server: https://learn.chatgpt.com/docs/app-server
- ACP: https://agentclientprotocol.com/protocol/overview · https://github.com/agentclientprotocol/rust-sdk
- Tauri channels: https://v2.tauri.app/develop/calling-frontend/
- Tuple CLI / capture / MCP / triggers: https://docs.tuple.app/cli/overview · https://docs.tuple.app/pairing-with-tuple/capturing-calls · https://docs.tuple.app/cli/mcp · https://docs.tuple.app/triggers/api-reference
- Block Dessa: https://block.xyz/dessa · https://techcrunch.com/2026/10/24/blocks-new-voice-model-sounds-eerily-human/
- sherpa-onnx: https://github.com/k2-fsa/sherpa-onnx · https://docs.rs/sherpa-onnx
- OpenAI Live delegation: https://developers.openai.com/api/docs/guides/live-delegation
- Gemini Live: https://blog.google/innovation-and-ai/technology/developers-tools/build-real-time-voice-applications-gemini-audio/
