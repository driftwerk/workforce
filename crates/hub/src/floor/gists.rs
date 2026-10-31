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
use workforce_desk::floor::{Outcome, Scope, Subject, SubjectKey, Turn, TurnOp};
use workforce_desk::gist::{self, Doing, Gist, Happening, Moment, Paragraph};
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
    async fn asks(&self, owner: Option<&str>, subject: &SubjectKey) -> eyre::Result<Vec<String>>;
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

    async fn asks(&self, owner: Option<&str>, subject: &SubjectKey) -> eyre::Result<Vec<String>> {
        PgFloorStore::asks(self, owner, subject).await
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
            TurnOp::Begin { turn, by_person } => {
                let whose = (turn.owner.clone(), turn.subject.key());
                let r = subjects.entry(whose.clone()).or_insert_with(|| Running {
                    subject: turn.subject.clone(),
                    name: turn.name.clone(),
                    turn: turn.id,
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
        let asks = self.0.kept.asks(owner, key).await.unwrap_or_default();
        let goal = model.goal(&job.subject);
        let input = gist::Input {
            name: &job.name,
            session: match job.subject {
                Subject::Session { engine, .. } => Some(engine_name(engine)),
                Subject::Agent { .. } => None,
            },
            goal: goal.as_deref(),
            asks: &asks,
            ask: job.ask.as_deref(),
            steps: &job.steps,
            reply: job.reply.as_deref(),
            doing: Some(job.doing),
            kept: kept.as_ref(),
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
                let renew =
                    w.purpose.is_some() && gist::purpose_due(purpose_at, job.moment, w.turned, at);
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
                let fresh = purpose.is_none() && plain_purpose.is_some();
                Gist {
                    purpose: purpose.or(plain_purpose),
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

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Claude => "Claude Code",
        Engine::Codex => "Codex",
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
        let (response, served) =
            with_served_log(with_reasoning_suppressed(service.llm.create_message(
                Some(system),
                &[LlmMessage::user(user.to_string())],
                &[],
                Some(&model),
                Some(400),
                true,
            )))
            .await;
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

        async fn asks(&self, _: Option<&str>, _: &SubjectKey) -> eyre::Result<Vec<String>> {
            Ok(vec!["Get Desk ready for the team".into()])
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
    }

    impl Scripted {
        fn new(say: &str, slow: Duration) -> Arc<Self> {
            Arc::new(Self {
                say: Mutex::new(say.into()),
                slow,
                asked: Mutex::new(vec![]),
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
            Some("Get Desk ready for the team.")
        );
        assert_eq!(
            plain.now, "Now working on: Look at and land it.",
            "its identifier dropped"
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
}
