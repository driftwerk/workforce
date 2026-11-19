//! At Act fully the boss runs as one orchestrator would
//! (designs/boss-as-claude-code.md): one oversight conversation per boss
//! and person, apart from its DM and its other duties, that every crew
//! turn of it runs in, resumed each time on the boss's own engine and
//! model. What happens reaches it there as notices (a session's turn end,
//! a watch it started ending, what its wake-ups found), and it acts on
//! them at once.
//!
//! - **One turn at a time.** A notice that comes while its turn runs waits
//!   and is folded into the next, which starts as soon as that one ends:
//!   never dropped, never told out of order.
//! - **Everything at hand.** Each turn opens with what the stores say now,
//!   whatever the transcript kept: the person's own words in the DM since
//!   its last turn, its plans, its watches (with Desk's context block of
//!   its sessions, open items, preferences and picture).
//! - **Only budget, audit and Stop.** The check-in ceiling counts its turns
//!   (a runaway stop, not a work limit); Desk's breaker holds them; a turn
//!   the person stops keeps its notices for the next, never retried by
//!   itself. A turn that fails is tried again, then backs off.
//! - **Local only, for now.** This hub's own bosses: never one served from
//!   a grant or relayed from a home, whose crew calls are decided where
//!   their sessions are. Below Act fully, or with Act fully not in force,
//!   what was queued goes the old way, as a check-in.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_desk::crew;
use workforce_desk::CheckIn;

use super::watcher::{check_in_channel, reported, Budget, Trigger};
use super::{name_of, CrewHub, CHECK_IN};
use crate::serve::AgentService;

/// The most notices held for one conversation: past it the oldest are
/// folded into one line.
const MAX_HELD: usize = 200;

/// How long after a failed turn its notices are tried again, in the crew's
/// minutes: once soon, then after a while.
const RETRY_SOON_MINUTES: u32 = 1;
const RETRY_LATER_MINUTES: u32 = 5;

/// The most of the person's DM words a turn is given, and of each.
const DM_WORDS: usize = 20;
const DM_WORD_CHARS: usize = 800;
/// How far back the person's DM words go when the conversation has had no
/// turn yet.
const DM_FIRST_LOOK_HOURS: i64 = 12;

/// How long after its last turn an unbound boss with notices flowing is
/// still swept: the sweep is the net under the notices, not their pace.
pub(super) const SWEEP_AFTER_QUIET_MINUTES: u32 = 10;

/// What a notices turn starts with, after [`CHECK_IN`].
const HEAD: &str = "Notices for you, at Act fully, since your last turn (oldest first):";

/// What the boss is told to do with them, after its standing instructions.
pub(super) const LOOP: &str = "This is your own running conversation as the person's boss: \
    their goals, what they told you and what you started all carry on here, and what happens \
    reaches you as notices like these. Act on each now, yourself, to the end of the work: \
    when a session says it opened a pull request or waits on CI, start a watch on it with \
    crew.watch (a note of what you'll do when it ends) rather than waiting to be told; when a \
    watch ends green on a reviewed pull request, merge it as the person's directives say, \
    move its ticket to done, close the finished session (crew.close_session) and start what \
    is unblocked; a ticket in Doing whose session is gone is restarted; an answer the person \
    already gave (their words below) is passed on, never asked again. Ask the person only \
    what they alone can decide, with crew.ask_person, and say who moves each wait on next. \
    Your words here are your own notes, not posted: what you do reaches the person as your \
    decisions and updates.";

/// One thing that happened, for the boss's conversation.
#[derive(Debug, Clone)]
pub(super) struct Notice {
    pub at: DateTime<Utc>,
    /// In words for the boss; what anyone else wrote fenced already.
    pub text: String,
    /// The watch whose end it tells: noted as told once a turn has it.
    pub watch: Option<Uuid>,
    /// Told once already in a turn that didn't run.
    pub again: bool,
}

impl Notice {
    pub(super) fn new(text: impl Into<String>) -> Self {
        Self {
            at: Utc::now(),
            text: text.into(),
            watch: None,
            again: false,
        }
    }
}

/// One conversation's notices.
#[derive(Debug, Default)]
struct Queue {
    pending: VecDeque<Notice>,
    /// Its runner is on it.
    running: bool,
    /// Not tried again before this (a failed turn, a spent ceiling).
    not_before: Option<Instant>,
    /// Failed turns in a row.
    failures: u32,
    /// Its last turn that ran, and when it began: the person's DM words
    /// are read from then on.
    last_turn: Option<(Instant, DateTime<Utc>)>,
    /// A notice folded away for room, counted.
    folded: usize,
}

/// Every oversight conversation's notices, by (boss, person).
#[derive(Debug, Default)]
pub(crate) struct Notices(parking_lot::Mutex<HashMap<(String, String), Queue>>);

impl CrewHub {
    /// Whether `boss` runs as an orchestrator now: this hub's own boss (never
    /// one served from a grant or relayed from a home), at Act fully in
    /// force.
    pub(crate) fn orchestrates(&self, boss: &str) -> bool {
        #[cfg(test)]
        if self
            .focused_turn_ends
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        !workforce_desk::grant::is_served(boss)
            && !super::relayed_decide::relayed_party(boss)
            && self
                .overseer_named(boss)
                .is_some_and(|(s, o)| self.unbound_given(s.agent_id.as_str(), &o))
    }

    /// A test's boss judges turn ends with the focused call, as one the loop
    /// doesn't run for does.
    #[cfg(test)]
    pub(crate) fn focused_turn_ends(&self) {
        self.focused_turn_ends
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether `boss`'s oversight conversation had a turn in the last
    /// [`SWEEP_AFTER_QUIET_MINUTES`]: then a sweep has nothing to add.
    pub(super) fn noticed_lately(&self, boss: &str) -> bool {
        let quiet = self.timing.minute * SWEEP_AFTER_QUIET_MINUTES;
        self.notices.0.lock().iter().any(|((b, _), q)| {
            b == boss && (q.running || q.last_turn.is_some_and(|(at, _)| at.elapsed() < quiet))
        })
    }

    /// Tells `boss`'s oversight conversation with `person` of `notice`; its
    /// turn starts now unless one runs, or it is waiting to be tried again.
    pub(super) fn notify(self: &Arc<Self>, boss: &str, person: &str, notice: Notice) {
        let key = (boss.to_string(), person.to_string());
        let start = {
            let mut all = self.notices.0.lock();
            let q = all.entry(key.clone()).or_default();
            if q.pending.len() >= MAX_HELD {
                q.pending.pop_front();
                q.folded += 1;
            }
            q.pending.push_back(notice);
            // Something new is new work: a turn the person stopped held its
            // notices only until now.
            if q.not_before.is_some() && q.failures == 0 {
                q.not_before = None;
            }
            claim(q)
        };
        if start {
            tokio::spawn(Arc::clone(self).run_notices(key));
        }
    }

    /// Starts every conversation whose notices wait and may go now: the
    /// watch loop's tick.
    pub(super) fn kick_notices(self: &Arc<Self>) {
        let due: Vec<(String, String)> = {
            let mut all = self.notices.0.lock();
            all.retain(|_, q| !q.pending.is_empty() || q.running || q.last_turn.is_some());
            all.iter_mut()
                .filter_map(|(k, q)| claim(q).then(|| k.clone()))
                .collect()
        };
        for key in due {
            tokio::spawn(Arc::clone(self).run_notices(key));
        }
    }

    /// Puts `batch` back at the front of `key`'s queue, to go no sooner
    /// than `after`, and lets its runner go.
    fn hold_back(&self, key: &(String, String), batch: Vec<Notice>, after: Option<Duration>) {
        let mut all = self.notices.0.lock();
        let q = all.entry(key.clone()).or_default();
        for n in batch.into_iter().rev() {
            q.pending.push_front(n);
        }
        q.running = false;
        q.not_before = after.map(|d| Instant::now() + d);
    }

    /// The runner of `key`'s conversation: a turn on what waits, again while
    /// more came meanwhile.
    async fn run_notices(self: Arc<Self>, key: (String, String)) {
        let (boss, person) = (key.0.as_str(), key.1.as_str());
        loop {
            let (batch, folded, since) = {
                let mut all = self.notices.0.lock();
                let Some(q) = all.get_mut(&key) else {
                    return;
                };
                if q.pending.is_empty() {
                    q.running = false;
                    return;
                }
                (
                    q.pending.drain(..).collect::<Vec<_>>(),
                    std::mem::take(&mut q.folded),
                    q.last_turn.map(|(_, at)| at),
                )
            };
            let Some((service, o)) = self.overseer_named(boss) else {
                tracing::info!(%boss, held = batch.len(), "notices for a boss that no longer oversees are let go");
                self.hold_back(&key, Vec::new(), None);
                return;
            };
            // Act fully ended, or isn't in force: the old way, as a check-in.
            if !self.orchestrates(boss) {
                self.hold_back(&key, Vec::new(), None);
                let triggers = batch.into_iter().map(|n| Trigger::line(n.text)).collect();
                self.wake(service, o, triggers).await;
                return;
            }
            // Held by Desk's breaker: its notices wait for it to let go.
            if self.desk.agent_held(boss).is_some() {
                self.hold_back(&key, batch, Some(self.timing.minute));
                return;
            }
            match self.budget(boss, &o).await {
                Ok(Budget::Open) => {}
                Ok(Budget::Spent { until, daily }) => {
                    self.paused(&service, until, daily);
                    let wait = (until - Utc::now()).to_std().unwrap_or(self.timing.minute);
                    self.hold_back(&key, batch, Some(wait));
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "counting the boss's turns failed");
                    self.hold_back(&key, batch, Some(self.timing.minute));
                    return;
                }
            }
            let began = Utc::now();
            let prompt = self
                .notices_prompt(&service, &o, person, &batch, folded, since)
                .await;
            let watches: Vec<Uuid> = batch.iter().filter_map(|n| n.watch).collect();
            match self.notices_turn(&service, person, &prompt).await {
                Ok(()) => {
                    if let Err(e) = self.desk.store().watches_delivered(&watches).await {
                        tracing::warn!(error = %format!("{e:#}"), "the told watches weren't marked told");
                    }
                    self.watches_told(&watches);
                    let mut all = self.notices.0.lock();
                    let q = all.entry(key.clone()).or_default();
                    q.failures = 0;
                    q.last_turn = Some((Instant::now(), began));
                }
                Err(Ran::Stopped) => {
                    // The person's Stop: kept, never tried again by itself.
                    tracing::info!(%boss, held = batch.len(), "the boss's notices turn was stopped; its notices wait for what comes next");
                    self.hold_back(&key, batch, Some(Duration::from_secs(365 * 24 * 3600)));
                    return;
                }
                Err(Ran::Failed(e)) => {
                    let failures = {
                        let mut all = self.notices.0.lock();
                        let q = all.entry(key.clone()).or_default();
                        q.failures += 1;
                        q.failures
                    };
                    tracing::warn!(%boss, error = %e, failures, "the boss's notices turn didn't run; tried again");
                    let again = batch
                        .into_iter()
                        .map(|mut n| {
                            n.again = true;
                            n
                        })
                        .collect();
                    let minutes = match failures {
                        1 => RETRY_SOON_MINUTES,
                        _ => RETRY_LATER_MINUTES,
                    };
                    self.hold_back(&key, again, Some(self.timing.minute * minutes));
                    return;
                }
            }
        }
    }

    /// Runs `prompt` in `service`'s oversight conversation with `person`,
    /// kept as a check-in (the ceiling counts it), under its mind.
    async fn notices_turn(
        &self,
        service: &Arc<AgentService>,
        person: &str,
        prompt: &str,
    ) -> Result<(), Ran> {
        let id = service.agent_id.as_str().to_string();
        let mut record = CheckIn {
            id: crew::new_id(),
            at: crew::now(),
            agent_id: id.clone(),
            kind: "notices".into(),
            prompt: prompt.to_string(),
            reply: None,
        };
        if let Err(e) = self.desk.store().put_check_in(&record).await {
            tracing::warn!(error = %e, "recording the boss's notices turn failed");
        }
        self.yield_to_person(&id).await;
        let ran = self
            .with_mind(
                &id,
                "its notices",
                self.run_turn(
                    service,
                    person,
                    &check_in_channel(&id),
                    prompt,
                    0,
                    None,
                    None,
                ),
            )
            .await;
        match ran {
            Ok((_, reply)) => {
                record.reply = Some(reply);
                if let Err(e) = self.desk.store().put_check_in(&record).await {
                    tracing::warn!(error = %e, "recording the boss's notices turn's answer failed");
                }
                Ok(())
            }
            Err(e) if format!("{e:#}").contains(super::deliver::STOPPED) => Err(Ran::Stopped),
            Err(e) => Err(Ran::Failed(format!("{e:#}"))),
        }
    }

    /// What a notices turn is given: the notices, then what the stores say
    /// now (the person's DM words since `since`, its plans, its watches),
    /// then its standing instructions.
    async fn notices_prompt(
        &self,
        service: &Arc<AgentService>,
        o: &workforce_common::oversight::Oversight,
        person: &str,
        batch: &[Notice],
        folded: usize,
        since: Option<DateTime<Utc>>,
    ) -> String {
        let tz = self.desk.timezone();
        let mut lines: Vec<String> = Vec::new();
        if folded > 0 {
            lines.push(format!(
                "- (and {folded} older notices, folded for room: crew.overview and crew.activity have them)"
            ));
        }
        for n in batch {
            lines.push(format!(
                "- {}{} {}",
                n.at.with_timezone(&tz).format("%H:%M"),
                if n.again {
                    " (again, as the last turn didn't run)"
                } else {
                    ""
                },
                n.text
            ));
        }
        let mut out = format!("{CHECK_IN} {HEAD}\n{}", lines.join("\n"));
        if let Some(words) = dm_words(service, person, since) {
            out.push_str("\n\n");
            out.push_str(&words);
        }
        if let Some(plans) = self.plans_said(service.agent_id.as_str()).await {
            out.push_str("\n\n");
            out.push_str(&plans);
        }
        if let Some(watches) = self.watches_said(service.agent_id.as_str(), person).await {
            out.push_str("\n\n");
            out.push_str(&watches);
        }
        out.push_str("\n\n");
        out.push_str(&Self::boss_hint(o));
        out.push(' ');
        out.push_str(LOOP);
        out
    }

    /// `boss`'s plans still open, a line each: where its tickets stand.
    async fn plans_said(&self, boss: &str) -> Option<String> {
        use workforce_desk::plan::PlanState;
        let plans = match self.desk.plans(None, None).await {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "the boss's plans couldn't be read for its notices");
                return None;
            }
        };
        let lines: Vec<String> = plans
            .iter()
            .filter(|p| p.plan.boss == boss)
            .filter(|p| !matches!(p.plan.state, PlanState::Done | PlanState::Dropped))
            .map(|p| {
                let g = &p.progress;
                format!(
                    "- {} (plan_id {}, {:?}{}): {} of {} done, {} doing, {} in review, {} ready{}{}",
                    crew::one_line(&p.plan.goal, 120),
                    p.plan.id,
                    p.plan.state,
                    p.paused_why
                        .as_deref()
                        .map(|w| format!(", paused: {}", crew::one_line(w, 120)))
                        .unwrap_or_default(),
                    g.done,
                    g.total,
                    g.doing,
                    g.review,
                    g.ready,
                    match g.next.is_empty() {
                        true => String::new(),
                        false => format!(", next {}", g.next.join(", ")),
                    },
                    p.held
                        .as_deref()
                        .map(|h| format!("; its starts are held: {h}"))
                        .unwrap_or_default(),
                )
            })
            .collect();
        (!lines.is_empty()).then(|| {
            reported(
                &format!("Your plans now:\n{}", lines.join("\n")),
                "Desk's plans",
            )
        })
    }
}

/// Whether a runner takes `q` now: notices wait, none runs, and it isn't
/// waiting to be tried again. Taken, it is marked running.
fn claim(q: &mut Queue) -> bool {
    let go =
        !q.running && !q.pending.is_empty() && q.not_before.is_none_or(|at| Instant::now() >= at);
    if go {
        q.running = true;
        q.not_before = None;
    }
    go
}

/// How a notices turn went.
enum Ran {
    /// The person stopped it.
    Stopped,
    Failed(String),
}

/// The person's own words in `service`'s DM with `person` since `since`
/// (else the last [`DM_FIRST_LOOK_HOURS`]): their answers and orders, read
/// from the DM itself so none is lost to a rotation or a compaction.
fn dm_words(service: &AgentService, person: &str, since: Option<DateTime<Utc>>) -> Option<String> {
    use workforce_desk::conversations::{Author, SpeakerKind};
    let since = since.unwrap_or_else(|| Utc::now() - chrono::Duration::hours(DM_FIRST_LOOK_HOURS));
    let speaker = workforce_desk::Speaker::agent(service.agent_id.as_str(), &name_of(service));
    let page = match crate::desk_conversations::page(
        service.sessions.as_ref(),
        &speaker,
        person,
        workforce_common::channels::DESK_CHANNEL,
        workforce_desk::Cursor::Newest,
        DM_WORDS * 3,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(error = %e, "the person's DM words couldn't be read for the boss's notices");
            return None;
        }
    };
    let tz = service_tz(service);
    let mut said: Vec<String> = page
        .messages
        .iter()
        .filter(|m| m.at > since)
        .filter(|m| m.from == Author::User && m.author.kind == SpeakerKind::You)
        .filter(|m| !crate::desk_surface::hub_note(m.text.trim()) && !m.text.trim().is_empty())
        .map(|m| {
            format!(
                "- {} {}",
                m.at.with_timezone(&tz).format("%H:%M"),
                crew::clip(&m.text, DM_WORD_CHARS)
            )
        })
        .collect();
    let from = said.len().saturating_sub(DM_WORDS);
    said.drain(..from);
    (!said.is_empty()).then(|| {
        format!(
            "What the person said in your DM since your last turn here (each was answered there \
             already; take them as their answers and orders, and pass on what a session waits for):\n{}",
            reported(&said.join("\n"), "the person's DM")
        )
    })
}

/// The person's time zone, as the boss's Desk keeps it.
fn service_tz(service: &AgentService) -> chrono_tz::Tz {
    super::for_service(service).map_or(chrono_tz::UTC, |c| c.desk.timezone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queue_is_claimed_once_and_not_before_its_time() {
        let mut q = Queue::default();
        assert!(!claim(&mut q), "nothing waits");
        q.pending.push_back(Notice::new("one"));
        assert!(claim(&mut q));
        assert!(!claim(&mut q), "its runner is on it");
        q.running = false;
        q.not_before = Some(Instant::now() + Duration::from_secs(60));
        assert!(!claim(&mut q), "not before its time");
        q.not_before = Some(Instant::now());
        assert!(claim(&mut q), "its time came");
        assert!(q.not_before.is_none());
    }
}
