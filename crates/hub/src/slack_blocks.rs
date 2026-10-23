//! Slack Block Kit builders for the Claude Code chat surface.
//!
//! [`render_turn`] owns the visual shape of a chat turn and is idempotent for a given
//! [`TurnState`]: the dispatcher edits one message in place per turn, so re-rendering
//! the same state must produce the same blocks.
//!
//! The card is *status only* — header, prompt, a one-line tool summary, stats, stop
//! button. Assistant response text is delivered as separate thread replies (see
//! `claude_code_chat::deliver_answer`); putting unbounded model output into a
//! single Slack message ran into the per-`section` 3000-char text limit and froze
//! cards on long turns.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_orchestrator::workflow::decision::{ButtonStyle, Choice};
use workforce_skills::adapters::slack::rich_text;

/// Lifecycle of a single chat turn.
#[derive(Debug, Clone)]
pub enum TurnStatus {
    Queued,
    Starting,
    Running,
    /// Rate-limited by Anthropic; the runner is sleeping `wait_secs` before
    /// retrying. Surfaced so the user sees a meaningful state instead of a
    /// frozen "working" card during the multi-minute wait.
    RateLimited {
        wait_secs: u64,
    },
    Done,
    Stopped,
    Failed(String),
}

impl TurnStatus {
    fn header(&self) -> (&'static str, String) {
        match self {
            Self::Queued => ("🕑", "queued behind prior turn".into()),
            Self::Starting => ("⏳", "starting".into()),
            Self::Running => ("⚙️", "working".into()),
            Self::RateLimited { wait_secs } => {
                let mins = wait_secs.div_ceil(60);
                ("🐢", format!("rate-limited · waiting ~{mins}m"))
            }
            Self::Done => ("✅", "done".into()),
            Self::Stopped => ("⏹", "stopped by user".into()),
            Self::Failed(_) => ("⚠️", "failed".into()),
        }
    }

    fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Starting | Self::Running | Self::RateLimited { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub struct TurnStats {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cost_usd: f64,
    pub num_turns: u32,
    pub duration_ms: u64,
    /// Running total spend across every prior turn in the same Slack
    /// thread, including this one. Rendered alongside the per-turn
    /// cost so the user can see the thread-level number without
    /// summing prior cards in their head. `None` when the cost ledger
    /// hasn't been wired (tests, ad-hoc fixtures).
    pub thread_cost_usd: Option<f64>,
    /// The engine and model that ran the turn.
    pub engine: Option<String>,
    pub model: Option<String>,
}

/// Diagnostic block surfaced under a failure card when the subprocess
/// exited non-zero. `label` is the human-readable cause (e.g. "killed
/// (signal 9 — likely OOM)"); `stderr_tail` is the captured tail of
/// the subprocess's stderr, already truncated by the runner.
#[derive(Debug, Clone, Default)]
pub struct CrashDiagnostic {
    pub label: String,
    pub stderr_tail: String,
    /// Uncommitted edits the dispatcher stashed *after* the crash so
    /// the user can recover them with `git stash apply`. Empty when
    /// the worktree was clean or the stash helper couldn't run.
    pub wip_stash: Vec<crate::projects::cc_session::WipStash>,
}

/// State for one turn, rendered into a Block Kit payload via [`render_turn`].
pub struct TurnState {
    pub user_text: String,
    pub user_mention: String,
    /// Free-form label for the thread's binding — `owner/name` for a single
    /// repo, `project:<name>` for a multi-repo project bind. Rendered
    /// verbatim in the turn card.
    pub binding: String,
    /// The CLI that runs this thread's turns, as the card names it.
    pub cli: &'static str,
    pub status: TurnStatus,
    pub tool_calls: Vec<(String, String)>,
    pub assistant_text: Vec<String>,
    pub stats: Option<TurnStats>,
    pub stop_value: String,
    /// When the turn was dispatched. The renderer ticks on this every
    /// `HEARTBEAT_INTERVAL` while the turn is live so the card surfaces
    /// "(working — 84s)" instead of freezing at the last tool name when
    /// a single tool call (cargo test, large clone) takes minutes. Set
    /// to `Instant::now()` at TurnState construction; never reset.
    pub started_at: std::time::Instant,
    /// Populated only when the turn ended in a non-zero subprocess exit
    /// (excluding `session_not_found`, which has its own recovery
    /// rendering). Surfaces the kind of crash + a stderr tail so users
    /// can see "killed (likely OOM)" instead of an opaque "exit 1" — the
    /// failure mode that ate two of last night's runs.
    pub crash_diagnostic: Option<CrashDiagnostic>,
}

/// Render the current turn state as `(notification_text, blocks)` for a chat message.
///
/// The output is bounded to ≤ 6 blocks regardless of how many tool calls the
/// turn made or how long the response is — this is the structural fix for the
/// "card frozen on ⚙️" bug. The model's actual response text is delivered as
/// separate thread replies by `post_answer_replies`, not stuffed into this card.
pub fn render_turn(state: &TurnState) -> (String, Vec<Value>) {
    let (emoji, label) = state.status.header();
    // While the turn is live, surface elapsed seconds in the header so
    // a long-running tool call ("cargo test", "git clone --local") has
    // a visible heartbeat. Hidden once we have final stats (the stats
    // block renders the precise duration).
    let label_with_elapsed = if state.status.is_live() && state.stats.is_none() {
        format!("{label} · {}", format_elapsed(state.started_at.elapsed()))
    } else {
        label
    };
    let summary = format!(
        "{emoji} {} {label_with_elapsed} · {} · `{}`",
        state.cli, state.user_mention, state.binding
    );

    let mut blocks: Vec<Value> = vec![
        context_mrkdwn(&summary),
        section_mrkdwn(&format!(">>> {}", truncate(&state.user_text, 2000))),
        section_mrkdwn(&tool_summary_line(&state.tool_calls)),
    ];

    if let TurnStatus::Failed(ref err) = state.status {
        blocks.push(section_mrkdwn(&format!(
            "> ⚠️ *Error:* {}",
            truncate(err, 800)
        )));
        // Crash diagnostics: show the cause label + stderr tail in a
        // separate block so the failure block stays compact for the
        // common case ("rate limited") and the diagnostic block can
        // grow to the Slack section limit (~3000 chars) without
        // crowding the rest of the card.
        if let Some(ref d) = state.crash_diagnostic {
            // Use a code block so multi-line stderr doesn't get markdown-
            // mangled. Slack's `mrkdwn` renders triple-backtick fenced
            // code consistently.
            let mut body = format!("> _Cause:_ {}", truncate(&d.label, 200));
            if !d.stderr_tail.is_empty() {
                body.push_str(&format!("\n```\n{}\n```", truncate(&d.stderr_tail, 2400)));
            }
            if !d.wip_stash.is_empty() {
                body.push_str("\n*Uncommitted edits stashed:*\n");
                for s in &d.wip_stash {
                    let short_sha = if s.sha.is_empty() {
                        String::new()
                    } else {
                        format!(" (`{}`)", s.sha.chars().take(8).collect::<String>())
                    };
                    body.push_str(&format!(
                        "• `{}`: `{}`{} — recover with `git stash apply {}`\n",
                        s.repo, s.stash_ref, short_sha, s.stash_ref
                    ));
                }
            }
            blocks.push(section_mrkdwn(&body));
        }
    }

    if let Some(ref s) = state.stats {
        let cache = if s.cache_read_tokens > 0 {
            format!(" · {} cache", short_tokens(s.cache_read_tokens))
        } else {
            String::new()
        };
        // Show the running thread total only when it's larger than
        // this turn's cost (i.e., there's a prior turn to add to).
        // Same-or-smaller would be `0.00` rounding noise on the first
        // turn and only adds clutter.
        let thread_total = match s.thread_cost_usd {
            Some(total) if total > s.cost_usd => format!(" · thread ${:.2}", total),
            _ => String::new(),
        };
        let engine = match (s.engine.as_deref(), s.model.as_deref()) {
            (Some(engine), Some(model)) => format!(" · via {engine} ({model})"),
            (Some(engine), None) => format!(" · via {engine}"),
            (None, _) => String::new(),
        };
        blocks.push(context_mrkdwn(&format!(
            "_{} turns · {} in / {} out{} · ${:.4} · {:.1}s{}{}_",
            s.num_turns,
            short_tokens(s.input_tokens),
            short_tokens(s.output_tokens),
            cache,
            s.cost_usd,
            s.duration_ms as f64 / 1000.0,
            thread_total,
            engine,
        )));
    }

    if state.status.is_live() {
        blocks.push(stop_button(&state.stop_value));
    }

    (summary, blocks)
}

/// One-line summary of tool activity. Bounded — does not grow with tool count.
/// `🔧 0 tool calls` while idle, `🔧 N tool call(s) — last: \`tool\`` otherwise.
fn tool_summary_line(tool_calls: &[(String, String)]) -> String {
    let n = tool_calls.len();
    let plural = if n == 1 { "" } else { "s" };
    match tool_calls.last() {
        None => format!("🔧 {n} tool call{plural}"),
        Some((last_tool, _)) => format!("🔧 {n} tool call{plural} — last: `{last_tool}`"),
    }
}

/// How a coding chat names itself on its cards: the CLI that runs its turns
/// and the command that opens a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatNames {
    pub cli: &'static str,
    pub command: &'static str,
}

/// The model and effort a thread's turns run on, as its cards show them.
/// `None` is the engine's own default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatChoice {
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl ChatChoice {
    fn line(&self) -> Option<String> {
        let parts: Vec<String> = [("model", &self.model), ("effort", &self.effort)]
            .into_iter()
            .filter_map(|(label, value)| value.as_ref().map(|v| format!("{label} `{v}`")))
            .collect();
        (!parts.is_empty()).then(|| format!("⚙️ {}", parts.join(" · ")))
    }
}

/// Welcome message shown when a coding chat opens a new thread.
pub fn welcome_blocks(
    names: ChatNames,
    choice: &ChatChoice,
    repo: &str,
    initial_task: Option<&str>,
) -> (String, Vec<Value>) {
    let ChatNames { cli, .. } = names;
    let summary = format!("{cli} chat started — repo `{repo}`");
    let mut blocks = vec![section_mrkdwn(&format!(
        "💬 *{cli} chat started* — bound to `{repo}`\nReply in this thread to keep the conversation going. Each message is a turn in the same {cli} session."
    ))];
    blocks.extend(choice.line().map(|line| context_mrkdwn(&line)));
    if let Some(task) = initial_task.filter(|t| !t.is_empty()) {
        blocks.push(section_mrkdwn(&format!(">>> {}", truncate(task, 2000))));
    }
    (summary, blocks)
}

/// Welcome message for a `project:<name>` thread — lists the repos so the
/// user can see what the session is bound to at a glance.
pub fn welcome_project_blocks(
    names: ChatNames,
    choice: &ChatChoice,
    project: &workforce_projects::Project,
    initial_task: Option<&str>,
    scratch_retention: Duration,
    github: bool,
) -> (String, Vec<Value>) {
    let ChatNames { cli, command } = names;
    let summary = format!("{cli} chat started — project `{}`", project.name);
    let body = if project.repos.is_empty() {
        let reach = if github {
            format!("ask {cli} to build something new, or to clone any repo you have access to and work on it — several side by side is fine")
        } else {
            format!("ask {cli} to build something new here. No GitHub token is configured, so it cannot clone or push — set one up if you need to work on an existing repo")
        };
        format!(
            "💬 *{cli} chat started* — this thread has its own bench (`{}`)\nAn empty workspace, private to this thread: {reach}. Files persist across turns and survive a context reset; the workspace is reclaimed once this thread has been idle for {}. `{command} help` shows the other ways to start a session.",
            project.name,
            fmt_retention(scratch_retention)
        )
    } else {
        let repos = project
            .repos
            .iter()
            .map(|r| format!("• `{}`", r.alias))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "💬 *{cli} chat started* — bound to project `{}`\n{cli} works against an isolated copy of every repo in this project (each `{command}` session gets its own).\n{repos}",
            project.name
        )
    };
    let mut blocks = vec![section_mrkdwn(&body)];
    blocks.extend(choice.line().map(|line| context_mrkdwn(&line)));
    if let Some(task) = initial_task.filter(|t| !t.is_empty()) {
        blocks.push(section_mrkdwn(&format!(">>> {}", truncate(task, 2000))));
    }
    (summary, blocks)
}

/// Retention window in the coarsest unit that stays true — a re-configured
/// sub-day TTL must not render as "0 days" on the card that promises
/// persistence.
fn fmt_retention(d: Duration) -> String {
    let hours = d.as_secs() / 3600;
    match hours / 24 {
        0 => plural(hours.max(1), "hour"),
        days => plural(days, "day"),
    }
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// In-thread placeholder posted right after the welcome card so Slack renders
/// the "1 reply" indicator on the parent — without it the thread is invisible
/// until the user types.
pub fn ready_blocks() -> (String, Vec<Value>) {
    let summary = "Ready when you are.".to_string();
    let blocks = vec![section_mrkdwn("👋 Ready when you are.")];
    (summary, blocks)
}

/// Notice posted in-thread when Claude Code reported its on-disk
/// transcript was missing and we auto-recovered by minting a fresh
/// UUID. When `brief_recap` is set, the dispatcher will inject the
/// original /cc ask as a system-prompt recap so Claude continues on the
/// same goal without the user having to re-state it; the card text
/// reflects that. With no brief we fall back to the prior wording.
pub fn session_lost_blocks(names: ChatNames, brief_recap: Option<&str>) -> (String, Vec<Value>) {
    let cli = names.cli;
    match brief_recap {
        Some(brief) => {
            let snippet = truncate(brief, 200);
            let summary = format!(
                "Prior session lost — continuing on: {}",
                truncate(brief, 80)
            );
            let body = format!(
                "🔄 *Prior {cli} session was lost* — restarting fresh.\n\
                 Continuing on the original ask: _{snippet}_\n\
                 Reply if your goal has changed."
            );
            (summary, vec![section_mrkdwn(&body)])
        }
        None => {
            let summary = format!("Prior {cli} session was lost — restarting fresh.");
            let body = format!("🔄 *Prior {cli} session was lost* — restarting fresh. Earlier context in this thread is no longer available to {cli}.");
            (summary, vec![section_mrkdwn(&body)])
        }
    }
}

/// Notice posted in-thread when we deliberately rotate to a fresh
/// Claude Code session — too many turns, or idle past the prompt-cache
/// TTL. Distinct from [`session_lost_blocks`] in that this is
/// intentional. We do **not** recap on rotation: the user is mid-
/// conversation and may have moved on, so injecting the *original* /cc
/// ask back into context would be intrusive. We do still surface the
/// brief in the card so the user knows what we'll fall back to if they
/// need a reminder.
pub fn session_rotated_blocks(names: ChatNames, brief_recap: Option<&str>) -> (String, Vec<Value>) {
    let cli = names.cli;
    match brief_recap {
        Some(brief) => {
            let snippet = truncate(brief, 200);
            let summary = format!("Starting a fresh {cli} session — continuing the thread.");
            let body = format!(
                "🔄 *Starting a fresh {cli} session* to keep replies fast and avoid rate limits.\n\
                 The thread is still rooted in the original ask: _{snippet}_\n\
                 Reply with anything new the model should know."
            );
            (summary, vec![section_mrkdwn(&body)])
        }
        None => {
            let summary = format!("Starting a fresh {cli} session to keep replies fast.");
            let body = format!("🔄 *Starting a fresh {cli} session* to keep replies fast and avoid rate limits. Earlier context in this thread is no longer available to {cli} — re-state anything that matters.");
            (summary, vec![section_mrkdwn(&body)])
        }
    }
}

/// Notice posted when a thread's workspace is gone — reclaimed by the idle
/// sweeper, or relocated because the project's repo set changed. Distinct from
/// [`session_rotated_blocks`]: there the files are still there and only the
/// context resets, so saying "to keep replies fast" here would misdescribe the
/// loss the user is about to notice.
pub fn workspace_reset_blocks(names: ChatNames, brief_recap: Option<&str>) -> (String, Vec<Value>) {
    let summary = "This thread's workspace was reclaimed — starting fresh.".to_string();
    let mut body = format!("🗂️ *This thread's workspace is gone* — it was reclaimed after being idle, or the project's repos changed. Any files {} had created here went with it, and earlier context is no longer available.", names.cli);
    if let Some(brief) = brief_recap {
        body.push_str(&format!(
            "\nStill rooted in the original ask: _{}_",
            truncate(brief, 200)
        ));
    }
    (summary, vec![section_mrkdwn(&body)])
}

/// Usage blurb for a coding chat command with no arguments.
pub fn usage_blocks(names: ChatNames, models: &[String], efforts: &[&str]) -> (String, Vec<Value>) {
    let ChatNames { cli, command } = names;
    let summary = format!(
        "Usage: {command} [@agent] [model:<name>] [effort:<level>] [<repo> | project:<name>] [initial task]"
    );
    let blocks = vec![section_mrkdwn(&format!(
        "Usage: `{command} [@<agent>] [model:<name>] [effort:<level>] [<repo> | project:<name>] [initial task]`\n\
         • `{command}` on its own opens your *bench* — a workspace that persists across turns, where {cli} can build something new or work on any repo you ask it to clone.\n\
         • `{command} <repo>` works in one repo (`owner/name`, or a bare `name` if unambiguous).\n\
         • `{command} project:<name>` works across every repo in a project, side by side.\n\
         • `model:<name>` and `effort:<level>` pin the thread for its lifetime; put them before the task text, around the agent and repo words: models {}, or a full model id; efforts {}.\n\
         • `@<agent>` is optional; required when the channel isn't bound to one agent and multiple agents exist.\n\
         • `{command} help` shows this message.\n\
         Example: `{command} @allan model:{} effort:{} driftwerk/workforce summarise what claude_code.rs does`",
        choices_list(models),
        choices_list(efforts),
        models.first().map(String::as_str).unwrap_or("default"),
        efforts.get(2).or(efforts.first()).copied().unwrap_or("high"),
    ))];
    (summary, blocks)
}

/// `` `a` | `b` | `c` ``, or "none" when the engine offers no named choices.
fn choices_list<S: AsRef<str>>(choices: &[S]) -> String {
    if choices.is_empty() {
        return "none".to_string();
    }
    choices
        .iter()
        .map(|c| format!("`{}`", c.as_ref()))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Error blurb shown when `/cc <repo>` can't be resolved to a cloned repo
/// (unknown name, ambiguous match, or not cloned yet).
pub fn chat_error_blocks(message: &str) -> (String, Vec<Value>) {
    let summary = truncate(message, 200);
    let blocks = vec![section_mrkdwn(&format!("⚠️ {message}"))];
    (summary, blocks)
}

/// Thread-root card shown while `/cc` is cloning a missing repo. Edited in
/// place to `welcome_blocks` on success or `chat_error_blocks` on failure.
pub fn cloning_blocks(repo: &str) -> (String, Vec<Value>) {
    let summary = format!("Cloning `{repo}` — one moment…");
    let blocks = vec![section_mrkdwn(&format!(
        "⏳ *Cloning* `{repo}` — one moment…"
    ))];
    (summary, blocks)
}

/// Card surfacing meaningful divergence between the bound branch and
/// `origin/<base>` after materialisation. Posted at most once per /cc
/// dispatch; suppressed entirely when the report doesn't warrant a
/// warning (no overlap and within `COMMIT_BEHIND_WARN_THRESHOLD`).
pub fn branch_staleness_blocks(
    repo_label: &str,
    report: &crate::projects::staleness::StalenessReport,
) -> (String, Vec<Value>) {
    let summary = format!(
        "`{}` is {} commits behind `{}` ({})",
        report.current_branch, report.commits_behind, report.base_branch, repo_label
    );
    let mut body = format!(
        "⚠️ *Branch divergence in `{repo}`* — `{branch}` vs `origin/{base}`\n\
         • `{base}` is {behind} commits ahead of `{branch}`\n\
         • `{branch}` is {ahead} commits ahead of `{base}`",
        repo = repo_label,
        branch = report.current_branch,
        base = report.base_branch,
        behind = report.commits_behind,
        ahead = report.commits_ahead,
    );
    if !report.overlapping_files.is_empty() {
        body.push_str("\n\n*Files changed on both sides — likely conflicts:*\n");
        const MAX_LISTED: usize = 10;
        for f in report.overlapping_files.iter().take(MAX_LISTED) {
            body.push_str(&format!("• `{f}`\n"));
        }
        let extra = report.overlapping_files.len().saturating_sub(MAX_LISTED);
        if extra > 0 {
            body.push_str(&format!("…and {extra} more\n"));
        }
        body.push_str(
            "\nConsider rebasing onto `origin/main` (or `git pull --rebase`) before this turn — the model will otherwise need to resolve these mid-flow.",
        );
    } else if report.commits_behind >= crate::projects::staleness::COMMIT_BEHIND_WARN_THRESHOLD {
        body.push_str(
            "\n\nNo file overlap, but the gap is large — a rebase before substantive work will save the next merge from being painful.",
        );
    }
    (summary, vec![section_mrkdwn(&body)])
}

/// Card summarising the outcome of a `/cc-rebase` invocation. Each
/// repo gets one block describing what happened (advanced / clean
/// already / conflict / error). Conflicts are listed by file so the
/// user can ask the CLI to resolve them in their next turn.
pub fn rebase_result_blocks(
    names: ChatNames,
    reports: &[crate::projects::cc_rebase::RepoRebaseReport],
) -> (String, Vec<Value>) {
    use crate::projects::cc_rebase::RebaseOutcome;
    if reports.is_empty() {
        return (
            "Nothing to rebase".to_string(),
            vec![section_mrkdwn(
                "ℹ️ Nothing to rebase — no git repos found in this thread's workspace.",
            )],
        );
    }
    let any_conflict = reports
        .iter()
        .any(|r| matches!(r.outcome, RebaseOutcome::Conflict { .. }));
    let any_error = reports
        .iter()
        .any(|r| matches!(r.outcome, RebaseOutcome::Error { .. }));
    let head = if any_conflict {
        "⚠️ /cc-rebase: conflicts — aborted, worktree unchanged"
    } else if any_error {
        "⚠️ /cc-rebase: one or more repos errored"
    } else {
        "✅ /cc-rebase: complete"
    };
    let mut blocks: Vec<Value> = vec![section_mrkdwn(&format!("*{head}*"))];
    for r in reports {
        let body = match &r.outcome {
            RebaseOutcome::Advanced { from, to, base } => format!(
                "✅ *`{}`* — rebased onto `origin/{base}` (`{from}` → `{to}`)",
                r.repo
            ),
            RebaseOutcome::AlreadyUpToDate => format!("✅ *`{}`* — already up to date", r.repo),
            RebaseOutcome::Skipped { reason } => {
                format!("⏭ *`{}`* — skipped: {reason}", r.repo)
            }
            RebaseOutcome::Conflict { files, base } => {
                let head: Vec<String> = files.iter().take(10).map(|f| format!("• `{f}`")).collect();
                let extra = files.len().saturating_sub(10);
                let listing = if files.is_empty() {
                    "_(no specific files reported)_".to_string()
                } else {
                    let mut s = head.join("\n");
                    if extra > 0 {
                        s.push_str(&format!("\n…and {extra} more"));
                    }
                    s
                };
                format!(
                    "⚠️ *`{}`* — conflicts vs `origin/{base}`; rebase aborted\n{listing}\n_Ask {} to resolve them in your next turn._",
                    r.repo, names.cli
                )
            }
            RebaseOutcome::Error { stage, message } => format!(
                "❌ *`{}`* — error during {}: {}",
                r.repo,
                stage,
                truncate(message, 400)
            ),
        };
        blocks.push(section_mrkdwn(&body));
    }
    (head.to_string(), blocks)
}

/// One-shot card posted after a successful /cc turn carrying the
/// project's quality-gate verdict (cargo fmt / tsc / …). Empty repos
/// or `NothingToDo` summaries collapse to no card; the dispatcher
/// suppresses the post in that case.
pub fn quality_gate_blocks(
    names: ChatNames,
    summary: &crate::projects::cc_quality_gate::GateSummary,
) -> Option<(String, Vec<Value>)> {
    use crate::projects::cc_quality_gate::{GateOutcome, GateSummary};
    match summary {
        GateSummary::NothingToDo => None,
        GateSummary::Passed { repos } => {
            let summary_line = if repos.len() == 1 {
                format!("✅ Quality gates passed in `{}`", repos[0])
            } else {
                format!("✅ Quality gates passed in {} repos", repos.len())
            };
            let body = format!("✅ *Quality gates passed* — `{}`", repos.join("`, `"));
            Some((summary_line, vec![section_mrkdwn(&body)]))
        }
        GateSummary::Failed { failures } => {
            let head = format!(
                "⚠️ Quality gate failed in {} repo{}",
                failures.len(),
                if failures.len() == 1 { "" } else { "s" }
            );
            let mut blocks = vec![section_mrkdwn(&format!(
                "⚠️ *Quality gate failed* — the model finished but the project's standard checks failed below. Reply with what you'd like {} to fix; the next turn runs the gates again.",
                names.cli
            ))];
            for r in failures {
                if let GateOutcome::Failed(f) = &r.outcome {
                    let body = format!(
                        "*`{}`* — `{}` exited {}\n```\n{}\n```",
                        r.repo,
                        f.gate,
                        f.exit_code,
                        truncate(&f.output_tail, 2400)
                    );
                    blocks.push(section_mrkdwn(&body));
                }
            }
            Some((head, blocks))
        }
    }
}

/// One-shot notice posted in-thread when the running thread spend
/// crosses a new `THRESHOLD_INCREMENT_USD` band (default $25). Soft —
/// purely informational, dispatch is not blocked. The user decides
/// whether to keep going or stop the thread.
pub fn thread_spend_threshold_blocks(band_usd: f64, total_usd: f64) -> (String, Vec<Value>) {
    let summary = format!(
        "Thread spend has crossed ${:.0} (now ${:.2})",
        band_usd, total_usd
    );
    let body = format!(
        "💸 *Thread spend has crossed ${:.0}* — running total is now *${:.2}*.\n\
         No turns are blocked; this is a heads-up so a runaway thread doesn't surprise you. \
         Stop the thread (`⏹ Stop` on any turn card) if you'd rather pause here.",
        band_usd, total_usd
    );
    (summary, vec![section_mrkdwn(&body)])
}

/// Card posted when a /cc dispatch is short-circuited because the Claude
/// Code CLI binary is missing from this hub image. Distinguishes "the
/// hub is broken, redeploy" from "your message failed for some reason"
/// so the operator knows where to look.
pub fn cli_missing_blocks(cli: &str, package: &str, command: &str) -> (String, Vec<Value>) {
    let summary = format!("{cli} CLI is not installed in this hub image");
    let body = format!(
        "⚠️ *{cli} CLI is not installed in this hub image.*\n\
         The hub container is missing `{package}` on `PATH`. \
         {command} cannot dispatch turns until the image is rebuilt and redeployed — \
         the project's `Dockerfile` already includes the install step \
         (`RUN npm install -g {package}`). \
         Existing threads will resume once the new image is up."
    );
    (summary, vec![section_mrkdwn(&body)])
}

/// No engine in the chain can serve this user; `reason` is the chain's own
/// account of why, which already says what to install or enable.
pub fn engine_unavailable_blocks(reason: &str) -> (String, Vec<Value>) {
    let summary = "No coding engine can run this turn".to_string();
    let body = format!("⚠️ *No coding engine can run this turn.*\n{reason}");
    (summary, vec![section_mrkdwn(&body)])
}

fn section_mrkdwn(text: &str) -> Value {
    json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": text },
    })
}

fn context_mrkdwn(text: &str) -> Value {
    json!({
        "type": "context",
        "elements": [ { "type": "mrkdwn", "text": text } ],
    })
}

fn stop_button(value: &str) -> Value {
    json!({
        "type": "actions",
        "elements": [{
            "type": "button",
            "text": { "type": "plain_text", "text": "⏹ Stop", "emoji": true },
            "style": "danger",
            "action_id": "cc_stop",
            "value": value,
        }],
    })
}

/// A periodic check-in card: the progress note as a section, plus a single
/// *Pause* button. `value` is `agent_id|session_key` so the click handler can
/// resolve the right service and cancel that conversation's current run.
/// Mirrors the `cc_stop` pattern but targets a regular agent turn.
pub fn pause_control_blocks(note: &str, value: &str) -> Vec<Value> {
    vec![
        section_mrkdwn(note),
        json!({
            "type": "actions",
            "elements": [{
                "type": "button",
                "text": { "type": "plain_text", "text": "⏸ Pause", "emoji": true },
                "style": "danger",
                "action_id": "run_pause",
                "value": value,
            }],
        }),
    ]
}

/// A settled check-in card: just the (past-tense) progress note, no Pause
/// button. Used to retire a live check-in card once the run ends — the note is
/// kept so the human can still see roughly how long it took.
pub fn settled_checkin_blocks(note: &str) -> Vec<Value> {
    vec![section_mrkdwn(note)]
}

/// The round-cap continuation control attached to a budget-capped final
/// reply. `value` is `session_key|is_dm` so the click handler can re-inject
/// "continue" against the correct session. One actions block.
pub fn continue_button_blocks(value: &str) -> Vec<Value> {
    vec![json!({
        "type": "actions",
        "elements": [{
            "type": "button",
            "text": { "type": "plain_text", "text": "▶ Continue working", "emoji": true },
            "style": "primary",
            "action_id": "turn_continue",
            "value": value,
        }],
    })]
}

/// Always-visible Continue / Exit controls for an active playbook, attached to
/// the agent's reply. Both buttons carry the same opaque `value`
/// (`session_key|nonce|is_dm`) so the click handler can re-inject the directive
/// against the correct session and reject a stale nonce. Returned as a one-block
/// vec ready to hand to the outbound `blocks` field.
pub fn playbook_control_buttons(value: &str) -> Vec<Value> {
    vec![json!({
        "type": "actions",
        "elements": [
            button("Continue", "primary", "playbook_continue", value),
            button("Exit", "danger", "playbook_exit", value),
        ],
    })]
}

/// The Exit-only control row, for a forced-choice step (`keep_continue = false`)
/// where the custom buttons are the only way forward. Exit is always offered as
/// the escape hatch. Same opaque 3-part `value` as the other controls.
pub fn playbook_exit_button(value: &str) -> Vec<Value> {
    vec![json!({
        "type": "actions",
        "elements": [
            button("Exit", "danger", "playbook_exit", value),
        ],
    })]
}

/// One custom step button, pre-mapped to its Slack representation.
pub struct CustomButton {
    /// Button label (emoji allowed).
    pub label: String,
    /// Slack style string (`"primary"` / `"danger"`), or `None` for the neutral
    /// grey default (Slack rejects an explicit `"default"`, so we omit the key).
    pub style: Option<String>,
    /// Where this button goes, as the 4th `value` part the dispatcher reads: a
    /// target step id, or the reserved `$exit` / `$finish` tokens.
    pub effect_token: String,
}

/// Custom branching buttons for a step, rendered as one actions row *above* the
/// standard Continue/Exit controls. Each button needs a **unique `action_id`**
/// (`playbook_goto_0`, `_1`, …) because Slack rejects duplicate action_ids
/// within a single actions block; the per-button target rides in a 4th value
/// part (`session_key|nonce|is_dm|<token>`). Empty input → empty vec (no row).
pub fn playbook_custom_buttons(value_base: &str, buttons: &[CustomButton]) -> Vec<Value> {
    if buttons.is_empty() {
        return vec![];
    }
    let elements: Vec<Value> = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| {
            button_styled(
                &b.label,
                b.style.as_deref(),
                &format!("playbook_goto_{i}"),
                &format!("{value_base}|{}", b.effect_token),
            )
        })
        .collect();
    vec![json!({ "type": "actions", "elements": elements })]
}

/// A compact "Step N/Total: Title" context line rendered just above the
/// playbook controls so the user always sees where they are alongside the
/// buttons.
pub fn playbook_step_context(current: usize, total: usize, title: &str) -> Value {
    context_mrkdwn(&format!("▶ Step {current}/{total}: {title}"))
}

/// The single "Finish" control for the *last* step of a playbook, where there's
/// nothing to continue onto — a lone primary button instead of Continue/Exit.
/// It reuses the `playbook_continue` action id, so the click advances past the
/// final step and the engine resolves it to `StepOutcome::Complete` (posting the
/// "✅ complete" message). Same opaque `value` (`session_key|nonce|is_dm`) as the
/// regular controls.
pub fn playbook_finish_button(value: &str) -> Vec<Value> {
    vec![json!({
        "type": "actions",
        "elements": [
            button("Finish", "primary", "playbook_continue", value),
        ],
    })]
}

/// Format a millisecond duration as a short human string: `45s`, `2m 5s`,
/// `1h 3m`. Negative inputs clamp to zero.
pub fn fmt_duration_ms(ms: i64) -> String {
    let total = (ms.max(0) / 1000) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// A playbook step "control card": a header line (`{emoji} *{header}*`) over a
/// status line, with the control button rows (if any) appended below. This is
/// the single message a step owns as it moves working → awaiting → done.
/// `state_emoji` carries the state (`▶` active, `✅` done); `button_rows` are
/// the pre-rendered `actions` blocks (empty once the step is finalized, so no
/// stale buttons survive). Always renders the text inside a section block — the
/// playbook lesson that a message with `blocks` must not rely on the top-level
/// `text` for display.
pub fn playbook_card_blocks(
    state_emoji: &str,
    header: &str,
    status: &str,
    button_rows: Vec<Value>,
) -> Vec<Value> {
    let mut blocks = vec![section_mrkdwn(&format!(
        "{state_emoji} *{header}*\n{status}"
    ))];
    blocks.extend(button_rows);
    blocks
}

/// Block Kit rendering of an approval-required prompt with the four
/// typed buttons the cascade UX (DW-525 / phase 4) calls for. The
/// caller passes the same options struct the runner emitted in
/// [`ProgressEvent::ApprovalRequired`]; buttons gated by
/// `allow_save_allow = false` (the `always_ask` case) collapse to two:
/// Allow once / Deny once + Always Deny.
///
/// The `value` of every button is `request_id` so the
/// `block_actions` handler can route the response back through the
/// `pending_approvals` map by exactly the same key the runner used.
pub fn permission_prompt_blocks(
    agent_id: &str,
    tool_name: &str,
    detail: &str,
    reason: &str,
    request_id: &str,
    options: &workforce_core::policy::PermissionPromptOptions,
) -> Vec<Value> {
    let header = section_mrkdwn(&format!(
        ":warning: *Approval required*\n\
         Agent `{agent_id}` wants to run `{tool_name}`",
    ));
    let detail_block = section_mrkdwn(&format!("> {}", truncate(detail, 280)));
    let reason_block = context_mrkdwn(&format!("_{}_", truncate(reason, 200)));

    let mut elements: Vec<Value> = Vec::with_capacity(4);
    if options.allow_once {
        elements.push(button("Allow", "primary", "perm_allow_once", request_id));
    }
    if options.deny_once {
        elements.push(button("Deny", "danger", "perm_deny_once", request_id));
    }
    if options.allow_always {
        elements.push(button(
            "Always Allow",
            "primary",
            "perm_allow_always",
            request_id,
        ));
    }
    if options.deny_always {
        elements.push(button(
            "Always Deny",
            "danger",
            "perm_deny_always",
            request_id,
        ));
    }

    vec![
        header,
        detail_block,
        reason_block,
        json!({ "type": "actions", "elements": elements }),
    ]
}

/// Routed (designated-approver) variant of [`permission_prompt_blocks`]:
/// the same prompt with a leading context line naming the requester, so a
/// DM'd approver knows on whose behalf they're deciding. Reuses the base
/// builder verbatim — only the "Requested by …" context line is new.
#[allow(clippy::too_many_arguments)]
pub fn routed_permission_prompt_blocks(
    agent_id: &str,
    tool_name: &str,
    detail: &str,
    reason: &str,
    request_id: &str,
    options: &workforce_core::policy::PermissionPromptOptions,
    requester_slack_id: &str,
) -> Vec<Value> {
    let mut blocks =
        permission_prompt_blocks(agent_id, tool_name, detail, reason, request_id, options);
    let context = context_mrkdwn(&format!(
        "Requested by <@{requester_slack_id}> · agent `{agent_id}` wants to run `{tool_name}`",
    ));
    blocks.insert(1, context);
    blocks
}

/// The `action_id` prefix of a decision card's buttons; the button's index
/// follows it, since Slack refuses two equal ids in one actions block.
pub const DECISION_ACTION_PREFIX: &str = "wf_decision_";
/// Body blocks a decision card carries itself; the rest goes to its thread.
const DECISION_BODY_BLOCKS: usize = 43;
/// Slack's block limit for one message.
const MESSAGE_BLOCKS: usize = 50;
const DECISION_TITLE_CHARS: usize = 150;
const DECISION_LABEL_CHARS: usize = 75;
const DECISION_CONFIRM_CHARS: usize = 300;
/// Slack's limit for a confirm dialog's buttons.
const DECISION_CONFIRM_BUTTON_CHARS: usize = 30;
/// Names a waiting line lists before it counts the rest.
const NAMES_SHOWN: usize = 20;

/// Where a decision stands, as its card shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum CardState<'a> {
    Open,
    Decided {
        label: &'a str,
        by_mention: String,
        at: DateTime<Utc>,
    },
    Expired {
        label: &'a str,
        at: DateTime<Utc>,
    },
    Stopped {
        by: Option<&'a str>,
    },
    /// A card whose decision the run no longer holds.
    Closed,
}

/// What a decision card shows.
pub struct DecisionCardView<'a> {
    pub title: &'a str,
    pub context: Option<&'a str>,
    pub body: Option<&'a str>,
    pub choices: &'a [Choice],
    /// Mentions when Slack knows the decider, else their names.
    pub waiting_on: Vec<String>,
    /// The deadline and the label of the choice it presses.
    pub expires: Option<(DateTime<Utc>, &'a str)>,
    pub state: CardState<'a>,
}

/// A decision card in `view`'s state: `(fallback text, blocks)`. The text is
/// the title alone, so a surface's broadcast rewrite has nothing to act on.
/// One builder serves every state, so a rebuilt card matches the posted one.
pub fn decision_card(
    view: &DecisionCardView<'_>,
    instance_id: &str,
    gate: Uuid,
) -> (String, Vec<Value>) {
    let (head, rest) = view.body.map(decision_card_overflow).unwrap_or_default();
    let header = json!({
        "type": "header",
        "text": { "type": "plain_text", "text": clip(view.title, DECISION_TITLE_CHARS), "emoji": true },
    });
    let actions = matches!(view.state, CardState::Open).then(|| {
        json!({
            "type": "actions",
            "block_id": format!("wf_decision:{gate}"),
            "elements": view
                .choices
                .iter()
                .enumerate()
                .map(|(at, choice)| decision_button(choice, at, instance_id, gate))
                .collect::<Vec<_>>(),
        })
    });
    let blocks = std::iter::once(header)
        .chain(view.context.map(context_mrkdwn))
        .chain(view.body.map(|_| json!({ "type": "divider" })))
        .chain(head)
        .chain((!rest.is_empty()).then(|| context_mrkdwn("_Continued in the thread._")))
        .chain([
            json!({ "type": "divider" }),
            context_mrkdwn(&decision_status(view)),
        ])
        .chain(actions)
        .collect();
    (view.title.to_string(), blocks)
}

fn decision_button(choice: &Choice, at: usize, instance_id: &str, gate: Uuid) -> Value {
    let mut button = button_styled(
        &clip(&choice.label, DECISION_LABEL_CHARS),
        choice.style.map(ButtonStyle::as_str),
        &format!("{DECISION_ACTION_PREFIX}{at}"),
        &decision_button_value(instance_id, gate, &choice.outcome),
    );
    if let Some(confirm) = &choice.confirm {
        button["confirm"] = json!({
            "title": { "type": "plain_text", "text": "Are you sure?" },
            "text": { "type": "plain_text", "text": clip(confirm, DECISION_CONFIRM_CHARS) },
            "confirm": { "type": "plain_text", "text": clip(&choice.label, DECISION_CONFIRM_BUTTON_CHARS) },
            "deny": { "type": "plain_text", "text": "Go back" },
        });
    }
    button
}

fn decision_status(view: &DecisionCardView<'_>) -> String {
    match &view.state {
        CardState::Open => {
            let expiry = view
                .expires
                .map(|(at, label)| format!(" · expires {} → *{label}*", slack_date(at)))
                .unwrap_or_default();
            format!("⏳ Waiting on {}{expiry}", either(&view.waiting_on))
        }
        CardState::Decided {
            label,
            by_mention,
            at,
        } => format!("✅ *{label}* by {by_mention} · {}", slack_date(*at)),
        CardState::Expired { label, at } => format!("⌛ Expired {} · *{label}*", slack_date(*at)),
        CardState::Stopped { by: Some(by) } => format!("⏹ Stopped by {by} · nothing was decided"),
        CardState::Stopped { by: None } => "⏹ Stopped · nothing was decided".to_string(),
        CardState::Closed => "This decision is closed.".to_string(),
    }
}

/// A moment as Slack renders it in each reader's own time zone.
fn slack_date(at: DateTime<Utc>) -> String {
    format!(
        "<!date^{}^{{date_short_pretty}} at {{time}}|{}>",
        at.timestamp(),
        at.to_rfc3339()
    )
}

/// `names` as one alternative: "Alice", "Alice or Bob", "Alice, Bob or Ana",
/// past [`NAMES_SHOWN`] names "…, Tom or 12 others".
pub fn either(names: &[String]) -> String {
    match names {
        [] => "nobody".to_string(),
        [only] => only.clone(),
        many if many.len() > NAMES_SHOWN => format!(
            "{} or {} others",
            many[..NAMES_SHOWN].join(", "),
            many.len() - NAMES_SHOWN
        ),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// A decision body as blocks: the head the card carries, at most
/// [`DECISION_BODY_BLOCKS`], and the messages that continue it in the thread.
pub fn decision_card_overflow(body: &str) -> (Vec<Value>, Vec<Vec<Value>>) {
    let mut groups = rich_text::build_messages(body, false).into_iter();
    let first = groups.next().unwrap_or_default();
    let split = first.len().min(DECISION_BODY_BLOCKS);
    let (head, tail) = first.split_at(split);
    let rest = tail
        .chunks(MESSAGE_BLOCKS)
        .map(<[Value]>::to_vec)
        .chain(groups)
        .collect();
    (head.to_vec(), rest)
}

/// What a decision button carries: the run, the decision and the outcome.
pub fn decision_button_value(instance_id: &str, gate: Uuid, outcome: &str) -> String {
    format!("{instance_id}|{gate}|{outcome}")
}

/// A decision button's value read back, or `None` for one this build did not
/// write.
pub fn parse_decision_value(value: &str) -> Option<(String, Uuid, String)> {
    let mut parts = value.splitn(3, '|');
    let instance = parts.next().filter(|id| !id.is_empty())?;
    let gate = Uuid::parse_str(parts.next()?).ok()?;
    let outcome = parts.next().filter(|outcome| !outcome.is_empty())?;
    Some((instance.to_string(), gate, outcome.to_string()))
}

/// `text` with every group notification made inert: `<!here>`,
/// `<!channel>`, `<!everyone>` and `<!subteam^…>` (labelled or not), and the
/// bare `@` forms, each broken by a word joiner Slack does not read through.
pub fn defuse_broadcasts(text: &str) -> String {
    static BROADCAST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(<!|@)(here\b|channel\b|everyone\b|subteam\^)")
            .expect("a valid pattern")
    });
    BROADCAST.replace_all(text, "${1}\u{2060}${2}").into_owned()
}

/// Text a decision card shows, made safe for a channel: secrets redacted and
/// broadcasts defused.
pub fn card_text(text: &str) -> String {
    defuse_broadcasts(&crate::redact::redact_secrets(text))
}

/// `s` in at most `max` characters, an ellipsis marking a cut.
fn clip(s: &str, max: usize) -> String {
    match s.chars().count() > max {
        true => s.chars().take(max - 1).chain(['…']).collect(),
        false => s.to_string(),
    }
}

fn button(text: &str, style: &str, action_id: &str, value: &str) -> Value {
    json!({
        "type": "button",
        "text": { "type": "plain_text", "text": text, "emoji": true },
        "style": style,
        "action_id": action_id,
        "value": value,
    })
}

/// Like [`button`] but the `style` key is omitted when `None` — Slack only
/// accepts `"primary"` / `"danger"`, and rendering a grey (default) button means
/// sending no style at all.
fn button_styled(text: &str, style: Option<&str>, action_id: &str, value: &str) -> Value {
    let mut b = json!({
        "type": "button",
        "text": { "type": "plain_text", "text": text, "emoji": true },
        "action_id": action_id,
        "value": value,
    });
    if let Some(s) = style {
        b["style"] = json!(s);
    }
    b
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn short_tokens(n: u32) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Render an elapsed `Duration` as a Slack-card-friendly short string.
/// Sub-minute values stay in seconds; longer values switch to `m+s`
/// format so the card stays compact even on multi-hour runs.
fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CC: ChatNames = ChatNames {
        cli: "Claude Code",
        command: "/cc",
    };
    const CX: ChatNames = ChatNames {
        cli: "Codex",
        command: "/cx",
    };

    #[test]
    fn a_codex_chat_names_its_own_cli_and_command_on_every_card() {
        let (summary, blocks) =
            welcome_blocks(CX, &ChatChoice::default(), "driftwerk/workforce", None);
        let text = format!("{summary} {}", serde_json::to_string(&blocks).unwrap());
        assert!(
            text.contains("Codex chat started") && !text.contains("Claude"),
            "{text}"
        );
        for (summary, blocks) in [
            usage_blocks(CX, &["sol".into()], &["low"]),
            session_lost_blocks(CX, None),
            session_lost_blocks(CX, Some("ask")),
            session_rotated_blocks(CX, None),
            session_rotated_blocks(CX, Some("ask")),
            workspace_reset_blocks(CX, None),
        ] {
            let text = format!("{summary} {}", serde_json::to_string(&blocks).unwrap());
            assert!(!text.contains("Claude") && !text.contains("/cc"), "{text}");
        }
        let (summary, _) = usage_blocks(CX, &[], &[]);
        assert!(summary.starts_with("Usage: /cx"));
        let conflict = crate::projects::cc_rebase::RepoRebaseReport {
            repo: "workforce".into(),
            outcome: crate::projects::cc_rebase::RebaseOutcome::Conflict {
                files: vec!["a.rs".into()],
                base: "main".into(),
            },
        };
        let (_, blocks) = rebase_result_blocks(CX, &[conflict]);
        assert!(!serde_json::to_string(&blocks).unwrap().contains("Claude"));
    }

    fn sample(status: TurnStatus) -> TurnState {
        TurnState {
            user_text: "do the thing".into(),
            user_mention: "<@U1>".into(),
            binding: "workforce".into(),
            cli: "Claude Code",
            status,
            tool_calls: vec![],
            assistant_text: vec![],
            stats: None,
            stop_value: "slack:ws:C1:t1".into(),
            started_at: std::time::Instant::now(),
            crash_diagnostic: None,
        }
    }

    #[test]
    fn a_codex_turn_card_names_codex_and_never_claude() {
        let mut state = sample(TurnStatus::Done);
        state.cli = "Codex";
        state.stats = Some(TurnStats {
            input_tokens: 6_700,
            output_tokens: 19,
            cache_read_tokens: 6_500,
            cost_usd: 0.0298,
            num_turns: 1,
            duration_ms: 26_400,
            thread_cost_usd: None,
            engine: Some("codex".into()),
            model: Some("gpt-5.6-sol".into()),
        });
        for status in [
            TurnStatus::Done,
            TurnStatus::Failed("boom".into()),
            TurnStatus::Running,
        ] {
            state.status = status;
            let (summary, blocks) = render_turn(&state);
            let text = format!("{summary} {}", serde_json::to_string(&blocks).unwrap());
            assert!(
                summary.starts_with("✅ Codex") || summary.contains(" Codex "),
                "{summary}"
            );
            assert!(!text.contains("Claude"), "{text}");
        }
    }

    #[test]
    fn every_turn_card_names_the_engine_and_model_that_ran_it() {
        let mut state = sample(TurnStatus::Done);
        state.stats = Some(TurnStats {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 0,
            cost_usd: 0.01,
            num_turns: 1,
            duration_ms: 1_000,
            thread_cost_usd: None,
            engine: Some("claude-code".into()),
            model: Some("fable".into()),
        });
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("via claude-code (fable)"), "{json}");
    }

    #[test]
    fn a_welcome_card_shows_the_thread_choice_and_usage_names_the_choices() {
        let choice = ChatChoice {
            model: Some("astra".into()),
            effort: Some("xhigh".into()),
        };
        let (_, blocks) = welcome_blocks(CX, &choice, "driftwerk/workforce", None);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(
            json.contains("model `astra`") && json.contains("effort `xhigh`"),
            "{json}"
        );
        let (_, blocks) = welcome_blocks(CX, &ChatChoice::default(), "driftwerk/workforce", None);
        assert!(!serde_json::to_string(&blocks).unwrap().contains("⚙️"));
        let (summary, blocks) = usage_blocks(CX, &["astra".into(), "sol".into()], &["low", "max"]);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(summary.contains("model:<name>"));
        assert!(
            json.contains("`astra` | `sol`") && json.contains("`low` | `max`"),
            "{json}"
        );
    }

    #[test]
    fn continue_button_blocks_shape() {
        let blocks = continue_button_blocks("slack:ws:C1:T1|true");
        assert_eq!(blocks.len(), 1, "one actions block");
        assert_eq!(blocks[0]["type"], "actions");
        let button = &blocks[0]["elements"][0];
        assert_eq!(button["type"], "button");
        assert_eq!(button["action_id"], "turn_continue");
        assert_eq!(button["style"], "primary");
        assert_eq!(button["text"]["text"], "▶ Continue working");
        assert_eq!(
            button["value"], "slack:ws:C1:T1|true",
            "value passes through"
        );
    }

    #[test]
    fn fmt_duration_ms_buckets() {
        assert_eq!(fmt_duration_ms(-100), "0s");
        assert_eq!(fmt_duration_ms(0), "0s");
        assert_eq!(fmt_duration_ms(45_000), "45s");
        assert_eq!(fmt_duration_ms(125_000), "2m 5s");
        assert_eq!(fmt_duration_ms(3_780_000), "1h 3m");
    }

    #[test]
    fn playbook_card_blocks_shape() {
        // Working state: header + status, no buttons.
        let working = playbook_card_blocks("▶", "Step 2/5: Implement", "⏱ working… 1m 2s", vec![]);
        assert_eq!(working.len(), 1);
        let json = serde_json::to_string(&working).unwrap();
        assert!(json.contains("Step 2/5: Implement"));
        assert!(json.contains("working"));
        assert!(!json.contains("playbook_continue"));

        // Awaiting state: the control rows ride below the header.
        let rows = playbook_control_buttons("sess|nonce|true");
        let awaiting = playbook_card_blocks("▶", "Step 2/5: Implement", "done — your move", rows);
        let json = serde_json::to_string(&awaiting).unwrap();
        assert!(json.contains("playbook_continue"));
        assert!(json.contains("playbook_exit"));
    }

    #[test]
    fn render_shows_stop_button_while_running() {
        let mut state = sample(TurnStatus::Running);
        state.tool_calls = vec![("Read".into(), "foo.rs".into())];
        let (summary, blocks) = render_turn(&state);
        assert!(summary.contains("Claude"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("cc_stop"));
        // Tool summary shows the count and the last tool name — the per-call detail
        // ("foo.rs") deliberately does NOT live in the card.
        assert!(json.contains("1 tool call "));
        assert!(json.contains("`Read`"));
        assert!(!json.contains("foo.rs"));
    }

    #[test]
    fn render_hides_stop_on_done() {
        let mut state = sample(TurnStatus::Done);
        // assistant_text is captured for the thread reply path; it must NOT
        // appear in the card.
        state.assistant_text = vec!["the final answer".into()];
        state.stats = Some(TurnStats {
            input_tokens: 1200,
            output_tokens: 345,
            cache_read_tokens: 0,
            cost_usd: 0.0123,
            num_turns: 2,
            duration_ms: 18_000,
            thread_cost_usd: None,
            engine: None,
            model: None,
        });
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(!json.contains("cc_stop"));
        assert!(!json.contains("the final answer"));
        assert!(json.contains("2 turns"));
        assert!(json.contains("$0.0123"));
    }

    #[test]
    fn render_shows_stop_button_while_queued() {
        let state = sample(TurnStatus::Queued);
        let (summary, blocks) = render_turn(&state);
        assert!(summary.contains("queued"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("cc_stop"));
        assert!(json.contains("queued"));
    }

    #[test]
    fn render_shows_failure_details() {
        let state = sample(TurnStatus::Failed("claude exited with code 1".into()));
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("failed"));
        assert!(json.contains("claude exited with code 1"));
    }

    #[test]
    fn render_includes_cache_tokens_when_present() {
        let mut state = sample(TurnStatus::Done);
        state.stats = Some(TurnStats {
            input_tokens: 1200,
            output_tokens: 345,
            cache_read_tokens: 800,
            cost_usd: 0.01,
            num_turns: 1,
            duration_ms: 5_000,
            thread_cost_usd: None,
            engine: None,
            model: None,
        });
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("cache"));
    }

    #[test]
    fn render_card_is_bounded_under_extreme_load() {
        // Structural fix: regardless of how many tool calls or how long the
        // accumulated response, the card stays a small fixed-shape status card.
        use workforce_skills::adapters::slack::MAX_SECTION_TEXT;
        let mut state = sample(TurnStatus::Running);
        state.tool_calls = (0..200)
            .map(|i| ("Bash".to_string(), format!("very long detail for tool call number {i} that would exceed limits if rendered in full")))
            .collect();
        state.assistant_text = vec!["x".repeat(50_000)];
        let (_, blocks) = render_turn(&state);
        assert!(
            blocks.len() <= 6,
            "card must be bounded; got {} blocks",
            blocks.len()
        );
        for b in &blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("section") {
                let text = b
                    .pointer("/text/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                assert!(
                    text.chars().count() <= MAX_SECTION_TEXT,
                    "section block text {} chars exceeds {} limit",
                    text.chars().count(),
                    MAX_SECTION_TEXT
                );
            }
        }
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("200 tool calls"));
        assert!(json.contains("`Bash`"));
    }

    #[test]
    fn render_tool_summary_with_no_calls() {
        let state = sample(TurnStatus::Running);
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("0 tool calls"));
        // No "last:" label when nothing has run yet.
        assert!(!json.contains("last:"));
    }

    #[test]
    fn render_shows_rate_limited_summary_with_minutes() {
        let state = sample(TurnStatus::RateLimited { wait_secs: 240 });
        let (summary, blocks) = render_turn(&state);
        assert!(summary.contains("rate-limited"));
        assert!(
            summary.contains("4m"),
            "summary surfaces minutes: {summary}"
        );
        let json = serde_json::to_string(&blocks).unwrap();
        // Stop button stays so the user can cancel the wait if they want.
        assert!(json.contains("cc_stop"));
    }

    #[test]
    fn render_rate_limited_rounds_subminute_waits_up_to_one_minute() {
        // 45s rounds up to 1m so the user doesn't see "0m".
        let state = sample(TurnStatus::RateLimited { wait_secs: 45 });
        let (summary, _) = render_turn(&state);
        assert!(summary.contains("1m"));
    }

    #[test]
    fn cloning_blocks_renders_repo_and_hourglass() {
        let (summary, blocks) = cloning_blocks("driftwerk/workforce");
        assert!(summary.contains("driftwerk/workforce"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("driftwerk/workforce"));
        assert!(json.contains("Cloning"));
        assert!(json.contains("⏳"));
    }

    #[test]
    fn chat_error_blocks_carries_warning_and_message() {
        let (summary, blocks) = chat_error_blocks("Repo `x/y` not cloned.");
        assert!(summary.contains("x/y"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("x/y"));
        assert!(json.contains("⚠️"));
    }

    #[test]
    fn ready_blocks_renders_wave_and_ready_text() {
        let (summary, blocks) = ready_blocks();
        assert!(summary.contains("Ready"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("👋"));
        assert!(json.contains("Ready"));
    }

    #[test]
    fn session_lost_blocks_without_brief_falls_back_to_prior_wording() {
        let (summary, blocks) = session_lost_blocks(CC, None);
        assert!(summary.contains("lost"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("🔄"));
        assert!(json.contains("restart"));
        assert!(json.contains("context"));
    }

    #[test]
    fn session_lost_blocks_with_brief_surfaces_recap() {
        let (summary, blocks) =
            session_lost_blocks(CC, Some("investigate every place that calls llm"));
        // Summary should hint at the recap so notifications carry context.
        assert!(summary.to_lowercase().contains("continuing"));
        let json = serde_json::to_string(&blocks).unwrap();
        // Brief snippet is rendered inside the card.
        assert!(json.contains("investigate every place that calls llm"));
        // Old "re-state anything that matters" wording must not leak when
        // we have a brief — that's the user-pain phrase.
        assert!(!json.contains("re-state anything"));
    }

    #[test]
    fn session_rotated_blocks_without_brief_falls_back_to_prior_wording() {
        let (summary, blocks) = session_rotated_blocks(CC, None);
        assert!(summary.contains("fresh"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("🔄"));
        assert!(json.contains("fresh"));
        assert!(json.contains("context"));
    }

    #[test]
    fn session_rotated_blocks_with_brief_surfaces_recap_without_injection_promise() {
        let (summary, blocks) = session_rotated_blocks(CC, Some("ship cost-tracking"));
        assert!(summary.contains("fresh"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("ship cost-tracking"));
        // Rotation is intentional — we do NOT promise to inject the recap
        // for the user, only to surface it for reference. Make sure we
        // don't claim "continuing on" the way the lost path does.
        assert!(!json.contains("Continuing on the original"));
    }

    #[test]
    fn branch_staleness_card_calls_out_overlapping_files() {
        let report = crate::projects::staleness::StalenessReport {
            current_branch: "feature".into(),
            base_branch: "main".into(),
            commits_behind: 12,
            commits_ahead: 3,
            overlapping_files: vec!["src/server.rs".into(), "src/db.rs".into()],
        };
        let (summary, blocks) = branch_staleness_blocks("rails", &report);
        assert!(summary.contains("feature"));
        assert!(summary.contains("12"));
        assert!(summary.contains("rails"));
        let json = serde_json::to_string(&blocks).unwrap();
        // Overlapping files must be listed individually so the user can
        // decide which ones to rebase past.
        assert!(json.contains("src/server.rs"));
        assert!(json.contains("src/db.rs"));
        // And the recommendation to rebase before working further.
        assert!(json.to_lowercase().contains("rebas"));
    }

    #[test]
    fn branch_staleness_card_truncates_long_overlap_lists() {
        let mut overlap = Vec::new();
        for i in 0..15 {
            overlap.push(format!("file_{i}.rs"));
        }
        let report = crate::projects::staleness::StalenessReport {
            current_branch: "feature".into(),
            base_branch: "main".into(),
            commits_behind: 1,
            commits_ahead: 1,
            overlapping_files: overlap,
        };
        let (_, blocks) = branch_staleness_blocks("rails", &report);
        let json = serde_json::to_string(&blocks).unwrap();
        // Listed up to 10, then "…and 5 more".
        assert!(json.contains("file_0.rs"));
        assert!(json.contains("file_9.rs"));
        assert!(!json.contains("file_10.rs"));
        assert!(json.contains("5 more"));
    }

    #[test]
    fn render_failure_card_surfaces_crash_diagnostic_block() {
        // Failed status alone produces only the "Error: …" line. Adding
        // the diagnostic should produce a SECOND block beneath it with
        // the cause label + stderr code fence — that's the hook the
        // operator needs to tell OOM from "the model just gave up".
        let mut state = sample(TurnStatus::Failed("killed (signal 9 — likely OOM)".into()));
        state.crash_diagnostic = Some(CrashDiagnostic {
            label: "killed (signal 9 — likely OOM)".into(),
            stderr_tail: "memory.events: oom_kill 1\n".into(),
            wip_stash: Vec::new(),
        });
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("OOM"));
        // stderr tail must appear inside a fenced code block.
        assert!(json.contains("oom_kill 1"));
        assert!(json.contains("```"));
    }

    #[test]
    fn workspace_reset_card_names_the_loss_rotation_does_not_cause() {
        let (_, blocks) = workspace_reset_blocks(CC, Some("build a wasm skill"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("workspace is gone"));
        assert!(json.contains("build a wasm skill"));
        // The rotation card's reason would be a lie here — files did go.
        assert!(!json.contains("keep replies fast"));

        let (_, blocks) = workspace_reset_blocks(CC, None);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("workspace is gone"));
        assert!(!json.contains("Still rooted"));
    }

    #[test]
    fn usage_names_every_way_to_start_a_session() {
        let (_, blocks) = usage_blocks(CC, &["opus".into(), "fable".into()], &["low", "high"]);
        let json = serde_json::to_string(&blocks).unwrap();
        for form in ["/cc` on its own", "<repo>", "project:<name>", "/cc help"] {
            assert!(json.contains(form), "usage must document {form}");
        }
        assert!(json.contains("bench"));
    }

    #[test]
    fn fmt_retention_uses_the_coarsest_unit_that_stays_true() {
        assert_eq!(fmt_retention(Duration::from_secs(7 * 24 * 3600)), "7 days");
        assert_eq!(fmt_retention(Duration::from_secs(24 * 3600)), "1 day");
        assert_eq!(fmt_retention(Duration::from_secs(12 * 3600)), "12 hours");
        assert_eq!(fmt_retention(Duration::from_secs(3600)), "1 hour");
        // Never "0" — a sub-hour window still has to read as a duration.
        assert_eq!(fmt_retention(Duration::from_secs(60)), "1 hour");
    }

    #[test]
    fn project_welcome_card_matches_the_workspace_the_user_gets() {
        use workforce_projects::{Project, ProjectRepo, RepoSource};

        let ttl = Duration::from_secs(7 * 24 * 3600);
        let scratch = Project::new("allan", "skills");
        let (_, blocks) = welcome_project_blocks(
            CC,
            &ChatChoice::default(),
            &scratch,
            Some("build a wasm skill"),
            ttl,
            true,
        );
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("bench"));
        assert!(
            json.contains("clone any repo"),
            "the bench's point is bringing code in"
        );
        assert!(json.contains("idle for 7 days"));
        assert!(json.contains("build a wasm skill"));

        // With no token the card must not promise cloning — the user learns
        // the limit up front rather than a round later.
        let (_, blocks) =
            welcome_project_blocks(CC, &ChatChoice::default(), &scratch, None, ttl, false);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("No GitHub token is configured"));
        assert!(!json.contains("clone any repo"));

        let mut repo_backed = Project::new("allan", "harbor");
        repo_backed.repos.push(ProjectRepo {
            alias: "rails".into(),
            source: RepoSource::Github {
                owner: "acme".into(),
                repo: "rails".into(),
            },
            default_branch: None,
            setup: None,
        });
        let (_, blocks) =
            welcome_project_blocks(CC, &ChatChoice::default(), &repo_backed, None, ttl, true);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("rails"));
        assert!(!json.contains("bench"));
        assert!(
            !json.contains("idle for"),
            "retention is a scratch-only concern; repo work lives in git"
        );
    }

    #[test]
    fn format_elapsed_shape_changes_at_minute_and_hour_boundaries() {
        use std::time::Duration;
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(3_599)), "59m59s");
        assert_eq!(format_elapsed(Duration::from_secs(3_600)), "1h00m");
        assert_eq!(format_elapsed(Duration::from_secs(7_270)), "2h01m");
    }

    #[test]
    fn live_card_includes_elapsed_in_header() {
        // While running, the header is the user's only signal of
        // liveness during long tool calls — it must include elapsed.
        let state = sample(TurnStatus::Running);
        let (summary, _) = render_turn(&state);
        // Even at t=0 the formatter always emits at least "0s".
        assert!(
            summary.contains('s'),
            "live header must include an elapsed-seconds suffix; got: {summary}"
        );
    }

    #[test]
    fn done_card_drops_elapsed_in_favor_of_stats_block() {
        // Once we have final stats the precise duration is rendered in
        // its own context block; duplicating it in the header would be
        // noise.
        let mut state = sample(TurnStatus::Done);
        state.stats = Some(TurnStats {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: 0.0,
            num_turns: 1,
            duration_ms: 1234,
            thread_cost_usd: None,
            engine: None,
            model: None,
        });
        let (summary, _) = render_turn(&state);
        // The status header for Done is "done"; no elapsed suffix
        // before the user-mention dot.
        assert!(summary.contains("done · <@U1>"));
    }

    #[test]
    fn render_failure_card_lists_wip_stashes_with_recovery_command() {
        // After a crash that produced uncommitted edits, the diagnostic
        // surfaces the stashes per repo so the user can recover their
        // work via `git stash apply`. This is the post-crash WIP-stash
        // hook documented in the retrospective.
        let mut state = sample(TurnStatus::Failed("exit 1".into()));
        state.crash_diagnostic = Some(CrashDiagnostic {
            label: "exit 1".into(),
            stderr_tail: String::new(),
            wip_stash: vec![
                crate::projects::cc_session::WipStash {
                    repo: "rails".into(),
                    stash_ref: "stash@{0}".into(),
                    sha: "abcd1234abcd1234abcd1234abcd1234abcd1234".into(),
                },
                crate::projects::cc_session::WipStash {
                    repo: "frontend".into(),
                    stash_ref: "stash@{0}".into(),
                    sha: "ef567890ef567890ef567890ef567890ef567890".into(),
                },
            ],
        });
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        // Each repo + the recovery command must appear so the user can
        // copy-paste their way back.
        assert!(json.contains("rails"));
        assert!(json.contains("frontend"));
        // First 8 chars of SHA, NOT the full SHA (kept compact in the
        // card).
        assert!(json.contains("abcd1234"));
        assert!(!json.contains("abcd1234abcd1234abcd1234abcd1234abcd1234"));
        assert!(json.contains("git stash apply"));
    }

    #[test]
    fn render_failure_card_omits_diagnostic_when_none_set() {
        // Diagnostic is opt-in so non-subprocess failures (cancellations,
        // setup errors) don't render an empty code fence.
        let state = sample(TurnStatus::Failed("worktree clone failed".into()));
        let (_, blocks) = render_turn(&state);
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(!json.contains("```"));
        assert!(!json.contains("Cause:"));
    }

    #[test]
    fn stats_block_shows_thread_total_when_strictly_above_turn_cost() {
        let mut state = sample(TurnStatus::Done);
        state.stats = Some(TurnStats {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cost_usd: 0.40,
            num_turns: 1,
            duration_ms: 1_000,
            thread_cost_usd: Some(2.75),
            engine: None,
            model: None,
        });
        let json = serde_json::to_string(&render_turn(&state).1).unwrap();
        assert!(json.contains("thread $2.75"));
        // Per-turn cost still rendered with its 4-decimal precision.
        assert!(json.contains("$0.4000"));
    }

    #[test]
    fn stats_block_omits_thread_total_on_first_turn() {
        // Thread total equal to (or below) this turn's cost is rounding
        // noise on the first turn — suppress so the line stays clean.
        let mut state = sample(TurnStatus::Done);
        state.stats = Some(TurnStats {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cost_usd: 0.40,
            num_turns: 1,
            duration_ms: 1_000,
            thread_cost_usd: Some(0.40),
            engine: None,
            model: None,
        });
        let json = serde_json::to_string(&render_turn(&state).1).unwrap();
        assert!(!json.contains("thread $"));
    }

    #[test]
    fn rebase_card_summarises_per_repo_outcomes() {
        use crate::projects::cc_rebase::{RebaseOutcome, RepoRebaseReport};
        let reports = vec![
            RepoRebaseReport {
                repo: "alpha".into(),
                outcome: RebaseOutcome::Advanced {
                    from: "deadbeef".into(),
                    to: "cafebabe".into(),
                    base: "main".into(),
                },
            },
            RepoRebaseReport {
                repo: "beta".into(),
                outcome: RebaseOutcome::Conflict {
                    files: vec!["src/server.rs".into(), "src/db.rs".into()],
                    base: "main".into(),
                },
            },
        ];
        let (head, blocks) = rebase_result_blocks(CC, &reports);
        // Conflict in any repo means the headline reflects that.
        assert!(head.to_lowercase().contains("conflicts"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("alpha"));
        assert!(json.contains("beta"));
        assert!(json.contains("deadbeef"));
        assert!(json.contains("cafebabe"));
        assert!(json.contains("src/server.rs"));
        assert!(json.contains("src/db.rs"));
        // Conflict path must point the user at fixing it in the thread's CLI.
        assert!(json.contains("Ask Claude Code to resolve"));
    }

    #[test]
    fn rebase_card_clean_run_says_complete() {
        use crate::projects::cc_rebase::{RebaseOutcome, RepoRebaseReport};
        let reports = vec![RepoRebaseReport {
            repo: "alpha".into(),
            outcome: RebaseOutcome::AlreadyUpToDate,
        }];
        let (head, _) = rebase_result_blocks(CC, &reports);
        assert!(head.contains("complete"));
    }

    #[test]
    fn rebase_card_with_no_repos_explains_why() {
        let (head, blocks) = rebase_result_blocks(CC, &[]);
        assert!(head.to_lowercase().contains("nothing"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.to_lowercase().contains("no git repos"));
    }

    #[test]
    fn quality_gate_pass_card_lists_repos() {
        use crate::projects::cc_quality_gate::{GateOutcome, GateSummary, RepoGateReport};
        let summary = GateSummary::Passed {
            repos: vec!["rails".into(), "frontend".into()],
        };
        let (head, blocks) = quality_gate_blocks(CC, &summary).unwrap();
        assert!(head.contains("2 repos"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("rails"));
        assert!(json.contains("frontend"));
        // Single-repo summary mentions the repo by name in the headline.
        let single = GateSummary::Passed {
            repos: vec!["solo".into()],
        };
        let (head, _) = quality_gate_blocks(CC, &single).unwrap();
        assert!(head.contains("`solo`"));
        // Sanity: an unused enum variant compiles in this scope.
        let _ = GateOutcome::Skipped;
        let _ = RepoGateReport {
            repo: "x".into(),
            outcome: GateOutcome::Skipped,
        };
    }

    #[test]
    fn quality_gate_fail_card_includes_repo_gate_and_output() {
        use crate::projects::cc_quality_gate::{
            GateFailure, GateOutcome, GateSummary, RepoGateReport,
        };
        let summary = GateSummary::Failed {
            failures: vec![RepoGateReport {
                repo: "rails".into(),
                outcome: GateOutcome::Failed(GateFailure {
                    gate: "cargo fmt --check".into(),
                    exit_code: 1,
                    output_tail: "Diff in src/lib.rs:1: …".into(),
                }),
            }],
        };
        let (head, blocks) = quality_gate_blocks(CC, &summary).unwrap();
        assert!(head.contains("1 repo"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("rails"));
        assert!(json.contains("cargo fmt --check"));
        assert!(json.contains("src/lib.rs"));
    }

    #[test]
    fn quality_gate_nothing_to_do_returns_no_card() {
        use crate::projects::cc_quality_gate::GateSummary;
        assert!(quality_gate_blocks(CC, &GateSummary::NothingToDo).is_none());
    }

    #[test]
    fn thread_spend_threshold_card_names_band_and_total() {
        let (summary, blocks) = thread_spend_threshold_blocks(25.0, 27.42);
        assert!(summary.contains("$25"));
        assert!(summary.contains("$27.42"));
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("$25"));
        assert!(json.contains("$27.42"));
        // Soft warning — must affirm that nothing is blocked. The exact
        // phrasing is "No turns are blocked"; checking for that anchor
        // pins the soft-cap contract without being brittle to copy
        // edits elsewhere in the body.
        assert!(json.contains("No turns are blocked"));
    }

    #[test]
    fn cli_missing_blocks_tells_operator_to_rebuild() {
        let (summary, blocks) = cli_missing_blocks(CC.cli, "@anthropic-ai/claude-code", CC.command);
        assert!(summary.to_lowercase().contains("not installed"));
        let json = serde_json::to_string(&blocks).unwrap();
        // Must point at the concrete fix so the operator doesn't have to
        // grep for it.
        assert!(json.contains("@anthropic-ai/claude-code"));
        assert!(json.to_lowercase().contains("rebuilt"));
    }

    #[test]
    fn permission_prompt_ask_shows_all_four_buttons() {
        let blocks = permission_prompt_blocks(
            "atlas",
            "vault.delete",
            "delete key xyz",
            "tool requires approval",
            "req-1",
            &workforce_core::policy::PermissionPromptOptions::ask(true),
        );
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("perm_allow_once"));
        assert!(json.contains("perm_deny_once"));
        assert!(json.contains("perm_allow_always"));
        assert!(json.contains("perm_deny_always"));
        // Each button's value carries the request_id so the response
        // routes back to the right pending approval.
        assert!(json.contains("\"value\":\"req-1\""));
        assert!(json.contains("vault.delete"));
        assert!(json.contains("atlas"));
    }

    #[test]
    fn permission_prompt_always_ask_hides_allow_always() {
        let blocks = permission_prompt_blocks(
            "atlas",
            "vault.delete",
            "delete key xyz",
            "always-ask",
            "req-2",
            &workforce_core::policy::PermissionPromptOptions::ask(false),
        );
        let json = serde_json::to_string(&blocks).unwrap();
        assert!(json.contains("perm_allow_once"));
        assert!(json.contains("perm_deny_once"));
        assert!(
            !json.contains("perm_allow_always"),
            "Always-Allow must be hidden when allow_save_allow is false"
        );
        assert!(json.contains("perm_deny_always"));
    }

    #[test]
    fn permission_prompt_truncates_long_detail_and_reason() {
        let very_long_detail = "x".repeat(500);
        let very_long_reason = "y".repeat(500);
        let blocks = permission_prompt_blocks(
            "atlas",
            "vault.delete",
            &very_long_detail,
            &very_long_reason,
            "req-3",
            &workforce_core::policy::PermissionPromptOptions::ask(true),
        );
        let json = serde_json::to_string(&blocks).unwrap();
        // Slack section text has a 3000-char limit; we truncate well
        // below that. The exact caps live in `permission_prompt_blocks`.
        assert!(json.contains("…"));
    }

    #[test]
    fn playbook_control_buttons_carry_continue_exit_and_value() {
        let json = serde_json::to_string(&playbook_control_buttons("sk|n1|true")).unwrap();
        assert!(json.contains("playbook_continue"));
        assert!(json.contains("playbook_exit"));
        assert!(json.contains("sk|n1|true"));
    }

    #[test]
    fn playbook_step_context_shows_position_and_title() {
        let json =
            serde_json::to_string(&playbook_step_context(3, 10, "Research & Design")).unwrap();
        assert!(json.contains("Step 3/10: Research & Design"));
    }

    #[test]
    fn playbook_finish_button_is_lone_continue_action() {
        // Finish is a single button that reuses the continue action id, so the
        // click advances past the final step → StepOutcome::Complete.
        let blocks = playbook_finish_button("sk|n1|false");
        let elements = blocks[0]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0]["action_id"],
            serde_json::json!("playbook_continue")
        );
        assert_eq!(elements[0]["text"]["text"], serde_json::json!("Finish"));
        assert!(!serde_json::to_string(&blocks)
            .unwrap()
            .contains("playbook_exit"));
    }

    #[test]
    fn playbook_custom_buttons_get_unique_ids_targets_and_styles() {
        let buttons = vec![
            CustomButton {
                label: "✅ Approve".into(),
                style: Some("primary".into()),
                effect_token: "merge".into(),
            },
            CustomButton {
                label: "Bail".into(),
                style: None,
                effect_token: "$exit".into(),
            },
        ];
        let blocks = playbook_custom_buttons("sk|n1|true", &buttons);
        let elements = blocks[0]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        // Unique action ids (Slack rejects duplicates within one block).
        assert_eq!(
            elements[0]["action_id"],
            serde_json::json!("playbook_goto_0")
        );
        assert_eq!(
            elements[1]["action_id"],
            serde_json::json!("playbook_goto_1")
        );
        // Target rides in the 4th value part.
        assert_eq!(elements[0]["value"], serde_json::json!("sk|n1|true|merge"));
        assert_eq!(elements[1]["value"], serde_json::json!("sk|n1|true|$exit"));
        // A styled button carries its style; a default one omits the key
        // entirely (Slack only accepts primary/danger).
        assert_eq!(elements[0]["style"], serde_json::json!("primary"));
        assert!(elements[1].get("style").is_none());
    }

    #[test]
    fn playbook_custom_buttons_empty_is_no_block() {
        assert!(playbook_custom_buttons("sk|n1|true", &[]).is_empty());
    }

    #[test]
    fn playbook_exit_button_is_lone_exit() {
        let blocks = playbook_exit_button("sk|n1|true");
        let elements = blocks[0]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["action_id"], serde_json::json!("playbook_exit"));
        assert!(!serde_json::to_string(&blocks)
            .unwrap()
            .contains("playbook_continue"));
    }

    mod decision_cards {
        use super::*;
        use workforce_orchestrator::workflow::decision::Next;

        const RUN: &str = "6f1c3a52-1b7e-4c58-9f0a-2d4e5b6c7d8e";

        fn gate() -> Uuid {
            Uuid::parse_str("0b9f8e7d-6c5b-4a39-8e17-f6e5d4c3b2a1").unwrap()
        }

        fn choice(label: &str, outcome: &str, style: Option<ButtonStyle>) -> Choice {
            Choice {
                label: label.into(),
                outcome: outcome.into(),
                style,
                done: None,
                confirm: None,
                next: Next::Complete,
                start: None,
                set: Default::default(),
            }
        }

        fn choices() -> Vec<Choice> {
            vec![
                Choice {
                    confirm: Some("Send this email now?".into()),
                    ..choice("Send", "send", Some(ButtonStyle::Primary))
                },
                choice("Cancel", "cancel", Some(ButtonStyle::Danger)),
            ]
        }

        fn at() -> DateTime<Utc> {
            "2026-10-16T09:00:00Z".parse().unwrap()
        }

        fn view<'a>(choices: &'a [Choice], state: CardState<'a>) -> DecisionCardView<'a> {
            DecisionCardView {
                title: "Send today's digest?",
                context: Some("Drafted from yesterday's figures"),
                body: Some("To: all@example.com\n\nToday's figures."),
                choices,
                waiting_on: vec!["<@U1>".into(), "<@U2>".into(), "Ana".into()],
                expires: Some((at(), "Cancel")),
                state,
            }
        }

        fn kinds(blocks: &[Value]) -> Vec<&str> {
            blocks.iter().filter_map(|b| b["type"].as_str()).collect()
        }

        fn text_of(blocks: &[Value]) -> String {
            serde_json::to_string(blocks).unwrap()
        }

        #[test]
        fn an_open_card_carries_title_context_body_waiting_line_and_buttons() {
            let choices = choices();
            let (text, blocks) = decision_card(&view(&choices, CardState::Open), RUN, gate());
            assert_eq!(text, "Send today's digest?");
            assert_eq!(blocks[0]["text"]["text"], "Send today's digest?");
            assert_eq!(
                blocks[1]["elements"][0]["text"],
                "Drafted from yesterday's figures"
            );
            assert_eq!(kinds(&blocks).last(), Some(&"actions"));
            let status = blocks[blocks.len() - 2]["elements"][0]["text"]
                .as_str()
                .unwrap();
            assert!(
                status.starts_with("⏳ Waiting on <@U1>, <@U2> or Ana · expires <!date^"),
                "{status}"
            );
            assert!(status.ends_with("→ *Cancel*"), "{status}");
            assert!(text_of(&blocks).contains("Today's figures."));
            let actions = blocks.last().unwrap();
            assert_eq!(actions["block_id"], format!("wf_decision:{}", gate()));
            let buttons = actions["elements"].as_array().unwrap();
            assert_eq!(buttons[0]["action_id"], "wf_decision_0");
            assert_eq!(buttons[1]["action_id"], "wf_decision_1");
            assert_eq!(buttons[0]["style"], "primary");
            assert_eq!(buttons[1]["style"], "danger");
            assert_eq!(
                buttons[0]["confirm"]["text"]["text"],
                "Send this email now?"
            );
            assert_eq!(buttons[0]["confirm"]["deny"]["text"], "Go back");
            assert!(buttons[1].get("confirm").is_none());
        }

        #[test]
        fn button_values_carry_instance_gate_and_outcome_and_parse_back() {
            let value = decision_button_value(RUN, gate(), "send");
            assert!(value.len() <= 140);
            assert_eq!(
                parse_decision_value(&value),
                Some((RUN.to_string(), gate(), "send".to_string()))
            );
            assert_eq!(parse_decision_value("run|not-a-gate|send"), None);
            assert_eq!(parse_decision_value(&format!("{RUN}|{}", gate())), None);
            assert_eq!(parse_decision_value(""), None);
        }

        #[test]
        fn a_decided_card_has_no_actions_and_says_who_and_when() {
            let choices = choices();
            let state = CardState::Decided {
                label: "Sent",
                by_mention: "<@U1>".into(),
                at: at(),
            };
            let (_, blocks) = decision_card(&view(&choices, state), RUN, gate());
            assert!(!kinds(&blocks).contains(&"actions"));
            let status = blocks.last().unwrap()["elements"][0]["text"]
                .as_str()
                .unwrap();
            assert!(
                status.starts_with("✅ *Sent* by <@U1> · <!date^1792141200^"),
                "{status}"
            );
        }

        #[test]
        fn expired_stopped_and_closed_cards_say_so() {
            let choices = choices();
            let status = |state| {
                let (_, blocks) = decision_card(&view(&choices, state), RUN, gate());
                assert!(!kinds(&blocks).contains(&"actions"));
                blocks.last().unwrap()["elements"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            assert!(status(CardState::Expired {
                label: "Cancelled",
                at: at()
            })
            .starts_with("⌛ Expired <!date^"));
            assert_eq!(
                status(CardState::Stopped { by: Some("Carol") }),
                "⏹ Stopped by Carol · nothing was decided"
            );
            assert_eq!(status(CardState::Closed), "This decision is closed.");
        }

        #[test]
        fn labels_and_confirm_texts_keep_to_slacks_limits() {
            let long = vec![Choice {
                confirm: Some("c".repeat(400)),
                ..choice(&"l".repeat(90), "send", None)
            }];
            let title = "t".repeat(200);
            let card = DecisionCardView {
                title: &title,
                ..view(&long, CardState::Open)
            };
            let (_, blocks) = decision_card(&card, RUN, gate());
            let chars = |v: &Value| v.as_str().unwrap().chars().count();
            assert_eq!(chars(&blocks[0]["text"]["text"]), 150);
            let button = &blocks.last().unwrap()["elements"][0];
            assert_eq!(chars(&button["text"]["text"]), 75);
            assert_eq!(chars(&button["confirm"]["text"]["text"]), 300);
            assert_eq!(chars(&button["confirm"]["confirm"]["text"]), 30);
            assert!(button.get("style").is_none());
        }

        #[test]
        fn a_card_never_exceeds_fifty_blocks_and_overflow_goes_to_the_thread() {
            let body = (0..120)
                .map(|n| format!("Paragraph {n}."))
                .collect::<Vec<_>>()
                .join("\n\n");
            let choices = choices();
            let card = DecisionCardView {
                body: Some(&body),
                ..view(&choices, CardState::Open)
            };
            let (_, blocks) = decision_card(&card, RUN, gate());
            assert!(blocks.len() <= 50, "{}", blocks.len());
            assert!(text_of(&blocks).contains("Continued in the thread."));
            let (head, rest) = decision_card_overflow(&body);
            assert!(head.len() <= 43);
            assert!(!rest.is_empty());
            assert!(rest.iter().all(|group| group.len() <= 50));
            let all = text_of(&head) + &rest.iter().map(|g| text_of(g)).collect::<String>();
            assert!(all.contains("Paragraph 0.") && all.contains("Paragraph 119."));
        }

        #[test]
        fn defuse_broadcasts_neutralises_here_channel_and_everyone() {
            let defused = defuse_broadcasts(
                "<!here> <!channel|channel> <!everyone> @here @Channel @everyone \
                 <!subteam^S0123|@reviewers>",
            );
            for word in ["here", "channel", "everyone", "Channel"] {
                assert!(!defused.contains(&format!("<!{word}")), "{defused}");
                assert!(!defused.contains(&format!("@{word}")), "{defused}");
            }
            assert!(defused.contains("<!\u{2060}here>"));
            assert!(defused.contains("@\u{2060}everyone"));
            assert!(defused.contains("<!\u{2060}subteam^S0123|@reviewers>"));
            let (head, _) = decision_card_overflow(&defused);
            assert!(!text_of(&head).contains("\"broadcast\""));
            assert_eq!(
                defuse_broadcasts("somewhere @hereford"),
                "somewhere @hereford"
            );
        }

        #[test]
        fn a_long_waiting_line_names_twenty_and_counts_the_rest() {
            let names = |n: usize| (1..=n).map(|i| format!("P{i}")).collect::<Vec<_>>();
            assert_eq!(either(&names(3)), "P1, P2 or P3");
            assert_eq!(either(&names(20)).matches(", ").count(), 18);
            let many = either(&names(212));
            assert!(many.starts_with("P1, P2,"), "{many}");
            assert!(many.ends_with("P20 or 192 others"), "{many}");
        }

        #[test]
        fn a_card_body_has_its_secrets_redacted() {
            let cleaned = card_text(concat!(
                "token: ghp_EX",
                "AMPLE00000000000000000000000000000 <!here>"
            ));
            assert!(cleaned.contains("[REDACTED GITHUB_PAT]"), "{cleaned}");
            assert!(!cleaned.contains("ghp_"));
            assert!(!cleaned.contains("<!here>"));
        }
    }
}
