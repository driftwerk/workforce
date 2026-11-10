//! `[oversight]` in an agent's `config.toml`: whether it oversees the crew
//! (the person's other agents and their Claude Code and Codex sessions in
//! Desk), how much it may do on its own, and when it speaks up.
//!
//! ```toml
//! [oversight]
//! enabled = true
//! group = "g-acme" # absent: everything ungrouped
//! level = "watch" # watch | suggest | act | full
//! track = { crew = true, desk_sessions = true, external_sessions = true, workspaces = false }
//! wake = { waiting_minutes = 5, quiet_minutes = 30, remind_minutes = 120, on_finish = true, on_failure = true, on_message = true, hold_minutes = 2 }
//! summaries = { morning = "09:00", evening = "" }
//! max_checkins_per_hour = 6
//! max_checkins_per_day = 100
//! act = { nudge = true, resume_restart = true, read_only_approvals = false, hand_out_work = false, interrupt = false, message_agents = false, message_terminal_sessions = false, answer_sessions = true, hand_off = false }
//! work = { prefer_sessions = true, parallel = 4, take_on = "handed", engine = "claude", max_turns_per_hour = 60 }
//! full = { repos = ["acme/acme-web", "~/dev/globex-api"], workflows = ["deploy.yml", "promote.yml"], budget_usd_per_day = 25 }
//! ```
//!
//! `full` is **Act fully**: everything `act` does, and, in the repositories
//! `[oversight.full] repos` names (`owner/name`, which is GitHub's, a
//! remote's URL, or a local checkout's path), answering and allowing, in
//! the sessions it runs and, with `answer_sessions`, in the others too:
//! merging a pull request by its number, pushing the default branch to
//! `origin`, creating and pushing one tag, `gh release create <tag>`, and
//! `gh workflow run` of a workflow `workflows` lists (by its file name
//! exactly) on the default branch, with no inputs. Deploys run through
//! those workflows: deploy tools and the project's own release, deploy and
//! promote scripts stay the person's. It needs both named
//! repositories and `budget_usd_per_day`, and holds only while today's
//! spend is under that budget; short of any of it, it acts as `act` and
//! says why ([`Oversight::fully`]). What no overseer does at any level, Act
//! fully included, is Desk's own list (`workforce_desk::never`).
//!
//! `group` is the sidebar group it oversees, as that group's boss: it sees
//! only that group, and the main overseer (no `group`) sees nothing inside
//! it. One agent oversees each scope; Desk checks the group is one of the
//! sidebar's.
//!
//! `[oversight.work]` is how it runs sessions for the person: the sessions
//! in its charge (handed to it, or every one with `take_on = "all"`) wake
//! it at each turn's end, up to `max_turns_per_hour` such turns, and at
//! most `parallel` of them start or work at once.
//!
//! A session asks the overseer on Desk's line: with `on_message` it gets a
//! turn of its own for that, else every question goes straight to the
//! person. At `act`, `answer_sessions` lets it answer; anything that reads
//! as pushing, deleting or sending, in the question or the answer, is the
//! person's to answer, and the overseer can only draft it.
//!
//! What the overseer sends an agent, or a session started in another
//! terminal, may lead to something done without asking: an agent acts under
//! its own permissions, and such a session may have been started not to ask.
//! So at level `act` it only suggests them, unless `message_agents` (nudges
//! and tasks to agents) or `message_terminal_sessions` (nudges to those
//! sessions) is on. Words that read as pushing, deleting or sending still
//! make any action a suggestion.
//!
//! A session in its charge is the person's while they are active in it:
//! `hold_minutes` after their last keystroke, prompt or message there. What
//! the session asks after that is the overseer's to answer; what it asks
//! while they are still typing waits for them.
//!
//! Check-ins are spent from a rolling hour, `max_checkins_per_day` the
//! ceiling over a day; what happens while they are spent waits for the next
//! one. Something waiting on the person, or a question a session asked,
//! wakes it once, and again only after `remind_minutes`.
//!
//! The table is read leniently by serde (it is kept as a raw value) and
//! checked here, so what is wrong with it is said in plain words, without
//! key names: Desk shows the refusal to the person as it is.

use serde::Serialize;
use toml::Value;
pub use workforce_core::scope::Scope;

/// How much an overseer does on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Looks only.
    #[default]
    Watch,
    /// Every action becomes a suggestion the person accepts or dismisses.
    Suggest,
    /// The actions `[oversight.act]` allows run; the rest are suggestions.
    Act,
    /// Act fully: as `Act`, and shipping (merging, pushing to the default
    /// branch, tagging, promoting, deploying) in the repositories
    /// `[oversight.full]` names, within its daily budget.
    Full,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::Suggest => "suggest",
            Self::Act => "act",
            Self::Full => "full",
        }
    }

    /// Whether it acts on its own: `act`, or `full` (which acts as `act`
    /// wherever Act fully doesn't reach).
    pub fn acts(self) -> bool {
        matches!(self, Self::Act | Self::Full)
    }
}

/// What the overseer keeps an eye on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Track {
    pub crew: bool,
    pub desk_sessions: bool,
    pub external_sessions: bool,
    pub workspaces: bool,
}

/// When the overseer speaks up on its own. A minute threshold of 0 is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Wake {
    pub waiting_minutes: u32,
    pub quiet_minutes: u32,
    /// How long before a question it already raised, still open and
    /// unchanged, is raised again; 0 never.
    pub remind_minutes: u32,
    pub on_finish: bool,
    pub on_failure: bool,
    /// A session's message to it on Desk's line wakes it; off, sessions'
    /// questions go to the person.
    pub on_message: bool,
    /// How long the person's last input into a session it runs keeps that
    /// session theirs.
    pub hold_minutes: u32,
}

/// The local times of the morning and evening summaries, `None` when off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summaries {
    pub morning: Option<(u32, u32)>,
    pub evening: Option<(u32, u32)>,
}

/// What the overseer may do without asking, at level `act`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Act {
    pub nudge: bool,
    pub resume_restart: bool,
    pub read_only_approvals: bool,
    pub hand_out_work: bool,
    /// Ending a session's turn (Esc in its terminal).
    pub interrupt: bool,
    /// Nudging an agent (with `nudge`), or handing it a task. Its turn
    /// answers to its Desk DM, relayed, at the trust of a paired device, so
    /// what it would post or send anywhere but Desk, delete, remember or
    /// hand on waits on the person (#64); its other tools run under its own
    /// permissions, as in any turn.
    pub message_agents: bool,
    /// Nudging a session started in another terminal (with `nudge`), which
    /// may have been started not to ask before it acts.
    pub message_terminal_sessions: bool,
    /// Answering what sessions ask it on Desk's line.
    pub answer_sessions: bool,
    /// Suggesting a session continue in another engine (`crew.handoff`):
    /// only ever a suggestion, since its brief goes to that engine's
    /// vendor; off, it isn't even suggested.
    pub hand_off: bool,
}

/// `[oversight.full]`: where, and on what budget, Act fully ships.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Full {
    /// The repositories it may ship in: `owner/name`, a remote's URL, or a
    /// local checkout's path, as the person wrote them.
    pub repos: Vec<String>,
    /// What it may spend in a day, in cents; `None` until said.
    pub budget_cents_per_day: Option<u64>,
    /// The GitHub workflows it may run (`gh workflow run`), by their file
    /// names exactly (`deploy.yml`); none, it runs none.
    pub workflows: Vec<String>,
}

impl Full {
    /// The daily budget in dollars, when there is one.
    pub fn budget_usd_per_day(&self) -> Option<f64> {
        self.budget_cents_per_day.map(|c| c as f64 / 100.0)
    }
}

/// Why Act fully isn't in force though chosen: no repositories named.
pub const FULL_NO_REPOS: &str =
    "Act fully needs at least one repository named, so it acts within Desk for now.";
/// Why Act fully isn't in force though chosen: no daily budget.
pub const FULL_NO_BUDGET: &str = "Act fully needs a daily budget, so it acts within Desk for now.";
/// The most workflows Act fully may name.
pub const MAX_FULL_WORKFLOWS: usize = 20;
/// The most repositories Act fully may name.
pub const MAX_FULL_REPOS: usize = 50;
/// The largest daily budget Act fully may have, in dollars.
pub const MAX_FULL_BUDGET_USD: f64 = 10_000.0;

/// Which sessions the overseer takes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TakeOn {
    /// Only the sessions it starts and the ones the person hands it.
    #[default]
    Handed,
    /// Every session it can, as each appears.
    All,
}

impl TakeOn {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handed => "handed",
            Self::All => "all",
        }
    }
}

/// How the overseer runs sessions for the person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Work {
    /// Real, visible sessions for the work; helpers only for read-only
    /// lookups.
    pub prefer_sessions: bool,
    /// How many of its sessions may be starting or working at once.
    pub parallel: u32,
    pub take_on: TakeOn,
    /// The engine new sessions run on when nothing says another.
    pub engine: String,
    /// Its turns for its sessions' turn ends, in any hour.
    pub max_turns_per_hour: u32,
}

/// The most sessions an overseer may run at once.
pub const MAX_PARALLEL: i64 = 12;
/// The most turns an hour it may spend on its sessions' turn ends.
pub const MAX_WORK_TURNS_PER_HOUR: i64 = 600;

/// An agent's `[oversight]`, every default filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Oversight {
    pub enabled: bool,
    /// What it oversees: everything ungrouped, or one sidebar group.
    pub scope: Scope,
    pub level: Level,
    pub track: Track,
    pub wake: Wake,
    pub summaries: Summaries,
    /// Check-ins in any hour.
    pub max_checkins_per_hour: u32,
    /// Check-ins in a day, the ceiling over the hourly budget.
    pub max_checkins_per_day: u32,
    pub act: Act,
    pub work: Work,
    /// `[oversight.full]`, read whatever the level (kept for when it is
    /// `full`).
    pub full: Full,
}

/// What the group it oversees is said wrongly as.
pub const NOT_A_GROUP: &str = "The group it oversees must be one of your sidebar's groups.";

/// The longest a minute threshold may be: a day.
pub const MAX_MINUTES: i64 = 1440;
/// The most check-ins a day may allow.
pub const MAX_CHECKINS: i64 = 500;
/// The most check-ins an hour may allow: one a minute.
pub const MAX_CHECKINS_PER_HOUR: i64 = 60;
/// How long, by default, the person's last input into a session the
/// overseer runs keeps it theirs.
pub const HOLD_MINUTES: u32 = 2;
/// The longest that hold may be: an hour.
pub const MAX_HOLD_MINUTES: i64 = 60;

impl Default for Oversight {
    fn default() -> Self {
        Self {
            enabled: false,
            scope: Scope::Ungrouped,
            level: Level::Watch,
            track: Track {
                crew: true,
                desk_sessions: true,
                external_sessions: true,
                workspaces: false,
            },
            wake: Wake {
                waiting_minutes: 5,
                quiet_minutes: 30,
                remind_minutes: 120,
                on_finish: true,
                on_failure: true,
                on_message: true,
                hold_minutes: HOLD_MINUTES,
            },
            summaries: Summaries {
                morning: Some((9, 0)),
                evening: None,
            },
            max_checkins_per_hour: 6,
            max_checkins_per_day: 100,
            act: Act {
                nudge: true,
                resume_restart: true,
                read_only_approvals: false,
                hand_out_work: false,
                interrupt: false,
                message_agents: false,
                message_terminal_sessions: false,
                answer_sessions: true,
                hand_off: false,
            },
            work: Work {
                prefer_sessions: true,
                parallel: 4,
                take_on: TakeOn::Handed,
                engine: "claude".into(),
                max_turns_per_hour: 60,
            },
            full: Full::default(),
        }
    }
}

type Table = toml::map::Map<String, Value>;

fn table<'a>(value: Option<&'a Value>, what: &str) -> Result<Option<&'a Table>, String> {
    match value {
        None => Ok(None),
        Some(Value::Table(t)) => Ok(Some(t)),
        Some(_) => Err(format!(
            "The settings for {what} need to be a group of settings."
        )),
    }
}

fn switch(t: Option<&Table>, key: &str, default: bool, what: &str) -> Result<bool, String> {
    match t.and_then(|t| t.get(key)) {
        None => Ok(default),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(_) => Err(format!(
            "Whether {what} is on or off must be true or false."
        )),
    }
}

fn whole(
    t: Option<&Table>,
    key: &str,
    default: u32,
    range: std::ops::RangeInclusive<i64>,
    problem: &str,
) -> Result<u32, String> {
    match t.and_then(|t| t.get(key)) {
        None => Ok(default),
        Some(Value::Integer(n)) if range.contains(n) => Ok(*n as u32),
        Some(_) => Err(problem.to_string()),
    }
}

/// `"09:00"` as hours and minutes; `""` is off.
fn time(
    t: Option<&Table>,
    key: &str,
    default: Option<(u32, u32)>,
    what: &str,
) -> Result<Option<(u32, u32)>, String> {
    let problem = || {
        format!("The time of the {what} summary must look like 09:00 or 18:30, or be empty to turn it off.")
    };
    match t.and_then(|t| t.get(key)) {
        None => Ok(default),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => {
            let (h, m) = s.trim().split_once(':').ok_or_else(problem)?;
            let h: u32 = h.parse().map_err(|_| problem())?;
            let m: u32 = m.parse().map_err(|_| problem())?;
            if h > 23 || m > 59 || s.trim().split_once(':').is_some_and(|(_, m)| m.len() != 2) {
                return Err(problem());
            }
            Ok(Some((h, m)))
        }
        Some(_) => Err(problem()),
    }
}

/// `[oversight.full]` as written, or what is wrong with it.
fn full_of(t: Option<&Table>) -> Result<Full, String> {
    let repos_wrong = || {
        format!(
            "The repositories it may ship in must be a list of up to {MAX_FULL_REPOS} names like acme/acme-web, remote addresses, or folder paths."
        )
    };
    let repos = match t.and_then(|t| t.get("repos")) {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= MAX_FULL_REPOS => {
            let mut out: Vec<String> = Vec::new();
            for item in items {
                match item {
                    Value::String(r) if !r.trim().is_empty() && r.trim().len() <= 500 => {
                        let r = r.trim().to_string();
                        if !out.contains(&r) {
                            out.push(r);
                        }
                    }
                    _ => return Err(repos_wrong()),
                }
            }
            out
        }
        Some(_) => return Err(repos_wrong()),
    };
    let workflows_wrong = || {
        format!(
            "The workflows it may run must be a list of up to {MAX_FULL_WORKFLOWS} file names like deploy.yml."
        )
    };
    let workflows = match t.and_then(|t| t.get("workflows")) {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= MAX_FULL_WORKFLOWS => {
            let mut out: Vec<String> = Vec::new();
            for item in items {
                match item {
                    // A plain file name: gh would read a path, or a number
                    // as a workflow's id.
                    Value::String(w)
                        if w.len() <= 200
                            && w.chars().all(|c| {
                                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
                            })
                            && w.strip_suffix(".yml")
                                .or_else(|| w.strip_suffix(".yaml"))
                                .is_some_and(|stem| {
                                    !stem.is_empty()
                                        && !stem.starts_with('.')
                                        && !stem.chars().all(|c| c.is_ascii_digit())
                                }) =>
                    {
                        if !out.contains(w) {
                            out.push(w.clone());
                        }
                    }
                    _ => return Err(workflows_wrong()),
                }
            }
            out
        }
        Some(_) => return Err(workflows_wrong()),
    };
    let budget_wrong = || {
        format!(
            "Its daily budget must be an amount in dollars above 0 and up to {MAX_FULL_BUDGET_USD}."
        )
    };
    let usd = match t.and_then(|t| t.get("budget_usd_per_day")) {
        None => None,
        Some(Value::Integer(n)) => Some(*n as f64),
        Some(Value::Float(f)) => Some(*f),
        Some(_) => return Err(budget_wrong()),
    };
    let budget_cents_per_day = match usd {
        None => None,
        Some(u) if u.is_finite() && u > 0.0 && u <= MAX_FULL_BUDGET_USD => {
            Some(((u * 100.0).round() as u64).max(1))
        }
        Some(_) => return Err(budget_wrong()),
    };
    Ok(Full {
        repos,
        budget_cents_per_day,
        workflows,
    })
}

impl Oversight {
    /// The `[oversight]` table as the hub runs it, or what is wrong with it,
    /// in plain words. Keys this hub does not know are ignored.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let d = Self::default();
        let root = table(Some(value), "overseeing the crew")?;
        let level = match root.and_then(|t| t.get("level")) {
            None => d.level,
            Some(Value::String(s)) => match s.trim() {
                "watch" => Level::Watch,
                "suggest" => Level::Suggest,
                "act" => Level::Act,
                "full" => Level::Full,
                _ => return Err(
                    "How much the overseer does on its own must be watch, suggest, act or full."
                        .into(),
                ),
            },
            Some(_) => {
                return Err(
                    "How much the overseer does on its own must be watch, suggest, act or full."
                        .into(),
                )
            }
        };
        let scope = match root.and_then(|t| t.get("group")) {
            None => Scope::Ungrouped,
            Some(Value::String(g)) if g.trim() == "ungrouped" => Scope::Ungrouped,
            Some(Value::String(g)) if !g.trim().is_empty() && g.trim().len() <= 100 => {
                Scope::Group(g.trim().to_string())
            }
            Some(_) => return Err(NOT_A_GROUP.into()),
        };
        let track = table(root.and_then(|t| t.get("track")), "what it keeps an eye on")?;
        let wake = table(root.and_then(|t| t.get("wake")), "when it speaks up")?;
        let summaries = table(root.and_then(|t| t.get("summaries")), "the summaries")?;
        let act = table(
            root.and_then(|t| t.get("act")),
            "what it may do without asking",
        )?;
        let work = table(
            root.and_then(|t| t.get("work")),
            "how it runs sessions for you",
        )?;
        let full = full_of(table(
            root.and_then(|t| t.get("full")),
            "what it may ship on its own",
        )?)?;
        let take_on = match work.and_then(|t| t.get("take_on")) {
            None => d.work.take_on,
            Some(Value::String(s)) if s.trim() == "handed" => TakeOn::Handed,
            Some(Value::String(s)) if s.trim() == "all" => TakeOn::All,
            Some(_) => {
                return Err("Whether it takes on sessions must be \"handed\" or \"all\".".into())
            }
        };
        let engine = match work.and_then(|t| t.get("engine")) {
            None => d.work.engine.clone(),
            Some(Value::String(s))
                if !s.trim().is_empty()
                    && s.trim()
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) =>
            {
                s.trim().to_string()
            }
            Some(_) => {
                return Err(
                    "Its default engine must be named in one word, such as claude or codex.".into(),
                )
            }
        };
        let minutes = |key: &str, default: u32, what: &str| {
            whole(
                wake,
                key,
                default,
                0..=MAX_MINUTES,
                &format!(
                    "The minutes before it speaks up about {what} must be a whole number from 0 (never) to {MAX_MINUTES} (a day)."
                ),
            )
        };
        Ok(Self {
            enabled: switch(root, "enabled", d.enabled, "overseeing the crew")?,
            scope,
            level,
            track: Track {
                crew: switch(track, "crew", d.track.crew, "watching your agents")?,
                desk_sessions: switch(
                    track,
                    "desk_sessions",
                    d.track.desk_sessions,
                    "watching the sessions started in Desk",
                )?,
                external_sessions: switch(
                    track,
                    "external_sessions",
                    d.track.external_sessions,
                    "watching sessions in other terminals",
                )?,
                workspaces: switch(
                    track,
                    "workspaces",
                    d.track.workspaces,
                    "watching other workspaces",
                )?,
            },
            wake: Wake {
                waiting_minutes: minutes(
                    "waiting_minutes",
                    d.wake.waiting_minutes,
                    "something waiting on you",
                )?,
                quiet_minutes: minutes("quiet_minutes", d.wake.quiet_minutes, "work gone quiet")?,
                remind_minutes: minutes(
                    "remind_minutes",
                    d.wake.remind_minutes,
                    "a question it already raised",
                )?,
                on_finish: switch(
                    wake,
                    "on_finish",
                    d.wake.on_finish,
                    "speaking up when work finishes",
                )?,
                on_failure: switch(
                    wake,
                    "on_failure",
                    d.wake.on_failure,
                    "speaking up when work fails",
                )?,
                on_message: switch(
                    wake,
                    "on_message",
                    d.wake.on_message,
                    "answering when a session asks it something",
                )?,
                hold_minutes: whole(
                    wake,
                    "hold_minutes",
                    d.wake.hold_minutes,
                    1..=MAX_HOLD_MINUTES,
                    &format!(
                        "How long your typing in a session keeps it yours must be a whole number of minutes from 1 to {MAX_HOLD_MINUTES}."
                    ),
                )?,
            },
            summaries: Summaries {
                morning: time(summaries, "morning", d.summaries.morning, "morning")?,
                evening: time(summaries, "evening", d.summaries.evening, "evening")?,
            },
            max_checkins_per_hour: whole(
                root,
                "max_checkins_per_hour",
                d.max_checkins_per_hour,
                1..=MAX_CHECKINS_PER_HOUR,
                &format!(
                    "The most check-ins an hour must be a whole number from 1 to {MAX_CHECKINS_PER_HOUR}."
                ),
            )?,
            max_checkins_per_day: whole(
                root,
                "max_checkins_per_day",
                d.max_checkins_per_day,
                1..=MAX_CHECKINS,
                &format!(
                    "The most check-ins a day must be a whole number from 1 to {MAX_CHECKINS}."
                ),
            )?,
            act: Act {
                nudge: switch(act, "nudge", d.act.nudge, "nudging without asking")?,
                resume_restart: switch(
                    act,
                    "resume_restart",
                    d.act.resume_restart,
                    "resuming and restarting without asking",
                )?,
                read_only_approvals: switch(
                    act,
                    "read_only_approvals",
                    d.act.read_only_approvals,
                    "allowing read-only requests without asking",
                )?,
                hand_out_work: switch(
                    act,
                    "hand_out_work",
                    d.act.hand_out_work,
                    "handing out work without asking",
                )?,
                interrupt: switch(
                    act,
                    "interrupt",
                    d.act.interrupt,
                    "interrupting a session without asking",
                )?,
                message_agents: switch(
                    act,
                    "message_agents",
                    d.act.message_agents,
                    "messaging agents without asking",
                )?,
                message_terminal_sessions: switch(
                    act,
                    "message_terminal_sessions",
                    d.act.message_terminal_sessions,
                    "messaging sessions started in another terminal without asking",
                )?,
                answer_sessions: switch(
                    act,
                    "answer_sessions",
                    d.act.answer_sessions,
                    "answering sessions' questions without asking",
                )?,
                hand_off: switch(
                    act,
                    "hand_off",
                    d.act.hand_off,
                    "suggesting a session continue in another engine",
                )?,
            },
            work: Work {
                prefer_sessions: switch(
                    work,
                    "prefer_sessions",
                    d.work.prefer_sessions,
                    "preferring real sessions to helpers",
                )?,
                parallel: whole(
                    work,
                    "parallel",
                    d.work.parallel,
                    1..=MAX_PARALLEL,
                    &format!(
                        "How many sessions it may run at once must be a whole number from 1 to {MAX_PARALLEL}."
                    ),
                )?,
                take_on,
                engine,
                max_turns_per_hour: whole(
                    work,
                    "max_turns_per_hour",
                    d.work.max_turns_per_hour,
                    1..=MAX_WORK_TURNS_PER_HOUR,
                    &format!(
                        "The most turns an hour it may spend on its sessions must be a whole number from 1 to {MAX_WORK_TURNS_PER_HOUR}."
                    ),
                )?,
            },
            full,
        })
    }

    /// Whether Act fully is in force by its settings, or why not: it is
    /// chosen, with repositories named and a daily budget. (Whether today's
    /// spend is under that budget is the hub's to read.) `Err(None)` when
    /// it isn't chosen at all.
    pub fn fully(&self) -> Result<&Full, Option<&'static str>> {
        if self.level != Level::Full {
            return Err(None);
        }
        if self.full.repos.is_empty() {
            return Err(Some(FULL_NO_REPOS));
        }
        if self.full.budget_cents_per_day.is_none() {
            return Err(Some(FULL_NO_BUDGET));
        }
        Ok(&self.full)
    }

    /// `[oversight]` as a config holds it: absent is the defaults (off).
    pub fn of(value: Option<&Value>) -> Result<Self, String> {
        value.map_or_else(|| Ok(Self::default()), Self::from_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<Oversight, String> {
        let doc: Table = toml::from_str(source).unwrap();
        Oversight::of(doc.get("oversight"))
    }

    #[test]
    fn answering_sessions_is_on_unless_switched_off_and_said_plainly_when_wrong() {
        let o = parse("[oversight]\nenabled = true\n").unwrap();
        assert!(o.act.answer_sessions && o.wake.on_message);
        let off = parse(
            "[oversight]\nact = { answer_sessions = false }\nwake = { on_message = false }\n",
        )
        .unwrap();
        assert!(!off.act.answer_sessions && !off.wake.on_message);
        assert_eq!(
            parse("[oversight]\nact = { answer_sessions = \"yes\" }\n").unwrap_err(),
            "Whether answering sessions' questions without asking is on or off must be true or false."
        );
        assert_eq!(
            parse("[oversight]\nwake = { on_message = 1 }\n").unwrap_err(),
            "Whether answering when a session asks it something is on or off must be true or false."
        );
    }

    #[test]
    fn the_persons_hold_is_two_minutes_unless_said_and_said_plainly_when_wrong() {
        assert_eq!(
            parse("[oversight]\nenabled = true\n")
                .unwrap()
                .wake
                .hold_minutes,
            2
        );
        assert_eq!(
            parse("[oversight.wake]\nhold_minutes = 5\n")
                .unwrap()
                .wake
                .hold_minutes,
            5
        );
        let wrong = "How long your typing in a session keeps it yours must be a whole number of minutes from 1 to 60.";
        for bad in ["0", "61", "\"2\"", "1.5"] {
            assert_eq!(
                parse(&format!("[oversight]\nwake = {{ hold_minutes = {bad} }}\n")).unwrap_err(),
                wrong,
                "{bad}"
            );
        }
    }

    #[test]
    fn act_fully_needs_named_repositories_and_a_budget_else_it_acts_within_desk() {
        let full = parse(
            "[oversight]\nenabled = true\nlevel = \"full\"\n\
             [oversight.full]\nrepos = [\"acme/acme-web\", \" ~/dev/globex-api \", \"acme/acme-web\"]\nbudget_usd_per_day = 25\n",
        )
        .unwrap();
        assert_eq!(full.level, Level::Full);
        assert!(full.level.acts());
        let f = full.fully().unwrap();
        assert_eq!(f.repos, vec!["acme/acme-web", "~/dev/globex-api"]);
        assert_eq!(f.budget_cents_per_day, Some(2500));
        assert_eq!(f.budget_usd_per_day(), Some(25.0));
        let cents = parse("[oversight.full]\nbudget_usd_per_day = 7.5\n").unwrap();
        assert_eq!(cents.full.budget_cents_per_day, Some(750));

        // Short of either, it acts as act and says why; not chosen, it's
        // nothing to say.
        let no_repos =
            parse("[oversight]\nlevel = \"full\"\nfull = { budget_usd_per_day = 5 }\n").unwrap();
        assert_eq!(no_repos.fully(), Err(Some(FULL_NO_REPOS)));
        let no_budget =
            parse("[oversight]\nlevel = \"full\"\nfull = { repos = [\"acme/acme-web\"] }\n")
                .unwrap();
        assert_eq!(no_budget.fully(), Err(Some(FULL_NO_BUDGET)));
        let act = parse("[oversight]\nlevel = \"act\"\nfull = { repos = [\"acme/acme-web\"], budget_usd_per_day = 5 }\n").unwrap();
        assert_eq!(act.fully(), Err(None));

        for bad in ["repos = \"acme/acme-web\"", "repos = [\"\"]", "repos = [3]"] {
            assert!(
                parse(&format!("[oversight.full]\n{bad}\n"))
                    .unwrap_err()
                    .starts_with("The repositories it may ship in"),
                "{bad}"
            );
        }
        for bad in ["0", "-5", "\"25\"", "10001"] {
            assert!(
                parse(&format!("[oversight.full]\nbudget_usd_per_day = {bad}\n"))
                    .unwrap_err()
                    .starts_with("Its daily budget"),
                "{bad}"
            );
        }
        assert_eq!(
            parse("[oversight]\nlevel = \"fully\"\n").unwrap_err(),
            "How much the overseer does on its own must be watch, suggest, act or full."
        );
        assert_eq!(serde_json::to_value(Level::Full).unwrap(), "full");
        let listed = parse(
            "[oversight.full]\nworkflows = [\"deploy.yml\", \"promote.yaml\", \"deploy.yml\"]\n",
        )
        .unwrap();
        assert_eq!(listed.full.workflows, vec!["deploy.yml", "promote.yaml"]);
        assert!(parse("").unwrap().full.workflows.is_empty());
        for bad in [
            "\"deploy\"",
            "\".github/workflows/deploy.yml\"",
            "3",
            "\"a b.yml\"",
            "\"123.yml\"",
            "\".yml\"",
            "\"..yml\"",
            "\"deploy.yml.txt\"",
        ] {
            assert!(
                parse(&format!("[oversight.full]\nworkflows = [{bad}]\n"))
                    .unwrap_err()
                    .starts_with("The workflows it may run"),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_group_it_oversees_is_one_groups_id_or_everything_ungrouped() {
        assert_eq!(
            parse("[oversight]\nenabled = true\n").unwrap().scope,
            Scope::Ungrouped
        );
        assert_eq!(
            parse("[oversight]\ngroup = \" g-acme \"\n").unwrap().scope,
            Scope::Group("g-acme".into())
        );
        assert_eq!(
            parse("[oversight]\ngroup = \"ungrouped\"\n").unwrap().scope,
            Scope::Ungrouped
        );
        for bad in ["group = \"\"", "group = 3", "group = [\"g-acme\"]"] {
            assert_eq!(
                parse(&format!("[oversight]\n{bad}\n")).unwrap_err(),
                NOT_A_GROUP,
                "{bad}"
            );
        }
    }

    #[test]
    fn absent_is_off_and_the_defaults() {
        assert_eq!(parse("").unwrap(), Oversight::default());
        assert!(!parse("").unwrap().enabled);
    }

    #[test]
    fn inline_and_sub_tables_read_the_same() {
        let inline = parse(
            "[oversight]\nenabled = true\nlevel = \"act\"\n\
             wake = { waiting_minutes = 0, quiet_minutes = 12, remind_minutes = 0, on_finish = false }\n\
             max_checkins_per_hour = 3\n\
             summaries = { morning = \"\", evening = \"18:30\" }\n\
             act = { read_only_approvals = true }\n",
        )
        .unwrap();
        let tables = parse(
            "[oversight]\nenabled = true\nlevel = \"act\"\n\n\
             max_checkins_per_hour = 3\n\n\
             [oversight.wake]\nwaiting_minutes = 0\nquiet_minutes = 12\nremind_minutes = 0\non_finish = false\n\n\
             [oversight.summaries]\nmorning = \"\"\nevening = \"18:30\"\n\n\
             [oversight.act]\nread_only_approvals = true\n",
        )
        .unwrap();
        assert_eq!(inline, tables);
        assert_eq!(inline.level, Level::Act);
        assert_eq!(
            (
                inline.wake.waiting_minutes,
                inline.wake.quiet_minutes,
                inline.wake.remind_minutes
            ),
            (0, 12, 0)
        );
        assert_eq!(
            (inline.max_checkins_per_hour, inline.max_checkins_per_day),
            (3, 100)
        );
        assert!(!inline.wake.on_finish && inline.wake.on_failure);
        assert_eq!(inline.summaries.morning, None);
        assert_eq!(inline.summaries.evening, Some((18, 30)));
        assert!(inline.act.read_only_approvals && inline.act.nudge);
        assert!(
            !inline.act.interrupt,
            "interrupting is the person's until they say"
        );
        let interrupts =
            parse("[oversight]\nenabled = true\n[oversight.act]\ninterrupt = true\n").unwrap();
        assert!(interrupts.act.interrupt);
        assert!(parse("[oversight]\nact = { interrupt = \"yes\" }\n")
            .unwrap_err()
            .contains("interrupting a session without asking"));
    }

    /// Messaging agents and sessions started in another terminal are each
    /// the person's to switch on, and off until they do.
    #[test]
    fn messaging_agents_and_terminal_sessions_are_off_until_switched_on() {
        let d = parse("[oversight]\nenabled = true\nlevel = \"act\"\n").unwrap();
        assert!(!d.act.message_agents && !d.act.message_terminal_sessions);
        let agents =
            parse("[oversight]\nlevel = \"act\"\nact = { message_agents = true }\n").unwrap();
        assert!(agents.act.message_agents && !agents.act.message_terminal_sessions);
        let terminals =
            parse("[oversight]\n[oversight.act]\nmessage_terminal_sessions = true\n").unwrap();
        assert!(!terminals.act.message_agents && terminals.act.message_terminal_sessions);
        for (source, says) in [
            (
                "[oversight]\nact = { message_agents = 1 }\n",
                "messaging agents without asking",
            ),
            (
                "[oversight]\nact = { message_terminal_sessions = \"on\" }\n",
                "messaging sessions started in another terminal without asking",
            ),
        ] {
            let e = parse(source).unwrap_err();
            assert!(
                e.contains(says) && e.contains("true or false"),
                "{source} → {e}"
            );
            assert!(!e.contains("message_"), "{source} → {e} names the key");
        }
    }

    /// Suggesting a session continue in another engine is off until the
    /// person switches it on.
    #[test]
    fn handing_off_is_off_until_switched_on() {
        let d = parse("[oversight]\nenabled = true\nlevel = \"act\"\n").unwrap();
        assert!(!d.act.hand_off);
        let on = parse("[oversight]\n[oversight.act]\nhand_off = true\n").unwrap();
        assert!(on.act.hand_off);
        let e = parse("[oversight]\nact = { hand_off = \"yes\" }\n").unwrap_err();
        assert!(
            e.contains("suggesting a session continue in another engine")
                && e.contains("true or false"),
            "{e}"
        );
    }

    #[test]
    fn how_it_runs_sessions_defaults_to_real_ones_four_at_a_time_only_those_handed_it() {
        let d = parse("[oversight]\nenabled = true\n").unwrap().work;
        assert!(d.prefer_sessions);
        assert_eq!((d.parallel, d.take_on), (4, TakeOn::Handed));
        assert_eq!((d.engine.as_str(), d.max_turns_per_hour), ("claude", 60));
        let inline = parse(
            "[oversight]\nwork = { prefer_sessions = false, parallel = 12, take_on = \"all\", engine = \"codex\", max_turns_per_hour = 1 }\n",
        )
        .unwrap();
        let table = parse(
            "[oversight]\n[oversight.work]\nprefer_sessions = false\nparallel = 12\ntake_on = \"all\"\nengine = \"codex\"\nmax_turns_per_hour = 1\n",
        )
        .unwrap();
        assert_eq!(inline, table);
        assert_eq!(
            inline.work,
            Work {
                prefer_sessions: false,
                parallel: 12,
                take_on: TakeOn::All,
                engine: "codex".into(),
                max_turns_per_hour: 1,
            }
        );
    }

    #[test]
    fn each_wrong_way_of_running_sessions_is_said_in_its_own_plain_words() {
        for (source, says) in [
            (
                "[oversight]\nwork = { parallel = 0 }\n",
                "How many sessions it may run at once must be a whole number from 1 to 12.",
            ),
            (
                "[oversight]\nwork = { parallel = 13 }\n",
                "How many sessions it may run at once must be a whole number from 1 to 12.",
            ),
            (
                "[oversight]\nwork = { parallel = \"4\" }\n",
                "How many sessions it may run at once must be a whole number from 1 to 12.",
            ),
            (
                "[oversight]\nwork = { take_on = \"some\" }\n",
                "Whether it takes on sessions must be \"handed\" or \"all\".",
            ),
            (
                "[oversight]\nwork = { take_on = true }\n",
                "Whether it takes on sessions must be \"handed\" or \"all\".",
            ),
            (
                "[oversight]\nwork = { prefer_sessions = \"yes\" }\n",
                "Whether preferring real sessions to helpers is on or off must be true or false.",
            ),
            (
                "[oversight]\nwork = { engine = \"Claude Code\" }\n",
                "Its default engine must be named in one word, such as claude or codex.",
            ),
            (
                "[oversight]\nwork = { engine = 3 }\n",
                "Its default engine must be named in one word, such as claude or codex.",
            ),
            (
                "[oversight]\nwork = { max_turns_per_hour = 0 }\n",
                "The most turns an hour it may spend on its sessions must be a whole number from 1 to 600.",
            ),
            (
                "[oversight]\nwork = 4\n",
                "The settings for how it runs sessions for you need to be a group of settings.",
            ),
        ] {
            let e = parse(source).unwrap_err();
            assert_eq!(e, says, "{source}");
            for key in ["parallel", "take_on", "prefer_sessions", "max_turns", "work"] {
                assert!(!e.contains(key), "{source} → {e} names {key}");
            }
        }
    }

    #[test]
    fn unknown_keys_are_kept_out_of_the_way() {
        let o =
            parse("[oversight]\nenabled = true\nmood = \"cheerful\"\n[oversight.wake]\nsoon = 1\n")
                .unwrap();
        assert!(o.enabled);
    }

    #[test]
    fn mistakes_are_said_in_plain_words_without_key_names() {
        for (source, says) in [
            (
                "[oversight]\nlevel = \"boss\"\n",
                "watch, suggest, act or full",
            ),
            ("[oversight]\nenabled = \"yes\"\n", "true or false"),
            (
                "[oversight]\nwake = { quiet_minutes = -1 }\n",
                "from 0 (never)",
            ),
            ("[oversight]\nwake = { waiting_minutes = 2000 }\n", "a day"),
            ("[oversight]\nsummaries = { morning = \"9am\" }\n", "09:00"),
            (
                "[oversight]\nsummaries = { evening = \"25:00\" }\n",
                "09:00",
            ),
            ("[oversight]\nmax_checkins_per_day = 0\n", "from 1 to 500"),
            ("[oversight]\nmax_checkins_per_hour = 61\n", "from 1 to 60"),
            (
                "[oversight]\nwake = { remind_minutes = 1441 }\n",
                "a question it already raised",
            ),
            ("[oversight]\ntrack = true\n", "group of settings"),
            ("oversight = 3\n", "group of settings"),
        ] {
            let e = parse(source).unwrap_err();
            assert!(e.contains(says), "{source} → {e}");
            for key in [
                "level",
                "enabled",
                "quiet_minutes",
                "waiting_minutes",
                "remind_minutes",
                "max_checkins",
                "track",
                "oversight",
                "[",
            ] {
                assert!(!e.contains(key), "{source} → {e} names {key}");
            }
        }
    }
}
