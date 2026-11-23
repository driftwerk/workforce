//! Moving and marking many messages at once, matched on the server: in
//! batches, within a time budget, carried on by job id, every batch journaled
//! before it is sent and again once it landed, so a call that ends mid-batch
//! is settled and `email.undo` puts it all back. Nothing is ever deleted: the
//! only expunge is `UID EXPUNGE` of uids already copied elsewhere.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use chrono::Utc;
use mailparse::MailHeaderMap;
use serde_json::{json, Value};
use tokio::time::Instant;
use tracing::warn;

use super::criteria::{quoted, Selection};
use super::error::EmailError;
use super::journal::{Batch, BulkAction, BulkJob, JobState, MailJournal, Origin};
use super::mailbox::{decoded, Mailbox};
use super::session::{Landed, MailSession, Selected};
use super::uids::UidSet;

pub const BATCH: usize = 500;
const SAMPLE: usize = 10;
const IDS_PER_SEARCH: usize = 20;
/// The most messages read back to find one batch's copies: a batch makes
/// at most [`BATCH`], and they are the first to arrive after it was sent.
const COPIES_READ: usize = 2 * BATCH;
const BUDGET: Duration = Duration::from_secs(45);

/// Why a job's step failed. `Lost`: the server stopped answering, so what
/// was sent may or may not have been done, and the next call settles it and
/// carries on. `Refused`: the server, the journal or the job itself said
/// no, which stops the job for good. Either holds the server's own words
/// fenced as outside content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    Lost(String),
    Refused(String),
}

impl From<EmailError> for Fault {
    fn from(e: EmailError) -> Fault {
        match e.is_transient() {
            true => Fault::Lost(e.fenced()),
            false => Fault::Refused(e.fenced()),
        }
    }
}

impl From<String> for Fault {
    fn from(why: String) -> Fault {
        Fault::Refused(why)
    }
}

impl From<Fault> for String {
    fn from(fault: Fault) -> String {
        match fault {
            Fault::Lost(why) | Fault::Refused(why) => why,
        }
    }
}

/// When a call stops starting new batches.
#[derive(Debug, Clone, Copy)]
pub struct Budget(Instant);

impl Budget {
    /// The bulk budget, or less when one command may take less.
    pub fn for_call(command_timeout: Duration) -> Budget {
        Budget(Instant::now() + BUDGET.min(command_timeout.max(Duration::from_secs(5))))
    }

    #[cfg(test)]
    pub fn of(d: Duration) -> Budget {
        Budget(Instant::now() + d)
    }

    fn spent(&self) -> bool {
        Instant::now() >= self.0
    }
}

/// What a new job is asked to do, and who asked.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    pub owner: &'a str,
    pub account: &'a str,
    pub tool: &'a str,
    pub origin: Origin,
    pub selection: &'a Selection,
    /// The source folder's UIDVALIDITY its uids were read under, when said.
    pub validity: Option<u32>,
}

fn renumbered(folder: &str) -> String {
    format!(
        "{}'s message numbers changed since they were read (the server rebuilt the folder)",
        decoded(folder)
    )
}

fn unnumbered(folder: &str) -> String {
    format!(
        "{}'s server doesn't say how it numbers its messages (no UIDVALIDITY), so messages read earlier can't be told from others",
        decoded(folder)
    )
}

/// Why `job` can never be undone, when it can't: without its source's
/// numbering, what it took can't be told from what was there.
fn past_undoing(job: &BulkJob) -> Option<String> {
    job.validity
        .is_none()
        .then(|| unnumbered(job.action.source()))
}

/// What undoing `job` would do, as its notes say it.
fn undoing(job: &BulkJob) -> String {
    match past_undoing(job) {
        None => "email.undo puts back what it did".to_string(),
        Some(why) => format!("what it did can't be undone, as {why}"),
    }
}

/// Why `folder`, numbered `now`, isn't the folder messages were read from
/// under `expected`, when it isn't; nothing to compare when none was said.
fn other_numbering(folder: &str, expected: Option<u32>, now: Option<u32>) -> Option<String> {
    match (expected, now) {
        (Some(_), None) => Some(unnumbered(folder)),
        (Some(was), Some(now)) if was != now => Some(renumbered(folder)),
        _ => None,
    }
}

fn nothing_more(why: &str) -> String {
    format!("{why}, so nothing more was done. Read the messages again and retry.")
}

/// Opens `folder`, to change it or only to look, where its numbering is
/// the one `expected` names.
pub(super) async fn open_folder(
    session: &mut (impl MailSession + ?Sized),
    folder: &str,
    expected: Option<u32>,
    change: bool,
) -> Result<Selected, Fault> {
    let selected = match change {
        true => session.select(folder).await,
        false => session.examine(folder).await,
    }?;
    match other_numbering(folder, expected, selected.validity) {
        Some(why) => Err(nothing_more(&why).into()),
        None => Ok(selected),
    }
}

/// Which of `uids` the selected folder still holds.
pub(super) async fn present(
    session: &mut (impl MailSession + ?Sized),
    uids: &UidSet,
) -> Result<UidSet, Fault> {
    match uids.is_empty() {
        true => Ok(UidSet::default()),
        false => Ok(session.search(&format!("UID {uids}")).await?),
    }
}

/// The uids `selection` means in the selected folder: only messages it
/// holds.
async fn resolve(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    selection: &Selection,
) -> Result<UidSet, Fault> {
    match selection {
        Selection::Uids(uids) => present(session, uids).await,
        Selection::Search(criteria) => {
            let query = criteria.to_imap(mailbox.caps.gmail)?;
            Ok(session.search(&query).await?)
        }
    }
}

/// What a dry run reports: how many messages match, and the newest few.
pub async fn plan(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    folder: &str,
    selection: &Selection,
    validity: Option<u32>,
) -> Result<Value, Fault> {
    let selected = open_folder(session, folder, validity, false).await?;
    let uids = resolve(session, mailbox, selection).await?;
    let newest: UidSet = uids.iter().rev().take(SAMPLE).collect();
    let sample = match newest.is_empty() {
        true => Vec::new(),
        false => session.summaries(&newest).await?,
    };
    let mut sample: Vec<Value> = sample
        .into_iter()
        .map(|s| json!({"uid": s.uid, "from": s.from, "subject": s.subject, "date": s.date}))
        .collect();
    sample.sort_by_key(|s| std::cmp::Reverse(s["uid"].as_u64()));
    Ok(json!({
        "dry_run": true,
        "folder": decoded(folder),
        "validity": selected.validity,
        "count": uids.len(),
        "sample": sample
    }))
}

/// A new job for `action` over what was asked for in its source folder.
pub async fn begin(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    action: BulkAction,
    ask: Ask<'_>,
) -> Result<BulkJob, Fault> {
    if ask.owner.is_empty() {
        return Err(
            "A mail job is someone's own, and this call acts for no one."
                .to_string()
                .into(),
        );
    }
    let selected = open_folder(session, action.source(), ask.validity, true).await?;
    let pending = resolve(session, mailbox, ask.selection).await?;
    Ok(BulkJob {
        id: uuid::Uuid::new_v4().to_string(),
        owner: ask.owner.to_string(),
        account: ask.account.to_string(),
        tool: ask.tool.to_string(),
        origin: ask.origin,
        asked: ask.selection.describe(),
        action,
        validity: selected.validity,
        pending,
        in_flight: None,
        done: Vec::new(),
        state: JobState::Running,
        stopped: None,
        unfound: 0,
        created: Utc::now(),
    })
}

async fn keep(journal: Option<&dyn MailJournal>, job: &BulkJob) -> Result<(), Fault> {
    match journal {
        Some(journal) => journal
            .save(job)
            .await
            .map_err(|e| Fault::Refused(format!("Couldn't record the job, so it stopped: {e}"))),
        None => Ok(()),
    }
}

/// Ends `job` in `state` for the reason `why`.
fn end(job: &mut BulkJob, state: JobState, why: String) {
    job.state = state;
    job.stopped = Some(why);
}

fn message_id_in(block: &str) -> Option<String> {
    let (headers, _) = mailparse::parse_headers(block.as_bytes()).ok()?;
    headers
        .get_first_value("Message-ID")
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Which of `uids` the selected folder holds, and the Message-ID of each
/// that has one: never a message it wasn't asked about.
async fn message_ids(
    session: &mut (impl MailSession + ?Sized),
    uids: &UidSet,
) -> Result<(UidSet, BTreeMap<u32, String>), Fault> {
    let headers = match uids.is_empty() {
        true => Vec::new(),
        false => session.headers(uids, &["MESSAGE-ID"]).await?,
    };
    let asked: Vec<(u32, String)> = headers
        .into_iter()
        .filter(|(uid, _)| uids.contains(*uid))
        .collect();
    let held = asked.iter().map(|(uid, _)| *uid).collect();
    let ids = asked
        .into_iter()
        .filter_map(|(uid, block)| Some((uid, message_id_in(&block)?)))
        .collect();
    Ok((held, ids))
}

/// Where `batch`'s messages were copied in the selected folder, numbered
/// `validity`, by source uid: found by Message-ID among what arrived there
/// since the batch was sent, each source message taking the newest copy of
/// its id that no other took. So a copy that was there before is never
/// one, and messages sharing an id find a copy each; where a label move
/// leaves the batch's own among older mail, the newest are its own. Nothing is found once
/// the folder was renumbered, or where the batch never knew where its
/// copies would land.
async fn copies_of(
    session: &mut (impl MailSession + ?Sized),
    batch: &Batch,
    validity: Option<u32>,
) -> Result<BTreeMap<u32, u32>, Fault> {
    let next = match batch.dst_next {
        Some(next) if validity.is_some() && validity == batch.dst_validity => next,
        _ => return Ok(BTreeMap::new()),
    };
    let wanted: BTreeSet<&str> = batch.message_ids.values().map(String::as_str).collect();
    let keys: Vec<String> = wanted
        .iter()
        .filter_map(|id| quoted(id).ok())
        .map(|q| format!("HEADER Message-ID {q}"))
        .collect();
    let mut hits = UidSet::default();
    for chunk in keys.chunks(IDS_PER_SEARCH) {
        let either = chunk
            .iter()
            .skip(1)
            .fold(chunk[0].clone(), |acc, key| format!("OR {acc} {key}"));
        let arrived = format!("UID {next}:{} {either}", u32::MAX);
        hits = hits.union(&session.search(&arrived).await?);
    }
    let read: UidSet = hits.iter().rev().take(COPIES_READ).collect();
    let (_, found) = message_ids(session, &read).await?;
    let mut copies: BTreeMap<String, VecDeque<u32>> = BTreeMap::new();
    for (uid, id) in found.into_iter().rev().filter(|(uid, _)| *uid >= next) {
        copies.entry(id).or_default().push_back(uid);
    }
    Ok(batch
        .message_ids
        .iter()
        .filter_map(|(src, id)| Some((*src, copies.get_mut(id)?.pop_front()?)))
        .collect())
}

/// Moves `uids` from the selected folder to `to` with only these uids ever
/// leaving it.
async fn relocate(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    uids: &UidSet,
    to: &str,
) -> Result<Option<Landed>, Fault> {
    match (mailbox.caps.moves, mailbox.caps.uidplus) {
        (true, _) => Ok(session.move_uids(uids, to).await?),
        (false, true) => {
            let landed = session.copy_uids(uids, to).await?;
            session.store(uids, "+FLAGS.SILENT (\\Deleted)").await?;
            session.expunge_uids(uids).await?;
            Ok(landed)
        }
        (false, false) => Err(
            "This server can't move just these messages safely (it has neither MOVE nor UIDPLUS), so nothing was moved."
                .to_string()
                .into(),
        ),
    }
}

/// The batch `uids` will be, read before anything is sent: for a move, the
/// messages there, their Message-IDs and where in the destination their
/// copies will start; for a mark, the messages it changes.
async fn prepare(
    session: &mut (impl MailSession + ?Sized),
    action: &BulkAction,
    uids: &UidSet,
) -> Result<Batch, Fault> {
    Ok(match action {
        BulkAction::Move { to, label, .. } => {
            let (src, message_ids) = message_ids(session, uids).await?;
            let dest = session.status(to).await?;
            Batch {
                src,
                dst: None,
                dst_validity: dest.validity,
                // A label leaving a message leaves it where it already was
                // in `to`, at whatever uid it had there.
                dst_next: match label {
                    Some(_) => Some(1),
                    None => dest.next,
                },
                message_ids,
            }
        }
        BulkAction::Mark { flag, on, .. } => Batch {
            src: session
                .search(&format!("UID {uids} {}", flag.changes(*on)))
                .await?,
            dst: None,
            dst_validity: None,
            dst_next: None,
            message_ids: BTreeMap::new(),
        },
    })
}

/// Sends `batch`, and what it then is: for a move the server reported
/// where, the uids that left and where they landed.
async fn apply(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    action: &BulkAction,
    batch: Batch,
) -> Result<Batch, Fault> {
    if batch.src.is_empty() {
        return Ok(batch);
    }
    match action {
        BulkAction::Move { to, .. } => {
            Ok(match relocate(session, mailbox, &batch.src, to).await? {
                Some(landed) => Batch {
                    src: landed.source.into_iter().collect(),
                    dst: Some(landed.dest.into_iter().collect()),
                    dst_next: batch
                        .dst_next
                        .filter(|_| batch.dst_validity == Some(landed.validity)),
                    dst_validity: Some(landed.validity),
                    ..batch
                },
                None => batch,
            })
        }
        BulkAction::Mark { flag, on, .. } => {
            let sign = if *on { '+' } else { '-' };
            session
                .store(&batch.src, &format!("{sign}FLAGS.SILENT ({})", flag.imap()))
                .await?;
            Ok(batch)
        }
    }
}

/// One batch of `job`, the `uids` its pending set starts with and `rest`
/// what follows them, journaled before it is sent and once it landed.
async fn step(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    job: &mut BulkJob,
    (uids, rest): (UidSet, UidSet),
    journal: Option<&dyn MailJournal>,
) -> Result<(), Fault> {
    let batch = prepare(session, &job.action, &uids).await?;
    job.in_flight = Some(batch.clone());
    keep(journal, job).await?;
    let done = apply(session, mailbox, &job.action, batch).await?;
    job.pending = rest;
    job.done.push(done);
    job.in_flight = None;
    keep(journal, job).await
}

/// What became of the batch `job` sent last and never heard back on, now
/// counted done: a mark either changed its messages or undo leaves them as
/// they were; a move's messages that left the source landed (found by
/// Message-ID among what arrived since), and any a copy left behind are
/// taken out after it, never one whose copy wasn't found.
async fn settle(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    job: &mut BulkJob,
    journal: Option<&dyn MailJournal>,
) -> Result<(), Fault> {
    let Some(batch) = job.in_flight.clone() else {
        return Ok(());
    };
    let settled = match &job.action {
        BulkAction::Mark { .. } => batch,
        BulkAction::Move { from, to, .. } => {
            let dest = session.select(to).await?;
            let copied = copies_of(session, &batch, dest.validity).await?;
            let copied_src: UidSet = copied.keys().copied().collect();
            let source = session.select(from).await?;
            let numbered = source.validity.is_some() && source.validity == job.validity;
            let still = match numbered {
                true => present(session, &batch.src).await?,
                false => batch.src.without(&copied_src),
            };
            let left_behind = still.intersection(&copied_src);
            let still = match numbered && !mailbox.caps.moves && mailbox.caps.uidplus {
                true if !left_behind.is_empty() => {
                    session
                        .store(&left_behind, "+FLAGS.SILENT (\\Deleted)")
                        .await?;
                    session.expunge_uids(&left_behind).await?;
                    still.without(&left_behind)
                }
                _ => still,
            };
            let gone = batch.src.without(&still);
            Batch {
                dst: Some(
                    gone.iter()
                        .filter_map(|u| copied.get(&u).copied())
                        .collect(),
                ),
                dst_next: batch
                    .dst_next
                    .filter(|_| dest.validity == batch.dst_validity),
                dst_validity: dest.validity,
                message_ids: batch
                    .message_ids
                    .into_iter()
                    .filter(|(uid, _)| gone.contains(*uid))
                    .collect(),
                src: gone,
            }
        }
    };
    job.pending = job.pending.without(&settled.src);
    if !settled.src.is_empty() {
        job.done.push(settled);
    }
    job.in_flight = None;
    keep(journal, job).await
}

fn stopped_note(job: &BulkJob) -> String {
    format!(
        "Job {} stopped; {}. {}",
        job.id,
        undoing(job),
        job.stopped.as_deref().unwrap_or("It stopped.")
    )
}

/// Carries `job` on, batch by batch, until it is done or the budget is
/// spent. A connection that drops leaves it running, its batch in flight,
/// for the next call to settle and carry on.
pub async fn run(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    job: &mut BulkJob,
    budget: Budget,
    journal: Option<&dyn MailJournal>,
) -> Result<(), Fault> {
    if job.state != JobState::Running {
        return Ok(());
    }
    let source = job.action.source().to_string();
    let now = session.select(&source).await?.validity;
    if let Some(why) = other_numbering(&source, job.validity, now) {
        end(job, JobState::Stopped, nothing_more(&why));
        return keep(journal, job).await;
    }
    keep(journal, job).await?;
    if job.in_flight.is_some() {
        settle(session, mailbox, job, journal).await?;
        session.select(&source).await?;
    }
    while !job.pending.is_empty() && !budget.spent() {
        let next = job.pending.split_first(BATCH);
        match step(session, mailbox, job, next, journal).await {
            Ok(()) => {}
            Err(Fault::Lost(why)) => {
                warn!(job = %job.id, error = %why, "a mail job's connection dropped; its next call carries it on");
                return Ok(());
            }
            Err(Fault::Refused(why)) => {
                end(job, JobState::Stopped, why);
                let _ = settle(session, mailbox, job, journal).await;
                keep(journal, job).await?;
                return Err(Fault::Refused(stopped_note(job)));
            }
        }
    }
    if job.pending.is_empty() {
        job.state = JobState::Done;
        keep(journal, job).await?;
    }
    Ok(())
}

/// Where one moved batch is now: its uids in the selected folder, numbered
/// `validity`, that are still there: by COPYUID when the server gave it in
/// that numbering, else its copies found by Message-ID.
async fn landed_now(
    session: &mut (impl MailSession + ?Sized),
    batch: &Batch,
    validity: Option<u32>,
) -> Result<UidSet, Fault> {
    match &batch.dst {
        Some(dst) if validity.is_some() && batch.dst_validity == validity => {
            present(session, dst).await
        }
        _ => Ok(copies_of(session, batch, validity)
            .await?
            .into_values()
            .collect()),
    }
}

/// Puts one batch back; how many of its messages were no longer there.
async fn undo_batch(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    action: &BulkAction,
    batch: &Batch,
) -> Result<usize, Fault> {
    let back = match action {
        BulkAction::Move { from, to, label } => {
            let selected = session.select(to).await?;
            let back = landed_now(session, batch, selected.validity).await?;
            match (label, back.is_empty()) {
                (_, true) => {}
                (Some(label), false) => {
                    session
                        .store(&back, &format!("+X-GM-LABELS.SILENT ({label})"))
                        .await?
                }
                (None, false) => {
                    relocate(session, mailbox, &back, from).await?;
                }
            }
            back
        }
        BulkAction::Mark { flag, on, .. } => {
            let back = present(session, &batch.src).await?;
            if !back.is_empty() {
                let sign = if *on { '-' } else { '+' };
                session
                    .store(&back, &format!("{sign}FLAGS.SILENT ({})", flag.imap()))
                    .await?;
            }
            back
        }
    };
    Ok(batch.src.len().saturating_sub(back.len()))
}

/// Puts back what `job` did, newest batch first, until all of it is back or
/// the budget is spent. A job still running stops taking new batches. A job
/// whose source can't be told to be numbered as it was can't be undone, and
/// is marked so.
pub async fn undo(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    job: &mut BulkJob,
    budget: Budget,
    journal: Option<&dyn MailJournal>,
) -> Result<(), Fault> {
    if JobState::SETTLED.contains(&job.state) {
        return Ok(());
    }
    settle(session, mailbox, job, journal).await?;
    let source = job.action.source().to_string();
    if let Some(why) = past_undoing(job) {
        end(job, JobState::Irreversible, why);
        return keep(journal, job).await;
    }
    if job.state != JobState::Undoing {
        job.state = JobState::Undoing;
        job.pending = UidSet::default();
        keep(journal, job).await?;
    }
    if matches!(job.action, BulkAction::Mark { .. }) {
        let now = session.select(&source).await?.validity;
        if let Some(why) = other_numbering(&source, job.validity, now) {
            end(job, JobState::Irreversible, why);
            return keep(journal, job).await;
        }
    }
    while !budget.spent() {
        let Some(batch) = job.done.last().cloned() else {
            break;
        };
        job.unfound += undo_batch(session, mailbox, &job.action, &batch).await?;
        job.done.pop();
        keep(journal, job).await?;
    }
    if job.done.is_empty() {
        job.state = JobState::Undone;
        keep(journal, job).await?;
    }
    Ok(())
}

/// A job as a call reports it. Without a journal it can't be carried on
/// or undone, so what is left is said as uids to call again with.
pub fn report(job: &BulkJob, journaled: bool) -> Value {
    let why = job.stopped.as_deref().unwrap_or_default();
    let note = match (job.state, journaled) {
        (JobState::Running, false) => format!(
            "Only {} of {} messages were done in this call. This hub keeps no job journal, so it can't carry the job on or undo it: call again with uids = remaining_uids for the rest.",
            job.handled(),
            job.handled() + job.pending.len()
        ),
        (JobState::Running, true) => format!(
            "Not finished: call again with job = \"{}\" to carry on.",
            job.id
        ),
        (JobState::Stopped, false) => format!(
            "Stopped after {} message(s): {why} This hub keeps no job journal, so what was done can't be undone with email.undo.",
            job.handled()
        ),
        (JobState::Stopped, true) => stopped_note(job),
        (JobState::Done, false) => {
            "Done. This hub keeps no job journal, so it can't be undone with email.undo."
                .to_string()
        }
        (JobState::Done, true) => format!("Done; {}.", undoing(job)),
        (JobState::Undoing, _) => format!(
            "Still putting messages back: call email.undo with job = \"{}\" to carry on.",
            job.id
        ),
        (JobState::Undone, _) if job.unfound > 0 => format!(
            "Put back, except {} message(s) that were no longer where they had been moved to.",
            job.unfound
        ),
        (JobState::Undone, _) => "Put back.".to_string(),
        (JobState::Irreversible, _) => format!(
            "Job {} can't be undone, so what it did stays as it is: {why}.",
            job.id
        ),
    };
    let mut out = json!({
        "state": job.state,
        "asked": job.asked,
        "handled": job.handled(),
        "remaining": job.pending.len(),
        "note": note,
    });
    if let Some(obj) = out.as_object_mut() {
        match journaled {
            true => obj.insert("job".to_string(), json!(job.id)),
            false if !job.pending.is_empty() => obj.insert(
                "remaining_uids".to_string(),
                json!(job.pending.iter().collect::<Vec<u32>>()),
            ),
            false => None,
        };
    }
    out
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::{BTreeMap, HashMap};

    use async_trait::async_trait;

    use super::super::error::EmailError;
    use super::super::session::{FolderStatus, Listed, Selected, Summary};
    use super::*;

    #[derive(Debug, Clone, Default)]
    pub struct Message {
        pub from: String,
        pub message_id: String,
        pub flags: Vec<String>,
    }

    #[derive(Debug, Clone, Default)]
    pub struct Folder {
        pub validity: Option<u32>,
        pub next: u32,
        pub messages: BTreeMap<u32, Message>,
    }

    /// An IMAP server in memory, recording every command it was sent.
    #[derive(Debug, Default)]
    pub struct FakeServer {
        pub caps: Vec<String>,
        pub folders: HashMap<String, Folder>,
        pub selected: Option<String>,
        pub commands: Vec<String>,
        pub step: Duration,
        pub copyuid: bool,
        /// A command (by its start) that fails, once: refused, or with the
        /// connection lost when `lost`.
        pub fail: Option<String>,
        pub lost: bool,
    }

    impl FakeServer {
        pub fn new(caps: &[&str], folders: &[&str]) -> FakeServer {
            FakeServer {
                caps: caps.iter().map(|c| c.to_string()).collect(),
                folders: folders
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let folder = Folder {
                            validity: Some(100 + i as u32),
                            next: 1,
                            ..Folder::default()
                        };
                        (f.to_string(), folder)
                    })
                    .collect(),
                copyuid: true,
                ..FakeServer::default()
            }
        }

        pub fn deliver(&mut self, folder: &str, from: &str, n: usize) {
            for _ in 0..n {
                let uid = self.folders[folder].next;
                self.deliver_as(folder, from, &format!("<{uid}.{from}@test>"));
            }
        }

        /// One message with `message_id`, as a copy of another may carry.
        pub fn deliver_as(&mut self, folder: &str, from: &str, message_id: &str) {
            let f = self.folders.get_mut(folder).unwrap();
            let uid = f.next;
            f.next += 1;
            f.messages.insert(
                uid,
                Message {
                    from: from.to_string(),
                    message_id: message_id.to_string(),
                    flags: Vec::new(),
                },
            );
        }

        pub fn count(&self, folder: &str) -> usize {
            self.folders[folder].messages.len()
        }

        pub fn mailbox(&self) -> Mailbox {
            let listed: Vec<Listed> = self
                .folders
                .keys()
                .map(|name| Listed {
                    name: name.clone(),
                    delimiter: Some("/".to_string()),
                    attributes: Vec::new(),
                })
                .collect();
            Mailbox::of(self.caps.iter().map(String::as_str), &listed)
        }

        fn here(&mut self) -> &mut Folder {
            let name = self.selected.clone().expect("a folder is selected");
            self.folders.get_mut(&name).unwrap()
        }

        fn take(&mut self, uids: &UidSet) -> Vec<(u32, Message)> {
            let here = self.here();
            uids.iter()
                .filter_map(|u| here.messages.get(&u).cloned().map(|m| (u, m)))
                .collect()
        }

        fn put(&mut self, to: &str, messages: Vec<(u32, Message)>) -> Option<Landed> {
            let copyuid = self.copyuid;
            let dest = self.folders.get_mut(to)?;
            let (source, landed): (Vec<u32>, Vec<u32>) = messages
                .into_iter()
                .map(|(src, mut m)| {
                    let uid = dest.next;
                    dest.next += 1;
                    m.flags.retain(|f| f != "\\Deleted");
                    dest.messages.insert(uid, m);
                    (src, uid)
                })
                .unzip();
            dest.validity.filter(|_| copyuid).map(|validity| Landed {
                validity,
                source,
                dest: landed,
            })
        }

        async fn tick(&mut self, command: String) -> Result<(), EmailError> {
            tokio::time::sleep(self.step).await;
            let failing = self
                .fail
                .as_deref()
                .is_some_and(|start| command.starts_with(start));
            self.commands.push(command);
            match failing {
                true if self.lost => {
                    self.fail = None;
                    Err(EmailError::Disconnected {
                        operation: "a command".to_string(),
                        detail: "the server went away".to_string(),
                    })
                }
                true => {
                    self.fail = None;
                    Err(EmailError::Imap("NO the server refused".to_string()))
                }
                false => Ok(()),
            }
        }
    }

    fn quoted_values(query: &str, key: &str) -> Vec<String> {
        query
            .split(key)
            .skip(1)
            .filter_map(|rest| rest.trim_start().strip_prefix('"')?.split('"').next())
            .map(str::to_string)
            .collect()
    }

    #[async_trait]
    impl MailSession for FakeServer {
        async fn capabilities(&mut self) -> Result<Vec<String>, EmailError> {
            Ok(self.caps.clone())
        }

        async fn list(&mut self) -> Result<Vec<Listed>, EmailError> {
            Ok(Vec::new())
        }

        async fn select(&mut self, folder: &str) -> Result<Selected, EmailError> {
            self.tick(format!("SELECT {folder}")).await?;
            let f = self
                .folders
                .get(folder)
                .ok_or_else(|| EmailError::Imap(format!("no folder {folder}")))?;
            let selected = Selected {
                validity: f.validity,
                exists: f.messages.len() as u32,
            };
            self.selected = Some(folder.to_string());
            Ok(selected)
        }

        async fn examine(&mut self, folder: &str) -> Result<Selected, EmailError> {
            self.select(folder).await
        }

        async fn search(&mut self, query: &str) -> Result<UidSet, EmailError> {
            self.tick(format!("UID SEARCH {query}")).await?;
            let here = self.here().clone();
            let within = query
                .strip_prefix("UID ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|set| UidSet::parse(set).unwrap());
            let froms = quoted_values(query, "FROM ");
            let ids = quoted_values(query, "HEADER Message-ID ");
            let flag_test = |m: &Message| {
                let has = |f: &str| m.flags.iter().any(|x| x == f);
                match query.split_whitespace().last() {
                    Some("UNSEEN") => !has("\\Seen"),
                    Some("SEEN") => has("\\Seen"),
                    Some("UNFLAGGED") => !has("\\Flagged"),
                    Some("FLAGGED") => has("\\Flagged"),
                    _ => true,
                }
            };
            Ok(here
                .messages
                .iter()
                .filter(|(uid, m)| {
                    within.as_ref().is_none_or(|w| w.contains(**uid))
                        && froms.iter().all(|f| m.from.contains(f.as_str()))
                        && (ids.is_empty() || ids.contains(&m.message_id))
                        && flag_test(m)
                })
                .map(|(uid, _)| *uid)
                .collect())
        }

        async fn move_uids(
            &mut self,
            uids: &UidSet,
            to: &str,
        ) -> Result<Option<Landed>, EmailError> {
            self.tick(format!("UID MOVE {uids} {to}")).await?;
            let messages = self.take(uids);
            let here = self.here();
            messages.iter().for_each(|(u, _)| {
                here.messages.remove(u);
            });
            Ok(self.put(to, messages))
        }

        async fn copy_uids(
            &mut self,
            uids: &UidSet,
            to: &str,
        ) -> Result<Option<Landed>, EmailError> {
            self.tick(format!("UID COPY {uids} {to}")).await?;
            let messages = self.take(uids);
            Ok(self.put(to, messages))
        }

        async fn store(&mut self, uids: &UidSet, change: &str) -> Result<(), EmailError> {
            self.tick(format!("UID STORE {uids} {change}")).await?;
            if let Some(labels) = change.strip_prefix("+X-GM-LABELS.SILENT (") {
                let label = labels.trim_end_matches(')').to_string();
                let into = if label == "\\Inbox" {
                    "INBOX".to_string()
                } else {
                    label
                };
                let messages = self.take(uids);
                self.put(&into, messages);
                return Ok(());
            }
            let (add, flag) = match change.split_once("FLAGS.SILENT (") {
                Some((sign, flag)) => (sign == "+", flag.trim_end_matches(')').to_string()),
                None => panic!("unexpected store {change}"),
            };
            let here = self.here();
            for uid in uids.iter() {
                if let Some(m) = here.messages.get_mut(&uid) {
                    m.flags.retain(|f| f != &flag);
                    if add {
                        m.flags.push(flag.clone());
                    }
                }
            }
            Ok(())
        }

        async fn expunge_uids(&mut self, uids: &UidSet) -> Result<(), EmailError> {
            self.tick(format!("UID EXPUNGE {uids}")).await?;
            let here = self.here();
            here.messages.retain(|uid, m| {
                !(uids.contains(*uid) && m.flags.iter().any(|f| f == "\\Deleted"))
            });
            Ok(())
        }

        async fn headers(
            &mut self,
            uids: &UidSet,
            _fields: &[&str],
        ) -> Result<Vec<(u32, String)>, EmailError> {
            self.tick(format!("UID FETCH {uids} HEADER")).await?;
            Ok(self
                .take(uids)
                .into_iter()
                .map(|(u, m)| (u, format!("Message-ID: {}\r\n\r\n", m.message_id)))
                .collect())
        }

        async fn summaries(&mut self, uids: &UidSet) -> Result<Vec<Summary>, EmailError> {
            Ok(self
                .take(uids)
                .into_iter()
                .map(|(uid, m)| Summary {
                    uid,
                    from: m.from,
                    subject: format!("Message {uid}"),
                    date: String::new(),
                })
                .collect())
        }

        async fn status(&mut self, folder: &str) -> Result<FolderStatus, EmailError> {
            self.tick(format!("STATUS {folder}")).await?;
            let f = self
                .folders
                .get(folder)
                .ok_or_else(|| EmailError::Imap(format!("no folder {folder}")))?;
            Ok(FolderStatus {
                messages: f.messages.len() as u32,
                validity: f.validity,
                next: Some(f.next),
            })
        }

        async fn create(&mut self, folder: &str) -> Result<(), EmailError> {
            self.tick(format!("CREATE {folder}")).await?;
            let made = Folder {
                validity: Some(200 + self.folders.len() as u32),
                next: 1,
                ..Folder::default()
            };
            self.folders.insert(folder.to_string(), made);
            Ok(())
        }

        async fn rename(&mut self, from: &str, to: &str) -> Result<(), EmailError> {
            self.tick(format!("RENAME {from} {to}")).await?;
            let f = self.folders.remove(from).unwrap();
            self.folders.insert(to.to_string(), f);
            Ok(())
        }

        async fn delete(&mut self, folder: &str) -> Result<(), EmailError> {
            self.tick(format!("DELETE {folder}")).await?;
            self.folders.remove(folder);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::criteria::Criteria;
    use super::super::journal::memory::MemoryJournal;
    use super::super::journal::MarkFlag;
    use super::fake::FakeServer;
    use super::*;

    fn from(sender: &str) -> Selection {
        Selection::Search(Criteria {
            from: Some(sender.to_string()),
            ..Criteria::default()
        })
    }

    fn move_to(to: &str) -> BulkAction {
        BulkAction::Move {
            from: "INBOX".to_string(),
            to: to.to_string(),
            label: None,
        }
    }

    fn ask(selection: &Selection) -> Ask<'_> {
        Ask {
            owner: "kat",
            account: "home",
            tool: "email.move",
            origin: Origin::Person,
            selection,
            validity: None,
        }
    }

    async fn job(server: &mut FakeServer, action: BulkAction, selection: Selection) -> BulkJob {
        let mailbox = server.mailbox();
        begin(server, &mailbox, action, ask(&selection))
            .await
            .unwrap()
    }

    async fn moved_all(
        server: &mut FakeServer,
        job: &mut BulkJob,
        journal: Option<&dyn MailJournal>,
    ) {
        let mailbox = server.mailbox();
        run(
            server,
            &mailbox,
            job,
            Budget::of(Duration::from_secs(60)),
            journal,
        )
        .await
        .unwrap();
    }

    async fn undone(server: &mut FakeServer, job: &mut BulkJob) {
        let mailbox = server.mailbox();
        undo(
            server,
            &mailbox,
            job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_dry_run_counts_and_samples_and_changes_nothing() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 25);
        server.deliver("INBOX", "friend@x.com", 3);
        let mailbox = server.mailbox();
        let out = plan(&mut server, &mailbox, "INBOX", &from("news@shop.com"), None)
            .await
            .unwrap();
        assert_eq!(out["count"], 25);
        assert_eq!(out["validity"], 100);
        let sample = out["sample"].as_array().unwrap();
        assert_eq!(sample.len(), 10);
        assert_eq!(sample[0]["uid"], 25, "newest first");
        assert_eq!(server.count("INBOX"), 28);
        assert!(server
            .commands
            .iter()
            .all(|c| !c.contains("MOVE") && !c.contains("STORE")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_move_runs_in_batches_until_its_time_is_spent_then_carries_on() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.step = Duration::from_secs(1);
        server.deliver("INBOX", "news@shop.com", 2_000);
        server.deliver("INBOX", "friend@x.com", 5);
        let journal = MemoryJournal::default();
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(2)),
            Some(&journal),
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Running);
        assert!(job.handled() > 0 && job.handled() < 2_000);
        assert_eq!(job.handled() + job.pending.len(), 2_000);

        let mut resumed = journal.get("kat", &job.id).await.unwrap().unwrap();
        assert_eq!(resumed, job, "every batch is journaled");
        run(
            &mut server,
            &mailbox,
            &mut resumed,
            Budget::of(Duration::from_secs(60)),
            Some(&journal),
        )
        .await
        .unwrap();
        assert_eq!(resumed.state, JobState::Done);
        assert_eq!(server.count("News"), 2_000);
        assert_eq!(server.count("INBOX"), 5);
        assert!(resumed
            .done
            .iter()
            .all(|b| b.src.len() <= BATCH && b.dst.is_some()));
    }

    #[tokio::test(start_paused = true)]
    async fn without_move_only_the_copied_uids_are_expunged() {
        let mut server = FakeServer::new(&["UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        server.deliver("INBOX", "friend@x.com", 2);
        let mailbox = server.mailbox();
        server.select("INBOX").await.unwrap();
        server
            .store(&std::iter::once(4).collect(), "+FLAGS.SILENT (\\Deleted)")
            .await
            .unwrap();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(server.count("News"), 3);
        assert_eq!(
            server.count("INBOX"),
            2,
            "a message already marked deleted stays"
        );
        assert!(server.commands.iter().any(|c| c == "UID EXPUNGE 1:3"));
        assert!(server.commands.iter().all(|c| c != "EXPUNGE"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_cant_move_just_some_messages_is_refused() {
        let mut server = FakeServer::new(&[], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        let err = run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap_err();
        let err = String::from(err);
        assert!(err.contains("can't move just these messages safely"));
        assert!(err.contains(&job.id), "the job is named");
        assert!(
            !err.contains("<<<EXTERNAL_UNTRUSTED_CONTENT_"),
            "Desk's own words aren't fenced as the server's"
        );
        assert_eq!(job.state, JobState::Stopped);
        assert_eq!(job.in_flight, None);
        assert!(job.done.is_empty());
        assert_eq!(server.count("INBOX"), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebuilt_mailbox_stops_the_job() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        server.folders.get_mut("INBOX").unwrap().validity = Some(999);
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Stopped);
        assert!(job.stopped.as_deref().unwrap().contains("rebuilt"));
        assert_eq!(server.count("INBOX"), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn undo_puts_moved_messages_back() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 1_200);
        let mailbox = server.mailbox();
        let journal = MemoryJournal::default();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            Some(&journal),
        )
        .await
        .unwrap();
        assert_eq!(server.count("INBOX"), 0);
        undo(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            Some(&journal),
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Undone);
        assert_eq!(server.count("INBOX"), 1_200);
        assert_eq!(server.count("News"), 0);
        assert_eq!(report(&job, true)["note"], "Put back.");
    }

    #[tokio::test(start_paused = true)]
    async fn without_copyuid_moved_messages_are_found_again_by_message_id() {
        let mut server = FakeServer::new(&["MOVE"], &["INBOX", "News"]);
        server.copyuid = false;
        server.deliver("INBOX", "news@shop.com", 45);
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(job.done[0].message_ids.len(), 45);
        assert_eq!(job.done[0].src.len(), 45);
        assert!(job.done[0].dst.is_none());
        undo(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(server.count("INBOX"), 45);
        assert_eq!(job.unfound, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gmail_archive_is_undone_by_putting_the_label_back() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS", "X-GM-EXT-1"], &["INBOX", "All"]);
        server.deliver("INBOX", "news@shop.com", 4);
        let mailbox = server.mailbox();
        let action = BulkAction::Move {
            from: "INBOX".to_string(),
            to: "All".to_string(),
            label: Some("\\Inbox".to_string()),
        };
        let mut job = job(&mut server, action, from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(server.count("INBOX"), 0);
        undo(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(server.count("INBOX"), 4);
        assert!(server
            .commands
            .iter()
            .any(|c| c.contains("+X-GM-LABELS.SILENT (\\Inbox)")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_gmail_archive_found_by_message_id_takes_back_its_own_among_older_mail() {
        let mut server = FakeServer::new(&["MOVE", "X-GM-EXT-1"], &["INBOX", "All"]);
        server.copyuid = false;
        for _ in 0..COPIES_READ {
            server.deliver_as("All", "news@shop.com", "<weekly@shop.com>");
        }
        server.deliver_as("INBOX", "news@shop.com", "<weekly@shop.com>");
        let action = BulkAction::Move {
            from: "INBOX".to_string(),
            to: "All".to_string(),
            label: Some("\\Inbox".to_string()),
        };
        let mut job = job(&mut server, action, from("news@shop.com")).await;
        moved_all(&mut server, &mut job, None).await;
        assert_eq!(server.count("INBOX"), 0);
        let archived = *server.folders["All"].messages.keys().max().unwrap();
        undone(&mut server, &mut job).await;
        assert_eq!(server.count("INBOX"), 1);
        assert_eq!(
            server
                .commands
                .iter()
                .filter(|c| c.contains("+X-GM-LABELS.SILENT (\\Inbox)"))
                .collect::<Vec<_>>(),
            vec![&format!(
                "UID STORE {archived} +X-GM-LABELS.SILENT (\\Inbox)"
            )],
            "the message archived is the one put back, not older mail sharing its id"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_mark_records_only_what_it_changed_and_undo_restores_exactly_that() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX"]);
        server.deliver("INBOX", "news@shop.com", 6);
        server.select("INBOX").await.unwrap();
        server
            .store(&[1, 2].into_iter().collect(), "+FLAGS.SILENT (\\Seen)")
            .await
            .unwrap();
        let mailbox = server.mailbox();
        let action = BulkAction::Mark {
            folder: "INBOX".to_string(),
            flag: MarkFlag::Seen,
            on: true,
        };
        let mut job = job(&mut server, action, from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(job.done[0].src.to_string(), "3:6");
        undo(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        let seen: Vec<u32> = server.folders["INBOX"]
            .messages
            .iter()
            .filter(|(_, m)| m.flags.iter().any(|f| f == "\\Seen"))
            .map(|(u, _)| *u)
            .collect();
        assert_eq!(seen, vec![1, 2], "what was read before stays read");
    }

    #[tokio::test(start_paused = true)]
    async fn an_undo_that_runs_out_of_time_carries_on() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 1_500);
        let mailbox = server.mailbox();
        let journal = MemoryJournal::default();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            Some(&journal),
        )
        .await
        .unwrap();
        server.step = Duration::from_secs(1);
        undo(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(1)),
            Some(&journal),
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Undoing);
        let mut again = journal.get("kat", &job.id).await.unwrap().unwrap();
        undo(
            &mut server,
            &mailbox,
            &mut again,
            Budget::of(Duration::from_secs(60)),
            Some(&journal),
        )
        .await
        .unwrap();
        assert_eq!(again.state, JobState::Undone);
        assert_eq!(server.count("INBOX"), 1_500);
    }

    #[tokio::test(start_paused = true)]
    async fn every_moved_batch_keeps_its_message_ids_and_where_the_server_put_it() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        moved_all(&mut server, &mut job, None).await;
        let batch = &job.done[0];
        assert_eq!(batch.message_ids.len(), 3);
        assert_eq!(batch.message_ids[&1], "<1.news@shop.com@test>");
        assert_eq!(batch.dst.as_ref().unwrap().to_string(), "1:3");
        assert_eq!(batch.dst_validity, Some(101));
    }

    #[tokio::test(start_paused = true)]
    async fn named_uids_that_arent_there_are_never_counted() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 2);
        let named = Selection::Uids([1, 2, 99].into_iter().collect());
        let mut job = job(&mut server, move_to("News"), named).await;
        assert_eq!(job.pending.to_string(), "1:2");
        moved_all(&mut server, &mut job, None).await;
        assert_eq!(job.handled(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn uids_read_under_another_numbering_are_refused_before_anything_changes() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 2);
        let mailbox = server.mailbox();
        let named = Selection::Uids(std::iter::once(1).collect());
        let stale = Ask {
            validity: Some(7),
            ..ask(&named)
        };
        let err = begin(&mut server, &mailbox, move_to("News"), stale)
            .await
            .unwrap_err();
        assert!(String::from(err).contains("message numbers changed"));
        let current = Ask {
            validity: Some(100),
            ..ask(&named)
        };
        assert!(begin(&mut server, &mailbox, move_to("News"), current)
            .await
            .is_ok());
        assert!(plan(&mut server, &mailbox, "INBOX", &named, Some(7))
            .await
            .is_err());
        assert_eq!(server.count("INBOX"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_copy_cut_off_before_its_expunge_is_settled_without_duplicates() {
        let mut server = FakeServer::new(&["UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        server.copyuid = false;
        server.fail = Some("UID EXPUNGE".to_string());
        let journal = MemoryJournal::default();
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        let err = run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            Some(&journal),
        )
        .await
        .unwrap_err();
        let err = String::from(err);
        assert!(err.contains("refused"));
        assert!(
            err.contains("<<<EXTERNAL_UNTRUSTED_CONTENT_"),
            "what the server said is fenced"
        );
        assert_eq!(server.count("INBOX"), 0, "the copies' originals are gone");
        assert_eq!(server.count("News"), 3);
        let kept = journal.get("kat", &job.id).await.unwrap().unwrap();
        assert_eq!(kept.in_flight, None);
        assert_eq!(kept.handled(), 3);
        let mut kept = kept;
        undone(&mut server, &mut kept).await;
        assert_eq!(server.count("INBOX"), 3);
        assert_eq!(server.count("News"), 0);
        assert_eq!(kept.unfound, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_move_that_never_heard_back_is_found_again_and_undone() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 4);
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        let (uids, _) = job.pending.split_first(BATCH);
        let batch = prepare(&mut server, &job.action, &uids).await.unwrap();
        server.move_uids(&uids, "News").await.unwrap();
        job.in_flight = Some(batch);
        assert_eq!(server.count("INBOX"), 0);

        let mut resumed = job.clone();
        moved_all(&mut server, &mut resumed, None).await;
        assert_eq!(resumed.state, JobState::Done);
        assert_eq!(resumed.handled(), 4);
        assert_eq!(resumed.done[0].dst.as_ref().unwrap().len(), 4);

        undo(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(job.state, JobState::Undone);
        assert_eq!(server.count("INBOX"), 4);
        assert_eq!(server.count("News"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_undo_by_message_id_takes_back_one_copy_each() {
        let mut server = FakeServer::new(&["MOVE"], &["INBOX", "News"]);
        server.copyuid = false;
        server.deliver("INBOX", "news@shop.com", 2);
        server.deliver_as("News", "news@shop.com", "<1.news@shop.com@test>");
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        moved_all(&mut server, &mut job, None).await;
        assert_eq!(server.count("News"), 3);
        undone(&mut server, &mut job).await;
        assert_eq!(server.count("INBOX"), 2);
        assert_eq!(server.count("News"), 1, "the earlier copy stays");
        assert_eq!(
            server.folders["News"]
                .messages
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(job.unfound, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_undo_counts_what_was_no_longer_there() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        moved_all(&mut server, &mut job, None).await;
        server.folders.get_mut("News").unwrap().messages.remove(&2);
        undone(&mut server, &mut job).await;
        assert_eq!(server.count("INBOX"), 2);
        assert_eq!(job.unfound, 1);
        assert!(report(&job, true)["note"]
            .as_str()
            .unwrap()
            .contains("except 1"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_mark_in_a_renumbered_folder_is_said_plainly_to_be_past_undoing() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX"]);
        server.deliver("INBOX", "news@shop.com", 2);
        let action = BulkAction::Mark {
            folder: "INBOX".to_string(),
            flag: MarkFlag::Seen,
            on: true,
        };
        let journal = MemoryJournal::default();
        let mut job = job(&mut server, action, from("news@shop.com")).await;
        moved_all(&mut server, &mut job, Some(&journal)).await;
        server.folders.get_mut("INBOX").unwrap().validity = Some(999);
        let mailbox = server.mailbox();
        let budget = Budget::of(Duration::from_secs(60));
        undo(&mut server, &mailbox, &mut job, budget, Some(&journal))
            .await
            .unwrap();
        assert_eq!(job.state, JobState::Irreversible);
        let note = report(&job, true)["note"].as_str().unwrap().to_string();
        assert!(note.contains("can't be undone"), "{note}");
        assert!(note.contains("numbers changed"), "{note}");
        assert!(!note.contains("email.undo puts back"), "{note}");
        assert_eq!(journal.latest("kat", "home").await.unwrap(), None);
        let sent = server.commands.len();
        undo(&mut server, &mailbox, &mut job, budget, Some(&journal))
            .await
            .unwrap();
        assert_eq!(server.commands.len(), sent, "asking again does nothing");
        assert!(server.folders["INBOX"]
            .messages
            .values()
            .all(|m| m.flags.iter().any(|f| f == "\\Seen")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_copy_never_sent_is_not_found_in_a_copy_that_was_already_there() {
        let mut server = FakeServer::new(&["UIDPLUS"], &["INBOX", "News"]);
        server.copyuid = false;
        server.deliver("INBOX", "news@shop.com", 2);
        server.deliver_as("News", "news@shop.com", "<1.news@shop.com@test>");
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        let (uids, _) = job.pending.split_first(BATCH);
        job.in_flight = Some(prepare(&mut server, &job.action, &uids).await.unwrap());

        let mut resumed = job.clone();
        moved_all(&mut server, &mut resumed, None).await;
        assert_eq!(resumed.state, JobState::Done);
        assert_eq!(resumed.handled(), 2);
        assert_eq!(server.count("INBOX"), 0);
        assert_eq!(server.count("News"), 3, "the earlier copy and both moved");

        undo(
            &mut server,
            &mailbox,
            &mut resumed,
            Budget::of(Duration::from_secs(60)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(server.count("INBOX"), 2, "nothing was lost");
        assert_eq!(server.count("News"), 1, "the earlier copy stays");
        assert_eq!(resumed.unfound, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn messages_sharing_a_message_id_are_each_found_and_put_back() {
        let mut server = FakeServer::new(&["MOVE"], &["INBOX", "News"]);
        server.copyuid = false;
        for _ in 0..3 {
            server.deliver_as("INBOX", "news@shop.com", "<same@shop.com>");
        }
        server.deliver_as("News", "news@shop.com", "<same@shop.com>");
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        moved_all(&mut server, &mut job, None).await;
        assert_eq!(server.count("News"), 4);
        undone(&mut server, &mut job).await;
        assert_eq!(server.count("INBOX"), 3, "every copy comes back");
        assert_eq!(
            server.folders["News"]
                .messages
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![1],
            "and only the one that was there before stays"
        );
        assert_eq!(job.unfound, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_connection_leaves_the_job_running_for_the_next_call() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 1_200);
        server.fail = Some("UID MOVE 501".to_string());
        server.lost = true;
        let journal = MemoryJournal::default();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        moved_all(&mut server, &mut job, Some(&journal)).await;
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.handled(), 500);
        assert!(job.in_flight.is_some(), "the batch it lost is kept");
        assert_eq!(job.stopped, None);

        let mut resumed = journal.get("kat", &job.id).await.unwrap().unwrap();
        assert_eq!(resumed, job);
        moved_all(&mut server, &mut resumed, Some(&journal)).await;
        assert_eq!(resumed.state, JobState::Done);
        assert_eq!(server.count("News"), 1_200);
        assert_eq!(server.count("INBOX"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn without_its_numbering_a_job_is_never_undone_and_named_uids_never_trusted() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 2);
        server.folders.get_mut("INBOX").unwrap().validity = None;
        let mailbox = server.mailbox();
        let named = Selection::Uids(std::iter::once(1).collect());
        let read_under = Ask {
            validity: Some(100),
            ..ask(&named)
        };
        let err = begin(&mut server, &mailbox, move_to("News"), read_under)
            .await
            .unwrap_err();
        assert!(String::from(err).contains("no UIDVALIDITY"));

        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        assert_eq!(job.validity, None);
        moved_all(&mut server, &mut job, None).await;
        assert_eq!(server.count("News"), 2);
        let said = report(&job, true)["note"].as_str().unwrap().to_string();
        assert!(
            said.contains("can't be undone") && !said.contains("puts back"),
            "the job says up front what undo will do: {said}"
        );
        job.state = JobState::Stopped;
        assert!(stopped_note(&job).contains("can't be undone"));
        job.state = JobState::Done;
        undone(&mut server, &mut job).await;
        assert_eq!(job.state, JobState::Irreversible);
        assert_eq!(server.count("News"), 2, "nothing is guessed back");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_journal_a_part_done_job_says_what_is_left() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.step = Duration::from_secs(1);
        server.deliver("INBOX", "news@shop.com", 1_200);
        let mailbox = server.mailbox();
        let named = Selection::Uids((1..=1_200).collect());
        let mut job = job(&mut server, move_to("News"), named).await;
        run(
            &mut server,
            &mailbox,
            &mut job,
            Budget::of(Duration::from_secs(2)),
            None,
        )
        .await
        .unwrap();
        let out = report(&job, false);
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("Only 500 of 1200"), "{note}");
        assert!(!note.contains("Done"), "{note}");
        assert!(out.get("job").is_none(), "there is no job to carry on");
        assert_eq!(out["remaining_uids"].as_array().unwrap().len(), 700);
        assert_eq!(out["remaining_uids"][0], 501);
    }
}
