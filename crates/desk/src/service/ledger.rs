//! The ledger's I/O shell: which records a session has, reading each from
//! its cursor in bounded steps, pricing and stamping what the readers in
//! [`crate::ledger`] find, and keeping it. One worker reads one record at a
//! time, so cursors never race, and a backfill of every known session's
//! history at start shares the worker with live reads.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::memory::Harvest;
use super::{Desk, DeskError};
use crate::engine::{Engine, SignIn, Via};
use crate::ledger::{
    self, Billing, CodexState, Label, Period, Priced, Pricer, Spend, SpendBy, SpendRow, SpendView,
};
use crate::ledger_store::{After, Cursor, Filter, Kept, LedgerRow, PAGE};
use crate::memory::record::{Harvested, Part, Read};
use crate::memory::Redactor;
use crate::model::{DeskEvent, SessionRecord};

/// How much of a record one step reads before keeping what it found.
const STEP_BYTES: u64 = 4 * 1024 * 1024;
/// The longest line read whole; a longer one (a huge tool result) is
/// stepped over, as no call is recorded on one.
const MAX_LINE: usize = 32 * 1024 * 1024;
/// How often a working session's record is read again on its tool calls.
const WHILE_WORKING: Duration = Duration::from_secs(60);
/// How long new spend waits to be announced, so a burst is one event.
const TELL_EVERY: Duration = Duration::from_secs(2);

/// What the worker does next.
pub(super) enum Job {
    /// Read a session's records from their cursors.
    Read(Uuid),
    /// Keep a call an engine reported live.
    Live { session: Uuid, spend: Spend },
}

/// What the worker is given: jobs in order, and reads to make before any
/// of them (a session about to compact, whose record must be kept first).
pub(super) struct Jobs {
    jobs: mpsc::UnboundedReceiver<Job>,
    soon: mpsc::UnboundedReceiver<Uuid>,
}

/// The ledger's share of the desk.
pub(super) struct LedgerRun {
    jobs: mpsc::UnboundedSender<Job>,
    soon: mpsc::UnboundedSender<Uuid>,
    /// Sessions waiting to be read: a session asked for twice is read once.
    waiting: Mutex<HashSet<Uuid>>,
    /// When each working session was last read on a tool call.
    read_at: Mutex<HashMap<Uuid, Instant>>,
    /// Sessions with new spend not yet announced.
    untold: Mutex<HashSet<Uuid>>,
    /// Each engine's sign-in and since when Desk has seen it so: a call
    /// before then is of an account Desk can't name.
    signed: Mutex<BTreeMap<Engine, (SignIn, DateTime<Utc>)>>,
    pricer: Option<Arc<dyn Pricer>>,
    /// Hides secrets in what the harvest keeps; none, nothing is harvested.
    pub(super) redactor: Option<Arc<dyn Redactor>>,
    /// Sessions whose turn or run ended: their next read closes the
    /// exchange they leave open.
    ending: Mutex<HashSet<Uuid>>,
    /// Each session's project, found once.
    projects: Mutex<HashMap<Uuid, String>>,
    /// Held by every read of a session's records, and by a change to
    /// memory's switches across the read it makes first and the switch
    /// itself: no read runs between them with the switch as it was.
    pub(super) reading: tokio::sync::Mutex<()>,
}

impl LedgerRun {
    pub(super) fn new(
        pricer: Option<Arc<dyn Pricer>>,
        redactor: Option<Arc<dyn Redactor>>,
    ) -> (Self, Jobs) {
        let (jobs, rx) = mpsc::unbounded_channel();
        let (soon, soon_rx) = mpsc::unbounded_channel();
        (
            Self {
                jobs,
                soon,
                waiting: Mutex::default(),
                read_at: Mutex::default(),
                untold: Mutex::default(),
                signed: Mutex::default(),
                pricer,
                redactor,
                ending: Mutex::default(),
                projects: Mutex::default(),
                reading: tokio::sync::Mutex::new(()),
            },
            Jobs {
                jobs: rx,
                soon: soon_rx,
            },
        )
    }

    /// Notes what each engine is signed in as now, keeping when it was
    /// first seen so.
    pub(super) fn signed_in(
        &self,
        now: DateTime<Utc>,
        sign_ins: impl Iterator<Item = (Engine, SignIn)>,
    ) {
        let mut signed = self.signed.lock();
        for (engine, sign_in) in sign_ins {
            match signed.get(&engine) {
                Some((known, _)) if *known == sign_in => {}
                _ => {
                    signed.insert(engine, (sign_in, now));
                }
            }
        }
    }

    /// How `spend` of `engine` was paid and by which account: what its
    /// record says; a session on one of the person's accounts, that
    /// account's plan; else the sign-in Desk saw when it was made.
    fn stamp(&self, engine: Engine, spend: Spend, on: Option<String>) -> Kept {
        let signed = self.signed.lock().get(&engine).cloned();
        let (billing, account) = match (on, signed) {
            (Some(name), _) => (Billing::Plan, Some(name)),
            (None, signed) => match signed {
                Some((SignIn::In { via, who, ids, .. }, since)) if spend.at >= since => (
                    match via {
                        Via::Plan => Billing::Plan,
                        Via::Key => Billing::Key,
                        Via::Vendor => Billing::Vendor,
                    },
                    who.or_else(|| ids.into_iter().next()),
                ),
                _ => (Billing::Unknown, None),
            },
        };
        let billing = spend.billing.unwrap_or(billing);
        let mut spend = spend;
        if spend.usd.is_none() {
            spend.usd = spend.model.as_deref().and_then(|model| {
                self.pricer
                    .as_ref()?
                    .usd(engine, billing, model, &spend.tokens)
            });
            spend.priced = match spend.usd {
                Some(_) => Priced::Priced,
                None => Priced::None,
            };
        }
        Kept {
            spend,
            billing,
            account,
        }
    }
}

/// One line of a record, as [`next_line`] reads it.
enum Line {
    /// A whole line, its bytes in the buffer, and its length with its end.
    Whole(u64),
    /// A line too long to keep, stepped over.
    Skipped(u64),
    /// What's left has no end yet: a line still being written.
    Partial,
    End,
}

/// The next line of `r` into `buf`, keeping at most [`MAX_LINE`] bytes.
fn next_line(r: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Line> {
    buf.clear();
    let mut len: u64 = 0;
    let mut over = false;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok(if len == 0 { Line::End } else { Line::Partial });
        }
        let (take, ended) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (avail.len(), false),
        };
        if !over {
            if buf.len() + take > MAX_LINE {
                over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&avail[..take]);
            }
        }
        len += take as u64;
        r.consume(take);
        if ended {
            return Ok(if over {
                Line::Skipped(len)
            } else {
                Line::Whole(len)
            });
        }
    }
}

/// What one step of reading a record found.
struct Step {
    spends: Vec<Spend>,
    /// What of the conversation its lines hold, past the harvest's offset.
    parts: Vec<Part>,
    cursor: Cursor,
    /// The record has nothing more to read now.
    done: bool,
}

/// Reads one step of `engine`'s record at `path` from `cursor`: whole lines
/// only, so a line still being written is read next time. Each line is
/// read once for every consumer whose offset it is past: the ledger's
/// (`read_to`) for its call, and, when `harvest`, the harvest's
/// (`harvested_to`) for its conversation. Each offset only moves on, so a
/// harvest catching up never counts a call twice. Without `harvest` the
/// harvest's offset stays where it is. A record shorter than its cursor
/// was replaced, and both are read again from its start.
fn read_step(engine: Engine, path: &Path, cursor: &Cursor, harvest: bool) -> std::io::Result<Step> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let replaced = len < cursor.read_to || (harvest && len < cursor.harvested_to);
    let (read_to, harvested_to, mut codex, mut harvested) = match replaced {
        true => (
            0,
            0,
            CodexState::default(),
            Harvested {
                decided: Harvested::of(&cursor.harvest).decided,
                ..Harvested::default()
            },
        ),
        false => (
            cursor.read_to,
            cursor.harvested_to,
            serde_json::from_value(cursor.seen.clone()).unwrap_or_default(),
            Harvested::of(&cursor.harvest),
        ),
    };
    let start = match harvest {
        true => read_to.min(harvested_to),
        false => read_to,
    };
    file.seek(SeekFrom::Start(start))?;
    let mut r = BufReader::with_capacity(256 * 1024, file);
    let mut buf = Vec::new();
    let mut at = start;
    let mut reads: Vec<Read> = Vec::new();
    let done = loop {
        if at - start >= STEP_BYTES {
            break at >= len;
        }
        match next_line(&mut r, &mut buf)? {
            Line::Whole(n) => {
                let (spend, parts) = (at >= read_to, harvest && at >= harvested_to);
                at += n;
                let read = match engine {
                    Engine::Claude => ledger::claude_read(&buf, spend, parts),
                    Engine::Codex => ledger::codex_read(
                        &buf,
                        spend.then_some(&mut codex),
                        parts.then_some(&mut harvested.codex),
                    ),
                    Engine::Gemini
                    | Engine::Copilot
                    | Engine::OpenCode
                    | Engine::Cursor
                    | Engine::Devin
                    | Engine::Custom => Read::default(),
                };
                reads.push(read);
            }
            Line::Skipped(n) => at += n,
            Line::Partial | Line::End => break true,
        }
    };
    let seen = match engine {
        Engine::Codex => serde_json::to_value(&codex).unwrap_or_default(),
        _ => serde_json::json!({}),
    };
    let (mut spends, mut parts) = (Vec::new(), Vec::new());
    for read in reads {
        spends.extend(read.spend);
        parts.extend(read.parts);
    }
    Ok(Step {
        spends: ledger::once_each(spends),
        parts,
        cursor: Cursor {
            read_to: read_to.max(at),
            seen,
            harvested_to: match harvest {
                true => harvested_to.max(at),
                false => harvested_to,
            },
            harvest: match harvest || replaced {
                true => harvested.value(),
                false => cursor.harvest.clone(),
            },
        },
        done,
    })
}

/// A Claude Code session's records: its transcript, and each of its
/// sub-agents' beside it (`<transcript>/subagents/agent-*.jsonl`), whose
/// calls are the session's work but are in no other record.
fn claude_records(transcript: &Path) -> Vec<PathBuf> {
    let mut records = vec![transcript.to_path_buf()];
    let subagents = transcript.with_extension("").join("subagents");
    if let Ok(entries) = std::fs::read_dir(&subagents) {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        found.sort();
        records.extend(found);
    }
    records
}

/// A query of the ledger, as the Floor, the Ledger view and the overseer
/// ask it.
#[derive(Debug, Clone)]
pub struct SpendQuery {
    pub by: SpendBy,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub limit: usize,
    pub filter: Filter,
}

/// A page of the ledger's rows and where the next one starts.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerPage {
    pub rows: Vec<LedgerRow>,
    pub next: Option<String>,
}

impl Desk {
    /// Reads `id`'s records when the worker gets to it.
    pub(super) fn spend_read(&self, id: Uuid) {
        if self.0.ledger.waiting.lock().insert(id) {
            let _ = self.0.ledger.jobs.send(Job::Read(id));
        }
    }

    /// Reads `id`'s records once its turn or its run has ended: the
    /// exchange it leaves open is closed.
    pub(super) fn spend_read_ending(&self, id: Uuid) {
        self.0.ledger.ending.lock().insert(id);
        self.spend_read(id);
    }

    /// Reads `id`'s records before anything else the worker has waiting:
    /// it is about to compact, and what it said is kept first. Never waits.
    pub(super) fn spend_read_soon(&self, id: Uuid) {
        let _ = self.0.ledger.soon.send(id);
    }

    /// Reads a working session's records on its tool calls, at most every
    /// [`WHILE_WORKING`].
    pub(super) fn spend_read_while_working(&self, id: Uuid) {
        let due = {
            let mut read_at = self.0.ledger.read_at.lock();
            let due = read_at
                .get(&id)
                .is_none_or(|t| t.elapsed() >= WHILE_WORKING);
            if due {
                read_at.insert(id, Instant::now());
            }
            due
        };
        if due {
            self.spend_read(id);
        }
    }

    /// Keeps a call `id`'s engine reported as it happened.
    pub(super) fn spend_live(&self, id: Uuid, spend: Spend) {
        let _ = self.0.ledger.jobs.send(Job::Live { session: id, spend });
    }

    /// Notes what the engines are signed in as, for the calls read from now.
    pub(super) fn spend_signed_in(&self) {
        let now = Utc::now();
        self.0
            .ledger
            .signed_in(now, self.sign_ins().by_engine.into_iter());
    }

    /// The ledger's worker: every known session's history first (closed
    /// ones too, oldest first, so a forked conversation's copied calls stay
    /// with the session that made them), then what comes. Until the desk
    /// stops.
    pub(super) async fn ledger_loop(self, jobs: Jobs) {
        let Jobs { mut jobs, mut soon } = jobs;
        match self.0.store.every_session().await {
            Ok(all) => all.iter().for_each(|r| self.spend_read(r.id)),
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "couldn't list the sessions whose spend to read"
            ),
        }
        let mut tell = tokio::time::interval(TELL_EVERY);
        let mut sweep = tokio::time::interval(super::memory::SWEEP_EVERY);
        let mut measure = tokio::time::interval(super::memory_index::MEASURE_EVERY);
        loop {
            tokio::select! {
                biased;
                _ = self.0.cancel.cancelled() => return,
                Some(id) = soon.recv() => self.read_spend(id).await,
                _ = tell.tick() => self.tell_spend(),
                _ = sweep.tick() => self.sweep_memory().await,
                _ = measure.tick() => self.measure_memory().await,
                job = jobs.recv() => match job {
                    None => return,
                    Some(Job::Read(id)) => {
                        self.0.ledger.waiting.lock().remove(&id);
                        self.read_spend(id).await;
                    }
                    Some(Job::Live { session, spend }) => self.keep_live(session, spend).await,
                },
            }
        }
    }

    fn tell_spend(&self) {
        let told: Vec<Uuid> = self.0.ledger.untold.lock().drain().collect();
        for session in &told {
            let _ = self.0.events.send(DeskEvent::Spend { session: *session });
        }
        if !told.is_empty() {
            let desk = self.clone();
            tokio::spawn(async move { desk.breaker_spend(Some(told)).await });
        }
    }

    async fn keep_live(&self, session: Uuid, spend: Spend) {
        let Some(r) = self.session_record(session).await else {
            return;
        };
        let engine = r.engine;
        let kept = self
            .0
            .ledger
            .stamp(engine, spend, self.account_name(r.account));
        match self
            .0
            .store
            .ledger_write(session, engine, None, &[kept])
            .await
        {
            Ok(0) => {}
            Ok(_) => {
                self.0.ledger.untold.lock().insert(session);
            }
            Err(e) => tracing::warn!(%session, error = %e, "couldn't keep a call's spend"),
        }
    }

    /// The registry's record of `id`, closed sessions too.
    async fn session_record(&self, id: Uuid) -> Option<SessionRecord> {
        if let Ok(r) = self.record(id) {
            return Some(r);
        }
        self.0.store.get(id).await.ok().flatten()
    }

    /// Where `r`'s engine keeps what it did: found from its id when the
    /// registry doesn't say.
    pub(super) fn records_of(&self, r: &SessionRecord) -> Vec<PathBuf> {
        let home = self.0.env.get("HOME").map(PathBuf::from);
        let known = r
            .transcript
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_file());
        match r.engine {
            Engine::Claude => known
                .or_else(|| {
                    let id = r.cli_session_id.as_deref()?;
                    super::find_transcript(home.as_deref()?, id).map(PathBuf::from)
                })
                .map(|t| claude_records(&t))
                .unwrap_or_default(),
            Engine::Codex => known
                .or_else(|| {
                    let thread = r.cli_session_id.as_deref()?;
                    crate::codex::find_rollout(home.as_deref()?, thread)
                })
                .into_iter()
                .collect(),
            Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => Vec::new(),
        }
    }

    /// Reads each of `id`'s records from its cursor to its end, for the
    /// ledger and, where it may be, for session memory.
    async fn read_spend(&self, id: Uuid) {
        let _reading = self.0.ledger.reading.lock().await;
        self.read_spend_held(id, None).await;
    }

    /// [`Self::read_spend`] with [`LedgerRun::reading`] already held; with
    /// `forced`, every record is read so whatever the switches say now.
    pub(super) async fn read_spend_held(&self, id: Uuid, forced: Option<Harvest>) {
        let Some(r) = self.session_record(id).await else {
            return;
        };
        // An agent's turn is its agent's spend, in its hub's own records.
        if self.0.opts.hub_internal.holds(std::path::Path::new(&r.cwd))
            || ledger::agent_turn(&r.cwd, &r.name)
        {
            return;
        }
        let known = self.0.ledger.projects.lock().get(&id).cloned();
        let (records, project) = {
            let desk = self.clone();
            let r = r.clone();
            let cwd = r.cwd.clone();
            tokio::task::spawn_blocking(move || {
                let project = known.unwrap_or_else(|| crate::worktree::git_project(&r.cwd));
                (desk.records_of(&r), project)
            })
            .await
            .unwrap_or_else(|_| (Vec::new(), cwd))
        };
        self.0.ledger.projects.lock().insert(id, project.clone());
        let ending = self.0.ledger.ending.lock().remove(&id);
        let modes = match forced {
            Some(mode) => vec![mode; records.len()],
            None => self.harvest_modes(&r, &records).await,
        };
        for (path, mode) in records.iter().zip(modes) {
            if let Err(e) = self.read_record(&r, path, mode, ending, &project).await {
                tracing::warn!(session = %id, error = %e, "couldn't read a session's record");
            }
        }
    }

    /// Reads `path`, one of `r`'s records, from its cursor to its end.
    /// `ending`: its session's turn or run ended, so the exchange it leaves
    /// open is closed. The reading, folding and redacting are done off the
    /// async runtime.
    async fn read_record(
        &self,
        r: &SessionRecord,
        path: &Path,
        mode: Harvest,
        ending: bool,
        project: &str,
    ) -> eyre::Result<()> {
        let source = path.to_string_lossy().into_owned();
        loop {
            let loaded = self
                .0
                .store
                .ledger_cursor(r.id, &source)
                .await?
                .unwrap_or_default();
            let cursor = self.harvest_begins(&loaded, path, mode);
            let redactor = self.0.ledger.redactor.clone();
            let (step, next, open, closed) = {
                let (engine, path) = (r.engine, path.to_path_buf());
                tokio::task::spawn_blocking(move || {
                    let on = mode == Harvest::On;
                    let mut step = read_step(engine, &path, &cursor, on)?;
                    let mut next = std::mem::take(&mut step.cursor);
                    let (mut open, mut closed) = (None, Vec::new());
                    if mode == Harvest::Skip {
                        // Never to be remembered: the harvest goes on with
                        // the ledger. An exchange left open before is
                        // closed where it was, never joined to what is said
                        // after.
                        next.harvested_to = next.read_to;
                        let mut h = Harvested::of(&next.harvest);
                        if let Some(mut x) = h.open.take() {
                            x.ended = Some(Utc::now());
                            closed.push(x);
                            next.harvest = h.value();
                        }
                    }
                    if let (Harvest::On, Some(redactor)) = (mode, redactor.as_deref()) {
                        let mut h = Harvested::of(&next.harvest);
                        let parts = std::mem::take(&mut step.parts);
                        (open, closed) =
                            crate::memory::record::fold(h.open.take(), parts, redactor);
                        if ending && step.done {
                            if let Some(mut x) = open.take() {
                                x.ended = Some(Utc::now());
                                closed.push(x);
                            }
                        }
                        h.open = open.clone();
                        next.harvest = h.value();
                    }
                    Ok::<_, std::io::Error>((step, next, open, closed))
                })
                .await??
            };
            if next == loaded && step.spends.is_empty() {
                return Ok(());
            }
            let on = self.account_name(r.account);
            let kept: Vec<Kept> = step
                .spends
                .into_iter()
                .map(|s| self.0.ledger.stamp(r.engine, s, on.clone()))
                .collect();
            let changed = self
                .0
                .store
                .record_write(&crate::memory_store::RecordWrite {
                    session: r.id,
                    engine: r.engine,
                    source: &source,
                    cursor: &next,
                    rows: &kept,
                    closed: &closed,
                    open: open.as_ref(),
                    project,
                    folder: &r.cwd,
                    name: r.display_name.as_deref().unwrap_or(&r.name),
                    cli_session_id: r.cli_session_id.as_deref(),
                })
                .await?;
            if changed > 0 {
                self.0.ledger.untold.lock().insert(r.id);
            }
            if step.done {
                return Ok(());
            }
        }
    }

    /// The ledger summed as `q` asks, in the person's timezone.
    pub async fn spend(&self, q: &SpendQuery) -> Result<SpendView, DeskError> {
        let tz = self.timezone();
        let cells = self
            .0
            .store
            .ledger_cells(q.from, q.to, tz.name(), &q.filter)
            .await?;
        let ids: Vec<Uuid> = cells
            .iter()
            .map(|c| c.session)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut labels: HashMap<Uuid, Label> = HashMap::new();
        let missing: Vec<Uuid> = {
            let st = self.0.state.lock();
            ids.iter()
                .filter(|id| match st.records.get(id) {
                    Some(r) => {
                        labels.insert(**id, label_of(r));
                        false
                    }
                    None => true,
                })
                .copied()
                .collect()
        };
        if !missing.is_empty() {
            for r in self.0.store.sessions_of(&missing).await? {
                labels.insert(r.id, label_of(&r));
            }
        }
        if q.by == SpendBy::Group {
            return Ok(self.spend_by_group(&cells, q, &labels).await);
        }
        Ok(ledger::fold(
            &cells,
            q.by,
            |id| labels.get(&id).cloned(),
            q.limit,
            q.from,
            q.to,
        ))
    }

    /// The ledger by group, per client: each group's sessions as their calls
    /// were kept, and its local agents' spend from the hub's own records,
    /// which keep no group, by the group each agent is in now. Named by the
    /// sidebar's names, a deleted group's as it was.
    async fn spend_by_group(
        &self,
        cells: &[ledger::Cell],
        q: &SpendQuery,
        labels: &HashMap<Uuid, Label>,
    ) -> SpendView {
        let mut view = ledger::fold(
            cells,
            SpendBy::Group,
            |id| labels.get(&id).cloned(),
            usize::MAX,
            q.from,
            q.to,
        );
        let reader = self.0.agent_spend.lock().clone();
        // An agent's own spend is no session's: left out when only one is
        // asked about.
        let agents = match reader.filter(|_| q.filter.session.is_none()) {
            Some(reader) => {
                let (from, to) = (q.from, q.to);
                match tokio::task::spawn_blocking(move || reader.spent(from, to)).await {
                    Ok(Ok(spent)) => spent,
                    Ok(Err(e)) => {
                        tracing::warn!(error = format!("{e:#}"), "couldn't read agents' spend");
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "couldn't read agents' spend");
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        for (agent, used) in agents {
            if used.tokens == 0 && used.usd == 0.0 {
                continue;
            }
            let group = self.group_of_agent(&agent);
            if q.filter
                .seen
                .as_ref()
                .is_some_and(|s| !s.sees(group.as_deref()))
            {
                continue;
            }
            let key = group.clone().unwrap_or_default();
            let at = match view.rows.iter().position(|r| r.key == key) {
                Some(at) => at,
                None => {
                    view.rows.push(SpendRow {
                        label: group
                            .clone()
                            .unwrap_or_else(|| ledger::UNGROUPED_SPEND.into()),
                        key,
                        session: None,
                        engine: None,
                        folder: None,
                        totals: Default::default(),
                        agents: None,
                    });
                    view.rows.len() - 1
                }
            };
            let row = &mut view.rows[at];
            let spent = row.agents.get_or_insert_with(Default::default);
            spent.tokens += used.tokens;
            spent.usd += used.usd;
            row.totals.usd += used.usd;
            view.total.usd += used.usd;
        }
        for r in &mut view.rows {
            if !r.key.is_empty() {
                if let Some(name) = self.group_name(&r.key) {
                    r.label = name;
                }
            }
        }
        view.rows.sort_by(|a, b| {
            b.totals
                .usd
                .total_cmp(&a.totals.usd)
                .then(b.totals.tokens.total().cmp(&a.totals.tokens.total()))
                .then(a.label.cmp(&b.label))
        });
        view.more = view.rows.len().saturating_sub(q.limit);
        view.rows.truncate(q.limit);
        view
    }

    /// Where `period` began, in the person's timezone.
    pub fn period_start(&self, period: Period, now: DateTime<Utc>) -> DateTime<Utc> {
        ledger::window_start(period, now, self.timezone())
    }

    /// The ledger's rows, newest first, a page from `after` (a page's
    /// `next`).
    pub async fn ledger_page(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        filter: &Filter,
        after: Option<&str>,
    ) -> Result<LedgerPage, DeskError> {
        let after = match after {
            Some(a) => Some(
                After::decode(a)
                    .ok_or_else(|| DeskError::Invalid("that isn't a page of the ledger".into()))?,
            ),
            None => None,
        };
        let rows = self
            .0
            .store
            .ledger_rows(from, to, filter, after.as_ref())
            .await?;
        let next = (rows.len() as i64 == PAGE)
            .then(|| rows.last())
            .flatten()
            .map(|r| {
                After {
                    at: r.at,
                    engine: r.engine,
                    key: r.key.clone(),
                }
                .encode()
            });
        Ok(LedgerPage { rows, next })
    }
}

fn label_of(r: &SessionRecord) -> Label {
    Label {
        name: r.display_name.clone().unwrap_or_else(|| r.name.clone()),
        cwd: r.cwd.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_record_is_read_in_whole_lines_and_a_line_being_written_waits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let fixture = include_str!("../../fixtures/ledger/claude.jsonl");
        let (head, tail) = fixture.split_at(fixture.find("msg_02").unwrap());
        std::fs::write(&path, head).unwrap();
        let first = read_step(Engine::Claude, &path, &Cursor::default(), false).unwrap();
        assert_eq!(
            first.spends.len(),
            1,
            "msg_01 once; msg_02's line isn't whole"
        );
        assert!(first.done);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(tail.as_bytes()).unwrap();
        let second = read_step(Engine::Claude, &path, &first.cursor, false).unwrap();
        let keys: Vec<&str> = second.spends.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["msg_02"]);
        assert_eq!(second.cursor.read_to, fixture.len() as u64);
        let again = read_step(Engine::Claude, &path, &second.cursor, false).unwrap();
        assert!(again.spends.is_empty() && again.cursor == second.cursor);

        std::fs::write(&path, head).unwrap();
        let replaced = read_step(Engine::Claude, &path, &second.cursor, false).unwrap();
        assert_eq!(
            replaced.spends.len(),
            1,
            "a shorter record is read from its start"
        );
    }

    const CLAUDE_MORE: &str = concat!(
        include_str!("../../fixtures/ledger/claude.jsonl"),
        include_str!("../../fixtures/memory/claude-more.jsonl")
    );
    const CODEX_MORE: &str = concat!(
        include_str!("../../fixtures/ledger/codex-rollout.jsonl"),
        include_str!("../../fixtures/memory/codex-more.jsonl")
    );

    #[test]
    fn the_ledger_and_the_harvest_read_from_their_own_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, CLAUDE_MORE).unwrap();
        let len = CLAUDE_MORE.len() as u64;
        let ledger = read_step(Engine::Claude, &path, &Cursor::default(), false).unwrap();
        assert!(ledger.parts.is_empty(), "no harvest, no parts");
        assert_eq!(
            (ledger.cursor.read_to, ledger.cursor.harvested_to),
            (len, 0)
        );
        let calls = ledger.spends.len();
        assert!(calls > 2);

        // The ledger at the end, the harvest at 0: read once more, for the
        // harvest alone.
        let harvest = read_step(Engine::Claude, &path, &ledger.cursor, true).unwrap();
        assert!(harvest.spends.is_empty(), "no call is counted twice");
        assert!(!harvest.parts.is_empty());
        assert_eq!(
            (harvest.cursor.read_to, harvest.cursor.harvested_to),
            (len, len)
        );
        let again = read_step(Engine::Claude, &path, &harvest.cursor, true).unwrap();
        assert!(again.spends.is_empty() && again.parts.is_empty());
        assert_eq!(again.cursor, harvest.cursor);

        // Both at once from the start find what each did alone.
        let both = read_step(Engine::Claude, &path, &Cursor::default(), true).unwrap();
        assert_eq!(both.spends, ledger.spends);
        assert_eq!(both.parts, harvest.parts);

        // The harvest held back (no redactor) stays where it was while the
        // ledger reads on.
        let head = &CLAUDE_MORE[..CLAUDE_MORE.find("msg_03").unwrap()];
        let head = &head[..=head.rfind('\n').unwrap()];
        std::fs::write(&path, head).unwrap();
        let first = read_step(Engine::Claude, &path, &Cursor::default(), true).unwrap();
        std::fs::write(&path, CLAUDE_MORE).unwrap();
        let held = read_step(Engine::Claude, &path, &first.cursor, false).unwrap();
        assert_eq!(
            (held.cursor.read_to, held.cursor.harvested_to),
            (len, head.len() as u64)
        );
        let caught_up = read_step(Engine::Claude, &path, &held.cursor, true).unwrap();
        assert!(caught_up.spends.is_empty());
        let mut parts = first.parts.clone();
        parts.extend(caught_up.parts);
        assert_eq!(
            parts, both.parts,
            "the harvest catches up where it was left"
        );

        // A record shorter than either offset was replaced: both read again.
        std::fs::write(&path, head).unwrap();
        let replaced = read_step(Engine::Claude, &path, &caught_up.cursor, true).unwrap();
        assert_eq!(replaced.parts, first.parts);
        assert_eq!(replaced.spends.len(), first.spends.len());
        let ledger_alone = read_step(Engine::Claude, &path, &caught_up.cursor, false).unwrap();
        assert_eq!(
            (
                ledger_alone.cursor.read_to,
                ledger_alone.cursor.harvested_to
            ),
            (head.len() as u64, 0),
            "a replaced record resets the harvest's offset too"
        );
    }

    #[test]
    fn a_harvest_catching_up_never_moves_the_ledgers_codex_state_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(&path, CODEX_MORE).unwrap();
        let ledger = read_step(Engine::Codex, &path, &Cursor::default(), false).unwrap();
        let harvest = read_step(Engine::Codex, &path, &ledger.cursor, true).unwrap();
        assert_eq!(harvest.cursor.seen, ledger.cursor.seen);
        let kept = Harvested::of(&harvest.cursor.harvest);
        assert_eq!(kept.codex.turn.as_deref(), Some("turn-3"));
        let asks = harvest
            .parts
            .iter()
            .filter(|p| matches!(p, Part::Ask { .. }))
            .count();
        assert_eq!(asks, 2);
    }

    #[test]
    fn a_line_too_long_to_keep_is_stepped_over() {
        let mut r = std::io::Cursor::new({
            let mut bytes = vec![b'x'; MAX_LINE + 10];
            bytes.extend_from_slice(b"\n{\"a\":1}\n");
            bytes
        });
        let mut buf = Vec::new();
        assert!(
            matches!(next_line(&mut r, &mut buf).unwrap(), Line::Skipped(n) if n == MAX_LINE as u64 + 11)
        );
        assert!(matches!(
            next_line(&mut r, &mut buf).unwrap(),
            Line::Whole(8)
        ));
        assert_eq!(buf, b"{\"a\":1}\n");
        assert!(matches!(next_line(&mut r, &mut buf).unwrap(), Line::End));
    }

    #[test]
    fn a_codex_rollout_carries_its_turn_and_model_across_steps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let fixture = include_str!("../../fixtures/ledger/codex-rollout.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        std::fs::write(&path, format!("{}\n", lines[..5].join("\n"))).unwrap();
        let first = read_step(Engine::Codex, &path, &Cursor::default(), false).unwrap();
        assert_eq!(first.spends.len(), 1);
        std::fs::write(&path, fixture).unwrap();
        let second = read_step(Engine::Codex, &path, &first.cursor, false).unwrap();
        assert_eq!(second.spends.len(), 1);
        assert_eq!(second.spends[0].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(second.spends[0].billing, Some(Billing::Plan));
    }

    #[test]
    fn a_session_reads_its_sub_agents_records_too() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("abc.jsonl");
        std::fs::write(&t, "").unwrap();
        std::fs::create_dir_all(dir.path().join("abc/subagents")).unwrap();
        for name in ["agent-b.jsonl", "agent-a.jsonl", "agent-a.meta.json"] {
            std::fs::write(dir.path().join("abc/subagents").join(name), "").unwrap();
        }
        let names: Vec<String> = claude_records(&t)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["abc.jsonl", "agent-a.jsonl", "agent-b.jsonl"]);
    }

    async fn spent(d: &Desk, filter: Filter) -> SpendView {
        d.spend(&SpendQuery {
            by: SpendBy::Session,
            from: DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
                .unwrap()
                .to_utc(),
            to: DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
                .unwrap()
                .to_utc(),
            limit: 10,
            filter,
        })
        .await
        .unwrap()
    }

    /// Waits for the ledger to hold `calls` calls in all.
    async fn until_calls(d: &Desk, calls: u64) -> SpendView {
        for _ in 0..400 {
            let v = spent(d, Filter::default()).await;
            if v.total.calls >= calls {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the ledger never held {calls} calls");
    }

    #[tokio::test]
    async fn known_sessions_history_is_read_at_start_once_each_call_and_a_fork_counts_nothing_twice(
    ) {
        use crate::store::{build_pool, tests::record_fixture, DeskStore};
        use workforce_postgres::test_support::with_test_pool;
        with_test_pool(build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let fixture = include_str!("../../fixtures/ledger/claude.jsonl");
            let transcript = dir.path().join("web.jsonl");
            std::fs::write(&transcript, fixture).unwrap();
            let sub = dir.path().join("web/subagents");
            std::fs::create_dir_all(&sub).unwrap();
            let line = |id: &str, out: u64| {
                format!(
                    "{{\"type\":\"assistant\",\"timestamp\":\"2026-10-31T09:10:00Z\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5-5\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":{out}}}}}}}\n"
                )
            };
            std::fs::write(sub.join("agent-a1.jsonl"), line("msg_sub", 500_000)).unwrap();
            // A fork of the same conversation, closed: its copied calls are
            // the first session's.
            let fork = dir.path().join("fork.jsonl");
            std::fs::write(&fork, fixture).unwrap();
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut web = record_fixture("acme-web");
            web.transcript = Some(transcript.to_string_lossy().into_owned());
            web.created_at -= chrono::Duration::hours(2);
            let mut forked = record_fixture("acme-web fork");
            forked.state = crate::model::SessionState::Closed;
            forked.transcript = Some(fork.to_string_lossy().into_owned());
            // A hub's agent's turn, adopted from another terminal: its
            // agent's spend, never the person's.
            let turn_transcript = dir.path().join("turn.jsonl");
            std::fs::write(&turn_transcript, line("msg_turn", 9_000_000)).unwrap();
            let mut turn = record_fixture("at-dm-d4774cbb6c89-2e");
            turn.cwd = "/data/agents/scout/projects/_agent-turn/work/session".into();
            turn.transcript = Some(turn_transcript.to_string_lossy().into_owned());
            store.put(&web).await.unwrap();
            store.put(&forked).await.unwrap();
            store.put(&turn).await.unwrap();
            let d = Desk::start(
                store,
                crate::testing::options(dir.path()),
                super::super::Floor {
                    pricer: Some(Arc::new(Table)),
                    ..Default::default()
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            let mut events = d.0.events.subscribe();
            let v = until_calls(&d, 3).await;
            assert_eq!(v.rows.len(), 1, "the fork spent nothing of its own: {:?}", v.rows);
            assert_eq!(v.rows[0].session, Some(web.id));
            assert_eq!(v.rows[0].label, "acme-web");
            assert_eq!(v.total.calls, 3, "msg_01 and msg_02 once, and the sub-agent's call");
            assert_eq!(v.total.tokens.output, 350 + 90 + 500_000);
            assert!((v.total.usd - (350.0 + 90.0 + 500_000.0) * 20.0 / 1e6).abs() < 1e-9);
            assert_eq!(v.total.billing, [Billing::Unknown].into(), "history from before Desk saw a sign-in");
            let told = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Ok(DeskEvent::Spend { session }) = events.recv().await {
                        break session;
                    }
                }
            })
            .await
            .expect("new spend is announced");
            assert_eq!(told, web.id);

            // More of the transcript: read from where it was left, once.
            let mut f = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
            std::io::Write::write_all(&mut f, line("msg_03", 10).as_bytes()).unwrap();
            d.spend_read(web.id);
            d.spend_read(web.id);
            d.spend_read(forked.id);
            d.spend_read(turn.id);
            let v = until_calls(&d, 4).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(spent(&d, Filter::default()).await.total.calls, 4);
            assert_eq!(v.rows[0].totals.calls, 4);

            let page = d
                .ledger_page(
                    DateTime::<Utc>::UNIX_EPOCH,
                    Utc::now(),
                    &Filter {
                        session: Some(web.id),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();
            let keys: Vec<&str> = page.rows.iter().map(|r| r.key.as_str()).collect();
            assert_eq!(keys, ["msg_sub", "msg_03", "msg_02", "msg_01"], "newest first");
            assert_eq!(page.next, None);
        })
        .await;
    }

    struct Table;

    impl Pricer for Table {
        fn usd(&self, _: Engine, _: Billing, model: &str, t: &ledger::Tokens) -> Option<f64> {
            (model == "claude-opus-5-5").then(|| t.output as f64 * 20.0 / 1e6)
        }
    }

    fn spend(at: &str, model: &str) -> Spend {
        Spend {
            key: "msg_1".into(),
            at: DateTime::parse_from_rfc3339(at).unwrap().to_utc(),
            model: Some(model.into()),
            tokens: ledger::Tokens {
                output: 1_000_000,
                ..Default::default()
            },
            usd: None,
            priced: Priced::None,
            billing: None,
        }
    }

    #[test]
    fn a_call_is_stamped_with_the_sign_in_seen_when_it_was_made_and_priced_from_the_table() {
        let (run, _rx) = LedgerRun::new(Some(Arc::new(Table)), None);
        let since = DateTime::parse_from_rfc3339("2026-10-31T09:00:00Z")
            .unwrap()
            .to_utc();
        let plan = crate::engine::claude_sign_in(
            r#"{"loggedIn":true,"authMethod":"claude.ai","email":"allan@example.com","orgId":"org-1","subscriptionType":"ultra"}"#,
        );
        run.signed_in(since, [(Engine::Claude, plan.clone())].into_iter());
        run.signed_in(
            since + chrono::Duration::hours(1),
            [(Engine::Claude, plan)].into_iter(),
        );
        let now = run.stamp(
            Engine::Claude,
            spend("2026-10-31T09:30:00Z", "claude-opus-5-5"),
            None,
        );
        assert_eq!(
            (
                now.billing,
                now.account.as_deref(),
                now.spend.usd,
                now.spend.priced
            ),
            (
                Billing::Plan,
                Some("allan@example.com"),
                Some(20.0),
                Priced::Priced
            ),
            "the same sign-in seen again keeps when it was first seen"
        );
        let before = run.stamp(
            Engine::Claude,
            spend("2026-10-30T22:00:00Z", "claude-opus-5-5"),
            None,
        );
        assert_eq!(
            (before.billing, before.account),
            (Billing::Unknown, None),
            "history from before Desk saw who was signed in isn't anyone's"
        );
        let unknown = run.stamp(
            Engine::Claude,
            spend("2026-10-31T09:30:00Z", "claude-mystery-1"),
            None,
        );
        assert_eq!(
            (unknown.spend.usd, unknown.spend.priced),
            (None, Priced::None)
        );
        let codex = run.stamp(
            Engine::Codex,
            Spend {
                billing: Some(Billing::Plan),
                ..spend("2026-10-31T09:30:00Z", "gpt-5.6-sol")
            },
            None,
        );
        assert_eq!(codex.billing, Billing::Plan, "what the record says wins");
        let work = run.stamp(
            Engine::Claude,
            spend("2026-10-30T22:00:00Z", "claude-opus-5-5"),
            Some("Work".into()),
        );
        assert_eq!(
            (work.billing, work.account.as_deref()),
            (Billing::Plan, Some("Work")),
            "a session on one of the person's accounts is that account's"
        );
    }
}
