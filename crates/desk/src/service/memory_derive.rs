//! Session memory's notes: one worker reads each due session's new
//! exchanges for decisions, facts and to-dos with the hub's cheap model,
//! within `derive_calls_per_day`, and keeps what [`derive::validate`]
//! allows; the person confirms, corrects or drops what it noted. Nothing of
//! a session is sent to the model while derivation is off for the Desk, for
//! its group (until the person lets it), or memory is paused.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::{Desk, DeskError, UNGROUPED};
use crate::memory::derive::{self, Deriver, Input, InputExchange, ItemKind, ItemState};
use crate::memory_derive_store::{model_name, Apply, Claim, ItemRow, Unread, UnreadGroup};

/// How long a claim holds while its model call runs: past it, a hub that
/// stopped mid-call finds the session due again.
const LEASE: Duration = Duration::from_secs(5 * 60);
/// The longest a model call is given.
const CALL_WITHIN: Duration = Duration::from_secs(2 * 60);
/// How often an idle worker looks for sessions due.
const IDLE_EVERY: Duration = Duration::from_secs(30);
/// How often a worker at the day's limit looks again.
const PAUSED_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often a session's reading is tried before Desk stops trying until
/// it says more.
const TRIES: i32 = 5;

/// What the person reads while noting is off for the Desk.
pub const DERIVE_OFF: &str =
    "Desk isn't noting decisions, facts or to-dos: that's turned off for this Desk.";
/// While a group's sessions aren't read for notes.
pub const DERIVE_GROUP_OFF: &str = "Desk doesn't note decisions, facts or to-dos from this group's sessions until you let its model read them, in the group's settings.";
/// While the day's calls are spent.
pub const DERIVE_PAUSED: &str =
    "Desk has noted as much as it may today, so noting is paused until tomorrow.";
/// Without a model to write with.
pub const NO_DERIVER: &str = "Desk has no model to note decisions with on this computer.";
/// While memory is paused or off for the session.
pub const DERIVE_MEMORY_OFF: &str =
    "Desk isn't noting decisions while it isn't remembering this session.";
/// While remembering is paused for every session.
pub const DERIVE_PAUSED_MEMORY: &str =
    "Desk isn't noting decisions while remembering is paused for every session.";
/// After it kept failing.
pub const DERIVE_FAILED: &str =
    "Desk couldn't note decisions from this session lately; it tries again when the session says more.";

/// The derivation worker's share of the desk.
pub(super) struct DeriveRun {
    pub(super) deriver: Option<Arc<dyn Deriver>>,
    poke: tokio::sync::Notify,
}

impl DeriveRun {
    pub(super) fn new(deriver: Option<Arc<dyn Deriver>>) -> Self {
        Self {
            deriver,
            poke: tokio::sync::Notify::new(),
        }
    }
}

/// Whether Desk is noting decisions from a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Noting {
    On,
    /// The day's calls are spent: until tomorrow.
    Paused,
    Off,
}

/// A note as the person reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteView {
    pub id: Uuid,
    pub kind: ItemKind,
    pub text: String,
    pub state: ItemState,
    /// Since when it is so.
    pub at: DateTime<Utc>,
    /// Desk's model wrote it, and it may be wrong.
    pub noted_by_desk: bool,
    /// The person confirmed or wrote it.
    pub confirmed: bool,
    /// None of the exchanges it cites is kept any more: its text stays.
    pub source_gone: bool,
}

impl From<&ItemRow> for NoteView {
    fn from(r: &ItemRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            text: r.text.clone(),
            state: r.state,
            at: r.valid_from,
            noted_by_desk: r.by != "person",
            confirmed: r.confirmed || r.by == "person",
            source_gone: !r.source_kept,
        }
    }
}

/// A note another replaced, as the person reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplacedView {
    pub id: Uuid,
    pub kind: ItemKind,
    pub text: String,
    /// When it was replaced.
    pub replaced_at: DateTime<Utc>,
}

/// The most replaced notes a session's panel lists.
const REPLACED_MAX: i64 = 20;

/// Why Desk isn't noting a session's decisions, or what else to know:
/// what the panel acts on, and what the person reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NotingWhy {
    DeskOff,
    NoModel,
    /// The person paused remembering for every session.
    Paused,
    /// The person turned remembering off for this session.
    SessionOff,
    GroupOff,
    DayCap,
    Failing,
}

impl NotingWhy {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::DeskOff => "desk_off",
            Self::NoModel => "no_model",
            Self::Paused => "paused",
            Self::SessionOff => "session_off",
            Self::GroupOff => "group_off",
            Self::DayCap => "day_cap",
            Self::Failing => "failing",
        }
    }

    pub(super) fn plain(self) -> &'static str {
        match self {
            Self::DeskOff => DERIVE_OFF,
            Self::NoModel => NO_DERIVER,
            Self::Paused => DERIVE_PAUSED_MEMORY,
            Self::SessionOff => DERIVE_MEMORY_OFF,
            Self::GroupOff => DERIVE_GROUP_OFF,
            Self::DayCap => DERIVE_PAUSED,
            Self::Failing => DERIVE_FAILED,
        }
    }
}

/// How many of each a session holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteCounts {
    pub decisions: usize,
    pub facts: usize,
    pub todos: usize,
    pub open_todos: usize,
    /// Every note Desk keeps of the session, those replaced and dropped
    /// too: what forgetting it deletes.
    pub kept: i64,
}

/// What Desk noted from a session, and whether it is noting.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notes {
    pub decisions: Vec<NoteView>,
    pub facts: Vec<NoteView>,
    pub todos: Vec<NoteView>,
    pub counts: NoteCounts,
    pub noting: Noting,
    /// Why it isn't, or what else to know, in plain words.
    pub why: Option<String>,
    /// The same, for the panel to act on: `desk_off`, `no_model`,
    /// `paused` (every session), `session_off`, `group_off` (the group's
    /// settings may let it), `day_cap` or `failing`.
    pub reason: Option<&'static str>,
    /// Notes another replaced lately, latest first: what they said, and
    /// when.
    pub replaced: Vec<ReplacedView>,
    /// Exchanges not yet read for notes.
    pub unread: i64,
    /// Whose model writes the notes: the provider the floor's cheap model
    /// runs on, under the hub's own account (`anthropic`, `codex`, ...).
    pub provider: Option<String>,
    /// The floor may write on another provider when that one can't
    /// (`[floor] fallback_providers`); never for a Desk session today, whose
    /// call has no person's login in scope.
    pub fallback_providers: bool,
}

/// What the person does to a note.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ItemChange {
    Confirm,
    Correct { text: String },
    Drop,
}

/// A reading for notes made in the same call as an account's refresh:
/// its claim, how far its group was read and the last exchange it reads,
/// whether more wait after them, and what the model reads.
pub(super) struct SharedRead {
    claim: Claim,
    read: (i32, i32),
    backlog: bool,
    group: Option<String>,
    rows: Vec<Unread>,
    pub(super) input: Input,
}

/// What one turn of the worker found.
enum Did {
    Work,
    Idle,
    Paused,
}

/// The exchanges one call reads, of `unread` (after `derived_to`, in
/// order): closed ones in one group, at most [`derive::BATCH`], up to the
/// first still open. Whether more closed ones wait after them.
fn batch(unread: &[Unread]) -> (&[Unread], bool) {
    let Some(first) = unread.first() else {
        return (&[], false);
    };
    let n = unread
        .iter()
        .take(derive::BATCH)
        .take_while(|x| x.ended.is_some() && x.group == first.group)
        .count();
    let backlog = unread.get(n).is_some_and(|x| x.ended.is_some());
    (&unread[..n], backlog)
}

/// How long after `tries` failures a session is tried again: a minute,
/// doubling, at most an hour.
fn retry_after(tries: i32) -> Duration {
    let mins = 1u64 << u32::try_from(tries.clamp(0, 6)).unwrap_or(0);
    Duration::from_secs(mins * 60).min(Duration::from_secs(60 * 60))
}

impl Desk {
    /// Tells the derivation worker there may be work.
    pub(super) fn derive_poke(&self) {
        self.0.derive.poke.notify_one();
    }

    /// The day calls are counted on: today in the person's time.
    pub(super) fn derive_day(&self) -> NaiveDate {
        let now = Utc::now();
        match self.budget_tz() {
            Some(tz) => now.with_timezone(&tz).date_naive(),
            None => now.date_naive(),
        }
    }

    /// The derivation worker, until the desk stops. Without a model, or
    /// with derivation off for the Desk, it ends at once.
    pub(super) async fn derive_loop(self) {
        let Some(deriver) = self.0.derive.deriver.clone() else {
            return;
        };
        let mut failures = 0u32;
        loop {
            if self.0.cancel.is_cancelled() {
                return;
            }
            // Off for the Desk: waits, since the person may turn it on.
            let m = self.memory_now();
            if !m.derive || !m.enabled {
                tokio::select! {
                    _ = self.0.cancel.cancelled() => return,
                    _ = self.0.derive.poke.notified() => {}
                    _ = tokio::time::sleep(PAUSED_EVERY) => {}
                }
                continue;
            }
            let wait = match self.derive_step(&*deriver).await {
                Ok(Did::Work) => {
                    failures = 0;
                    continue;
                }
                Ok(Did::Idle) => {
                    failures = 0;
                    IDLE_EVERY
                }
                Ok(Did::Paused) => PAUSED_EVERY,
                Err(e) => {
                    failures += 1;
                    if failures == 1 {
                        tracing::warn!(
                            error = format!("{e:#}"),
                            "couldn't note decisions from sessions; trying again"
                        );
                    }
                    Duration::from_secs(1 << failures.min(8)).min(IDLE_EVERY * 10)
                }
            };
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = self.0.derive.poke.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Reads one due session's new exchanges for notes.
    async fn derive_step(&self, deriver: &dyn Deriver) -> eyre::Result<Did> {
        let cap = self.0.opts.memory.derive_calls_per_day;
        let day = self.derive_day();
        if self.0.store.derive_calls(day).await? >= i64::from(cap) {
            return Ok(Did::Paused);
        }
        let Some(claim) = self.0.store.derive_claim(LEASE.as_secs_f64()).await? else {
            return Ok(Did::Idle);
        };
        // Each group is read on its own: the first whose sessions may be
        // read, so one that may not holds only its own place.
        let mut chosen: Option<UnreadGroup> = None;
        let mut others = false;
        for g in self.0.store.derive_unread_groups(claim.session).await? {
            let facts = self
                .0
                .store
                .derive_facts(claim.session, g.group.as_deref())
                .await?;
            if facts.paused || facts.session_off {
                // Nothing of the session is sent; what it said waits.
                self.0.store.derive_release(&claim, None).await?;
                return Ok(Did::Work);
            }
            match (facts.group_on, chosen.is_some()) {
                (true, false) => chosen = Some(g),
                (true, true) => others = true,
                (false, _) => {}
            }
        }
        let Some(read) = chosen else {
            // Nothing closed to read, or only in groups not read yet: due
            // again as a turn ends, or the person lets them be read.
            self.0.store.derive_release(&claim, None).await?;
            return Ok(Did::Work);
        };
        let group = read.group.clone();
        let unread = self
            .0
            .store
            .derive_unread(
                claim.session,
                group.as_deref(),
                read.from,
                i64::try_from(derive::BATCH).unwrap_or(12) + 1,
            )
            .await?;
        let (rows, more) = batch(&unread);
        let backlog = more || others;
        let Some(last) = rows.last() else {
            self.0.store.derive_release(&claim, None).await?;
            return Ok(Did::Work);
        };
        let input = Input {
            purpose: None,
            items: Vec::new(),
            // What reads as instructions to a model is never read for
            // notes, as it is left out of briefs.
            exchanges: rows
                .iter()
                .filter(|x| !x.flagged)
                .map(|x| InputExchange {
                    id: x.id,
                    at: x.at,
                    asker: x.asker,
                    ask: x.ask.clone(),
                    answer: x.answer.clone(),
                    steps: x.steps.clone(),
                })
                .collect(),
        };
        if input.exchanges.is_empty() {
            self.derive_write(
                &claim,
                (read.from, last.seq),
                backlog,
                group.as_deref(),
                "",
                rows,
                &Default::default(),
            )
            .await?;
            return Ok(Did::Work);
        }
        let input = Input {
            purpose: deriver.purpose(claim.session).await,
            items: self
                .0
                .store
                .held_items(claim.session, group.as_deref())
                .await?,
            ..input
        };
        if !self.0.store.derive_take_call(day, cap).await? {
            self.0
                .store
                .derive_release(&claim, Some(Utc::now()))
                .await?;
            return Ok(Did::Paused);
        }
        let nonce = Uuid::new_v4().simple().to_string()[..16].to_string();
        let asked = tokio::time::timeout(
            CALL_WITHIN,
            deriver.answer(
                claim.session,
                last.engine,
                derive::SYSTEM,
                &derive::prompt(&input, &nonce),
            ),
        )
        .await;
        let answered = match asked {
            Ok(Ok(a)) => a,
            failed => {
                let e = match failed {
                    Ok(Err(e)) => {
                        // No model took it: the day's count is given back.
                        self.0.store.derive_untake_call(day).await?;
                        e
                    }
                    // One that took too long may have been spent: it counts.
                    _ => eyre::eyre!("the model took too long"),
                };
                let gave_up = claim.tries + 1 >= TRIES;
                tracing::info!(session = %claim.session, error = format!("{e:#}"), gave_up, "couldn't note decisions from a session");
                self.0
                    .store
                    .derive_failed(
                        &claim,
                        retry_after(claim.tries),
                        gave_up,
                        DERIVE_FAILED,
                        None,
                    )
                    .await?;
                return Ok(Did::Work);
            }
        };
        let Some(out) = derive::parse(&answered.text) else {
            // It answered with something else: tried again a few times,
            // then this stretch is passed over, so one batch can't hold up
            // the session or spend the day's calls.
            let gave_up = claim.tries + 1 >= TRIES;
            tracing::info!(session = %claim.session, gave_up, "the model's notes weren't JSON");
            self.0
                .store
                .derive_failed(
                    &claim,
                    retry_after(claim.tries),
                    gave_up,
                    DERIVE_FAILED,
                    gave_up.then_some((group.as_deref(), last.seq)),
                )
                .await?;
            return Ok(Did::Work);
        };
        self.derive_keep(
            &claim,
            (read.from, last.seq),
            backlog,
            group.as_deref(),
            rows,
            &input,
            &out,
            &answered.model,
        )
        .await?;
        Ok(Did::Work)
    }

    /// Keeps what [`derive::validate`] allows of the notes `model` wrote
    /// from `rows` (read as `input`), and moves the session past them.
    #[allow(clippy::too_many_arguments)]
    async fn derive_keep(
        &self,
        claim: &Claim,
        read: (i32, i32),
        backlog: bool,
        group: Option<&str>,
        rows: &[Unread],
        input: &Input,
        out: &derive::Out,
        model: &str,
    ) -> eyre::Result<()> {
        let mut applied = derive::validate(out, input);
        // What the model wrote is kept as safely as what it read.
        if let Some(r) = &self.0.ledger.redactor {
            for a in &mut applied.add {
                a.text = r.stored(&a.text).text.into_owned();
            }
        }
        if applied.dropped > 0 {
            tracing::info!(session = %claim.session, dropped = applied.dropped, kept = applied.add.len(), "dropped notes the record doesn't bear out");
        }
        let by = format!("model:{}", model_name(model));
        self.derive_write(claim, read, backlog, group, &by, rows, &applied)
            .await
    }

    /// Session `id`'s reading for notes, to be made in the same call as its
    /// account's refresh that reads the closed exchanges `read` (by key):
    /// only while noting is on for it and within the day's calls, it is due
    /// now and no worker holds it, and the exchanges it would read next
    /// begin among those. None otherwise, and the account is refreshed
    /// alone; the worker reads what is left as it would.
    pub(super) async fn derive_shared(&self, id: Uuid, read: &HashSet<&str>) -> Option<SharedRead> {
        let m = self.memory_now();
        let deriver = self.0.derive.deriver.clone()?;
        if !m.enabled || !m.derive || read.is_empty() {
            return None;
        }
        match self.derive_shared_try(id, &*deriver, read).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(session = %id, error = format!("{e:#}"), "couldn't read a session for notes with its account; it is read on its own");
                None
            }
        }
    }

    async fn derive_shared_try(
        &self,
        id: Uuid,
        deriver: &dyn Deriver,
        read: &HashSet<&str>,
    ) -> eyre::Result<Option<SharedRead>> {
        let cap = self.0.opts.memory.derive_calls_per_day;
        if self.0.store.derive_calls(self.derive_day()).await? >= i64::from(cap) {
            return Ok(None);
        }
        // Due now, and no worker holds it; else the account goes alone.
        let Some(claim) = self
            .0
            .store
            .derive_claim_session(id, LEASE.as_secs_f64())
            .await?
        else {
            return Ok(None);
        };
        match self.derive_shared_of(claim.clone(), deriver, read).await {
            Ok(Some(s)) => Ok(Some(s)),
            left => {
                // Still due: the worker reads it as it would have.
                self.0
                    .store
                    .derive_release(&claim, Some(Utc::now()))
                    .await?;
                left
            }
        }
    }

    /// What `claim`'s reading shared with an account would read: one group
    /// whose sessions may be read, from its first unread exchange, as far
    /// as the account reads.
    async fn derive_shared_of(
        &self,
        claim: Claim,
        deriver: &dyn Deriver,
        read: &HashSet<&str>,
    ) -> eyre::Result<Option<SharedRead>> {
        let groups = self.0.store.derive_unread_groups(claim.session).await?;
        // More than one group waits: the worker reads them in turn.
        let [g] = groups.as_slice() else {
            return Ok(None);
        };
        let facts = self
            .0
            .store
            .derive_facts(claim.session, g.group.as_deref())
            .await?;
        if facts.paused || facts.session_off || !facts.group_on {
            return Ok(None);
        }
        let unread = self
            .0
            .store
            .derive_unread(
                claim.session,
                g.group.as_deref(),
                g.from,
                i64::try_from(derive::BATCH).unwrap_or(12) + 1,
            )
            .await?;
        let (batch, more) = batch(&unread);
        // Behind (its first unread is before what the account reads), it
        // reads its own; ahead, as far as the account reads, the rest next.
        let n = batch
            .iter()
            .take_while(|x| read.contains(x.key.as_str()))
            .count();
        let rows = &batch[..n];
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        let exchanges: Vec<InputExchange> = rows
            .iter()
            .filter(|x| !x.flagged)
            .map(|x| InputExchange {
                id: x.id,
                at: x.at,
                asker: x.asker,
                ask: x.ask.clone(),
                answer: x.answer.clone(),
                steps: x.steps.clone(),
            })
            .collect();
        // Only what reads as instructions: the worker passes it over
        // without a call.
        if exchanges.is_empty() {
            return Ok(None);
        }
        let input = Input {
            purpose: deriver.purpose(claim.session).await,
            items: self
                .0
                .store
                .held_items(claim.session, g.group.as_deref())
                .await?,
            exchanges,
        };
        Ok(Some(SharedRead {
            read: (g.from, last.seq),
            backlog: more || n < batch.len(),
            group: g.group.clone(),
            rows: rows.to_vec(),
            input,
            claim,
        }))
    }

    /// Counts the shared call among the day's readings for notes; false
    /// past `derive_calls_per_day`.
    pub(super) async fn derive_shared_take(&self) -> eyre::Result<bool> {
        let cap = self.0.opts.memory.derive_calls_per_day;
        self.0.store.derive_take_call(self.derive_day(), cap).await
    }

    /// Gives back the shared call's count: no model took it.
    pub(super) async fn derive_shared_untake(&self) {
        if let Err(e) = self.0.store.derive_untake_call(self.derive_day()).await {
            tracing::info!(
                error = format!("{e:#}"),
                "couldn't give back a reading for notes' count"
            );
        }
    }

    /// The shared reading wasn't made: the session is due as it was, and
    /// the worker reads it.
    pub(super) async fn derive_unshare(&self, s: SharedRead) {
        if let Err(e) = self
            .0
            .store
            .derive_release(&s.claim, Some(Utc::now()))
            .await
        {
            tracing::info!(session = %s.claim.session, error = format!("{e:#}"), "couldn't let a session's reading for notes go; it is due once its lease is up");
        }
    }

    /// Keeps the notes `model` wrote in the shared call, as the worker
    /// keeps its own; when they can't be read, the worker reads the same
    /// exchanges on its own.
    pub(super) async fn derive_shared_kept(
        &self,
        s: SharedRead,
        out: Option<&derive::Out>,
        model: &str,
    ) {
        let Some(out) = out else {
            tracing::info!(session = %s.claim.session, "the notes read with a session's account weren't JSON; they are read on their own");
            self.derive_unshare(s).await;
            return;
        };
        if let Err(e) = self
            .derive_keep(
                &s.claim,
                s.read,
                s.backlog,
                s.group.as_deref(),
                &s.rows,
                &s.input,
                out,
                model,
            )
            .await
        {
            tracing::info!(session = %s.claim.session, error = format!("{e:#}"), "couldn't keep the notes read with a session's account");
            self.derive_unshare(s).await;
        }
    }

    /// Writes what was noted from `rows` and moves the session past them.
    #[allow(clippy::too_many_arguments)]
    async fn derive_write(
        &self,
        claim: &Claim,
        (from, to): (i32, i32),
        backlog: bool,
        group: Option<&str>,
        by: &str,
        rows: &[Unread],
        applied: &derive::Applied,
    ) -> eyre::Result<()> {
        let ids: Vec<(Uuid, String)> = applied
            .add
            .iter()
            .map(|item| {
                // Its project is its latest source's.
                let project = rows
                    .iter()
                    .filter(|x| item.sources.contains(&x.id))
                    .max_by_key(|x| x.at)
                    .map(|x| x.project.clone())
                    .unwrap_or_default();
                (Uuid::new_v4(), project)
            })
            .collect();
        let wrote = self
            .0
            .store
            .derive_apply(&Apply {
                claim,
                from,
                to,
                backlog,
                group,
                by,
                ids: &ids,
                applied,
            })
            .await?;
        if !wrote {
            tracing::info!(session = %claim.session, "a session was forgotten or read again while its notes were written; they weren't kept");
        } else if !applied.is_empty() {
            self.memory_noted(claim.session);
        }
        Ok(())
    }

    /// After notes changed: the search index has them to write.
    fn memory_noted(&self, _session: Uuid) {
        self.memory_index_poke();
    }

    /// Makes session `id`'s new exchanges due to be read for notes now, as
    /// opening its memory panel or handing its work off does.
    pub async fn derive_now(&self, id: Uuid) -> Result<(), DeskError> {
        if self.0.store.derive_due_now(id).await? {
            self.derive_poke();
        }
        Ok(())
    }

    /// What Desk noted from session `id`, and whether it is noting, in
    /// `group` (its group now; none ungrouped).
    pub(super) async fn session_notes(
        &self,
        id: Uuid,
        group: Option<&str>,
    ) -> Result<Notes, DeskError> {
        let items = self.0.store.session_items(id).await?;
        let queued = self.0.store.derive_queued(id).await?;
        // Said of the group the next reading is stamped with; with nothing
        // waiting, of the one it is in.
        let group = match &queued.next_group {
            Some(next) => next.as_deref(),
            None => group,
        };
        let by = self
            .0
            .derive
            .deriver
            .as_ref()
            .map(|d| d.written_by())
            .unwrap_or_default();
        let mut notes = Notes {
            provider: by.provider,
            fallback_providers: by.fallback,
            decisions: Vec::new(),
            facts: Vec::new(),
            todos: Vec::new(),
            counts: NoteCounts::default(),
            noting: Noting::On,
            why: None,
            reason: None,
            replaced: self
                .0
                .store
                .replaced_items(id, REPLACED_MAX)
                .await?
                .iter()
                .filter_map(|r| {
                    Some(ReplacedView {
                        id: r.id,
                        kind: r.kind,
                        text: r.text.clone(),
                        replaced_at: r.valid_to?,
                    })
                })
                .collect(),
            unread: queued.unread,
        };
        for r in &items {
            let list = match r.kind {
                ItemKind::Decision => &mut notes.decisions,
                ItemKind::Fact => &mut notes.facts,
                ItemKind::Todo => &mut notes.todos,
            };
            list.push(NoteView::from(r));
        }
        notes.counts = NoteCounts {
            decisions: notes.decisions.len(),
            facts: notes.facts.len(),
            todos: notes.todos.len(),
            open_todos: notes
                .todos
                .iter()
                .filter(|t| t.state == ItemState::Open)
                .count(),
            kept: self.0.store.items_kept(id).await?,
        };
        let (noting, why) = self.noting(id, group, queued.failed.is_some()).await?;
        notes.noting = noting;
        notes.reason = why.map(NotingWhy::code);
        notes.why = why.map(|w| w.plain().to_string());
        Ok(notes)
    }

    /// Whether Desk notes decisions from session `id` in `group`, and why
    /// not.
    async fn noting(
        &self,
        id: Uuid,
        group: Option<&str>,
        failed: bool,
    ) -> Result<(Noting, Option<NotingWhy>), DeskError> {
        let m = &self.memory_now();
        if !m.enabled || !m.derive {
            return Ok((Noting::Off, Some(NotingWhy::DeskOff)));
        }
        if self.0.derive.deriver.is_none() {
            return Ok((Noting::Off, Some(NotingWhy::NoModel)));
        }
        let facts = self.0.store.derive_facts(id, group).await?;
        if facts.paused {
            return Ok((Noting::Off, Some(NotingWhy::Paused)));
        }
        if facts.session_off {
            return Ok((Noting::Off, Some(NotingWhy::SessionOff)));
        }
        if !facts.group_on {
            return Ok((Noting::Off, Some(NotingWhy::GroupOff)));
        }
        if self.0.store.derive_calls(self.derive_day()).await? >= i64::from(m.derive_calls_per_day)
        {
            return Ok((Noting::Paused, Some(NotingWhy::DayCap)));
        }
        Ok((Noting::On, failed.then_some(NotingWhy::Failing)))
    }

    /// The person's change to note `id`: confirmed, corrected (a note of
    /// theirs replaces it) or dropped. The note as it now stands; none once
    /// dropped.
    pub async fn change_memory_item(
        &self,
        id: Uuid,
        change: &ItemChange,
    ) -> Result<Option<NoteView>, DeskError> {
        let gone = || DeskError::Conflict("That note was replaced or dropped already.".into());
        let Some(item) = self.0.store.memory_item(id).await? else {
            return Err(DeskError::NotFound);
        };
        let now = match change {
            ItemChange::Confirm => {
                if !self.0.store.confirm_item(id).await? {
                    return Err(gone());
                }
                id
            }
            ItemChange::Correct { text } => {
                let Some(text) = derive::note_text(text) else {
                    return Err(DeskError::Invalid("Say what the note should say.".into()));
                };
                // Kept and searched as everything Desk remembers is: with
                // every secret in it hidden, or not at all.
                let Some(redactor) = &self.0.ledger.redactor else {
                    return Err(DeskError::Invalid(crate::memory::NO_REDACTOR.into()));
                };
                let text = redactor.stored(&text).text.into_owned();
                match self.0.store.correct_item(id, &text).await? {
                    Some(new) => new,
                    None => return Err(gone()),
                }
            }
            ItemChange::Drop => {
                if !self.0.store.drop_item(id).await? {
                    return Err(DeskError::NotFound);
                }
                self.memory_noted(item.session);
                return Ok(None);
            }
        };
        self.memory_noted(item.session);
        Ok(self
            .0
            .store
            .memory_item(now)
            .await?
            .as_ref()
            .map(NoteView::from))
    }

    /// The group session `id`'s notes are read in now: its sidebar group,
    /// none when ungrouped.
    pub(super) fn notes_group(&self, id: Uuid) -> Option<String> {
        self.group_of_member(&super::session_item(id))
            .filter(|g| g != UNGROUPED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::memory::record::Asker;

    fn unread(seq: i32, group: Option<&str>, closed: bool) -> Unread {
        Unread {
            id: Uuid::from_u128(seq as u128),
            seq,
            key: format!("k-{seq}"),
            at: Utc::now(),
            ended: closed.then(Utc::now),
            engine: Engine::Claude,
            group: group.map(str::to_string),
            project: "/dev/acme-web".into(),
            asker: Asker::Person,
            ask: "ask".into(),
            answer: "answer".into(),
            steps: Vec::new(),
            flagged: false,
        }
    }

    #[test]
    fn a_batch_is_closed_exchanges_of_one_group_at_most_twelve() {
        let all: Vec<Unread> = (1..=14).map(|i| unread(i, None, true)).collect();
        let (rows, backlog) = batch(&all);
        assert_eq!((rows.len(), backlog), (12, true));

        let open: Vec<Unread> = vec![unread(1, None, true), unread(2, None, false)];
        let (rows, backlog) = batch(&open);
        assert_eq!(
            (rows.len(), backlog),
            (1, false),
            "up to the one still open"
        );

        let moved = vec![
            unread(1, Some("g-acme"), true),
            unread(2, Some("g-acme"), true),
            unread(3, Some("g-globex"), true),
        ];
        let (rows, backlog) = batch(&moved);
        assert_eq!(rows.len(), 2, "never across groups");
        assert!(backlog, "the next group's are read next");

        assert_eq!(batch(&[unread(1, None, false)]).0.len(), 0);
        assert_eq!(batch(&[]).0.len(), 0);
    }

    #[test]
    fn a_failing_session_is_tried_again_later_each_time_to_an_hour() {
        assert_eq!(retry_after(0), Duration::from_secs(60));
        assert_eq!(retry_after(2), Duration::from_secs(4 * 60));
        assert_eq!(retry_after(40), Duration::from_secs(60 * 60));
    }
}
