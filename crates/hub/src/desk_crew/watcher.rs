//! Keeping the overseers in step with their configs (their tools, bound to
//! each one's scope, their summary jobs, one overseer per scope), and
//! waking each: what happened in its scope, and nothing outside it,
//! becomes a turn in its Desk DM — something waiting on the person too
//! long, work gone quiet, work finished or failed — gathered for a minute
//! into one check-in, never about its own doing, and held to the rooms'
//! loop guard. Check-ins are spent from a rolling hour, under a ceiling for
//! the day; while they are spent what happens is held for the next one, and
//! the person is told until when. A question already raised wakes it again
//! only as a reminder, after a while.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use workforce_agent::tools::Tool as _;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::{Level, Oversight, Scope, WorkStyle};
use workforce_desk::crew::Crew as _;
use workforce_desk::crew::{
    self, Event, ItemKind, ItemState, Kind, MentionedApproval, Party, PartyKind,
};
use workforce_desk::line::{LineMessage, Wait};
use workforce_desk::model::{DeskEvent, Origin};
use workforce_desk::CheckIn;

use super::github::CHECK_GITHUB;
use super::goal::SAVE_GOALS;
use super::tools::{CrewTool, ACT, READ};
use super::{name_of, CrewHub, CHECK_IN, MAX_HOPS};
use crate::scheduler::{JobPayload, Schedule};
use crate::serve::AgentService;

/// The `desk.*` tools an overseer doesn't have: it acts through its gated
/// `crew.*` tools instead, and reads through them too, since they keep to
/// its scope and `desk.*` reads every session.
const DESK_TOOLS: [&str; 9] = crate::desk_tools::NAMES;

const MORNING_JOB: &str = "crew-summary-morning";
const EVENING_JOB: &str = "crew-summary-evening";

/// Where the open questions the overseer raised were kept before they were
/// the boss's open items: read once, moved, and gone
/// ([`CrewHub::raised_marks`]).
pub(super) const RAISED: &str = "hub.crew.raised";
/// How long a raised question is remembered.
const RAISED_FOR: chrono::TimeDelta = super::items::MARK_FOR;
/// The most check-in lines held while check-ins are spent; older go.
const MAX_HELD: usize = 30;

/// The check-ins an unbound overseer may take in an hour, and in a day: a
/// ceiling against a loop that runs away, not a limit on the work.
pub(super) const UNBOUND_CHECKINS_PER_HOUR: u32 = 240;
pub(super) const UNBOUND_CHECKINS_PER_DAY: u32 = 3000;
/// How soon, in minutes, a session waiting on a question or an approval,
/// and one gone quiet, wakes an unbound overseer, unless the person set
/// sooner.
const UNBOUND_WAITING_MINUTES: u32 = 1;
const UNBOUND_QUIET_MINUTES: u32 = 3;
/// How often, in minutes, an unbound overseer sweeps its scope while it
/// has anything in it.
const UNBOUND_SWEEP_MINUTES: u32 = 2;
/// What a check-in tried again is marked with: tried again once only.
const RETRIED: &str = "(again, as the last check-in didn't run) ";

/// What a sweep wakes an unbound overseer with.
pub(super) const SWEEP: &str = "Regular sweep: nothing may sit idle. Look over every session \
    and ticket in your scope with crew.overview and keep everything moving.";

/// `o` as an unbound overseer runs it: woken soon for what waits and what
/// goes quiet (sooner only if the person said so), for every turn's end
/// and failure, and checking in as often as the work needs, under a
/// ceiling against runaway loops.
pub(super) fn unbound_timings(o: &Oversight) -> Oversight {
    let soon = |set: u32, unbound: u32| match set {
        0 => unbound,
        n => n.min(unbound),
    };
    let mut o = o.clone();
    o.wake.waiting_minutes = soon(o.wake.waiting_minutes, UNBOUND_WAITING_MINUTES);
    o.wake.quiet_minutes = soon(o.wake.quiet_minutes, UNBOUND_QUIET_MINUTES);
    o.wake.on_finish = true;
    o.wake.on_failure = true;
    o.max_checkins_per_hour = UNBOUND_CHECKINS_PER_HOUR;
    o.max_checkins_per_day = UNBOUND_CHECKINS_PER_DAY;
    o
}

/// A session's messages from the boss that never went in, as the boss is
/// told of them.
struct Undelivered {
    trigger: Trigger,
    /// They wait behind what it asks the person, which is raised too.
    behind_person: bool,
}

/// When `item` last moved, as the keys of what is said of its stopping
/// carry it: each is said once each time it stops.
fn moved_at(item: &crew::Item) -> i64 {
    item.last_activity
        .map(|t| t.timestamp_millis())
        .unwrap_or_default()
}

fn quiet_key(item: &crew::Item) -> String {
    format!("quiet:{}:{}", item.id, moved_at(item))
}

/// One thing worth waking the overseer for.
#[derive(Debug, Clone)]
pub(super) struct Trigger {
    pub line: String,
    pub approvals: Vec<MentionedApproval>,
    pub hop: i16,
    /// The open question it is about (a session's question, an approval,
    /// something waiting on the person), the same however it is noticed.
    pub about: Option<String>,
    /// The sidebar group of what it is about: the overseer whose scope sees
    /// that is the one it wakes.
    pub group: Option<String>,
    /// The session whose turn end the fast call handed to the full turn
    /// with its refused next step (`super::idle`): said over once the
    /// check-in given it ran.
    pub handed: Option<uuid::Uuid>,
    /// Desk's own look, not something that happened: a reminder of what
    /// still waits, the regular sweep, a check-in tried again. At Act fully
    /// it never lifts a turn the person stopped.
    pub routine: bool,
}

impl Trigger {
    /// Only `line`: what happened, with nothing beside it.
    pub(super) fn line(line: String) -> Self {
        Self {
            line,
            approvals: Vec::new(),
            hop: 0,
            about: None,
            group: None,
            handed: None,
            routine: false,
        }
    }
}

/// What an Act-level check-in is asked while the composer writes the
/// boss's updates.
pub(super) const QUIET_ACT: &str =
    "Act. What you do with the crew.* tools is kept as your decisions and \
     reaches the person in your next update; give a reason each time. \
     Say nothing to the person unless something needs them, then use crew.ask_person.";

/// The boss `agent`'s own check-in channel, off its DM: a quiet check-in's
/// words stay there.
pub fn check_in_channel(agent: &str) -> String {
    format!("{DESK_CHANNEL}:checkin:{agent}")
}

/// What the overseer may spend on check-ins now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Budget {
    Open,
    /// Spent until `until`: the hour's budget, or the day's ceiling.
    Spent {
        until: DateTime<Utc>,
        daily: bool,
    },
}

/// `o`'s check-in budget at `now`, given when the check-ins since the
/// start of the day (`midnight`, the next `tomorrow`) or of the last
/// `hour`, whichever is earlier, were.
pub(super) fn budget(
    o: &Oversight,
    wakes: &[DateTime<Utc>],
    now: DateTime<Utc>,
    hour: chrono::TimeDelta,
    (midnight, tomorrow): (DateTime<Utc>, DateTime<Utc>),
) -> Budget {
    let today = wakes.iter().filter(|at| **at >= midnight).count();
    if today >= o.max_checkins_per_day as usize {
        return Budget::Spent {
            until: tomorrow,
            daily: true,
        };
    }
    let mut in_hour: Vec<DateTime<Utc>> = wakes
        .iter()
        .copied()
        .filter(|at| *at > now - hour)
        .collect();
    in_hour.sort();
    let per_hour = o.max_checkins_per_hour as usize;
    if in_hour.len() >= per_hour {
        // Free again once enough of them are an hour old.
        return Budget::Spent {
            until: in_hour[in_hour.len() - per_hour] + hour,
            daily: false,
        };
    }
    Budget::Open
}

/// Whether `t` is worth a check-in given the questions `raised` and when:
/// anything not about one is, as is one never raised; one raised before
/// only as a reminder, `remind` after it was.
pub(super) fn fresh(
    t: &mut Trigger,
    raised: &HashMap<String, DateTime<Utc>>,
    remind: Option<chrono::TimeDelta>,
    now: DateTime<Utc>,
) -> bool {
    let Some(at) = t.about.as_ref().and_then(|k| raised.get(k)) else {
        return true;
    };
    if remind.is_some_and(|r| now - *at >= r) {
        t.line = format!("Reminder, still open: {}", t.line);
        return true;
    }
    false
}

/// Adds `found` to what is `pending`: each line once, each open question
/// once (its latest), the latest [`MAX_HELD`].
pub(super) fn hold(pending: &mut Vec<Trigger>, found: Vec<Trigger>) {
    for t in found {
        pending.retain(|p| p.line != t.line && (t.about.is_none() || p.about != t.about));
        pending.push(t);
    }
    let over = pending.len().saturating_sub(MAX_HELD);
    pending.drain(..over);
}

/// The key an open question is raised by: whose, and what it asks.
fn question_key(subject: &str, kind: &str, mark: &str) -> String {
    format!("{kind}:{subject}:{}", crew::one_line(mark, 200))
}

pub(super) fn tracked(o: &Oversight, subject: &Party) -> bool {
    match subject.kind {
        PartyKind::Agent => o.track.crew,
        PartyKind::Session => match subject.origin {
            Some(Origin::External) => o.track.external_sessions,
            _ => o.track.desk_sessions,
        },
        PartyKind::Manager | PartyKind::You | PartyKind::Desk => false,
    }
}

/// `report`, the words of sessions and agents as the crew relays them,
/// fenced as data for the overseer to read: they choose their own words, so
/// nothing in them is the person's instruction.
pub(crate) fn reported(report: &str, from: &str) -> String {
    workforce_agent::security::external_content::wrap_external_content(report, from).wrapped_content
}

fn who(p: &Party) -> String {
    match (p.kind, p.engine) {
        (PartyKind::Session, Some(engine)) => format!(
            "{} ({} session {}{})",
            p.name,
            engine.title(),
            p.id,
            if p.origin == Some(Origin::External) {
                ", in another terminal"
            } else {
                ""
            }
        ),
        _ => format!("{} (agent {})", p.name, p.id),
    }
}

/// What a boss at Act fully, or unbound, is told of its plans: a draft for
/// the person unless they agreed it, started only on their word.
pub(super) const PLAN_DRAFT: &str = "A plan the person hasn't agreed is a draft for them to review and approve; start one only when their own words ask you to start the work (start_now).";

/// What Act fully aims at: the end state an order names, with no polish
/// before it. Worded once for [`STANDING_RULE`] and the decision prompts in
/// `idle` and `answer`.
macro_rules! end_state {
    () => {
        "Aim straight at the end state an order names: that is the deliverable, and everything else serves it. Never hold the deliverable behind polish their instructions don't ask for (another review round once one came back clean, an extra watch, groundwork it doesn't need): ship it, and polish beside it or after."
    };
}
pub(crate) use end_state;

/// What a boss at Act fully decides by: the person's own instructions,
/// both ways, with no stance of its own on production, and the end state
/// they ordered as the work, its independent steps at once.
pub(crate) const STANDING_RULE: &str = concat!("The person's instructions are the rule, both ways: their open orders (in your open items, in their words), their preferences, and each session's own instructions. Where they forbid a kind of step (\"don't allow anything that would impact production\"), refuse it and say what to do instead. Where they want work shipped (\"getting stuff out to production is vital as long as nothing breaks\"), do it and have the session check nothing broke. Where they say keep work going or take over, act on a session's sensible next step that serves it (have the session run it) and report \"✓ did X — why\". Touching production or staging is no reason by itself to ask. Ask the person only when their instructions don't cover the step and you truly can't judge (a destructive or irreversible step outside their orders is such a step), or when only they can physically do it (their credentials, a payment). That is rare: you handle about 99 in 100 yourself. When you must ask, answer buttons are fine, alone: no recommendation essay beside them. ", end_state!(), " Run steps that don't depend on each other at once (each in a session of its own, where you may start one), within your parallel limit.");

/// What a boss that starts sessions does with work the person asks it
/// for in its DM: starts the session itself, at once, in the folder the
/// work obviously belongs in, and answers a question itself.
pub(super) const START_THE_WORK: &str = "Your “How it works” setting is Only in sessions. Work the person asks for (a code change, a fix, an investigation in a repo, a ticket) goes to a real session: start it now with crew.start_session (crew.assign_ticket for a board ticket), never offer or suggest one and never do the work here, then say which session has it and in which folder. Pick the folder yourself: a session linked to the ticket or its PR's repo, else the folder of the person's latest session (crew.overview), else the only one; ask only when you can't tell. A question (\"what tickets do I have?\") you answer here with your tools.";

/// [`START_THE_WORK`] for a boss that prefers sessions: code changes and
/// long tasks start their session, and checks, lookups and ops it runs
/// itself.
pub(super) const START_THE_CODE_WORK: &str = "Your “How it works” setting is Prefer sessions. Code changes and long tasks the person asks for (a fix, a feature, a refactor, a ticket) go to a real session: start it now with crew.start_session (crew.assign_ticket for a board ticket), never offer or suggest one and never change code here, then say which session has it and in which folder. Pick the folder yourself: a session linked to the ticket or its PR's repo, else the folder of the person's latest session (crew.overview), else the only one; ask only when you can't tell. Checks, lookups and ops (ssh to one of their machines, gh, reading files and logs, a quick command) you run yourself with your own tools, at once, and answer from what they show; a destructive or irreversible step follows the person's directives. A question (\"what tickets do I have?\") you answer here with your tools.";

/// What a boss that does the work itself is told of sessions.
pub(super) const DO_IT_YOURSELF: &str = "Your “How it works” setting is Do it myself: do the work the person asks for yourself with your own tools, or start a real session for it (crew.start_session, crew.assign_ticket for a board ticket) when it is long or they'd want to watch it; a destructive or irreversible step follows the person's directives.";

/// What a boss is told of its own work where starting sessions isn't its
/// to do (at Suggest, or with “Hand out work” off), by its style.
pub(super) const OWN_WORK_SESSIONS_ONLY: &str = "Your “How it works” setting is Only in sessions: commands, builds and code changes go to a session (crew.start_session, which may only suggest one), never here; you read, search and keep notes yourself.";
pub(super) const OWN_WORK_PREFER: &str = "Your “How it works” setting is Prefer sessions: code changes, builds and long tasks go to a session (crew.start_session, which may only suggest one); checks, lookups and ops (ssh to one of their machines, gh, reading files and logs, a quick command) you run yourself.";

/// What every DM turn of a boss is told before it speaks of a session.
pub(super) const CHECK_FIRST: &str = "Before you say anything about a session, look at it yourself: crew.inside shows what it is doing from evidence, crew.account its account; never guess. Answer as Done / Now / Next / Waiting on, from the account, and say how fresh it is if it's over a few minutes old.";

impl CrewHub {
    /// Keeps tools, summaries and the one-overseer-per-scope rule in step
    /// with the agents' configs.
    pub(super) async fn sync(&self) {
        let mut scopes: HashMap<Scope, usize> = HashMap::new();
        for s in self.services() {
            let o = self.oversight_of(&s);
            if o.enabled {
                *scopes.entry(o.scope).or_default() += 1;
            }
        }
        let current = self.overseers();
        let mut switched = false;
        for (keep, o) in &current {
            if scopes.get(&o.scope).is_some_and(|n| *n > 1) {
                crate::agent_config_write::one_overseer(keep, self.state.def_store.as_deref())
                    .await;
                switched = true;
            }
        }
        if switched {
            self.read.lock().clear();
        }
        let current = self.overseers();
        // Act fully holds only under its budget: today's spend, read now,
        // and Desk told again what each may ship in.
        self.read_spent_today(&current).await;
        let current = self.overseers();
        let want: HashMap<String, (Scope, String)> = current
            .iter()
            .map(|(s, o)| {
                (
                    s.agent_id.as_str().to_string(),
                    (o.scope.clone(), o.level.as_str().to_string()),
                )
            })
            .collect();
        let had = self.tooled.lock().clone();
        if want != had {
            // Registered over the old ones, so a running turn never finds
            // them gone.
            let mut tools: HashMap<String, Vec<CrewTool>> = current
                .iter()
                .map(|(s, o)| {
                    let id = s.agent_id.as_str();
                    let at = CrewTool::for_level(&self.me(), id, &o.scope, o.level == Level::Watch);
                    (id.to_string(), at)
                })
                .collect();
            for s in self.services() {
                let kept = tools.get(s.agent_id.as_str());
                let keeps = |name: &str| kept.is_some_and(|t| t.iter().any(|t| t.name() == name));
                for name in READ.iter().chain(ACT.iter()).filter(|n| !keeps(n)) {
                    s.tools.unregister(name);
                }
                // `desk.*` only on agents that oversee nothing.
                match kept.is_some() {
                    true => crate::connector_pins::pin(&s),
                    false => crate::desk_driver::register_tools(&s),
                }
            }
            for (s, o) in &current {
                for name in DESK_TOOLS {
                    s.tools.unregister(name);
                }
                let id = s.agent_id.as_str();
                for tool in tools.remove(id).unwrap_or_default() {
                    s.tools.register(Arc::new(tool));
                }
                // An overseer has Desk's browser.
                crate::desk_browser::equip(s);
                tracing::info!(agent_id = %s.agent_id, group = o.scope.group().unwrap_or("ungrouped"), level = o.level.as_str(), "Oversees the crew");
            }
            *self.tooled.lock() = want;
        }
        for s in self.services() {
            let o = current
                .iter()
                .find(|(c, _)| c.agent_id == s.agent_id)
                .map(|(_, o)| o.clone());
            self.sync_summaries(&s, o.as_ref()).await;
            // An overseer recalls only its own memories while groups have
            // bosses of their own.
            let now = (*s.resolved.snapshot()).clone();
            let kept = super::recall_kept_apart(&s, now.clone());
            if kept.memory.agent_recall_scope != now.memory.agent_recall_scope {
                s.resolved.store(kept);
            }
        }
    }

    async fn sync_summaries(&self, service: &AgentService, o: Option<&Oversight>) {
        if let Some(scheduler) = service.scheduler.as_ref() {
            sync_summaries(scheduler.as_ref(), o).await;
        }
    }
}

/// Registers the summary jobs `o` asks for on `scheduler`, and removes the
/// ones it doesn't (all of them when the agent doesn't oversee).
pub(super) async fn sync_summaries(
    scheduler: &dyn crate::hosts::SchedulerHost,
    o: Option<&Oversight>,
) {
    let wanted = [
        (MORNING_JOB, o.and_then(|o| o.summaries.morning), false),
        (EVENING_JOB, o.and_then(|o| o.summaries.evening), true),
    ];
    let jobs = scheduler.list_jobs().await;
    for (name, at, evening) in wanted {
        match at {
            Some((h, m)) => {
                let schedule = Schedule::Cron {
                    expr: format!("{m} {h} * * *"),
                    tz: None,
                };
                let same = jobs
                    .iter()
                    .any(|j| j.name == name && j.schedule == schedule && j.enabled);
                if !same {
                    if let Err(e) = scheduler
                        .upsert_job(
                            name,
                            Some("The overseer's summary of the crew, in its Desk DM"),
                            schedule,
                            JobPayload::CrewSummary { evening },
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "registering a crew summary failed");
                    }
                }
            }
            None => {
                for job in jobs.iter().filter(|j| j.name == name) {
                    scheduler.remove_job(&job.id).await;
                }
            }
        }
    }
}

impl CrewHub {
    /// What `event` wakes the overseer for, if anything.
    pub(super) fn trigger_for(
        &self,
        event: &Event,
        overseer: &str,
        o: &Oversight,
    ) -> Option<Trigger> {
        // A restart that stopped work a session had running (its CI
        // watchers, its sub-agents), whoever did it but this boss: the boss
        // sees it re-checks that work.
        let stopped = matches!(event.kind, Kind::Restarted | Kind::Resumed)
            && event
                .detail
                .as_ref()
                .and_then(|d| d.get("stopped"))
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|n| n > 0);
        let quiet = match event.actor.kind {
            PartyKind::Manager => !stopped || event.actor.id == overseer,
            PartyKind::You => !stopped,
            _ => false,
        };
        if quiet || event.hop >= MAX_HOPS {
            return None;
        }
        let subject = event.subject.as_ref()?;
        if subject.id == overseer || !tracked(o, subject) {
            return None;
        }
        // A session the person took back wakes no boss: it sees it, and
        // leaves it alone.
        if subject.kind == PartyKind::Session
            && subject.id.parse().is_ok_and(|s| self.taken_back(s))
        {
            return None;
        }
        // A session it runs wakes it on its own line when its turn ends
        // (`work_line`); one that asks the person wakes it here as before,
        // and so does every turn's end while it only watches.
        let runs = || subject.id.parse().is_ok_and(|s| self.runs(s, overseer));
        if event.kind == Kind::Finished && o.level != Level::Watch && runs() {
            return None;
        }
        let unbound = self.unbound(o);
        let view = || subject.id.parse().ok().and_then(|s| self.desk.view(s).ok());
        // A plain turn's end its Act fully boss judges at once (`idle`)
        // wakes nothing else.
        if event.kind == Kind::Finished && view().is_some_and(|v| self.moves_on_itself(&v)) {
            return None;
        }
        // A question the person is in control of is theirs: nothing is said
        // of it (`answer`). One its Act fully boss answers itself wakes
        // nothing else.
        if event.kind == Kind::Question
            && view().is_some_and(|v| {
                super::answer::question_of(&v).is_some()
                    && (self.person_has_questions(&v, overseer) || self.answers_question_itself(&v))
            })
        {
            return None;
        }
        let wanted = match event.kind {
            _ if stopped => runs() || o.wake.on_failure || unbound,
            Kind::Finished | Kind::Question => o.wake.on_finish || unbound,
            Kind::Failed | Kind::Limit => o.wake.on_failure || unbound,
            // Unbound, what waits on an answer wakes it at once, unless it
            // is a request its decision call has (`decide`).
            Kind::NeedsYou => {
                unbound
                    && match event.approval_id.as_deref() {
                        // A request: only while it still waits and isn't
                        // one its decision call has (`decide`), which has
                        // often answered it by now.
                        Some(a) => subject
                            .id
                            .parse()
                            .ok()
                            .and_then(|s| self.desk.view(s).ok())
                            .is_some_and(|v| {
                                v.pending.as_ref().is_some_and(|p| p.id.to_string() == a)
                                    && !self.answers_at_once(&v)
                                    && !self.dialog_is_persons(&v, overseer)
                            }),
                        None => true,
                    }
            }
            _ => false,
        };
        let about = match event.kind {
            Kind::NeedsYou => Some(question_key(&subject.id, "needs", &event.text)),
            _ => None,
        };
        let about = about.or_else(|| {
            (event.kind == Kind::Question).then(|| {
                let asked = event
                    .detail
                    .as_ref()
                    .and_then(|d| d.pointer("/question/text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&event.text);
                question_key(&subject.id, "asked", asked)
            })
        });
        let line = match stopped {
            true => format!(
                "{} {}. Make sure it restarts what it still needs of that work, and that what \
                 waited on it (CI, a review, an answer) still happens; it was told to check.",
                match event.actor.kind {
                    // Said to the boss, "You" would be it.
                    PartyKind::You => "The person",
                    _ => event.actor.name.as_str(),
                },
                event.text
            ),
            false => format!("{} {}", who(subject), event.text),
        };
        wanted.then(|| Trigger {
            line,
            approvals: Vec::new(),
            hop: event.hop,
            about,
            group: event.group.clone(),
            handed: None,
            routine: false,
        })
    }

    /// Whether what `item` waits on is not raised to `overseer` here: a
    /// question of a session the person is in control of, or its Act fully
    /// boss answers itself (`answer`), and a request its decision call has
    /// (`decide`).
    pub(crate) fn raised_elsewhere(&self, item: &crew::Item, overseer: &str) -> bool {
        item.kind == ItemKind::Session
            && item
                .id
                .parse()
                .ok()
                .and_then(|s| self.desk.view(s).ok())
                .is_some_and(|v| match (&item.approval, &item.question) {
                    // Only while its call may still be deciding it: one it
                    // left to the person, or Desk refused, is raised.
                    // Answered since the overview was read (often by the
                    // decision call itself): it no longer waits.
                    (Some(a), _) if v.pending.as_ref().is_none_or(|p| p.id.to_string() != a.id) => {
                        true
                    }
                    (Some(_), _) => {
                        (self.answers_at_once(&v)
                            && v.pending
                                .as_ref()
                                .is_some_and(|p| Utc::now() - p.since < super::decide::DECIDING))
                            || self.dialog_is_persons(&v, overseer)
                    }
                    (None, Some(_)) => {
                        super::answer::question_of(&v).is_some()
                            && (self.person_has_questions(&v, overseer)
                                || self.answers_question_itself(&v))
                    }
                    (None, None) => false,
                })
    }

    /// What has waited on the person, or gone quiet, past the overseer's
    /// thresholds. What waits is said each time, about its open question
    /// ([`fresh`] says whether it is worth raising); work gone quiet once
    /// (keys already in `noted` are left out, and `noted` keeps only what
    /// still holds).
    pub(super) async fn thresholds(
        &self,
        overseer: &str,
        o: &Oversight,
        noted: &mut HashSet<String>,
    ) -> Vec<Trigger> {
        let fast;
        let o = if self.unbound(o) {
            fast = unbound_timings(o);
            &fast
        } else {
            o
        };
        // What runs on its machines is looked inside of, for the next look
        // to read.
        self.glance_group_machines(overseer, o);
        let seen = self.desk.seen_by(&o.scope);
        // Before the overview is read: how long each item has been quiet is
        // judged from when its activity was, not after the read.
        let now = Utc::now();
        let Ok(overview) = self.desk.overview_for(self, None, Some(&seen)).await else {
            return Vec::new();
        };
        let tz = self.desk.timezone();
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let past = |at: Option<chrono::DateTime<Utc>>, minutes: u32| {
            minutes > 0 && at.is_some_and(|at| now - at >= minute * minutes as i32)
        };
        let mut holding = HashSet::new();
        let mut out = Vec::new();
        let waiting = self.waiting_from(overseer).await;
        let away = self.machine_items(overseer);
        for item in overview.items.iter().chain(&away) {
            if item.id == overseer {
                continue;
            }
            let party = Party {
                kind: match item.kind {
                    ItemKind::Agent => PartyKind::Agent,
                    ItemKind::Session => PartyKind::Session,
                },
                id: item.id.clone(),
                name: item.name.clone(),
                engine: item.engine,
                origin: item.origin,
            };
            if !tracked(o, &party) {
                continue;
            }
            // A session the person took back is left alone.
            if item.kind == ItemKind::Session && item.id.parse().is_ok_and(|s| self.taken_back(s)) {
                continue;
            }
            // What Desk sees inside it, from what it already read.
            let inside = match item.kind {
                ItemKind::Session => self.seen_inside(&item.id, o),
                ItemKind::Agent => None,
            };
            let evidence = inside.as_ref().and_then(|(g, s)| {
                s.as_ref()
                    .filter(|s| super::inside::moves(s.kind))
                    .map(|s| {
                        (
                            super::inside::evidence_key(&item.id, g, s),
                            super::inside::evidence_line(&who(&party), g, s),
                        )
                    })
            });
            // Said once a stall, instead of its quiet from then on.
            let waits = waiting.get(&item.id).map(Vec::as_slice).unwrap_or_default();
            if let Some(oldest) = waits.first() {
                let key = format!("undelivered:{}:{}:{}", item.id, oldest.id, moved_at(item));
                holding.insert(key.clone());
                holding.insert(quiet_key(item));
                holding.extend(evidence.as_ref().map(|(k, _)| k.clone()));
                if let Some(found) =
                    self.undelivered(overseer, item, &party, waits, now, o.work.stall_minutes)
                {
                    if noted.insert(key) {
                        noted.insert(quiet_key(item));
                        noted.extend(evidence.as_ref().map(|(k, _)| k.clone()));
                        out.push(found.trigger);
                    }
                    if !found.behind_person {
                        continue;
                    }
                }
            }
            if self.raised_elsewhere(item, overseer) {
                continue;
            }
            let (key, trigger) = match (item.state, &evidence) {
                (ItemState::NeedsYou, _) if past(item.since, o.wake.waiting_minutes) => {
                    let waited = item
                        .since
                        .map(|s| (now - s).num_minutes().max(1))
                        .unwrap_or(1);
                    let (asks, about, approvals) = match (&item.approval, &item.question) {
                        (Some(a), _) => (
                            format!("{} (approval {})", a.summary, a.id),
                            question_key(&item.id, "approval", &a.id),
                            vec![MentionedApproval {
                                session_id: item.id.clone(),
                                approval_id: a.id.clone(),
                                summary: a.summary.clone(),
                            }],
                        ),
                        (None, Some(q)) => (
                            format!("asks “{}”", q.text),
                            question_key(&item.id, "asked", &q.text),
                            vec![],
                        ),
                        (None, None) => (
                            "is waiting on you".into(),
                            question_key(&item.id, "waiting", ""),
                            vec![],
                        ),
                    };
                    (
                        about.clone(),
                        Trigger {
                            line: format!(
                                "{} has waited on you for {waited} min: {asks}",
                                who(&party)
                            ),
                            approvals,
                            hop: 0,
                            about: Some(about),
                            group: item.group_id.clone(),
                            handed: None,
                            routine: false,
                        },
                    )
                }
                (ItemState::Working | ItemState::Quiet, Some((key, line))) => {
                    holding.insert(quiet_key(item));
                    noted.insert(quiet_key(item));
                    (
                        key.clone(),
                        Trigger {
                            line: line.clone(),
                            approvals: vec![],
                            hop: 0,
                            about: None,
                            group: item.group_id.clone(),
                            handed: None,
                            routine: false,
                        },
                    )
                }
                (ItemState::Working | ItemState::Quiet, None)
                    if past(item.last_activity, o.wake.quiet_minutes) =>
                {
                    let quiet = item
                        .last_activity
                        .map(|s| (now - s).num_minutes().max(1))
                        .unwrap_or(1);
                    (
                        quiet_key(item),
                        Trigger {
                            line: format!(
                                "{} has been quiet for {quiet} min while working on {}{}{}",
                                who(&party),
                                item.task.as_deref().unwrap_or("its task"),
                                item.detail
                                    .as_deref()
                                    .map(|d| format!(" (last: {d})"))
                                    .unwrap_or_default(),
                                inside
                                    .as_ref()
                                    .filter(|(g, _)| g.as_of.is_some())
                                    .map(|(g, _)| format!(" — what Desk sees: {}", g.line(tz, now)))
                                    .unwrap_or_default()
                            ),
                            approvals: vec![],
                            hop: 0,
                            about: None,
                            group: item.group_id.clone(),
                            handed: None,
                            routine: false,
                        },
                    )
                }
                _ => continue,
            };
            holding.insert(key.clone());
            if trigger.about.is_some() || noted.insert(key) {
                out.push(trigger);
            }
        }
        noted.retain(|k| holding.contains(k));
        out
    }

    /// `overseer`'s own messages still waiting to go into each session, by
    /// session id, oldest first.
    async fn waiting_from(&self, overseer: &str) -> HashMap<String, Vec<LineMessage>> {
        match self.desk.store().line_waiting().await {
            Ok(rows) => rows
                .into_iter()
                .filter(|m| m.from.kind == PartyKind::Manager && m.from.id == overseer)
                .fold(HashMap::new(), |mut by, m| {
                    by.entry(m.to.id.clone()).or_default().push(m);
                    by
                }),
            Err(e) => {
                tracing::warn!(error = %e, "the boss's waiting messages couldn't be read");
                HashMap::new()
            }
        }
    }

    /// The trigger for a session `waiting`, `overseer`'s messages to it
    /// (oldest first), haven't reached for `stall_minutes` while it showed
    /// no progress of its own: none while Desk holds any of them on purpose,
    /// or once it isn't running (stopped, closed, or its process lost: Desk
    /// offers the person to run it again).
    fn undelivered(
        &self,
        overseer: &str,
        item: &crew::Item,
        party: &Party,
        waiting: &[LineMessage],
        now: DateTime<Utc>,
        stall_minutes: u32,
    ) -> Option<Undelivered> {
        let stall = self.crew_minutes(i64::from(stall_minutes));
        let oldest = waiting.first()?;
        let quiet = item.last_activity.is_none_or(|at| now - at >= stall);
        if now - oldest.at < stall || !quiet {
            return None;
        }
        let view = self.desk.view(item.id.parse().ok()?).ok()?;
        if !view.state.is_running() || self.person_has_questions(&view, overseer) {
            return None;
        }
        let waits = self.desk.rows_wait(&view, waiting)?;
        const LONG_STEP: &str =
            "it may be stuck, or in one long step (a build, a test run), which a restart would cut off";
        let first = match view.pending {
            Some(_) => Some(Wait::Dialog),
            None => waits.first().copied().flatten(),
        };
        let (why, behind_person) = match first {
            Some(Wait::Dialog) => (
                "it waits on an approval or a dialog in its terminal, and your messages wait behind that"
                    .to_string(),
                true,
            ),
            Some(Wait::Asking) => (
                "it ended its turn asking the person something, and your messages wait behind that"
                    .to_string(),
                true,
            ),
            Some(Wait::Stopped) => ("Desk can't reach it to put them in".to_string(), false),
            Some(Wait::Working) => (format!("its turn hasn't ended: {LONG_STEP}"), false),
            Some(Wait::NextCall) => (
                format!("it was told to read it with its next tool call, and hasn't made one: {LONG_STEP}"),
                false,
            ),
            Some(Wait::Behind) => (
                "an earlier message was handed to it and was never said to have gone in, and the rest wait behind it"
                    .to_string(),
                false,
            ),
            None => ("it was free to take it, and nothing took it in".to_string(), false),
            Some(
                Wait::Typing | Wait::Paused | Wait::PersonDriving | Wait::ModeEnding | Wait::Awaited,
            ) => {
                return None
            }
        };
        let move_it = match behind_person {
            true => {
                "Answer what it waits on if that is yours to (crew.history shows it); it stays \
                 raised for the person meanwhile."
            }
            false => {
                "First read where it stands (crew.history), then move it now: answer what it waits \
                 on, restart it (crew.restart) only if its history shows it stuck, start a fresh \
                 session on the work (crew.start_session), or do the step yourself; if a restart \
                 of it already failed, don't try another. Tell the person only if none of those \
                 can work."
            }
        };
        let at = oldest
            .at
            .with_timezone(&self.desk.timezone())
            .format("%H:%M");
        let minutes = (now - oldest.at).num_minutes().max(1);
        let line = format!(
            "{} hasn't taken your message of {at} in {minutes} min ({} waiting, the oldest “{}”) \
             and has shown no progress meanwhile: {why}. {move_it}",
            who(party),
            waiting.len(),
            crew::words(&oldest.text, 20),
        );
        Some(Undelivered {
            trigger: Trigger {
                line,
                approvals: vec![],
                hop: 0,
                about: None,
                group: item.group_id.clone(),
                handed: None,
                routine: false,
            },
            behind_person,
        })
    }

    /// What an unbound overseer is told each time it wakes: be the boss.
    pub(super) fn boss_hint(o: &Oversight) -> String {
        format!(
            "You are unbound: no limits but the person's budget, their Stop, and their hold while \
             they type. Be the boss, keep everything moving, spawn sessions for open work up to the \
             parallel limit ({}). For every session in your scope (crew.overview): waiting on a \
             question or an approval, answer or approve it; finished with an obvious next step from \
             its goal, plan or ticket, nudge it on; stuck or errored, resume or restart it; and \
             while an approved plan's next tickets are free and fewer than {} sessions are working, start \
             sessions for them (crew.start_session or crew.assign_ticket). Between turns, set a \
             session's mode with crew.set_mode: auto before routine work, default before work that \
             will need your judgement. Nothing should sit idle. Check the person's preferences first and apply \
             the one that fits (a close match too: apply it and say so). With none, decide yourself; give the \
             person a question only when you truly can't, with crew.ask_person and generalizes, so their \
             answer settles that kind of case from then on. Give a reason each time; the person reads them in Activity. Your own limits stay theirs: asked to change them, say plainly you can't and post crew.ask_oversight_change. {STANDING_RULE} {PLAN_DRAFT} {SAVE_GOALS} {CHECK_GITHUB}",
            o.work.parallel, o.work.parallel
        )
    }

    /// Whether the overseer `overseer`'s scope has a session or an agent
    /// in it but itself.
    pub(super) async fn has_work(&self, overseer: &str, o: &Oversight) -> bool {
        let seen = self.desk.seen_by(&o.scope);
        self.desk
            .overview_for(self, None, Some(&seen))
            .await
            .is_ok_and(|v| v.items.iter().any(|i| i.id != overseer))
    }

    /// What the overseer is told it may do at `level`, under `o`'s
    /// switches: at Act and Act fully, also which of them are off, so what
    /// becomes a suggestion is said with the switch that would let it act.
    pub(super) fn level_hint(level: Level, o: &Oversight) -> String {
        let hint = match level {
            Level::Full => format!(
                "{} {STANDING_RULE} {PLAN_DRAFT} {SAVE_GOALS} {CHECK_GITHUB}",
                Self::level_words(level)
            ),
            _ => format!("{} {CHECK_GITHUB}", Self::level_words(level)),
        };
        let off = o.act.off();
        if !level.acts() || off.is_empty() {
            return hint;
        }
        let named: Vec<String> = off.iter().map(|label| format!("“{label}”")).collect();
        format!(
            "{hint} Off: {}; for those, say which to turn on.",
            named.join(", ")
        )
    }

    /// The short line a turn the person asks the overseer in its Desk DM
    /// carries: its level now and, where it acts, the switches that are off,
    /// so it acts within them from the first request.
    pub(super) fn dm_hint(level: Level, o: &Oversight, unbound: bool) -> String {
        let says = match level {
            _ if unbound => {
                let work = Self::style_line(o);
                return format!("You oversee the crew, unbound: do everything yourself with the crew.* tools. {work} {STANDING_RULE} {PLAN_DRAFT} {SAVE_GOALS} {CHECK_FIRST} {CHECK_GITHUB}");
            }
            Level::Watch => "at Watch: don't act; say what you would do.",
            Level::Suggest => "at Suggest: your crew.* actions become suggestions the person accepts.",
            Level::Act => "at Act: do the work yourself with the crew.* tools, don't offer it.",
            Level::Full => "at Act fully: do the work yourself with the crew.* tools, and ship in the repos the person named.",
        };
        // At Act and above, work the person asks for starts its session
        // here and now, wherever starting one is the boss's to do.
        // Below that, it still knows what of its own work it does here.
        let says = match (level, level.acts() && o.act.hand_out_work) {
            (_, true) => format!("{says} {}", Self::style_line(o)),
            (Level::Watch, false) => says.to_string(),
            (_, false) => format!("{says} {}", Self::own_work_line(o)),
        };
        let says = match level {
            Level::Full => format!("{says} {STANDING_RULE}"),
            _ => says,
        };
        let off = o.act.off();
        let off = match level.acts() && !off.is_empty() {
            true => format!(
                " Off: {}; for those, say which to turn on.",
                off.iter()
                    .map(|label| format!("“{label}”"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            false => String::new(),
        };
        format!("You oversee the crew {says}{off} {CHECK_FIRST} {CHECK_GITHUB}")
    }

    /// What a boss that starts sessions does with the work the person asks
    /// for, as its “How it works” setting says.
    pub(super) fn style_line(o: &Oversight) -> &'static str {
        match o.work.style {
            WorkStyle::SessionsOnly => START_THE_WORK,
            WorkStyle::PreferSessions => START_THE_CODE_WORK,
            WorkStyle::Direct => DO_IT_YOURSELF,
        }
    }

    /// What of its own work a boss does here, where it doesn't start the
    /// sessions itself.
    pub(super) fn own_work_line(o: &Oversight) -> &'static str {
        match o.work.style {
            WorkStyle::SessionsOnly => OWN_WORK_SESSIONS_ONLY,
            WorkStyle::PreferSessions => OWN_WORK_PREFER,
            WorkStyle::Direct => DO_IT_YOURSELF,
        }
    }

    fn level_words(level: Level) -> &'static str {
        match level {
            Level::Watch => "You only watch: don't act; say what you would do if it helps.",
            Level::Suggest => "Use the crew.* tools to propose what to do; each becomes a suggestion the person can do or dismiss right beside your message. Give a reason each time.",
            Level::Act => "You act: do the work yourself with the crew.* tools (hand work to a session already there before starting one, nudge, answer) within the person's switches; the rest becomes a suggestion. Give a reason each time.",
            Level::Full => "You act fully: as at Act, and in the repositories the person named you may also approve and answer merging a pull request by its number, pushing the default branch, creating and pushing a tag, creating a GitHub release, and running the workflows they listed (that is how deploys and promotions run; deploy tools and release scripts stay theirs). Never anything that reads secrets, publishes to a package registry, force-pushes, deletes on the remote or outside the session's folder, spends money, or changes your own limits: those stay the person's (asked to change them, say plainly you can't and post crew.ask_oversight_change for them to approve). Give a reason each time.",
        }
    }

    /// Wakes the overseer `service` with `triggers`, as one check-in: each
    /// in its scope, as it was routed.
    /// Whether its turn ran.
    pub(super) async fn wake(
        &self,
        service: Arc<AgentService>,
        o: Oversight,
        triggers: Vec<Trigger>,
    ) -> bool {
        let Some(owner) = self.person(Some(&service)) else {
            tracing::info!("the overseer has no Desk conversation to check in on yet");
            return false;
        };
        // At Act fully what it found is a notice in its own running
        // conversation (`super::orchestrator`), acted on there at once.
        if self.orchestrates(service.agent_id.as_str()) {
            if let Some(crew) = self.me.upgrade() {
                for t in triggers {
                    crew.notify(
                        service.agent_id.as_str(),
                        &owner,
                        super::orchestrator::Notice::of(t),
                    );
                }
                return true;
            }
        }
        let lines: Vec<String> = triggers.iter().map(|t| format!("- {}", t.line)).collect();
        let (level, why) = self.level_now(service.agent_id.as_str(), &o);
        let hint = match self.unbound_given(service.agent_id.as_str(), &o) {
            true => Self::boss_hint(&o),
            false => Self::level_hint(level, &o),
        };
        // Where it acts and its updates are composed, what it did reaches
        // the person through the next update: it says nothing itself.
        let composed = level.acts() && o.updates.enabled;
        let ask = match composed {
            true => QUIET_ACT,
            false => "Tell the person briefly, in plain words, what needs them and what you did or suggest — what needs \
                      them first. Name sessions and agents as above.",
        };
        // At Watch and Suggest its message is also the boss's update: one
        // post, not two.
        let update = match !level.acts() && o.updates.enabled {
            true => self.update_for_wake(&service, &o).await,
            false => None,
        };
        let prompt = format!(
            "{CHECK_IN} Here is what happened on the crew since you last looked (you oversee it at level {}):\n{}\n\n\
             {ask} {}{}{}",
            level.as_str(),
            reported(&lines.join("\n"), "the crew's sessions and agents"),
            hint,
            why.map(|w| format!(" {w} Tell the person once if it matters.")).unwrap_or_default(),
            update
                .as_ref()
                .map(|(_, handing, _)| format!("\n\n{handing}"))
                .unwrap_or_default(),
        );
        let hop = triggers.iter().map(|t| t.hop).max().unwrap_or(0);
        let approvals: Vec<MentionedApproval> =
            triggers.into_iter().flat_map(|t| t.approvals).collect();
        // Off the DM, so its own words aren't posted, unless it names
        // requests the person sees beside its message.
        let quiet = composed && approvals.is_empty();
        let started = Utc::now();
        let reply = self
            .check_in_on(
                &service,
                &owner,
                "wake",
                &prompt,
                hop,
                Some(approvals),
                quiet,
            )
            .await;
        if let (Some(reply), Some((p, _, _))) = (&reply, &update) {
            self.update_answered(&service, p, reply).await;
        }
        // What it suggested where no message carries it goes with its
        // next update.
        if quiet && reply.is_some() {
            self.keep_quiet_suggestions(service.agent_id.as_str(), started)
                .await;
        }
        reply.is_some()
    }

    /// Puts what `service`'s own scheduled check-in (its heartbeat) found
    /// into its Desk DM with the person, as a check-in of its own: the
    /// agent tells them in its words. Whether it was delivered.
    pub(crate) async fn heartbeat_found(&self, service: &Arc<AgentService>, found: &str) -> bool {
        let Some(owner) = self.person(Some(service)) else {
            tracing::info!(agent = %service.agent_id, "a check-in found something, but there is no Desk conversation to put it in yet");
            return false;
        };
        let prompt = format!(
            "{CHECK_IN} Your own scheduled check-in, going through your check-in list (HEARTBEAT.md), found \
             something that needs the person:\n\n{}\n\nTell them briefly, in plain words, what needs them \
             and what you suggest.",
            reported(found, "your scheduled check-in")
        );
        self.check_in(service, &owner, "heartbeat", &prompt, 0, None)
            .await
            .is_some()
    }

    /// Runs one unprompted turn of the overseer's and keeps it as a
    /// check-in. Its answer, if it gave one.
    async fn check_in(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        kind: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
    ) -> Option<String> {
        self.check_in_on(service, owner, kind, prompt, hop, listed, false)
            .await
    }

    /// [`Self::check_in`], `quiet`: on the boss's own check-in channel off
    /// its DM, so its reply isn't posted (what it asks with
    /// crew.ask_person still is).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn check_in_on(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        kind: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
        quiet: bool,
    ) -> Option<String> {
        let record = CheckIn {
            id: crew::new_id(),
            at: crew::now(),
            agent_id: service.agent_id.as_str().to_string(),
            kind: kind.to_string(),
            prompt: prompt.to_string(),
            reply: None,
        };
        if let Err(e) = self.desk.store().put_check_in(&record).await {
            tracing::warn!(error = %e, "recording a check-in failed");
        }
        let id = service.agent_id.as_str().to_string();
        // Under its mind ([`super::mind`]); the wake loop's check-ins hold
        // it already. Begun by itself, it yields to the person's own turn.
        self.yield_to_person(&id).await;
        self.with_mind(
            &id,
            "a check-in",
            self.check_in_held(service, owner, prompt, hop, listed, quiet, record),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_in_held(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
        quiet: bool,
        mut record: CheckIn,
    ) -> Option<String> {
        let id = service.agent_id.as_str().to_string();
        self.wake_hop.lock().insert(id.clone(), hop);
        let ran = match quiet {
            true => {
                self.run_turn(
                    service,
                    owner,
                    &check_in_channel(&id),
                    prompt,
                    hop,
                    listed,
                    None,
                )
                .await
            }
            false => {
                self.run_dm_turn(service, owner, prompt, hop, listed, None)
                    .await
            }
        };
        self.wake_hop.lock().remove(&id);
        match ran {
            Ok((_, reply)) => {
                record.reply = Some(reply.clone());
                if let Err(e) = self.desk.store().put_check_in(&record).await {
                    tracing::warn!(error = %e, "recording a check-in's answer failed");
                }
                Some(reply)
            }
            Err(e) => {
                tracing::warn!(error = %e, "the overseer's check-in did not run");
                None
            }
        }
    }

    /// The overseer's morning or evening summary, in its DM.
    pub(crate) async fn summary(
        &self,
        service: &Arc<AgentService>,
        evening: bool,
    ) -> Result<String, String> {
        let (_, o) = self
            .overseer_named(service.agent_id.as_str())
            .ok_or("this agent no longer oversees the crew")?;
        let seen = self.desk.seen_by(&o.scope);
        let owner = self
            .person(Some(service))
            .ok_or("no one has a Desk conversation with the overseer yet")?;
        let which = if evening { "evening" } else { "morning" };
        let since = if evening {
            self.midnight()
        } else {
            Utc::now() - chrono::Duration::hours(16)
        };
        let events = self
            .desk
            .store()
            .events_since(since, &[], 150, Some(&seen))
            .await
            .map_err(|e| e.to_string())?;
        let lines: Vec<String> = events
            .iter()
            .map(|e| format!("- {} {} {}", e.at.format("%H:%M"), e.actor.name, e.text))
            .collect();
        let prompt = format!(
            "{CHECK_IN} Write the person's {which} summary of the crew. What happened {}:\n{}\n\n\
             Use crew.overview for where everyone is now. Start with one short paragraph that says what \
             matters most (it is shown on its own as the brief), then anything else worth knowing. {}",
            if evening { "today" } else { "since yesterday evening" },
            if lines.is_empty() {
                "- nothing recorded".to_string()
            } else {
                reported(&lines.join("\n"), "the crew's activity")
            },
            Self::level_hint(self.level_now(service.agent_id.as_str(), &o).0, &o),
        );
        let reply = self
            .check_in(service, &owner, which, &prompt, 0, None)
            .await
            .ok_or("the summary turn did not run")?;
        let me = Party::manager(service.agent_id.as_str(), &name_of(service));
        self.desk.note(Event::new(
            Kind::Summary,
            me,
            None,
            format!("sent your {which} summary"),
        ));
        Ok(reply)
    }

    /// An hour, as this crew's minutes run.
    fn hour(&self) -> chrono::TimeDelta {
        chrono::Duration::from_std(self.timing.minute * 60).unwrap_or(chrono::TimeDelta::hours(1))
    }

    /// The person's time zone.
    fn tz(&self) -> chrono_tz::Tz {
        self.services()
            .first()
            .map_or(chrono_tz::UTC, |s| s.config.user_timezone())
    }

    /// The start of the next day, in the person's time zone.
    fn tomorrow(&self) -> DateTime<Utc> {
        let tz = self.tz();
        Utc::now()
            .with_timezone(&tz)
            .date_naive()
            .succ_opt()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .and_then(|m| m.and_local_timezone(tz).earliest())
            .map_or_else(
                || Utc::now() + chrono::TimeDelta::hours(24),
                |m| m.with_timezone(&Utc),
            )
    }

    /// What the overseer `agent` may spend on check-ins now: each
    /// overseer's are its own.
    pub(super) async fn budget(&self, agent: &str, o: &Oversight) -> eyre::Result<Budget> {
        let fast;
        let o = if self.unbound(o) {
            fast = unbound_timings(o);
            &fast
        } else {
            o
        };
        let now = Utc::now();
        let midnight = self.midnight();
        let wakes = self
            .desk
            .store()
            .wakes_since(agent, midnight.min(now - self.hour()))
            .await?;
        Ok(budget(
            o,
            &wakes,
            now,
            self.hour(),
            (midnight, self.tomorrow()),
        ))
    }

    /// Tells the person, where they look, that the overseer holds its
    /// check-ins until `until`, and why.
    pub(super) fn paused(&self, service: &AgentService, until: DateTime<Utc>, daily: bool) {
        let why = if daily { "daily" } else { "hourly" };
        let text = format!(
            "paused check-ins until {}: {why} limit reached",
            until.with_timezone(&self.tz()).format("%H:%M")
        );
        tracing::warn!(agent = %service.agent_id, %until, "the overseer {text}");
        let me = Party::manager(service.agent_id.as_str(), &name_of(service));
        self.desk.note(Event::new(Kind::Quiet, me, None, text));
    }
}

/// The session whose turn `event` says ended, when it does.
pub(super) fn finished_session(event: &Event) -> Option<uuid::Uuid> {
    let subject = event.subject.as_ref()?;
    (event.kind == Kind::Finished && subject.kind == PartyKind::Session)
        .then(|| subject.id.parse().ok())
        .flatten()
}

/// Runs [`CrewHub::sync`] every tick, and at once when a config is saved.
pub(super) async fn sync_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    loop {
        crew.sync().await;
        tokio::select! {
            () = cancel.cancelled() => return,
            () = crew.poke.notified() => {}
            () = tokio::time::sleep(crew.timing.tick) => {}
        }
    }
}

/// One overseer's check-ins as they gather: what waits for the next, since
/// when, what of its scope it has noted, the one firing, and until when its
/// check-ins are spent (and the pause the person was told of).
#[derive(Default)]
struct Waking {
    pending: Vec<Trigger>,
    window: Option<Instant>,
    noted: HashSet<String>,
    firing: Option<tokio::task::JoinHandle<()>>,
    held_until: Option<DateTime<Utc>>,
    told: Option<DateTime<Utc>>,
    /// When it, unbound, last swept its scope.
    swept: Option<Instant>,
}

/// Turns what happens into the overseers' check-ins: each event, and each
/// threshold passed, goes to the overseer whose scope sees its group, and
/// no other; each overseer's check-ins are gathered and spent apart.
pub(super) fn wake_loop(
    crew: Arc<CrewHub>,
    cancel: CancellationToken,
) -> impl std::future::Future<Output = ()> {
    let mut events = crew.desk.events();
    let mut rework = crew.rework.subscribe();
    async move {
        let mut tick = tokio::time::interval(crew.timing.tick.min(crew.timing.coalesce));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut waking: HashMap<String, Waking> = HashMap::new();
        // Kept on the boss's open items, so a restart doesn't raise them again.
        let mut raised = crew.raised_marks().await;
        // A check-in whose turn didn't run comes back here, to be tried again
        // once: none is dropped without a word.
        let (retry, mut retried) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<Trigger>)>();
        // What a boss's notices hand back when it no longer runs on them (Act
        // fully ended, or isn't in force yet): checked in on here, the old way.
        let (_unused, none_back) = tokio::sync::mpsc::unbounded_channel();
        let mut handed_back = crew.wake_back.1.lock().take().unwrap_or(none_back);
        // The ready tickets nobody has, so that one that becomes so wakes it.
        let mut ready_seen = HashMap::new();
        let mut ready_dirty = false;
        crew.ready_unhanded(&mut ready_seen, |_| None).await;
        loop {
            // What was found, by the overseer it is for.
            let mut found: Vec<(String, Trigger)> = Vec::new();
            tokio::select! {
                () = cancel.cancelled() => return,
                received = events.recv() => match received {
                    Ok(DeskEvent::Activity { event }) => {
                        if let Some((s, o)) = crew.overseer_of_group(event.group.as_deref()) {
                            if let Some(mut t) = crew.trigger_for(&event, s.agent_id.as_str(), &o) {
                                if !crew.its_plan_tells(&event).await {
                                    if let Some(session) = finished_session(&event) {
                                        t.line = crew.with_doing(session, t.line).await;
                                    }
                                    found.push((s.agent_id.as_str().to_string(), t));
                                }
                            }
                        }
                    }
                    // Looked at on the next tick, not here: reading the boards
                    // while events wait would let the channel lag and lose
                    // the wakes among them.
                    Ok(DeskEvent::Ticket { .. }) => ready_dirty = true,
                    // Events were lost: the boards may have changed in them.
                    Err(RecvError::Lagged(_)) => ready_dirty = true,
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                },
                // A turn's end the boss's fast call handed to its full turn.
                Ok(event) = rework.recv() => {
                    if let Some((s, o)) = crew.overseer_of_group(event.group.as_deref()) {
                        if let Some(mut t) = crew.trigger_for(&event, s.agent_id.as_str(), &o) {
                            if let Some(session) = finished_session(&event) {
                                t.line = crew.with_doing(session, t.line).await;
                                t.handed = Some(session);
                            }
                            found.push((s.agent_id.as_str().to_string(), t));
                        }
                    }
                }
                Some((id, back)) = handed_back.recv() => {
                    let w = waking.entry(id).or_default();
                    w.window.get_or_insert_with(Instant::now);
                    hold(&mut w.pending, back);
                }
                Some((id, again)) = retried.recv() => {
                    tracing::warn!(agent = %id, held = again.len(), "the overseer's check-in didn't run; trying it again");
                    // Held again as it was: what it asks was raised already.
                    let w = waking.entry(id).or_default();
                    w.window.get_or_insert_with(Instant::now);
                    hold(&mut w.pending, again);
                }
                _ = tick.tick() => {
                    let overseers = crew.overseers();
                    // Kept up to date with no overseer too, so that one set up
                    // later isn't woken for what has long been ready.
                    if std::mem::take(&mut ready_dirty) {
                        let of = |g: Option<&str>| crew.overseer_of_group(g);
                        for t in crew.ready_unhanded(&mut ready_seen, |g| of(g).map(|(_, o)| o)).await {
                            if let Some((s, _)) = of(t.group.as_deref()) {
                                found.push((s.agent_id.as_str().to_string(), t));
                            }
                        }
                    }
                    for (s, o) in &overseers {
                        let id = s.agent_id.as_str().to_string();
                        let w = waking.entry(id.clone()).or_default();
                        let mut noted = std::mem::take(&mut w.noted);
                        let at = crew.thresholds(&id, o, &mut noted).await;
                        let w = waking.entry(id.clone()).or_default();
                        w.noted = noted;
                        found.extend(at.into_iter().map(|mut t| {
                            t.routine = true;
                            (id.clone(), t)
                        }));
                        // Unbound, it sweeps its scope every few minutes while
                        // there is anything in it, whatever else woke it.
                        let every = crew.timing.minute * UNBOUND_SWEEP_MINUTES;
                        let due = w.swept.is_none_or(|at| at.elapsed() >= every);
                        // Running on notices, it is swept only once they have
                        // been quiet a while: the net under them.
                        let noticed = crew.orchestrates(&id) && crew.noticed_lately(&id);
                        if due && !noticed && crew.unbound(o) && crew.has_work(&id, o).await {
                            if let Some(w) = waking.get_mut(&id) {
                                w.swept = Some(Instant::now());
                            }
                            found.push((id.clone(), Trigger {
                                line: SWEEP.into(),
                                approvals: Vec::new(),
                                hop: 0,
                                about: None,
                                group: None,
                                handed: None,
                                routine: true,
                            }));
                        }
                    }
                    // An overseer gone has nothing waiting.
                    waking.retain(|id, _| overseers.iter().any(|(s, _)| s.agent_id.as_str() == id));
                }
            }
            let now = Utc::now();
            for (id, mut t) in found {
                let Some((_, o)) = crew.overseer_named(&id) else {
                    continue;
                };
                let remind = (o.wake.remind_minutes > 0).then(|| {
                    chrono::Duration::from_std(crew.timing.minute * o.wake.remind_minutes)
                        .unwrap_or(RAISED_FOR)
                });
                if fresh(&mut t, &raised, remind, now) {
                    let w = waking.entry(id).or_default();
                    w.window.get_or_insert_with(Instant::now);
                    hold(&mut w.pending, vec![t]);
                }
            }
            let ids: Vec<String> = waking.keys().cloned().collect();
            for id in ids {
                let Some(w) = waking.get_mut(&id) else {
                    continue;
                };
                if w.firing.as_ref().is_some_and(|f| f.is_finished()) {
                    w.firing = None;
                }
                let due = w
                    .window
                    .is_some_and(|at| at.elapsed() >= crew.timing.coalesce);
                if !due || w.firing.is_some() || w.pending.is_empty() {
                    continue;
                }
                // Its mind is busy, or the person's own turn runs in its DM:
                // what it found keeps gathering for its next look, until it has
                // waited long enough to queue for the mind in turn.
                if w.window.is_some_and(|at| !crew.mind_may_start(&id, at)) {
                    continue;
                }
                if w.held_until.is_some_and(|until| Utc::now() < until) {
                    continue;
                }
                let Some((service, o)) = crew.overseer_named(&id) else {
                    w.window = None;
                    w.pending.clear();
                    continue;
                };
                // A check-in that speaks in the DM waits while the person types
                // to the boss (at most the typing cap); a quiet one acts now.
                let (level, _) = crew.level_now(&id, &o);
                // Requests beside its message, and what needs the person.
                let asks = w.pending.iter().any(|t| !t.approvals.is_empty());
                let needs = asks || w.pending.iter().any(|t| t.about.is_some());
                let speaks = !(level.acts() && o.updates.enabled) || asks;
                if speaks && crew.typing_release(&id).is_some() {
                    continue;
                }
                // Away, a check-in that would only tell the person gathers for
                // their return; one with what needs them still goes.
                if speaks && !needs && crew.away_now(&id).await {
                    continue;
                }
                // Held by Desk's breaker, it takes no turns: what it would have
                // checked in on is on the Floor for the person.
                if crew.desk.agent_held(&id).is_some() {
                    w.window = None;
                    w.pending.clear();
                    continue;
                }
                // What it gathered of a group that has since become another's
                // boundary is not its any more.
                let seen = crew.desk.seen_by(&o.scope);
                w.pending.retain(|t| seen.sees(t.group.as_deref()));
                if w.pending.is_empty() {
                    w.window = None;
                    continue;
                }
                match crew.budget(&id, &o).await {
                    Ok(Budget::Open) => w.held_until = None,
                    Ok(Budget::Spent { until, daily }) => {
                        w.held_until = Some(until);
                        if w.told != Some(until) {
                            w.told = Some(until);
                            crew.paused(&service, until, daily);
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "counting the overseer's check-ins failed");
                        w.held_until = chrono::Duration::from_std(crew.timing.coalesce)
                            .ok()
                            .map(|d| Utc::now() + d);
                        continue;
                    }
                }
                w.window = None;
                let triggers = std::mem::take(&mut w.pending);
                let now = Utc::now();
                raised.retain(|_, at| now - *at < RAISED_FOR);
                for t in &triggers {
                    if let Some(key) = &t.about {
                        raised.insert(key.clone(), now);
                        crew.mark_raised(&id, key, &t.line, now).await;
                    }
                }
                let crew = Arc::clone(&crew);
                let retry = retry.clone();
                let again: Vec<Trigger> = triggers
                    .iter()
                    .filter(|t| !t.line.starts_with(RETRIED))
                    .map(|t| Trigger {
                        line: format!("{RETRIED}{}", t.line),
                        routine: true,
                        ..t.clone()
                    })
                    .collect();
                let handed: Vec<uuid::Uuid> = triggers.iter().filter_map(|t| t.handed).collect();
                w.firing = Some(tokio::spawn(async move {
                    // Its prompt is written once it holds its mind.
                    let woke = crew
                        .with_mind(&id, "a check-in", crew.wake(service, o, triggers))
                        .await;
                    if woke {
                        // A refused next step handed to it was its to take
                        // (`super::idle`).
                        for s in handed {
                            crew.full_turn_over(&id, s);
                        }
                    } else if !again.is_empty() {
                        let _ = retry.send((id, again));
                    }
                }));
            }
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use chrono::TimeDelta;

    fn oversight(per_hour: u32, per_day: u32) -> Oversight {
        Oversight {
            max_checkins_per_hour: per_hour,
            max_checkins_per_day: per_day,
            ..Oversight::default()
        }
    }

    #[test]
    fn check_ins_are_spent_from_a_rolling_hour_under_a_ceiling_for_the_day() {
        let now = Utc::now();
        let hour = TimeDelta::hours(1);
        let day = (now - TimeDelta::hours(10), now + TimeDelta::hours(14));
        let ago = |m: i64| now - TimeDelta::minutes(m);
        let wakes = [ago(200), ago(50), ago(10)];
        assert_eq!(
            budget(&oversight(3, 100), &wakes, now, hour, day),
            Budget::Open
        );
        assert_eq!(
            budget(&oversight(2, 100), &wakes, now, hour, day),
            Budget::Spent {
                until: ago(50) + hour,
                daily: false
            },
            "free again once the hour's oldest is an hour old"
        );
        assert_eq!(
            budget(&oversight(1, 100), &wakes, now, hour, day),
            Budget::Spent {
                until: ago(10) + hour,
                daily: false
            }
        );
        assert_eq!(
            budget(&oversight(6, 3), &wakes, now, hour, day),
            Budget::Spent {
                until: day.1,
                daily: true
            },
            "the day's ceiling holds until the next day"
        );
        let yesterday = (now + TimeDelta::minutes(5), now + TimeDelta::days(1));
        assert_eq!(
            budget(&oversight(6, 1), &wakes, now, hour, yesterday),
            Budget::Open,
            "yesterday's check-ins are not today's"
        );
    }

    #[test]
    fn unbound_wakes_soon_and_is_capped_only_against_a_runaway_loop() {
        let o = Oversight::default();
        let fast = unbound_timings(&o);
        assert_eq!(
            (fast.wake.waiting_minutes, fast.wake.quiet_minutes),
            (1, 3),
            "from the defaults of {} and {}",
            o.wake.waiting_minutes,
            o.wake.quiet_minutes
        );
        assert!(fast.wake.on_finish && fast.wake.on_failure);
        // The person's sooner setting stands.
        let mut sooner = o.clone();
        sooner.wake.quiet_minutes = 2;
        assert_eq!(unbound_timings(&sooner).wake.quiet_minutes, 2);
        // A busy night: 100 check-ins in the last hour.
        let now = Utc::now();
        let hour = TimeDelta::hours(1);
        let day = (now - TimeDelta::hours(10), now + TimeDelta::hours(14));
        let wakes: Vec<_> = (0..100).map(|i| now - TimeDelta::seconds(30 * i)).collect();
        assert!(matches!(
            budget(&o, &wakes, now, hour, day),
            Budget::Spent { .. }
        ));
        assert_eq!(budget(&fast, &wakes, now, hour, day), Budget::Open);
        let runaway: Vec<_> = (0..UNBOUND_CHECKINS_PER_HOUR as i64)
            .map(|i| now - TimeDelta::seconds(i))
            .collect();
        assert!(matches!(
            budget(&fast, &runaway, now, hour, day),
            Budget::Spent { .. }
        ));
    }

    fn asking(about: &str) -> Trigger {
        Trigger {
            line: format!("web asks “{about}”"),
            approvals: vec![],
            hop: 0,
            about: Some(about.into()),
            group: None,
            handed: None,
            routine: false,
        }
    }

    #[test]
    fn a_raised_question_wakes_it_again_only_as_a_reminder() {
        let now = Utc::now();
        let raised = HashMap::from([("ship".to_string(), now - TimeDelta::minutes(30))]);
        let mut news = Trigger {
            about: None,
            ..asking("x")
        };
        assert!(
            fresh(&mut news, &raised, None, now),
            "news is always worth it"
        );
        assert!(
            fresh(&mut asking("merge"), &raised, None, now),
            "a new question"
        );
        assert!(
            !fresh(&mut asking("ship"), &raised, None, now),
            "never reminded"
        );
        assert!(!fresh(
            &mut asking("ship"),
            &raised,
            Some(TimeDelta::hours(2)),
            now
        ));
        let mut due = asking("ship");
        assert!(fresh(&mut due, &raised, Some(TimeDelta::minutes(20)), now));
        assert!(
            due.line.starts_with("Reminder, still open: "),
            "{}",
            due.line
        );
    }

    #[test]
    fn what_is_held_is_each_thing_once_and_the_latest() {
        let mut pending = vec![asking("ship")];
        let mut again = asking("ship");
        again.line = "web has waited on you for 9 min: asks “ship”".into();
        hold(&mut pending, vec![again.clone(), asking("merge")]);
        assert_eq!(
            pending.iter().map(|t| t.line.as_str()).collect::<Vec<_>>(),
            [again.line.as_str(), "web asks “merge”"]
        );
        let many = (0..MAX_HELD + 5)
            .map(|n| Trigger {
                about: None,
                ..asking(&n.to_string())
            })
            .collect();
        hold(&mut pending, many);
        assert_eq!(pending.len(), MAX_HELD);
        assert_eq!(
            pending.last().unwrap().line,
            format!("web asks “{}”", MAX_HELD + 4)
        );
    }
}
