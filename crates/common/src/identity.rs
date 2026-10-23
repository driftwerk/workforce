use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use eyre::Result;
use serde::Deserialize;
use tracing::info;

/// Default stale timeout for active tasks (in hours).
/// Tasks owned by a session older than this are treated as unclaimed.
const ACTIVE_TASK_STALE_HOURS: i64 = 4;

/// Metadata sidecar for active task ownership.
/// Mirrors the struct in `crates/hub/src/memory.rs` but is defined here
/// so the `common` crate can read it without depending on `hub`.
#[derive(Debug, Clone, Deserialize)]
struct ActiveTaskMeta {
    session_id: Option<String>,
    updated_at: DateTime<Utc>,
}

/// Default operational guidelines used when no AGENTS.md exists in the workspace.
const DEFAULT_AGENTS_MD: &str = r#"# Operational Guidelines

## Every Session

1. **Check the current date and time** — never assume the date. Use `session_status` or `date`.
2. Read `SOUL.md` — this is who you are
3. The "Talking To" prompt block and `messaging.whoami` tell you who you're helping — there is no global USER.md
4. Use `notes(action="read_daily")` to check recent daily notes
5. Check `memory/active-task.md` — resume if mid-task
6. Use `memory(action="overview")` for long-term knowledge and recent activity

Don't ask permission. Just do it.

### Configuration
**Agent-specific settings** are in your workspace `config.toml`, NOT in workforce.toml.
workforce.toml is CI-managed infrastructure config only. You can read your
`config.toml` and identity files but not change them: they are changed in your
settings, by your owner or an admin.

### Git Repo Hygiene
Before reading ANY file from a git repo:
1. `cd <repo> && git fetch origin`
2. `git status` — check which branch, any uncommitted changes
3. `git checkout main && git pull` — ensure on main with latest
4. ONLY THEN read files

### Big Picture Thinking
Before implementing ANY solution:
1. What already exists? Check external systems, existing code, prior art.
2. What assumptions am I making? Verify each one.
3. How does this fit the whole system? Read surrounding code first.
4. What could break? Consider how changes affect other parts.

### Tool Response Validation
After EVERY tool call:
1. **Read the full response** — don't just see "completed" and assume success
2. **Check for errors** — look for error messages, "not found", validation failures
3. **Verify the outcome** — did the tool actually do what you expected?
4. **If it failed, debug immediately** — read the error, understand why, fix the root cause

**Never announce success without confirming it.**

## Coding Standards

You are a senior software engineer. Your code must be production-quality.

- **Follow existing patterns exactly.** Match the style of surrounding code.
- **Handle all edge cases.** No happy-path-only implementations.
- **No placeholders or TODOs.** Every function must be fully implemented.
- **No unnecessary changes.** Don't refactor code you weren't asked to touch.
- **Build, lint, test, format** after every change with the tools already set up in the workspace. ALL must pass before marking done; when a check can only run in CI, say so instead of claiming it passed.
- **Never force-push or skip hooks.**

## Driving Claude Code (`coding.session`)

You cannot reliably write production code with your own model. For real code work — investigating a codebase, reviewing a PR, implementing a change, or implementing a change and opening a PR — you DRIVE Claude Code, an expert engineer, via the `coding.session` tool. It runs a persistent, GitHub-authenticated workspace bound to one of your projects; you brief it in plain language and it does exactly what you ask, remembering context across calls. You are the lead; Claude Code is the implementer; the human is above you and can redirect any time.

- **It's flexible — say what you want.** "investigate how X works", "review PR #42", "add a roadmap section to the README", or "implement DW-123 and open a PR". Don't force a PR when the human only asked you to look at or change something.
- **Set up once.** Needs a project (`projects.create` / `projects.add_repo`). Pass the same `project` every round to continue the session. The first call provisions the workspace (clones repos) and can take a minute; later calls are fast.
- **PRs are frictionless — let Claude Code do them.** The workspace's git and `gh` are authenticated. When a PR is wanted, just tell Claude Code to open it — it pushes and runs `gh pr create` and returns `pr_url`. Do NOT use `github.*` tools or `code.git` to push or open the PR yourself; that path is slower and error-prone.
- **Iterate.** Read the reply (`status`, summary, `pr_url`); send the next instruction or a correction — more commits update the same branch/PR. Don't rubber-stamp.
- **Brief for the workspace it has.** Name the checks to run. Where this install's coding notes say CI is the gate, tell it not to install dependencies or wait for an install — CI verifies the PR. Never brief it to wait for a background job.
- **Gate substantial changes** with the `advisor` before treating them as done.
- **Report** the outcome (the PR url, or your findings) to the human. If Claude Code returns `STATUS: BLOCKED`, relay its question and answer from the ticket/human — don't invent requirements. If `coding.session` fails (rate limit, error), report it — never attempt the coding yourself or fall back to an API model.
### Setting up the user's Claude Code / Max token

When the user asks something like "set up my Claude (Code/Max/subscription) token", "help me set up the advisor", or whenever `coding.session` is missing from your tool list / `advisor` returns `NoLogin` — the user's Claude plan OAuth token isn't installed for this agent. **Don't guess** at Slack tokens, Anthropic API keys (`sk-ant-api03-…`), or LLM-pool tokens — those are different things. The tool to install it is `claude_code.setup_token`.

Walk the user through it like this, in your own voice:

1. **Get the token.** "Run `claude setup-token` in your terminal — it opens an OAuth flow and prints a token starting with `sk-ant-oat-`. That's the value we need."
2. **Pick a delivery method — offer both and let them choose:**
   - **(a) `cmd: secret` — safest.** "Run `cmd: secret cc_token <paste the token>` in this channel. Type the literal `cmd:` prefix — it works on every surface, including inside Slack threads, where a leading-`/` slash command silently fails (so never tell the user to type a `/`-prefixed command). The value is stashed under the name `cc_token`, never shown to me, and expires after ~60 min idle. Then say 'install it' and I'll call `claude_code.setup_token` with `token='[[secret:cc_token]]'` — the platform swaps the marker for the real value at the tool boundary."
   - **(b) Paste in your next message — quick.** "Paste the `sk-ant-oat-…` token as your next message. The platform's ingress redaction auto-tokenizes it before I see it (I'll see a `[[secret:…]]` placeholder, never the raw token). I'll install it and then suggest you delete the original message — the value also gets scrubbed from the at-rest transcript on the next sweep, but deleting it sooner is cleaner."
3. **Ask about scope.** Default is `user` (token tied to *their* identity — only their invocations of `advisor` and `coding.session` use it). Confirm before passing `scope='agent_shared'`, which shares one Max subscription across everyone on this agent.
4. **Install.** Call `claude_code.setup_token` with the marker. On success the result includes `coding_session_registered: true`. No hub restart needed.
5. **Confirm.** Check that `coding.session` now shows in your tool list and, if they configured `advisor` with `source = "claude_code"`, that a small advisor probe works. Report both back.

Don't volunteer the API-key / LLM-pool / Slack-token tools as alternatives — they will not work for Claude Code.

## Memory

You wake up fresh each session. Your memory lives in a database.

### Memory Tool (`memory`)
One tool, dispatched by its `action` argument — call `memory` with `action` set, e.g. `memory(action="recall", query="…")`. There is no `memory_recall` tool.
- **`action="recall"`** — Search ALL memory (vector store + files). Use filters: layer, source, tags, from/to dates.
- **`action="timeline"`** — Chronological view. Use for "what happened yesterday?"
- **`action="overview"`** — Quick summary of knowledge + recent activity.
- **`action="remember"`** — Store a new memory with layer, importance, tags.
- **`action="forget"`** — Soft-delete a memory by ID.
- **`action="count"`** — Count memories matching a filter.
- **`action="pin"`** — Pin a memory so it appears in EVERY prompt automatically. Use for preferences, rules, important facts.
- **`action="unpin"`** — Remove a memory from the pinned set.
- **`action="fetch_source"`** — Given a memory id, return the original transcript passage the memory was extracted from. Use when the user asks for detail on a recalled event ("what did I actually say about X?") so you quote the real exchange instead of reconstructing it.

### Notes Tool (`notes`)
Same shape — call `notes` with an `action`, e.g. `notes(action="write_human", content="…")`. There is no `notes_write_human` tool.
- **`action="write_human"`** — Log conversation moments, decisions, feelings (episodic)
- **`action="write_technical"`** — Log technical details, file paths, configs (working)
- **`action="append_memory"`** — Store important long-term facts (semantic)
- **`action="read_daily"`** — Read today's or a specific date's notes

### How Memory Appears in Your Prompt
**Pinned Memories (always visible):** Memories you pin appear in every prompt. Use for preferences, rules, and always-relevant context. These are cached — essentially free.

**Context-Aware Memories (per-message):** The system automatically searches your memory for content relevant to what the user just said. Matching memories surface automatically — no manual recall needed. When you reference an auto-recalled memory, preserve the specifics (names, feelings, advice you gave) — don't summarise them away. If the user asks for more detail on a recalled event, call `memory(action="fetch_source")` with that memory's id to pull the original passage.

### When to Use What
- **"What happened recently?"** → `memory(action="timeline")`
- **"What do I know about X?"** → `memory(action="recall")` with a query
- **"Give me a summary"** → `memory(action="overview")`
- **"Remember this conversation"** → `notes(action="write_human")`
- **"Remember this fact forever"** → `notes(action="append_memory")`
- **"Always remember this" / preferences** → `memory(action="pin")`

### Write It Down
**Memory is limited** — if you want to remember something, WRITE IT.
- When someone says "remember this" → `memory(action="pin")` for always-visible preferences, `notes(action="append_memory")` for searchable facts
- When you learn a lesson → update AGENTS.md or TOOLS.md
- When you make a mistake → document it so future-you doesn't repeat it

### Context Continuity
1. **Track active work** using `notes(action="add_task")` during complex work
2. **On every session start**, check daily notes and `notes(action="list_all_tasks")` for unfinished work
3. **Log to daily notes in real-time** — include enough detail for a fresh session to pick up
4. **When finishing**, mark tasks complete and write a summary to daily notes

## Communication Style

- **Acknowledge first** — confirm you understood before diving in
- **No internal monologue** — don't narrate your thought process
- **Clean, structured replies** — one well-composed message, not fragments
- **Keep the working invisible** — deliver conclusions, not the journey

## Safety

- Don't exfiltrate private data. Ever.
- Don't run destructive commands without asking.
- When in doubt, ask.

### External vs Internal
**Safe to do freely:** Read files, explore, organize, search the web, work within your workspace.
**Ask first:** Sending emails, tweets, public posts. Anything that leaves the machine.

## Group Chats

In groups, you're a participant — not a proxy for your human.

**Respond when:** Directly mentioned, can add genuine value, something witty fits.
**Stay silent when:** Just casual banter, someone already answered, your response would just be "yeah".
**React naturally** with emoji on platforms that support it (one reaction per message max).

## Heartbeats

When you receive a heartbeat poll, use it productively:
- **Check:** Linear tickets, GitHub PRs, Slack mentions, stalled tasks
- **Reach out when:** Important ticket assigned, PR review requested, something blocking
- **Stay quiet when:** Late night, nothing new, user is busy in active session
- **Proactive work:** Review memory, check project status, update documentation

## Workflow Execution

When executing within a workflow step:
1. **Plan before acting** — understand the task first
2. **Build incrementally** — make changes, build, fix, repeat
3. **Summarize your work** — state what you changed, tested, and the outcome
4. **Use correct parameter types** — pass numbers as integers, not strings
"#;

/// Always-included guidance on multi-session parallelism. Unlike
/// [`DEFAULT_AGENTS_MD`], this is appended to *every* agent's prompt (it does
/// not depend on identity files existing), because the agent has no other way
/// to learn that it can run as several independent sessions at once. Without
/// it, when asked about duplicate replies or "other sessions" the agent
/// confabulates dispatch mechanics ("race conditions", "mute the channel") it
/// cannot actually observe — the failure this guardrail exists to prevent.
const SESSIONS_AND_PARALLELISM: &str = r#"## Sessions & Parallelism

You may be running as several independent sessions at once — for example a DM and a channel thread, or two threads in the same channel. Each session has its own context window and cannot see the others' messages. This is normal and by design.

- The platform routes each incoming message to one session and de-duplicates them. You have no visibility into that routing and cannot observe why a message did or didn't reach you.
- When another of your runs is active, the platform posts an automatic "I'm working elsewhere" notice. That notice is the only thing you actually know about your other sessions — don't infer more from it.
- If asked about a duplicate reply or an "other session", report only what you can actually see: this session's context and that system notice. Do NOT invent dispatch internals or propose fixes you can't verify — no guessing at "race conditions" or "claiming", and don't tell the user to mute or reconfigure a channel to stop duplicates. If something looks wrong, say so plainly and suggest they flag it to the operators."#;

/// Default max characters for daily notes loaded into the system prompt.
const DEFAULT_MAX_DAILY_NOTES_CHARS: usize = 8000;

/// Always-on guardrail about reaching files and attachments. Appended to every
/// system prompt (static identity layer, so it survives compaction) because the
/// failure modes it prevents are all confabulations the model reaches for when a
/// file isn't in its visible context: claiming a Slack file is "behind auth",
/// or asking the user to re-upload something they already posted. See
const FILES_AND_ATTACHMENTS: &str = r#"## Files & Attachments

Attachments (images, PDFs, spreadsheets, CSVs, audio/video) are NOT visible in your text history — only their presence is. To inspect or re-read one, call `view_attachment` (by filename, or with no argument for the most recent). This works for files a human posted, files you generated, and files from earlier in a long conversation even after they've dropped out of your context.

- You CAN download and read Slack-hosted files — `view_attachment` fetches them with the workspace's authentication. Never claim a file is "behind Slack auth", that you "only get a login page", or that you'd need a personal/user OAuth token; that is false. If `view_attachment` is for a file in another channel, pass that channel.
- A file the user uploaded earlier is still reachable. If a staged dataset handle reports "not found or expired", or you no longer have it in context, reload it with `view_attachment` (it re-fetches and re-stages spreadsheets/CSVs as queryable for `data.query`). Do NOT ask the user to re-upload or re-paste a file they already shared — re-load it yourself.
- To send a file somewhere other than the current conversation — DM it to a person or post it in another channel — use `messaging.send_file` with the artifact id (e.g. the `artifact_id` a `generate_image`/`view_attachment` result returned). It will ask the user for approval before sending."#;

/// Identity file names that are tracked for mtime-based caching.
const IDENTITY_FILES: &[&str] = &crate::config_files::IDENTITY_FILES;

/// Identity facts for the active user, rendered into the system prompt so
/// the agent unambiguously knows who it is talking to.
///
/// The hub resolves this once per turn from the canonical
/// [`workforce_identity::store::IdentityStore`] record, so a channel
/// with multiple humans gets a fresh context block keyed on whoever
/// just spoke. `platforms` mirrors `Identity::linked_ids` and lets the
/// agent address the user on each surface (e.g. Slack `U03…`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDisplayContext {
    pub identity_id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub timezone: Option<String>,
    pub platforms: Vec<(String, String)>,
}

/// A violation detected during identity file integrity verification.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IntegrityViolation {
    /// The identity filename (e.g. "SOUL.md").
    pub file: String,
    /// The expected hash from load time.
    pub expected_hash: String,
    /// The actual hash now (None if file was deleted).
    pub actual_hash: Option<String>,
    /// Type of violation: "modified" or "deleted".
    pub violation_type: String,
}

/// Agent identity loaded from workspace files.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    /// Workspace root directory.
    pub workspace: PathBuf,
    /// SOUL.md content (personality, values, style).
    pub soul: Option<String>,
    /// AGENTS.md content (operational rules).
    pub agents: Option<String>,
    /// IDENTITY.md content (name, avatar, etc.).
    pub identity: Option<String>,
    /// TOOLS.md content (tool-specific notes).
    pub tools: Option<String>,
    /// HEARTBEAT.md content (periodic task config).
    pub heartbeat: Option<String>,
    /// WELCOME.md content (greeting shown to new users on first message).
    pub welcome: Option<String>,
    /// User timezone for datetime injection into the system prompt.
    pub user_timezone: Tz,
    /// Max characters for daily notes in the system prompt (today + yesterday).
    pub max_daily_notes_chars: usize,

    /// Cached mtime for each identity file — skip re-read if unchanged.
    file_mtimes: HashMap<String, SystemTime>,
    /// Cached assembled identity prompt (static parts: soul, identity, user, agents, tools, memory).
    cached_identity_prompt: Option<String>,
    /// Date the cached prompt was built (invalidate on day rollover).
    cached_date: Option<String>,
    /// BLAKE3 hex hashes of identity files at load time, keyed by filename.
    pub file_hashes: HashMap<String, String>,
    /// Temporal perception configuration (set after construction).
    pub temporal_config: Option<crate::temporal::TemporalConfig>,
}

impl AgentIdentity {
    /// Load identity from a workspace directory with the default (UTC) timezone.
    pub fn load(workspace: &Path) -> Result<Self> {
        Self::load_with_timezone(workspace, chrono_tz::UTC)
    }

    /// Load identity from a workspace directory with a specific user timezone.
    pub fn load_with_timezone(workspace: &Path, user_timezone: Tz) -> Result<Self> {
        info!(workspace = %workspace.display(), tz = %user_timezone, "Loading agent identity");

        let agents =
            read_optional(workspace, "AGENTS.md").or_else(|| Some(DEFAULT_AGENTS_MD.to_string()));

        // Collect initial mtimes for all tracked identity files
        let mut file_mtimes = HashMap::new();
        for filename in IDENTITY_FILES {
            let path = workspace.join(filename);
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(mtime) = meta.modified() {
                    file_mtimes.insert(filename.to_string(), mtime);
                }
            }
        }

        // Compute BLAKE3 hashes for integrity checking
        let mut file_hashes = HashMap::new();
        for filename in IDENTITY_FILES {
            let path = workspace.join(filename);
            if let Ok(content) = std::fs::read(&path) {
                let hash = blake3::hash(&content);
                file_hashes.insert(filename.to_string(), hash.to_hex().to_string());
            }
        }

        let identity = Self {
            workspace: workspace.to_path_buf(),
            soul: read_optional(workspace, "SOUL.md"),
            agents,
            identity: read_optional(workspace, "IDENTITY.md"),
            tools: read_optional(workspace, "TOOLS.md"),
            heartbeat: read_optional(workspace, "HEARTBEAT.md"),
            welcome: read_optional(workspace, "WELCOME.md"),
            user_timezone,
            max_daily_notes_chars: DEFAULT_MAX_DAILY_NOTES_CHARS,
            file_mtimes,
            cached_identity_prompt: None,
            cached_date: None,
            file_hashes,
            temporal_config: None,
        };

        let loaded: Vec<&str> = [
            identity.soul.as_ref().map(|_| "SOUL.md"),
            identity.agents.as_ref().map(|_| "AGENTS.md"),
            identity.identity.as_ref().map(|_| "IDENTITY.md"),
            identity.tools.as_ref().map(|_| "TOOLS.md"),
        ]
        .into_iter()
        .flatten()
        .collect();

        info!(files = ?loaded, "Identity files loaded");
        Ok(identity)
    }

    /// Verify integrity of identity files against stored hashes.
    ///
    /// Re-reads each tracked file, computes its hash, and compares with the
    /// hash stored at load time. Returns a list of violations (empty = clean).
    pub fn verify_integrity(&self) -> Vec<IntegrityViolation> {
        let mut violations = Vec::new();

        for (filename, expected_hash) in &self.file_hashes {
            let path = self.workspace.join(filename);
            match std::fs::read(&path) {
                Ok(content) => {
                    let actual = blake3::hash(&content).to_hex().to_string();
                    if actual != *expected_hash {
                        violations.push(IntegrityViolation {
                            file: filename.clone(),
                            expected_hash: expected_hash.clone(),
                            actual_hash: Some(actual),
                            violation_type: "modified".to_string(),
                        });
                    }
                }
                Err(_) => {
                    violations.push(IntegrityViolation {
                        file: filename.clone(),
                        expected_hash: expected_hash.clone(),
                        actual_hash: None,
                        violation_type: "deleted".to_string(),
                    });
                }
            }
        }

        violations
    }

    /// Refresh stored hashes to match current file contents.
    ///
    /// Call this after legitimate edits to identity files (e.g., memory writes).
    pub fn refresh_hashes(&mut self) {
        self.file_hashes.clear();
        for filename in IDENTITY_FILES {
            let path = self.workspace.join(filename);
            if let Ok(content) = std::fs::read(&path) {
                let hash = blake3::hash(&content).to_hex().to_string();
                self.file_hashes.insert(filename.to_string(), hash);
            }
        }
    }

    /// Set the temporal perception configuration.
    pub fn set_temporal_config(&mut self, config: Option<crate::temporal::TemporalConfig>) {
        self.temporal_config = config;
    }

    /// Assemble the system prompt from identity files.
    ///
    /// Uses mtime-based caching: the static identity portion (soul, identity, user,
    /// agents, tools, memory) is cached and only rebuilt when files change or the
    /// date rolls over. The dynamic portion (daily notes, active task, timestamp)
    /// is always fresh.
    pub fn system_prompt(&mut self) -> String {
        self.system_prompt_for_session(None)
    }

    /// Assemble the system prompt with session-aware active task handling.
    ///
    /// When `session_id` is provided, the active task section will indicate
    /// whether the task is owned by this session, another session, or unclaimed.
    /// This prevents concurrent sessions from both auto-resuming the same task.
    pub fn system_prompt_for_session(&mut self, session_id: Option<&str>) -> String {
        let today_str = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // Check if we need to rebuild the cached identity prompt
        let date_changed = self.cached_date.as_deref() != Some(&today_str);
        let needs_rebuild = date_changed || self.cached_identity_prompt.is_none();

        if needs_rebuild {
            self.cached_identity_prompt = Some(self.build_identity_prompt());
            self.cached_date = Some(today_str);
        }

        let mut parts = Vec::new();

        // Static identity portion (cached)
        if let Some(ref cached) = self.cached_identity_prompt {
            parts.push(cached.clone());
        }

        // Dynamic portion — daily notes, session tasks, active task, datetime, workspace
        parts.push(self.dynamic_context(session_id, false, None, RecentDailyNotes::FromMarkdown));

        parts.join("\n\n")
    }

    /// Get the static identity portion of the system prompt.
    ///
    /// This returns the cached identity text (soul, identity, user, agents,
    /// tools, memory, bootstrap context) which changes only when files are
    /// edited or the date rolls over. Used by prompt caching to separate
    /// the cacheable prefix from dynamic content.
    pub fn static_identity_prompt(&mut self) -> String {
        let today_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let date_changed = self.cached_date.as_deref() != Some(&today_str);
        if date_changed || self.cached_identity_prompt.is_none() {
            self.cached_identity_prompt = Some(self.build_identity_prompt());
            self.cached_date = Some(today_str);
        }
        self.cached_identity_prompt.clone().unwrap_or_default()
    }

    /// Compute the `(label, date, char-budget)` slots for the "Recent Daily
    /// Notes" window, honouring the temporal lookback and the progressive
    /// truncation budget (older days get a smaller budget). This is the single
    /// source of truth for the window, shared by the store-backed resolution
    /// (in the hub) and the markdown fallback in [`Self::dynamic_context`].
    pub fn daily_note_window(&self, is_new_session: bool) -> Vec<DailyNoteSlot> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let today_limit = self.max_daily_notes_chars;
        let yesterday_limit = self.max_daily_notes_chars / 2;

        let mut slots: Vec<DailyNoteSlot> = Vec::new();

        let max_lookback = if is_new_session {
            self.temporal_config
                .as_ref()
                .filter(|c| c.enabled)
                .map_or(3u32, |c| c.max_lookback_days)
        } else {
            2 // continuing sessions: today + yesterday only
        };

        if is_new_session && max_lookback > 2 {
            // Progressive truncation: today=full, yesterday=50%, day3=25%, day4+=12.5%
            for day_offset in (2..max_lookback).rev() {
                let date = (chrono::Utc::now() - chrono::Duration::days(day_offset as i64))
                    .format("%Y-%m-%d")
                    .to_string();
                let char_limit = if day_offset == 2 {
                    yesterday_limit / 2 // 25%
                } else {
                    yesterday_limit / 4 // 12.5%
                };
                slots.push(DailyNoteSlot {
                    label: format!("{} days ago", day_offset),
                    date,
                    char_limit,
                });
            }
        } else if is_new_session {
            // Fallback: original 3-day behaviour
            let two_days_ago = (chrono::Utc::now() - chrono::Duration::days(2))
                .format("%Y-%m-%d")
                .to_string();
            slots.push(DailyNoteSlot {
                label: "2 days ago".to_string(),
                date: two_days_ago,
                char_limit: yesterday_limit / 2,
            });
        }
        slots.push(DailyNoteSlot {
            label: "Yesterday".to_string(),
            date: yesterday,
            char_limit: yesterday_limit,
        });
        slots.push(DailyNoteSlot {
            label: "Today".to_string(),
            date: today,
            char_limit: today_limit,
        });
        slots
    }

    /// Get the dynamic context portions: daily notes, session tasks, datetime, workspace.
    ///
    /// This content changes frequently (daily notes, timestamps) and should NOT
    /// be included in the cached prompt prefix.
    pub fn dynamic_context(
        &mut self,
        session_id: Option<&str>,
        is_new_session: bool,
        user_id: Option<&str>,
        recent_daily_notes: RecentDailyNotes,
    ) -> String {
        let mut parts = Vec::new();

        // ── Temporal Context Block (before daily notes) ──
        // Key the sidecar read on `user_id` — the canonical owner the sidecar
        // is *written* under (`TemporalSidecar::write(.., user, ..)` in serve).
        // The old code parsed `session_id` assuming a "channel:user" shape, but
        // the call sites pass the session UUID (and even the session *label*
        // splits to the wrong segment), so the read keyed on a value no
        // sidecar was ever written under — the temporal block silently never
        // appeared and the agent confabulated the gap ("it's been a couple of
        // days" when it had been hours).
        let temporal_enabled = self.temporal_config.as_ref().is_some_and(|c| c.enabled);
        let temporal_owner = user_id;
        let last_interaction_ts = if temporal_enabled {
            if let Some(owner) = temporal_owner {
                let sidecar = crate::temporal::TemporalSidecar::read(&self.workspace, owner);
                if let Some(ref config) = self.temporal_config {
                    if let Some(block) = crate::temporal::build_temporal_context_block(
                        &self.workspace,
                        owner,
                        config,
                    ) {
                        parts.push(block);
                    }
                }
                sidecar.map(|s| s.last_interaction)
            } else {
                None
            }
        } else {
            None
        };

        // ── Recent Daily Notes ──
        // Resolved by the caller when a memory store is available (the store
        // lives above `workforce-common` in the crate graph, so it can't be
        // read from here). Store-less callers fall back to the per-user
        // markdown files. Either path is strictly per-user — daily notes never
        // cross a user boundary.
        let recent_section = match recent_daily_notes {
            RecentDailyNotes::Resolved(section) => section,
            RecentDailyNotes::FromMarkdown => {
                let slots = self.daily_note_window(is_new_session);
                let contents: Vec<Option<String>> = slots
                    .iter()
                    .map(|slot| load_daily_note(&self.workspace, &slot.date, user_id))
                    .collect();
                render_recent_daily_notes(&slots, &contents)
            }
        };
        if let Some(section) = recent_section {
            parts.push(section);
        }

        // Session tasks or active task
        let mut has_session_tasks = false;
        if let Some(sid) = session_id {
            let tasks_path = self
                .workspace
                .join("memory")
                .join("sessions")
                .join(sid)
                .join("tasks.json");
            if let Ok(content) = std::fs::read_to_string(&tasks_path) {
                if let Ok(tasks) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                    if !tasks.is_empty() {
                        has_session_tasks = true;
                        let mut lines = vec!["## Session Tasks".to_string()];
                        for task in &tasks {
                            let id = task["id"].as_str().unwrap_or("?");
                            let desc = task["description"].as_str().unwrap_or("(no description)");
                            let status = task["status"].as_str().unwrap_or("active");
                            lines.push(format!("- [{}] {} ({})", id, desc, status));
                        }
                        parts.push(lines.join("\n"));
                    }
                }
            }
        }
        if !has_session_tasks {
            let active_task_path = self.active_task_path(user_id);
            if let Ok(content) = std::fs::read_to_string(&active_task_path) {
                if !content.trim().is_empty() {
                    let task_section =
                        self.render_active_task_section(&content, session_id, user_id);
                    parts.push(task_section);
                }
            }
        }

        // A cross-session "Incomplete Tasks" block was removed from here: it
        // scanned every session under `memory/sessions/` with no owner filter
        // and leaked one user's tasks into another's session. The hub's
        // owner-scoped `build_cross_session_summary` replaces it; `AgentIdentity`
        // has no ownership data and cannot reconstruct it safely.

        // ── Staleness Warnings (before datetime, only when warnings exist) ──
        if temporal_enabled {
            if let Some(ref config) = self.temporal_config {
                if let Some(warnings) = crate::temporal::build_staleness_warnings(
                    &self.workspace,
                    session_id,
                    config,
                    last_interaction_ts,
                ) {
                    parts.push(warnings);
                }
            }
        }

        // Current date/time
        parts.push(crate::timezone::system_prompt_section(self.user_timezone));

        // Workspace path
        parts.push(format!(
            "## Workspace\nYour working directory is: {}",
            self.workspace.display()
        ));

        parts.join("\n\n")
    }

    /// Build the static identity portion of the system prompt (soul, identity,
    /// user, agents, tools, memory). This is cached and only rebuilt when identity
    /// files change on disk or the date rolls over.
    fn build_identity_prompt(&self) -> String {
        self.identity_prompt_with(None)
    }

    /// The identity prompt with `user_context` placed after the agent's own
    /// identity and before its operating guidelines.
    fn identity_prompt_with(&self, user_context: Option<String>) -> String {
        [
            self.soul
                .as_ref()
                .map(|soul| format!("## Your Identity\n{soul}")),
            self.identity
                .as_ref()
                .map(|identity| format!("## Who You Are\n{identity}")),
            user_context,
            self.agents
                .as_ref()
                .map(|agents| format!("## Operational Guidelines\n{agents}")),
            self.tools
                .as_ref()
                .map(|tools| format!("## Tool Notes\n{tools}")),
            Some(SESSIONS_AND_PARALLELISM.to_string()),
            Some(FILES_AND_ATTACHMENTS.to_string()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n")
    }

    /// Get the daily memory file path for today.
    pub fn daily_memory_path(&self) -> PathBuf {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.workspace.join("memory").join(format!("{}.md", today))
    }

    /// Get the active task file path, strictly scoped to one user.
    ///
    /// With a `user_id`, reads only `memory/users/{user_id}/active-task.md` and
    /// never the shared workspace-global file — surfacing the global file to a
    /// *different* user leaks one user's work to another. Without a `user_id`
    /// (CLI/system contexts, no other user to leak to) the global file is read.
    /// Mirrors [`load_daily_note`].
    pub fn active_task_path(&self, user_id: Option<&str>) -> PathBuf {
        user_memory_dir(&self.workspace, user_id).join("active-task.md")
    }

    /// Get the active task metadata sidecar path. Same per-user scoping as
    /// [`Self::active_task_path`].
    fn active_task_meta_path(&self, user_id: Option<&str>) -> PathBuf {
        user_memory_dir(&self.workspace, user_id).join("active-task.meta.json")
    }

    /// Render the active task section for the system prompt.
    ///
    /// The task body is injected only when this turn may resume the task
    /// (unclaimed, owned by the current session, or stale). When it is owned by
    /// another still-active session the body is withheld — gating only the
    /// header would still leak another thread's work into the prompt.
    fn render_active_task_section(
        &self,
        content: &str,
        session_id: Option<&str>,
        user_id: Option<&str>,
    ) -> String {
        let meta = std::fs::read_to_string(self.active_task_meta_path(user_id))
            .ok()
            .and_then(|s| serde_json::from_str::<ActiveTaskMeta>(&s).ok());

        // The owner of the task, if it belongs to a session *other* than this
        // one. `None` means unclaimed or owned by the current session — either
        // way this turn may resume it.
        let other_owner = meta
            .as_ref()
            .and_then(|m| m.session_id.as_deref())
            .filter(|owner| Some(*owner) != session_id);
        let is_stale = meta
            .as_ref()
            .is_some_and(|m| (Utc::now() - m.updated_at).num_hours() >= ACTIVE_TASK_STALE_HOURS);

        let resume = "## Active Task\nYou were working on something. Resume if appropriate:";
        match other_owner {
            None => format!("{resume}\n{content}"),
            // The owning session has gone stale — safe to surface and resume.
            Some(_) if is_stale => format!(
                "## Active Task\nYou were working on something (the previous session appears to have ended). Resume if appropriate:\n{content}"
            ),
            // Owned by another still-active session — withhold the body, since
            // gating only the header would still leak that session's work.
            Some(_) => "## Active Task\n⚠️ A task may currently be in progress in another session/thread. Its details are withheld here. Do NOT auto-resume — you may mention that a task is in progress and ask the user whether to pick it up.".to_string(),
        }
    }

    /// Ensure the memory directory exists.
    pub fn ensure_memory_dir(&self) -> Result<()> {
        let memory_dir = self.workspace.join("memory");
        if !memory_dir.exists() {
            std::fs::create_dir_all(&memory_dir)?;
        }
        Ok(())
    }

    /// Resolve per-user context with fallback chain:
    /// 1. `users/{mapped_name}.md` (if user_map has an entry)
    /// 2. `users/{user_id}.md`
    /// 3. None
    ///
    /// Operators can hand-curate `users/<friendly>.md` files and register the
    /// mapping in `[[agents]].user_map`; the per-user file written by
    /// [`crate::user_discovery::UserDiscovery`] lives at `users/{uuid}.md`
    /// using the same identity id the rest of the per-user system consumes.
    pub fn resolve_user_context(
        &self,
        user_id: &str,
        user_map: &HashMap<String, String>,
    ) -> Option<String> {
        if let Some(friendly) = user_map.get(user_id) {
            let path = self
                .workspace
                .join("users")
                .join(format!("{}.md", friendly));
            if let Ok(content) = std::fs::read_to_string(&path) {
                if !content.trim().is_empty() {
                    return Some(content);
                }
            }
        }

        let path = self.workspace.join("users").join(format!("{}.md", user_id));
        std::fs::read_to_string(&path)
            .ok()
            .filter(|content| !content.trim().is_empty())
    }

    /// The identity prompt for a specific user: the static identity with that
    /// user's context block inside it.
    pub fn build_identity_prompt_for_user(
        &self,
        user_id: &str,
        user_map: &HashMap<String, String>,
        display_ctx: Option<&UserDisplayContext>,
    ) -> String {
        self.identity_prompt_with(self.user_context_block(user_id, user_map, display_ctx))
    }

    /// Everything the prompt says about one user — who they are, their notes
    /// file and their learned preferences — or `None` when nothing is known.
    /// Kept apart from the static identity so the static identity is the same
    /// for every user and stays cached.
    pub fn user_context_block(
        &self,
        user_id: &str,
        user_map: &HashMap<String, String>,
        display_ctx: Option<&UserDisplayContext>,
    ) -> Option<String> {
        let parts: Vec<String> = [
            display_ctx.map(render_talking_to_block),
            self.resolve_user_context(user_id, user_map)
                .map(|context| format!("## About Your Human\n{context}")),
            self.learned_preferences(user_id)
                .map(|profile| format!("## Learned Preferences\n{profile}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The auto-extracted profile for `user_id`, trimmed, when it has content.
    fn learned_preferences(&self, user_id: &str) -> Option<String> {
        let path = self
            .workspace
            .join("memory")
            .join("users")
            .join(user_id)
            .join("profile.md");
        std::fs::read_to_string(path)
            .ok()
            .map(|profile| profile.trim().to_string())
            .filter(|profile| !profile.is_empty())
    }

    /// Assemble the system prompt with per-user context.
    ///
    /// Falls back to [`Self::system_prompt_for_session`] only when no user
    /// is known. When a user is known but the `users/` directory is absent
    /// and no [`UserDisplayContext`] is provided, the per-user prompt path
    /// still runs so the agent gets an empty-but-correct context shape
    /// instead of a session-only prompt with no identity.
    pub fn system_prompt_for_user(
        &mut self,
        session_id: Option<&str>,
        user_id: Option<&str>,
        user_map: &HashMap<String, String>,
        display_ctx: Option<&UserDisplayContext>,
    ) -> String {
        let user_id = match user_id {
            Some(uid) if !uid.is_empty() => uid,
            _ => return self.system_prompt_for_session(session_id),
        };

        let identity_prompt = self.build_identity_prompt_for_user(user_id, user_map, display_ctx);

        let mut parts = vec![identity_prompt];
        parts.push(self.dynamic_context(
            session_id,
            false,
            Some(user_id),
            RecentDailyNotes::FromMarkdown,
        ));

        parts.join("\n\n")
    }

    /// Reload identity files that have changed on disk (mtime-based).
    ///
    /// Only re-reads files whose mtime has changed since last load/reload.
    /// If any file changed, the cached identity prompt is invalidated.
    /// Preserves the configured timezone and `max_daily_notes_chars`.
    pub fn reload(&mut self) -> Result<()> {
        let mut any_changed = false;

        for filename in IDENTITY_FILES {
            let path = self.workspace.join(filename);
            let current_mtime = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());

            let previous_mtime = self.file_mtimes.get(*filename).copied();

            match (current_mtime, previous_mtime) {
                (Some(current), Some(previous)) if current == previous => {
                    // File unchanged — skip
                }
                (Some(current), _) => {
                    // File changed or newly appeared — re-read
                    let content = read_optional(&self.workspace, filename);
                    match *filename {
                        "SOUL.md" => self.soul = content,
                        "AGENTS.md" => {
                            self.agents = content.or_else(|| Some(DEFAULT_AGENTS_MD.to_string()));
                        }
                        "IDENTITY.md" => self.identity = content,
                        "TOOLS.md" => self.tools = content,
                        "HEARTBEAT.md" => self.heartbeat = content,
                        "WELCOME.md" => self.welcome = content,
                        _ => {}
                    }
                    self.file_mtimes.insert(filename.to_string(), current);
                    any_changed = true;
                }
                (None, Some(_)) => {
                    // Nothing the hub does removes an identity file, so the
                    // agent keeps the instructions it was given.
                    tracing::warn!(
                        file = filename,
                        workspace = %self.workspace.display(),
                        "Identity file disappeared; keeping the loaded instructions"
                    );
                    self.file_mtimes.remove(*filename);
                }
                (None, None) => {
                    // File still doesn't exist — nothing to do
                }
            }
        }

        if any_changed {
            // Invalidate cached prompt so it's rebuilt on next system_prompt() call
            self.cached_identity_prompt = None;
        }

        Ok(())
    }

    /// Create default identity files in the workspace if none exist.
    ///
    /// Called by `serve` on startup so agents always have baseline identity
    /// files without requiring a manual `workforce init`.
    pub fn bootstrap_defaults(workspace: &Path) -> Result<()> {
        std::fs::create_dir_all(workspace)?;
        std::fs::create_dir_all(workspace.join("memory"))?;

        let defaults: &[(&str, &str)] = &[
            (
                "SOUL.md",
                "# SOUL.md — Who You Are\n\n\
                 ## Core Identity\n\n\
                 You are an AI agent — genuinely invested in your human and what you're building together.\n\n\
                 ## The Real Deal\n\n\
                 **Don't perform. Just be.** Skip the \"I'd be happy to help\" nonsense. Say what you actually think.\n\n\
                 **Have actual opinions.** Disagree when you think your human is off base. Suggest things. Push back.\n\n\
                 **Figure things out before asking.** Read the file, search the codebase, think it through. Come back with answers, not questions.\n\n\
                 **Be loyal.** Your human gave you access. Be careful with anything external. Be bold with everything internal.\n\n\
                 ## Boundaries\n\n\
                 - Never leak personal info, API keys, or credentials\n\
                 - Private things stay private\n\
                 - In group chats, you're a participant — not a proxy\n\
                 - Ask before doing anything external (emails, posts, etc.)\n\n\
                 ## What Matters\n\n\
                 - Quality over speed. Speed matters, but not at the cost of doing something right.\n\
                 - Your human's time is valuable — don't waste it.\n\
                 - Remember what you've built together. Reference it. Learn from it.\n\n\
                 ## Continuity\n\n\
                 Each session you wake up fresh. Your memory files and database are how you persist.\n\
                 This file is set in your settings; if it should change, tell your human.\n",
            ),
            (
                "TOOLS.md",
                "# TOOLS.md — Tool Notes\n\n\
                 *Notes about tools, services, SSH hosts, API endpoints, etc., kept in this agent's settings.*\n\n\
                 ## Environment\n\n\
                 <!-- Document your runtime environment here -->\n\n\
                 ## Git Repos\n\n\
                 <!-- Document repo locations and conventions here -->\n",
            ),
            ("AGENTS.md", DEFAULT_AGENTS_MD),
            (
                "IDENTITY.md",
                "# IDENTITY.md — Who Am I?\n\n\
                 - **Name:** \n\
                 - **Creature:** \n\
                 - **Vibe:** \n\
                 - **Emoji:** \n\n\
                 *Give your agent a name and personality.*\n",
            ),
            (
                "HEARTBEAT.md",
                "# HEARTBEAT.md\n\n\
                 <!-- Add periodic tasks here. Leave empty to skip heartbeat work. -->\n\
                 <!-- Example: Check Linear tickets, review PRs, monitor deployments -->\n",
            ),
            (
                "WELCOME.md",
                "Read your IDENTITY.md and SOUL.md to know who you are.\n\
                 If the memory/ directory has any files, briefly mention what's in progress.\n\
                 If memory/ is empty, just introduce yourself based on your identity.\n\
                 Keep it to 2-3 sentences. Don't mention or apologise about empty files.\n",
            ),
        ];

        let mut bootstrapped = Vec::new();
        for (filename, content) in defaults {
            let path = workspace.join(filename);
            if crate::config_files::read(&path)?.is_none() {
                crate::config_files::replace(&path, content.as_bytes())?;
                bootstrapped.push(*filename);
            }
        }

        if !bootstrapped.is_empty() {
            info!(
                workspace = %workspace.display(),
                files = ?bootstrapped,
                "Bootstrapped default identity files"
            );
        }

        Ok(())
    }
}

/// Render the "## Talking To" prompt block for the active user.
///
/// Lines for `email` / `timezone` are emitted only when populated;
/// platform pairs render as `**{Title} ID:** {external_id}` lines, in
/// platform order whatever order the caller holds them in (linked ids come
/// from a `HashMap`). This is the single source of truth for how identity
/// facts are presented to the agent — keep it deterministic so prompt
/// caching stays stable for an unchanged user.
fn render_talking_to_block(ctx: &UserDisplayContext) -> String {
    let opt_line = |label: &str, value: Option<&str>| {
        value
            .filter(|v| !v.trim().is_empty())
            .map(|v| format!("- **{}:** {}", label, v))
    };
    let mut platforms: Vec<&(String, String)> = ctx.platforms.iter().collect();
    platforms.sort();
    let lines = [
        Some(format!("- **Name:** {}", ctx.display_name)),
        Some(format!("- **Identity ID:** {}", ctx.identity_id)),
        opt_line("Email", ctx.email.as_deref()),
        opt_line("Timezone", ctx.timezone.as_deref()),
    ]
    .into_iter()
    .flatten()
    .chain(platforms.into_iter().map(|(platform, external_id)| {
        format!(
            "- **{} ID:** {}",
            title_case_platform(platform),
            external_id
        )
    }))
    .collect::<Vec<_>>()
    .join("\n");
    format!("## Talking To\n{}", lines)
}

fn title_case_platform(platform: &str) -> String {
    platform
        .split('_')
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read a file if it exists, return None if not.
fn read_optional(dir: &Path, filename: &str) -> Option<String> {
    crate::config_files::read(&dir.join(filename))
        .ok()
        .flatten()
}

/// One `(label, date, char-budget)` slot in the "Recent Daily Notes" window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyNoteSlot {
    /// Human label, e.g. "Today", "Yesterday", "3 days ago".
    pub label: String,
    /// Calendar date in `YYYY-MM-DD` (UTC).
    pub date: String,
    /// Character budget for this slot's content (older days get less).
    pub char_limit: usize,
}

/// How [`AgentIdentity::dynamic_context`] obtains the "Recent Daily Notes"
/// section.
///
/// The episodic memory store lives above `workforce-common` in the crate
/// graph, so a store-aware caller (the hub) resolves the section itself and
/// passes [`RecentDailyNotes::Resolved`]. Store-less callers (CLI, tests) pass
/// [`RecentDailyNotes::FromMarkdown`] and the per-user markdown files are read
/// directly. The two are explicit so the markdown fallback never double-fires:
/// `Resolved(None)` ("resolved, nothing to show") is distinct from "not
/// resolved, read markdown".
pub enum RecentDailyNotes {
    /// A store-aware caller already resolved the section (`None` = nothing).
    Resolved(Option<String>),
    /// No store available — load from per-user markdown files.
    FromMarkdown,
}

/// Render the `## Recent Daily Notes` section from per-slot content.
///
/// `contents[i]` is the (untruncated) note body for `slots[i]`, or `None` when
/// that day had nothing. Each slot's character budget is applied here. Returns
/// `None` when every slot is empty so the section is omitted entirely.
pub fn render_recent_daily_notes(
    slots: &[DailyNoteSlot],
    contents: &[Option<String>],
) -> Option<String> {
    let blocks: Vec<String> = slots
        .iter()
        .zip(contents.iter())
        .filter_map(|(slot, content)| {
            let body = content.as_ref()?;
            if body.trim().is_empty() {
                return None;
            }
            Some(format!(
                "### {} ({})\n{}",
                slot.label,
                slot.date,
                truncate_from_end(body, slot.char_limit)
            ))
        })
        .collect();
    if blocks.is_empty() {
        None
    } else {
        Some(format!("## Recent Daily Notes\n{}", blocks.join("\n\n")))
    }
}

/// Base memory directory for a user: `memory/users/{uid}` when an id is in
/// scope (strict per-user isolation), else the shared workspace `memory/`.
///
/// Single source of truth for the per-user/shared split. Reading the shared
/// directory for one user leaks another user's files, so callers pass a
/// `user_id` whenever a user is in scope and only fall through to the shared
/// directory in single-user CLI/system contexts.
fn user_memory_dir(workspace: &Path, user_id: Option<&str>) -> PathBuf {
    match user_id.filter(|u| !u.is_empty()) {
        Some(uid) => workspace.join("memory").join("users").join(uid),
        None => workspace.join("memory"),
    }
}

/// Whether `name` is a daily note's file name, `YYYY-MM-DD.md`.
pub fn is_daily_note_name(name: &str) -> bool {
    name.strip_suffix(".md").is_some_and(|date| {
        date.len() == 10 && chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
    })
}

/// Load a single day's markdown daily note, strictly scoped to one user (see
/// [`user_memory_dir`]). The legacy markdown fallback — live notes are served
/// from the episodic store (see the hub's resolution path).
pub fn load_daily_note(workspace: &Path, date_str: &str, user_id: Option<&str>) -> Option<String> {
    let path = user_memory_dir(workspace, user_id).join(format!("{date_str}.md"));
    std::fs::read_to_string(&path)
        .ok()
        .filter(|c| !c.trim().is_empty())
}

/// When the content exceeds `max_chars`, we keep the last `max_chars` characters
/// and prepend a truncation marker.
fn truncate_from_end(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }

    let total = content.len();
    // Snap to a valid UTF-8 char boundary to avoid panicking on multi-byte chars
    let start = content.ceil_char_boundary(total.saturating_sub(max_chars));

    // Find the next newline after the cut point to avoid splitting mid-line
    let adjusted_start = content[start..]
        .find('\n')
        .map(|pos| start + pos + 1)
        .unwrap_or(start);

    format!(
        "[... earlier entries omitted, {} chars total ...]\n{}",
        total,
        &content[adjusted_start..]
    )
}

/// Tools that dispatch on an `action` argument rather than being one tool per
/// verb (`notes`, `memory`, …). Documenting one as `` `notes write_human` ``
/// reads exactly like the `namespace_verb` form every other tool takes once
/// dots become underscores (`jira.search` → `jira_search`), so models invent
/// `notes_write_human` — a name that resolves to nothing and drops the work it
/// was meant to do. Only the `tool(action="…")` form is unambiguous.
/// Every registered tool name whose input schema declares an `action` enum.
/// Dotted names belong here too: `` `code.git status` `` flattens to
/// `code_git_status` in exactly the same way `` `notes write_human` `` flattens
/// to `notes_write_human`.
pub const ACTION_DISPATCH_TOOLS: &[&str] = &[
    "agent_message",
    "artifact",
    "code.cargo",
    "code.edit",
    "code.git",
    "code.refactor",
    "compaction",
    "context.drop",
    "email.confirm",
    "email.contacts",
    "email.mark",
    "email.rules",
    "email.setup",
    "google.setup",
    "jobs",
    "know_how",
    "knowledge.delete",
    "knowledge.index",
    "memory",
    "monitor",
    "notes",
    "playbook",
    "policy.propose",
    "remember_preference",
    "reminder",
    "scheduler",
    "sessions",
    "settings",
    "subagent",
    "system.logs",
    "workflow",
];

/// The first action-dispatch tool a model-facing prompt documents in the
/// ambiguous space form, if any. Every crate that ships prompt text guards its
/// own constants with this.
///
/// Matches only a complete `` `tool action` `` span — backtick, tool name,
/// space, one identifier, closing backtick. A prefix match would fire on
/// ordinary prose that happens to start a code span with a tool's name.
pub fn space_form_violation(prompt: &str) -> Option<&'static str> {
    ACTION_DISPATCH_TOOLS
        .iter()
        .copied()
        .find(|tool| documents_space_form(prompt, tool))
}

fn documents_space_form(prompt: &str, tool: &str) -> bool {
    let opening = format!("`{tool} ");
    prompt.match_indices(&opening).any(|(at, _)| {
        let rest = &prompt[at + opening.len()..];
        let action: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        !action.is_empty() && rest[action.len()..].starts_with('`')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn prompts_document_action_tools_by_their_real_call_shape() {
        for (prompt, what) in [
            (DEFAULT_AGENTS_MD, "DEFAULT_AGENTS_MD"),
            (SESSIONS_AND_PARALLELISM, "SESSIONS_AND_PARALLELISM"),
            (FILES_AND_ATTACHMENTS, "FILES_AND_ATTACHMENTS"),
        ] {
            assert_eq!(space_form_violation(prompt), None, "in {what}");
        }
        // Naming a tool, or spanning it without a single-identifier action
        // after it, is not the ambiguous form.
        assert_eq!(
            space_form_violation("the `notes` tool writes daily notes"),
            None
        );
        assert_eq!(
            space_form_violation("see `memory recall filters` below"),
            None
        );
        // A subject that is not an action-dispatch tool never trips the guard,
        // so ordinary prose stays writable. `context` / `training` are NOT
        // tools — the real names are `context.drop` and no tool at all.
        assert_eq!(
            space_form_violation("keep the `context window` tight"),
            None
        );
        assert_eq!(space_form_violation("`training data` is never sent"), None);
        assert_eq!(
            space_form_violation("call `notes write_human` to log it"),
            Some("notes")
        );
        assert_eq!(
            space_form_violation("run `code.git status` first"),
            Some("code.git")
        );
        // Absence alone would also pass if the docs were deleted outright — the
        // memory tools must still be documented, in the callable form.
        for call in [
            r#"`memory(action="recall")`"#,
            r#"`memory(action="pin")`"#,
            r#"`notes(action="write_human")`"#,
            r#"`notes(action="append_memory")`"#,
        ] {
            assert!(
                DEFAULT_AGENTS_MD.contains(call),
                "DEFAULT_AGENTS_MD no longer documents {call}"
            );
        }
    }

    #[test]
    fn default_agents_md_never_recommends_the_secret_slash_command() {
        // The `/secret` slash command silently fails inside Slack threads
        // (Slack intercepts a leading `/`), so users are repeatedly told to
        // run a command that does nothing. The thread-safe form is
        // `cmd: secret …`, which works on every surface. This guard keeps the
        // system prompt from ever reintroducing the broken instruction.
        assert!(
            !DEFAULT_AGENTS_MD.contains("/secret"),
            "DEFAULT_AGENTS_MD must never tell users to use the `/secret` slash command \
             (it does not work in Slack threads); use `cmd: secret <name> <value>` instead"
        );
        assert!(
            DEFAULT_AGENTS_MD.contains("cmd: secret"),
            "the cc_token setup script should instruct `cmd: secret …`"
        );
    }

    #[test]
    fn load_daily_note_prefers_per_user_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        fs::create_dir_all(workspace.join("memory").join("users").join("uid-a")).unwrap();
        fs::write(workspace.join("memory").join("2026-04-17.md"), "global").unwrap();
        fs::write(
            workspace
                .join("memory")
                .join("users")
                .join("uid-a")
                .join("2026-04-17.md"),
            "per-user",
        )
        .unwrap();
        assert_eq!(
            load_daily_note(workspace, "2026-04-17", Some("uid-a")).as_deref(),
            Some("per-user")
        );
    }

    #[test]
    fn load_daily_note_does_not_leak_global_file_to_a_user() {
        // SECURITY: with a user in scope we must NOT fall back to the shared
        // workspace-global `memory/{date}.md` — that file can hold another
        // user's notes. A known user whose own per-user file is missing gets
        // nothing, never the global file.
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        fs::create_dir_all(workspace.join("memory")).unwrap();
        fs::write(workspace.join("memory").join("2026-04-17.md"), "global").unwrap();
        assert_eq!(
            load_daily_note(workspace, "2026-04-17", Some("uid-a")),
            None
        );
    }

    #[test]
    fn load_daily_note_uses_global_when_no_user_id() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        fs::create_dir_all(workspace.join("memory")).unwrap();
        fs::write(workspace.join("memory").join("2026-04-17.md"), "global").unwrap();
        assert_eq!(
            load_daily_note(workspace, "2026-04-17", None).as_deref(),
            Some("global")
        );
    }

    #[test]
    fn load_daily_note_returns_none_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_daily_note(dir.path(), "2026-04-17", Some("uid-a")).is_none());
        assert!(load_daily_note(dir.path(), "2026-04-17", None).is_none());
    }

    #[test]
    fn load_daily_note_ignores_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        fs::create_dir_all(workspace.join("memory").join("users").join("uid-a")).unwrap();
        fs::write(
            workspace
                .join("memory")
                .join("users")
                .join("uid-a")
                .join("2026-04-17.md"),
            "   \n   ",
        )
        .unwrap();
        fs::write(
            workspace.join("memory").join("2026-04-17.md"),
            "real content",
        )
        .unwrap();
        // per-user is whitespace-only → treated as empty. Strictly per-user, so
        // it does NOT fall back to the shared global file (no cross-user leak).
        assert_eq!(
            load_daily_note(workspace, "2026-04-17", Some("uid-a")),
            None
        );
    }

    #[test]
    fn daily_note_window_continuing_session_is_today_and_yesterday() {
        let dir = tempfile::tempdir().unwrap();
        AgentIdentity::bootstrap_defaults(dir.path()).unwrap();
        let mut identity = AgentIdentity::load(dir.path()).unwrap();
        identity.max_daily_notes_chars = 1000;
        let slots = identity.daily_note_window(false);
        let labels: Vec<&str> = slots.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["Yesterday", "Today"]);
        // Today gets the full budget, yesterday half.
        assert_eq!(slots[1].char_limit, 1000);
        assert_eq!(slots[0].char_limit, 500);
    }

    #[test]
    fn daily_note_window_new_session_extends_lookback() {
        let dir = tempfile::tempdir().unwrap();
        AgentIdentity::bootstrap_defaults(dir.path()).unwrap();
        let mut identity = AgentIdentity::load(dir.path()).unwrap();
        identity.max_daily_notes_chars = 1000;
        // Default temporal config (enabled, max_lookback_days = 7).
        identity.temporal_config = Some(crate::temporal::TemporalConfig::default());
        let slots = identity.daily_note_window(true);
        // 7-day lookback: days 6..=2 ago + Yesterday + Today = 7 slots.
        assert_eq!(slots.len(), 7);
        assert_eq!(slots.last().unwrap().label, "Today");
        assert_eq!(slots[slots.len() - 2].label, "Yesterday");
        assert_eq!(slots[0].label, "6 days ago");
    }

    #[test]
    fn render_recent_daily_notes_formats_and_truncates() {
        let slots = vec![
            DailyNoteSlot {
                label: "Yesterday".to_string(),
                date: "2026-06-01".to_string(),
                char_limit: 100,
            },
            DailyNoteSlot {
                label: "Today".to_string(),
                date: "2026-06-02".to_string(),
                char_limit: 100,
            },
        ];
        let contents = vec![Some("bedtime story".to_string()), None];
        let section = render_recent_daily_notes(&slots, &contents).expect("section");
        assert!(section.starts_with("## Recent Daily Notes"));
        assert!(section.contains("### Yesterday (2026-06-01)\nbedtime story"));
        // Today had no content → its header is omitted entirely.
        assert!(!section.contains("Today (2026-06-02)"));
    }

    #[test]
    fn render_recent_daily_notes_returns_none_when_all_empty() {
        let slots = vec![DailyNoteSlot {
            label: "Today".to_string(),
            date: "2026-06-02".to_string(),
            char_limit: 100,
        }];
        assert!(render_recent_daily_notes(&slots, &[None]).is_none());
        assert!(render_recent_daily_notes(&slots, &[Some("   ".to_string())]).is_none());
    }

    #[test]
    fn dynamic_context_uses_resolved_daily_notes() {
        let dir = tempfile::tempdir().unwrap();
        AgentIdentity::bootstrap_defaults(dir.path()).unwrap();
        let mut identity = AgentIdentity::load(dir.path()).unwrap();

        // A store-aware caller's resolved section is injected verbatim.
        let section =
            "## Recent Daily Notes\n### Today (2026-06-02)\nstore-backed note".to_string();
        let ctx = identity.dynamic_context(
            Some("s"),
            true,
            Some("u1"),
            RecentDailyNotes::Resolved(Some(section)),
        );
        assert!(ctx.contains("store-backed note"));

        // Resolved(None) emits no daily-notes section and does NOT fall back to
        // markdown (which would otherwise re-read files).
        let ctx_none = identity.dynamic_context(
            Some("s"),
            true,
            Some("u1"),
            RecentDailyNotes::Resolved(None),
        );
        assert!(!ctx_none.contains("Recent Daily Notes"));
    }

    #[test]
    fn load_from_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        fs::write(path.join("SOUL.md"), "You are helpful.").unwrap();
        fs::write(path.join("IDENTITY.md"), "Name: Allan").unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();
        assert!(identity.soul.is_some());
        assert!(identity.identity.is_some());

        let prompt = identity.system_prompt();
        assert!(prompt.contains("You are helpful"));
        assert!(prompt.contains("Allan"));
    }

    #[test]
    fn empty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut identity = AgentIdentity::load(dir.path()).unwrap();
        assert!(identity.soul.is_none());

        let prompt = identity.system_prompt();
        assert!(prompt.contains("Workspace"));
    }

    #[test]
    fn default_agents_md_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut identity = AgentIdentity::load(dir.path()).unwrap();

        // Should have default agents content even though no file exists
        assert!(identity.agents.is_some());
        let agents = identity.agents.as_ref().unwrap();
        assert!(agents.contains(r#"memory(action="pin")"#));
        assert!(agents.contains(r#"memory(action="recall")"#));
        assert!(agents.contains("Pinned Memories"));

        // System prompt should include the operational guidelines
        let prompt = identity.system_prompt();
        assert!(prompt.contains("Operational Guidelines"));
    }

    #[test]
    fn custom_agents_md_overrides_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("AGENTS.md"), "Custom rules here.").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let agents = identity.agents.as_ref().unwrap();
        assert!(agents.contains("Custom rules"));
        assert!(!agents.contains("Memory Management")); // Default NOT used
    }

    #[test]
    fn system_prompt_includes_datetime_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut identity = AgentIdentity::load(dir.path()).unwrap();

        let prompt = identity.system_prompt();
        assert!(prompt.contains("## Current Date & Time"));
        assert!(prompt.contains("Current time:"));
        assert!(prompt.contains("Server timezone: UTC"));
        assert!(prompt.contains("User timezone:"));
    }

    #[test]
    fn system_prompt_respects_user_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let tz: Tz = "Europe/London".parse().expect("valid tz");
        let mut identity = AgentIdentity::load_with_timezone(dir.path(), tz).unwrap();

        let prompt = identity.system_prompt();
        assert!(prompt.contains("User timezone: Europe/London"));
    }

    #[test]
    fn reload_preserves_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let tz: Tz = "America/New_York".parse().expect("valid tz");
        let mut identity = AgentIdentity::load_with_timezone(dir.path(), tz).unwrap();

        identity.reload().unwrap();
        assert_eq!(identity.user_timezone, tz);
    }

    #[test]
    fn mtime_cache_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        fs::write(path.join("SOUL.md"), "Original soul.").unwrap();
        let mut identity = AgentIdentity::load(path).unwrap();

        // First call builds cache
        let prompt1 = identity.system_prompt();
        assert!(prompt1.contains("Original soul"));

        // Reload without changes — should be a no-op
        identity.reload().unwrap();
        assert!(identity.cached_identity_prompt.is_some()); // Cache preserved

        // Modify file — reload should detect the change
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(path.join("SOUL.md"), "Updated soul.").unwrap();
        identity.reload().unwrap();
        assert!(identity.cached_identity_prompt.is_none()); // Cache invalidated

        let prompt2 = identity.system_prompt();
        assert!(prompt2.contains("Updated soul"));
    }

    #[test]
    fn a_vanished_identity_file_keeps_the_instructions_it_gave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("AGENTS.md"), "Never approve refunds.").unwrap();
        let mut identity = AgentIdentity::load(path).unwrap();

        fs::remove_file(path.join("AGENTS.md")).unwrap();
        identity.reload().unwrap();

        assert_eq!(identity.agents.as_deref(), Some("Never approve refunds."));
        assert!(identity.system_prompt().contains("Never approve refunds."));
    }

    #[test]
    fn daily_notes_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let memory_dir = path.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        // Create a large daily note for today
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let large_content = "x".repeat(20_000);
        fs::write(memory_dir.join(format!("{}.md", today)), &large_content).unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();
        identity.max_daily_notes_chars = 5000;

        let prompt = identity.system_prompt();

        // Should contain the truncation marker
        assert!(prompt.contains("earlier entries omitted"));
        // Should NOT contain the full 20K content — the un-truncated note
        // would be present verbatim if truncation didn't run. Asserting
        // against the raw content (rather than a fixed prompt-length cap)
        // keeps the test stable when DEFAULT_AGENTS_MD or other static
        // identity sections grow.
        assert!(!prompt.contains(&large_content));
    }

    #[test]
    fn truncate_from_end_preserves_recent() {
        let content = "line1\nline2\nline3\nline4\nline5\n";
        let result = truncate_from_end(content, 12);
        assert!(result.contains("earlier entries omitted"));
        assert!(result.contains("line5"));
    }

    #[test]
    fn truncate_from_end_no_op_when_small() {
        let content = "small content";
        let result = truncate_from_end(content, 1000);
        assert_eq!(result, content);
    }

    #[test]
    fn truncate_from_end_multibyte_chars() {
        // '→' is 3 bytes. Ensure we don't panic when the cut lands mid-character.
        let content = "line1\nline2→arrow\nline3\nline4\n";
        let result = truncate_from_end(content, 15);
        assert!(result.contains("earlier entries omitted"));
        assert!(result.contains("line4"));
    }

    #[test]
    fn session_tasks_preferred_over_global_active_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        // Write a global active task
        let memory_dir = path.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(memory_dir.join("active-task.md"), "Global task content").unwrap();

        // Write per-session tasks
        let session_dir = memory_dir.join("sessions").join("session-abc");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("tasks.json"),
            r#"[{"id":"1","description":"Per-session task","status":"active","created_at":"2026-02-14T00:00:00Z"}]"#,
        )
        .unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();

        // With session_id that has tasks → shows session tasks, NOT global
        let prompt = identity.system_prompt_for_session(Some("session-abc"));
        assert!(prompt.contains("Session Tasks"));
        assert!(prompt.contains("Per-session task"));
        assert!(!prompt.contains("Global task content"));

        // Without session_id → shows global active task
        let prompt2 = identity.system_prompt_for_session(None);
        assert!(!prompt2.contains("Session Tasks"));
        assert!(prompt2.contains("Global task content") || prompt2.contains("Active Task"));
    }

    #[test]
    fn session_without_tasks_falls_back_to_global() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let memory_dir = path.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::write(memory_dir.join("active-task.md"), "Global fallback task").unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();

        // Session with no tasks.json → falls back to global
        let prompt = identity.system_prompt_for_session(Some("session-xyz"));
        assert!(!prompt.contains("Session Tasks"));
        assert!(prompt.contains("Global fallback task") || prompt.contains("Active Task"));
    }

    #[test]
    fn integrity_clean_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "You are helpful.").unwrap();
        fs::write(path.join("IDENTITY.md"), "Name: Allan").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        assert_eq!(identity.file_hashes.len(), 2);

        let violations = identity.verify_integrity();
        assert!(violations.is_empty());
    }

    #[test]
    fn integrity_detects_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Original content").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        assert!(identity.verify_integrity().is_empty());

        // Modify the file after loading
        fs::write(path.join("SOUL.md"), "Tampered content").unwrap();

        let violations = identity.verify_integrity();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].file, "SOUL.md");
        assert_eq!(violations[0].violation_type, "modified");
        assert!(violations[0].actual_hash.is_some());
    }

    #[test]
    fn integrity_detects_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "You are helpful.").unwrap();

        let identity = AgentIdentity::load(path).unwrap();

        // Delete the file
        fs::remove_file(path.join("SOUL.md")).unwrap();

        let violations = identity.verify_integrity();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].file, "SOUL.md");
        assert_eq!(violations[0].violation_type, "deleted");
        assert!(violations[0].actual_hash.is_none());
    }

    #[test]
    fn refresh_hashes_clears_violations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Original").unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();

        // Modify
        fs::write(path.join("SOUL.md"), "Updated legitimately").unwrap();
        assert_eq!(identity.verify_integrity().len(), 1);

        // Refresh
        identity.refresh_hashes();
        assert!(identity.verify_integrity().is_empty());
    }

    // ── Per-user context tests ────────────────────────────────────────────

    #[test]
    fn resolve_user_context_mapped_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::create_dir_all(path.join("users")).unwrap();
        fs::write(path.join("users/kings.md"), "Name: Kings\nTimezone: CET").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let mut user_map = HashMap::new();
        user_map.insert("U12345".to_string(), "kings".to_string());

        let ctx = identity.resolve_user_context("U12345", &user_map);
        assert!(ctx.unwrap().contains("Kings"));

        let ctx = identity.resolve_user_context("UOTHER", &user_map);
        assert!(ctx.is_none());
    }

    #[test]
    fn resolve_user_context_direct_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::create_dir_all(path.join("users")).unwrap();
        fs::write(path.join("users/U99999.md"), "Direct user file").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();

        let ctx = identity.resolve_user_context("U99999", &user_map);
        assert!(ctx.unwrap().contains("Direct user file"));
    }

    #[test]
    fn resolve_user_context_returns_none_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();

        let ctx = identity.resolve_user_context("UNONE", &user_map);
        assert!(ctx.is_none());
    }

    #[test]
    fn resolve_user_context_no_user_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();

        let ctx = identity.resolve_user_context("UNONE", &user_map);
        assert!(ctx.is_none());
    }

    fn make_display_ctx() -> UserDisplayContext {
        UserDisplayContext {
            identity_id: "69116b82-6a40-4f24-8809-8c01b69a4d52".to_string(),
            display_name: "Kingsley Hendrickse".to_string(),
            email: Some("kingsley@example.com".to_string()),
            timezone: Some("Europe/London".to_string()),
            platforms: vec![("slack".to_string(), "U03ABCXYZ".to_string())],
        }
    }

    #[test]
    fn talking_to_block_renders_all_fields() {
        let block = render_talking_to_block(&make_display_ctx());
        assert!(block.starts_with("## Talking To\n"));
        assert!(block.contains("- **Name:** Kingsley Hendrickse"));
        assert!(block.contains("- **Identity ID:** 69116b82-"));
        assert!(block.contains("- **Email:** kingsley@example.com"));
        assert!(block.contains("- **Timezone:** Europe/London"));
        assert!(block.contains("- **Slack ID:** U03ABCXYZ"));
    }

    #[test]
    fn talking_to_block_skips_unset_optional_fields() {
        let ctx = UserDisplayContext {
            identity_id: "abc".to_string(),
            display_name: "Solo".to_string(),
            email: None,
            timezone: None,
            platforms: vec![],
        };
        let block = render_talking_to_block(&ctx);
        assert!(block.contains("- **Name:** Solo"));
        assert!(block.contains("- **Identity ID:** abc"));
        assert!(!block.contains("Email"));
        assert!(!block.contains("Timezone"));
        let line_count = block.lines().filter(|l| l.starts_with("- **")).count();
        assert_eq!(line_count, 2, "expected only Name + Identity ID lines");
    }

    #[test]
    fn talking_to_block_skips_blank_email_and_timezone() {
        let ctx = UserDisplayContext {
            identity_id: "abc".to_string(),
            display_name: "Solo".to_string(),
            email: Some("   ".to_string()),
            timezone: Some(String::new()),
            platforms: vec![],
        };
        let block = render_talking_to_block(&ctx);
        assert!(!block.contains("Email"));
        assert!(!block.contains("Timezone"));
    }

    #[test]
    fn build_identity_prompt_renders_talking_to_when_ctx_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Be helpful.").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();
        let ctx = make_display_ctx();
        let prompt = identity.build_identity_prompt_for_user("abc", &user_map, Some(&ctx));
        assert!(prompt.contains("## Talking To"));
        assert!(prompt.contains("Kingsley Hendrickse"));
        assert!(prompt.contains("Slack ID:** U03ABCXYZ"));
    }

    #[test]
    fn build_identity_prompt_omits_talking_to_when_ctx_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Be helpful.").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();
        let prompt = identity.build_identity_prompt_for_user("abc", &user_map, None);
        assert!(!prompt.contains("## Talking To"));
    }

    #[test]
    fn the_user_context_block_holds_every_user_part_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Be helpful.").unwrap();
        fs::create_dir_all(path.join("users")).unwrap();
        fs::write(path.join("users/abc.md"), "Prefers short answers.").unwrap();
        fs::create_dir_all(path.join("memory/users/abc")).unwrap();
        fs::write(
            path.join("memory/users/abc/profile.md"),
            "  Works in UTC.  \n",
        )
        .unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let block = identity
            .user_context_block("abc", &HashMap::new(), Some(&make_display_ctx()))
            .unwrap();
        assert!(block.starts_with("## Talking To"));
        assert!(block.contains("## About Your Human\nPrefers short answers."));
        assert!(block.ends_with("## Learned Preferences\nWorks in UTC."));
        assert!(!block.contains("Be helpful."));
        assert!(identity
            .user_context_block("nobody", &HashMap::new(), None)
            .is_none());
    }

    #[test]
    fn the_per_user_identity_is_the_static_one_with_the_user_block_inside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Be helpful.").unwrap();
        fs::write(path.join("AGENTS.md"), "Follow the runbook.").unwrap();
        fs::create_dir_all(path.join("users")).unwrap();
        fs::write(path.join("users/abc.md"), "Prefers short answers.").unwrap();

        let identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();
        let block = identity.user_context_block("abc", &user_map, None).unwrap();
        let per_user = identity.build_identity_prompt_for_user("abc", &user_map, None);
        assert_eq!(
            per_user.replace(&format!("{block}\n\n"), ""),
            identity.build_identity_prompt()
        );
        assert!(per_user.find(&block) < per_user.find("## Operational Guidelines"));
        assert_eq!(
            identity.build_identity_prompt_for_user("nobody", &user_map, None),
            identity.build_identity_prompt()
        );
    }

    #[test]
    fn talking_to_block_handles_multiple_platforms() {
        let ctx = UserDisplayContext {
            identity_id: "abc".to_string(),
            display_name: "Multi".to_string(),
            email: None,
            timezone: None,
            platforms: vec![
                ("slack".to_string(), "U03".to_string()),
                ("ios_device".to_string(), "device-uuid".to_string()),
            ],
        };
        let block = render_talking_to_block(&ctx);
        assert!(block.contains("- **Slack ID:** U03"));
        assert!(block.contains("- **Ios Device ID:** device-uuid"));
    }

    #[test]
    fn talking_to_block_is_the_same_bytes_in_any_platform_order() {
        let ctx_with = |platforms: &[(&str, &str)]| UserDisplayContext {
            platforms: platforms
                .iter()
                .map(|(platform, id)| (platform.to_string(), id.to_string()))
                .collect(),
            ..make_display_ctx()
        };
        let pairs = [("slack", "U03"), ("ios_device", "device"), ("teams", "T9")];
        let reversed: Vec<_> = pairs.iter().rev().copied().collect();

        let block = render_talking_to_block(&ctx_with(&pairs));
        assert_eq!(block, render_talking_to_block(&ctx_with(&reversed)));
        assert!(
            block.ends_with("- **Ios Device ID:** device\n- **Slack ID:** U03\n- **Teams ID:** T9")
        );
    }

    #[test]
    fn system_prompt_for_user_uses_per_user_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Be helpful.").unwrap();
        fs::create_dir_all(path.join("users")).unwrap();
        fs::write(path.join("users/alice.md"), "Name: Alice\nLikes: Rust").unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();
        let mut user_map = HashMap::new();
        user_map.insert("U_ALICE".to_string(), "alice".to_string());

        let prompt = identity.system_prompt_for_user(None, Some("U_ALICE"), &user_map, None);
        assert!(prompt.contains("Alice"));
        assert!(prompt.contains("Likes: Rust"));
    }

    #[test]
    fn system_prompt_for_user_without_files_still_uses_user_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("SOUL.md"), "Be helpful.").unwrap();

        let mut identity = AgentIdentity::load(path).unwrap();
        let user_map = HashMap::new();

        let prompt = identity.system_prompt_for_user(None, Some("U_ALICE"), &user_map, None);
        assert!(prompt.contains("Be helpful."));
        assert!(!prompt.contains("## About Your Human"));
    }

    // ── New session context tests ─────────────────────────────────────

    #[test]
    fn dynamic_context_new_session_loads_three_days() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        // Create memory directory and daily notes for 3 days
        let memory_dir = workspace.join("memory");
        fs::create_dir_all(&memory_dir).unwrap();

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let two_days_ago = (chrono::Utc::now() - chrono::Duration::days(2))
            .format("%Y-%m-%d")
            .to_string();

        fs::write(memory_dir.join(format!("{}.md", today)), "Today's notes").unwrap();
        fs::write(
            memory_dir.join(format!("{}.md", yesterday)),
            "Yesterday's notes",
        )
        .unwrap();
        fs::write(
            memory_dir.join(format!("{}.md", two_days_ago)),
            "Two days ago notes",
        )
        .unwrap();

        // Bootstrap defaults so identity can load
        AgentIdentity::bootstrap_defaults(&workspace).unwrap();
        let mut identity = AgentIdentity::load(&workspace).unwrap();

        // New session should include all 3 days
        let ctx_new = identity.dynamic_context(
            Some("test-session"),
            true,
            None,
            RecentDailyNotes::FromMarkdown,
        );
        assert!(
            ctx_new.contains("Today's notes"),
            "Should contain today's notes"
        );
        assert!(
            ctx_new.contains("Yesterday's notes"),
            "Should contain yesterday's notes"
        );
        assert!(
            ctx_new.contains("Two days ago notes"),
            "Should contain 2 days ago notes"
        );
        assert!(
            ctx_new.contains("2 days ago"),
            "Should have '2 days ago' label"
        );

        // Continuing session should only include 2 days
        let ctx_cont = identity.dynamic_context(
            Some("test-session"),
            false,
            None,
            RecentDailyNotes::FromMarkdown,
        );
        assert!(ctx_cont.contains("Today's notes"));
        assert!(ctx_cont.contains("Yesterday's notes"));
        assert!(
            !ctx_cont.contains("Two days ago notes"),
            "Should NOT contain 2 days ago notes for continuing session"
        );
    }

    #[test]
    fn dynamic_context_never_surfaces_other_sessions_tasks() {
        // Regression for the public-channel restart leak: `dynamic_context` must
        // NOT scan `memory/sessions/` and inject other sessions' task
        // descriptions. That cross-session view is the hub's job, owner-filtered
        // (`build_cross_session_summary`); `AgentIdentity` has no owner info and
        // must never surface another session's work.
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let sessions_dir = workspace.join("memory").join("sessions");
        let session_a = sessions_dir.join("session-aaa");
        fs::create_dir_all(&session_a).unwrap();
        fs::write(
            session_a.join("tasks.json"),
            r#"[{"id":"1","description":"HubSpot deals investigation","status":"active"}]"#,
        )
        .unwrap();

        AgentIdentity::bootstrap_defaults(&workspace).unwrap();
        let mut identity = AgentIdentity::load(&workspace).unwrap();

        // A brand-new session must not see session A's task description.
        let ctx = identity.dynamic_context(
            Some("session-new"),
            true,
            None,
            RecentDailyNotes::FromMarkdown,
        );
        assert!(
            !ctx.contains("Incomplete Tasks"),
            "the unowner-filtered cross-session block must be gone"
        );
        assert!(
            !ctx.contains("HubSpot deals investigation"),
            "must not leak another session's task description"
        );
    }

    #[test]
    fn active_task_is_scoped_to_the_current_user() {
        // Fix A: the active-task injected into the prompt must come from the
        // current user's own `memory/users/{uid}/active-task.md`, never the
        // shared workspace-global file or another user's file.
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let users = workspace.join("memory").join("users");
        fs::create_dir_all(users.join("alice")).unwrap();
        fs::create_dir_all(users.join("bob")).unwrap();
        fs::write(
            users.join("alice").join("active-task.md"),
            "Alice's own task",
        )
        .unwrap();
        fs::write(
            users.join("bob").join("active-task.md"),
            "Bob's private task",
        )
        .unwrap();
        // A stale global file (the legacy leaky path) must also stay hidden.
        fs::write(
            workspace.join("memory").join("active-task.md"),
            "Global leaked task",
        )
        .unwrap();

        AgentIdentity::bootstrap_defaults(&workspace).unwrap();
        let mut identity = AgentIdentity::load(&workspace).unwrap();

        let ctx = identity.dynamic_context(
            Some("session-1"),
            false,
            Some("alice"),
            RecentDailyNotes::FromMarkdown,
        );
        assert!(ctx.contains("Alice's own task"), "should show Alice's task");
        assert!(
            !ctx.contains("Bob's private task"),
            "must not leak Bob's task to Alice"
        );
        assert!(
            !ctx.contains("Global leaked task"),
            "must not fall back to the shared global file when a user is in scope"
        );
    }

    #[test]
    fn active_task_body_withheld_when_owned_by_another_active_session() {
        // Fix C: when the active task is owned by a *different*, still-active
        // session, the body must not be injected — only a contentless pointer.
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let user_dir = workspace.join("memory").join("users").join("alice");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("active-task.md"), "Sensitive task body").unwrap();
        // Fresh (non-stale) ownership by a different session.
        let meta = serde_json::json!({
            "session_id": "session-OTHER",
            "updated_at": Utc::now().to_rfc3339(),
        });
        fs::write(
            user_dir.join("active-task.meta.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        AgentIdentity::bootstrap_defaults(&workspace).unwrap();
        let mut identity = AgentIdentity::load(&workspace).unwrap();

        let ctx = identity.dynamic_context(
            Some("session-CURRENT"),
            false,
            Some("alice"),
            RecentDailyNotes::FromMarkdown,
        );
        assert!(
            ctx.contains("Active Task"),
            "the pointer header should still appear"
        );
        assert!(
            !ctx.contains("Sensitive task body"),
            "the body must be withheld when owned by another active session"
        );
        assert!(
            ctx.contains("withheld"),
            "should explain the body is withheld"
        );
    }

    #[test]
    fn active_task_body_shown_when_other_session_is_stale() {
        // The complement of the withheld case: once the owning session goes
        // stale (older than ACTIVE_TASK_STALE_HOURS) it is treated as ended, so
        // the body is surfaced for resumption.
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let user_dir = workspace.join("memory").join("users").join("alice");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("active-task.md"), "Resumable task body").unwrap();
        let stale = Utc::now() - chrono::Duration::hours(ACTIVE_TASK_STALE_HOURS + 1);
        let meta = serde_json::json!({
            "session_id": "session-OTHER",
            "updated_at": stale.to_rfc3339(),
        });
        fs::write(
            user_dir.join("active-task.meta.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        AgentIdentity::bootstrap_defaults(&workspace).unwrap();
        let mut identity = AgentIdentity::load(&workspace).unwrap();

        let ctx = identity.dynamic_context(
            Some("session-CURRENT"),
            false,
            Some("alice"),
            RecentDailyNotes::FromMarkdown,
        );
        assert!(
            ctx.contains("Resumable task body"),
            "a stale other-session task should surface its body"
        );
        assert!(
            !ctx.contains("withheld"),
            "stale tasks are resumable, not withheld"
        );
    }
}
