//! Budgets and the breaker's I/O shell: the person's budgets kept and
//! changed, each subject's breaker fed what its sessions do and spend
//! ([`crate::breaker`] decides), and what it decides delivered: a steer into
//! the running turn, a hold from the next one, a Floor need.
//!
//! Off until the person asks for it: with no budget set and loop watching
//! off (the default), nothing here answers a hook, refuses a start or adds
//! a need.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_core::budget::{
    self, Budget, BudgetScope, OnExceed, Period, Pool, Standing, Subject, Unit, WARN_AT,
};

use super::{session_item, Desk, DeskError, Live};
use crate::breaker::{self as pure, Level, Reason, Signal, Step, Whose};
use crate::budget_store::Used;
use crate::crew::{By, Event, Kind, Party, PartyKind};
use crate::engine::Engine;
use crate::floor::{Answer, DecideRef, Need, NeedKind, Place, Who};
use crate::model::DeskEvent;

/// Where the breaker's own settings are kept.
const RULES_KEY: &str = "hub.breaker.rules";
/// How often held breakers are looked at again, so a hold whose period
/// ended lifts without new spend.
const LOOK_EVERY: Duration = Duration::from_secs(60);

/// The breaker's settings, beside the budgets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rules {
    /// Watch sessions for loops and repeated failures. Off unless the
    /// person turns it on.
    pub loops: bool,
}

/// The budgets and every subject's breaker, as the desk runs them.
#[derive(Default)]
pub(super) struct BreakerRun {
    budgets: Vec<Budget>,
    states: HashMap<String, pure::State>,
    rules: Rules,
    /// What the hub calls its agents, as last told.
    names: HashMap<String, String>,
}

/// What a running session's breaker has for it in this run.
#[derive(Debug, Default)]
pub(crate) struct LiveBreaker {
    /// Turns begun while it ran here (a prompt, or its own waking).
    turns: u64,
    /// BreakerOf from the turn after this one.
    hold_after: Option<u64>,
    /// A steer waiting for its next tool call to be delivered with.
    steer: Option<(Reason, String)>,
    /// What Desk typed into it and has yet to hear submitted, and whether
    /// the person sent it: the rest of what it is prompted with is typed
    /// in its terminal.
    typed: std::collections::VecDeque<(String, bool)>,
}

/// How many of Desk's own prompts are remembered until heard.
const TYPED_KEPT: usize = 8;

/// A breaker's subject key.
pub fn session_subject(id: Uuid) -> String {
    format!("session:{id}")
}

pub fn agent_subject(id: &str) -> String {
    format!("agent:{id}")
}

pub fn pool_subject(pool: &Pool) -> String {
    match pool {
        Pool::Desk => "pool:desk".into(),
        Pool::Group(g) => format!("pool:group:{g}"),
    }
}

/// What a subject key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerOf {
    Session(Uuid),
    Agent(String),
    Pool(Pool),
}

impl BreakerOf {
    pub fn parse(subject: &str) -> Option<Self> {
        match subject.split_once(':')? {
            ("session", id) => Uuid::parse_str(id).ok().map(Self::Session),
            ("agent", id) if !id.is_empty() => Some(Self::Agent(id.to_string())),
            ("pool", "desk") => Some(Self::Pool(Pool::Desk)),
            ("pool", rest) => rest
                .strip_prefix("group:")
                .filter(|g| !g.is_empty())
                .map(|g| Self::Pool(Pool::Group(g.to_string()))),
            _ => None,
        }
    }

    fn whose(&self) -> Whose {
        match self {
            Self::Session(_) => Whose::Session,
            Self::Agent(_) => Whose::Agent,
            Self::Pool(_) => Whose::Pool,
        }
    }
}

/// Reads what each local agent spent, every billing counted.
pub trait AgentSpend: Send + Sync {
    fn spent(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> eyre::Result<Vec<(String, Used)>>;
}

/// Whether an agent's next turn may run, by its budgets.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentVerdict {
    Proceed,
    /// Near or over a budget that only warns: tell the person.
    Warn(String),
    /// Held: its turns are refused until the person says Continue.
    Held(String),
}

/// A breaker as Desk shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakerView {
    pub subject: String,
    pub name: String,
    pub level: Level,
    /// While it holds: what the Floor asks, and what each answer does.
    pub ask: Option<String>,
    pub why: Option<String>,
}

/// A budget, and where what it counts stands now.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetView {
    #[serde(flatten)]
    pub budget: Budget,
    /// For a default (`each_*`): the furthest along of what it applies to.
    pub standing: Option<Standing>,
}

/// `GET /api/desk/budgets`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetsView {
    pub budgets: Vec<BudgetView>,
    pub breakers: Vec<BreakerView>,
    pub rules: Rules,
}

/// A budget to set: one for the same group, scope, period and unit is
/// replaced.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBudget {
    pub scope: BudgetScope,
    #[serde(default)]
    pub group: Option<String>,
    pub period: Period,
    pub unit: Unit,
    pub amount: f64,
    #[serde(default)]
    pub warn_at: Option<f64>,
    #[serde(default)]
    pub on_exceed: Option<OnExceed>,
}

/// A change to a budget's amounts.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetPatch {
    pub amount: Option<f64>,
    pub warn_at: Option<f64>,
    pub on_exceed: Option<OnExceed>,
}

/// What the sessions of `pool` spent, of each session's `spent`: every
/// session on Desk (one since forgotten too), or a group's members now.
/// The breaker and Settings count a total by this one rule.
fn sessions_used(
    pool: &Pool,
    spent: Option<&HashMap<Uuid, Used>>,
    groups: &HashMap<Uuid, Option<String>>,
) -> Used {
    let mut total = Used::default();
    for (id, u) in spent.into_iter().flatten() {
        let counts = match pool {
            Pool::Desk => true,
            Pool::Group(g) => groups.get(id).and_then(|x| x.as_deref()) == Some(g.as_str()),
        };
        if counts {
            total.add(*u);
        }
    }
    total
}

/// How long the Floor waits for the groups' budgets to be counted.
pub(super) const SPEND_WAIT: Duration = Duration::from_millis(500);
/// How long what was counted for the Floor is kept.
const SPEND_KEPT: Duration = Duration::from_secs(5);
/// How often a failing read of the agents' spend is warned about; the
/// rest are said at debug.
const SPEND_WARN_EVERY: Duration = Duration::from_secs(600);

/// Warns that the agents' spend couldn't be read, at most once in
/// [`SPEND_WARN_EVERY`]: it is read every minute and on every Floor read.
fn warn_agents_spend(error: &str) {
    static LAST: parking_lot::Mutex<Option<std::time::Instant>> = parking_lot::Mutex::new(None);
    let mut last = LAST.lock();
    if last.is_none_or(|at| at.elapsed() >= SPEND_WARN_EVERY) {
        *last = Some(std::time::Instant::now());
        tracing::warn!(error, "couldn't read agents' spend");
    } else {
        tracing::debug!(error, "couldn't read agents' spend");
    }
}

/// A tool call's key: the same call hashes the same.
fn key_of(label: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    label.hash(&mut h);
    h.finish()
}

/// "a day budget of $11", "a week budget of 2M tokens".
fn budget_words(b: &Budget) -> String {
    let amount = match b.unit {
        Unit::Usd => budget::usd(b.amount),
        Unit::Tokens => format!("{} tokens", budget::tokens(b.amount)),
    };
    format!("a {} budget of {amount}", b.period.words())
}

impl Desk {
    /// Reads the budgets, breakers and rules as kept.
    pub(super) async fn breaker_load(&self) {
        let store = &self.0.store;
        let budgets = store.budgets().await.unwrap_or_else(|e| {
            tracing::warn!(error = format!("{e:#}"), "couldn't read budgets");
            Vec::new()
        });
        let states = store.breakers().await.unwrap_or_else(|e| {
            tracing::warn!(error = format!("{e:#}"), "couldn't read breakers");
            HashMap::new()
        });
        let rules = match store.state(RULES_KEY).await {
            Ok(Some(e)) => serde_json::from_value(e.value).unwrap_or_default(),
            _ => Rules::default(),
        };
        {
            let mut run = self.0.breakers.lock();
            run.budgets = budgets;
            run.states = states;
            run.rules = rules;
        }
    }

    /// Looks at every breaker again each minute, until the desk stops.
    pub(super) async fn breaker_loop(self) {
        let mut look = tokio::time::interval(LOOK_EVERY);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = look.tick() => self.breaker_spend(None).await,
            }
        }
    }

    /// Counts budgets in `tz`, the person's (the hub's `user_timezone`, as
    /// its cost governor counts agents' budgets).
    pub fn count_budgets_in(&self, tz: chrono_tz::Tz) {
        *self.0.budget_tz.lock() = Some(tz);
    }

    /// The timezone budgets are counted in: the hub's, else the crew's
    /// once it is there; none before either says.
    pub(super) fn budget_tz(&self) -> Option<chrono_tz::Tz> {
        let set = *self.0.budget_tz.lock();
        set.or_else(|| self.reader().map(|c| c.timezone()))
    }

    fn rules(&self) -> Rules {
        self.0.breakers.lock().rules
    }

    /// What a subject is called now.
    fn subject_name(&self, held: &BreakerOf) -> String {
        match held {
            BreakerOf::Session(id) => self
                .view(*id)
                .map(|v| v.label().to_string())
                .unwrap_or_else(|_| "A session".into()),
            BreakerOf::Agent(id) => {
                let boss = self.scope_of_boss(id).is_some();
                let names = self.0.breakers.lock();
                crate::crew::shown_name(id, names.names.get(id).map(String::as_str), boss)
            }
            BreakerOf::Pool(Pool::Desk) => "Desk".into(),
            BreakerOf::Pool(Pool::Group(g)) => {
                let st = self.0.state.lock();
                st.layout
                    .groups
                    .iter()
                    .find(|x| x.id == *g)
                    .map_or_else(|| "A group".into(), |x| x.name.clone())
            }
        }
    }

    fn breaker_view(&self, subject: &str, state: &pure::State) -> Option<BreakerView> {
        let held = BreakerOf::parse(subject)?;
        let name = self.subject_name(&held);
        let said = pure::need_text(&name, held.whose(), &state.level);
        Some(BreakerView {
            subject: subject.to_string(),
            name,
            level: state.level.clone(),
            ask: said.as_ref().map(|(a, _)| a.clone()),
            why: said.map(|(_, w)| w),
        })
    }

    /// Every breaker that isn't clear.
    pub fn breakers(&self) -> Vec<BreakerView> {
        let states: Vec<(String, pure::State)> = self
            .0
            .breakers
            .lock()
            .states
            .iter()
            .filter(|(_, s)| s.level != Level::Clear)
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        let mut views: Vec<BreakerView> = states
            .iter()
            .filter_map(|(k, s)| self.breaker_view(k, s))
            .collect();
        views.sort_by(|a, b| a.subject.cmp(&b.subject));
        views
    }

    /// `subject`'s breaker, clear when it has none.
    pub fn breaker(&self, subject: &str) -> Option<BreakerView> {
        let state = self
            .0
            .breakers
            .lock()
            .states
            .get(subject)
            .cloned()
            .unwrap_or_default();
        self.breaker_view(subject, &state)
    }

    fn held(&self, subject: &str) -> Option<pure::State> {
        self.0
            .breakers
            .lock()
            .states
            .get(subject)
            .filter(|s| s.held())
            .cloned()
    }

    /// What holds the agent `id`'s turns, in plain words, while it is held.
    pub fn agent_held(&self, id: &str) -> Option<String> {
        let subject = agent_subject(id);
        let state = self.held(&subject)?;
        Some(self.breaker_view(&subject, &state)?.ask.unwrap_or_default())
    }

    /// The Floor's needs for what the breaker holds and the person hasn't
    /// said Keep paused to.
    pub fn breaker_needs(&self) -> Vec<Need> {
        let held: Vec<(String, pure::State)> = self
            .0
            .breakers
            .lock()
            .states
            .iter()
            .filter(|(_, s)| matches!(s.level, Level::Held { kept: false, .. }))
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        held.into_iter()
            .filter_map(|(subject, state)| {
                let Level::Held { since, .. } = state.level else {
                    return None;
                };
                let what = BreakerOf::parse(&subject)?;
                let view = self.breaker_view(&subject, &state)?;
                let (who, place) = match &what {
                    BreakerOf::Session(id) => (
                        Who {
                            kind: PartyKind::Session,
                            id: id.to_string(),
                            name: view.name.clone(),
                            engine: self.view(*id).ok().map(|v| v.engine),
                        },
                        Place::Session,
                    ),
                    BreakerOf::Agent(id) => (
                        Who {
                            kind: PartyKind::Agent,
                            id: id.clone(),
                            name: view.name.clone(),
                            engine: None,
                        },
                        Place::Dm,
                    ),
                    BreakerOf::Pool(_) => (
                        Who {
                            kind: PartyKind::Desk,
                            id: subject.clone(),
                            name: view.name.clone(),
                            engine: None,
                        },
                        Place::Elsewhere,
                    ),
                };
                Some(Need {
                    id: format!("breaker:{subject}"),
                    kind: NeedKind::Approval,
                    who,
                    ask: view.ask?,
                    why: view.why,
                    options: None,
                    chose: None,
                    since,
                    place,
                    answer: Answer::Decide {
                        at: DecideRef::Breaker { subject },
                    },
                    group: None,
                    boss: None,
                })
            })
            .collect()
    }

    /// Folds `signal` into `subject`'s breaker, keeps it, and does what it
    /// decides.
    fn breaker_signal(&self, subject: &str, signal: Signal, by: Option<&By>) -> Vec<Step> {
        let Some(held) = BreakerOf::parse(subject) else {
            return Vec::new();
        };
        let now = Utc::now();
        let (before, after, steps) = {
            let mut run = self.0.breakers.lock();
            let before = run.states.get(subject).cloned().unwrap_or_default();
            let (after, steps) = match held {
                BreakerOf::Pool(_) => pure::pool_step(before.clone(), signal, now),
                BreakerOf::Session(_) | BreakerOf::Agent(_) => {
                    pure::step(before.clone(), signal, now)
                }
            };
            if after == pure::State::default() {
                run.states.remove(subject);
            } else {
                run.states.insert(subject.to_string(), after.clone());
            }
            (before, after, steps)
        };
        if before == after {
            return steps;
        }
        let store = std::sync::Arc::clone(&self.0.store);
        let (key, kept) = (subject.to_string(), after.clone());
        tokio::spawn(async move {
            if let Err(e) = store.put_breaker(&key, &kept).await {
                tracing::warn!(subject = %key, error = format!("{e:#}"), "couldn't keep a breaker");
            }
        });
        for step in &steps {
            self.breaker_apply(&held, subject, step, &after, by);
        }
        if before.level != after.level {
            if let Some(view) = self.breaker_view(subject, &after) {
                self.announce(DeskEvent::Breaker {
                    breaker: Box::new(view),
                });
            }
        }
        steps
    }

    fn subject_party(&self, held: &BreakerOf, name: &str) -> Party {
        match held {
            BreakerOf::Session(id) => {
                self.view(*id)
                    .map(|v| Party::session(&v))
                    .unwrap_or_else(|_| Party {
                        kind: PartyKind::Session,
                        id: id.to_string(),
                        name: name.to_string(),
                        engine: None,
                        origin: None,
                    })
            }
            BreakerOf::Agent(id) => Party::agent(id, name),
            BreakerOf::Pool(pool) => Party {
                kind: PartyKind::Desk,
                id: pool_subject(pool),
                name: name.to_string(),
                engine: None,
                origin: None,
            },
        }
    }

    fn breaker_apply(
        &self,
        held: &BreakerOf,
        subject: &str,
        step: &Step,
        state: &pure::State,
        by: Option<&By>,
    ) {
        let name = self.subject_name(held);
        let party = self.subject_party(held, &name);
        match step {
            Step::Steer { reason, text } => {
                if let BreakerOf::Session(id) = held {
                    self.steer(*id, reason.clone(), text.clone());
                }
                self.note(Event::new(
                    Kind::Steered,
                    Party::breaker(),
                    Some(party),
                    format!("told {name}: {}", crate::crew::clip(text, 200)),
                ));
            }
            Step::Hold { .. } => {
                if let BreakerOf::Session(id) = held {
                    if let Some(l) = self.0.state.lock().live.get_mut(id) {
                        l.breaker.hold_after = Some(l.breaker.turns);
                    }
                }
                let ask = self.breaker_view(subject, state).and_then(|v| v.ask);
                let text = match held {
                    BreakerOf::Pool(_) => format!("held new sessions for {name}"),
                    BreakerOf::Session(_) => format!("paused {name} from its next turn"),
                    BreakerOf::Agent(_) => format!("paused {name}'s new turns"),
                };
                let mut event = Event::new(Kind::Paused, Party::breaker(), Some(party), text);
                event.reason = ask;
                self.note(event);
            }
            Step::Release => {
                if let BreakerOf::Session(id) = held {
                    if let Some(l) = self.0.state.lock().live.get_mut(id) {
                        l.breaker.hold_after = None;
                    }
                }
                let by = by.cloned().unwrap_or_else(By::you);
                self.note(by.event(Kind::Continued, Some(party), format!("let {name} go on")));
            }
            Step::Clear => {
                if let BreakerOf::Session(id) = held {
                    if let Some(l) = self.0.state.lock().live.get_mut(id) {
                        l.breaker.hold_after = None;
                    }
                }
            }
        }
    }

    /// Delivers `text` into `id`'s running turn, the way its engine takes
    /// one: Claude Code with its next tool call's result, Codex with
    /// `turn/steer`.
    fn steer(&self, id: Uuid, reason: Reason, text: String) {
        let mut st = self.0.state.lock();
        let engine = st.records.get(&id).map(|r| r.engine);
        let Some(l) = st.live.get_mut(&id) else {
            return;
        };
        match engine {
            Some(Engine::Claude) => l.breaker.steer = Some((reason, text)),
            Some(Engine::Codex) => {
                if let Some(link) = &l._codex {
                    let _ = link.steers.send(text);
                }
            }
            Some(
                Engine::Gemini
                | Engine::Copilot
                | Engine::OpenCode
                | Engine::Cursor
                | Engine::Devin
                | Engine::Custom,
            )
            | None => {}
        }
    }

    /// Says `text` into a Codex session's running turn.
    pub(super) fn steer_codex(&self, id: Uuid, text: String) {
        if let Some(link) = self
            .0
            .state
            .lock()
            .live
            .get(&id)
            .and_then(|l| l._codex.as_ref())
        {
            let _ = link.steers.send(text);
        }
    }

    // ── What a session's hooks tell it ──────────────────────────────────

    /// A tool call starting in `id`: why it is refused, while its breaker
    /// holds it from this turn; else it is watched for a loop.
    pub(super) fn breaker_tool(&self, id: Uuid, label: &str) -> Option<String> {
        let subject = session_subject(id);
        if let Some(state) = self.held(&subject) {
            // Held before this run of it began (a restart took it back
            // mid-turn): the turn it is on finishes, and the next is held.
            let this_turn = {
                let mut st = self.0.state.lock();
                match st.live.get_mut(&id) {
                    Some(l) => match l.breaker.hold_after {
                        Some(h) => l.breaker.turns > h,
                        None => {
                            l.breaker.hold_after = Some(l.breaker.turns);
                            false
                        }
                    },
                    None => true,
                }
            };
            if this_turn {
                let ask = self
                    .breaker_view(&subject, &state)
                    .and_then(|v| v.ask)
                    .unwrap_or_default();
                return Some(format!(
                    "Desk's breaker has paused this session: {ask} Don't try other tools. End \
                     your turn and tell the person they can say Continue on Desk's Floor."
                ));
            }
        }
        if self.rules().loops {
            self.breaker_signal(
                &subject,
                Signal::Tool {
                    key: key_of(label),
                    label: label.to_string(),
                },
                None,
            );
        }
        None
    }

    /// A tool call in `id` failed.
    pub(super) fn breaker_failed(&self, id: Uuid, label: &str) {
        if self.rules().loops {
            self.breaker_signal(
                &session_subject(id),
                Signal::Failed {
                    key: key_of(label),
                    label: label.to_string(),
                },
                None,
            );
        }
    }

    /// A steer waiting to be delivered with `id`'s tool call result.
    pub(super) fn take_steer(l: &mut Live) -> Option<String> {
        l.breaker.steer.take().map(|(_, text)| text)
    }

    /// `id`'s turn ended: what it was told it has heard. A budget's steer
    /// not yet delivered waits for its next turn; a loop's is over with it.
    pub(super) fn breaker_settled(&self, id: Uuid) {
        if let Some(l) = self.0.state.lock().live.get_mut(&id) {
            if matches!(l.breaker.steer, Some((Reason::Loop | Reason::Errors, _))) {
                l.breaker.steer = None;
            }
        }
        if self
            .0
            .breakers
            .lock()
            .states
            .contains_key(&session_subject(id))
        {
            self.breaker_signal(&session_subject(id), Signal::Settled, None);
        }
    }

    /// A turn began in `id`: by a prompt (`person`: the person's words,
    /// which let a held session go on) or its own waking.
    pub(super) fn breaker_turn(&self, id: Uuid, person: bool) {
        if let Some(l) = self.0.state.lock().live.get_mut(&id) {
            l.breaker.turns += 1;
        }
        if person && self.held(&session_subject(id)).is_some() {
            self.breaker_signal(&session_subject(id), Signal::Continue, Some(&By::you()));
        }
    }

    /// Desk typed `text` into `id`, sent by `by`: what its prompt hook
    /// then hears is that sender's, not the person's at the terminal.
    pub(super) fn breaker_typed(&self, id: Uuid, text: &str, by: Option<&By>) {
        let person = by.is_some_and(|b| b.party.kind == PartyKind::You);
        if let Some(l) = self.0.state.lock().live.get_mut(&id) {
            l.breaker.typed.push_back((text.trim().to_string(), person));
            while l.breaker.typed.len() > TYPED_KEPT {
                l.breaker.typed.pop_front();
            }
        }
    }

    /// Whether `prompt`, just submitted in `id`, is the person's own words:
    /// not one Desk typed for someone else, not an engine's own (its
    /// notices are `<…>`-tagged), and not the last one again (a loop or a
    /// schedule submitting it once more).
    pub(super) fn person_prompted(&self, id: Uuid, prompt: &str) -> bool {
        let said = prompt.trim();
        if said.is_empty() || said.starts_with('<') {
            return false;
        }
        let mut st = self.0.state.lock();
        let again = st
            .records
            .get(&id)
            .and_then(|r| r.last_prompt.as_deref())
            .is_some_and(|last| last.trim() == said);
        let Some(l) = st.live.get_mut(&id) else {
            return false;
        };
        if let Some(at) = l.breaker.typed.iter().position(|(t, _)| t == said) {
            return l.breaker.typed.remove(at).is_some_and(|(_, person)| person);
        }
        !again
    }

    /// `id` was just started again (resumed, restarted): while its breaker
    /// holds it, every turn of this run is held, the first too.
    pub(super) fn breaker_launched(&self, id: Uuid) {
        if self.held(&session_subject(id)).is_some() {
            if let Some(l) = self.0.state.lock().live.get_mut(&id) {
                l.breaker.hold_after = Some(0);
            }
        }
    }

    // ── Spend ───────────────────────────────────────────────────────────

    /// Folds what `sessions` spent (every session, for `None`) into their
    /// breakers and the pools'. Nothing to do with no budgets, unless a
    /// breaker still holds something.
    pub(super) async fn breaker_spend(&self, sessions: Option<Vec<Uuid>>) {
        let (budgets, any_held) = {
            let run = self.0.breakers.lock();
            (
                run.budgets.to_vec(),
                run.states.values().any(pure::State::held),
            )
        };
        if budgets.is_empty() && !any_held {
            return;
        }
        let now = Utc::now();
        // Never on a guessed timezone: a day budget's day is the person's.
        let Some(tz) = self.budget_tz() else {
            return;
        };
        let used = match self.used_by_period(&budgets, now, tz).await {
            Some(u) => u,
            None => return,
        };
        let agents = self.agents_by_period(&budgets, now, tz).await;
        let groups: HashMap<Uuid, Option<String>> = {
            let st = self.0.state.lock();
            st.records
                .keys()
                .map(|id| {
                    (
                        *id,
                        st.layout.group_of(&session_item(*id)).map(|g| g.id.clone()),
                    )
                })
                .collect()
        };
        let window = |p: Period| {
            (
                budget::window_start(p, now, tz),
                budget::window_end(p, now, tz),
            )
        };
        let standing = |b: &Budget, u: f64| {
            let (from, to) = window(b.period);
            budget::standing_of(b, u, from, to)
        };
        let held_sessions: HashSet<Uuid> = {
            let run = self.0.breakers.lock();
            run.states
                .iter()
                .filter(|(_, s)| s.held())
                .filter_map(|(k, _)| match BreakerOf::parse(k) {
                    Some(BreakerOf::Session(id)) => Some(id),
                    _ => None,
                })
                .collect()
        };
        let ids: Vec<Uuid> = match sessions {
            Some(ids) => ids
                .into_iter()
                .chain(held_sessions)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect(),
            None => groups.keys().copied().collect(),
        };
        for id in ids {
            let group = groups.get(&id).cloned().flatten();
            let subject = Subject::Session {
                id,
                group: group.as_deref(),
            };
            let standings: Vec<Standing> = budget::applying(&budgets, &subject)
                .into_iter()
                .map(|b| {
                    let spent = used
                        .get(&b.period)
                        .and_then(|m| m.get(&id))
                        .copied()
                        .unwrap_or_default();
                    standing(b, spent.of(b.unit))
                })
                .collect();
            let known = self
                .0
                .breakers
                .lock()
                .states
                .contains_key(&session_subject(id));
            if !standings.is_empty() || known {
                self.breaker_signal(&session_subject(id), Signal::Spend(standings), None);
            }
        }
        // Every pool with a budget, and every pool still holding.
        let mut pools: Vec<Pool> = budgets
            .iter()
            .filter(|b| b.scope == BudgetScope::Desk)
            .map(|b| b.group.clone().map_or(Pool::Desk, Pool::Group))
            .collect();
        pools.extend(self.0.breakers.lock().states.keys().filter_map(|k| {
            match BreakerOf::parse(k) {
                Some(BreakerOf::Pool(p)) => Some(p),
                _ => None,
            }
        }));
        pools.sort();
        pools.dedup();
        for pool in pools {
            let standings: Vec<Standing> = budget::pools(&budgets, &pool)
                .into_iter()
                .map(|b| {
                    let mut total = sessions_used(&pool, used.get(&b.period), &groups);
                    total.add(self.agents_used(&pool, b.period, &agents));
                    standing(b, total.of(b.unit))
                })
                .collect();
            self.breaker_signal(&pool_subject(&pool), Signal::Spend(standings), None);
        }
    }

    /// What the local agents spent toward `pool` in `period`.
    fn agents_used(
        &self,
        pool: &Pool,
        period: Period,
        agents: &HashMap<Period, HashMap<String, Used>>,
    ) -> Used {
        let mut total = Used::default();
        let Some(spent) = agents.get(&period) else {
            return total;
        };
        for (agent, u) in spent {
            let counts = match pool {
                Pool::Desk => true,
                Pool::Group(g) => self.group_of_local_agent(agent).as_deref() == Some(g),
            };
            if counts {
                total.add(*u);
            }
        }
        total
    }

    /// Each local agent's spend from the start of every period `budgets`
    /// use, every billing counted (a budget the person sets is about use):
    /// none until the hub says how to read it.
    async fn agents_by_period(
        &self,
        budgets: &[Budget],
        now: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> HashMap<Period, HashMap<String, Used>> {
        let Some(reader) = self.0.agent_spend.lock().clone() else {
            return HashMap::new();
        };
        let mut periods: Vec<Period> = budgets.iter().map(|b| b.period).collect();
        periods.sort();
        periods.dedup();
        let mut out = HashMap::new();
        for p in periods {
            let reader = std::sync::Arc::clone(&reader);
            let from = budget::window_start(p, now, tz);
            match tokio::task::spawn_blocking(move || reader.spent(from, now)).await {
                Ok(Ok(spent)) => {
                    out.insert(p, spent.into_iter().collect());
                }
                Ok(Err(e)) => warn_agents_spend(&format!("{e:#}")),
                Err(e) => warn_agents_spend(&e.to_string()),
            }
        }
        out
    }

    /// Reads local agents' spend with `reader` from now on: the hub's
    /// telemetry.
    pub fn spend_agents_with(&self, reader: std::sync::Arc<dyn AgentSpend>) {
        *self.0.agent_spend.lock() = Some(reader);
    }

    /// The budgets that apply to the agent `id`: its own, else its group's
    /// default, else the ungrouped one.
    pub fn agent_budgets(&self, id: &str) -> Vec<Budget> {
        let group = self.group_of_local_agent(id);
        let run = self.0.breakers.lock();
        budget::applying(
            &run.budgets,
            &Subject::Agent {
                id,
                group: group.as_deref(),
            },
        )
        .into_iter()
        .cloned()
        .collect()
    }

    /// Folds where the agent `id` (called `name`) stands against its
    /// budgets into its breaker: whether its next turn may run, and what to
    /// tell the person if it is near or over.
    ///
    /// A `preflight` (a scheduled run asking first) holds or lets go as the
    /// turn itself would, but says no warning: that is the turn's to say.
    pub fn agent_stand(
        &self,
        id: &str,
        name: &str,
        standings: Vec<Standing>,
        preflight: bool,
    ) -> AgentVerdict {
        self.0
            .breakers
            .lock()
            .names
            .insert(id.to_string(), name.to_string());
        let subject = agent_subject(id);
        if preflight {
            let state = self
                .0
                .breakers
                .lock()
                .states
                .get(&subject)
                .cloned()
                .unwrap_or_default();
            let (_, steps) = pure::step(state, Signal::Spend(standings.clone()), Utc::now());
            if steps
                .iter()
                .any(|s| matches!(s, Step::Hold { .. } | Step::Clear))
            {
                self.breaker_signal(&subject, Signal::Spend(standings), None);
            }
            return match self.agent_held(id) {
                Some(why) => AgentVerdict::Held(why),
                None => AgentVerdict::Proceed,
            };
        }
        let steps = self.breaker_signal(&subject, Signal::Spend(standings.clone()), None);
        if let Some(why) = self.agent_held(id) {
            return AgentVerdict::Held(why);
        }
        let steered = steps.iter().any(|s| matches!(s, Step::Steer { .. }));
        match budget::tightest(standings).filter(|_| steered) {
            Some(st) => AgentVerdict::Warn(match st.status {
                budget::Status::Over => format!(
                    "{name} is over its {} budget ({}). The budget only warns.",
                    st.period.words(),
                    st.amounts()
                ),
                budget::Status::Warn | budget::Status::Under => format!(
                    "{name} has used {} of its {} budget ({}).",
                    st.percent(),
                    st.period.words(),
                    st.amounts()
                ),
            }),
            None => AgentVerdict::Proceed,
        }
    }

    /// Each session's spend from the start of every period `budgets` use.
    async fn used_by_period(
        &self,
        budgets: &[Budget],
        now: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Option<HashMap<Period, HashMap<Uuid, Used>>> {
        let mut periods: Vec<Period> = budgets.iter().map(|b| b.period).collect();
        periods.sort();
        periods.dedup();
        let mut used = HashMap::new();
        for p in periods {
            match self
                .0
                .store
                .ledger_used(budget::window_start(p, now, tz))
                .await
            {
                Ok(u) => {
                    used.insert(p, u);
                }
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "couldn't sum the ledger for budgets"
                    );
                    return None;
                }
            }
        }
        Some(used)
    }

    // ── Starts and prompts by others ────────────────────────────────────

    /// What Desk's breaker asks while it holds new starts in `group`'s
    /// pools (Desk's and the group's), when it does.
    pub fn starts_held(&self, group: Option<&str>) -> Option<String> {
        budget::pools_of(group).into_iter().find_map(|pool| {
            let subject = pool_subject(&pool);
            let state = self.held(&subject)?;
            Some(
                self.breaker_view(&subject, &state)
                    .and_then(|v| v.ask)
                    .unwrap_or_default(),
            )
        })
    }

    /// Why a start by `by` is refused while a pool it counts toward holds
    /// new starts. The person's own starts are never refused.
    pub(super) fn breaker_refuses_start(&self, by: Option<&By>) -> Result<(), DeskError> {
        let Some(by) = by.filter(|b| b.party.kind != PartyKind::You) else {
            return Ok(());
        };
        let group = match by.party.kind {
            PartyKind::Agent | PartyKind::Manager => self.group_of_local_agent(&by.party.id),
            PartyKind::Session | PartyKind::You | PartyKind::Desk => None,
        };
        for pool in budget::pools_of(group.as_deref()) {
            let subject = pool_subject(&pool);
            if let Some(state) = self.held(&subject) {
                let ask = self
                    .breaker_view(&subject, &state)
                    .and_then(|v| v.ask)
                    .unwrap_or_default();
                return Err(DeskError::Conflict(format!(
                    "Desk's breaker holds new sessions: {ask} The person can say Continue on \
                     Desk's Floor."
                )));
            }
        }
        Ok(())
    }

    /// Why a prompt by `by` to `id` is refused while its breaker holds it.
    /// The person's own prompts let it go on instead.
    pub(super) fn breaker_refuses_prompt(
        &self,
        id: Uuid,
        by: Option<&By>,
    ) -> Result<(), DeskError> {
        // Only the person goes past a hold: no one named is not the person.
        if by.is_some_and(|b| b.party.kind == PartyKind::You) {
            return Ok(());
        }
        let subject = session_subject(id);
        match self.held(&subject) {
            Some(state) => {
                let ask = self
                    .breaker_view(&subject, &state)
                    .and_then(|v| v.ask)
                    .unwrap_or_default();
                Err(DeskError::Conflict(format!(
                    "Desk's breaker has paused this session: {ask} The person can say Continue \
                     on Desk's Floor."
                )))
            }
            None => Ok(()),
        }
    }

    // ── The person's answers ────────────────────────────────────────────

    /// The person lets `subject` go on (Continue).
    pub fn breaker_continue(&self, subject: &str, by: By) -> Result<BreakerView, DeskError> {
        BreakerOf::parse(subject).ok_or(DeskError::NotFound)?;
        if self.held(subject).is_none() {
            return Err(DeskError::Conflict("it isn't paused".into()));
        }
        self.breaker_signal(subject, Signal::Continue, Some(&by));
        self.breaker(subject).ok_or(DeskError::NotFound)
    }

    /// The person leaves `subject` paused (Keep paused): its need goes,
    /// the hold stays.
    pub fn breaker_keep(&self, subject: &str) -> Result<BreakerView, DeskError> {
        BreakerOf::parse(subject).ok_or(DeskError::NotFound)?;
        if self.held(subject).is_none() {
            return Err(DeskError::Conflict("it isn't paused".into()));
        }
        self.breaker_signal(subject, Signal::Keep, None);
        if let Some(view) = self.breaker(subject) {
            self.announce(DeskEvent::Breaker {
                breaker: Box::new(view.clone()),
            });
            return Ok(view);
        }
        Err(DeskError::NotFound)
    }

    pub async fn set_breaker_rules(&self, rules: Rules) -> Result<Rules, DeskError> {
        self.0
            .store
            .put_state(
                RULES_KEY,
                &serde_json::to_value(rules).map_err(|e| DeskError::Internal(e.into()))?,
            )
            .await?;
        self.0.breakers.lock().rules = rules;
        if !rules.loops {
            // What it had seen of loops is forgotten; holds stay until the
            // person answers them.
            let subjects: Vec<String> = self.0.breakers.lock().states.keys().cloned().collect();
            for s in subjects {
                self.breaker_signal(&s, Signal::Settled, None);
            }
        }
        Ok(rules)
    }

    // ── Budgets ─────────────────────────────────────────────────────────

    pub fn budget_list(&self) -> Vec<Budget> {
        self.0.breakers.lock().budgets.clone()
    }

    /// Every budget with where it stands, the breakers, and the rules.
    pub async fn budgets_view(&self) -> Result<BudgetsView, DeskError> {
        let budgets = self.budget_list();
        let now = Utc::now();
        let tz = self
            .budget_tz()
            .ok_or_else(|| DeskError::Conflict("Desk doesn't know your timezone yet".into()))?;
        let used = self
            .used_by_period(&budgets, now, tz)
            .await
            .ok_or_else(|| DeskError::Internal(eyre::eyre!("couldn't sum the ledger")))?;
        let agents = self.agents_by_period(&budgets, now, tz).await;
        let groups: HashMap<Uuid, Option<String>> = {
            let st = self.0.state.lock();
            st.records
                .keys()
                .map(|id| {
                    (
                        *id,
                        st.layout.group_of(&session_item(*id)).map(|g| g.id.clone()),
                    )
                })
                .collect()
        };
        let views = budgets
            .iter()
            .map(|b| {
                let (from, to) = (
                    budget::window_start(b.period, now, tz),
                    budget::window_end(b.period, now, tz),
                );
                let of_period = used.get(&b.period);
                let spent = |id: &Uuid| {
                    of_period
                        .and_then(|m| m.get(id))
                        .copied()
                        .unwrap_or_default()
                };
                let agent_spent = |id: &str| {
                    agents
                        .get(&b.period)
                        .and_then(|m| m.get(id))
                        .copied()
                        .unwrap_or_default()
                };
                let standing = match &b.scope {
                    BudgetScope::Session(id) => {
                        Some(budget::standing_of(b, spent(id).of(b.unit), from, to))
                    }
                    BudgetScope::Desk => {
                        let pool = b.group.clone().map_or(Pool::Desk, Pool::Group);
                        let mut total = sessions_used(&pool, of_period, &groups);
                        total.add(self.agents_used(&pool, b.period, &agents));
                        Some(budget::standing_of(b, total.of(b.unit), from, to))
                    }
                    BudgetScope::EachSession => {
                        budget::tightest(groups.iter().filter_map(|(id, g)| {
                            let subject = Subject::Session {
                                id: *id,
                                group: g.as_deref(),
                            };
                            budget::applying(&budgets, &subject)
                                .iter()
                                .any(|a| a.id == b.id)
                                .then(|| budget::standing_of(b, spent(id).of(b.unit), from, to))
                        }))
                    }
                    BudgetScope::Agent(id) => {
                        let u = agent_spent(id);
                        Some(budget::standing_of(b, u.of(b.unit), from, to))
                    }
                    BudgetScope::EachAgent => budget::tightest(
                        agents
                            .get(&b.period)
                            .into_iter()
                            .flatten()
                            .filter(|(id, _)| self.agent_budgets(id).iter().any(|a| a.id == b.id))
                            .map(|(_, u)| budget::standing_of(b, u.of(b.unit), from, to)),
                    ),
                };
                BudgetView {
                    budget: b.clone(),
                    standing,
                }
            })
            .collect();
        Ok(BudgetsView {
            budgets: views,
            breakers: self.breakers(),
            rules: self.rules(),
        })
    }

    /// Each group's own budget, the furthest along, by group id, as the
    /// Floor heads a boss's section: none counted when no group has one,
    /// and none when they can't be counted within [`SPEND_WAIT`] (Desk
    /// doesn't know the timezone yet, the ledger can't be read, or reading
    /// it is slow), so the Floor is never held up by it. What was counted is
    /// kept for [`SPEND_KEPT`], or until the ledger or the budgets change.
    pub async fn group_standings(&self) -> HashMap<String, Standing> {
        if let Some((at, kept)) = self.0.group_spend.lock().as_ref() {
            if at.elapsed() < SPEND_KEPT {
                return kept.clone();
            }
        }
        let counted = tokio::time::timeout(SPEND_WAIT, self.count_group_standings())
            .await
            .unwrap_or_else(|_| {
                tracing::debug!("the groups' budgets took too long to count");
                HashMap::new()
            });
        *self.0.group_spend.lock() = Some((std::time::Instant::now(), counted.clone()));
        counted
    }

    /// What the Floor counts for the groups' bosses is to be counted again.
    pub(super) fn group_spend_changed(&self) {
        *self.0.group_spend.lock() = None;
    }

    async fn count_group_standings(&self) -> HashMap<String, Standing> {
        let budgets: Vec<Budget> = self
            .budget_list()
            .into_iter()
            .filter(|b| b.scope == BudgetScope::Desk && b.group.is_some())
            .collect();
        if budgets.is_empty() {
            return HashMap::new();
        }
        let Some(tz) = self.budget_tz() else {
            tracing::debug!("the groups' budgets can't be counted before Desk knows the timezone");
            return HashMap::new();
        };
        let now = Utc::now();
        // Only the periods the groups' own budgets use are read.
        let Some(used) = self.used_by_period(&budgets, now, tz).await else {
            return HashMap::new();
        };
        let agents = self.agents_by_period(&budgets, now, tz).await;
        let groups: HashMap<Uuid, Option<String>> = {
            let st = self.0.state.lock();
            st.records
                .keys()
                .map(|id| {
                    (
                        *id,
                        st.layout.group_of(&session_item(*id)).map(|g| g.id.clone()),
                    )
                })
                .collect()
        };
        let mut by: HashMap<String, Vec<Standing>> = HashMap::new();
        for b in &budgets {
            let Some(group) = b.group.clone() else {
                continue;
            };
            let pool = Pool::Group(group.clone());
            let mut total = sessions_used(&pool, used.get(&b.period), &groups);
            total.add(self.agents_used(&pool, b.period, &agents));
            let (from, to) = (
                budget::window_start(b.period, now, tz),
                budget::window_end(b.period, now, tz),
            );
            by.entry(group)
                .or_default()
                .push(budget::standing_of(b, total.of(b.unit), from, to));
        }
        by.into_iter()
            .filter_map(|(g, s)| Some((g, budget::tightest(s)?)))
            .collect()
    }

    fn check_new(&self, b: &Budget) -> Result<(), DeskError> {
        budget::check(b).map_err(DeskError::Invalid)?;
        if let Some(g) = &b.group {
            if !self.0.state.lock().layout.groups.iter().any(|x| x.id == *g) {
                return Err(DeskError::Invalid(format!("there is no group {g:?}")));
            }
        }
        if let BudgetScope::Session(id) = &b.scope {
            if !self.0.state.lock().records.contains_key(id) {
                return Err(DeskError::NotFound);
            }
        }
        Ok(())
    }

    async fn budgets_changed(&self, by: &By, text: String) {
        let budgets = match self.0.store.budgets().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "couldn't read budgets back");
                return;
            }
        };
        self.0.breakers.lock().budgets = budgets.clone();
        self.group_spend_changed();
        self.announce(DeskEvent::Budgets { budgets });
        self.note(by.event(Kind::Budgeted, None, text));
        self.breaker_spend(None).await;
    }

    /// Sets a budget, replacing the one for the same group, scope, period
    /// and unit.
    pub async fn put_budget(&self, new: NewBudget, by: By) -> Result<Budget, DeskError> {
        let b = Budget {
            id: Uuid::new_v4(),
            scope: new.scope,
            group: new.group.filter(|g| !g.trim().is_empty()),
            period: new.period,
            unit: new.unit,
            amount: new.amount,
            warn_at: new.warn_at.unwrap_or(WARN_AT),
            on_exceed: new.on_exceed.unwrap_or(OnExceed::Pause),
            updated_at: Utc::now(),
        };
        self.check_new(&b)?;
        let kept = self.0.store.put_budget(&b).await?;
        let text = format!(
            "set {} for {}",
            budget_words(&kept),
            self.scope_words(&kept)
        );
        self.budgets_changed(&by, text).await;
        Ok(kept)
    }

    /// Changes a budget's amounts.
    pub async fn patch_budget(
        &self,
        id: Uuid,
        patch: BudgetPatch,
        by: By,
    ) -> Result<Budget, DeskError> {
        let mut b = self
            .budget_list()
            .into_iter()
            .find(|b| b.id == id)
            .ok_or(DeskError::NotFound)?;
        b.amount = patch.amount.unwrap_or(b.amount);
        b.warn_at = patch.warn_at.unwrap_or(b.warn_at);
        b.on_exceed = patch.on_exceed.unwrap_or(b.on_exceed);
        b.updated_at = Utc::now();
        budget::check(&b).map_err(DeskError::Invalid)?;
        let kept = self
            .0
            .store
            .update_budget(&b)
            .await?
            .ok_or(DeskError::NotFound)?;
        let text = format!(
            "changed the budget for {} to {}",
            self.scope_words(&kept),
            budget_words(&kept)
        );
        self.budgets_changed(&by, text).await;
        Ok(kept)
    }

    /// Removes a budget. What it holds stays held until the person says
    /// Continue, or its period ends.
    pub async fn delete_budget(&self, id: Uuid, by: By) -> Result<(), DeskError> {
        let b = self
            .budget_list()
            .into_iter()
            .find(|b| b.id == id)
            .ok_or(DeskError::NotFound)?;
        if !self.0.store.delete_budget(id).await? {
            return Err(DeskError::NotFound);
        }
        let text = format!("removed {} for {}", budget_words(&b), self.scope_words(&b));
        self.budgets_changed(&by, text).await;
        Ok(())
    }

    /// What a budget is for, in words: "Desk", "each session in Acme",
    /// "auth-fix".
    fn scope_words(&self, b: &Budget) -> String {
        let group = b
            .group
            .as_ref()
            .map(|g| self.subject_name(&BreakerOf::Pool(Pool::Group(g.clone()))));
        match (&b.scope, group) {
            (BudgetScope::Desk, None) => "everything on Desk".into(),
            (BudgetScope::Desk, Some(g)) => format!("everything in {g}"),
            (BudgetScope::EachSession, None) => "each session".into(),
            (BudgetScope::EachSession, Some(g)) => format!("each session in {g}"),
            (BudgetScope::EachAgent, None) => "each agent".into(),
            (BudgetScope::EachAgent, Some(g)) => format!("each agent in {g}"),
            (BudgetScope::Session(id), _) => self.subject_name(&BreakerOf::Session(*id)),
            (BudgetScope::Agent(id), _) => id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_read_back_as_what_they_name() {
        let id = Uuid::new_v4();
        assert_eq!(
            BreakerOf::parse(&session_subject(id)),
            Some(BreakerOf::Session(id))
        );
        assert_eq!(
            BreakerOf::parse(&agent_subject("atlas")),
            Some(BreakerOf::Agent("atlas".into()))
        );
        assert_eq!(
            BreakerOf::parse(&pool_subject(&Pool::Desk)),
            Some(BreakerOf::Pool(Pool::Desk))
        );
        let acme = Pool::Group("g-acme".into());
        assert_eq!(
            BreakerOf::parse(&pool_subject(&acme)),
            Some(BreakerOf::Pool(acme))
        );
        assert_eq!(BreakerOf::parse("pool:group:"), None);
        assert_eq!(BreakerOf::parse("nothing"), None);
    }
}
