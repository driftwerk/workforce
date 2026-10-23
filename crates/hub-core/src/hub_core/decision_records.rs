//! What each decision was asked and what came back, kept so an install can
//! train and judge a decision model on its own decisions
//!.
//!
//! One table for every use. A record holds the `Input` its request was
//! built from — a person's message, a step's result — so it is strictly
//! write-only to the hub: no tool, endpoint or turn reads it back, and only
//! the offline `workforce decide gather` (run on the host by whoever
//! administers it) does. Postgres only: an install without it records
//! nothing.
//!
//! Beside it, who moved a workflow run by hand ([`Intervention`]): the
//! surest labels a branch has. They are kept here and not on the run, so
//! no prompt, template or replica from before them ever meets one.
//!
//! A person's records go with their identity, and every record goes
//! after `retention_days`; both hold for what an install recorded before
//! it switched recording off. The store itself, and opening, pruning and
//! the per-person operations on it, live in `decision_store`.
//!
//! Recording never touches the decision it records. What is to be kept
//! goes on a bounded queue one task drains ([`Recorder`]): a slow or failing
//! database delays no turn and no step, and what the queue cannot take or
//! the store refuses is counted and reported, never retried.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;
use workforce_decide::{Answer, Decision, Input, RequestBudget};
use workforce_orchestrator::workflow::instance::{Actor, Intervention, WorkflowInstance};

/// Writes that may wait for the store. Past this the newest is dropped.
const QUEUE: usize = 1_024;

/// How long one write may take before it is given up.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often what was not kept is reported, while there is any.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Whom and what a decision is for. Recorded beside it, never sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub agent_id: Option<String>,
    pub person: Option<String>,
    /// The kind of channel the request came in on (`slack`, `gateway`, …).
    pub channel: Option<String>,
    pub instance_id: Option<String>,
    pub step_id: Option<String>,
    /// Whether this agent's decisions are recorded (`[decision.records]`,
    /// the install's under the agent's own).
    pub recorded: bool,
}

/// One decision as it is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRecord {
    pub usage: String,
    pub agent_id: Option<String>,
    pub person: Option<String>,
    pub channel: Option<String>,
    pub instance_id: Option<String>,
    pub step_id: Option<String>,
    pub input: Input,
    /// The budget the request was built within.
    pub request_chars: usize,
    pub backend: String,
    pub model: String,
    /// The answers to the input's own questions.
    pub answers: BTreeMap<String, Answer>,
    /// Whether the answer was acted on; `false` for a shadow.
    pub acted: bool,
    /// What the path beside the model decided, when one ran: the LLM
    /// classifier's `on_topic` / `off_topic` for a shadowed scope gate.
    pub verdict: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: i64,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub record: NewRecord,
}

#[async_trait]
pub trait DecisionRecords: Send + Sync {
    async fn insert(&self, record: NewRecord) -> Result<i64>;
    /// Records of `usage` made at or after `since` whose id is past
    /// `after`, in the order they were kept, at most `limit`: a reader
    /// walks them a page at a time, passing the last id it was given.
    async fn list(
        &self,
        usage: &str,
        since: DateTime<Utc>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Record>>;
    /// Keep `intervention` on the run `instance_id`.
    async fn insert_intervention(
        &self,
        instance_id: &str,
        intervention: Intervention,
    ) -> Result<()>;
    /// Every intervention kept on each of `instance_ids`, oldest first.
    async fn interventions(
        &self,
        instance_ids: &[String],
    ) -> Result<BTreeMap<String, Vec<Intervention>>>;
    /// Remove every record and intervention made before `before`; returns
    /// how many went.
    async fn prune(&self, before: DateTime<Utc>) -> Result<u64>;
    /// Remove every record of a decision made for `person` and every
    /// intervention they made; returns how many went.
    async fn forget_person(&self, person: &str) -> Result<u64>;
    /// How many records and interventions name `person`.
    async fn person_records(&self, person: &str) -> Result<u64>;
    /// [`Self::person_records`] for every person named anywhere.
    async fn people(&self) -> Result<BTreeMap<String, u64>>;
    /// Make every record and intervention naming `from` name `to`; returns
    /// how many changed.
    async fn reassign_person(&self, from: &str, to: &str) -> Result<u64>;
}

/// The person behind `intervention`, when a person made it.
pub fn person_of(intervention: &Intervention) -> Option<&str> {
    match &intervention.actor {
        Actor::Person { id } => Some(id),
        Actor::Agent { .. } | Actor::System => None,
    }
}

/// One thing to keep.
#[derive(Debug)]
enum Kept {
    Decision(NewRecord),
    Intervention {
        instance_id: String,
        intervention: Intervention,
    },
}

impl Kept {
    async fn write(self, store: &dyn DecisionRecords) -> Result<()> {
        match self {
            Kept::Decision(record) => store.insert(record).await.map(|_| ()),
            Kept::Intervention {
                instance_id,
                intervention,
            } => store.insert_intervention(&instance_id, intervention).await,
        }
    }
}

/// What was not kept since the hub started.
#[derive(Debug, Default)]
struct Lost {
    /// The queue was full.
    dropped: AtomicU64,
    /// The store refused the write or took too long over it.
    failed: AtomicU64,
}

/// The way into the store from a turn or a step: a bounded queue that one
/// task drains. Handing something over never waits.
#[derive(Debug, Clone)]
pub struct Recorder {
    queue: mpsc::Sender<Kept>,
    lost: Arc<Lost>,
}

impl Recorder {
    /// A recorder writing to `store`, taking at most [`QUEUE`] writes
    /// ahead of it.
    pub fn start(store: Arc<dyn DecisionRecords>) -> Self {
        Self::with_queue(store, QUEUE)
    }

    fn with_queue(store: Arc<dyn DecisionRecords>, queue: usize) -> Self {
        let (queue, waiting) = mpsc::channel(queue);
        let lost = Arc::new(Lost::default());
        tokio::spawn(write(store, waiting, Arc::clone(&lost)));
        Self { queue, lost }
    }

    fn keep(&self, kept: Kept) {
        if self.queue.try_send(kept).is_err() {
            self.lost.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("decision.records.dropped").increment(1);
        }
    }

    /// Keep that `intervention` moved `instance` to where it now stands.
    pub fn moved(&self, instance: &WorkflowInstance, intervention: Intervention) {
        self.keep(Kept::Intervention {
            instance_id: instance.id.clone(),
            intervention: Intervention {
                to_step: instance.current_step_id.clone(),
                ..intervention
            },
        });
    }

    /// Keep that `intervention` ended the run `instance_id`.
    pub fn ended(&self, instance_id: &str, intervention: Intervention) {
        self.keep(Kept::Intervention {
            instance_id: instance_id.to_string(),
            intervention: Intervention {
                to_step: None,
                ..intervention
            },
        });
    }
}

/// Write what arrives on `waiting` to `store`, one at a time, and say once
/// a minute how much was not kept.
async fn write(
    store: Arc<dyn DecisionRecords>,
    mut waiting: mpsc::Receiver<Kept>,
    lost: Arc<Lost>,
) {
    let first = tokio::time::Instant::now() + REPORT_INTERVAL;
    let mut report = tokio::time::interval_at(first, REPORT_INTERVAL);
    report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_error = None;
    let mut reported = (0, 0);
    loop {
        tokio::select! {
            kept = waiting.recv() => {
                let Some(kept) = kept else { return };
                let failure = match tokio::time::timeout(WRITE_TIMEOUT, kept.write(store.as_ref())).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(_) => format!("no answer from the store in {} s", WRITE_TIMEOUT.as_secs()),
                };
                lost.failed.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("decision.records.failed").increment(1);
                last_error = Some(failure);
            }
            _ = report.tick() => {
                let now = (
                    lost.dropped.load(Ordering::Relaxed),
                    lost.failed.load(Ordering::Relaxed),
                );
                if now != reported {
                    warn!(
                        target: "decision",
                        dropped = now.0 - reported.0,
                        failed = now.1 - reported.1,
                        error = last_error.take().as_deref().unwrap_or("the queue was full"),
                        "decision records not kept"
                    );
                    reported = now;
                }
            }
        }
    }
}

/// Decisions answered and not yet kept: the use that asked adds what its
/// other path made of them, then keeps them.
#[must_use = "records are kept only by `keep`"]
#[derive(Debug, Default)]
pub struct Pending(Vec<NewRecord>);

impl Pending {
    /// The records this holds, for a test that writes them to a store itself.
    #[cfg(all(any(test, feature = "test-support"), feature = "postgres"))]
    pub fn into_records(self) -> Vec<NewRecord> {
        self.0
    }

    /// `input`'s answers out of `decision`, for `subject`; nothing when the
    /// subject's decisions are not recorded.
    pub fn of(
        subject: &Subject,
        input: &Input,
        budget: RequestBudget,
        decision: &Decision,
        acted: bool,
    ) -> Self {
        if !subject.recorded {
            return Self::default();
        }
        let answers: BTreeMap<String, Answer> = input
            .questions()
            .into_keys()
            .filter_map(|id| {
                let answer = decision.response.answers.get(&id)?.clone();
                Some((id, answer))
            })
            .collect();
        Self(vec![NewRecord {
            usage: input.usage().as_str().to_string(),
            agent_id: subject.agent_id.clone(),
            person: subject.person.clone(),
            channel: subject.channel.clone(),
            instance_id: subject.instance_id.clone(),
            step_id: subject.step_id.clone(),
            input: input.clone(),
            request_chars: budget.get(),
            backend: decision.backend.clone(),
            model: decision.response.model.clone(),
            answers,
            acted,
            verdict: None,
        }])
    }

    pub fn and(mut self, more: Pending) -> Self {
        self.0.extend(more.0);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Hand them to the install's recorder, each with `verdict`, without
    /// waiting for the write.
    pub fn keep(self, verdict: Option<&str>) {
        self.keep_in(recorder().as_ref(), verdict);
    }

    pub fn keep_in(self, recorder: Option<&Recorder>, verdict: Option<&str>) {
        let Some(recorder) = recorder else {
            return;
        };
        for record in self.0 {
            recorder.keep(Kept::Decision(NewRecord {
                verdict: verdict.map(str::to_string),
                ..record
            }));
        }
    }
}

static RECORDER: parking_lot::RwLock<Option<Recorder>> = parking_lot::RwLock::new(None);

/// Record through `recorder` from now on: the install's own, once its
/// store is open.
pub fn record_with(recorder: Recorder) {
    *RECORDER.write() = Some(recorder);
}

/// The install's recorder, while it records.
pub fn recorder() -> Option<Recorder> {
    RECORDER.read().clone()
}

/// Keep that `intervention` moved `instance` to where it now stands, while
/// the install records. An intervention says who moved which run between
/// which steps and holds nothing anyone asked, so it follows the install's
/// switch alone.
pub fn moved(instance: &WorkflowInstance, intervention: Intervention) {
    if let Some(recorder) = recorder() {
        recorder.moved(instance, intervention);
    }
}

/// Keep that `intervention` ended the run `instance_id`, while the install
/// records.
pub fn ended(instance_id: &str, intervention: Intervention) {
    if let Some(recorder) = recorder() {
        recorder.ended(instance_id, intervention);
    }
}

/// A store in memory, for tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryDecisionRecords {
    records: parking_lot::Mutex<Vec<Record>>,
    interventions: parking_lot::Mutex<Vec<(String, Intervention)>>,
    /// How long each write takes: a slow database.
    pub delay: Duration,
}

#[cfg(any(test, feature = "test-support"))]
impl InMemoryDecisionRecords {
    pub fn all(&self) -> Vec<Record> {
        self.records.lock().clone()
    }

    /// The interventions kept on `instance_id`, once `expected` have landed.
    pub async fn interventions_of(&self, instance_id: &str, expected: usize) -> Vec<Intervention> {
        let of = || {
            let kept = self.interventions.lock();
            let of = kept.iter().filter(|(id, _)| id == instance_id);
            of.map(|(_, i)| i.clone()).collect::<Vec<_>>()
        };
        for _ in 0..200 {
            if of().len() >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        of()
    }

    /// Every record, once the writes spawned so far have landed.
    pub async fn settled(&self, expected: usize) -> Vec<Record> {
        for _ in 0..200 {
            if self.all().len() >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.all()
    }
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl DecisionRecords for InMemoryDecisionRecords {
    async fn insert(&self, record: NewRecord) -> Result<i64> {
        tokio::time::sleep(self.delay).await;
        let mut records = self.records.lock();
        let id = records.len() as i64 + 1;
        records.push(Record {
            id,
            at: Utc::now(),
            record,
        });
        Ok(id)
    }

    async fn list(
        &self,
        usage: &str,
        since: DateTime<Utc>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Record>> {
        Ok(self
            .all()
            .into_iter()
            .filter(|r| r.record.usage == usage && r.at >= since && r.id > after)
            .take(limit)
            .collect())
    }

    async fn insert_intervention(
        &self,
        instance_id: &str,
        intervention: Intervention,
    ) -> Result<()> {
        tokio::time::sleep(self.delay).await;
        let kept = (instance_id.to_string(), intervention);
        self.interventions.lock().push(kept);
        Ok(())
    }

    async fn interventions(
        &self,
        instance_ids: &[String],
    ) -> Result<BTreeMap<String, Vec<Intervention>>> {
        let mut found: BTreeMap<String, Vec<Intervention>> = BTreeMap::new();
        for (instance_id, intervention) in self.interventions.lock().iter() {
            if instance_ids.contains(instance_id) {
                let kept = found.entry(instance_id.clone()).or_default();
                kept.push(intervention.clone());
            }
        }
        Ok(found)
    }

    async fn prune(&self, before: DateTime<Utc>) -> Result<u64> {
        let mut records = self.records.lock();
        let mut interventions = self.interventions.lock();
        let kept = records.len() + interventions.len();
        records.retain(|r| r.at >= before);
        interventions.retain(|(_, i)| i.at >= before);
        Ok((kept - records.len() - interventions.len()) as u64)
    }

    async fn forget_person(&self, person: &str) -> Result<u64> {
        let mut records = self.records.lock();
        let mut interventions = self.interventions.lock();
        let kept = records.len() + interventions.len();
        records.retain(|r| r.record.person.as_deref() != Some(person));
        interventions.retain(|(_, i)| person_of(i) != Some(person));
        Ok((kept - records.len() - interventions.len()) as u64)
    }

    async fn person_records(&self, person: &str) -> Result<u64> {
        let records = self.records.lock();
        let interventions = self.interventions.lock();
        Ok((records
            .iter()
            .filter(|r| r.record.person.as_deref() == Some(person))
            .count()
            + interventions
                .iter()
                .filter(|(_, i)| person_of(i) == Some(person))
                .count()) as u64)
    }

    async fn people(&self) -> Result<BTreeMap<String, u64>> {
        let records = self.records.lock();
        let interventions = self.interventions.lock();
        Ok(records
            .iter()
            .filter_map(|r| r.record.person.as_deref())
            .chain(interventions.iter().filter_map(|(_, i)| person_of(i)))
            .fold(BTreeMap::new(), |mut people, person| {
                *people.entry(person.to_string()).or_default() += 1;
                people
            }))
    }

    async fn reassign_person(&self, from: &str, to: &str) -> Result<u64> {
        let mut records = self.records.lock();
        let mut interventions = self.interventions.lock();
        let mut moved = 0;
        for record in records
            .iter_mut()
            .filter(|r| r.record.person.as_deref() == Some(from))
        {
            record.record.person = Some(to.to_string());
            moved += 1;
        }
        for (_, intervention) in interventions
            .iter_mut()
            .filter(|(_, i)| person_of(i) == Some(from))
        {
            intervention.actor = Actor::Person { id: to.to_string() };
            moved += 1;
        }
        Ok(moved)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use super::*;
    use serde_json::json;
    use workforce_decide::Response;

    pub fn decision(answers: serde_json::Value) -> Decision {
        Decision {
            response: Response {
                model: "fake-1".into(),
                answers: serde_json::from_value(answers).unwrap(),
                usage: Default::default(),
            },
            backend: "kev".into(),
            fell_back: false,
        }
    }

    /// Make the hub under test record into a store in memory, and hand it
    /// back: for a test of a path that records through the install's own
    /// recorder. Every test shares the one store, so tests running at once
    /// never take the recorder from each other; look in it only for what the
    /// test itself wrote. Its writer runs on a runtime of its own, which
    /// outlives any one test's.
    pub fn record_in_memory() -> Arc<InMemoryDecisionRecords> {
        static SHARED: std::sync::OnceLock<Arc<InMemoryDecisionRecords>> =
            std::sync::OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| {
            let runtime: &'static tokio::runtime::Runtime = Box::leak(Box::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("recorder runtime"),
            ));
            let _entered = runtime.enter();
            let store = Arc::new(InMemoryDecisionRecords::default());
            *RECORDER.write() = Some(Recorder::start(store.clone()));
            store
        }))
    }

    pub fn scope(message: &str) -> Input {
        Input::ScopeGate {
            message: message.into(),
            topic: "deploys".into(),
        }
    }

    pub fn noul(p: f64) -> Decision {
        decision(json!({
            "on_topic": {"type": "noul", "noul": p},
            "use_case": {"type": "choice", "choice": "action",
                         "probabilities": {"action": 0.8}, "confidence": 0.8},
        }))
    }

    pub fn moved_by(person: &str) -> Intervention {
        use workforce_orchestrator::workflow::instance::InterventionKind;
        Intervention {
            kind: InterventionKind::Goto,
            actor: Actor::Person { id: person.into() },
            at: Utc::now(),
            from_step: Some("review".into()),
            to_step: Some("fix".into()),
            outcome: None,
            applied: None,
            chosen: None,
        }
    }

    pub async fn reassign_person_contract(store: &dyn DecisionRecords) {
        let Pending(mut records) = Pending::of(
            &recorded(),
            &scope("one"),
            RequestBudget::DEFAULT,
            &noul(0.2),
            false,
        );
        let record = NewRecord {
            person: Some("bob".into()),
            ..records.remove(0)
        };
        store.insert(record.clone()).await.unwrap();
        store
            .insert_intervention("i-1", moved_by("bob"))
            .await
            .unwrap();
        store
            .insert_intervention("i-1", moved_by("carol"))
            .await
            .unwrap();

        assert_eq!(store.person_records("bob").await.unwrap(), 2);
        let people = store.people().await.unwrap();
        assert_eq!(
            (people.get("bob"), people.get("carol")),
            (Some(&2), Some(&1))
        );
        assert_eq!(store.reassign_person("bob", "dave").await.unwrap(), 2);
        assert_eq!(store.person_records("bob").await.unwrap(), 0);
        assert_eq!(store.person_records("dave").await.unwrap(), 2);
        let found = store.interventions(&["i-1".to_string()]).await.unwrap();
        let mut people: Vec<_> = found["i-1"].iter().filter_map(person_of).collect();
        people.sort_unstable();
        assert_eq!(people, ["carol", "dave"]);
        assert_eq!(store.forget_person("dave").await.unwrap(), 2);
    }

    pub fn recorded() -> Subject {
        Subject {
            agent_id: Some("ops".into()),
            person: Some("alice".into()),
            channel: Some("slack".into()),
            recorded: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[tokio::test]
    async fn a_record_keeps_the_input_its_own_answers_and_the_other_paths_verdict() {
        let store = Arc::new(InMemoryDecisionRecords::default());
        let input = scope("Roll back the deploy?");
        let pending = Pending::of(
            &recorded(),
            &input,
            RequestBudget::DEFAULT,
            &noul(0.9),
            false,
        );
        let recorder = Recorder::start(store.clone());
        pending.keep_in(Some(&recorder), Some("on_topic"));
        let kept = store.settled(1).await;
        assert_eq!(kept.len(), 1);
        let record = &kept[0].record;
        assert_eq!(record.usage, "scope_gate");
        assert_eq!(record.input, input);
        assert_eq!(record.request_chars, 1_500);
        assert_eq!(
            record.answers.keys().collect::<Vec<_>>(),
            ["on_topic"],
            "a question that rode along belongs to its own record"
        );
        assert_eq!(record.verdict.as_deref(), Some("on_topic"));
        assert_eq!(
            (
                record.person.as_deref(),
                record.agent_id.as_deref(),
                record.acted
            ),
            (Some("alice"), Some("ops"), false)
        );
        assert_eq!(
            (record.backend.as_str(), record.model.as_str()),
            ("kev", "fake-1")
        );
    }

    #[tokio::test]
    async fn an_agent_whose_decisions_are_not_recorded_leaves_nothing() {
        let store = Arc::new(InMemoryDecisionRecords::default());
        let unrecorded = Subject {
            recorded: false,
            ..recorded()
        };
        let pending = Pending::of(
            &unrecorded,
            &scope("hello"),
            RequestBudget::DEFAULT,
            &noul(0.9),
            true,
        );
        assert!(pending.is_empty());
        pending.keep_in(Some(&Recorder::start(store.clone())), None);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(store.all().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_store_delays_no_caller_and_what_the_queue_cannot_take_is_counted() {
        let store = Arc::new(InMemoryDecisionRecords {
            delay: Duration::from_secs(3_600),
            ..Default::default()
        });
        let recorder = Recorder::with_queue(store.clone(), 2);
        let one = || {
            Pending::of(
                &recorded(),
                &scope("hello"),
                RequestBudget::DEFAULT,
                &noul(0.9),
                true,
            )
        };
        one().keep_in(Some(&recorder), None);
        tokio::task::yield_now().await;
        let started = tokio::time::Instant::now();
        for _ in 0..10 {
            one().keep_in(Some(&recorder), None);
        }
        assert_eq!(started.elapsed(), Duration::ZERO, "no caller waits");
        assert_eq!(
            recorder.lost.dropped.load(Ordering::Relaxed),
            8,
            "one is being written, two wait, the rest are dropped and counted"
        );
        assert!(store.all().is_empty());

        tokio::time::sleep(WRITE_TIMEOUT * 3 + Duration::from_secs(1)).await;
        assert_eq!(
            recorder.lost.failed.load(Ordering::Relaxed),
            3,
            "a write the store never answers is given up, and counted"
        );
    }

    #[tokio::test]
    async fn an_intervention_is_kept_with_where_the_run_then_stood_and_goes_with_its_person() {
        use workforce_orchestrator::workflow::instance::{TriggerInfo, TriggerSource};
        let store = Arc::new(InMemoryDecisionRecords::default());
        let recorder = Recorder::start(store.clone());
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::Value::Null,
        };
        let mut run = WorkflowInstance::new("bug-fix", trigger, "review", None);
        let goto = Intervention {
            to_step: None,
            ..moved_by("alice")
        };
        run.current_step_id = Some("merge".into());
        recorder.moved(&run, goto);
        recorder.moved(&run, moved_by("bob"));
        let kept = store.interventions_of(&run.id, 2).await;
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].to_step.as_deref(), Some("merge"));
        let ids = [run.id.clone(), "another-run".to_string()];
        let found = store.interventions(&ids).await.unwrap();
        assert_eq!(found.keys().collect::<Vec<_>>(), [&run.id]);

        assert_eq!(store.forget_person("alice").await.unwrap(), 1);
        let left = store.interventions_of(&run.id, 1).await;
        assert_eq!(left.len(), 1);
        assert_eq!(person_of(&left[0]), Some("bob"));
    }

    #[tokio::test]
    async fn the_in_memory_store_reassigns_a_person() {
        reassign_person_contract(&InMemoryDecisionRecords::default()).await;
    }

    #[tokio::test]
    async fn the_in_memory_store_lists_by_use_and_prunes_by_age() {
        let store = InMemoryDecisionRecords::default();
        let pending = Pending::of(
            &recorded(),
            &scope("one"),
            RequestBudget::DEFAULT,
            &noul(0.2),
            true,
        )
        .and(Pending::of(
            &recorded(),
            &Input::Routing {
                message: "one".into(),
                agent: None,
                channel: None,
            },
            RequestBudget::DEFAULT,
            &noul(0.2),
            false,
        ));
        for record in pending.0 {
            store.insert(record).await.unwrap();
        }
        let epoch = DateTime::<Utc>::UNIX_EPOCH;
        assert_eq!(store.forget_person("bob").await.unwrap(), 0);
        assert_eq!(store.list("routing", epoch, 0, 10).await.unwrap().len(), 1);
        assert_eq!(
            store.list("scope_gate", epoch, 0, 0).await.unwrap().len(),
            0
        );
        assert_eq!(store.prune(epoch).await.unwrap(), 0);
        assert_eq!(
            store
                .prune(Utc::now() + chrono::Duration::seconds(5))
                .await
                .unwrap(),
            2
        );
    }
}
