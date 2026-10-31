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

use super::{Desk, DeskError};
use crate::engine::{Engine, SignIn, Via};
use crate::ledger::{
    self, Billing, CodexState, Label, Period, Priced, Pricer, Spend, SpendBy, SpendView,
};
use crate::ledger_store::{After, Cursor, Filter, Kept, LedgerRow, PAGE};
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

/// The ledger's share of the desk.
pub(super) struct LedgerRun {
    jobs: mpsc::UnboundedSender<Job>,
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
}

impl LedgerRun {
    pub(super) fn new(pricer: Option<Arc<dyn Pricer>>) -> (Self, mpsc::UnboundedReceiver<Job>) {
        let (jobs, rx) = mpsc::unbounded_channel();
        (
            Self {
                jobs,
                waiting: Mutex::default(),
                read_at: Mutex::default(),
                untold: Mutex::default(),
                signed: Mutex::default(),
                pricer,
            },
            rx,
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
    /// record says, else the sign-in Desk saw when it was made.
    fn stamp(&self, engine: Engine, spend: Spend) -> Kept {
        let signed = self.signed.lock().get(&engine).cloned();
        let (billing, account) = match signed {
            Some((SignIn::In { via, who, ids, .. }, since)) if spend.at >= since => (
                match via {
                    Via::Plan => Billing::Plan,
                    Via::Key => Billing::Key,
                    Via::Vendor => Billing::Vendor,
                },
                who.or_else(|| ids.into_iter().next()),
            ),
            _ => (Billing::Unknown, None),
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
    cursor: Cursor,
    /// The record has nothing more to read now.
    done: bool,
}

/// Reads one step of `engine`'s record at `path` from `cursor`: whole lines
/// only, so a line still being written is read next time. A record
/// shorter than its cursor was replaced, and is read again from its start.
fn read_step(engine: Engine, path: &Path, cursor: &Cursor) -> std::io::Result<Step> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let (start, mut codex) = match len < cursor.read_to {
        true => (0, CodexState::default()),
        false => (
            cursor.read_to,
            serde_json::from_value(cursor.seen.clone()).unwrap_or_default(),
        ),
    };
    file.seek(SeekFrom::Start(start))?;
    let mut r = BufReader::with_capacity(256 * 1024, file);
    let mut buf = Vec::new();
    let mut at = start;
    let mut spends = Vec::new();
    let done = loop {
        if at - start >= STEP_BYTES {
            break at >= len;
        }
        match next_line(&mut r, &mut buf)? {
            Line::Whole(n) => {
                at += n;
                let spend = match engine {
                    Engine::Claude => ledger::claude_line(&buf),
                    Engine::Codex => ledger::codex_line(&buf, &mut codex),
                    Engine::Gemini
                    | Engine::Copilot
                    | Engine::OpenCode
                    | Engine::Cursor
                    | Engine::Devin
                    | Engine::Custom => None,
                };
                spends.extend(spend);
            }
            Line::Skipped(n) => at += n,
            Line::Partial | Line::End => break true,
        }
    };
    let seen = match engine {
        Engine::Codex => serde_json::to_value(&codex).unwrap_or_default(),
        _ => serde_json::json!({}),
    };
    Ok(Step {
        spends: ledger::once_each(spends),
        cursor: Cursor { read_to: at, seen },
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
    pub(super) async fn ledger_loop(self, mut jobs: mpsc::UnboundedReceiver<Job>) {
        match self.0.store.every_session().await {
            Ok(all) => all.iter().for_each(|r| self.spend_read(r.id)),
            Err(e) => tracing::warn!(error = %e, "couldn't list the sessions whose spend to read"),
        }
        let mut tell = tokio::time::interval(TELL_EVERY);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tell.tick() => self.tell_spend(),
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
        for session in told {
            let _ = self.0.events.send(DeskEvent::Spend { session });
        }
    }

    async fn keep_live(&self, session: Uuid, spend: Spend) {
        let Some(engine) = self.session_record(session).await.map(|r| r.engine) else {
            return;
        };
        let kept = self.0.ledger.stamp(engine, spend);
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
    fn records_of(&self, r: &SessionRecord) -> Vec<PathBuf> {
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

    /// Reads each of `id`'s records from its cursor to its end.
    async fn read_spend(&self, id: Uuid) {
        let Some(r) = self.session_record(id).await else {
            return;
        };
        let records = {
            let desk = self.clone();
            let r = r.clone();
            tokio::task::spawn_blocking(move || desk.records_of(&r))
                .await
                .unwrap_or_default()
        };
        for path in records {
            if let Err(e) = self.read_record(&r, &path).await {
                tracing::warn!(session = %id, error = %e, "couldn't read a session's spend");
            }
        }
    }

    async fn read_record(&self, r: &SessionRecord, path: &Path) -> eyre::Result<()> {
        let source = path.to_string_lossy().into_owned();
        loop {
            let cursor = self
                .0
                .store
                .ledger_cursor(r.id, &source)
                .await?
                .unwrap_or_default();
            let step = {
                let (engine, path, cursor) = (r.engine, path.to_path_buf(), cursor.clone());
                tokio::task::spawn_blocking(move || read_step(engine, &path, &cursor)).await??
            };
            if step.cursor == cursor && step.spends.is_empty() {
                return Ok(());
            }
            let kept: Vec<Kept> = step
                .spends
                .into_iter()
                .map(|s| self.0.ledger.stamp(r.engine, s))
                .collect();
            let changed = self
                .0
                .store
                .ledger_write(r.id, r.engine, Some((&source, &step.cursor)), &kept)
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
        Ok(ledger::fold(
            &cells,
            q.by,
            |id| labels.get(&id).cloned(),
            q.limit,
            q.from,
            q.to,
        ))
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
        let first = read_step(Engine::Claude, &path, &Cursor::default()).unwrap();
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
        let second = read_step(Engine::Claude, &path, &first.cursor).unwrap();
        let keys: Vec<&str> = second.spends.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["msg_02"]);
        assert_eq!(second.cursor.read_to, fixture.len() as u64);
        let again = read_step(Engine::Claude, &path, &second.cursor).unwrap();
        assert!(again.spends.is_empty() && again.cursor == second.cursor);

        std::fs::write(&path, head).unwrap();
        let replaced = read_step(Engine::Claude, &path, &second.cursor).unwrap();
        assert_eq!(
            replaced.spends.len(),
            1,
            "a shorter record is read from its start"
        );
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
        let first = read_step(Engine::Codex, &path, &Cursor::default()).unwrap();
        assert_eq!(first.spends.len(), 1);
        std::fs::write(&path, fixture).unwrap();
        let second = read_step(Engine::Codex, &path, &first.cursor).unwrap();
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
            store.put(&web).await.unwrap();
            store.put(&forked).await.unwrap();
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
        let (run, _rx) = LedgerRun::new(Some(Arc::new(Table)));
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
        );
        assert_eq!(
            (before.billing, before.account),
            (Billing::Unknown, None),
            "history from before Desk saw who was signed in isn't anyone's"
        );
        let unknown = run.stamp(
            Engine::Claude,
            spend("2026-10-31T09:30:00Z", "claude-mystery-1"),
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
        );
        assert_eq!(codex.billing, Billing::Plan, "what the record says wins");
    }
}
