//! Keeping the overseer in step with its config (its tools, its summary
//! jobs, one overseer at a time), and waking it: what happened on the crew
//! becomes a turn in its Desk DM — something waiting on the person too
//! long, work gone quiet, work finished or failed — gathered for a minute
//! into one check-in, at most so many a day, never about its own doing,
//! and held to the rooms' loop guard.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use workforce_common::oversight::{Level, Oversight};
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

/// The `desk.*` tools that act on sessions: the overseer acts through its
/// gated `crew.*` tools instead.
const DESK_ACTIONS: [&str; 4] = ["desk.start", "desk.prompt", "desk.approve", "desk.stop"];

const MORNING_JOB: &str = "crew-summary-morning";
const EVENING_JOB: &str = "crew-summary-evening";

/// One thing worth waking the overseer for.
#[derive(Debug, Clone)]
pub(super) struct Trigger {
    pub line: String,
    pub approvals: Vec<MentionedApproval>,
    pub hop: i16,
}

fn tracked(o: &Oversight, subject: &Party) -> bool {
    match subject.kind {
        PartyKind::Agent => o.track.crew,
        PartyKind::Session => match subject.origin {
            Some(Origin::External) => o.track.external_sessions,
            _ => o.track.desk_sessions,
        },
        PartyKind::Manager | PartyKind::You => false,
    }
}

fn who(p: &Party) -> String {
    match (p.kind, p.engine) {
        (PartyKind::Session, Some(engine)) => format!(
            "{} ({} session {}{})",
            p.name,
            match engine {
                workforce_desk::model::Engine::Claude => "Claude Code",
                workforce_desk::model::Engine::Codex => "Codex",
            },
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

impl CrewHub {
    /// Keeps tools, summaries and the one-overseer rule in step with the
    /// agents' configs.
    pub(super) async fn sync(&self) {
        let enabled: Vec<Arc<AgentService>> = self
            .services()
            .into_iter()
            .filter(|s| self.oversight_of(s).enabled)
            .collect();
        let current = self.overseer_service();
        if enabled.len() > 1 {
            if let Some((keep, _)) = &current {
                crate::agent_config_write::one_overseer(keep, self.state.def_store.as_deref())
                    .await;
                self.read.lock().clear();
            }
        }
        let current = self.overseer_service();
        let want = current.as_ref().map(|(s, o)| {
            (
                s.agent_id.as_str().to_string(),
                o.level.as_str().to_string(),
            )
        });
        let had = self.tooled.lock().clone();
        if want != had {
            for s in self.services() {
                for name in READ.iter().chain(ACT.iter()) {
                    s.tools.unregister(name);
                }
                crate::desk_driver::register_tools(&s);
            }
            if let Some((s, o)) = &current {
                for name in DESK_ACTIONS {
                    s.tools.unregister(name);
                }
                for tool in CrewTool::for_level(&self.me(), o.level == Level::Watch) {
                    s.tools.register(Arc::new(tool));
                }
                tracing::info!(agent_id = %s.agent_id, level = o.level.as_str(), "Oversees the crew");
            }
            *self.tooled.lock() = want;
        }
        for s in self.services() {
            let o = current
                .as_ref()
                .filter(|(c, _)| c.agent_id == s.agent_id)
                .map(|(_, o)| o.clone());
            self.sync_summaries(&s, o.as_ref()).await;
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
        let wanted = match event.kind {
            Kind::Finished | Kind::Question => o.wake.on_finish,
            Kind::Failed | Kind::Limit => o.wake.on_failure,
            _ => false,
        };
        wanted.then(|| Trigger {
            line: format!("{} {}", who(subject), event.text),
            approvals: Vec::new(),
            hop: event.hop,
        })
    }

    /// What has waited on the person, or gone quiet, past the overseer's
    /// thresholds; each once (keys already in `noted` are left out, and
    /// `noted` keeps only what still holds).
    pub(super) async fn thresholds(
        &self,
        overseer: &str,
        o: &Oversight,
        noted: &mut HashSet<String>,
    ) -> Vec<Trigger> {
        let Ok(overview) = self.desk.overview(self).await else {
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
            let (key, trigger) = match item.state {
                ItemState::NeedsYou if past(item.since, o.wake.waiting_minutes) => {
                    let waited = item
                        .since
                        .map(|s| (now - s).num_minutes().max(1))
                        .unwrap_or(1);
                    let (asks, mark, approvals) = match (&item.approval, &item.question) {
                        (Some(a), _) => (
                            format!("{} (approval {})", a.summary, a.id),
                            a.id.clone(),
                            vec![MentionedApproval {
                                session_id: item.id.clone(),
                                approval_id: a.id.clone(),
                                summary: a.summary.clone(),
                            }],
                        ),
                        (None, Some(q)) => (format!("asks “{}”", q.text), q.text.clone(), vec![]),
                        (None, None) => ("is waiting on you".into(), String::new(), vec![]),
                    };
                    (
                        format!("waiting:{}:{mark}", item.id),
                        Trigger {
                            line: format!(
                                "{} has waited on you for {waited} min: {asks}",
                                who(&party)
                            ),
                            approvals,
                            hop: 0,
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
                        },
                    )
                }
                _ => continue,
            };
            holding.insert(key.clone());
            if noted.insert(key) {
                out.push(trigger);
            }
        }
        noted.retain(|k| holding.contains(k));
        out
    }

    fn level_hint(level: Level) -> &'static str {
        match level {
            Level::Watch => "You only watch: don't act; say what you would do if it helps.",
            Level::Suggest => "Use the crew.* tools to propose what to do; each becomes a suggestion the person can do or dismiss right beside your message. Give a reason each time.",
            Level::Act => "You may act with the crew.* tools within what the person allowed; anything else becomes a suggestion they can do or dismiss. Give a reason each time.",
        }
    }

    /// Wakes the overseer with `triggers`, as one check-in.
    pub(super) async fn wake(&self, triggers: Vec<Trigger>) {
        let Some((service, o)) = self.overseer_service() else {
            return;
        };
        let Some(owner) = self.person(Some(&service)) else {
            tracing::info!("the overseer has no Desk conversation to check in on yet");
            return;
        };
        let lines: Vec<String> = triggers.iter().map(|t| format!("- {}", t.line)).collect();
        let prompt = format!(
            "{CHECK_IN} Here is what happened on the crew since you last looked (you oversee it at level {}):\n{}\n\n\
             Tell the person briefly, in plain words, what needs them and what you did or suggest — what needs \
             them first. Name sessions and agents as above. {}",
            o.level.as_str(),
            lines.join("\n"),
            Self::level_hint(o.level),
        );
        let hop = triggers.iter().map(|t| t.hop).max().unwrap_or(0);
        let approvals: Vec<MentionedApproval> =
            triggers.into_iter().flat_map(|t| t.approvals).collect();
        self.check_in(&service, &owner, "wake", &prompt, hop, Some(approvals))
            .await;
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
        *self.wake_hop.lock() = Some(hop);
        let ran = self
            .run_dm_turn(service, owner, prompt, hop, listed, None)
            .await;
        *self.wake_hop.lock() = None;
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
        let (overseer, o) = self
            .overseer_service()
            .ok_or("no agent oversees the crew")?;
        if overseer.agent_id != service.agent_id {
            return Err("this agent no longer oversees the crew".into());
        }
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
            .events_since(since, &[], 150)
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
            if lines.is_empty() { "- nothing recorded".to_string() } else { lines.join("\n") },
            Self::level_hint(o.level),
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

    /// Whether the overseer may check in again today.
    async fn under_cap(&self, o: &Oversight) -> bool {
        match self.desk.store().wake_ups_since(self.midnight()).await {
            Ok(n) => n < i64::from(o.max_checkins_per_day),
            Err(e) => {
                tracing::warn!(error = %e, "counting today's check-ins failed");
                false
            }
        }
    }
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

/// Turns what happens into the overseer's check-ins.
pub(super) async fn wake_loop(crew: Arc<CrewHub>, cancel: CancellationToken) {
    let (_, mut events) = crew.desk.subscribe();
    let mut tick = tokio::time::interval(crew.timing.tick.min(crew.timing.coalesce));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: Vec<Trigger> = Vec::new();
    let mut window: Option<Instant> = None;
    let mut noted: HashSet<String> = HashSet::new();
    let mut firing: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let mut found = Vec::new();
        tokio::select! {
            () = cancel.cancelled() => return,
            received = events.recv() => match received {
                Ok(DeskEvent::Activity { event }) => {
                    if let Some((s, o)) = crew.overseer_service() {
                        found.extend(crew.trigger_for(&event, s.agent_id.as_str(), &o));
                    }
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                if let Some((s, o)) = crew.overseer_service() {
                    found.extend(crew.thresholds(s.agent_id.as_str(), &o, &mut noted).await);
                }
            }
        }
        if !found.is_empty() {
            window.get_or_insert_with(Instant::now);
            pending.extend(found);
        }
        if firing.as_ref().is_some_and(|f| f.is_finished()) {
            firing = None;
        }
        let due = window.is_some_and(|w| w.elapsed() >= crew.timing.coalesce);
        if !due || firing.is_some() || pending.is_empty() {
            continue;
        }
        window = None;
        let triggers = std::mem::take(&mut pending);
        let Some((_, o)) = crew.overseer_service() else {
            continue;
        };
        if !crew.under_cap(&o).await {
            tracing::info!("the overseer has checked in as often as it may today");
            continue;
        }
        let crew = Arc::clone(&crew);
        firing = Some(tokio::spawn(async move { crew.wake(triggers).await }));
    }
}
