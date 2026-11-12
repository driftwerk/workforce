//! Gists written as turns run. Every op the log's writer applied is seen
//! here; a subject's gist is written through the hub's model when
//! [`gist::due`] says, off the writer's path, and its owner's floor told it
//! changed. The floor's paragraph is written from the gists when a read
//! finds it out of date. Without a model nothing is written: the floor says
//! each gist plainly from the turns. When the model fails, the line is said
//! plainly and why is logged, once a while per subject.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::time::Duration;
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
/// How often one cause of the model's failures is logged at WARN.
const WARN_EVERY: Duration = Duration::from_secs(10 * 60);
/// Causes remembered as warned of before the list is started again.
const WARN_CAUSES: usize = 256;
/// Turns looked up to be adopted before the list is started again.
const LOOKED: usize = 4096;
/// How long a session's turn-end gist waits for its account to be written
/// before the floor writes it itself.
const HANDED_FOR: Duration = Duration::from_secs(90);
/// How long an account's gist stands for the turn end it came before.
const DELIVERED_FOR: Duration = Duration::from_secs(120);

/// Whether a Desk session's turn-end gist is written from its account
/// instead: the desk says, with the same rule its account writer keeps.
pub type HandsOff = Arc<dyn Fn(Uuid) -> bool + Send + Sync>;

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
    /// A turn still open that this hub saw no beginning of: one begun
    /// before it started, as logged.
    Adopt(Box<Turn>),
    /// A turn whose lookup failed, to be looked up again.
    Unlooked(Uuid),
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
    /// When each cause of a failure was last logged at WARN.
    warned: Mutex<HashMap<String, Instant>>,
    /// Whose turn-end gists the account writer writes.
    hands_off: OnceLock<HandsOff>,
    /// Turn-end gists handed to the account writer, waiting for it.
    handed: Mutex<HashMap<SubjectKey, Job>>,
    /// Gists the account writer wrote before the floor saw their turn
    /// end, each taken by that turn end.
    delivered: Mutex<HashMap<SubjectKey, Instant>>,
}

/// What the writer knows of the turns running: each subject's, whose each
/// turn is, and the turns looked up to be adopted.
#[derive(Default)]
struct Floor {
    subjects: HashMap<Whose, Running>,
    turns: HashMap<Uuid, Whose>,
    looked: HashSet<Uuid>,
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
            warned: Mutex::new(HashMap::new()),
            hands_off: OnceLock::new(),
            handed: Mutex::new(HashMap::new()),
            delivered: Mutex::new(HashMap::new()),
        }));
        tokio::spawn(gists.clone().read(rx));
        gists
    }

    /// Writes through `model` from now on; the first one served stays.
    pub fn serve(&self, model: Arc<dyn Model>) {
        let _ = self.0.model.set(model);
    }

    /// Hands the turn-end gists of the Desk sessions `hands_off` says to
    /// their account writer, which writes each from the account's answer
    /// ([`Self::account_gist`]); the first one given stays.
    pub fn hand_turn_ends(&self, hands_off: HandsOff) {
        let _ = self.0.hands_off.set(hands_off);
    }

    /// Whether `job` is a turn end whose gist the account writer writes.
    fn handed_off(&self, job: &Job) -> bool {
        let Subject::Session { id, .. } = &job.subject else {
            return false;
        };
        job.moment == Moment::Ended && self.0.hands_off.get().is_some_and(|f| f(*id))
    }

    /// Waits for the account writer to write `job`'s gist; past
    /// [`HANDED_FOR`] (it couldn't), the floor writes it as ever.
    async fn handed(&self, job: Job) {
        let key = job.whose.1.clone();
        // Used up here, so a later turn's end is never taken as written.
        let just = self
            .0
            .delivered
            .lock()
            .remove(&key)
            .is_some_and(|at| at.elapsed() < DELIVERED_FOR);
        if just {
            // Its account came first: the gist is written already.
            return;
        }
        let turn = job.turn;
        self.0.handed.lock().insert(key.clone(), job);
        tokio::time::sleep(HANDED_FOR).await;
        let waiting = {
            let mut handed = self.0.handed.lock();
            match handed.get(&key) {
                Some(j) if j.turn == turn => handed.remove(&key),
                _ => None,
            }
        };
        if let Some(job) = waiting {
            debug!(target: "floor", subject = ?key, "a session's account wrote no gist in time; the floor writes it");
            self.written(job).await;
        }
    }

    /// Writes session `session`'s turn-end gist from its account: its
    /// purpose and now said plainly (none: kept as they were), `by_model`
    /// when the model wrote the account.
    pub async fn account_gist(
        &self,
        session: Uuid,
        purpose: Option<String>,
        now: Option<String>,
        by_model: bool,
    ) {
        let key = SubjectKey::session(session);
        let handed = self.0.handed.lock().remove(&key);
        if handed.is_none() {
            // The account came before the floor's turn end: that end is
            // written already once it comes.
            self.0.delivered.lock().insert(key.clone(), Instant::now());
        }
        let owner = handed.as_ref().and_then(|j| j.whose.0.clone());
        let whose: Whose = (owner.clone(), key.clone());
        let lock = Arc::clone(self.0.writing.lock().entry(whose).or_default());
        let _one = lock.lock().await;
        let kept = self
            .0
            .kept
            .gist(owner.as_deref(), &key)
            .await
            .unwrap_or_else(|e| {
                warn!(target: "floor", error = %format!("{e:#}"), "reading a gist failed");
                None
            });
        let at = crew::now();
        let (kept_purpose, kept_at) = kept
            .as_ref()
            .map_or((None, None), |k| (k.purpose.clone(), k.purpose_at));
        let renewed = purpose.is_some() && purpose != kept_purpose;
        let turn = handed
            .as_ref()
            .map(|j| j.turn)
            .or(kept.as_ref().map(|k| k.turn))
            .unwrap_or_default();
        let gist = Gist {
            purpose: if renewed { purpose } else { kept_purpose },
            purpose_at: if renewed { Some(at) } else { kept_at },
            now: now
                .or_else(|| kept.as_ref().map(|k| k.now.clone()))
                .unwrap_or_else(|| "Finished what it was asked.".into()),
            now_at: at,
            turn,
            by_model,
        };
        match self.0.kept.put_gist(owner.as_deref(), &key, &gist).await {
            Ok(()) => (self.0.told)(owner.as_deref()),
            Err(e) => warn!(target: "floor", error = %format!("{e:#}"), "keeping a gist failed"),
        }
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
        let mut floor = Floor::default();
        while let Some(seen) = rx.recv().await {
            match seen {
                Seen::Op(op) => self.op(op, &mut floor),
                Seen::Due(whose) => {
                    if let Some(r) = floor.subjects.get_mut(&whose) {
                        r.deferred = false;
                        self.step(&whose, r);
                    }
                }
                Seen::Adopt(turn) => self.adopted(*turn, &mut floor),
                Seen::Unlooked(turn) => {
                    floor.looked.remove(&turn);
                }
            }
        }
    }

    /// Whether `turn` gets a gist: an agent's turn run as a Claude Code
    /// session is the agent's, whose own turn has one, and a session the
    /// hub ran itself is never the person's.
    fn ours(&self, turn: &Turn) -> bool {
        let agents =
            matches!(turn.subject, Subject::Session { .. }) && gist::agent_turn_folder(&turn.name);
        let hubs = self
            .0
            .model
            .get()
            .is_some_and(|m| m.hub_internal(&turn.subject));
        !agents && !hubs
    }

    /// `turn`'s subject as running it, counted open once more.
    fn opened<'a>(turn: &Turn, by_person: bool, floor: &'a mut Floor) -> (Whose, &'a mut Running) {
        let whose = (turn.owner.clone(), turn.subject.key());
        let r = floor
            .subjects
            .entry(whose.clone())
            .or_insert_with(|| Running {
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
        floor.turns.insert(turn.id, whose.clone());
        (whose, r)
    }

    /// A step of a turn this hub saw no beginning of: the turn is looked up
    /// in the log, once, and taken up if it is still open.
    fn adopt(&self, turn: Uuid, floor: &mut Floor) {
        if floor.looked.len() >= LOOKED {
            floor.looked.clear();
        }
        if !floor.looked.insert(turn) {
            return;
        }
        let gists = self.clone();
        tokio::spawn(async move {
            match gists.0.kept.turn(turn).await {
                Ok(Some(t)) if t.ended_at.is_none() => {
                    let _ = gists.0.tx.send(Seen::Adopt(Box::new(t))).await;
                }
                Ok(_) => {}
                Err(e) => {
                    // Looked up again on its next step.
                    let _ = gists.0.tx.send(Seen::Unlooked(turn)).await;
                    gists.unwritten(
                        &turn.to_string(),
                        &format!("reading it from the log failed: {e:#}"),
                        "a turn begun before the hub started couldn't be taken up",
                    );
                }
            }
        });
    }

    /// A turn begun before this hub started, taken up as if it had been
    /// seen beginning: its latest step, as logged, said now.
    fn adopted(&self, turn: Turn, floor: &mut Floor) {
        // Looked up and not since ended: a lookup that lands after the
        // turn's end never opens it again.
        if !floor.looked.contains(&turn.id)
            || floor.turns.contains_key(&turn.id)
            || !self.ours(&turn)
        {
            return;
        }
        let doing = turn.doing.clone();
        let (whose, r) = Self::opened(&turn, false, floor);
        if let Some(doing) = doing {
            r.steps.push_back(doing);
            r.unsaid += 1;
        }
        self.step(&whose, r);
    }

    fn op(&self, op: TurnOp, floor: &mut Floor) {
        match op {
            TurnOp::Begin { turn, .. } if !self.ours(&turn) => {}
            TurnOp::Begin { turn, by_person } => {
                let (whose, r) = Self::opened(&turn, by_person, floor);
                self.write(&whose, r, Moment::Began { by_person }, None);
            }
            TurnOp::Progress {
                turn,
                doing: Some(doing),
                ..
            } => {
                let Floor {
                    subjects, turns, ..
                } = &mut *floor;
                let Some(whose) = turns.get(&turn) else {
                    // Begun before this hub started: taken up from the log.
                    return self.adopt(turn, floor);
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
                let Floor {
                    subjects, turns, ..
                } = floor;
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
                let Floor {
                    subjects,
                    turns,
                    looked,
                } = floor;
                looked.remove(&turn);
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
        match self.handed_off(&job) {
            true => tokio::spawn(async move { gists.handed(job).await }),
            false => tokio::spawn(async move { gists.written(job).await }),
        };
    }

    async fn ended_unseen(&self, turn: Uuid, outcome: Outcome, answer: Option<String>) {
        let t = match self.0.kept.turn(turn).await {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(e) => {
                return warn!(target: "floor", error = %format!("{e:#}"), "reading a turn for its gist failed")
            }
        };
        let job = Job {
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
        };
        match self.handed_off(&job) {
            true => self.handed(job).await,
            false => self.written(job).await,
        }
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
        let unwritten = |cause: String| {
            self.unwritten(
                &format!("{:?}", job.whose),
                &cause,
                "the model couldn't write a gist; it is said plainly",
            )
        };
        let written = match tokio::time::timeout(gist::WRITE_WITHIN, asked).await {
            Ok(Ok(text)) => {
                let written = gist::parse(&text);
                if written.is_none() {
                    unwritten(format!(
                        "the model's answer ({} characters) had no line saying what it is doing now",
                        text.chars().count()
                    ));
                }
                written
            }
            Ok(Err(e)) => {
                unwritten(format!("the model call failed: {e:#}"));
                None
            }
            Err(_) => {
                unwritten(format!(
                    "the model gave no answer within {} seconds",
                    gist::WRITE_WITHIN.as_secs()
                ));
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
            None => {
                let (plain_purpose, now) = gist::plainly(&input);
                let due = gist::purpose_due(purpose_at, job.moment, false, at, began);
                let renewed = match (&purpose, &job.subject) {
                    (None, _) => plain_purpose,
                    // A session's stale purpose follows its goal, else its
                    // latest prompt.
                    (Some(_), Subject::Session { .. }) if due => gist::latest_plainly(&input),
                    // An agent's follows its latest ask, while it has a
                    // Desk conversation in the window.
                    (Some(_), Subject::Agent { .. }) if due && began.is_some() => plain_purpose,
                    _ => None,
                };
                let fresh = renewed.is_some();
                // The model's line of this turn (or one said plainly in its
                // place) that it can't say anew is said plainly, noted, so
                // it never stands still unsaid; the model's next line has
                // no note.
                let unsaid = job.moment == Moment::Stepped
                    && kept.as_ref().is_some_and(|k| {
                        k.turn == job.turn && (k.by_model || gist::is_unsaid(&k.now))
                    });
                Gist {
                    purpose: renewed.or(purpose),
                    purpose_at: if fresh { Some(at) } else { purpose_at },
                    now: if unsaid { gist::unsaid(&now) } else { now },
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

    /// Logs `what` failed for `subject`, and its `cause`: at WARN no more
    /// than once per cause every [`WARN_EVERY`] across the hub (a missing
    /// login is the hub's state, not each subject's), at debug otherwise.
    fn unwritten(&self, subject: &str, cause: &str, what: &str) {
        if self.warn_due(&cause_of(what, cause)) {
            warn!(target: "floor", subject, cause, "{what}");
        } else {
            debug!(target: "floor", subject, cause, "{what}");
        }
    }

    /// Whether a failure of `cause` is logged at WARN now. The causes
    /// remembered are few; past [`WARN_CAUSES`] they are started again.
    fn warn_due(&self, cause: &str) -> bool {
        let mut warned = self.0.warned.lock();
        if warned
            .get(cause)
            .is_some_and(|at| at.elapsed() < WARN_EVERY)
        {
            return false;
        }
        if warned.len() >= WARN_CAUSES {
            warned.clear();
        }
        warned.insert(cause.to_string(), Instant::now());
        true
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
        let unwritten = |cause: String| {
            self.unwritten(
                &format!("paragraph {:?} {}", scope.owner, scope.personal),
                &cause,
                "the model couldn't write the floor's paragraph; the last one stands",
            )
        };
        let text = match tokio::time::timeout(gist::WRITE_WITHIN, asked).await {
            Ok(Ok(text)) => {
                let paragraph = gist::parse_summary(&text);
                if paragraph.is_none() {
                    unwritten("the model's answer was empty".into());
                }
                paragraph
            }
            Ok(Err(e)) => {
                unwritten(format!("the model call failed: {e:#}"));
                None
            }
            Err(_) => {
                unwritten(format!(
                    "the model gave no answer within {} seconds",
                    gist::WRITE_WITHIN.as_secs()
                ));
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

/// What a failure is warned of once for: `what` failed and its cause, its
/// numbers (counts, ids, codes' digits) left out and cut short, so the same
/// cause is one however often it comes.
fn cause_of(what: &str, cause: &str) -> String {
    let cause: String = cause
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(120)
        .collect();
    format!("{what}: {cause}")
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
/// agent's (on a hub that serves no agents, its plumbing's: the same plan,
/// [`crate::agent_registry::AgentRegistry::host`]), each call costed to the
/// person it is for.
pub(crate) struct HubModel {
    pub state: Arc<crate::gateway::GatewayState>,
    /// `[floor] fallback_providers`: whether a person's line may be written
    /// on another signed-in provider when the agent's own plan can't.
    pub fallback: bool,
}

/// Who a gist's call for `owner` runs as on `service`: the person it is
/// for, on their own login. The hub writes gists off the turn that caused
/// them, where no identity is in scope, so a call made bare reaches no
/// login of anyone's. `owner` is the floor's owner; with none (a personal
/// hub's Desk) the call is the agent's own background work, paid by
/// whoever pays for that.
fn principal_for(
    service: &crate::serve::AgentService,
    owner: Option<&str>,
) -> workforce_common::session::TurnPrincipal {
    let person = person_of(owner).map(|person| person.to_string());
    service.background_principal(person.as_deref())
}

/// The person `owner` names, if it names one.
fn person_of(owner: Option<&str>) -> Option<Uuid> {
    owner.and_then(|owner| workforce_common::subscription::Payer::classify(Some(owner)).person())
}

/// Run a gist's model call on its own agent `service` as `principal`
/// ([`principal_for`]). `quiet` (a personal hub's Desk) when the call may
/// have no owner by design: a login it draws on with no identity in scope
/// is then noted at debug, where a hub many people share warns of it.
/// `None`, with no call made, when no login serves `model` on it.
async fn written_on<F: std::future::Future>(
    service: &crate::serve::AgentService,
    principal: &workforce_common::session::TurnPrincipal,
    model: &str,
    quiet: bool,
    call: F,
) -> Option<F::Output> {
    let work = service.in_background(principal, crate::serve::Route::Model(Some(model)), call);
    match quiet {
        // A personal hub's Desk: the hub's own work for its person, which
        // may run on their accounts when the install's login is at its
        // limit.
        true => {
            crate::hub_core::logins::install_pool::hub_own_work(
                crate::subscription::unowned_by_design(work),
            )
            .await
        }
        false => work.await,
    }
}

/// The provider `service`'s own plan runs on, without its failover chain:
/// what a fallback call is made on, so a login that fails there never
/// falls through to a metered entry later in the chain.
fn own_provider(
    service: &crate::serve::AgentService,
) -> Arc<dyn workforce_agent::llm::provider::LlmProvider> {
    service
        .failover_provider
        .as_ref()
        .map_or_else(|| Arc::clone(&service.llm), |chain| chain.primary())
}

/// Run a gist's model call on a fallback agent `service`, as `person` and
/// on `person`'s own `kind` login only: never a login the agent or the hub
/// shares, and never a metered provider. `None`, with no call made, when
/// `person` has no usable `kind` login there.
async fn written_as<F: std::future::Future>(
    service: &crate::serve::AgentService,
    person: Uuid,
    kind: workforce_common::subscription::SubscriptionKind,
    call: F,
) -> Option<F::Output> {
    let principal = workforce_common::session::TurnPrincipal {
        run_as: Some(person.to_string()),
        bill_to: Some(person.to_string()),
    };
    crate::subscription::personal_logins_only(crate::user_config::as_principal(&principal, async {
        match service.logins.resolve(kind) {
            Ok(_) => Some(call.await),
            Err(_) => None,
        }
    }))
    .await
}

/// A gist's model call for `owner` on `service`'s own login
/// ([`written_on`]); `None` when nobody's login serves `model`: the gist is
/// then said plainly, as it is without a model.
#[cfg(test)]
async fn written_for<F: std::future::Future>(
    service: &crate::serve::AgentService,
    owner: Option<&str>,
    model: &str,
    call: F,
) -> Option<F::Output> {
    written_on(service, &principal_for(service, owner), model, true, call).await
}

/// What a gist may fall back on when `primary`'s own plan has no usable
/// login (signed out, cooling down or at its limit): of `others`, as
/// `(provider, agent)`, one agent on each other subscription provider, in
/// the order given, with the kind of login it runs on. A metered provider
/// is never fallen back on: a plan at its limit never runs up a bill.
fn fallbacks<T>(
    primary: &str,
    others: impl IntoIterator<Item = (String, T)>,
) -> Vec<(workforce_common::subscription::SubscriptionKind, T)> {
    let mut seen = HashSet::from([primary.to_string()]);
    others
        .into_iter()
        .filter_map(|(key, agent)| {
            let kind = workforce_agent::llm::provider::ProviderKind::from_str_loose(&key)?
                .subscription_route()?
                .kind();
            seen.insert(key).then_some((kind, agent))
        })
        .collect()
}

/// The answer to `system` and `user` for `owner`, on `primary`'s cheap
/// model, else, when `primary`'s plan has no usable login or its call
/// fails, on another signed-in provider's ([`fallbacks`] of `others`).
/// A fallback is only for a person in scope, only on an agent that
/// `admits` them (`(agent, person)`), and only on their own login
/// ([`written_as`]); its cost is theirs, kept on `primary`'s ledger.
#[cfg(test)]
async fn answer_on(
    primary: Arc<crate::serve::AgentService>,
    others: Vec<Arc<crate::serve::AgentService>>,
    admits: impl Fn(&str, &str) -> bool,
    owner: Option<&str>,
    system: &str,
    user: &str,
) -> eyre::Result<String> {
    answer_with(
        primary,
        others,
        admits,
        true,
        owner,
        system,
        user,
        Call::GIST,
    )
    .await
    .map(|(text, _)| text)
}

/// What a call on the floor's model is for: how long its answer may be,
/// and what its cost is booked as.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Call {
    pub max_tokens: u32,
    pub task: &'static str,
    pub description: &'static str,
}

impl Call {
    /// A gist or the floor's paragraph.
    pub(crate) const GIST: Call = Call {
        max_tokens: 400,
        task: "floor_gist",
        description: "floor:gist",
    };

    /// A session's running account, with its turn-end gist; booked apart
    /// so it shows on its own in spend.
    pub(crate) const ACCOUNT: Call = Call {
        max_tokens: workforce_desk::account::ANSWER_TOKENS,
        task: workforce_desk::account::TASK,
        description: "desk:account",
    };

    /// A running turn's `now` in a session's account.
    pub(crate) const ACCOUNT_NOW: Call = Call {
        max_tokens: 200,
        task: workforce_desk::account::TASK,
        description: "desk:account-now",
    };

    /// A question about a session, answered from its exchanges.
    pub(crate) const ACCOUNT_QUESTION: Call = Call {
        max_tokens: workforce_desk::account::QUESTION_TOKENS,
        task: workforce_desk::account::QUESTION_TASK,
        description: "desk:account-question",
    };
}

/// [`answer_on`], `quiet` as [`written_on`] has it, for `call`: the text,
/// and the model that wrote it.
#[allow(clippy::too_many_arguments)]
async fn answer_with(
    primary: Arc<crate::serve::AgentService>,
    others: Vec<Arc<crate::serve::AgentService>>,
    admits: impl Fn(&str, &str) -> bool,
    quiet: bool,
    owner: Option<&str>,
    system: &str,
    user: &str,
    call: Call,
) -> eyre::Result<(String, String)> {
    use workforce_agent::llm::provider::with_reasoning_suppressed;
    use workforce_agent::llm::types::LlmMessage;
    use workforce_agent::runner::with_served_log;
    use workforce_common::cost::{CostEntry, TokenCounts};
    use workforce_common::subscription::Origin;

    let person = person_of(owner);
    let mut others = match person {
        Some(person) => others
            .into_iter()
            .filter(|s| s.agent_id != primary.agent_id)
            .filter(|s| admits(s.agent_id.as_str(), &person.to_string()))
            .collect(),
        // Nobody's own login to draw on: the agent's own plan or nothing.
        None => vec![],
    };
    others.sort_by(|a, b| a.agent_id.as_str().cmp(b.agent_id.as_str()));
    let others = others.into_iter().map(|s| {
        let key = s.tier_key.read().clone();
        (key, s)
    });
    let primary_key = primary.tier_key.read().clone();
    let candidates = std::iter::once((None, Arc::clone(&primary))).chain(
        fallbacks(&primary_key, others)
            .into_iter()
            .map(|(kind, s)| (Some(kind), s)),
    );
    let messages = [LlmMessage::user(user.to_string())];
    let principal = principal_for(&primary, owner);
    let mut failed = None;
    for (kind, service) in candidates {
        let model = floor_model(&service.tier_registry(), &service.tier_key.read())
            .unwrap_or_else(|| crate::turn_loop::cheap_model(&service));
        let llm = match kind {
            None => Arc::clone(&service.llm),
            Some(_) => own_provider(&service),
        };
        let asked = with_served_log(with_reasoning_suppressed(llm.create_message(
            Some(system),
            &messages,
            &[],
            Some(&model),
            Some(call.max_tokens),
            true,
        )));
        let ran = match (kind, person) {
            (None, _) => written_on(&service, &principal, &model, quiet, asked).await,
            (Some(kind), Some(person)) => written_as(&service, person, kind, asked).await,
            (Some(_), None) => None,
        };
        let Some((response, served)) = ran else {
            continue;
        };
        let response = match response {
            Ok(response) => response,
            Err(e) => {
                failed = Some(e);
                continue;
            }
        };
        let identity = match kind {
            None => owner.map(str::to_string),
            Some(_) => {
                debug!(target: "floor", agent = %primary.agent_id, on = %service.agent_id, "a gist is written on the person's other signed-in plan");
                person.map(|p| p.to_string())
            }
        };
        primary.cost_tracker.record(CostEntry {
            task_type: Some(call.task),
            identity_id: identity.as_deref(),
            description: Some(call.description),
            served,
            ..CostEntry::new(
                primary.agent_id.as_str(),
                &response.model,
                TokenCounts::from(&response.usage),
                Origin::Floor,
            )
        });
        return Ok((
            response.all_text().unwrap_or_default(),
            response.model.clone(),
        ));
    }
    Err(failed.unwrap_or_else(|| {
        eyre::eyre!(
            "no signed-in plan could take the call: each is signed out, cooling down or at its limit"
        )
    }))
}

impl HubModel {
    /// The answer to `system` and `user` for `call`, on `subject`'s account
    /// (none: the floor's own) for `owner`, as gists are written: the text,
    /// and the model that wrote it.
    pub(crate) async fn answer_for(
        &self,
        subject: Option<&Subject>,
        owner: Option<&str>,
        system: &str,
        user: &str,
        call: Call,
    ) -> eyre::Result<(String, String)> {
        let registry = &self.state.registry;
        let service = match subject {
            Some(Subject::Agent { agent_id, .. }) => registry.get(agent_id),
            _ => None,
        }
        .or_else(|| registry.host())
        .ok_or_else(|| eyre::eyre!("no agent to write with"))?;
        let others = match self.fallback {
            true => registry.all_services(),
            false => vec![],
        };
        let admits = |agent: &str, person: &str| registry.admits(agent, person);
        answer_with(
            service,
            others,
            admits,
            self.fallback,
            owner,
            system,
            user,
            call,
        )
        .await
    }
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
        self.answer_for(subject, owner, system, user, Call::GIST)
            .await
            .map(|(text, _)| text)
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
        /// The turns as logged.
        turns: Mutex<HashMap<Uuid, Turn>>,
        /// Reading a turn fails.
        unreadable: Mutex<bool>,
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

        async fn turn(&self, id: Uuid) -> eyre::Result<Option<Turn>> {
            if *self.unreadable.lock() {
                eyre::bail!("the log is down");
            }
            Ok(self.turns.lock().get(&id).cloned())
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
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_the_model_cant_say_is_said_plainly_and_noted_until_it_can() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, told) = gists(Arc::clone(&model));
        let id = Uuid::new_v4();
        gists.saw(&begin_for(started(id), true));
        settle().await;
        let first = memory.written.lock()[0].clone();
        assert!(first.by_model);

        *model.say.lock() = String::new();
        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        gists.saw(&step(id, "● Bash cargo test"));
        settle().await;
        let noted = memory.written.lock()[1].clone();
        assert!(!noted.by_model);
        assert_eq!(
            noted.now, "Working on it now. (Couldn't describe this right now.)",
            "the line moves on, said plainly, and says why"
        );
        assert_eq!(noted.purpose, first.purpose, "the model's purpose stands");
        assert_eq!(noted.purpose_at, first.purpose_at);
        assert_eq!(*told.lock(), 2, "and its owner's floor told");

        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        gists.saw(&step(id, "● Read README"));
        settle().await;
        assert!(
            gist::is_unsaid(&memory.written.lock()[2].now),
            "still noted while the model can't say it"
        );

        *model.say.lock() = NOTE.into();
        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        gists.saw(&step(id, "● Bash cargo build"));
        settle().await;
        let next = memory.written.lock()[3].clone();
        assert!(next.by_model);
        assert_eq!(next.now, "Testing the build.", "the note clears");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_model_a_stale_sessions_purpose_follows_its_latest_prompt() {
        let (gists, memory, _) = gists(Scripted::new("", Duration::ZERO));
        let id = Uuid::new_v4();
        let whose = (Some("u1".to_string()), SubjectKey::session(id));
        *memory.asked.lock() = Asked {
            asks: vec!["Plan the budget".into(), "Fix the login page".into()],
            began: None,
        };
        let session = |task: &str| Turn {
            subject: Subject::Session {
                id,
                engine: Engine::Claude,
            },
            name: "release".into(),
            task: Some(task.into()),
            ..started(Uuid::new_v4())
        };
        let budget = "Planning next year's budget.";
        memory.gists.lock().insert(
            whose.clone(),
            kept_purpose(budget, chrono::Duration::minutes(10)),
        );
        gists.saw(&begin_for(session("Ship the release"), true));
        settle().await;
        assert_eq!(
            memory.written.lock()[0].purpose.as_deref(),
            Some(budget),
            "a purpose inside its window stands"
        );

        memory
            .gists
            .lock()
            .insert(whose, kept_purpose(budget, chrono::Duration::minutes(40)));
        gists.saw(&begin_for(session("Ship the release"), true));
        settle().await;
        let written = memory.written.lock()[1].clone();
        assert_eq!(
            written.purpose.as_deref(),
            Some("Ship the release."),
            "a stale one follows its latest prompt"
        );
        assert!(written.purpose_at > Some(crew::now() - chrono::Duration::minutes(1)));
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

    /// A session a boss oversees has its turn-end gist written from its
    /// account, with no call of the floor's own; when the account brings
    /// none in time, the floor writes it as ever.
    #[tokio::test(start_paused = true)]
    async fn a_handed_off_turn_end_is_written_from_the_account_or_by_the_floor_when_none_comes() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, told) = gists(Arc::clone(&model));
        gists.hand_turn_ends(Arc::new(|_| true));
        let id = Uuid::new_v4();
        let session = |turn: Uuid| Turn {
            subject: Subject::Session {
                id,
                engine: Engine::Claude,
            },
            name: "login-fix".into(),
            ..started(turn)
        };
        let ends = |turn: Uuid| TurnOp::End {
            turn,
            at: crew::now(),
            outcome: Outcome::Finished,
            answer: Some("It builds.".into()),
            question: None,
        };
        let first = Uuid::new_v4();
        gists.saw(&begin_for(session(first), true));
        settle().await;
        let calls = model.asked.lock().len();
        assert_eq!(
            memory.written.lock().len(),
            1,
            "its beginning is the floor's"
        );
        gists.saw(&ends(first));
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            1,
            "its end waits for the account"
        );
        gists
            .account_gist(
                id,
                Some("Fixing the login page.".into()),
                Some("Fixed the login page; the fix is in review.".into()),
                true,
            )
            .await;
        {
            let written = memory.written.lock();
            assert_eq!(written.len(), 2);
            let g = written.last().unwrap();
            assert_eq!(g.now, "Fixed the login page; the fix is in review.");
            assert_eq!(g.purpose.as_deref(), Some("Fixing the login page."));
            assert_eq!(g.turn, first, "the turn it ended");
            assert!(g.by_model);
        }
        assert!(*told.lock() >= 2, "the floor is told");
        assert_eq!(
            model.asked.lock().len(),
            calls,
            "no call of the floor's own"
        );
        // Long after, the floor never writes that turn's end again.
        tokio::time::sleep(HANDED_FOR * 2).await;
        settle().await;
        assert_eq!(memory.written.lock().len(), 2);

        // A turn whose account never comes: the floor writes it itself.
        let second = Uuid::new_v4();
        gists.saw(&begin_for(session(second), true));
        settle().await;
        gists.saw(&ends(second));
        settle().await;
        let before = memory.written.lock().len();
        tokio::time::sleep(HANDED_FOR + Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            before + 1,
            "written by the floor in the end"
        );
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

    #[tokio::test(start_paused = true)]
    async fn a_turn_begun_before_the_hub_started_is_taken_up_on_its_next_step() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, told) = gists(Arc::clone(&model));
        let id = Uuid::new_v4();
        let mut running = started(id);
        running.doing = Some("● Bash cargo test".into());
        memory.turns.lock().insert(id, running);
        let ended = Uuid::new_v4();
        let mut done = started(ended);
        done.ended_at = Some(crew::now());
        memory.turns.lock().insert(ended, done);

        gists.saw(&step(ended, "● Read README"));
        settle().await;
        assert!(
            memory.written.lock().is_empty(),
            "an ended turn is not taken up"
        );

        gists.saw(&step(id, "● Bash cargo test"));
        gists.saw(&step(id, "● Read README"));
        settle().await;
        assert_eq!(memory.written.lock().len(), 1, "taken up and said at once");
        assert!(memory.written.lock()[0].by_model);
        assert_eq!(memory.written.lock()[0].turn, id);
        assert!(model.asked.lock()[0].contains("- Bash cargo test"));
        assert_eq!(*told.lock(), 1);

        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        gists.saw(&step(id, "● Bash cargo build"));
        settle().await;
        assert_eq!(memory.written.lock().len(), 2, "its line keeps moving");
        assert!(model.asked.lock()[1].contains("- Bash cargo build"));

        gists.saw(&TurnOp::End {
            turn: id,
            at: crew::now(),
            outcome: Outcome::Finished,
            answer: Some("It builds.".into()),
            question: None,
        });
        settle().await;
        assert_eq!(memory.written.lock().len(), 3, "and is said as it ends");
        assert!(model.asked.lock()[2].contains("Its latest reply: It builds."));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_that_lands_after_the_turn_ended_never_opens_it_again() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let id = Uuid::new_v4();
        memory.turns.lock().insert(id, started(id));
        // Its step and its end arrive before the step's lookup lands.
        gists.saw(&step(id, "● Bash cargo test"));
        gists.saw(&TurnOp::End {
            turn: id,
            at: crew::now(),
            outcome: Outcome::Finished,
            answer: Some("It builds.".into()),
            question: None,
        });
        settle().await;
        let written = memory.written.lock().clone();
        assert_eq!(written.len(), 1, "only its end is said: {written:?}");
        assert!(model.asked.lock()[0].contains("Its latest reply: It builds."));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_lookup_is_warned_of_once_and_tried_again_on_the_next_step() {
        let (lines, _guard, _others) = Lines::capture();
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        *memory.unreadable.lock() = true;
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        for id in [one, two] {
            let mut running = started(id);
            running.doing = Some("● Bash cargo test".into());
            memory.turns.lock().insert(id, running);
            gists.saw(&step(id, "● Bash cargo test"));
        }
        settle().await;
        let failed = "a turn begun before the hub started couldn't be taken up";
        assert_eq!(lines.at("WARN", failed).len(), 1, "one WARN for the cause");
        assert_eq!(lines.at("DEBUG", failed).len(), 1);
        assert!(memory.written.lock().is_empty());

        *memory.unreadable.lock() = false;
        gists.saw(&step(one, "● Read README"));
        settle().await;
        assert_eq!(
            memory.written.lock().len(),
            1,
            "looked up again and taken up"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_the_hub_ran_itself_is_not_taken_up_after_a_restart() {
        let model = Scripted::new(NOTE, Duration::ZERO);
        let (gists, memory, _) = gists(Arc::clone(&model));
        let session = Uuid::new_v4();
        model.hubs.lock().push(session);
        let id = Uuid::new_v4();
        memory.turns.lock().insert(
            id,
            Turn {
                subject: Subject::Session {
                    id: session,
                    engine: Engine::Claude,
                },
                ..started(id)
            },
        );
        gists.saw(&step(id, "● Bash cargo test"));
        settle().await;
        assert!(memory.written.lock().is_empty());
        assert!(model.asked.lock().is_empty());
    }

    /// What the log says while the guard is held, every level from debug.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Lines {
        fn capture() -> (Self, tracing::dispatcher::DefaultGuard, tracing::Dispatch) {
            let lines = Self::default();
            let writer = lines.clone();
            let dispatch = tracing::Dispatch::new(
                tracing_subscriber::fmt()
                    .with_writer(move || writer.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::DEBUG)
                    .finish(),
            );
            // A second dispatcher makes every callsite ask each one, so
            // another test's thread can't leave one uninterested.
            let others = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
            let guard = tracing::dispatcher::set_default(&dispatch);
            (lines, guard, others)
        }

        /// The lines at `level` that say `text`.
        fn at(&self, level: &str, text: &str) -> Vec<String> {
            String::from_utf8_lossy(&self.0.lock())
                .lines()
                .filter(|l| l.contains(&format!(" {level} ")) && l.contains(text))
                .map(str::to_string)
                .collect()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_gist_is_warned_of_with_its_cause_once_per_cause_every_ten_minutes() {
        let (lines, _guard, _others) = Lines::capture();
        let model = Scripted::new("", Duration::ZERO);
        let (gists, _, _) = gists(Arc::clone(&model));
        let failed = "the model couldn't write a gist";
        let id = Uuid::new_v4();
        gists.saw(&begin_for(started(id), true));
        settle().await;
        let warned = lines.at("WARN", failed);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].contains("the model call failed: no model"),
            "{}",
            warned[0]
        );

        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        gists.saw(&step(id, "● Bash cargo test"));
        settle().await;
        assert_eq!(
            lines.at("WARN", failed).len(),
            1,
            "not again inside ten minutes"
        );
        assert_eq!(lines.at("DEBUG", failed).len(), 1, "but kept at debug");

        let other = Turn {
            subject: Subject::Agent {
                agent_id: "ranger".into(),
                session_id: "dm".into(),
            },
            ..started(Uuid::new_v4())
        };
        gists.saw(&begin_for(other.clone(), true));
        settle().await;
        assert_eq!(
            lines.at("WARN", failed).len(),
            1,
            "the same cause for another subject is the hub's state, warned of once"
        );
        assert_eq!(lines.at("DEBUG", failed).len(), 2);

        *model.say.lock() = "Sure, here is a note.".into();
        gists.saw(&begin_for(other.clone(), true));
        settle().await;
        let warned = lines.at("WARN", failed);
        assert_eq!(warned.len(), 2, "another cause is its own");
        assert!(
            warned[1].contains("had no line saying what it is doing now"),
            "{}",
            warned[1]
        );
        assert!(
            !warned[1].contains("Sure, here is a note"),
            "its text is never logged"
        );
        *model.say.lock() = "Sure.".into();
        gists.saw(&begin_for(started(Uuid::new_v4()), true));
        settle().await;
        assert_eq!(
            lines.at("WARN", failed).len(),
            2,
            "an answer of another length is the same cause"
        );

        *model.say.lock() = String::new();
        tokio::time::sleep(Duration::from_secs(8 * 60)).await;
        gists.saw(&step(id, "● Read README"));
        settle().await;
        let warned = lines.at("WARN", failed);
        assert_eq!(warned.len(), 3, "ten minutes on, warned again");
        assert!(warned[2].contains("the model call failed"), "{}", warned[2]);

        let slow = Scripted::new(NOTE, Duration::from_secs(60));
        let (gists, _, _) = self::gists(slow);
        gists.saw(&begin_for(started(Uuid::new_v4()), true));
        tokio::time::sleep(Duration::from_secs(46)).await;
        settle().await;
        assert!(lines
            .at("WARN", failed)
            .iter()
            .any(|l| l.contains("no answer within 45 seconds")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_paragraph_is_warned_of_with_its_cause() {
        let (lines, _guard, _others) = Lines::capture();
        let (gists, _, _) = gists(Scripted::new("", Duration::ZERO));
        let scope = Scope {
            owner: "u1".into(),
            personal: true,
        };
        let entry = Happening {
            who: Who {
                kind: workforce_desk::crew::PartyKind::Agent,
                id: "scout".into(),
                name: "Scout".into(),
                engine: None,
            },
            doing: Doing::Working,
            gist: Said {
                purpose: None,
                now: "Testing.".into(),
                at: crew::now(),
            },
        };
        gists.summary(&scope, vec![entry], crew::now()).await;
        settle().await;
        let warned = lines.at("WARN", "the model couldn't write the floor's paragraph");
        assert_eq!(warned.len(), 1);
        assert!(warned[0].contains("the model call failed"), "{}", warned[0]);
    }

    #[test]
    fn a_gist_falls_back_on_one_agent_of_each_other_signed_in_provider() {
        use workforce_common::subscription::SubscriptionKind;
        let others = [
            ("claude-code", "pair"),
            ("anthropic", "metered"),
            ("chatgpt", "allan"),
            ("chatgpt", "allan-two"),
            ("codex", "coder"),
            ("gemini", "metered-too"),
        ]
        .map(|(key, agent)| (key.to_string(), agent));
        assert_eq!(
            fallbacks("claude-code", others),
            vec![
                (SubscriptionKind::ChatGpt, "allan"),
                (SubscriptionKind::ChatGpt, "coder")
            ],
            "its own provider and metered ones are never fallen back on"
        );
    }

    #[test]
    fn the_fallback_is_off_unless_a_hub_turns_it_on() {
        use workforce_common::settings::WorkforceConfig;
        assert!(!WorkforceConfig::default().floor.fallback_providers);
        let on: WorkforceConfig = toml::from_str("[floor]\nfallback_providers = true\n").unwrap();
        assert!(on.floor.fallback_providers);
    }

    /// A ChatGPT agent `coder` on another provider (Codex) than `primary`.
    async fn codex_agent(
        dir: &std::path::Path,
        server: &wiremock::MockServer,
    ) -> Arc<crate::serve::AgentService> {
        let fallback = crate::serve::testing::chatgpt_service(dir, server, |config| {
            config.agents.push(workforce_common::settings::AgentConfig {
                id: "coder".into(),
                default: true,
                ..Default::default()
            });
        })
        .await;
        *fallback.tier_key.write() = "codex".into();
        fallback
    }

    #[tokio::test]
    async fn a_gist_falls_back_only_for_a_person_and_only_on_their_own_login() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let primary = chatgpt_agent(tmp.path(), &server, None).await;
        let other = tempfile::tempdir().unwrap();
        let fallback = codex_agent(other.path(), &server).await;
        // The other agent shares a login: never a fallback's.
        let shared = chatgpt_login_at(&fallback, "codex/auth_json", "shared");
        crate::serve::testing::answer_bearer(&server, &shared).await;
        let others = || vec![Arc::clone(&primary), Arc::clone(&fallback)];
        let anyone = |_: &str, _: &str| true;
        let no_plan = "no signed-in plan could take the call";

        for owner in [None, Some("slack:U123"), Some(ALICE)] {
            let answer = answer_on(Arc::clone(&primary), others(), anyone, owner, "s", "u").await;
            let error = format!("{:#}", answer.unwrap_err());
            assert!(error.contains(no_plan), "{owner:?}: {error}");
        }
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no person, or only a shared login, is no fallback"
        );

        let alice = chatgpt_login_at(&fallback, &format!("users/{ALICE}/codex/auth_json"), ALICE);
        crate::serve::testing::answer_bearer(&server, &alice).await;
        let (theirs, its) = (
            primary.cost_tracker.record_count(),
            fallback.cost_tracker.record_count(),
        );
        let answer = answer_on(
            Arc::clone(&primary),
            others(),
            anyone,
            Some(ALICE),
            "s",
            "u",
        )
        .await;
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(bearers(&server).await, vec![format!("Bearer {alice}")]);
        assert_eq!(
            primary.cost_tracker.record_count(),
            theirs + 1,
            "costed where the person's own gist is"
        );
        assert_eq!(
            fallback.cost_tracker.record_count(),
            its,
            "never the other agent's"
        );
    }

    #[tokio::test]
    async fn a_gist_whose_own_call_fails_is_written_on_the_next_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let primary = chatgpt_agent(tmp.path(), &server, None).await;
        // Its own login, which the backend refuses.
        chatgpt_login_at(&primary, &format!("users/{ALICE}/codex/auth_json"), ALICE);
        let other = tempfile::tempdir().unwrap();
        let fallback = codex_agent(other.path(), &server).await;
        let alice = chatgpt_login_at(&fallback, &format!("users/{ALICE}/codex/auth_json"), ALICE);
        crate::serve::testing::answer_bearer(&server, &alice).await;
        let anyone = |_: &str, _: &str| true;
        let others = vec![Arc::clone(&primary), Arc::clone(&fallback)];

        let answer = answer_on(Arc::clone(&primary), others, anyone, Some(ALICE), "s", "u").await;
        assert!(answer.is_ok(), "{answer:?}");
        let seen = bearers(&server).await;
        assert!(seen.len() >= 2, "its own call was made first: {seen:?}");
        assert_eq!(seen.last(), Some(&format!("Bearer {alice}")));
    }

    #[tokio::test]
    async fn a_gist_never_falls_back_on_an_agent_its_person_may_not_use() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let primary = chatgpt_agent(tmp.path(), &server, None).await;
        let other = tempfile::tempdir().unwrap();
        let fallback = crate::serve::testing::chatgpt_service(other.path(), &server, |config| {
            config.agents.push(workforce_common::settings::AgentConfig {
                id: "coder".into(),
                default: true,
                ..Default::default()
            });
        })
        .await;
        *fallback.tier_key.write() = "codex".into();
        let alice = chatgpt_login_at(&fallback, &format!("users/{ALICE}/codex/auth_json"), ALICE);
        crate::serve::testing::answer_bearer(&server, &alice).await;
        let others = || vec![Arc::clone(&primary), Arc::clone(&fallback)];
        let asked = Mutex::new(vec![]);
        let refused = |agent: &str, person: &str| {
            asked.lock().push((agent.to_string(), person.to_string()));
            false
        };

        let answer = answer_on(
            Arc::clone(&primary),
            others(),
            refused,
            Some(ALICE),
            "s",
            "u",
        )
        .await;
        assert!(answer.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            *asked.lock(),
            vec![("coder".to_string(), ALICE.to_string())]
        );

        let admitted = |agent: &str, person: &str| agent == "coder" && person == ALICE;
        let answer = answer_on(
            Arc::clone(&primary),
            others(),
            admitted,
            Some(ALICE),
            "s",
            "u",
        )
        .await;
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(bearers(&server).await, vec![format!("Bearer {alice}")]);
    }

    #[tokio::test]
    async fn on_a_shared_hub_a_gist_with_no_owner_drawing_on_a_shared_login_is_still_warned_of() {
        use workforce_agent::llm::types::LlmMessage;
        let (lines, _guard, _others) = Lines::capture();
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = chatgpt_agent(tmp.path(), &server, None).await;
        let shared = chatgpt_login_at(&service, "codex/auth_json", "shared");
        crate::serve::testing::answer_bearer(&server, &shared).await;
        let messages = [LlmMessage::user("user".to_string())];
        let call = service.llm.create_message(
            Some("system"),
            &messages,
            &[],
            Some("gpt-6-luna"),
            Some(400),
            true,
        );
        let principal = principal_for(&service, None);
        assert!(written_on(&service, &principal, "gpt-6-luna", false, call)
            .await
            .is_some());
        assert!(!lines
            .at(
                "WARN",
                "a subscription login served a call with no identity in scope"
            )
            .is_empty());
    }

    #[tokio::test]
    async fn a_fallback_is_made_on_the_agents_own_plan_never_its_failover_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let chained = crate::serve::testing::chatgpt_service(tmp.path(), &server, |config| {
            config.llm.failover = vec!["ollama:mock".into()];
        })
        .await;
        let chain = chained
            .failover_provider
            .as_ref()
            .expect("a failover chain");
        let own = own_provider(&chained);
        let same = |a: &Arc<dyn workforce_agent::llm::provider::LlmProvider>,
                    b: &Arc<dyn workforce_agent::llm::provider::LlmProvider>| {
            std::ptr::eq(Arc::as_ptr(a).cast::<()>(), Arc::as_ptr(b).cast::<()>())
        };
        assert!(same(&own, &chain.primary()), "the chain's own provider");
        assert!(!same(&own, &chained.llm), "not the chain");
    }

    #[tokio::test]
    async fn a_gist_with_no_owner_drawing_on_a_shared_login_is_not_warned_of() {
        let (lines, _guard, _others) = Lines::capture();
        let tmp = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        let service = chatgpt_agent(tmp.path(), &server, None).await;
        let shared = chatgpt_login_at(&service, "codex/auth_json", "shared");
        crate::serve::testing::answer_bearer(&server, &shared).await;
        let gap = "a subscription login served a call with no identity in scope";

        assert!(write_for(&service, None).await.is_some());
        assert!(
            lines.at("WARN", gap).is_empty(),
            "{:?}",
            lines.at("WARN", gap)
        );
        assert!(!lines
            .at(
                "DEBUG",
                "a subscription login served background work that has no owner"
            )
            .is_empty());

        // Anywhere else, a call with no identity is still an identity gap.
        assert!(service
            .logins
            .resolve(workforce_common::subscription::SubscriptionKind::ChatGpt)
            .is_ok());
        assert_eq!(lines.at("WARN", gap).len(), 1);
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
