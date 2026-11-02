//! Gists written as turns run. Every op the log's writer applied is seen
//! here; a subject's gist is written through the hub's model when
//! [`gist::due`] says, off the writer's path, and its owner's floor told it
//! changed. The floor's paragraph is written from the gists when a read
//! finds it out of date. Without a model nothing is written: the floor says
//! each gist plainly from the turns.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::Instant;
use tracing::{debug, warn};
use uuid::Uuid;
use workforce_desk::crew;
use workforce_desk::floor::{Outcome, Place, Scope, Subject, SubjectKey, Turn, TurnOp};
use workforce_desk::gist::{self, Asked, Doing, Gist, Happening, Moment, Paragraph, Reach};
use workforce_desk::model::Engine;

use super::postgres::PgFloorStore;

/// Ops seen but not yet read before new ones are dropped.
const BACKLOG: usize = 1024;
/// How many gists are written at once.
const CALLS: usize = 4;
/// How many of a running subject's latest steps a gist is written from.
const STEPS: usize = 12;

/// What writes: the hub's cheap model, and what the hub knows of a subject
/// beyond its turns.
#[async_trait]
pub trait Model: Send + Sync {
    /// The model's answer to `system` and `user`, on `subject`'s account
    /// (none: the floor's own) for `owner`.
    async fn answer(
        &self,
        subject: Option<&Subject>,
        owner: Option<&str>,
        system: &str,
        user: &str,
    ) -> eyre::Result<String>;

    /// What a Desk session was started to do.
    fn goal(&self, subject: &Subject) -> Option<String>;

    /// Whether `subject` is a session the hub ran itself (a completion, an
    /// agent's turn), never the person's to say what it does.
    fn hub_internal(&self, subject: &Subject) -> bool;
}

/// Where gists and paragraphs are kept.
#[async_trait]
pub trait Kept: Send + Sync {
    async fn gist(&self, owner: Option<&str>, subject: &SubjectKey) -> eyre::Result<Option<Gist>>;
    async fn put_gist(
        &self,
        owner: Option<&str>,
        subject: &SubjectKey,
        gist: &Gist,
    ) -> eyre::Result<()>;
    /// What `subject`'s purpose is read from.
    async fn asks(
        &self,
        owner: Option<&str>,
        subject: &SubjectKey,
        reach: &Reach,
    ) -> eyre::Result<Asked>;
    async fn turn(&self, id: Uuid) -> eyre::Result<Option<Turn>>;
    async fn summary(&self, owner: &str, personal: bool) -> eyre::Result<Option<(Paragraph, i64)>>;
    async fn put_summary(
        &self,
        owner: &str,
        personal: bool,
        paragraph: &Paragraph,
        fingerprint: i64,
    ) -> eyre::Result<()>;
}

#[async_trait]
impl Kept for PgFloorStore {
    async fn gist(&self, owner: Option<&str>, subject: &SubjectKey) -> eyre::Result<Option<Gist>> {
        PgFloorStore::gist(self, owner, subject).await
    }

    async fn put_gist(
        &self,
        owner: Option<&str>,
        subject: &SubjectKey,
        gist: &Gist,
    ) -> eyre::Result<()> {
        PgFloorStore::put_gist(self, owner, subject, gist).await
    }

    async fn asks(
        &self,
        owner: Option<&str>,
        subject: &SubjectKey,
        reach: &Reach,
    ) -> eyre::Result<Asked> {
        PgFloorStore::asks(self, owner, subject, reach).await
    }

    async fn turn(&self, id: Uuid) -> eyre::Result<Option<Turn>> {
        PgFloorStore::turn(self, id).await
    }

    async fn summary(&self, owner: &str, personal: bool) -> eyre::Result<Option<(Paragraph, i64)>> {
        PgFloorStore::summary(self, owner, personal).await
    }

    async fn put_summary(
        &self,
        owner: &str,
        personal: bool,
        paragraph: &Paragraph,
        fingerprint: i64,
    ) -> eyre::Result<()> {
        PgFloorStore::put_summary(self, owner, personal, paragraph, fingerprint).await
    }
}

/// Whose subject: its owner (none, a personal hub's Desk session) and key.
type Whose = (Option<String>, SubjectKey);

/// Who is told an owner's floor changed.
type Told = Arc<dyn Fn(Option<&str>) + Send + Sync>;

enum Seen {
    Op(TurnOp),
    /// A subject's steps not yet said may be said now.
    Due(Whose),
}

/// The gists' writer, as the log's writer and the floor's reads use it.
#[derive(Clone)]
pub struct Gists(Arc<Inner>);

struct Inner {
    tx: mpsc::Sender<Seen>,
    kept: Arc<dyn Kept>,
    model: OnceLock<Arc<dyn Model>>,
    told: Told,
    /// One write at a time per subject, so each builds on the last.
    writing: Mutex<HashMap<Whose, Arc<tokio::sync::Mutex<()>>>>,
    calls: Semaphore,
    /// When each person's paragraph was last tried.
    tried: Mutex<HashMap<(String, bool), Instant>>,
}

/// A subject as its running turn has it.
struct Running {
    subject: Subject,
    name: String,
    turn: Uuid,
    place: Place,
    ask: Option<String>,
    by_person: bool,
    steps: VecDeque<String>,
    /// Steps since its gist was last written.
    unsaid: usize,
    last: Option<Instant>,
    deferred: bool,
    background: u32,
    open: usize,
}

/// One gist to write.
struct Job {
    whose: Whose,
    subject: Subject,
    name: String,
    turn: Uuid,
    place: Place,
    ask: Option<String>,
    steps: Vec<String>,
    reply: Option<String>,
    doing: Doing,
    moment: Moment,
}

impl Gists {
    /// Starts reading what the log applied; `told` is called with whose
    /// floor a written gist changed.
    pub fn start(kept: Arc<dyn Kept>, told: Told) -> Self {
        let (tx, rx) = mpsc::channel(BACKLOG);
        let gists = Self(Arc::new(Inner {
            tx,
            kept,
            model: OnceLock::new(),
            told,
            writing: Mutex::new(HashMap::new()),
            calls: Semaphore::new(CALLS),
            tried: Mutex::new(HashMap::new()),
        }));
        tokio::spawn(gists.clone().read(rx));
        gists
    }

    /// Writes through `model` from now on; the first one served stays.
    pub fn serve(&self, model: Arc<dyn Model>) {
        let _ = self.0.model.set(model);
    }

    /// The log applied `op`. Never waits: a full backlog drops it.
    pub fn saw(&self, op: &TurnOp) {
        let worth = match op {
            TurnOp::Begin { .. } | TurnOp::Background { .. } | TurnOp::End { .. } => true,
            TurnOp::Progress { doing, .. } => doing.is_some(),
            _ => false,
        };
        if worth && self.0.model.get().is_some() {
            let _ = self.0.tx.try_send(Seen::Op(op.clone()));
        }
    }

    async fn read(self, mut rx: mpsc::Receiver<Seen>) {
        let mut subjects: HashMap<Whose, Running> = HashMap::new();
        let mut turns: HashMap<Uuid, Whose> = HashMap::new();
        while let Some(seen) = rx.recv().await {
            match seen {
                Seen::Op(op) => self.op(op, &mut subjects, &mut turns),
                Seen::Due(whose) => {
                    if let Some(r) = subjects.get_mut(&whose) {
                        r.deferred = false;
                        self.step(&whose, r);
                    }
                }
            }
        }
    }

    fn op(
        &self,
        op: TurnOp,
        subjects: &mut HashMap<Whose, Running>,
        turns: &mut HashMap<Uuid, Whose>,
    ) {
        match op {
            // An agent's turn run as a Claude Code session is the agent's,
            // whose own turn has a gist.
            TurnOp::Begin { turn, .. }
                if matches!(turn.subject, Subject::Session { .. })
                    && gist::agent_turn_folder(&turn.name) => {}
            // Nor is a session the hub ran itself the person's.
            TurnOp::Begin { turn, .. }
                if self
                    .0
                    .model
                    .get()
                    .is_some_and(|m| m.hub_internal(&turn.subject)) => {}
            TurnOp::Begin { turn, by_person } => {
                let whose = (turn.owner.clone(), turn.subject.key());
                let r = subjects.entry(whose.clone()).or_insert_with(|| Running {
                    subject: turn.subject.clone(),
                    name: turn.name.clone(),
                    turn: turn.id,
                    place: turn.place.clone(),
                    ask: None,
                    by_person,
                    steps: VecDeque::new(),
                    unsaid: 0,
                    last: None,
                    deferred: false,
                    background: 0,
                    open: 0,
                });
                r.subject = turn.subject.clone();
                r.name = turn.name.clone();
                r.turn = turn.id;
                r.place = turn.place.clone();
                r.ask = turn.task.clone();
                r.by_person = by_person;
                r.steps.clear();
                r.background = 0;
                r.open += 1;
                turns.insert(turn.id, whose.clone());
                self.write(&whose, r, Moment::Began { by_person }, None);
            }
            TurnOp::Progress {
                turn,
                doing: Some(doing),
                ..
            } => {
                let Some(whose) = turns.get(&turn) else {
                    return;
                };
                let Some(r) = subjects.get_mut(whose) else {
                    return;
                };
                if r.steps.back() != Some(&doing) {
                    r.steps.push_back(doing);
                    if r.steps.len() > STEPS {
                        r.steps.pop_front();
                    }
                    r.unsaid += 1;
                }
                let whose = whose.clone();
                self.step(&whose, r);
            }
            TurnOp::Background { turn, count, .. } => {
                if let Some(r) = turns.get(&turn).and_then(|w| subjects.get_mut(w)) {
                    r.background = count;
                }
            }
            TurnOp::End {
                turn,
                outcome,
                answer,
                ..
            } => {
                if matches!(outcome, Outcome::Stopped | Outcome::Interrupted) {
                    if let Some(whose) = turns.remove(&turn) {
                        Self::closed(&whose, subjects);
                    }
                    return;
                }
                let Some(whose) = turns.remove(&turn) else {
                    // Begun before this hub started: what it was is logged.
                    let gists = self.clone();
                    tokio::spawn(async move { gists.ended_unseen(turn, outcome, answer).await });
                    return;
                };
                if let Some(r) = subjects.get_mut(&whose) {
                    r.turn = turn;
                    self.write(&whose, r, Moment::Ended, Some((outcome, answer)));
                }
                Self::closed(&whose, subjects);
            }
            _ => {}
        }
    }

    fn closed(whose: &Whose, subjects: &mut HashMap<Whose, Running>) {
        if let Some(r) = subjects.get_mut(whose) {
            r.open = r.open.saturating_sub(1);
            if r.open == 0 {
                subjects.remove(whose);
            }
        }
    }

    /// Writes a running subject's new steps when [`gist::due`] says, or
    /// once it will.
    fn step(&self, whose: &Whose, r: &mut Running) {
        match gist::due(Moment::Stepped, r.last.map(|at| at.elapsed()), r.unsaid) {
            gist::Due::Now => self.write(whose, r, Moment::Stepped, None),
            gist::Due::In(wait) if !r.deferred => {
                r.deferred = true;
                let (tx, whose) = (self.0.tx.clone(), whose.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    let _ = tx.send(Seen::Due(whose)).await;
                });
            }
            _ => {}
        }
    }

    fn write(
        &self,
        whose: &Whose,
        r: &mut Running,
        moment: Moment,
        ended: Option<(Outcome, Option<String>)>,
    ) {
        r.last = Some(Instant::now());
        r.unsaid = 0;
        let doing = match &ended {
            Some((outcome, _)) => doing_at_end(*outcome),
            None if r.background > 0 => Doing::Waiting(r.background),
            None => Doing::Working,
        };
        let job = Job {
            whose: whose.clone(),
            subject: r.subject.clone(),
            name: r.name.clone(),
            turn: r.turn,
            place: r.place.clone(),
            ask: r.ask.clone(),
            steps: r.steps.iter().cloned().collect(),
            reply: ended.and_then(|(_, answer)| answer),
            doing,
            moment,
        };
        let gists = self.clone();
        tokio::spawn(async move { gists.written(job).await });
    }

    async fn ended_unseen(&self, turn: Uuid, outcome: Outcome, answer: Option<String>) {
        let t = match self.0.kept.turn(turn).await {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(e) => {
                return warn!(target: "floor", error = %format!("{e:#}"), "reading a turn for its gist failed")
            }
        };
        self.written(Job {
            whose: (t.owner.clone(), t.subject.key()),
            subject: t.subject.clone(),
            name: t.name.clone(),
            turn,
            place: t.place.clone(),
            ask: t.task.clone(),
            steps: vec![],
            reply: answer,
            doing: doing_at_end(outcome),
            moment: Moment::Ended,
        })
        .await;
    }

    /// Writes `job`'s gist on what was kept: the model's, with the purpose
    /// it kept unless [`gist::purpose_due`]; when the model can't, the plain
    /// reading, except over a model's gist of a turn still running.
    async fn written(&self, job: Job) {
        let Some(model) = self.0.model.get().cloned() else {
            return;
        };
        let lock = Arc::clone(self.0.writing.lock().entry(job.whose.clone()).or_default());
        let _one = lock.lock().await;
        let Ok(_call) = self.0.calls.acquire().await else {
            return;
        };
        let (owner, key) = (job.whose.0.as_deref(), &job.whose.1);
        let kept = self.0.kept.gist(owner, key).await.unwrap_or_else(|e| {
            warn!(target: "floor", error = %format!("{e:#}"), "reading a gist failed");
            None
        });
        if let (Subject::Agent { .. }, Some(surface)) = (&job.subject, gist::elsewhere(&job.place))
        {
            // The person's Desk conversation alone gives an agent its
            // purpose. A turn from another surface changes only what the
            // agent is doing now, said without its content.
            let at = crew::now();
            let (purpose, purpose_at) = kept
                .as_ref()
                .and_then(|k| k.purpose.clone().map(|p| (p, k.purpose_at)))
                .unwrap_or_else(|| (gist::ELSEWHERE_PURPOSE.into(), None));
            let gist = Gist {
                purpose: Some(purpose),
                purpose_at,
                now: gist::elsewhere_now(surface, job.moment == Moment::Ended),
                now_at: at,
                turn: job.turn,
                by_model: false,
            };
            return match self.0.kept.put_gist(owner, key, &gist).await {
                Ok(()) => (self.0.told)(owner),
                Err(e) => {
                    warn!(target: "floor", error = %format!("{e:#}"), "keeping a gist failed")
                }
            };
        }
        let read_at = crew::now();
        // An agent's purpose is its current conversation's, read from its
        // latest asks; a session's is its one job's.
        let reach = match &job.subject {
            Subject::Agent { .. } => Reach::Thread {
                since: read_at - gist::THREAD_WITHIN,
            },
            Subject::Session { .. } => Reach::Job,
        };
        let Asked { asks, began } = self
            .0
            .kept
            .asks(owner, key, &reach)
            .await
            .unwrap_or_else(|e| {
                warn!(target: "floor", error = %format!("{e:#}"), "reading a subject's asks failed");
                Asked::default()
            });
        // With no Desk conversation in the window, the last purpose stands.
        // A purpose from before this thread is not offered to the model to
        // keep: it would only be written out again.
        let outdated = kept
            .as_ref()
            .and_then(|k| k.purpose_at.zip(began))
            .is_some_and(|(written, began)| written < began);
        // The neutral line (no time) is no purpose to keep.
        let offered = kept.as_ref().map(|k| Gist {
            purpose: if outdated || k.purpose_at.is_none() {
                None
            } else {
                k.purpose.clone()
            },
            ..k.clone()
        });
        let goal = model.goal(&job.subject);
        let input = gist::Input {
            name: &job.name,
            session: match job.subject {
                Subject::Session { engine, .. } => Some(Engine::title(engine)),
                Subject::Agent { .. } => None,
            },
            goal: goal.as_deref(),
            asks: &asks,
            ask: job.ask.as_deref(),
            steps: &job.steps,
            reply: job.reply.as_deref(),
            doing: Some(job.doing),
            kept: offered.as_ref(),
        };
        let prompt = gist::prompt(&input);
        let asked = model.answer(Some(&job.subject), owner, gist::GIST_SYSTEM, &prompt);
        let written = match tokio::time::timeout(gist::WRITE_WITHIN, asked).await {
            Ok(Ok(text)) => gist::parse(&text),
            Ok(Err(e)) => {
                debug!(target: "floor", error = %format!("{e:#}"), "no model to write a gist; said plainly");
                None
            }
            Err(_) => {
                debug!(target: "floor", "writing a gist took too long; said plainly");
                None
            }
        };
        let at = crew::now();
        let (purpose, purpose_at) = kept
            .as_ref()
            .map_or((None, None), |k| (k.purpose.clone(), k.purpose_at));
        let gist = match written {
            Some(w) => {
                let renew = w.purpose.is_some()
                    && gist::purpose_due(purpose_at, job.moment, w.turned, at, began);
                Gist {
                    purpose: if renew { w.purpose } else { purpose },
                    purpose_at: if renew { Some(at) } else { purpose_at },
                    now: w.now,
                    now_at: at,
                    turn: job.turn,
                    by_model: true,
                }
            }
            None if job.moment == Moment::Stepped
                && kept
                    .as_ref()
                    .is_some_and(|k| k.by_model && k.turn == job.turn) =>
            {
                return;
            }
            None => {
                let (plain_purpose, now) = gist::plainly(&input);
                // An agent's plain purpose follows its latest ask.
                let fresh = plain_purpose.is_some()
                    && (purpose.is_none()
                        || (began.is_some()
                            && gist::purpose_due(purpose_at, job.moment, false, at, began)));
                Gist {
                    purpose: if fresh { plain_purpose } else { purpose },
                    purpose_at: if fresh { Some(at) } else { purpose_at },
                    now,
                    now_at: at,
                    turn: job.turn,
                    by_model: false,
                }
            }
        };
        match self.0.kept.put_gist(owner, key, &gist).await {
            Ok(()) => (self.0.told)(owner),
            Err(e) => warn!(target: "floor", error = %format!("{e:#}"), "keeping a gist failed"),
        }
    }

    /// The floor's paragraph for `scope`, as kept, when anything is
    /// happening. One out of date ([`gist::summary_due`]) is written again
    /// from `entries` off this read, tried no more often than it may be
    /// written.
    pub async fn summary(
        &self,
        scope: &Scope,
        entries: Vec<Happening>,
        now: DateTime<Utc>,
    ) -> Option<Paragraph> {
        if entries.is_empty() {
            return None;
        }
        let kept = self
            .0
            .kept
            .summary(&scope.owner, scope.personal)
            .await
            .unwrap_or_else(|e| {
                warn!(target: "floor", error = %format!("{e:#}"), "reading the floor's paragraph failed");
                None
            });
        let mark = gist::fingerprint(&entries);
        let due = gist::summary_due(kept.as_ref().map(|(p, m)| (p.at, *m)), mark, now);
        if due && self.0.model.get().is_some() && self.try_summary(scope) {
            let (gists, scope) = (self.clone(), scope.clone());
            tokio::spawn(async move { gists.write_summary(&scope, &entries, mark).await });
        }
        kept.map(|(paragraph, _)| paragraph)
    }

    fn try_summary(&self, scope: &Scope) -> bool {
        let mut tried = self.0.tried.lock();
        let key = (scope.owner.clone(), scope.personal);
        let every = gist::SUMMARY_EVERY.to_std().unwrap_or_default();
        if tried.get(&key).is_some_and(|at| at.elapsed() < every) {
            return false;
        }
        tried.insert(key, Instant::now());
        true
    }

    async fn write_summary(&self, scope: &Scope, entries: &[Happening], mark: i64) {
        let Some(model) = self.0.model.get().cloned() else {
            return;
        };
        let prompt = gist::summary_prompt(entries);
        let asked = model.answer(None, Some(&scope.owner), gist::SUMMARY_SYSTEM, &prompt);
        let text = match tokio::time::timeout(gist::WRITE_WITHIN, asked).await {
            Ok(Ok(text)) => gist::parse_summary(&text),
            Ok(Err(e)) => {
                debug!(target: "floor", error = %format!("{e:#}"), "no model to write the floor's paragraph");
                None
            }
            Err(_) => {
                debug!(target: "floor", "writing the floor's paragraph took too long");
                None
            }
        };
        let Some(text) = text else {
            return;
        };
        let paragraph = Paragraph {
            text,
            at: crew::now(),
        };
        match self
            .0
            .kept
            .put_summary(&scope.owner, scope.personal, &paragraph, mark)
            .await
        {
            Ok(()) => (self.0.told)(Some(&scope.owner)),
            Err(e) => {
                warn!(target: "floor", error = %format!("{e:#}"), "keeping the floor's paragraph failed")
            }
        }
    }
}

fn doing_at_end(outcome: Outcome) -> Doing {
    match outcome {
        Outcome::Failed => Doing::Failed,
        Outcome::Limit => Doing::Limit,
        _ => Doing::Finished,
    }
}

/// The model a gist is written on for an agent on `tier_key`: its
/// provider's cheap tier (a Claude plan's is Haiku), never the agent's main
/// or compaction model; none where the provider maps no cheap one.
fn floor_model(
    tiers: &workforce_agent::llm::tiers::TierRegistry,
    tier_key: &str,
) -> Option<String> {
    let (routing, model) = tiers.resolve_routed_model("cheap", tier_key);
    (model != "cheap").then(|| {
        workforce_agent::llm::tiers::TierRegistry::format_routed(routing.as_deref(), model)
    })
}

/// The hub's own model: an agent's turn's gist on that agent's cheap
/// model, a Desk session's and the floor's paragraph on the hub's default
/// agent's, each call costed to the person it is for.
pub(crate) struct HubModel {
    pub state: Arc<crate::gateway::GatewayState>,
}

/// Run a gist's model call as the person it is for, on their own login. The
/// hub writes gists off the turn that caused them, where no identity is in
/// scope, so a call made bare reaches no login of anyone's. `owner` is the
/// floor's owner; with none (a personal hub's Desk) the call is the agent's
/// own background work, paid by whoever pays for that. `None`, with no call
/// made, when nobody's login serves `model`: the gist is then said plainly,
/// as it is without a model.
async fn written_for<F: std::future::Future>(
    service: &crate::serve::AgentService,
    owner: Option<&str>,
    model: &str,
    call: F,
) -> Option<F::Output> {
    let person = owner
        .and_then(|owner| workforce_common::subscription::Payer::classify(Some(owner)).person())
        .map(|person| person.to_string());
    service
        .in_background(
            &service.background_principal(person.as_deref()),
            crate::serve::Route::Model(Some(model)),
            call,
        )
        .await
}

#[async_trait]
impl Model for HubModel {
    async fn answer(
        &self,
        subject: Option<&Subject>,
        owner: Option<&str>,
        system: &str,
        user: &str,
    ) -> eyre::Result<String> {
        use workforce_agent::llm::provider::with_reasoning_suppressed;
        use workforce_agent::llm::types::LlmMessage;
        use workforce_agent::runner::with_served_log;
        use workforce_common::cost::{CostEntry, TokenCounts};
        use workforce_common::subscription::Origin;

        let registry = &self.state.registry;
        let service = match subject {
            Some(Subject::Agent { agent_id, .. }) => registry.get(agent_id),
            _ => None,
        }
        .or_else(|| registry.default_agent())
        .ok_or_else(|| eyre::eyre!("no agent to write with"))?;
        let model = floor_model(&service.tier_registry(), &service.tier_key.read())
            .unwrap_or_else(|| crate::turn_loop::cheap_model(&service));
        let messages = [LlmMessage::user(user.to_string())];
        let asked = with_served_log(with_reasoning_suppressed(service.llm.create_message(
            Some(system),
            &messages,
            &[],
            Some(&model),
            Some(400),
            true,
        )));
        let (response, served) = written_for(&service, owner, &model, asked)
            .await
            .ok_or_else(|| eyre::eyre!("no login to write with"))?;
        let response = response?;
        service.cost_tracker.record(CostEntry {
            task_type: Some("floor_gist"),
            identity_id: owner,
            description: Some("floor:gist"),
            served,
            ..CostEntry::new(
                service.agent_id.as_str(),
                &response.model,
                TokenCounts::from(&response.usage),
                Origin::Floor,
            )
        });
        Ok(response.all_text().unwrap_or_default())
    }

    fn goal(&self, subject: &Subject) -> Option<String> {
        let Subject::Session { id, .. } = subject else {
            return None;
        };
        crate::desk_driver::desk()?
            .view(*id)
            .ok()
            .map(|v| v.goal)
            .filter(|g| !g.trim().is_empty())
    }

    fn hub_internal(&self, subject: &Subject) -> bool {
        let Subject::Session { id, .. } = subject else {
            return false;
        };
        crate::desk_driver::desk().is_some_and(|d| d.hub_internal(*id))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use workforce_desk::floor::{Place, Who};
    use workforce_desk::gist::Said;

    use super::*;

    #[derive(Default)]
    struct Memory {
        gists: Mutex<HashMap<Whose, Gist>>,
        written: Mutex<Vec<Gist>>,
        summaries: Mutex<HashMap<(String, bool), (Paragraph, i64)>>,
        /// What the log has of the subject's asks, and how it was asked.
        asked: Mutex<Asked>,
        reached: Mutex<Vec<Reach>>,
    }

    #[async_trait]
    impl Kept for Memory {
        async fn gist(
            &self,
            owner: Option<&str>,
            subject: &SubjectKey,
        ) -> eyre::Result<Option<Gist>> {
            Ok(self
                .gists
                .lock()
                .get(&(owner.map(str::to_string), subject.clone()))
                .cloned())
        }

        async fn put_gist(
            &self,
            owner: Option<&str>,
            subject: &SubjectKey,
            gist: &Gist,
        ) -> eyre::Result<()> {
            self.gists
                .lock()
                .insert((owner.map(str::to_string), subject.clone()), gist.clone());
            self.written.lock().push(gist.clone());
            Ok(())
        }

        async fn asks(
            &self,
            _: Option<&str>,
            _: &SubjectKey,
            reach: &Reach,
        ) -> eyre::Result<Asked> {
            self.reached.lock().push(reach.clone());
            Ok(self.asked.lock().clone())
        }

        async fn turn(&self, _: Uuid) -> eyre::Result<Option<Turn>> {
            Ok(None)
        }

        async fn summary(
            &self,
            owner: &str,
            personal: bool,
        ) -> eyre::Result<Option<(Paragraph, i64)>> {
            Ok(self
                .summaries
                .lock()
                .get(&(owner.to_string(), personal))
                .cloned())
        }

        async fn put_summary(
            &self,
            owner: &str,
            personal: bool,
            paragraph: &Paragraph,
            fingerprint: i64,
        ) -> eyre::Result<()> {
            self.summaries.lock().insert(
                (owner.to_string(), personal),
                (paragraph.clone(), fingerprint),
            );
            Ok(())
        }
    }

    /// Answers with `say`, after `slow`; fails when `say` is empty.
    struct Scripted {
        say: Mutex<String>,
        slow: Duration,
        asked: Mutex<Vec<String>>,
        /// The sessions the hub ran itself.
        hubs: Mutex<Vec<Uuid>>,
    }

    impl Scripted {
        fn new(say: &str, slow: Duration) -> Arc<Self> {
            Arc::new(Self {
                say: Mutex::new(say.into()),
                slow,
                asked: Mutex::new(vec![]),
                hubs: Mutex::new(vec![]),
            })
        }
    }

    #[async_trait]
    impl Model for Scripted {
        async fn answer(
            &self,
            _: Option<&Subject>,
            _: Option<&str>,
            _: &str,
            user: &str,
        ) -> eyre::Result<String> {
            self.asked.lock().push(user.to_string());
            tokio::time::sleep(self.slow).await;
            let say = self.say.lock().clone();
            if say.is_empty() {
                eyre::bail!("no model");
            }
            Ok(say)
        }

        fn goal(&self, _: &Subject) -> Option<String> {
            None
        }

        fn hub_internal(&self, subject: &Subject) -> bool {
            matches!(subject, Subject::Session { id, .. } if self.hubs.lock().contains(id))
        }
    }

    fn started(id: Uuid) -> Turn {
        let at = crew::now();
        Turn {
            id,
            owner: Some("u1".into()),
            subject: Subject::Agent {
                agent_id: "scout".into(),
                session_id: "dm".into(),
            },
            name: "Scout".into(),
            place: Place::Dm,
            task: Some("Look at #12 and land it".into()),
            doing: None,
            started_at: at,
            heard_at: at,
            ended_at: None,
            outcome: None,
            answer: None,
            question: None,
            settled_at: None,
            waits: vec![],
            idle: vec![],
            token: None,
            instance: None,
        }
    }

    fn gists(model: Arc<Scripted>) -> (Gists, Arc<Memory>, Arc<Mutex<usize>>) {
        let memory = Arc::new(Memory::default());
        *memory.asked.lock() = Asked {
            asks: vec!["Get Desk ready for the team".into()],
            began: None,
        };
        let told = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&told);
        let gists = Gists::start(
            Arc::clone(&memory) as Arc<dyn Kept>,
            Arc::new(move |_| *counted.lock() += 1),
        );
        gists.serve(model);
        (gists, memory, told)
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    fn step(turn: Uuid, doing: &str) -> TurnOp {
        TurnOp::Progress {
            turn,
            doing: Some(doing.into()),
            at: crew::now(),
        }
    }

    const NOTE: &str =
        "PURPOSE: Getting Desk ready for the team.\nCHANGED: no\nNOW: Testing the build.";

    #[tokio::test(start_paused = true)]
    async fn a_gist_is_written_as_a_turn_begins_on_new_steps_every_two_minutes_and_as_it_ends() {
        let model = Scripted::new(NOTE, Duration::from_millis(5));
        let (gists, memory, told) = gists(Arc::clone(&model));
        let id = Uuid::new_v4();
        gists.saw(&TurnOp::Begin {
            turn: Box::new(started(id)),
            by_person: true,
        });
        settle().await;
        assert_eq!(memory.written.lock().len(), 1, "written as it begins");
        assert_eq!(*told.lock(), 1, "and its owner's floor told");
        let first = memory.written.lock()[0].clone();
        assert_eq!(first.now, "Testing the build.");
        assert_eq!(
            first.purpose.as_deref(),
            Some("Getting Desk ready for the team.")
        );

        gists.saw(&step(id, "● Bash cargo test"));
        gists.saw(&step(id, "● Read README"));
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            1,
            "steps inside two minutes wait"
        );
        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        settle().await;
        assert_eq!(memory.written.lock().len(), 2, "then are said once");
        assert!(model.asked.lock()[1].contains("- Bash cargo test\n- Read README"));
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            2,
            "no new steps, nothing to say"
        );

        gists.saw(&TurnOp::End {
            turn: id,
            at: crew::now(),
            outcome: Outcome::Finished,
            answer: Some("It builds.".into()),
            question: None,
        });
        settle().await;
        assert_eq!(memory.written.lock().len(), 3, "written as it ends");
        assert!(model.asked.lock()[2].contains("Its latest reply: It builds."));
        assert!(model.asked.lock()[2].contains("PURPOSE: Getting Desk ready for the team."));
    }

    #[tokio::test(start_paused = true)]
    async fn a_model_that_fails_or_is_slow_leaves_a_plain_gist_and_never_one_over_its_own() {
        let model = Scripted::new("", Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let id = Uuid::new_v4();
        gists.saw(&TurnOp::Begin {
            turn: Box::new(started(id)),
            by_person: true,
        });
        settle().await;
        let plain = memory.written.lock()[0].clone();
        assert!(!plain.by_model);
        assert_eq!(
            plain.purpose.as_deref(),
            Some("Look at and land it."),
            "an agent's plain purpose is its latest ask, its identifier dropped"
        );
        assert_eq!(
            plain.now, "Working on it now.",
            "the ask is the purpose already"
        );

        *model.say.lock() = NOTE.into();
        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        gists.saw(&step(id, "● Bash cargo test"));
        settle().await;
        assert!(memory.written.lock()[1].by_model);

        let stuck = Scripted::new(NOTE, Duration::from_secs(60));
        let (gists, memory, _) = self::gists(stuck);
        gists.saw(&TurnOp::Begin {
            turn: Box::new(started(id)),
            by_person: true,
        });
        tokio::time::sleep(Duration::from_secs(46)).await;
        settle().await;
        assert!(
            !memory.written.lock()[0].by_model,
            "past its time it is said plainly"
        );
        memory
            .gists
            .lock()
            .values_mut()
            .for_each(|g| g.by_model = true);
        let before = memory.written.lock().len();
        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        gists.saw(&step(id, "● Bash cargo test"));
        tokio::time::sleep(Duration::from_secs(61)).await;
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            before,
            "a step the model can't say leaves its gist of this turn as it was"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_plan_still_writes_the_gist_and_the_turn_never_waits_for_it() {
        let model = Scripted::new(NOTE, Duration::from_secs(10));
        let (gists, memory, _) = gists(Arc::clone(&model));
        let id = Uuid::new_v4();
        let before = Instant::now();
        gists.saw(&TurnOp::Begin {
            turn: Box::new(started(id)),
            by_person: true,
        });
        for i in 0..2 * BACKLOG {
            gists.saw(&step(id, &format!("● step {i}")));
        }
        assert_eq!(
            Instant::now(),
            before,
            "what the turn hands over is taken without waiting, however slow the model"
        );
        settle().await;
        assert!(
            memory.written.lock().is_empty(),
            "the model is still writing"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        settle().await;
        let written = memory.written.lock()[0].clone();
        assert!(written.by_model, "ten seconds is within its time");
        assert_eq!(written.now, "Testing the build.");
    }

    fn kept_purpose(purpose: &str, ago: chrono::Duration) -> Gist {
        Gist {
            purpose: Some(purpose.into()),
            purpose_at: Some(crew::now() - ago),
            now: "Before.".into(),
            now_at: crew::now() - ago,
            turn: Uuid::new_v4(),
            by_model: true,
        }
    }

    fn scout() -> Whose {
        (Some("u1".into()), SubjectKey::agent("scout"))
    }

    fn begin_for(turn: Turn, by_person: bool) -> TurnOp {
        TurnOp::Begin {
            turn: Box::new(turn),
            by_person,
        }
    }

    const LOGIN: &str = "PURPOSE: Finding why the login page fails.\nCHANGED: no\nNOW: Looking.";

    #[tokio::test(start_paused = true)]
    async fn an_agents_old_first_topic_gives_way_to_the_recent_conversation() {
        let model = Scripted::new(LOGIN, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        // Days old: its purpose was written about the budget.
        memory.gists.lock().insert(
            scout(),
            kept_purpose("Planning next year's budget.", chrono::Duration::days(3)),
        );
        *memory.asked.lock() = Asked {
            asks: vec!["Why does the login page fail?".into()],
            began: Some(crew::now()),
        };
        gists.saw(&begin_for(started(Uuid::new_v4()), true));
        settle().await;

        let written = memory.written.lock()[0].clone();
        assert_eq!(
            written.purpose.as_deref(),
            Some("Finding why the login page fails.")
        );
        let asked = model.asked.lock()[0].clone();
        assert!(asked.contains("Why does the login page fail?"));
        assert!(
            !asked.contains("budget"),
            "the old topic is not offered to be kept: {asked}"
        );
        let reach = memory.reached.lock()[0].clone();
        match &reach {
            Reach::Thread { since } => {
                let within = crew::now() - *since;
                assert!(
                    within >= gist::THREAD_WITHIN
                        && within < gist::THREAD_WITHIN + chrono::Duration::minutes(1)
                );
            }
            other => panic!("an agent reads its thread, not {other:?}"),
        };
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_conversation_replaces_a_purpose_written_minutes_ago_but_the_same_one_keeps_it() {
        let model = Scripted::new(LOGIN, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let ago = chrono::Duration::minutes(20);
        memory
            .gists
            .lock()
            .insert(scout(), kept_purpose("Planning next year's budget.", ago));
        // The purpose was written before this conversation began.
        *memory.asked.lock() = Asked {
            asks: vec!["Why does the login page fail?".into()],
            began: Some(crew::now() - chrono::Duration::minutes(5)),
        };
        // Not by the person's prompt, and the model says it didn't turn.
        gists.saw(&begin_for(started(Uuid::new_v4()), false));
        settle().await;
        assert_eq!(
            memory.written.lock()[0].purpose.as_deref(),
            Some("Finding why the login page fails.")
        );

        // Written inside the conversation it is still the same.
        let (gists, memory, _) = self::gists(Scripted::new(LOGIN, Duration::ZERO));
        memory
            .gists
            .lock()
            .insert(scout(), kept_purpose("Planning next year's budget.", ago));
        *memory.asked.lock() = Asked {
            asks: vec!["Plan the budget".into()],
            began: Some(crew::now() - chrono::Duration::minutes(25)),
        };
        gists.saw(&begin_for(started(Uuid::new_v4()), false));
        settle().await;
        assert_eq!(
            memory.written.lock()[0].purpose.as_deref(),
            Some("Planning next year's budget.")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_of_topic_by_the_person_replaces_an_agents_recent_purpose() {
        let turned = "PURPOSE: Finding why the login page fails.\nCHANGED: yes\nNOW: Looking.";
        let (gists, memory, _) = gists(Scripted::new(turned, Duration::ZERO));
        let minute = chrono::Duration::minutes(1);
        memory.gists.lock().insert(
            scout(),
            kept_purpose("Planning next year's budget.", minute),
        );
        *memory.asked.lock() = Asked {
            asks: vec![
                "Plan the budget".into(),
                "Why does the login page fail?".into(),
            ],
            began: Some(crew::now() - chrono::Duration::hours(1)),
        };
        gists.saw(&begin_for(started(Uuid::new_v4()), false));
        settle().await;
        assert_eq!(
            memory.written.lock()[0].purpose.as_deref(),
            Some("Planning next year's budget."),
            "the hub's own turn never turns it"
        );
        gists.saw(&begin_for(started(Uuid::new_v4()), true));
        settle().await;
        assert_eq!(
            memory.written.lock()[1].purpose.as_deref(),
            Some("Finding why the login page fails.")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_with_no_asks_in_the_window_still_shows_a_purpose() {
        // The model says nothing usable, and the log has no asks in the window.
        let (gists, memory, _) = gists(Scripted::new("", Duration::ZERO));
        *memory.asked.lock() = Asked::default();
        memory.gists.lock().insert(
            scout(),
            kept_purpose("Planning next year's budget.", chrono::Duration::days(2)),
        );
        let mut turn = started(Uuid::new_v4());
        turn.task = None;
        gists.saw(&begin_for(turn, false));
        settle().await;
        let written = memory.written.lock()[0].clone();
        assert!(
            written.purpose.as_deref().is_some_and(|p| !p.is_empty()),
            "never blank: {written:?}"
        );
    }

    fn in_slack() -> Turn {
        Turn {
            place: Place::Slack {
                team: None,
                channel: "C1".into(),
                thread: None,
            },
            task: Some("Secret ask from the channel".into()),
            ..started(Uuid::new_v4())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slack_turn_changes_only_what_an_agent_is_doing_never_its_purpose() {
        let model = Scripted::new(LOGIN, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let kept = kept_purpose("Planning next year's budget.", chrono::Duration::hours(2));
        let was = kept.purpose_at;
        memory.gists.lock().insert(scout(), kept);
        gists.saw(&begin_for(in_slack(), true));
        settle().await;
        let written = memory.written.lock()[0].clone();
        assert_eq!(
            written.purpose.as_deref(),
            Some("Planning next year's budget.")
        );
        assert_eq!(written.purpose_at, was);
        assert_eq!(written.now, "Answering a request in Slack.");
        assert!(
            model.asked.lock().is_empty(),
            "Slack content never reaches the model"
        );
        assert!(memory.reached.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_never_talked_to_in_desk_has_a_neutral_purpose_and_a_desk_turn_replaces_it() {
        let model = Scripted::new(LOGIN, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        gists.saw(&begin_for(in_slack(), true));
        settle().await;
        let written = memory.written.lock()[0].clone();
        assert_eq!(written.purpose.as_deref(), Some(gist::ELSEWHERE_PURPOSE));
        assert_eq!(written.purpose_at, None);
        memory.gists.lock().insert(scout(), written);
        *memory.asked.lock() = Asked {
            asks: vec!["Why does the login page fail?".into()],
            began: Some(crew::now()),
        };
        gists.saw(&begin_for(started(Uuid::new_v4()), true));
        settle().await;
        let next = memory.written.lock()[1].clone();
        assert_eq!(
            next.purpose.as_deref(),
            Some("Finding why the login page fails.")
        );
        assert!(!model.asked.lock()[0].contains("requests from other places"));
        assert!(!model.asked.lock()[0].contains("Secret"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_sessions_purpose_stays_whatever_the_model_says_and_reads_its_whole_job() {
        let turned = "PURPOSE: Something else entirely.\nCHANGED: no\nNOW: Looking.";
        let (gists, memory, _) = gists(Scripted::new(turned, Duration::ZERO));
        let id = Uuid::new_v4();
        let whose = (Some("u1".to_string()), SubjectKey::session(id));
        memory.gists.lock().insert(
            whose,
            kept_purpose(
                "Getting Desk ready for the team.",
                chrono::Duration::minutes(10),
            ),
        );
        let session = Turn {
            subject: Subject::Session {
                id,
                engine: Engine::Claude,
            },
            name: "release".into(),
            ..started(Uuid::new_v4())
        };
        gists.saw(&begin_for(session, true));
        settle().await;
        assert_eq!(
            memory.written.lock()[0].purpose.as_deref(),
            Some("Getting Desk ready for the team.")
        );
        assert_eq!(memory.reached.lock()[0], Reach::Job);
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_model_an_agents_purpose_is_its_latest_ask_and_follows_a_new_thread() {
        let (gists, memory, _) = gists(Scripted::new("", Duration::ZERO));
        memory.gists.lock().insert(
            scout(),
            kept_purpose("Planning next year's budget.", chrono::Duration::days(2)),
        );
        *memory.asked.lock() = Asked {
            asks: vec![
                "Plan the budget".into(),
                "Why does the login page fail?".into(),
            ],
            began: Some(crew::now()),
        };
        let mut turn = started(Uuid::new_v4());
        turn.task = Some("Why does the login page fail?".into());
        gists.saw(&begin_for(turn, true));
        settle().await;
        let written = memory.written.lock()[0].clone();
        assert!(!written.by_model);
        assert_eq!(
            written.purpose.as_deref(),
            Some("Why does the login page fail?"),
            "the recent thread, not the first ask or the old purpose"
        );
        assert!(written.purpose_at > Some(crew::now() - chrono::Duration::minutes(1)));
    }

    #[test]
    fn a_gist_is_written_on_the_providers_cheap_tier() {
        let tiers = workforce_agent::llm::tiers::TierRegistry::from_defaults().unwrap();
        assert_eq!(floor_model(&tiers, "claude-code").as_deref(), Some("haiku"));
        assert_eq!(floor_model(&tiers, "codex").as_deref(), Some("gpt-6-luna"));
        assert_eq!(floor_model(&tiers, "no-such-provider"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_agents_turn_run_as_a_session_gets_no_gist_of_its_own() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let turn = Turn {
            owner: None,
            subject: Subject::Session {
                id: Uuid::new_v4(),
                engine: Engine::Claude,
            },
            name: "at-dm-d4774cbb6c89-2e".into(),
            ..started(Uuid::new_v4())
        };
        gists.saw(&TurnOp::Begin {
            turn: Box::new(turn),
            by_person: false,
        });
        settle().await;
        assert!(memory.written.lock().is_empty());
        assert!(model.asked.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_the_hub_ran_itself_gets_no_gist_whatever_it_is_called() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let session = |id| Turn {
            owner: None,
            subject: Subject::Session {
                id,
                engine: Engine::Claude,
            },
            name: "b369e00e-c276-4ae0-8394-6d83707380a0-c2".into(),
            ..started(Uuid::new_v4())
        };
        let hubs = Uuid::new_v4();
        model.hubs.lock().push(hubs);
        gists.saw(&TurnOp::Begin {
            turn: Box::new(session(hubs)),
            by_person: false,
        });
        settle().await;
        assert!(memory.written.lock().is_empty());
        assert!(model.asked.lock().is_empty());

        gists.saw(&TurnOp::Begin {
            turn: Box::new(session(Uuid::new_v4())),
            by_person: true,
        });
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            1,
            "the person's own is written"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_paragraph_is_written_when_out_of_date_and_tried_at_most_every_ten_minutes() {
        let model = Scripted::new("The release is being tested.", Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let scope = Scope {
            owner: "u1".into(),
            personal: true,
        };
        let entry = |now: &str| Happening {
            who: Who {
                kind: workforce_desk::crew::PartyKind::Agent,
                id: "scout".into(),
                name: "Scout".into(),
                engine: None,
            },
            doing: Doing::Working,
            gist: Said {
                purpose: None,
                now: now.into(),
                at: crew::now(),
            },
        };
        assert_eq!(gists.summary(&scope, vec![], crew::now()).await, None);
        assert_eq!(
            gists
                .summary(&scope, vec![entry("Testing.")], crew::now())
                .await,
            None
        );
        settle().await;
        let first = gists
            .summary(&scope, vec![entry("Testing.")], crew::now())
            .await
            .unwrap();
        assert_eq!(first.text, "The release is being tested.");
        assert_eq!(model.asked.lock().len(), 1);
        assert!(model.asked.lock()[0].contains("- Scout (working): Testing."));
        gists
            .summary(&scope, vec![entry("Shipping.")], crew::now())
            .await;
        settle().await;
        assert_eq!(
            model.asked.lock().len(),
            1,
            "changed, but not ten minutes old"
        );
        let later = first.at + chrono::Duration::minutes(11);
        tokio::time::sleep(Duration::from_secs(11 * 60)).await;
        gists.summary(&scope, vec![entry("Shipping.")], later).await;
        settle().await;
        assert_eq!(model.asked.lock().len(), 2);
        assert!(memory.summaries.lock().contains_key(&("u1".into(), true)));
    }

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    /// An agent on the ChatGPT backend, owned by `owner`, on a mock backend.
    async fn chatgpt_agent(
        dir: &std::path::Path,
        server: &wiremock::MockServer,
        owner: Option<&str>,
    ) -> Arc<crate::serve::AgentService> {
        let owner = owner.map(str::to_string);
        crate::serve::testing::chatgpt_service(dir, server, |config| {
            config.agents.push(workforce_common::settings::AgentConfig {
                id: "allan".into(),
                default: true,
                owner_id: owner,
                ..Default::default()
            });
        })
        .await
    }

    async fn bearers(server: &wiremock::MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/responses")
            .filter_map(|r| {
                r.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
            .collect()
    }

    /// `key` in the global vault holds a ChatGPT login for `who`; its token.
    fn chatgpt_login_at(service: &crate::serve::AgentService, key: &str, who: &str) -> String {
        let login = crate::subscription::test_support::login(
            &format!("acct-{}", Uuid::new_v4()),
            who,
            "rt-0",
        );
        service
            .global_vault
            .as_ref()
            .expect("global vault")
            .set(key, &login)
            .unwrap();
        crate::subscription::AuthJson::parse(&login)
            .unwrap()
            .access_token()
            .unwrap()
            .to_string()
    }

    /// The call a gist makes, made bare as the writer makes it: in a task
    /// with no identity in scope.
    async fn write_for(
        service: &crate::serve::AgentService,
        owner: Option<&str>,
    ) -> Option<String> {
        use workforce_agent::llm::types::LlmMessage;
        let messages = [LlmMessage::user("user".to_string())];
        written_for(
            service,
            owner,
            "gpt-6-luna",
            service.llm.create_message(
                Some("system"),
                &messages,
                &[],
                Some("gpt-6-luna"),
                Some(400),
                true,
            ),
        )
        .await
        .map(|response| response.expect("served").all_text().unwrap_or_default())
    }

    #[tokio::test]
    async fn a_gist_is_written_on_its_owners_own_login() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = chatgpt_agent(tmp.path(), &server, None).await;
        let alice = chatgpt_login_at(&service, &format!("users/{ALICE}/codex/auth_json"), ALICE);
        crate::serve::testing::answer_bearer(&server, &alice).await;

        assert!(write_for(&service, Some(ALICE)).await.is_some());

        let seen = bearers(&server).await;
        assert_eq!(seen, vec![format!("Bearer {alice}")]);
    }

    #[tokio::test]
    async fn a_gist_with_no_owner_is_the_agents_owners_work() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = chatgpt_agent(tmp.path(), &server, Some(BOB)).await;
        let bob = chatgpt_login_at(&service, &format!("users/{BOB}/codex/auth_json"), BOB);
        crate::serve::testing::answer_bearer(&server, &bob).await;

        for owner in [None, Some("slack:U123")] {
            assert!(write_for(&service, owner).await.is_some(), "{owner:?}");
        }

        let seen = bearers(&server).await;
        assert_eq!(seen, vec![format!("Bearer {bob}"); 2]);
    }

    /// The prod condition: no person's login to draw on and none shared. The
    /// call is not made, to fail over and then on to the hub default, each
    /// time a turn moves.
    #[tokio::test]
    async fn a_gist_nobodys_login_serves_is_not_asked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = chatgpt_agent(tmp.path(), &server, None).await;
        chatgpt_login_at(&service, &format!("users/{BOB}/codex/auth_json"), BOB);

        for owner in [None, Some(ALICE)] {
            assert!(write_for(&service, owner).await.is_none(), "{owner:?}");
        }

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A login the install shares serves the floor's own gists, as it does
    /// a personal hub's Desk, which has no person to bill.
    #[tokio::test]
    async fn a_gist_with_no_one_to_bill_runs_on_the_shared_login() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = chatgpt_agent(tmp.path(), &server, None).await;
        let shared = chatgpt_login_at(&service, "codex/auth_json", "shared");
        crate::serve::testing::answer_bearer(&server, &shared).await;

        assert!(write_for(&service, None).await.is_some());

        let seen = bearers(&server).await;
        assert_eq!(seen, vec![format!("Bearer {shared}")]);
    }
}
