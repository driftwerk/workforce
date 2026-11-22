//! Moving and marking many messages at once, matched on the server: in
//! batches, within a time budget, carried on by job id, every batch journaled
//! so `email.undo` puts it back. Nothing is ever deleted: the only expunge is
//! `UID EXPUNGE` of uids already copied elsewhere.

use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::time::Instant;

use super::criteria::{quoted, Selection};
use super::journal::{Batch, BulkAction, BulkJob, JobState, MailJournal};
use super::mailbox::Mailbox;
use super::session::{Landed, MailSession};
use super::uids::UidSet;

pub const BATCH: usize = 500;
const SAMPLE: usize = 10;
const IDS_PER_SEARCH: usize = 20;
const BUDGET: Duration = Duration::from_secs(45);

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

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The uids `selection` means in the selected folder.
async fn resolve(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    selection: &Selection,
) -> Result<UidSet, String> {
    match selection {
        Selection::Uids(uids) => Ok(uids.clone()),
        Selection::Search(criteria) => {
            let query = criteria.to_imap(mailbox.caps.gmail)?;
            session.search(&query).await.map_err(text)
        }
    }
}

/// What a dry run reports: how many messages match, and the newest few.
pub async fn plan(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    folder: &str,
    selection: &Selection,
) -> Result<Value, String> {
    session.examine(folder).await.map_err(text)?;
    let uids = resolve(session, mailbox, selection).await?;
    let newest: UidSet = uids.iter().rev().take(SAMPLE).collect();
    let sample = match newest.is_empty() {
        true => Vec::new(),
        false => session.summaries(&newest).await.map_err(text)?,
    };
    let mut sample: Vec<Value> = sample
        .into_iter()
        .map(|s| json!({"uid": s.uid, "from": s.from, "subject": s.subject, "date": s.date}))
        .collect();
    sample.sort_by_key(|s| std::cmp::Reverse(s["uid"].as_u64()));
    Ok(json!({ "dry_run": true, "folder": folder, "count": uids.len(), "sample": sample }))
}

/// A new job for `action` over what `selection` means in its source folder.
pub async fn begin(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    owner: &str,
    account: &str,
    action: BulkAction,
    selection: &Selection,
) -> Result<BulkJob, String> {
    let selected = session.select(action.source()).await.map_err(text)?;
    let pending = resolve(session, mailbox, selection).await?;
    Ok(BulkJob {
        id: uuid::Uuid::new_v4().to_string(),
        owner: owner.to_string(),
        account: account.to_string(),
        asked: selection.describe(),
        action,
        validity: selected.validity,
        pending,
        done: Vec::new(),
        state: JobState::Running,
        stopped: None,
        unfound: 0,
        created: Utc::now(),
    })
}

async fn keep(journal: Option<&dyn MailJournal>, job: &BulkJob) -> Result<(), String> {
    match journal {
        Some(journal) => journal
            .save(job)
            .await
            .map_err(|e| format!("Couldn't record the job, so it stopped: {e}")),
        None => Ok(()),
    }
}

fn stop(job: &mut BulkJob, why: String) {
    job.state = JobState::Stopped;
    job.stopped = Some(why);
}

/// Message-IDs of `uids`, for a server that won't say where moved messages
/// land.
async fn message_ids(
    session: &mut (impl MailSession + ?Sized),
    uids: &UidSet,
) -> Result<Vec<String>, String> {
    let headers = session.headers(uids, &["MESSAGE-ID"]).await.map_err(text)?;
    Ok(headers
        .iter()
        .filter_map(|(_, block)| {
            let (headers, _) = mailparse::parse_headers(block.as_bytes()).ok()?;
            use mailparse::MailHeaderMap;
            headers.get_first_value("Message-ID")
        })
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect())
}

/// Moves `uids` from the selected folder to `to` with only these uids ever
/// leaving it.
async fn relocate(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    uids: &UidSet,
    to: &str,
) -> Result<Option<Landed>, String> {
    match (mailbox.caps.moves, mailbox.caps.uidplus) {
        (true, _) => session.move_uids(uids, to).await.map_err(text),
        (false, true) => {
            let landed = session.copy_uids(uids, to).await.map_err(text)?;
            session
                .store(uids, "+FLAGS.SILENT (\\Deleted)")
                .await
                .map_err(text)?;
            session.expunge_uids(uids).await.map_err(text)?;
            Ok(landed)
        }
        (false, false) => Err(
            "This server can't move just these messages safely (it has neither MOVE nor UIDPLUS), so nothing was moved."
                .to_string(),
        ),
    }
}

async fn run_batch(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    action: &BulkAction,
    uids: &UidSet,
) -> Result<Batch, String> {
    match action {
        BulkAction::Move { to, .. } => {
            let ids = match mailbox.caps.uidplus {
                true => Vec::new(),
                false => message_ids(session, uids).await?,
            };
            let landed = relocate(session, mailbox, uids, to).await?;
            Ok(Batch {
                src: uids.clone(),
                dst: landed.as_ref().map(|l| l.dest.iter().copied().collect()),
                dst_validity: landed.map(|l| l.validity),
                message_ids: ids,
            })
        }
        BulkAction::Mark { flag, on, .. } => {
            let changing = session
                .search(&format!("UID {uids} {}", flag.changes(*on)))
                .await
                .map_err(text)?;
            if !changing.is_empty() {
                let sign = if *on { '+' } else { '-' };
                session
                    .store(&changing, &format!("{sign}FLAGS.SILENT ({})", flag.imap()))
                    .await
                    .map_err(text)?;
            }
            Ok(Batch {
                src: changing,
                dst: None,
                dst_validity: None,
                message_ids: Vec::new(),
            })
        }
    }
}

/// Carries `job` on, batch by batch, until it is done or the budget is spent.
pub async fn run(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    job: &mut BulkJob,
    budget: Budget,
    journal: Option<&dyn MailJournal>,
) -> Result<(), String> {
    if job.state != JobState::Running {
        return Ok(());
    }
    let selected = session.select(job.action.source()).await.map_err(text)?;
    if selected.validity != job.validity {
        stop(
            job,
            format!(
                "{} was rebuilt by the server since the job began, so its message numbers changed; nothing more was done.",
                job.action.source()
            ),
        );
        return keep(journal, job).await;
    }
    keep(journal, job).await?;
    while !job.pending.is_empty() && !budget.spent() {
        let (batch, rest) = job.pending.split_first(BATCH);
        let done = run_batch(session, mailbox, &job.action, &batch).await;
        match done {
            Ok(done) => {
                job.done.push(done);
                job.pending = rest;
                keep(journal, job).await?;
            }
            Err(why) => {
                stop(job, why.clone());
                keep(journal, job).await?;
                return Err(why);
            }
        }
    }
    if job.pending.is_empty() {
        job.state = JobState::Done;
        keep(journal, job).await?;
    }
    Ok(())
}

/// Where one moved batch is now: its uids in `to`, found by COPYUID when the
/// server gave it and still holds that folder's numbering, else by
/// Message-ID.
async fn landed_now(
    session: &mut (impl MailSession + ?Sized),
    batch: &Batch,
    validity: u32,
) -> Result<UidSet, String> {
    match (&batch.dst, batch.dst_validity) {
        (Some(dst), Some(v)) if v == validity => Ok(dst.clone()),
        _ => {
            let mut found = UidSet::default();
            for ids in batch.message_ids.chunks(IDS_PER_SEARCH) {
                let keys = ids
                    .iter()
                    .map(|id| quoted(id).map(|q| format!("HEADER Message-ID {q}")))
                    .collect::<Result<Vec<_>, _>>()?;
                let query = keys
                    .iter()
                    .skip(1)
                    .fold(keys[0].clone(), |acc, key| format!("OR {acc} {key}"));
                let uids = session.search(&query).await.map_err(text)?;
                found = found.iter().chain(uids.iter()).collect();
            }
            Ok(found)
        }
    }
}

async fn undo_batch(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    action: &BulkAction,
    batch: &Batch,
) -> Result<usize, String> {
    match action {
        BulkAction::Move { from, to, label } => {
            let selected = session.select(to).await.map_err(text)?;
            let back = landed_now(session, batch, selected.validity).await?;
            if !back.is_empty() {
                match label {
                    Some(label) => {
                        session
                            .store(&back, &format!("+X-GM-LABELS.SILENT ({label})"))
                            .await
                            .map_err(text)?;
                    }
                    None => {
                        relocate(session, mailbox, &back, from).await?;
                    }
                }
            }
            Ok(batch.src.len().saturating_sub(back.len()))
        }
        BulkAction::Mark { folder, flag, on } => {
            session.select(folder).await.map_err(text)?;
            if !batch.src.is_empty() {
                let sign = if *on { '-' } else { '+' };
                session
                    .store(&batch.src, &format!("{sign}FLAGS.SILENT ({})", flag.imap()))
                    .await
                    .map_err(text)?;
            }
            Ok(0)
        }
    }
}

/// Puts back what `job` did, newest batch first, until all of it is back or
/// the budget is spent. A job still running stops taking new batches.
pub async fn undo(
    session: &mut (impl MailSession + ?Sized),
    mailbox: &Mailbox,
    job: &mut BulkJob,
    budget: Budget,
    journal: Option<&dyn MailJournal>,
) -> Result<(), String> {
    match job.state {
        JobState::Undone => return Ok(()),
        JobState::Running | JobState::Done | JobState::Stopped => {
            job.state = JobState::Undoing;
            job.pending = UidSet::default();
            keep(journal, job).await?;
        }
        JobState::Undoing => {}
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

/// A job as a call reports it.
pub fn report(job: &BulkJob) -> Value {
    let mut out = json!({
        "job": job.id,
        "state": job.state,
        "asked": job.asked,
        "handled": job.handled(),
        "remaining": job.pending.len(),
    });
    let note = match job.state {
        JobState::Running => Some(format!(
            "Not finished: call again with job = \"{}\" to carry on.",
            job.id
        )),
        JobState::Undoing => Some(format!(
            "Still putting messages back: call email.undo with job = \"{}\" to carry on.",
            job.id
        )),
        JobState::Done => Some("Done. email.undo puts it back.".to_string()),
        JobState::Undone if job.unfound > 0 => Some(format!(
            "Put back, except {} message(s) that were no longer where they had been moved to.",
            job.unfound
        )),
        JobState::Undone => Some("Put back.".to_string()),
        JobState::Stopped => job.stopped.clone(),
    };
    if let (Some(obj), Some(note)) = (out.as_object_mut(), note) {
        obj.insert("note".to_string(), Value::String(note));
    }
    out
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::{BTreeMap, HashMap};

    use async_trait::async_trait;

    use super::super::error::EmailError;
    use super::super::session::{Listed, Selected, Summary};
    use super::*;

    #[derive(Debug, Clone, Default)]
    pub struct Message {
        pub from: String,
        pub message_id: String,
        pub flags: Vec<String>,
    }

    #[derive(Debug, Clone, Default)]
    pub struct Folder {
        pub validity: u32,
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
                            validity: 100 + i as u32,
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
            let f = self.folders.get_mut(folder).unwrap();
            for _ in 0..n {
                let uid = f.next;
                f.next += 1;
                f.messages.insert(
                    uid,
                    Message {
                        from: from.to_string(),
                        message_id: format!("<{uid}.{from}@test>"),
                        flags: Vec::new(),
                    },
                );
            }
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
            copyuid.then(|| Landed {
                validity: dest.validity,
                source,
                dest: landed,
            })
        }

        async fn tick(&mut self, command: String) {
            self.commands.push(command);
            tokio::time::sleep(self.step).await;
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
            self.tick(format!("SELECT {folder}")).await;
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
            self.tick(format!("UID SEARCH {query}")).await;
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
            self.tick(format!("UID MOVE {uids} {to}")).await;
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
            self.tick(format!("UID COPY {uids} {to}")).await;
            let messages = self.take(uids);
            Ok(self.put(to, messages))
        }

        async fn store(&mut self, uids: &UidSet, change: &str) -> Result<(), EmailError> {
            self.tick(format!("UID STORE {uids} {change}")).await;
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
            self.tick(format!("UID EXPUNGE {uids}")).await;
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
            self.tick(format!("UID FETCH {uids} HEADER")).await;
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

        async fn messages_in(&mut self, folder: &str) -> Result<u32, EmailError> {
            Ok(self
                .folders
                .get(folder)
                .map_or(0, |f| f.messages.len() as u32))
        }

        async fn create(&mut self, folder: &str) -> Result<(), EmailError> {
            self.tick(format!("CREATE {folder}")).await;
            self.folders.insert(folder.to_string(), Folder::default());
            Ok(())
        }

        async fn rename(&mut self, from: &str, to: &str) -> Result<(), EmailError> {
            self.tick(format!("RENAME {from} {to}")).await;
            let f = self.folders.remove(from).unwrap();
            self.folders.insert(to.to_string(), f);
            Ok(())
        }

        async fn delete(&mut self, folder: &str) -> Result<(), EmailError> {
            self.tick(format!("DELETE {folder}")).await;
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

    async fn job(server: &mut FakeServer, action: BulkAction, selection: Selection) -> BulkJob {
        let mailbox = server.mailbox();
        begin(server, &mailbox, "kat", "home", action, &selection)
            .await
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_dry_run_counts_and_samples_and_changes_nothing() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 25);
        server.deliver("INBOX", "friend@x.com", 3);
        let mailbox = server.mailbox();
        let out = plan(&mut server, &mailbox, "INBOX", &from("news@shop.com"))
            .await
            .unwrap();
        assert_eq!(out["count"], 25);
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
        assert!(err.contains("can't move just these messages safely"));
        assert_eq!(job.state, JobState::Stopped);
        assert_eq!(server.count("INBOX"), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rebuilt_mailbox_stops_the_job() {
        let mut server = FakeServer::new(&["MOVE", "UIDPLUS"], &["INBOX", "News"]);
        server.deliver("INBOX", "news@shop.com", 3);
        let mailbox = server.mailbox();
        let mut job = job(&mut server, move_to("News"), from("news@shop.com")).await;
        server.folders.get_mut("INBOX").unwrap().validity = 999;
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
        assert_eq!(report(&job)["note"], "Put back.");
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
}
