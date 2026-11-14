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
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::{Level, Oversight, Scope};
use workforce_desk::crew::Crew as _;
use workforce_desk::crew::{
    self, Event, ItemKind, ItemState, Kind, MentionedApproval, Party, PartyKind,
};
use workforce_desk::model::{DeskEvent, Origin};
use workforce_desk::CheckIn;

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

/// What every DM turn of a boss is told before it speaks of a session.
/// What an unbound boss is told of its plans: a draft for the person,
/// started only on their word (a bound one can't approve one at all).
pub(super) const PLAN_DRAFT: &str = "A plan you make is a draft for the person to review and approve; start one only when their own words ask you to start the work (start_now).";

pub(super) const CHECK_FIRST: &str = "Before you say anything about a session, read its account with crew.account; never guess. Answer as Done / Now / Next / Waiting on, from the account, and say how fresh it is if it's over a few minutes old.";

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
            for s in self.services() {
                for name in READ.iter().chain(ACT.iter()) {
                    s.tools.unregister(name);
                }
                // `desk.*` only on agents that oversee nothing.
                match want.contains_key(s.agent_id.as_str()) {
                    true => crate::connector_pins::pin(&s),
                    false => crate::desk_driver::register_tools(&s),
                }
            }
            for (s, o) in &current {
                for name in DESK_TOOLS {
                    s.tools.unregister(name);
                }
                let id = s.agent_id.as_str();
                for tool in CrewTool::for_level(&self.me(), id, &o.scope, o.level == Level::Watch) {
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
        if matches!(event.actor.kind, PartyKind::Manager | PartyKind::You) || event.hop >= MAX_HOPS
        {
            return None;
        }
        let subject = event.subject.as_ref()?;
        if subject.id == overseer || !tracked(o, subject) {
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
        wanted.then(|| Trigger {
            line: format!("{} {}", who(subject), event.text),
            approvals: Vec::new(),
            hop: event.hop,
            about,
            group: event.group.clone(),
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
        let seen = self.desk.seen_by(&o.scope);
        let Ok(overview) = self.desk.overview_for(self, None, Some(&seen)).await else {
            return Vec::new();
        };
        let now = Utc::now();
        let minute = chrono::Duration::from_std(self.timing.minute).unwrap_or_default();
        let past = |at: Option<chrono::DateTime<Utc>>, minutes: u32| {
            minutes > 0 && at.is_some_and(|at| now - at >= minute * minutes as i32)
        };
        let mut holding = HashSet::new();
        let mut out = Vec::new();
        for item in &overview.items {
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
            // A session's question the person is in control of, or its Act
            // fully boss answers itself (`answer`), and a request its decision
            // call has (`decide`): not raised here.
            if item.kind == ItemKind::Session
                && item
                    .id
                    .parse()
                    .ok()
                    .and_then(|s| self.desk.view(s).ok())
                    .is_some_and(|v| match (&item.approval, &item.question) {
                        // Only while its call may still be deciding it: one
                        // it left to the person, or Desk refused, is raised.
                        // Answered since the overview was read (often by the
                        // decision call itself): it no longer waits.
                        (Some(a), _)
                            if v.pending.as_ref().is_none_or(|p| p.id.to_string() != a.id) =>
                        {
                            true
                        }
                        (Some(_), _) => {
                            (self.answers_at_once(&v)
                                && v.pending.as_ref().is_some_and(|p| {
                                    Utc::now() - p.since < super::decide::DECIDING
                                }))
                                || self.dialog_is_persons(&v, overseer)
                        }
                        (None, Some(_)) => {
                            super::answer::question_of(&v).is_some()
                                && (self.person_has_questions(&v, overseer)
                                    || self.answers_question_itself(&v))
                        }
                        (None, None) => false,
                    })
            {
                continue;
            }
            let (key, trigger) = match item.state {
                ItemState::NeedsYou if past(item.since, o.wake.waiting_minutes) => {
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
                        },
                    )
                }
                ItemState::Working | ItemState::Quiet
                    if past(item.last_activity, o.wake.quiet_minutes) =>
                {
                    let quiet = item
                        .last_activity
                        .map(|s| (now - s).num_minutes().max(1))
                        .unwrap_or(1);
                    (
                        format!(
                            "quiet:{}:{}",
                            item.id,
                            item.last_activity
                                .map(|t| t.timestamp_millis())
                                .unwrap_or(0)
                        ),
                        Trigger {
                            line: format!(
                                "{} has been quiet for {quiet} min while working on {}{}",
                                who(&party),
                                item.task.as_deref().unwrap_or("its task"),
                                item.detail
                                    .as_deref()
                                    .map(|d| format!(" (last: {d})"))
                                    .unwrap_or_default()
                            ),
                            approvals: vec![],
                            hop: 0,
                            about: None,
                            group: item.group_id.clone(),
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

    /// What an unbound overseer is told each time it wakes: be the boss.
    pub(super) fn boss_hint(o: &Oversight) -> String {
        format!(
            "You are unbound: no limits but the person's budget, their Stop, and their hold while \
             they type. Be the boss, keep everything moving, spawn sessions for open work up to the \
             parallel limit ({}). For every session in your scope (crew.overview): waiting on a \
             question or an approval, answer or approve it; finished with an obvious next step from \
             its goal, plan or ticket, nudge it on; stuck or errored, resume or restart it; and \
             while the plan's next tickets are free and fewer than {} sessions are working, start \
             sessions for them (crew.start_session or crew.assign_ticket). Between turns, set a \
             session's mode with crew.set_mode: auto before routine work, default before work that \
             will need your judgement. Nothing should sit idle. Check the person's preferences first and apply \
             the one that fits (a close match too: apply it and say so). With none, decide yourself; give the \
             person a question only when you truly can't, with crew.ask_person and generalizes, so their \
             answer settles that kind of case from then on. Give a reason each time; the person reads them in Activity. Your own limits stay theirs: asked to change them, say plainly you can't and post crew.ask_oversight_change.",
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
        let hint = Self::level_words(level);
        let off = o.act.off();
        if !level.acts() || off.is_empty() {
            return hint.to_string();
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
                return format!("You oversee the crew, unbound: do everything yourself with the crew.* tools. {PLAN_DRAFT} {CHECK_FIRST}");
            }
            Level::Watch => "at Watch: don't act; say what you would do.",
            Level::Suggest => "at Suggest: your crew.* actions become suggestions the person accepts.",
            Level::Act => "at Act: do the work yourself with the crew.* tools, don't offer it.",
            Level::Full => "at Act fully: do the work yourself with the crew.* tools, and ship in the repos the person named.",
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
        format!("You oversee the crew {says}{off} {CHECK_FIRST}")
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
        let mut record = CheckIn {
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
    fn paused(&self, service: &AgentService, until: DateTime<Utc>, daily: bool) {
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
pub(super) async fn wake_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let (_, mut events) = crew.desk.subscribe();
    let mut rework = crew.rework.subscribe();
    let mut tick = tokio::time::interval(crew.timing.tick.min(crew.timing.coalesce));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut waking: HashMap<String, Waking> = HashMap::new();
    // Kept on the boss's open items, so a restart doesn't raise them again.
    let mut raised = crew.raised_marks().await;
    // A check-in whose turn didn't run comes back here, to be tried again
    // once: none is dropped without a word.
    let (retry, mut retried) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<Trigger>)>();
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
                            if let Some(session) = finished_session(&event) {
                                t.line = crew.with_doing(session, t.line).await;
                            }
                            found.push((s.agent_id.as_str().to_string(), t));
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
                        }
                        found.push((s.agent_id.as_str().to_string(), t));
                    }
                }
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
                    found.extend(at.into_iter().map(|t| (id.clone(), t)));
                    // Unbound, it sweeps its scope every few minutes while
                    // there is anything in it, whatever else woke it.
                    let every = crew.timing.minute * UNBOUND_SWEEP_MINUTES;
                    let due = w.swept.is_none_or(|at| at.elapsed() >= every);
                    if due && crew.unbound(o) && crew.has_work(&id, o).await {
                        if let Some(w) = waking.get_mut(&id) {
                            w.swept = Some(Instant::now());
                        }
                        found.push((id.clone(), Trigger {
                            line: SWEEP.into(),
                            approvals: Vec::new(),
                            hop: 0,
                            about: None,
                            group: None,
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
                    ..t.clone()
                })
                .collect();
            w.firing = Some(tokio::spawn(async move {
                if !crew.wake(service, o, triggers).await && !again.is_empty() {
                    let _ = retry.send((id, again));
                }
            }));
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
