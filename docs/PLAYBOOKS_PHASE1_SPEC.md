# Playbooks — Phase 1 Implementation Spec

Status: **draft for review** (2026-05-30). Phase 1 = the novel engineering. Phases 2–3 (agent tool, schema reference, CLI, REST) are deliberately out of scope here and are mostly copy-and-slim of the workflow ecosystem.

## 0. Recap of the model

A **Playbook** is an ordered list of step prompts injected, one at a time, into the user's **normal, persistent chat session**, gated by the user. Unlike workflows (fresh agent per step + JSON variable handoff), a playbook keeps one live session that accumulates context. The controller is thin: it decides *which prompt to inject next* and pins each step's output to disk so it survives compaction. The LLM loop, session store, and Slack send paths are unchanged — we wire a controller between them.

Core loop: `start` → inject step 1 prompt as a turn → user chats freely → `continue` (pin step 1 artifact, inject step 2 prompt) → … → last step `continue` completes → `exit` at any point clears the overlay and leaves the session intact.

## 1. New & touched files

| File | Change |
|---|---|
| `crates/common/src/playbook_state.rs` (new) | `PlaybookProgress`, `ArtifactRef` (runtime state; lives in `common` so `Session` can hold it without depending on `orchestrator`) |
| `crates/common/src/session.rs` | typed metadata accessors: `playbook()`, `set_playbook()`, `clear_playbook()` (mirror `seq()`/`set_seq()` pattern, key `"playbook"`) |
| `crates/orchestrator/src/playbook/definition.rs` (new) | `PlaybookDefinition`, `PlaybookStep`, `from_toml`/`from_file`/`load_dir`, validation |
| `crates/orchestrator/src/playbook/engine.rs` (new) | pure `advance()` / `start()` state-machine functions (mirror `workflow/engine.rs` purity) |
| `crates/orchestrator/src/playbook/mod.rs` (new) | re-exports |
| `crates/agent-runtime/src/rollover.rs` | one block in `seal_and_mint` to carry `metadata["playbook"]` to the successor |
| `crates/hub/src/playbook_runtime.rs` (new) | load `.driftwerk/playbooks/*.toml` into `Arc<RwLock<HashMap>>`, hot-reload via existing `FsWatcher`, expose `list/get` |
| `crates/hub/src/slack_manager.rs` | playbook interceptor in `ProcessMessage`; Continue/Exit on final reply; `playbook_continue`/`playbook_exit` button routing |
| `crates/hub/src/slack_blocks.rs` | `playbook_control_buttons(nonce)` |
| `crates/hub/src/serve.rs` | "## Active Playbook" block in `build_enhanced_prompt_with_context`; thread optional `blocks` through the final `send_to_surfaces` |
| `.driftwerk/playbooks/` (new dir) | TOML definitions |

## 2. Data model

```rust
// crates/common/src/playbook_state.rs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybookProgress {
    pub id: String,            // playbook definition id
    pub step: usize,           // 0-based index of the CURRENTLY-ACTIVE step
    pub nonce: String,         // regenerated each advance; guards stale button clicks
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    pub step_id: String,
    pub path: String,          // e.g. ".driftwerk/playbooks/<session>/design.md"
    pub synopsis: String,      // first heading / first ~200 chars, for prompt injection
}
```

Stored at `session.metadata["playbook"]` as JSON. Persisted to `.meta.json` for free (the whole `metadata` Value is serialized — verified in `SessionMeta::from_session`), so it survives restart.

`session.rs` accessors (mirror existing `seq`/`set_seq` at lines 533–543):

```rust
pub fn playbook(&self) -> Option<PlaybookProgress> {
    self.metadata.get("playbook").and_then(|v| serde_json::from_value(v.clone()).ok())
}
pub fn set_playbook(&mut self, p: &PlaybookProgress) {
    self.metadata_object_mut().insert("playbook".into(), serde_json::json!(p));
    self.updated_at = Utc::now();
}
pub fn clear_playbook(&mut self) {
    self.metadata_object_mut().remove("playbook");
    self.updated_at = Utc::now();
}
```

## 3. Definition format (slim TOML)

```toml
[playbook]
id = "design-impl-review"
name = "Design → Implement → Review"
description = "Human-in-the-loop feature build."

[[step]]
id = "design"
title = "Design the implementation"
artifact = "design.md"          # optional; controller appends a write-it instruction
prompt = """
You are designing a code implementation. Ask what the development task is and
which GitHub repos are involved, then use context.fuse, knowledge.*, code.*,
the github skill and coding.session to investigate and produce a plan...
"""

[[step]]
id = "implement"
title = "Implement"
artifact = "impl-summary.md"
prompt = "Implement the design in design.md. ..."

[[step]]
id = "review"
title = "Review"
prompt = "Review the implementation against design.md. ..."
```

`PlaybookDefinition`/`PlaybookStep` structs + `from_toml`/`load_dir` mirror `workflow/definition.rs:331–360`. Validation rules: ≥1 step; unique step ids; non-empty prompt; `id` matches filename on save.

## 4. Control flow — the interceptor (heart of Phase 1)

Placed in `slack_manager.rs` `ProcessMessage`, right after the access-gate block (~line 1380, same spot as the `cmd: cc` interceptor), **before** tokenization.

> **Structural cost — confirm before building.** Today `svc` (the bound `AgentService`) resolves *inside* the spawned task (~line 2434) and the session loads at ~2563. The interceptor needs both at ~1380, so this adds, to **every inbound message** on the hot path: (1) an early agent-binding resolve, and (2) a `sessions.get(&session_id)` / `get_or_create` metadata read — needed both to detect an active playbook and to catch a no-playbook `playbook start`. The read is a cache hit in `JsonlSessionStore` (in-memory `cache` lock, no disk), so it's cheap, but it is a genuine addition to per-message work. Confirm the binding resolution can be hoisted cleanly (it currently feeds the access gate at ~1335, so a resolved agent id is already in hand there — reuse it).

```
let progress = session.playbook();   // Option<PlaybookProgress>
let directive = playbook_directive(&text);   // parses start/continue/exit (see §4.1)

match (progress, directive) {
  // ── No active playbook ───────────────────────────────────────────
  (None, Start(id))      => start_playbook(id)        // §4.2  (rewrite text → step0 prompt, fall through)
  (None, Continue|Exit)  => fall through (plain words, normal chat)

  // ── Active playbook ──────────────────────────────────────────────
  (Some(p), Exit)        => { session.clear_playbook(); ack "Exited — context preserved"; continue (no turn) }
  (Some(p), Continue(n)) => advance(p, n)             // §4.3
  (Some(p), _)           => fall through (free-form chat; agent keeps full context)
}
```

### 4.1 `playbook_directive` (playbook-only vocabulary)

Recognises Continue/Exit **only via an explicit prefix or a button click — never a bare word.** Accepted: `/continue`, `cmd: continue`, `/exit`, `cmd: exit` (through the existing `command_body` helper), and the `playbook_continue` / `playbook_exit` buttons. A bare `continue` / `exit` in prose is **not** a directive.

> **Why no bare words:** the playbook overlay rides on the user's normal chat. A user mid-design typing "continue with that approach but reconsider X" must reach the agent as an ordinary message, not silently advance the step and swallow their text into the next turn. The button is the unambiguous primary path; the `cmd:`/`/` prefixes are the typed escape hatch. This removes a whole class of false-advance bugs.

Also recognised (only when **no** playbook is active): `playbook start <id>` / `playbook list` via the same prefix forms (`cmd: playbook start <id>`). Continue carries the button nonce when it came from a click. Anything else → `None` → normal chat. This is the **playbook-only interceptor** (chosen over extending `parse_workflow_command`): it is inert unless `session.playbook().is_some()` (except `start`/`list`).

### 4.2 Start

1. Look up definition; error to user if unknown.
2. Create scratch dir `.driftwerk/playbooks/<session_id>/`.
3. `session.set_playbook(PlaybookProgress { id, step: 0, nonce: new_nonce(text), artifacts: [] })`; save.
4. **Rewrite the inbound turn**: pass the full step prompt as the `message` arg (the LLM receives it via `task_content`, serve.rs:7042) and `display_content = "▶ Playbook '<name>' — Step 1/<n>: <title>"`. Fall through to the normal turn spawn → the agent runs step 1 on the full prompt, while only the lean label is **stored in session history** (`store_content`, serve.rs:6844). This is the same mechanism workflow steps use, and it's a feature: the giant step prompts never accumulate in history — the artifact files + system-prompt synopsis (§5.1) carry their substance forward instead.

   **In-channel visibility:** unlike the `/cc` path, the normal agent reply path renders **no** echo of the user turn — the channel shows only the user's own message and the agent's reply. So `display_content` affects the *stored transcript*, not what's posted. If we want a visible "▶ Step N: <title>" divider in the thread, the controller must **explicitly post** a one-line message before the turn (see open question §6.3).

`render_step_prompt` = step.prompt + (if `artifact`) a standard appended line: *"When this step is complete, write your output to `<scratch>/<artifact>` so later steps can build on it."* + (for step > 0) *"Prior artifacts: <list of paths>."*

### 4.3 Continue / advance

1. **Guards**: reject if `session.metadata["task_active"] == true` → ack "still working, try again when I've finished". Reject if `nonce` present and ≠ `p.nonce` → ack "that was an old button" (stale-click guard).
2. **Pin current step's artifact**: if the current step declared `artifact`, read `<scratch>/<artifact>`; build `ArtifactRef { step_id, path, synopsis = first_heading_or_200_chars(file) }`; push to `p.artifacts`. If the file is missing, still record the path with a `"(not written)"` synopsis and log — don't block.
3. **Advance**: `p.step += 1`. If `p.step >= def.steps.len()` → completion: `clear_playbook()`, ack "✅ Playbook complete", `continue` (no turn). Else regenerate `p.nonce`, `set_playbook(p)`, save.
4. **Inject next step**: full prompt as `message`, `display_content = "▶ Step <k>/<n>: <title>"` (same split as §4.2.4), fall through to the turn spawn.

`new_nonce()` is a fresh random token (uuid), **not** derived from the message — it only needs to be unguessable-enough to invalidate older buttons, and regenerates every advance.

`advance()`/`start()` decision logic lives as pure fns in `orchestrator/src/playbook/engine.rs`; `slack_manager.rs` does the IO (session save, text rewrite, scratch read).

## 5. The three integration touch-points

### 5.1 Artifact survival across compaction (§ the load-bearing constraint)

Two independent mechanisms, both needed:

- **Files** (`<scratch>/<step>.md`) — not in the token window until read; immune to compaction and restart.
- **System-prompt synopsis** — in `build_enhanced_prompt_with_context` (serve.rs ~5304, beside "Available Workflows"). This fn takes `session_id`, not the `Session`, so it loads the session itself (same self-sourcing as lessons/named-secrets). If `session.playbook()` is `Some`, push:

  ```
  ## Active Playbook: <name> — Step <k>/<n>: <title>
  You are mid-playbook. Prior step outputs are pinned on disk; read them when you need detail:
  - design: `.driftwerk/playbooks/<sess>/design.md` — <synopsis>
  ```

  Regenerated every turn, so it survives scrollback summarization — the agent never "forgets the artifact exists." Synopsis is the cheap file-derived string (no LLM call) in Phase 1.

### 5.2 Continue/Exit buttons on the final reply

- `slack_blocks.rs`: `playbook_control_buttons(nonce) -> Value` — an `actions` block with `Continue` (primary, `action_id: "playbook_continue"`) and `Exit` (danger, `action_id: "playbook_exit"`), `value: "<session_key>|<nonce>"`.
- `send_to_surfaces` currently hardcodes `blocks: None` (slack_manager.rs:3870). Add an optional `blocks: Option<Vec<Value>>` param (or a `send_to_surfaces_with_blocks` sibling). At the final-reply site (line 2865), after the run completes, reload the session; if `playbook().is_some()`, pass `Some(playbook_control_buttons(nonce))`. Buttons render under the agent's reply, always visible while a playbook is active and idle.
- Button routing: in the `InteractiveAction` arm (slack_manager.rs ~3097, beside `cc_stop`/`perm_*`), handle `playbook_continue` / `playbook_exit` by parsing `value` → `(session_key, nonce)` and driving the **same** advance/exit logic as the text path (factor §4.3 into a shared fn). A click is an *idle* signal (turn already finished), so it starts a new turn — it does **not** go through `inject_tx` (that path is for a *blocked* runner mid-turn).

### 5.3 Rollover carry

In `rollover.rs::seal_and_mint` (~line 133, where `label`/`context_type`/`model_override` are copied):

```rust
if let Some(pb) = current.metadata.get("playbook") {
    next.metadata_object_mut().insert("playbook".into(), pb.clone());
}
```

Without this the playbook silently dies the first time an implement step triggers rollover.

## 6. Edge cases & decisions

- **Stale click** → nonce mismatch rejected (§4.3.1).
- **Continue mid-turn** → `task_active` guard (§4.3.1).
- **Exit then manual** → `clear_playbook` leaves the session; user continues normally. Trivially correct because the session *is* the carrier.
- **Restart mid-playbook** → metadata persisted; next message re-detects. Buttons reappear on the next reply.
- **Last-step continue** → completion + clear.
- **Definition missing/renamed while active** → if `def` for `p.id` not found at advance time, ack "playbook definition '<id>' is gone — exiting" and `clear_playbook`.
- **Concurrency on the store** → all mutations are load-modify-save on the session; follow the existing `task_active` save pattern (serve.rs:7022). No new locks.

## 7. Test plan

- `playbook/definition.rs`: parse/validate unit tests (good TOML, dup ids, empty prompt, id/filename mismatch) — mirror workflow definition tests.
- `playbook/engine.rs`: pure `advance`/`start` tests (step increments, completion at last step, nonce regenerates, artifact appended).
- `session.rs`: `set_playbook`/`playbook`/`clear_playbook` round-trip + survives `SessionMeta` serialize/deserialize.
- `rollover.rs`: `seal_and_mint` carries `playbook` to successor; absent when not set.
- `command`/directive parsing: `continue`, `Continue`, `cmd: continue`, `/exit`, nonce extraction, non-directive falls through.

## 8. Out of scope (Phase 2/3)

`PlaybookTool` agent tool (start/list/status/exit/validate/save/get/schema), `PLAYBOOK_SCHEMA_REFERENCE`, "Available Playbooks" authoring section, CLI, REST, LLM-generated richer synopses, playbook↔workflow composition.

---

## Resolved decisions (2026-05-30)

1. **Phase-1 start path** — ✅ ship a minimal `playbook start <id>` / `playbook list` directive now (works as `cmd: playbook start <id>` in threads). Full `PlaybookTool` still deferred to Phase 2.
2. **Artifact instruction** — ✅ auto-append the "write your output to `<scratch>/<artifact>`" line when a step declares `artifact`.
3. **In-channel step marker** — ✅ the controller posts a one-line `▶ Step N: <title>` divider when advancing. Full prompt never shown in-channel; lean label stored in the transcript via `display_content`.
4. **Scratch location** — ✅ default `.driftwerk/playbooks/<session_id>/` (gitignored scratch); a step opts into the repo worktree only when the artifact is a deliverable.
