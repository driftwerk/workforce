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
use std::time::Instant;

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_desk::crew;
use workforce_desk::CheckIn;

use workforce_desk::crew::MentionedApproval;

use super::watcher::{check_in_channel, reported, Budget, Trigger, SWEEP};
use super::{name_of, CrewHub, CHECK_IN};
use crate::serve::AgentService;

/// The most notices held for one conversation: past it the oldest are
/// folded into one line.
const MAX_HELD: usize = 200;

/// What a notices turn is kept as among the boss's check-ins: the ceiling
/// counts it as it counts a wake-up.
pub(crate) const NOTICES_KIND: &str = "notices";

/// How many runs in a row may find a boss not overseeing (a config
/// reloading) before its notices are let go, a crew minute apart.
const MAX_MISSES: u32 = 5;

/// The longest a notices turn runs before it is let go as failed, in the
/// crew's minutes: its notices are tried again, and the sweep comes back.
const TURN_MINUTES: u32 = 30;

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
    crew.watch (a note of what you'll do when it ends) rather than waiting to be told; when its \
    review is to be done, start a review session with crew.start_session and review_of (the \
    repo and number), and once it has ended record its verdict with crew.record_review; a \
    clean review at the pull request's current head ends review. When a pull request is green \
    with a clean review at its head, merge it with crew.github_merge, move its ticket to done \
    and start the next ticket; Desk closes the finished session. When its CI is red, tell its \
    session to fix it and push. A ticket in Doing whose session is gone is restarted; an \
    answer the person already gave (their words below) is passed on, never asked again. Never \
    hold a session without a directive that needs it: a hold names its directive and what ends \
    it, and its question goes to the person at once with crew.ask_person; a hold stops new \
    work, never its reading (CI, checks, logs). Say who moves each wait on next. \
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
    /// How many hand-offs it came by: the turn it is in is one more, so a
    /// chain between the boss and an agent ends at `MAX_HOPS`.
    pub hop: i16,
    /// The requests it names, beside it as a wake-up's are.
    pub approvals: Vec<MentionedApproval>,
    /// Desk's own look (a reminder, the sweep, a check-in tried again), not
    /// something that happened: it never lifts a turn the person stopped.
    pub routine: bool,
}

impl Notice {
    pub(super) fn new(text: impl Into<String>) -> Self {
        Self {
            at: Utc::now(),
            text: text.into(),
            watch: None,
            again: false,
            hop: 0,
            approvals: Vec::new(),
            routine: false,
        }
    }

    /// What the wake loop found, `t`, as a notice.
    pub(super) fn of(t: Trigger) -> Self {
        Self {
            routine: t.routine || t.line == SWEEP,
            hop: t.hop,
            approvals: t.approvals,
            ..Self::new(t.line)
        }
    }

    /// As the wake loop takes it, when it runs the old way.
    fn into_trigger(self) -> Trigger {
        Trigger {
            hop: self.hop,
            approvals: self.approvals,
            routine: self.routine,
            ..Trigger::line(self.text)
        }
    }
}

/// Why a conversation's notices wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// The person stopped its last turn: they wait for something new to
    /// happen (never Desk's own look), and are never tried again by
    /// themselves.
    Stopped,
    /// Not before then: a failed turn, Desk's breaker, a spent ceiling.
    Until(Instant),
}

/// One conversation's notices.
#[derive(Debug, Default)]
struct Queue {
    pending: VecDeque<Notice>,
    /// Its runner is on it.
    running: bool,
    /// Why what waits doesn't go yet.
    hold: Option<Hold>,
    /// Failed turns in a row.
    failures: u32,
    /// Its last turn that ran, and when it began: the person's DM words
    /// are read from then on.
    last_turn: Option<(Instant, DateTime<Utc>)>,
    /// A notice folded away for room, counted.
    folded: usize,
    /// The spent ceiling the person was told of, so it is said once.
    told_paused: Option<DateTime<Utc>>,
    /// Runs in a row that found its boss not overseeing.
    misses: u32,
}

impl Queue {
    /// Adds `notice`. Past [`MAX_HELD`] the oldest plain one is folded away
    /// for room: a watch's end never is, as nothing else tells it.
    fn push(&mut self, notice: Notice) {
        if self.pending.len() >= MAX_HELD {
            if let Some(at) = self.pending.iter().position(|n| n.watch.is_none()) {
                self.pending.remove(at);
                self.folded += 1;
            }
        }
        // Something new to act on lifts a Stop; Desk's own look doesn't.
        if !notice.routine && self.hold == Some(Hold::Stopped) {
            self.hold = None;
        }
        self.pending.push_back(notice);
    }
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
        local_boss(boss)
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

    /// Whether `boss`'s oversight conversation has had a turn in the last
    /// [`SWEEP_AFTER_QUIET_MINUTES`], runs one, or was stopped by the
    /// person: then a sweep has nothing to add.
    pub(super) fn noticed_lately(&self, boss: &str) -> bool {
        let quiet = self.timing.minute * SWEEP_AFTER_QUIET_MINUTES;
        self.notices.0.lock().iter().any(|((b, _), q)| {
            b == boss
                && (q.running
                    || q.hold == Some(Hold::Stopped)
                    || q.last_turn.is_some_and(|(at, _)| at.elapsed() < quiet))
        })
    }

    /// Tells `boss`'s oversight conversation with `person` of `notice`; its
    /// turn starts now unless one runs, or it is held.
    pub(super) fn notify(self: &Arc<Self>, boss: &str, person: &str, notice: Notice) {
        let key = (boss.to_string(), person.to_string());
        let start = {
            let mut all = self.notices.0.lock();
            let q = all.entry(key.clone()).or_default();
            q.push(notice);
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
            all.retain(|_, q| {
                !q.pending.is_empty() || q.running || q.last_turn.is_some() || q.hold.is_some()
            });
            all.iter_mut()
                .filter_map(|(k, q)| claim(q).then(|| k.clone()))
                .collect()
        };
        for key in due {
            tokio::spawn(Arc::clone(self).run_notices(key));
        }
    }

    /// Puts `batch` back at the front of `key`'s queue, held as `hold`, and
    /// lets its runner go.
    fn hold_back(&self, key: &(String, String), batch: Vec<Notice>, hold: Option<Hold>) {
        let mut all = self.notices.0.lock();
        let q = all.entry(key.clone()).or_default();
        for n in batch.into_iter().rev() {
            q.pending.push_front(n);
        }
        q.running = false;
        q.hold = hold;
    }

    /// The watches `batch` tells, kept as told: none is told again, here or
    /// after a restart. Only once Desk has kept that are they let go here,
    /// or the next look would tell them again.
    async fn delivered(&self, batch: &[Notice]) {
        let watches: Vec<Uuid> = batch.iter().filter_map(|n| n.watch).collect();
        if watches.is_empty() {
            return;
        }
        match self.desk.store().watches_delivered(&watches).await {
            Ok(()) => self.watches_told(&watches),
            // Kept in flight: not told again here; a restart may.
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the told watches weren't marked told")
            }
        }
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
                // Gone for a moment (a config reloading) or for good: held
                // a while, then, with no one left to tell, let go and its
                // watches' ends kept as told.
                let misses = {
                    let mut all = self.notices.0.lock();
                    let q = all.entry(key.clone()).or_default();
                    q.misses += 1;
                    q.misses
                };
                if misses < MAX_MISSES && self.state.registry.get(boss).is_some() {
                    self.hold_back(
                        &key,
                        batch,
                        Some(Hold::Until(Instant::now() + self.timing.minute)),
                    );
                    return;
                }
                tracing::info!(%boss, held = batch.len(), "notices for a boss that no longer oversees are let go");
                self.delivered(&batch).await;
                continue;
            };
            {
                let mut all = self.notices.0.lock();
                all.entry(key.clone()).or_default().misses = 0;
            }
            // Act fully ended, or isn't in force (yet: today's spend is
            // read after a restart): the wake loop takes them the old way,
            // as a check-in, with all it holds a check-in to. A watch's end
            // handed on is kept as told there.
            if !self.orchestrates(boss) {
                self.delivered(&batch).await;
                let triggers: Vec<Trigger> = batch.into_iter().map(Notice::into_trigger).collect();
                if self.wake_back.0.send((boss.to_string(), triggers)).is_err() {
                    tracing::warn!(%boss, "the wake loop is gone: notices for a boss not at Act fully were let go");
                }
                continue;
            }
            // Held by Desk's breaker: its notices wait for it to let go.
            if self.desk.agent_held(boss).is_some() {
                self.hold_back(
                    &key,
                    batch,
                    Some(Hold::Until(Instant::now() + self.timing.minute)),
                );
                return;
            }
            match self.budget(boss, &o).await {
                Ok(Budget::Open) => {}
                Ok(Budget::Spent { until, daily }) => {
                    let tell = {
                        let mut all = self.notices.0.lock();
                        let q = all.entry(key.clone()).or_default();
                        let tell = q.told_paused != Some(until);
                        q.told_paused = Some(until);
                        tell
                    };
                    if tell {
                        self.paused(&service, until, daily);
                    }
                    let wait = (until - Utc::now()).to_std().unwrap_or(self.timing.minute);
                    self.hold_back(&key, batch, Some(Hold::Until(Instant::now() + wait)));
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "counting the boss's turns failed");
                    self.hold_back(
                        &key,
                        batch,
                        Some(Hold::Until(Instant::now() + self.timing.minute)),
                    );
                    return;
                }
            }
            let began = Utc::now();
            let prompt = self
                .notices_prompt(&service, &o, person, &batch, folded, since)
                .await;
            let hop = batch.iter().map(|n| n.hop).max().unwrap_or(0);
            let approvals: Vec<MentionedApproval> =
                batch.iter().flat_map(|n| n.approvals.clone()).collect();
            let turn = self.notices_turn(&service, person, &prompt, hop, approvals);
            tokio::pin!(turn);
            // Past its time it is stopped as the person's Stop stops it and
            // waited for to its end, never left running beside the next.
            // Not running yet (waiting on its mind), it is looked at again
            // after as long once more.
            let mut stopped = false;
            let ran = loop {
                tokio::select! {
                    ran = &mut turn => break ran,
                    () = tokio::time::sleep(self.timing.minute * TURN_MINUTES), if !stopped => {
                        use crate::hub_core::live_turns::{Applied, Control};
                        let live = match service.sessions.find(&check_in_channel(boss), person) {
                            Ok(Some(s)) => service.live_turns.apply(&s.id, Control::Cancel).await,
                            _ => Applied::NotLive,
                        };
                        stopped = !matches!(live, Applied::NotLive);
                    }
                }
            };
            // A turn that came through stands; one stopped for its time is a
            // failure, never the person's Stop.
            let ran = match ran {
                Err(Ran::Stopped) if stopped => Err(Ran::Failed(format!(
                    "it ran past {TURN_MINUTES} minutes and was stopped"
                ))),
                ran => ran,
            };
            match ran {
                Ok(()) => {
                    self.delivered(&batch).await;
                    let mut all = self.notices.0.lock();
                    let q = all.entry(key.clone()).or_default();
                    q.failures = 0;
                    q.told_paused = None;
                    q.last_turn = Some((Instant::now(), began));
                }
                Err(Ran::Stopped) => {
                    // The person's Stop: kept for what happens next, never
                    // tried again by itself, and not a failure.
                    tracing::info!(%boss, held = batch.len(), "the boss's notices turn was stopped; its notices wait for what comes next");
                    // What happened while it ran is new: that goes now.
                    let fresh = {
                        let mut all = self.notices.0.lock();
                        let q = all.entry(key.clone()).or_default();
                        q.failures = 0;
                        let fresh = q.pending.iter().any(|n| !n.routine);
                        if fresh {
                            for n in batch.iter().rev() {
                                q.pending.push_front(n.clone());
                            }
                        }
                        fresh
                    };
                    if fresh {
                        continue;
                    }
                    self.hold_back(&key, batch, Some(Hold::Stopped));
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
                    self.hold_back(
                        &key,
                        again,
                        Some(Hold::Until(Instant::now() + self.timing.minute * minutes)),
                    );
                    return;
                }
            }
        }
    }

    /// Runs `prompt` in `service`'s oversight conversation with `person`,
    /// at `hop` hand-offs with `approvals` beside it, kept as a check-in of
    /// its own kind (the ceiling counts it), under its mind.
    async fn notices_turn(
        &self,
        service: &Arc<AgentService>,
        person: &str,
        prompt: &str,
        hop: i16,
        approvals: Vec<MentionedApproval>,
    ) -> Result<(), Ran> {
        let id = service.agent_id.as_str().to_string();
        let mut record = CheckIn {
            id: crew::new_id(),
            at: crew::now(),
            agent_id: id.clone(),
            kind: NOTICES_KIND.into(),
            prompt: prompt.to_string(),
            reply: None,
        };
        if let Err(e) = self.desk.store().put_check_in(&record).await {
            tracing::warn!(error = %e, "recording the boss's notices turn failed");
        }
        self.yield_to_person(&id).await;
        let ran = self
            .with_mind(&id, "its notices", async {
                // Its crew calls are one hand-off on from what woke it.
                self.wake_hop.lock().insert(id.clone(), hop);
                // Its notices are plan turns: judged at no less than the floor.
                let ran = crate::agent_turn::with_effort_floor(
                    super::decide::EFFORT_FLOOR,
                    self.run_turn(
                        service,
                        person,
                        &check_in_channel(&id),
                        prompt,
                        hop,
                        Some(approvals),
                        None,
                    ),
                )
                .await;
                self.wake_hop.lock().remove(&id);
                ran
            })
            .await;
        match ran {
            Ok((_, reply)) => {
                record.reply = Some(reply);
                if let Err(e) = self.desk.store().put_check_in(&record).await {
                    tracing::warn!(error = %e, "recording the boss's notices turn's answer failed");
                }
                Ok(())
            }
            Err(e) if super::deliver::stopped(&e) => Err(Ran::Stopped),
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
        out.push(' ');
        out.push_str(super::goal::SAVE_GOALS);
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

/// Whether `boss` is this hub's own: never one served from another hub's
/// grant, nor one relayed from its home, whose crew calls are decided where
/// their sessions are.
fn local_boss(boss: &str) -> bool {
    !workforce_desk::grant::is_served(boss) && !super::relayed_decide::relayed_party(boss)
}

/// Whether a runner takes `q` now: notices wait, none runs, and nothing
/// holds them. Taken, it is marked running.
fn claim(q: &mut Queue) -> bool {
    let free = match q.hold {
        None => true,
        Some(Hold::Stopped) => false,
        Some(Hold::Until(at)) => Instant::now() >= at,
    };
    let go = free && !q.running && !q.pending.is_empty();
    if go {
        q.running = true;
        q.hold = None;
    }
    go
}

/// How a notices turn went.
enum Ran {
    /// The person stopped it.
    Stopped,
    Failed(String),
}

/// The person's own messages in `service`'s DM with `person` since
/// `since`, oldest first, among its newest `look` messages: read from the
/// DM itself, so none is lost to a rotation or a compaction. Desk's own
/// notes in it are not theirs.
pub(super) fn person_messages(
    service: &AgentService,
    person: &str,
    since: DateTime<Utc>,
    look: usize,
) -> Vec<(DateTime<Utc>, String)> {
    use workforce_desk::conversations::{Author, SpeakerKind};
    let speaker = workforce_desk::Speaker::agent(service.agent_id.as_str(), &name_of(service));
    let page = match crate::desk_conversations::page(
        service.sessions.as_ref(),
        &speaker,
        person,
        workforce_common::channels::DESK_CHANNEL,
        workforce_desk::Cursor::Newest,
        look,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(error = %e, "the person's DM words couldn't be read");
            return Vec::new();
        }
    };
    page.messages
        .into_iter()
        .filter(|m| m.at > since)
        .filter(|m| m.from == Author::User && m.author.kind == SpeakerKind::You)
        .filter(|m| !crate::desk_surface::hub_note(m.text.trim()) && !m.text.trim().is_empty())
        .map(|m| (m.at, m.text))
        .collect()
}

/// When `service` last replied in its DM with `person` in a turn of its
/// own, among the `look` newest messages there: Desk's posts by it (plan
/// cards, updates, notices, a turn's failure) aren't its words to answer.
pub(super) fn boss_last_said(
    service: &AgentService,
    person: &str,
    look: usize,
) -> Option<DateTime<Utc>> {
    use workforce_desk::conversations::Author;
    let speaker = workforce_desk::Speaker::agent(service.agent_id.as_str(), &name_of(service));
    match crate::desk_conversations::page(
        service.sessions.as_ref(),
        &speaker,
        person,
        workforce_common::channels::DESK_CHANNEL,
        workforce_desk::Cursor::Newest,
        look,
    ) {
        Ok(page) => page
            .messages
            .into_iter()
            .filter(|m| m.from == Author::Agent && !m.text.trim().is_empty())
            .filter(|m| {
                m.mark
                    .as_ref()
                    .is_none_or(|k| k.delivery.is_none() && !k.failed)
            })
            .map(|m| m.at)
            .max(),
        Err(e) => {
            tracing::info!(error = %e, "the boss's DM couldn't be read");
            None
        }
    }
}

/// The threads read for the person's words, at most: the ones last
/// written to.
const THREADS_READ: usize = 20;

/// The person's own messages in the threads of `service`'s DM with
/// `person` written to since `since`, newer than it: their replies there,
/// as [`person_messages`] reads the DM itself.
pub(super) fn person_thread_messages(
    service: &AgentService,
    person: &str,
    since: DateTime<Utc>,
    look: usize,
) -> Vec<(DateTime<Utc>, String)> {
    use workforce_desk::conversations::{Author, SpeakerKind};
    let store = service.sessions.as_ref();
    // Threads' channels are `desk:thread:<message id>`, all under one prefix.
    let threads_on = workforce_core::desk::DESK_THREAD_PREFIX.trim_end_matches(':');
    let mut threads: Vec<_> = match store.list_for_owner_on(person, threads_on, 2_000) {
        Ok(all) => all
            .into_iter()
            .filter(|s| {
                !s.is_sealed()
                    && s.updated_at > since
                    && workforce_core::desk::dm_thread_parent(&s.channel).is_some()
            })
            .collect(),
        Err(e) => {
            tracing::info!(error = %e, "the person's DM threads couldn't be listed");
            return Vec::new();
        }
    };
    threads.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    let speaker = workforce_desk::Speaker::agent(service.agent_id.as_str(), &name_of(service));
    let mut out = Vec::new();
    for head in threads.into_iter().take(THREADS_READ) {
        let page = match crate::desk_conversations::page(
            store,
            &speaker,
            person,
            &head.channel,
            workforce_desk::Cursor::Newest,
            look,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "a DM thread's words couldn't be read");
                continue;
            }
        };
        out.extend(
            page.messages
                .into_iter()
                .filter(|m| m.at > since)
                .filter(|m| m.from == Author::User && m.author.kind == SpeakerKind::You)
                .filter(|m| {
                    !crate::desk_surface::hub_note(m.text.trim()) && !m.text.trim().is_empty()
                })
                .map(|m| (m.at, m.text)),
        );
    }
    out
}

/// The person's own words in `service`'s DM with `person` since `since`
/// (else the last [`DM_FIRST_LOOK_HOURS`]): their answers and orders.
fn dm_words(service: &AgentService, person: &str, since: Option<DateTime<Utc>>) -> Option<String> {
    let since = since.unwrap_or_else(|| Utc::now() - chrono::Duration::hours(DM_FIRST_LOOK_HOURS));
    let tz = service_tz(service);
    let mut said: Vec<String> = person_messages(service, person, since, DM_WORDS * 3)
        .iter()
        .map(|(at, text)| {
            format!(
                "- {} {}",
                at.with_timezone(&tz).format("%H:%M"),
                crew::clip(text, DM_WORD_CHARS)
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
    use std::time::Duration;

    #[test]
    fn a_queue_is_claimed_once_and_not_before_its_time() {
        let mut q = Queue::default();
        assert!(!claim(&mut q), "nothing waits");
        q.push(Notice::new("one"));
        assert!(claim(&mut q));
        assert!(!claim(&mut q), "its runner is on it");
        q.running = false;
        q.hold = Some(Hold::Until(Instant::now() + Duration::from_secs(60)));
        assert!(!claim(&mut q), "not before its time");
        q.push(Notice::new("two"));
        assert!(!claim(&mut q), "something new doesn't lift a wait for time");
        q.hold = Some(Hold::Until(Instant::now()));
        assert!(claim(&mut q), "its time came");
        assert!(q.hold.is_none());
    }

    #[test]
    fn a_stop_holds_until_something_happens_and_the_sweep_never_lifts_it() {
        let mut q = Queue::default();
        q.push(Notice::new("one"));
        q.hold = Some(Hold::Stopped);
        assert!(!claim(&mut q), "held by the person's Stop");
        let mut sweep = Notice::new(SWEEP);
        sweep.routine = true;
        q.push(sweep);
        let mut reminder = Notice::new("web has waited on you for 30 min");
        reminder.routine = true;
        q.push(reminder);
        assert!(!claim(&mut q), "neither the sweep nor a reminder lifts it");
        q.push(Notice::new("a session finished its turn"));
        assert!(claim(&mut q), "something new does");
    }

    #[test]
    fn past_the_most_held_a_plain_notice_is_folded_and_a_watch_end_never() {
        let mut q = Queue::default();
        let mut ended = Notice::new("your watch ended");
        ended.watch = Some(Uuid::new_v4());
        q.push(ended);
        for i in 0..MAX_HELD {
            q.push(Notice::new(format!("n{i}")));
        }
        assert_eq!(q.pending.len(), MAX_HELD);
        assert_eq!(q.folded, 1);
        assert!(q.pending.iter().any(|n| n.watch.is_some()), "kept");
        assert_eq!(q.pending[1].text, "n1", "the oldest plain one went");
    }

    #[test]
    fn a_trigger_keeps_its_hops_and_requests_as_a_notice_and_back() {
        let mut t = Trigger::line("an agent answered".into());
        t.hop = 3;
        t.approvals = vec![MentionedApproval {
            session_id: "s".into(),
            approval_id: "a".into(),
            summary: "run it".into(),
        }];
        let n = Notice::of(t);
        assert_eq!(n.hop, 3);
        assert_eq!(n.approvals.len(), 1);
        assert!(!n.routine);
        let back = n.into_trigger();
        assert_eq!((back.hop, back.approvals.len()), (3, 1));
        assert!(Notice::of(Trigger::line(SWEEP.into())).routine);
        let mut reminder = Trigger::line("web has waited on you".into());
        reminder.routine = true;
        assert!(Notice::of(reminder).into_trigger().routine);
    }

    #[test]
    fn only_this_hubs_own_boss_can_run_on_notices() {
        assert!(local_boss("desk"));
        assert!(!local_boss("acme-hub/desk"), "served from a grant");
        assert!(
            !local_boss(&format!("{}/desk", workforce_desk::grant::HOME_HUB)),
            "relayed from its home"
        );
    }
}
